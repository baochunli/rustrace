use chrono::{TimeZone, Utc};
use rustrace_model::{
    DocumentId, Event, EventEnvelope, FORMAT_VERSION_V1, FileFocused, Hash, SessionId,
    compute_event_hash, encode_event_hash_material,
};

fn hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; Hash::LENGTH])
}

fn envelope() -> EventEnvelope {
    EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: SessionId::new("session-01").unwrap(),
        sequence: 7,
        monotonic_millis: 25,
        wall_clock_utc: Some(Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()),
        previous_event_hash: hash(0x22),
        event_hash: hash(0x33),
        event: Event::FileFocused(FileFocused {
            document_id: DocumentId::new("document-01").unwrap(),
        }),
    }
}

#[test]
fn canonical_hash_material_and_digest_match_the_golden_vector() {
    let envelope = envelope();

    let material = encode_event_hash_material(&envelope).unwrap();
    assert_eq!(material.len(), 193);
    assert_eq!(
        String::from_utf8(material).unwrap(),
        r#"{"format_version":1,"session_id":"session-01","sequence":7,"monotonic_millis":25,"wall_clock_utc":"2026-01-02T03:04:05Z","event":{"type":"file_focused","payload":{"document_id":"document-01"}}}"#
    );
    assert_eq!(
        compute_event_hash(Hash::zero(), &envelope)
            .unwrap()
            .to_string(),
        "ce8cd99a855aad67f76af4c68d4387788394565a56680b75f8d9897708d2afcd"
    );
}

#[test]
fn identity_changes_affect_the_digest_but_supplied_chain_fields_do_not() {
    let original = envelope();
    let expected = compute_event_hash(Hash::zero(), &original).unwrap();

    let mut content_changed = original.clone();
    content_changed.event = Event::FileFocused(FileFocused {
        document_id: DocumentId::new("document-02").unwrap(),
    });
    assert_ne!(
        compute_event_hash(Hash::zero(), &content_changed).unwrap(),
        expected
    );

    let mut identity_changed = original.clone();
    identity_changed.monotonic_millis += 1;
    assert_ne!(
        compute_event_hash(Hash::zero(), &identity_changed).unwrap(),
        expected
    );

    let mut supplied_chain_changed = original.clone();
    supplied_chain_changed.previous_event_hash = hash(0x44);
    supplied_chain_changed.event_hash = hash(0x55);
    assert_eq!(
        encode_event_hash_material(&supplied_chain_changed).unwrap(),
        encode_event_hash_material(&original).unwrap()
    );
    assert_eq!(
        compute_event_hash(Hash::zero(), &supplied_chain_changed).unwrap(),
        expected
    );
    assert_ne!(
        compute_event_hash(hash(0x66), &supplied_chain_changed).unwrap(),
        expected
    );
}

#[test]
fn sealing_sets_both_chain_fields_from_the_explicit_previous_hash() {
    let previous = hash(0xaa);
    let sealed = envelope().seal(previous).unwrap();

    assert_eq!(sealed.previous_event_hash, previous);
    assert_eq!(
        sealed.event_hash,
        compute_event_hash(previous, &sealed).unwrap()
    );
}
