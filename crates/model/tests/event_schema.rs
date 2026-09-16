use std::str::FromStr;

use chrono::{TimeZone, Utc};
use rustrace_model::{
    CodeActionApplied, CommandFinished, CommandId, CommandOutput, CommandStarted,
    CompletionAccepted, CompletionRequested, DecodeError, DecodeOutcome, DecodePolicy, Diagnostic,
    DiagnosticSeverity, DocumentHash, DocumentId, EditOrigin, Event, EventEnvelope,
    ExternalFileChange, FILE_EDITED_ENVELOPE_MAX_OVERHEAD_BYTES, FORMAT_VERSION_V1, FileCreated,
    FileDeleted, FileEdited, FileFocused, FileRenamed, Hash, MAX_ENVELOPE_BYTES,
    MAX_FILE_EDITED_TRANSACTION_BYTES, MAX_IDENTIFIER_BYTES, MAX_INSERTED_TEXT_BYTES,
    MAX_JSON_KEY_BYTES, MAX_JSON_NESTING, MAX_JSON_STRING_BYTES, MAX_JSON_VALUES,
    MAX_MONOTONIC_MILLIS, MAX_OUTPUT_BYTES, MAX_PATH_BYTES, MAX_STRING_BYTES, MAX_VECTOR_ITEMS,
    OutputStream, SelectionChanged, SelectionState, SessionEnded, SessionId, SessionResumed,
    SessionStarted, SkipReason, SubmissionFinalized, TestCaseCompared, TestCaseComparisonError,
    TestCaseComparisonOutcome, TextEdit, TextRange, ValidationError, ViewportChanged,
    WorkspaceCheckpoint, WorkspaceDirectory, WorkspacePath, decode_envelope, encode_envelope,
};

fn session_id() -> SessionId {
    SessionId::new("session-01").unwrap()
}

fn document_id() -> DocumentId {
    DocumentId::new("document-01").unwrap()
}

fn command_id() -> CommandId {
    CommandId::new("command-01").unwrap()
}

fn hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; 32])
}

fn workspace_path(value: &str) -> WorkspacePath {
    WorkspacePath::new(value).unwrap()
}

fn workspace_directory(value: &str) -> WorkspaceDirectory {
    WorkspaceDirectory::new(value).unwrap()
}

fn maximum_workspace_path() -> WorkspacePath {
    let value = [
        "a".repeat(255),
        "b".repeat(255),
        "c".repeat(255),
        "d".repeat(254),
        "e".to_owned(),
    ]
    .join("/");
    assert_eq!(value.len(), MAX_PATH_BYTES);
    workspace_path(&value)
}

fn edit() -> TextEdit {
    TextEdit {
        start_byte: 2,
        end_byte: 4,
        inserted_text: "let x = 1;".to_owned(),
    }
}

fn envelope(event: Event) -> EventEnvelope {
    EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: session_id(),
        sequence: 1,
        monotonic_millis: 25,
        wall_clock_utc: Some(Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()),
        previous_event_hash: hash(0),
        event_hash: hash(0x11),
        event,
    }
}

fn decode(json: &[u8], policy: DecodePolicy) -> Result<DecodeOutcome, DecodeError> {
    decode_envelope(json, policy)
}

fn raw_envelope(event_type: &str, payload: &str, format_version: u32) -> String {
    let known = String::from_utf8(
        encode_envelope(&envelope(Event::FileFocused(FileFocused {
            document_id: document_id(),
        })))
        .unwrap(),
    )
    .unwrap();
    let known_event = r#""event":{"type":"file_focused","payload":{"document_id":"document-01"}}"#;
    let replacement = format!(r#""event":{{"type":"{event_type}","payload":{payload}}}"#);
    known.replace(known_event, &replacement).replacen(
        "\"format_version\":1",
        &format!("\"format_version\":{format_version}"),
        1,
    )
}

fn raw_envelope_with_event(event: &str) -> String {
    let known = String::from_utf8(
        encode_envelope(&envelope(Event::FileFocused(FileFocused {
            document_id: document_id(),
        })))
        .unwrap(),
    )
    .unwrap();
    let known_event = r#""event":{"type":"file_focused","payload":{"document_id":"document-01"}}"#;
    known.replace(known_event, &format!("\"event\":{event}"))
}

#[test]
fn canonical_json_is_exact_and_round_trips() {
    let value = envelope(Event::FileFocused(FileFocused {
        document_id: document_id(),
    }));

    let encoded = encode_envelope(&value).unwrap();
    assert_eq!(
        String::from_utf8(encoded.clone()).unwrap(),
        concat!(
            "{\"format_version\":1,\"session_id\":\"session-01\",\"sequence\":1,\"monotonic_millis\":25,\"wall_clock_utc\":\"2026-01-02T03:04:05Z\",\"previous_event_hash\":\"",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "\",\"event_hash\":\"",
            "1111111111111111111111111111111111111111111111111111111111111111",
            "\",\"event\":{\"type\":\"file_focused\",\"payload\":{\"document_id\":\"document-01\"}}}",
        )
    );
    assert_eq!(
        decode(&encoded, DecodePolicy::RejectUnsupported).unwrap(),
        DecodeOutcome::Decoded(value)
    );
}

#[test]
fn every_v1_event_variant_has_exact_golden_json_and_round_trips() {
    let events = vec![
        Event::SessionStarted(SessionStarted {
            client_version: "0.1.0".to_owned(),
            starter_workspace_hash: hash(1),
        }),
        Event::SessionResumed(SessionResumed { last_sequence: 8 }),
        Event::SessionEnded(SessionEnded {
            final_workspace_hash: hash(2),
        }),
        Event::TestCaseCompared(TestCaseCompared {
            command_id: command_id(),
            case: "sample_1".to_owned(),
            expected_blake3: hash(12),
            actual_blake3: Some(hash(12)),
            outcome: TestCaseComparisonOutcome::Pass,
        }),
        Event::TestCaseCompared(TestCaseCompared {
            command_id: command_id(),
            case: "sample_2".to_owned(),
            expected_blake3: hash(14),
            actual_blake3: Some(hash(15)),
            outcome: TestCaseComparisonOutcome::Mismatch {
                line: 2,
                expected_len: 3,
                actual_len: 4,
            },
        }),
        Event::TestCaseCompared(TestCaseCompared {
            command_id: command_id(),
            case: "sample_3".to_owned(),
            expected_blake3: hash(16),
            actual_blake3: None,
            outcome: TestCaseComparisonOutcome::Error {
                reason: TestCaseComparisonError::LaunchFailed,
            },
        }),
        Event::FileCreated(FileCreated {
            document_id: document_id(),
            path: workspace_path("src/main.rs"),
            contents: "fn main() {}".to_owned(),
            content_hash: hash(3),
        }),
        Event::FileDeleted(FileDeleted {
            document_id: document_id(),
            path: workspace_path("src/old.rs"),
            previous_hash: hash(4),
        }),
        Event::FileRenamed(FileRenamed {
            document_id: document_id(),
            old_path: workspace_path("src/old.rs"),
            new_path: workspace_path("src/new.rs"),
        }),
        Event::FileFocused(FileFocused {
            document_id: document_id(),
        }),
        Event::FileEdited(FileEdited {
            document_id: document_id(),
            version_before: 2,
            version_after: 3,
            origin: EditOrigin::Keyboard,
            edits: vec![edit()],
            selection_before: SelectionState::new(2, 4),
            selection_after: SelectionState::caret(12),
            hash_before: hash(5),
            hash_after: hash(6),
        }),
        Event::SelectionChanged(SelectionChanged {
            document_id: document_id(),
            anchor_byte: 4,
            active_byte: 9,
        }),
        Event::ViewportChanged(ViewportChanged {
            document_id: document_id(),
            top_line: 10,
            horizontal_column: 2,
        }),
        Event::CargoCommandStarted(CommandStarted {
            command_id: command_id(),
            program: "cargo".to_owned(),
            arguments: vec!["test".to_owned(), "--workspace".to_owned()],
            working_directory: workspace_directory("."),
        }),
        Event::CargoDiagnostic(Diagnostic {
            command_id: command_id(),
            document_id: Some(document_id()),
            severity: DiagnosticSeverity::Error,
            code: Some("E0308".to_owned()),
            message: "mismatched types".to_owned(),
            range: Some(TextRange {
                start_line: 3,
                start_column: 4,
                end_line: 3,
                end_column: 8,
            }),
        }),
        Event::CargoOutput(CommandOutput {
            command_id: command_id(),
            stream: OutputStream::Stderr,
            output: "Compiling rustrace".to_owned(),
        }),
        Event::CargoCommandFinished(CommandFinished {
            command_id: command_id(),
            exit_code: Some(0),
            success: true,
        }),
        Event::LspCompletionRequested(CompletionRequested {
            document_id: document_id(),
            document_version: 3,
            position_byte: 12,
        }),
        Event::LspCompletionAccepted(CompletionAccepted {
            document_id: document_id(),
            document_version: 3,
            label: "println!".to_owned(),
            primary_edit: edit(),
            additional_edits: vec![edit()],
        }),
        Event::LspCodeActionApplied(CodeActionApplied {
            title: "Import std::io".to_owned(),
            kind: Some("quickfix".to_owned()),
            document_ids: vec![document_id()],
        }),
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: hash(7),
            documents: vec![DocumentHash {
                document_id: document_id(),
                hash: hash(8),
            }],
        }),
        Event::ExternalFileChange(ExternalFileChange {
            path: workspace_path("src/main.rs"),
            previous_contents: Some("old".to_owned()),
            new_contents: Some("new".to_owned()),
            previous_hash: Some(hash(9)),
            new_hash: Some(hash(10)),
        }),
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: hash(11),
            event_count: 42,
            clean: false,
            warnings: vec!["external change reviewed".to_owned()],
        }),
    ];

    let expected_events = [
        format!(
            r#"{{"type":"session_started","payload":{{"client_version":"0.1.0","starter_workspace_hash":"{}"}}}}"#,
            hash(1)
        ),
        r#"{"type":"session_resumed","payload":{"last_sequence":8}}"#.to_owned(),
        format!(
            r#"{{"type":"session_ended","payload":{{"final_workspace_hash":"{}"}}}}"#,
            hash(2)
        ),
        format!(
            r#"{{"type":"test_case_compared","payload":{{"command_id":"command-01","case":"sample_1","expected_blake3":"{}","actual_blake3":"{}","outcome":{{"kind":"pass"}}}}}}"#,
            hash(12),
            hash(12)
        ),
        format!(
            r#"{{"type":"test_case_compared","payload":{{"command_id":"command-01","case":"sample_2","expected_blake3":"{}","actual_blake3":"{}","outcome":{{"kind":"mismatch","line":2,"expected_len":3,"actual_len":4}}}}}}"#,
            hash(14),
            hash(15)
        ),
        format!(
            r#"{{"type":"test_case_compared","payload":{{"command_id":"command-01","case":"sample_3","expected_blake3":"{}","actual_blake3":null,"outcome":{{"kind":"error","reason":"launch_failed"}}}}}}"#,
            hash(16)
        ),
        format!(
            r#"{{"type":"file_created","payload":{{"document_id":"document-01","path":"src/main.rs","contents":"fn main() {{}}","content_hash":"{}"}}}}"#,
            hash(3)
        ),
        format!(
            r#"{{"type":"file_deleted","payload":{{"document_id":"document-01","path":"src/old.rs","previous_hash":"{}"}}}}"#,
            hash(4)
        ),
        r#"{"type":"file_renamed","payload":{"document_id":"document-01","old_path":"src/old.rs","new_path":"src/new.rs"}}"#.to_owned(),
        r#"{"type":"file_focused","payload":{"document_id":"document-01"}}"#.to_owned(),
        format!(
            r#"{{"type":"file_edited","payload":{{"document_id":"document-01","version_before":2,"version_after":3,"origin":"keyboard","edits":[{{"start_byte":2,"end_byte":4,"inserted_text":"let x = 1;"}}],"selection_before":{{"anchor_byte":2,"active_byte":4}},"selection_after":{{"anchor_byte":12,"active_byte":12}},"hash_before":"{}","hash_after":"{}"}}}}"#,
            hash(5),
            hash(6)
        ),
        r#"{"type":"selection_changed","payload":{"document_id":"document-01","anchor_byte":4,"active_byte":9}}"#.to_owned(),
        r#"{"type":"viewport_changed","payload":{"document_id":"document-01","top_line":10,"horizontal_column":2}}"#.to_owned(),
        r#"{"type":"cargo_command_started","payload":{"command_id":"command-01","program":"cargo","arguments":["test","--workspace"],"working_directory":"."}}"#.to_owned(),
        r#"{"type":"cargo_diagnostic","payload":{"command_id":"command-01","document_id":"document-01","severity":"error","code":"E0308","message":"mismatched types","range":{"start_line":3,"start_column":4,"end_line":3,"end_column":8}}}"#.to_owned(),
        r#"{"type":"cargo_output","payload":{"command_id":"command-01","stream":"stderr","output":"Compiling rustrace"}}"#.to_owned(),
        r#"{"type":"cargo_command_finished","payload":{"command_id":"command-01","exit_code":0,"success":true}}"#.to_owned(),
        r#"{"type":"lsp_completion_requested","payload":{"document_id":"document-01","document_version":3,"position_byte":12}}"#.to_owned(),
        r#"{"type":"lsp_completion_accepted","payload":{"document_id":"document-01","document_version":3,"label":"println!","primary_edit":{"start_byte":2,"end_byte":4,"inserted_text":"let x = 1;"},"additional_edits":[{"start_byte":2,"end_byte":4,"inserted_text":"let x = 1;"}]}}"#.to_owned(),
        r#"{"type":"lsp_code_action_applied","payload":{"title":"Import std::io","kind":"quickfix","document_ids":["document-01"]}}"#.to_owned(),
        format!(
            r#"{{"type":"workspace_checkpoint","payload":{{"workspace_hash":"{}","documents":[{{"document_id":"document-01","hash":"{}"}}]}}}}"#,
            hash(7),
            hash(8)
        ),
        format!(
            r#"{{"type":"external_file_change","payload":{{"path":"src/main.rs","previous_contents":"old","new_contents":"new","previous_hash":"{}","new_hash":"{}"}}}}"#,
            hash(9),
            hash(10)
        ),
        format!(
            r#"{{"type":"submission_finalized","payload":{{"final_workspace_hash":"{}","event_count":42,"clean":false,"warnings":["external change reviewed"]}}}}"#,
            hash(11)
        ),
    ];

    assert_eq!(events.len(), 23);
    assert_eq!(events.len(), expected_events.len());
    for (event, expected_json) in events.into_iter().zip(expected_events) {
        assert_eq!(serde_json::to_string(&event).unwrap(), expected_json);
        let original = envelope(event);
        let encoded = encode_envelope(&original).unwrap();
        assert_eq!(
            decode(&encoded, DecodePolicy::RejectUnsupported).unwrap(),
            DecodeOutcome::Decoded(original)
        );
    }
}

#[test]
fn every_event_path_field_validates_during_deserialization() {
    let zero_hash = "0".repeat(64);
    let invalid_events = [
        format!(
            r#"{{"type":"file_created","payload":{{"document_id":"document-01","path":"../outside.rs","contents":"","content_hash":"{zero_hash}"}}}}"#
        ),
        format!(
            r#"{{"type":"file_deleted","payload":{{"document_id":"document-01","path":"/tmp/answer.rs","previous_hash":"{zero_hash}"}}}}"#
        ),
        r#"{"type":"file_renamed","payload":{"document_id":"document-01","old_path":"C:old.rs","new_path":"src/new.rs"}}"#.to_owned(),
        r#"{"type":"file_renamed","payload":{"document_id":"document-01","old_path":"src/old.rs","new_path":"src/../new.rs"}}"#.to_owned(),
        r#"{"type":"cargo_command_started","payload":{"command_id":"command-01","program":"cargo","arguments":[],"working_directory":"./"}}"#.to_owned(),
        r#"{"type":"external_file_change","payload":{"path":"\\\\server\\share\\file.rs","previous_contents":null,"new_contents":null,"previous_hash":null,"new_hash":null}}"#.to_owned(),
    ];

    for json in invalid_events {
        let encoded = raw_envelope_with_event(&json);
        assert!(decode_envelope(encoded.as_bytes(), DecodePolicy::RejectUnsupported).is_err());
    }

    let non_nfc = r#"{"type":"external_file_change","payload":{"path":"src/cafe\u0301.rs","previous_contents":null,"new_contents":null,"previous_hash":null,"new_hash":null}}"#;
    let encoded = raw_envelope_with_event(non_nfc);
    let error = decode_envelope(encoded.as_bytes(), DecodePolicy::RejectUnsupported).unwrap_err();
    assert!(error.to_string().contains("NFC"), "{error}");
}

#[test]
fn event_decoding_rejects_backslash_path_aliases() {
    let zero_hash = "0".repeat(64);
    let payload = format!(
        r#"{{"document_id":"document-01","path":"src\\lib.rs","contents":"","content_hash":"{zero_hash}"}}"#
    );
    let encoded = raw_envelope("file_created", &payload, FORMAT_VERSION_V1);

    let error = decode(encoded.as_bytes(), DecodePolicy::RejectUnsupported).unwrap_err();
    assert!(error.to_string().contains("backslash"), "{error}");
}

#[test]
fn unknown_variant_is_rejected_or_explicitly_skipped() {
    let known = encode_envelope(&envelope(Event::FileFocused(FileFocused {
        document_id: document_id(),
    })))
    .unwrap();
    let unknown = String::from_utf8(known)
        .unwrap()
        .replace("file_focused", "future_event");

    assert!(matches!(
        decode(unknown.as_bytes(), DecodePolicy::RejectUnsupported),
        Err(DecodeError::UnknownEventVariant { ref variant }) if variant == "future_event"
    ));
    assert_eq!(
        decode(unknown.as_bytes(), DecodePolicy::SkipUnsupported).unwrap(),
        DecodeOutcome::Skipped(rustrace_model::SkippedEvent {
            format_version: 1,
            sequence: Some(1),
            event_variant: Some("future_event".to_owned()),
            encoded_len: unknown.len(),
            reason: SkipReason::UnknownEventVariant,
        })
    );
}

#[test]
fn unsupported_version_is_rejected_or_explicitly_skipped() {
    let known = encode_envelope(&envelope(Event::FileFocused(FileFocused {
        document_id: document_id(),
    })))
    .unwrap();
    let future = String::from_utf8(known).unwrap().replacen(
        "\"format_version\":1",
        "\"format_version\":2",
        1,
    );

    assert_eq!(
        decode(future.as_bytes(), DecodePolicy::RejectUnsupported),
        Err(DecodeError::UnsupportedVersion {
            found: 2,
            supported: FORMAT_VERSION_V1,
        })
    );
    assert_eq!(
        decode(future.as_bytes(), DecodePolicy::SkipUnsupported).unwrap(),
        DecodeOutcome::Skipped(rustrace_model::SkippedEvent {
            format_version: 2,
            sequence: Some(1),
            event_variant: Some("file_focused".to_owned()),
            encoded_len: future.len(),
            reason: SkipReason::UnsupportedVersion,
        })
    );
}

#[test]
fn identifiers_are_bounded_and_canonical() {
    assert!(SessionId::new("").is_err());
    assert!(DocumentId::new("contains space").is_err());
    assert!(CommandId::new("x".repeat(MAX_IDENTIFIER_BYTES + 1)).is_err());

    let boundary = "x".repeat(MAX_IDENTIFIER_BYTES);
    assert_eq!(SessionId::new(&boundary).unwrap().as_str(), boundary);
    assert_eq!(
        serde_json::to_string(&session_id()).unwrap(),
        "\"session-01\""
    );

    let valid = String::from_utf8(
        encode_envelope(&envelope(Event::FileFocused(FileFocused {
            document_id: document_id(),
        })))
        .unwrap(),
    )
    .unwrap();
    let invalid = valid.replacen("session-01", "contains space", 1);
    assert!(matches!(
        decode(invalid.as_bytes(), DecodePolicy::RejectUnsupported),
        Err(DecodeError::InvalidIdentifier {
            field: "session_id",
            ..
        })
    ));
}

#[test]
fn hashes_have_a_fixed_canonical_wire_encoding() {
    let text = "ab".repeat(32);
    let value = Hash::from_str(&text).unwrap();
    assert_eq!(value.to_string(), text);
    assert_eq!(
        serde_json::to_string(&value).unwrap(),
        format!("\"{text}\"")
    );
    assert!(Hash::from_str("00").is_err());
    assert!(Hash::from_str(&"AB".repeat(32)).is_err());

    let valid = String::from_utf8(
        encode_envelope(&envelope(Event::FileFocused(FileFocused {
            document_id: document_id(),
        })))
        .unwrap(),
    )
    .unwrap();
    let invalid = valid.replacen(&"11".repeat(32), &"gg".repeat(32), 1);
    assert!(matches!(
        decode(invalid.as_bytes(), DecodePolicy::RejectUnsupported),
        Err(DecodeError::InvalidHash {
            field: "event_hash",
            ..
        })
    ));
}

#[test]
fn invalid_sequence_and_timestamps_are_rejected() {
    let mut value = envelope(Event::FileFocused(FileFocused {
        document_id: document_id(),
    }));
    value.sequence = 0;
    assert_eq!(
        encode_envelope(&value),
        Err(rustrace_model::EncodeError::Validation(
            ValidationError::SequenceMustStartAtOne
        ))
    );

    value.sequence = 1;
    value.monotonic_millis = u64::MAX;
    assert!(matches!(
        encode_envelope(&value),
        Err(rustrace_model::EncodeError::Validation(
            ValidationError::MonotonicMillisOutOfRange { .. }
        ))
    ));

    let encoded = String::from_utf8(
        encode_envelope(&envelope(Event::FileFocused(FileFocused {
            document_id: document_id(),
        })))
        .unwrap(),
    )
    .unwrap();
    let invalid = encoded.replace("2026-01-02T03:04:05Z", "not-a-time");
    assert!(matches!(
        decode(invalid.as_bytes(), DecodePolicy::RejectUnsupported),
        Err(DecodeError::InvalidTimestamp { .. })
    ));
}

#[test]
fn raw_envelope_and_nesting_limits_run_before_typed_decode() {
    let oversized = vec![b' '; MAX_ENVELOPE_BYTES + 1];
    assert_eq!(
        decode(&oversized, DecodePolicy::RejectUnsupported),
        Err(DecodeError::EnvelopeTooLarge {
            actual: MAX_ENVELOPE_BYTES + 1,
            maximum: MAX_ENVELOPE_BYTES,
        })
    );

    let nested = format!(
        "{{\"format_version\":1,\"payload\":{}{}}}",
        "[".repeat(MAX_JSON_NESTING),
        "]".repeat(MAX_JSON_NESTING),
    );
    assert!(matches!(
        decode(nested.as_bytes(), DecodePolicy::RejectUnsupported),
        Err(DecodeError::NestingTooDeep { .. })
    ));
}

#[test]
fn semantic_size_limits_are_recursive() {
    let cases = [
        envelope(Event::SessionStarted(SessionStarted {
            client_version: "x".repeat(MAX_STRING_BYTES + 1),
            starter_workspace_hash: hash(1),
        })),
        envelope(Event::FileEdited(FileEdited {
            document_id: document_id(),
            version_before: 1,
            version_after: 2,
            origin: EditOrigin::Paste,
            edits: vec![TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: "x".repeat(MAX_INSERTED_TEXT_BYTES + 1),
            }],
            selection_before: SelectionState::caret(0),
            selection_after: SelectionState::caret(MAX_INSERTED_TEXT_BYTES as u64),
            hash_before: hash(3),
            hash_after: hash(4),
        })),
        envelope(Event::CargoOutput(CommandOutput {
            command_id: command_id(),
            stream: OutputStream::Stdout,
            output: "x".repeat(MAX_OUTPUT_BYTES + 1),
        })),
        envelope(Event::CargoCommandStarted(CommandStarted {
            command_id: command_id(),
            program: "cargo".to_owned(),
            arguments: vec![String::new(); MAX_VECTOR_ITEMS + 1],
            working_directory: workspace_directory("."),
        })),
    ];

    for value in cases {
        assert!(matches!(
            encode_envelope(&value),
            Err(rustrace_model::EncodeError::Validation(_))
        ));
    }
}

#[test]
fn exact_limit_boundary_values_are_accepted() {
    let values = [
        envelope(Event::SessionStarted(SessionStarted {
            client_version: "x".repeat(MAX_STRING_BYTES),
            starter_workspace_hash: hash(1),
        })),
        envelope(Event::FileCreated(FileCreated {
            document_id: document_id(),
            path: maximum_workspace_path(),
            contents: "x".repeat(MAX_INSERTED_TEXT_BYTES),
            content_hash: hash(2),
        })),
        envelope(Event::FileEdited(FileEdited {
            document_id: document_id(),
            version_before: 1,
            version_after: 2,
            origin: EditOrigin::Paste,
            edits: vec![TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: "x".repeat(MAX_INSERTED_TEXT_BYTES),
            }],
            selection_before: SelectionState::caret(0),
            selection_after: SelectionState::caret(MAX_INSERTED_TEXT_BYTES as u64),
            hash_before: hash(3),
            hash_after: hash(4),
        })),
        envelope(Event::CargoOutput(CommandOutput {
            command_id: command_id(),
            stream: OutputStream::Stdout,
            output: "x".repeat(MAX_OUTPUT_BYTES),
        })),
        envelope(Event::CargoCommandStarted(CommandStarted {
            command_id: command_id(),
            program: "cargo".to_owned(),
            arguments: vec![String::new(); MAX_VECTOR_ITEMS],
            working_directory: WorkspaceDirectory::from(maximum_workspace_path()),
        })),
    ];

    for value in values {
        let encoded = encode_envelope(&value).unwrap();
        assert!(matches!(
            decode(&encoded, DecodePolicy::RejectUnsupported).unwrap(),
            DecodeOutcome::Decoded(_)
        ));
    }
}

#[test]
fn invalid_ranges_and_versions_are_rejected() {
    let bad_edit = envelope(Event::FileEdited(FileEdited {
        document_id: document_id(),
        version_before: 4,
        version_after: 4,
        origin: EditOrigin::Formatter,
        edits: vec![TextEdit {
            start_byte: 9,
            end_byte: 2,
            inserted_text: String::new(),
        }],
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(0),
        hash_before: hash(1),
        hash_after: hash(2),
    }));
    assert!(matches!(
        encode_envelope(&bad_edit),
        Err(rustrace_model::EncodeError::Validation(_))
    ));

    let bad_range = envelope(Event::CargoDiagnostic(Diagnostic {
        command_id: command_id(),
        document_id: None,
        severity: DiagnosticSeverity::Warning,
        code: None,
        message: "warning".to_owned(),
        range: Some(TextRange {
            start_line: 5,
            start_column: 1,
            end_line: 4,
            end_column: 9,
        }),
    }));
    assert!(matches!(
        encode_envelope(&bad_range),
        Err(rustrace_model::EncodeError::Validation(_))
    ));
}

#[test]
fn lexical_preflight_caps_containers_and_total_values() {
    let oversized_array = format!("[{}]", vec!["0"; MAX_VECTOR_ITEMS + 1].join(","));
    assert!(matches!(
        decode(oversized_array.as_bytes(), DecodePolicy::RejectUnsupported),
        Err(DecodeError::JsonContainerTooLarge { .. })
    ));

    let oversized_object = format!(
        "{{{}}}",
        (0..=MAX_VECTOR_ITEMS)
            .map(|index| format!("\"key-{index}\":0"))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(matches!(
        decode(oversized_object.as_bytes(), DecodePolicy::RejectUnsupported),
        Err(DecodeError::JsonContainerTooLarge { .. })
    ));

    let inner = format!("[{}]", vec!["0"; MAX_VECTOR_ITEMS].join(","));
    let grouped = format!(
        "[{}]",
        vec![inner; (MAX_JSON_VALUES / MAX_VECTOR_ITEMS) + 1].join(",")
    );
    assert!(grouped.len() < MAX_ENVELOPE_BYTES);
    assert!(matches!(
        decode(grouped.as_bytes(), DecodePolicy::RejectUnsupported),
        Err(DecodeError::JsonValueLimitExceeded { .. })
    ));
}

#[test]
fn lexical_preflight_caps_raw_strings_and_escaped_keys() {
    let oversized_string = format!("\"{}\"", "x".repeat(MAX_JSON_STRING_BYTES + 1));
    assert!(oversized_string.len() < MAX_ENVELOPE_BYTES);
    assert!(matches!(
        decode(oversized_string.as_bytes(), DecodePolicy::RejectUnsupported),
        Err(DecodeError::JsonStringTooLong { .. })
    ));

    let escaped_key = "\\u0061".repeat((MAX_JSON_KEY_BYTES / 6) + 1);
    let oversized_key = format!("{{\"{escaped_key}\":0}}");
    assert!(oversized_key.len() < MAX_ENVELOPE_BYTES);
    assert!(matches!(
        decode(oversized_key.as_bytes(), DecodePolicy::RejectUnsupported),
        Err(DecodeError::JsonStringTooLong { .. })
    ));
}

#[test]
fn lexical_preflight_rejects_duplicates_at_every_object_level() {
    let valid = raw_envelope(
        "file_focused",
        r#"{"document_id":"document-01"}"#,
        FORMAT_VERSION_V1,
    );
    let duplicate_envelope = valid.replacen(
        "\"format_version\":1",
        "\"format_version\":2,\"format_version\":1",
        1,
    );
    let duplicate_event = valid.replacen(
        "\"type\":\"file_focused\"",
        "\"type\":\"future_event\",\"type\":\"file_focused\"",
        1,
    );
    let duplicate_payload = valid.replacen(
        "\"document_id\":\"document-01\"",
        "\"document_id\":\"hidden\",\"document_id\":\"document-01\"",
        1,
    );
    let duplicate_nested = raw_envelope(
        "future_event",
        r#"{"outer":{"key":1,"\u006bey":2}}"#,
        FORMAT_VERSION_V1,
    );

    for json in [
        duplicate_envelope,
        duplicate_event,
        duplicate_payload,
        duplicate_nested,
    ] {
        assert!(matches!(
            decode(json.as_bytes(), DecodePolicy::SkipUnsupported),
            Err(DecodeError::DuplicateObjectKey { .. })
        ));
    }
}

#[test]
fn skip_policy_cannot_bypass_unknown_payload_preflight_limits() {
    let oversized_items = vec!["0"; MAX_VECTOR_ITEMS + 1].join(",");
    let oversized_array = raw_envelope(
        "future_event",
        &format!("{{\"items\":[{oversized_items}]}}"),
        1,
    );
    let oversized_string = raw_envelope(
        "future_event",
        &format!(
            "{{\"opaque\":\"{}\"}}",
            "x".repeat(MAX_JSON_STRING_BYTES + 1)
        ),
        1,
    );

    for json in [oversized_array, oversized_string] {
        assert!(json.len() < MAX_ENVELOPE_BYTES);
        assert!(matches!(
            decode(json.as_bytes(), DecodePolicy::SkipUnsupported),
            Err(DecodeError::JsonContainerTooLarge { .. } | DecodeError::JsonStringTooLong { .. })
        ));
    }
}

#[test]
fn file_edited_enforces_editor_transaction_structure() {
    let transaction = |version_after, edits| {
        envelope(Event::FileEdited(FileEdited {
            document_id: document_id(),
            version_before: 7,
            version_after,
            origin: EditOrigin::CodeAction,
            edits,
            selection_before: SelectionState::new(8, 2),
            selection_after: SelectionState::new(2, 10),
            hash_before: hash(1),
            hash_after: hash(2),
        }))
    };

    assert!(matches!(
        encode_envelope(&transaction(9, vec![edit()])),
        Err(rustrace_model::EncodeError::Validation(
            ValidationError::InvalidVersionTransition {
                before: 7,
                after: 9
            }
        ))
    ));
    let unchecked_jump = serde_json::to_vec(&transaction(9, vec![edit()])).unwrap();
    assert!(matches!(
        decode_envelope(&unchecked_jump, DecodePolicy::RejectUnsupported),
        Err(DecodeError::Validation(
            ValidationError::InvalidVersionTransition {
                before: 7,
                after: 9
            }
        ))
    ));
    assert!(matches!(
        encode_envelope(&transaction(
            8,
            vec![
                TextEdit {
                    start_byte: 4,
                    end_byte: 4,
                    inserted_text: "later".to_owned(),
                },
                TextEdit {
                    start_byte: 2,
                    end_byte: 2,
                    inserted_text: "earlier".to_owned(),
                },
            ],
        )),
        Err(rustrace_model::EncodeError::Validation(
            ValidationError::EditsNotCanonical { .. }
        ))
    ));
    assert!(matches!(
        encode_envelope(&transaction(
            8,
            vec![
                TextEdit {
                    start_byte: 2,
                    end_byte: 5,
                    inserted_text: "first".to_owned(),
                },
                TextEdit {
                    start_byte: 4,
                    end_byte: 6,
                    inserted_text: "overlap".to_owned(),
                },
            ],
        )),
        Err(rustrace_model::EncodeError::Validation(
            ValidationError::EditsOverlap { .. }
        ))
    ));

    let equal_offsets = transaction(
        8,
        vec![
            TextEdit {
                start_byte: 2,
                end_byte: 2,
                inserted_text: "🦀".to_owned(),
            },
            TextEdit {
                start_byte: 2,
                end_byte: 2,
                inserted_text: "界".to_owned(),
            },
            TextEdit {
                start_byte: 2,
                end_byte: 5,
                inserted_text: "é".to_owned(),
            },
        ],
    );
    let encoded = encode_envelope(&equal_offsets).unwrap();
    assert_eq!(
        decode_envelope(&encoded, DecodePolicy::RejectUnsupported).unwrap(),
        DecodeOutcome::Decoded(equal_offsets)
    );

    let missing_provenance = String::from_utf8(encoded)
        .unwrap()
        .replace("\"origin\":\"code_action\",", "");
    assert!(matches!(
        decode_envelope(
            missing_provenance.as_bytes(),
            DecodePolicy::RejectUnsupported
        ),
        Err(DecodeError::InvalidEnvelope { .. })
    ));
}

#[test]
fn file_edited_budget_reserves_exact_worst_case_envelope_overhead() {
    let maximum_wall_clock = chrono::DateTime::<Utc>::MAX_UTC;
    let minimum_wall_clock = chrono::DateTime::<Utc>::MIN_UTC;
    assert_eq!(
        serde_json::to_string(&maximum_wall_clock).unwrap().len(),
        35
    );
    assert_eq!(
        serde_json::to_string(&minimum_wall_clock).unwrap().len(),
        25
    );
    assert_eq!(
        serde_json::to_string(&Option::<chrono::DateTime<Utc>>::None)
            .unwrap()
            .len(),
        4
    );

    let transaction = FileEdited {
        document_id: document_id(),
        version_before: 0,
        version_after: 1,
        origin: EditOrigin::Formatter,
        edits: vec![TextEdit {
            start_byte: 0,
            end_byte: 0,
            inserted_text: String::new(),
        }],
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(0),
        hash_before: hash(1),
        hash_after: hash(2),
    };
    let transaction_len = serde_json::to_vec(&transaction).unwrap().len();
    let worst_case = EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: SessionId::new("s".repeat(MAX_IDENTIFIER_BYTES)).unwrap(),
        sequence: u64::MAX,
        monotonic_millis: MAX_MONOTONIC_MILLIS,
        wall_clock_utc: Some(maximum_wall_clock),
        previous_event_hash: Hash::from_bytes([0xff; Hash::LENGTH]),
        event_hash: Hash::from_bytes([0xff; Hash::LENGTH]),
        event: Event::FileEdited(transaction),
    };
    worst_case.validate().unwrap();
    let envelope_len = serde_json::to_vec(&worst_case).unwrap().len();

    assert_eq!(
        envelope_len - transaction_len,
        FILE_EDITED_ENVELOPE_MAX_OVERHEAD_BYTES
    );
    assert_eq!(
        MAX_FILE_EDITED_TRANSACTION_BYTES + FILE_EDITED_ENVELOPE_MAX_OVERHEAD_BYTES,
        MAX_ENVELOPE_BYTES
    );
    assert!(envelope_len - transaction_len < 1024);
}

#[test]
fn canonical_envelope_encoding_stops_at_exact_byte_limit() {
    let maximum_text = "x".repeat(MAX_INSERTED_TEXT_BYTES);
    let mut exact = envelope(Event::FileEdited(FileEdited {
        document_id: document_id(),
        version_before: 0,
        version_after: 1,
        origin: EditOrigin::Formatter,
        edits: vec![
            TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: maximum_text.clone(),
            },
            TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: maximum_text.clone(),
            },
            TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: maximum_text,
            },
            TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: String::new(),
            },
        ],
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(0),
        hash_before: hash(1),
        hash_after: hash(2),
    }));
    let base_len = serde_json::to_vec(&exact).unwrap().len();
    let exact_tail = MAX_ENVELOPE_BYTES - base_len;
    assert!(exact_tail <= MAX_INSERTED_TEXT_BYTES);
    let Event::FileEdited(transaction) = &mut exact.event else {
        unreachable!();
    };
    transaction.edits[3].inserted_text = "y".repeat(exact_tail);

    let encoded = encode_envelope(&exact).unwrap();
    assert_eq!(encoded.len(), MAX_ENVELOPE_BYTES);
    assert_eq!(
        decode_envelope(&encoded, DecodePolicy::RejectUnsupported).unwrap(),
        DecodeOutcome::Decoded(exact.clone())
    );

    let Event::FileEdited(transaction) = &mut exact.event else {
        unreachable!();
    };
    transaction.edits[3].inserted_text.push('y');
    assert!(matches!(
        encode_envelope(&exact),
        Err(rustrace_model::EncodeError::EnvelopeTooLarge { .. })
    ));

    let escape_heavy = envelope(Event::FileEdited(FileEdited {
        document_id: document_id(),
        version_before: 0,
        version_after: 1,
        origin: EditOrigin::Paste,
        edits: (0..8)
            .map(|_| TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: "\n".repeat(MAX_INSERTED_TEXT_BYTES),
            })
            .collect(),
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(0),
        hash_before: hash(1),
        hash_after: hash(2),
    }));
    assert!(matches!(
        encode_envelope(&escape_heavy),
        Err(rustrace_model::EncodeError::EnvelopeTooLarge { .. })
    ));
}

trait AmbiguousIfDeserialize<Marker> {
    fn marker() {}
}

impl<T: ?Sized> AmbiguousIfDeserialize<()> for T {}
impl<T: serde::de::DeserializeOwned> AmbiguousIfDeserialize<u8> for T {}

#[test]
fn persisted_event_types_do_not_expose_unbounded_serde_deserialization() {
    let _ = <Event as AmbiguousIfDeserialize<_>>::marker;
    let _ = <EventEnvelope as AmbiguousIfDeserialize<_>>::marker;
    let _ = <FileEdited as AmbiguousIfDeserialize<_>>::marker;
}
