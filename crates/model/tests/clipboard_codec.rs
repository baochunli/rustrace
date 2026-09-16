use chrono::{TimeZone, Utc};
use rustrace_model::*;

fn reference() -> RecordedEventRef {
    RecordedEventRef {
        session_id: SessionId::new("segment-a").unwrap(),
        sequence: 2,
        event_hash: Hash::from_bytes([0x11; 32]),
    }
}

fn source() -> ClipboardSource {
    ClipboardSource {
        prefix: reference(),
        document_id: DocumentId::new("source").unwrap(),
        path: WorkspacePath::new("a.rs").unwrap(),
        version: 7,
        content_hash: Hash::from_bytes([0x22; 32]),
        start_byte: 2,
        end_byte: 8,
    }
}

fn paste() -> InternalPaste {
    InternalPaste {
        source: reference(),
        transaction: EditorTransaction {
            document_id: DocumentId::new("destination").unwrap(),
            version_before: 0,
            version_after: 1,
            origin: EditOrigin::Paste,
            edits: vec![TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: "\r\n🦀".to_owned(),
            }],
            selection_before: SelectionState::caret(0),
            selection_after: SelectionState::caret(6),
            hash_before: document_hash(""),
            hash_after: document_hash("\r\n🦀"),
        },
    }
}

fn envelope(event: Event) -> EventEnvelope {
    EventEnvelope {
        format_version: 1,
        session_id: SessionId::new("segment-a").unwrap(),
        sequence: 3,
        monotonic_millis: 10,
        wall_clock_utc: None,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event,
    }
}

#[test]
fn source_and_required_linked_paste_round_trip_canonically() {
    for event in [
        Event::ClipboardCopied(source()),
        Event::InternalPaste(paste()),
    ] {
        let envelope = envelope(event).seal(Hash::zero()).unwrap();
        let bytes = encode_envelope(&envelope).unwrap();
        let DecodeOutcome::Decoded(decoded) =
            decode_envelope(&bytes, DecodePolicy::RejectUnsupported).unwrap()
        else {
            panic!("known event skipped")
        };
        assert_eq!(decoded, envelope);
        assert_eq!(encode_envelope(&decoded).unwrap(), bytes);
    }
    let actual = serde_json::to_string(&Event::ClipboardCopied(source())).unwrap();
    let expected = format!(
        r#"{{"type":"clipboard_copied","payload":{{"prefix":{{"session_id":"segment-a","sequence":2,"event_hash":"{}"}},"document_id":"source","path":"a.rs","version":7,"content_hash":"{}","start_byte":2,"end_byte":8}}}}"#,
        "11".repeat(32),
        "22".repeat(32),
    );
    assert_eq!(actual, expected);
}

#[test]
fn rejection_golden_has_only_fixed_reason_and_channel_and_fits_one_kibibyte() {
    let rejection = Event::PasteRejected(PasteRejected {
        reason: PasteRejectionReason::ExternalInput,
        channel: PasteInputChannel::TerminalBracketed,
    });
    assert_eq!(
        serde_json::to_string(&rejection).unwrap(),
        r#"{"type":"paste_rejected","payload":{"reason":"external_input","channel":"terminal_bracketed"}}"#
    );
    for reason in [
        PasteRejectionReason::ExternalInput,
        PasteRejectionReason::UnverifiableInput,
        PasteRejectionReason::MissingLiveSource,
        PasteRejectionReason::OutsideEditor,
    ] {
        for channel in [
            PasteInputChannel::TerminalBracketed,
            PasteInputChannel::InternalShortcut,
            PasteInputChannel::ProductionCommand,
            PasteInputChannel::Programmatic,
        ] {
            let mut event = envelope(Event::PasteRejected(PasteRejected { reason, channel }));
            event.session_id = SessionId::new("s".repeat(MAX_IDENTIFIER_BYTES)).unwrap();
            event.sequence = u64::MAX;
            event.monotonic_millis = MAX_MONOTONIC_MILLIS;
            event.wall_clock_utc = Some(Utc.with_ymd_and_hms(9999, 12, 31, 23, 59, 59).unwrap());
            let bytes = encode_envelope(&event).unwrap();
            assert!(bytes.len() <= MAX_PASTE_REJECTION_BYTES);
            assert!(matches!(
                decode_envelope(&bytes, DecodePolicy::RejectUnsupported),
                Ok(DecodeOutcome::Decoded(_))
            ));
        }
    }
}

#[test]
fn rejection_wire_limit_applies_even_to_whitespace() {
    let event = envelope(Event::PasteRejected(PasteRejected {
        reason: PasteRejectionReason::UnverifiableInput,
        channel: PasteInputChannel::Programmatic,
    }));
    let mut bytes = encode_envelope(&event).unwrap();
    bytes.resize(MAX_PASTE_REJECTION_BYTES, b' ');
    assert!(decode_envelope(&bytes, DecodePolicy::RejectUnsupported).is_ok());
    bytes.push(b' ');
    assert!(matches!(
        decode_envelope(&bytes, DecodePolicy::RejectUnsupported),
        Err(DecodeError::EnvelopeTooLarge {
            maximum: MAX_PASTE_REJECTION_BYTES,
            ..
        })
    ));
}

#[test]
fn new_events_require_links_and_closed_enums_without_downgrade_or_payload_fields() {
    let original = serde_json::to_value(envelope(Event::InternalPaste(paste()))).unwrap();
    for field in ["source", "transaction"] {
        let mut missing = original.clone();
        missing["event"]["payload"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(
            decode_envelope(
                &serde_json::to_vec(&missing).unwrap(),
                DecodePolicy::SkipUnsupported
            )
            .is_err()
        );
    }
    for field in ["session_id", "sequence", "event_hash"] {
        let mut missing = original.clone();
        missing["event"]["payload"]["source"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(
            decode_envelope(
                &serde_json::to_vec(&missing).unwrap(),
                DecodePolicy::RejectUnsupported
            )
            .is_err()
        );
    }
    for origin in ["keyboard", "unknown", "verified_internal", ""] {
        let mut forged = original.clone();
        forged["event"]["payload"]["transaction"]["origin"] = origin.into();
        assert!(
            decode_envelope(
                &serde_json::to_vec(&forged).unwrap(),
                DecodePolicy::RejectUnsupported
            )
            .is_err()
        );
    }
    let rejected = serde_json::to_value(envelope(Event::PasteRejected(PasteRejected {
        reason: PasteRejectionReason::ExternalInput,
        channel: PasteInputChannel::TerminalBracketed,
    })))
    .unwrap();
    for field in [
        "payload",
        "preview",
        "hash",
        "length",
        "character_count",
        "line_count",
    ] {
        let mut added = rejected.clone();
        added["event"]["payload"][field] = "not permitted".into();
        assert!(
            decode_envelope(
                &serde_json::to_vec(&added).unwrap(),
                DecodePolicy::RejectUnsupported
            )
            .is_err()
        );
    }
    for field in ["reason", "channel"] {
        let mut forged = rejected.clone();
        forged["event"]["payload"][field] = "internal_authorized".into();
        assert!(
            decode_envelope(
                &serde_json::to_vec(&forged).unwrap(),
                DecodePolicy::SkipUnsupported
            )
            .is_err()
        );
    }
}

#[test]
fn source_and_paste_structural_limits_are_checked_before_replay() {
    for (start_byte, end_byte) in [(0, 0), (8, 2), (0, MAX_INTERNAL_CLIPBOARD_BYTES as u64 + 1)] {
        let mut source = source();
        source.start_byte = start_byte;
        source.end_byte = end_byte;
        assert!(encode_envelope(&envelope(Event::ClipboardCopied(source))).is_err());
    }
    let mut source = source();
    source.prefix.sequence = 0;
    assert!(encode_envelope(&envelope(Event::ClipboardCopied(source))).is_err());
    let mut paste = paste();
    paste
        .transaction
        .edits
        .push(paste.transaction.edits[0].clone());
    assert!(encode_envelope(&envelope(Event::InternalPaste(paste))).is_err());
}
