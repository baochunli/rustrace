//! Lifecycle fixtures use existing decoder/replay APIs. Before implementation,
//! the valid sequence fails at the unknown new start event (behavioral wire Red).
use rustrace_journal::{CheckpointFile, CheckpointSnapshot, OpenDocument, StoredCheckpoint};
use rustrace_model::*;
use rustrace_replay::ReplayEngine;
use serde_json::{Value, json};

fn initial() -> ReplayEngine {
    let snapshot = CheckpointSnapshot::new(
        SessionId::new("runner").unwrap(),
        1,
        vec![CheckpointFile {
            path: WorkspacePath::new("main.rs").unwrap(),
            contents: b"A".to_vec(),
        }],
        Some(DocumentId::new("main").unwrap()),
        vec![OpenDocument {
            document_id: DocumentId::new("main").unwrap(),
            path: WorkspacePath::new("main.rs").unwrap(),
            version: 0,
            selection: SelectionState::caret(0),
        }],
    )
    .unwrap();
    let owning_event = EventEnvelope {
        format_version: 1,
        session_id: SessionId::new("runner").unwrap(),
        sequence: 1,
        monotonic_millis: 10,
        wall_clock_utc: None,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event: Event::WorkspaceCheckpoint(snapshot.event_payload()),
    }
    .seal(Hash::zero())
    .unwrap();
    ReplayEngine::from_initial_checkpoint(StoredCheckpoint {
        owning_event,
        snapshot,
    })
    .unwrap()
}

fn dependency_initial() -> ReplayEngine {
    let files = [
        ("Cargo.lock", "lock-before\n", "lock"),
        (
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n[workspace]\n",
            "manifest",
        ),
        ("main.rs", "A", "main"),
    ];
    let snapshot = CheckpointSnapshot::new(
        SessionId::new("runner").unwrap(),
        1,
        files
            .iter()
            .map(|(path, contents, _)| CheckpointFile {
                path: WorkspacePath::new(*path).unwrap(),
                contents: contents.as_bytes().to_vec(),
            })
            .collect(),
        Some(DocumentId::new("main").unwrap()),
        files
            .iter()
            .map(|(path, _, document_id)| OpenDocument {
                document_id: DocumentId::new(*document_id).unwrap(),
                path: WorkspacePath::new(*path).unwrap(),
                version: 0,
                selection: SelectionState::caret(0),
            })
            .collect(),
    )
    .unwrap();
    let owning_event = EventEnvelope {
        format_version: 1,
        session_id: SessionId::new("runner").unwrap(),
        sequence: 1,
        monotonic_millis: 10,
        wall_clock_utc: None,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event: Event::WorkspaceCheckpoint(snapshot.event_payload()),
    }
    .seal(Hash::zero())
    .unwrap();
    ReplayEngine::from_initial_checkpoint(StoredCheckpoint {
        owning_event,
        snapshot,
    })
    .unwrap()
}

fn link(replay: &ReplayEngine, sequence: u64) -> Value {
    json!({"checkpoint_sequence": sequence, "checkpoint_event_hash": replay.last_event_hash(),
        "workspace_hash": replay.current_workspace_hash(), "workspace_version": 1})
}

fn start(replay: &ReplayEngine) -> Value {
    json!({"type": "controlled_command_started", "payload": {
        "command_id": "command-2", "action": "check",
        "argv": ["/trusted/rustup", "run", "fixture", "/trusted/cargo", "check", "--frozen"],
        "environment": {"policy_version": 1, "retained_names": ["HOME", "PATH"]},
        "selected_toolchain": "fixture",
        "tools": [
            {"component": "rustup", "executable": "/trusted/rustup", "version": "rustup 1.28.1 (fixture)"},
            {"component": "rustc", "executable": "/trusted/rustc", "version": "rustc 1.98.1 (fixture)\nhost: fixture\nrelease: 1.98.1"},
            {"component": "cargo", "executable": "/trusted/cargo", "version": "cargo 1.98.1 (fixture)"},
            {"component": "rustdoc", "executable": "/trusted/rustdoc", "version": "rustdoc 1.98.1 (fixture)"}
        ], "before": link(replay, 1), "deadline_millis": 300000, "output_limit": 8388608
    }})
}

fn console_file_start(replay: &ReplayEngine) -> Value {
    let mut value = start(replay);
    value["payload"]["action"] = json!("run");
    value["payload"]["argv"] = json!([
        "/trusted/rustup",
        "run",
        "fixture",
        "/trusted/cargo",
        "run",
        "--frozen"
    ]);
    value["payload"]["console"] = json!({
        "stdin":{"kind":"file","path":"input.txt"},
        "stdout":{"kind":"file","path":"output.txt"}
    });
    value
}

fn test_case_start(replay: &ReplayEngine) -> Value {
    let mut value = start(replay);
    value["payload"]["action"] = json!("run");
    value["payload"]["argv"] = json!([
        "/trusted/rustup",
        "run",
        "fixture",
        "/trusted/cargo",
        "run",
        "--locked"
    ]);
    value["payload"]["console"] = json!({
        "stdin":{"kind":"file","path":"sample.in"},
        "stdout":{"kind":"console"}
    });
    value
}

fn through_test_case_finish(exit_code: i32, completeness: &str) -> ReplayEngine {
    let mut replay = initial();
    let event = test_case_start(&replay);
    apply(&mut replay, 2, event);
    apply(
        &mut replay,
        3,
        json!({"type": "controlled_command_output", "payload": {
            "command_id": "command-2", "stream": "stdout", "offset": 0,
            "bytes_hex": "ff001b"
        }}),
    );
    post_checkpoint(&mut replay);
    let mut value = finish(&replay);
    value["payload"]["outcome"] = json!({"kind":"exited","code":exit_code});
    value["payload"]["stdout"]["completeness"] = json!(completeness);
    apply(&mut replay, 5, value);
    replay
}

fn comparison(outcome: Value, actual_blake3: Option<Hash>) -> Value {
    let expected_blake3 = if outcome["kind"] == "pass" {
        actual_blake3.expect("PASS fixture has actual bytes")
    } else {
        Hash::from_bytes([42; Hash::LENGTH])
    };
    json!({"type":"test_case_compared","payload":{
        "command_id":"command-2","case":"sample",
        "expected_blake3":expected_blake3,
        "actual_blake3":actual_blake3,
        "outcome":outcome
    }})
}

fn captured_stdout_hash() -> Hash {
    Hash::from_bytes(*blake3::hash(&[0xff, 0x00, 0x1b]).as_bytes())
}

fn format_start(replay: &ReplayEngine) -> Value {
    json!({"type": "controlled_command_started", "payload": {
        "command_id": "command-2", "action": "format",
        "argv": ["/trusted/rustup", "run", "fixture", "/trusted/cargo-fmt", "fmt"],
        "environment": {"policy_version": 1, "retained_names": ["HOME", "PATH"]},
        "selected_toolchain": "fixture",
        "tools": [
            {"component": "rustup", "executable": "/trusted/rustup", "version": "rustup 1.28.1 (fixture)"},
            {"component": "rustc", "executable": "/trusted/rustc", "version": "rustc 1.98.1 (fixture)\nhost: fixture\nrelease: 1.98.1"},
            {"component": "cargo", "executable": "/trusted/cargo", "version": "cargo 1.98.1 (fixture)"},
            {"component": "rustdoc", "executable": "/trusted/rustdoc", "version": "rustdoc 1.98.1 (fixture)"},
            {"component": "cargo_fmt", "executable": "/trusted/cargo-fmt", "version": "rustfmt 1.98.1 (fixture)"},
            {"component": "rustfmt", "executable": "/trusted/rustfmt", "version": "rustfmt 1.98.1 (fixture)"}
        ], "before": link(replay, 1), "deadline_millis": 300000, "output_limit": 8388608
    }})
}

fn formatter_edit() -> Value {
    serde_json::to_value(Event::FileEdited(EditorTransaction {
        document_id: DocumentId::new("main").unwrap(),
        version_before: 0,
        version_after: 1,
        origin: EditOrigin::Formatter,
        edits: vec![TextEdit {
            start_byte: 0,
            end_byte: 1,
            inserted_text: "B".to_owned(),
        }],
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(0),
        hash_before: document_hash("A"),
        hash_after: document_hash("B"),
    }))
    .unwrap()
}

fn dependency_start(replay: &ReplayEngine) -> Value {
    let mut value = start(replay);
    value["payload"]["action"] = json!("add");
    value["payload"]["argv"] = json!([
        "/trusted/rustup",
        "run",
        "fixture",
        "/trusted/cargo",
        "add",
        "serde@1.0.229"
    ]);
    value
}

fn dependency_edit(document_id: &str, before: &str, after: &str) -> Value {
    serde_json::to_value(Event::FileEdited(EditorTransaction {
        document_id: DocumentId::new(document_id).unwrap(),
        version_before: 0,
        version_after: 1,
        origin: EditOrigin::DependencyTool,
        edits: vec![TextEdit {
            start_byte: 0,
            end_byte: before.len() as u64,
            inserted_text: after.to_owned(),
        }],
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(0),
        hash_before: document_hash(before),
        hash_after: document_hash(after),
    }))
    .unwrap()
}

fn decode(replay: &ReplayEngine, sequence: u64, value: Value) -> EventEnvelope {
    let bytes = serde_json::to_vec(
        &json!({"format_version": 1, "session_id": replay.session_id(),
        "sequence": sequence, "monotonic_millis": sequence * 10, "wall_clock_utc": null,
        "previous_event_hash": Hash::zero(), "event_hash": Hash::zero(), "event": value}),
    )
    .unwrap();
    let result = decode_envelope(&bytes, DecodePolicy::RejectUnsupported);
    assert!(
        result.is_ok(),
        "required command lifecycle evidence must decode: {result:?}"
    );
    let DecodeOutcome::Decoded(event) = result.unwrap() else {
        panic!("not decoded")
    };
    event.seal(replay.last_event_hash()).unwrap()
}

fn unchecked_event_hash(previous: Hash, envelope: &EventEnvelope) -> Hash {
    let material = format!(
        concat!(
            "{{\"format_version\":{},\"session_id\":{},\"sequence\":{},",
            "\"monotonic_millis\":{},\"wall_clock_utc\":{},\"event\":{}}}"
        ),
        envelope.format_version,
        serde_json::to_string(&envelope.session_id).unwrap(),
        envelope.sequence,
        envelope.monotonic_millis,
        serde_json::to_string(&envelope.wall_clock_utc).unwrap(),
        serde_json::to_string(&envelope.event).unwrap(),
    );
    let mut hasher = blake3::Hasher::new();
    hasher.update(previous.as_bytes());
    hasher.update(&(material.len() as u64).to_be_bytes());
    hasher.update(material.as_bytes());
    Hash::from_bytes(*hasher.finalize().as_bytes())
}

fn apply(replay: &mut ReplayEngine, sequence: u64, value: Value) {
    let event = decode(replay, sequence, value);
    replay.apply(&event).unwrap();
}

fn through_output() -> ReplayEngine {
    let mut replay = initial();
    let event = start(&replay);
    apply(&mut replay, 2, event);
    apply(
        &mut replay,
        3,
        json!({"type": "controlled_command_output", "payload": {
        "command_id": "command-2", "stream": "stdout", "offset": 0, "bytes_hex": "ff001b"}}),
    );
    replay
}

fn post_checkpoint(replay: &mut ReplayEngine) {
    let snapshot = CheckpointSnapshot::new(
        SessionId::new("runner").unwrap(),
        4,
        vec![CheckpointFile {
            path: WorkspacePath::new("main.rs").unwrap(),
            contents: b"A".to_vec(),
        }],
        Some(DocumentId::new("main").unwrap()),
        vec![OpenDocument {
            document_id: DocumentId::new("main").unwrap(),
            path: WorkspacePath::new("main.rs").unwrap(),
            version: 0,
            selection: SelectionState::caret(0),
        }],
    )
    .unwrap();
    apply(
        replay,
        4,
        serde_json::to_value(Event::WorkspaceCheckpoint(snapshot.event_payload())).unwrap(),
    );
}

fn formatter_post_checkpoint(replay: &mut ReplayEngine) -> StoredCheckpoint {
    let snapshot = CheckpointSnapshot::new(
        replay.session_id().clone(),
        4,
        vec![CheckpointFile {
            path: WorkspacePath::new("main.rs").unwrap(),
            contents: b"B".to_vec(),
        }],
        Some(DocumentId::new("main").unwrap()),
        vec![OpenDocument {
            document_id: DocumentId::new("main").unwrap(),
            path: WorkspacePath::new("main.rs").unwrap(),
            version: 1,
            selection: SelectionState::caret(0),
        }],
    )
    .unwrap();
    let owning_event = decode(
        replay,
        4,
        serde_json::to_value(Event::WorkspaceCheckpoint(snapshot.event_payload())).unwrap(),
    );
    replay.apply(&owning_event).unwrap();
    StoredCheckpoint {
        owning_event,
        snapshot,
    }
}

fn finish(replay: &ReplayEngine) -> Value {
    json!({"type": "controlled_command_finished", "payload": {
        "command_id": "command-2", "after": link(replay, 4),
        "started_millis": 20, "finished_millis": 40,
        "outcome": {"kind": "exited", "code": 7},
        "stdout": {"bytes": 3, "completeness": "complete"},
        "stderr": {"bytes": 0, "completeness": "complete"}
    }})
}

fn console_file_finish(replay: &ReplayEngine) -> Value {
    json!({"type": "controlled_command_finished", "payload": {
        "command_id": "command-2", "after": link(replay, 4),
        "started_millis": 20, "finished_millis": 40,
        "outcome": {"kind": "exited", "code": 0},
        "stdout": {"bytes": 0, "completeness": "unavailable", "mode":"redirected"},
        "stderr": {"bytes": 3, "completeness": "complete"}
    }})
}

#[test]
fn redirected_stdout_replay_requires_unavailable_capture_and_no_stdout_chunks() {
    let mut replay = initial();
    let start = console_file_start(&replay);
    apply(&mut replay, 2, start);
    let rejected = decode(
        &replay,
        3,
        json!({"type":"controlled_command_output","payload":{
            "command_id":"command-2","stream":"stdout","offset":0,"bytes_hex":"01"
        }}),
    );
    assert!(replay.apply(&rejected).is_err());
    apply(
        &mut replay,
        3,
        json!({"type":"controlled_command_output","payload":{
            "command_id":"command-2","stream":"stderr","offset":0,"bytes_hex":"657272"
        }}),
    );
    post_checkpoint(&mut replay);

    let mut wrong = console_file_finish(&replay);
    wrong["payload"]["stdout"] = json!({"bytes":0,"completeness":"complete"});
    let rejected = decode(&replay, 5, wrong);
    assert!(replay.apply(&rejected).is_err());

    let finish = console_file_finish(&replay);
    apply(&mut replay, 5, finish);
    assert!(!replay.controlled_command_pending());
    assert_eq!(replay.command_output_bytes(), 3);
}

#[test]
fn comparison_replay_accepts_one_immediately_linked_typed_result() {
    for outcome in [
        json!({"kind":"pass"}),
        json!({"kind":"mismatch","line":1,"expected_len":3,"actual_len":3}),
    ] {
        let mut replay = through_test_case_finish(0, "complete");
        apply(
            &mut replay,
            6,
            comparison(outcome, Some(captured_stdout_hash())),
        );
    }

    let mut replay = through_test_case_finish(7, "complete");
    apply(
        &mut replay,
        6,
        comparison(
            json!({"kind":"error","reason":"nonzero_exit"}),
            Some(captured_stdout_hash()),
        ),
    );
}

#[test]
fn comparison_replay_rejects_combined_expected_size_without_advancing_cursor() {
    let mut replay = initial();
    let start = test_case_start(&replay);
    apply(&mut replay, 2, start);
    apply(
        &mut replay,
        3,
        json!({"type":"controlled_command_output","payload":{
            "command_id":"command-2","stream":"stdout","offset":0,"bytes_hex":"0a58"
        }}),
    );
    post_checkpoint(&mut replay);
    let mut finished = finish(&replay);
    finished["payload"]["outcome"] = json!({"kind":"exited","code":0});
    finished["payload"]["stdout"] = json!({"bytes":2,"completeness":"complete"});
    apply(&mut replay, 5, finished);

    let actual_hash = Hash::from_bytes(*blake3::hash(b"\nX").as_bytes());
    let valid = decode(
        &replay,
        6,
        comparison(
            json!({"kind":"mismatch","line":2,
            "expected_len":MAX_TEST_CASE_EXPECTED_LINE_BYTES - 1,"actual_len":1}),
            Some(actual_hash),
        ),
    );
    assert_eq!(
        unchecked_event_hash(valid.previous_event_hash, &valid),
        valid.event_hash
    );
    // Construct correctly hashed, invalid typed evidence without using seal or
    // decode, which must themselves reject it once model validation is fixed.
    let mut forged = valid.clone();
    let Event::TestCaseCompared(comparison) = &mut forged.event else {
        unreachable!()
    };
    let TestCaseComparisonOutcome::Mismatch { expected_len, .. } = &mut comparison.outcome else {
        unreachable!()
    };
    *expected_len = MAX_TEST_CASE_EXPECTED_LINE_BYTES;
    forged.event_hash = unchecked_event_hash(forged.previous_event_hash, &forged);

    let before_sequence = replay.next_sequence();
    let before_hash = replay.last_event_hash();
    let before_output = replay.command_output_bytes();
    let before_workspace_hash = replay.current_workspace_hash();
    assert!(
        replay.apply(&forged).is_err(),
        "line 2 needs at least one preceding expected LF byte"
    );
    assert_eq!(replay.next_sequence(), before_sequence);
    assert_eq!(replay.last_event_hash(), before_hash);
    assert_eq!(replay.command_output_bytes(), before_output);
    assert_eq!(replay.current_workspace_hash(), before_workspace_hash);
    replay.apply(&valid).unwrap();
    assert_eq!(replay.next_sequence(), before_sequence + 1);
    assert_eq!(replay.last_event_hash(), valid.event_hash);
}

#[test]
fn comparison_pass_stdout_must_fit_expected_file_but_mismatch_may_exceed_it() {
    for size in [
        MAX_TEST_CASE_EXPECTED_LINE_BYTES,
        MAX_TEST_CASE_EXPECTED_LINE_BYTES + 1,
    ] {
        let mut replay = initial();
        let start = test_case_start(&replay);
        apply(&mut replay, 2, start);
        let stdout = vec![b'X'; size as usize];
        for (index, chunk) in stdout.chunks(MAX_COMMAND_CHUNK_BYTES).enumerate() {
            let sequence = replay.next_sequence();
            apply(
                &mut replay,
                sequence,
                json!({"type":"controlled_command_output","payload":{
                    "command_id":"command-2","stream":"stdout",
                    "offset":index * MAX_COMMAND_CHUNK_BYTES,"bytes_hex":"58".repeat(chunk.len())
                }}),
            );
        }
        let checkpoint_sequence = replay.next_sequence();
        let snapshot = CheckpointSnapshot::new(
            replay.session_id().clone(),
            checkpoint_sequence,
            vec![CheckpointFile {
                path: WorkspacePath::new("main.rs").unwrap(),
                contents: b"A".to_vec(),
            }],
            Some(DocumentId::new("main").unwrap()),
            vec![OpenDocument {
                document_id: DocumentId::new("main").unwrap(),
                path: WorkspacePath::new("main.rs").unwrap(),
                version: 0,
                selection: SelectionState::caret(0),
            }],
        )
        .unwrap();
        apply(
            &mut replay,
            checkpoint_sequence,
            serde_json::to_value(Event::WorkspaceCheckpoint(snapshot.event_payload())).unwrap(),
        );
        let mut finished = finish(&replay);
        finished["payload"]["after"] =
            serde_json::to_value(replay.command_tree_link().unwrap()).unwrap();
        finished["payload"]["finished_millis"] = json!(checkpoint_sequence * 10);
        finished["payload"]["outcome"] = json!({"kind":"exited","code":0});
        finished["payload"]["stdout"] = json!({"bytes":size,"completeness":"complete"});
        let finish_sequence = replay.next_sequence();
        apply(&mut replay, finish_sequence, finished);

        let actual_hash = Hash::from_bytes(*blake3::hash(&stdout).as_bytes());
        let before_sequence = replay.next_sequence();
        let before_hash = replay.last_event_hash();
        let before_output = replay.command_output_bytes();
        let before_workspace_hash = replay.current_workspace_hash();
        let pass = decode(
            &replay,
            before_sequence,
            comparison(json!({"kind":"pass"}), Some(actual_hash)),
        );
        if size == MAX_TEST_CASE_EXPECTED_LINE_BYTES {
            replay.apply(&pass).unwrap();
            assert_eq!(replay.next_sequence(), before_sequence + 1);
            assert_eq!(replay.last_event_hash(), pass.event_hash);
        } else {
            assert!(
                replay.apply(&pass).is_err(),
                "PASS cannot equal an oversized expected snapshot"
            );
            assert_eq!(replay.next_sequence(), before_sequence);
            assert_eq!(replay.last_event_hash(), before_hash);
            assert_eq!(replay.command_output_bytes(), before_output);
            assert_eq!(replay.current_workspace_hash(), before_workspace_hash);
            let mismatch = decode(
                &replay,
                before_sequence,
                comparison(
                    json!({"kind":"mismatch","line":1,"expected_len":MAX_TEST_CASE_EXPECTED_LINE_BYTES,"actual_len":size}),
                    Some(actual_hash),
                ),
            );
            replay.apply(&mismatch).unwrap();
            assert_eq!(replay.next_sequence(), before_sequence + 1);
            assert_eq!(replay.last_event_hash(), mismatch.event_hash);
        }
    }
}

#[test]
fn comparison_replay_rejects_unlinked_duplicate_or_forged_results() {
    let invalid = [
        ("command_id", json!("other")),
        ("case", json!("other")),
        ("actual_blake3", json!(Hash::from_bytes([9; Hash::LENGTH]))),
    ];
    for (field, value) in invalid {
        let mut replay = through_test_case_finish(0, "complete");
        let mut event = comparison(json!({"kind":"pass"}), Some(captured_stdout_hash()));
        event["payload"][field] = value.clone();
        if field == "actual_blake3" {
            event["payload"]["expected_blake3"] = value;
        }
        let envelope = decode(&replay, 6, event);
        let before = replay.last_event_hash();
        assert!(replay.apply(&envelope).is_err(), "accepted forged {field}");
        assert_eq!(replay.last_event_hash(), before);
    }

    let mut replay = through_test_case_finish(0, "complete");
    apply(
        &mut replay,
        6,
        comparison(json!({"kind":"pass"}), Some(captured_stdout_hash())),
    );
    let duplicate = decode(
        &replay,
        7,
        comparison(json!({"kind":"pass"}), Some(captured_stdout_hash())),
    );
    assert!(replay.apply(&duplicate).is_err());

    let mut replay = through_test_case_finish(0, "complete");
    apply(
        &mut replay,
        6,
        json!({"type":"file_focused","payload":{"document_id":"main"}}),
    );
    let delayed = decode(
        &replay,
        7,
        comparison(json!({"kind":"pass"}), Some(captured_stdout_hash())),
    );
    assert!(replay.apply(&delayed).is_err());
}

#[test]
fn comparison_replay_rejects_intervening_mutations_and_checkpoints() {
    let edit = Event::FileEdited(EditorTransaction {
        document_id: DocumentId::new("main").unwrap(),
        version_before: 0,
        version_after: 1,
        origin: EditOrigin::Keyboard,
        edits: vec![TextEdit {
            start_byte: 0,
            end_byte: 1,
            inserted_text: "B".to_owned(),
        }],
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(0),
        hash_before: document_hash("A"),
        hash_after: document_hash("B"),
    });
    let checkpoint = CheckpointSnapshot::new(
        SessionId::new("runner").unwrap(),
        6,
        vec![CheckpointFile {
            path: WorkspacePath::new("main.rs").unwrap(),
            contents: b"A".to_vec(),
        }],
        Some(DocumentId::new("main").unwrap()),
        vec![OpenDocument {
            document_id: DocumentId::new("main").unwrap(),
            path: WorkspacePath::new("main.rs").unwrap(),
            version: 0,
            selection: SelectionState::caret(0),
        }],
    )
    .unwrap();
    for intervening in [edit, Event::WorkspaceCheckpoint(checkpoint.event_payload())] {
        let mut replay = through_test_case_finish(0, "complete");
        apply(&mut replay, 6, serde_json::to_value(intervening).unwrap());
        let delayed = decode(
            &replay,
            7,
            comparison(json!({"kind":"pass"}), Some(captured_stdout_hash())),
        );
        assert!(
            replay.apply(&delayed).is_err(),
            "comparison must name the immediately preceding finish"
        );
    }
}

#[test]
fn comparison_replay_requires_the_matching_run_and_capture_classification() {
    let mut nonzero = through_test_case_finish(7, "complete");
    let pass = decode(
        &nonzero,
        6,
        comparison(json!({"kind":"pass"}), Some(captured_stdout_hash())),
    );
    assert!(nonzero.apply(&pass).is_err());

    let mut incomplete = through_test_case_finish(0, "truncated");
    let mismatch = decode(
        &incomplete,
        6,
        comparison(
            json!({"kind":"mismatch","line":1,"expected_len":3,"actual_len":3}),
            Some(captured_stdout_hash()),
        ),
    );
    assert!(incomplete.apply(&mismatch).is_err());

    let mut wrong_error = through_test_case_finish(7, "complete");
    let event = decode(
        &wrong_error,
        6,
        comparison(
            json!({"kind":"error","reason":"capture_truncated"}),
            Some(captured_stdout_hash()),
        ),
    );
    assert!(wrong_error.apply(&event).is_err());

    let mut replay = through_output();
    post_checkpoint(&mut replay);
    let event = finish(&replay);
    apply(&mut replay, 5, event);
    let not_a_run = decode(
        &replay,
        6,
        comparison(
            json!({"kind":"error","reason":"nonzero_exit"}),
            Some(captured_stdout_hash()),
        ),
    );
    assert!(replay.apply(&not_a_run).is_err());

    let mut replay = initial();
    let start = console_file_start(&replay);
    apply(&mut replay, 2, start);
    apply(
        &mut replay,
        3,
        json!({"type":"controlled_command_output","payload":{
            "command_id":"command-2","stream":"stderr","offset":0,"bytes_hex":"657272"
        }}),
    );
    post_checkpoint(&mut replay);
    let finish = console_file_finish(&replay);
    apply(&mut replay, 5, finish);
    let redirected = decode(
        &replay,
        6,
        comparison(json!({"kind":"error","reason":"capture_unavailable"}), None),
    );
    assert!(replay.apply(&redirected).is_err());
}

#[test]
fn comparison_after_terminal_event_is_rejected() {
    let mut replay = through_test_case_finish(0, "complete");
    let final_workspace_hash = replay.current_workspace_hash();
    apply(
        &mut replay,
        6,
        json!({"type":"session_ended","payload":{
            "final_workspace_hash":final_workspace_hash
        }}),
    );
    let event = decode(
        &replay,
        7,
        comparison(json!({"kind":"pass"}), Some(captured_stdout_hash())),
    );
    assert!(matches!(
        replay.apply(&event),
        Err(rustrace_replay::ReplayError::EventAfterTerminal { sequence: 7 })
    ));
}

fn format_finish(replay: &ReplayEngine) -> Value {
    let after = serde_json::to_value(replay.command_tree_link().unwrap()).unwrap();
    json!({"type": "controlled_command_finished", "payload": {
        "command_id": "command-2", "after": after,
        "started_millis": 20, "finished_millis": 40,
        "outcome": {"kind": "exited", "code": 0},
        "stdout": {"bytes": 0, "completeness": "complete"},
        "stderr": {"bytes": 0, "completeness": "complete"}
    }})
}

#[test]
fn exact_nonzero_lifecycle_replays_without_executing_any_source() {
    let mut replay = through_output();
    post_checkpoint(&mut replay);
    let event = finish(&replay);
    apply(&mut replay, 5, event);
    assert_eq!(
        replay
            .workspace_state()
            .file(&WorkspacePath::new("main.rs").unwrap()),
        Some(b"A".as_slice())
    );
}

#[test]
fn formatter_edits_require_the_active_format_operation() {
    let mut replay = initial();
    let edit = decode(&replay, 2, formatter_edit());
    let before = replay.last_event_hash();
    assert!(replay.apply(&edit).is_err());
    assert_eq!(replay.last_event_hash(), before);

    let event = start(&replay);
    apply(&mut replay, 2, event);
    let edit = decode(&replay, 3, formatter_edit());
    let before = replay.last_event_hash();
    assert!(replay.apply(&edit).is_err());
    assert_eq!(replay.last_event_hash(), before);
}

#[test]
fn dependency_edits_require_the_matching_active_dependency_operation() {
    let manifest_before = "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n[workspace]\n";
    let manifest_after = format!("{manifest_before}\n[dependencies]\nserde = \"1\"\n");

    let mut replay = dependency_initial();
    let edit = decode(
        &replay,
        2,
        dependency_edit("manifest", manifest_before, &manifest_after),
    );
    assert!(replay.apply(&edit).is_err());

    let event = start(&replay);
    apply(&mut replay, 2, event);
    let edit = decode(
        &replay,
        3,
        dependency_edit("manifest", manifest_before, &manifest_after),
    );
    assert!(replay.apply(&edit).is_err());

    let mut replay = dependency_initial();
    let event = dependency_start(&replay);
    apply(&mut replay, 2, event);
    let source_edit = decode(&replay, 3, dependency_edit("main", "A", "B"));
    let before = replay.last_event_hash();
    assert!(
        replay.apply(&source_edit).is_err(),
        "an active dependency command must not explain a source edit"
    );
    assert_eq!(replay.last_event_hash(), before);

    apply(
        &mut replay,
        3,
        dependency_edit("manifest", manifest_before, &manifest_after),
    );
    apply(
        &mut replay,
        4,
        dependency_edit("lock", "lock-before\n", "lock-after\n"),
    );
    assert_eq!(
        replay
            .workspace_state()
            .file(&WorkspacePath::new("Cargo.toml").unwrap()),
        Some(manifest_after.as_bytes())
    );
    assert_eq!(
        replay
            .workspace_state()
            .file(&WorkspacePath::new("Cargo.lock").unwrap()),
        Some(b"lock-after\n".as_slice())
    );
    assert_eq!(
        replay
            .workspace_state()
            .file(&WorkspacePath::new("main.rs").unwrap()),
        Some(b"A".as_slice())
    );
    assert_eq!(replay.workspace_version(), 4);
}

#[test]
fn invalid_dependency_argv_cannot_open_a_replay_provenance_window() {
    let mut replay = dependency_initial();
    let start = dependency_start(&replay);
    let mut event = decode(&replay, 2, start);
    assert_eq!(
        unchecked_event_hash(replay.last_event_hash(), &event),
        event.event_hash,
        "the unchecked fixture hash must match the canonical valid hash"
    );
    let Event::ControlledCommandStarted(start) = &mut event.event else {
        panic!("fixture must be a controlled command start")
    };
    start.argv[5] = "--git".to_owned();
    event.event_hash = unchecked_event_hash(event.previous_event_hash, &event);

    let before = replay.last_event_hash();
    assert!(
        replay.apply(&event).is_err(),
        "out-of-policy Add evidence must not activate dependency provenance"
    );
    assert_eq!(replay.last_event_hash(), before);
    assert!(!replay.controlled_command_pending());
}

#[test]
fn active_format_cannot_bypass_existing_document_identity_or_version_validation() {
    for field in ["document_id", "version_before"] {
        let mut replay = initial();
        let event = format_start(&replay);
        apply(&mut replay, 2, event);
        let mut value = formatter_edit();
        match field {
            "document_id" => value["payload"][field] = json!("other"),
            "version_before" => {
                value["payload"][field] = json!(1);
                value["payload"]["version_after"] = json!(2);
            }
            _ => unreachable!(),
        }
        let edit = decode(&replay, 3, value);
        let before = replay.last_event_hash();
        assert!(replay.apply(&edit).is_err(), "accepted stale {field}");
        assert_eq!(replay.last_event_hash(), before);
        apply(&mut replay, 3, formatter_edit());
    }
}

#[test]
fn active_format_edit_replays_exactly_across_a_certified_checkpoint() {
    let mut replay = initial();
    let event = format_start(&replay);
    apply(&mut replay, 2, event);
    apply(&mut replay, 3, formatter_edit());
    assert_eq!(
        replay
            .workspace_state()
            .file(&WorkspacePath::new("main.rs").unwrap()),
        Some(b"B".as_slice())
    );
    assert_eq!(replay.workspace_version(), 3);

    let checkpoint = formatter_post_checkpoint(&mut replay);
    assert_eq!(replay.command_tree_link().unwrap().workspace_version, 3);
    let certified = replay.certify_checkpoint(checkpoint).unwrap();
    let mut seek = ReplayEngine::from_checkpoint(certified);

    for candidate in [&mut replay, &mut seek] {
        let event = format_finish(candidate);
        apply(candidate, 5, event);
        assert!(!candidate.controlled_command_pending());
        assert_eq!(candidate.workspace_version(), 3);
        assert_eq!(
            candidate
                .workspace_state()
                .file(&WorkspacePath::new("main.rs").unwrap()),
            Some(b"B".as_slice())
        );
    }
    assert_eq!(replay.last_event_hash(), seek.last_event_hash());
}

#[test]
fn resealed_inconsistent_pair_tree_time_and_capture_are_rejected_atomically() {
    for case in 0..8 {
        let mut replay = through_output();
        post_checkpoint(&mut replay);
        let mut value = finish(&replay);
        match case {
            0 => value["payload"]["command_id"] = json!("different"),
            1 => value["payload"]["after"]["checkpoint_sequence"] = json!(3),
            2 => value["payload"]["after"]["checkpoint_event_hash"] = json!(Hash::zero()),
            3 => value["payload"]["after"]["workspace_hash"] = json!(Hash::zero()),
            4 => value["payload"]["after"]["workspace_version"] = json!(2),
            5 => value["payload"]["started_millis"] = json!(21),
            6 => value["payload"]["finished_millis"] = json!(51),
            _ => value["payload"]["stdout"]["bytes"] = json!(0),
        }
        let old_hash = replay.last_event_hash();
        let event = decode(&replay, 5, value);
        assert!(replay.apply(&event).is_err(), "tamper {case} accepted");
        assert_eq!(replay.last_event_hash(), old_hash);
        let valid = finish(&replay);
        apply(&mut replay, 5, valid);
    }
}

fn deadline_finish(
    replay: &ReplayEngine,
    finished_millis: u64,
    envelope_millis: u64,
) -> EventEnvelope {
    let mut value = finish(replay);
    value["payload"]["finished_millis"] = json!(finished_millis);
    value["payload"]["outcome"] = json!({"kind":"terminated","reason":"deadline","signal":9});
    let mut event = decode(replay, 5, value);
    event.monotonic_millis = envelope_millis;
    event.seal(replay.last_event_hash()).unwrap()
}

fn assert_finish_rejected_atomically(replay: &mut ReplayEngine, event: EventEnvelope) {
    let before_hash = replay.last_event_hash();
    let before_output = replay.command_output_bytes();
    let before_workspace_hash = replay.current_workspace_hash();
    let before_version = replay.workspace_version();
    assert!(replay.apply(&event).is_err());
    assert_eq!(replay.last_event_hash(), before_hash);
    assert_eq!(replay.command_output_bytes(), before_output);
    assert_eq!(replay.current_workspace_hash(), before_workspace_hash);
    assert_eq!(replay.workspace_version(), before_version);
    assert!(replay.controlled_command_pending());
}

#[test]
fn delayed_recording_accepts_truthful_finish_beyond_runtime_window_but_enforces_chronology() {
    let mut replay = initial();
    let mut start_value = start(&replay);
    start_value["payload"]["deadline_millis"] = json!(1);
    apply(&mut replay, 2, start_value);
    apply(
        &mut replay,
        3,
        json!({"type": "controlled_command_output", "payload": {
        "command_id": "command-2", "stream": "stdout", "offset": 0, "bytes_hex": "ff001b"}}),
    );
    post_checkpoint(&mut replay);

    let started_millis = 20;
    let former_ceiling = started_millis + 1 + MAX_COMMAND_CLEANUP_MILLIS;
    let truthful_finish = former_ceiling + 1;
    let truthful_envelope = truthful_finish + 1;

    let deadline_too_early = deadline_finish(&replay, started_millis, 50);
    assert_finish_rejected_atomically(&mut replay, deadline_too_early);
    let finish_after_envelope = deadline_finish(&replay, truthful_finish, former_ceiling);
    assert_finish_rejected_atomically(&mut replay, finish_after_envelope);
    let backward_envelope = deadline_finish(&replay, 39, 39);
    assert_finish_rejected_atomically(&mut replay, backward_envelope);

    let truthful = deadline_finish(&replay, truthful_finish, truthful_envelope);
    replay.apply(&truthful).unwrap();
    assert!(!replay.controlled_command_pending());
}

#[test]
fn output_offset_and_simultaneous_command_cannot_bypass_lifecycle() {
    let mut replay = through_output();
    let duplicate = decode(&replay, 4, start(&initial()));
    assert!(replay.apply(&duplicate).is_err());
    let wrong_offset = decode(
        &replay,
        4,
        json!({"type": "controlled_command_output", "payload": {
        "command_id": "command-2", "stream": "stdout", "offset": 0, "bytes_hex": "01"}}),
    );
    assert!(replay.apply(&wrong_offset).is_err());
    let edit = decode(
        &replay,
        4,
        json!({"type": "file_created", "payload": {
        "document_id": "new", "path": "new.rs", "contents": "", "content_hash": document_hash("")}}),
    );
    assert!(
        replay.apply(&edit).is_err(),
        "source mutation during command accepted"
    );
}

#[test]
fn internal_paste_advances_command_tree_version_across_certified_seek() {
    let session_id = SessionId::new("paste-command-link").unwrap();
    let source_id = DocumentId::new("source").unwrap();
    let destination_id = DocumentId::new("destination").unwrap();
    let source_path = WorkspacePath::new("source.rs").unwrap();
    let destination_path = WorkspacePath::new("destination.rs").unwrap();
    let copied = "copy";
    let snapshot = CheckpointSnapshot::new(
        session_id.clone(),
        1,
        vec![
            CheckpointFile {
                path: source_path.clone(),
                contents: copied.as_bytes().to_vec(),
            },
            CheckpointFile {
                path: destination_path.clone(),
                contents: Vec::new(),
            },
        ],
        Some(source_id.clone()),
        vec![
            OpenDocument {
                document_id: source_id.clone(),
                path: source_path.clone(),
                version: 0,
                selection: SelectionState::new(0, copied.len() as u64),
            },
            OpenDocument {
                document_id: destination_id.clone(),
                path: destination_path.clone(),
                version: 0,
                selection: SelectionState::caret(0),
            },
        ],
    )
    .unwrap();
    let owning_event = EventEnvelope {
        format_version: 1,
        session_id,
        sequence: 1,
        monotonic_millis: 10,
        wall_clock_utc: None,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event: Event::WorkspaceCheckpoint(snapshot.event_payload()),
    }
    .seal(Hash::zero())
    .unwrap();
    let mut replay = ReplayEngine::from_initial_checkpoint(StoredCheckpoint {
        owning_event,
        snapshot,
    })
    .unwrap();

    let copied_event = Event::ClipboardCopied(ClipboardSource {
        prefix: RecordedEventRef {
            session_id: replay.session_id().clone(),
            sequence: 1,
            event_hash: replay.last_event_hash(),
        },
        document_id: source_id,
        path: source_path,
        version: 0,
        content_hash: document_hash(copied),
        start_byte: 0,
        end_byte: copied.len() as u64,
    });
    let copied_envelope = decode(&replay, 2, serde_json::to_value(copied_event).unwrap());
    replay.apply(&copied_envelope).unwrap();
    let paste_event = Event::InternalPaste(InternalPaste {
        source: RecordedEventRef {
            session_id: copied_envelope.session_id,
            sequence: copied_envelope.sequence,
            event_hash: copied_envelope.event_hash,
        },
        transaction: EditorTransaction {
            document_id: destination_id,
            version_before: 0,
            version_after: 1,
            origin: EditOrigin::Paste,
            edits: vec![TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: copied.to_owned(),
            }],
            selection_before: SelectionState::caret(0),
            selection_after: SelectionState::caret(copied.len() as u64),
            hash_before: document_hash(""),
            hash_after: document_hash(copied),
        },
    });
    let paste_envelope = decode(&replay, 3, serde_json::to_value(paste_event).unwrap());
    replay.apply(&paste_envelope).unwrap();

    let state = replay.workspace_state();
    let checkpoint_snapshot = CheckpointSnapshot::new(
        replay.session_id().clone(),
        4,
        state
            .files()
            .iter()
            .map(|(path, contents)| CheckpointFile {
                path: path.clone(),
                contents: contents.clone(),
            })
            .collect(),
        state.active_document().cloned(),
        state
            .documents()
            .values()
            .map(|document| OpenDocument {
                document_id: document.document_id().clone(),
                path: document.path().clone(),
                version: document.version(),
                selection: document.selection(),
            })
            .collect(),
    )
    .unwrap();
    let checkpoint_event = decode(
        &replay,
        4,
        serde_json::to_value(Event::WorkspaceCheckpoint(
            checkpoint_snapshot.event_payload(),
        ))
        .unwrap(),
    );
    replay.apply(&checkpoint_event).unwrap();
    let stored = StoredCheckpoint {
        owning_event: checkpoint_event,
        snapshot: checkpoint_snapshot,
    };
    let certified = replay.certify_checkpoint(stored).unwrap();
    let mut seek = ReplayEngine::from_checkpoint(certified);

    assert_eq!(replay.workspace_version(), 3);
    assert_eq!(replay.command_tree_link().unwrap().workspace_version, 3);
    assert_eq!(seek.workspace_version(), 3);
    assert_eq!(seek.command_tree_link().unwrap().workspace_version, 3);

    for candidate in [&mut replay, &mut seek] {
        let mut stale = start(candidate);
        stale["payload"]["command_id"] = json!("command-5");
        stale["payload"]["before"] =
            serde_json::to_value(candidate.command_tree_link().unwrap()).unwrap();
        stale["payload"]["before"]["workspace_version"] = json!(1);
        let stale = decode(candidate, 5, stale);
        let before = candidate.last_event_hash();
        assert!(candidate.apply(&stale).is_err());
        assert_eq!(candidate.last_event_hash(), before);

        let mut valid = start(candidate);
        valid["payload"]["command_id"] = json!("command-5");
        valid["payload"]["before"] =
            serde_json::to_value(candidate.command_tree_link().unwrap()).unwrap();
        apply(candidate, 5, valid);
    }
}

#[test]
fn output_limit_termination_requires_the_exact_started_capture_cap() {
    let mut replay = through_output();
    post_checkpoint(&mut replay);
    let mut value = finish(&replay);
    value["payload"]["outcome"] =
        json!({"kind":"terminated","reason":"output_limit","signal":null});
    value["payload"]["stdout"]["completeness"] = json!("truncated");
    let event = decode(&replay, 5, value);
    let before = replay.last_event_hash();
    assert!(
        replay.apply(&event).is_err(),
        "output-limit termination accepted below its started cap"
    );
    assert_eq!(replay.last_event_hash(), before);
}
