//! Behavioral codec Red: these fixtures use the existing public decoder, so
//! failure is missing runtime wire behavior, not a missing Rust API.
use rustrace_model::{DecodeOutcome, DecodePolicy, decode_envelope, encode_envelope};
use serde_json::json;

fn envelope(event: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "format_version": 1, "session_id": "runner", "sequence": 3,
        "monotonic_millis": 12, "wall_clock_utc": null,
        "previous_event_hash": "00".repeat(32), "event_hash": "00".repeat(32),
        "event": event
    }))
    .unwrap()
}

fn cargo_start(action: &str, subcommand: &str, tail: &[&str], console: bool) -> Vec<u8> {
    let mut argv = vec![
        json!("/trusted/rustup"),
        json!("run"),
        json!("fixture"),
        json!("/trusted/cargo"),
        json!(subcommand),
    ];
    argv.extend(tail.iter().map(|argument| json!(argument)));
    let mut payload = json!({
        "command_id":"command-2", "action":action, "argv":argv,
        "environment":{"policy_version":1,"retained_names":["HOME","PATH"]},
        "selected_toolchain":"fixture",
        "tools":[
            {"component":"rustup","executable":"/trusted/rustup","version":"rustup fixture"},
            {"component":"rustc","executable":"/trusted/rustc","version":"rustc fixture"},
            {"component":"cargo","executable":"/trusted/cargo","version":"cargo fixture"},
            {"component":"rustdoc","executable":"/trusted/rustdoc","version":"rustdoc fixture"}
        ],
        "before":{"checkpoint_sequence":2,"checkpoint_event_hash":"00".repeat(32),
            "workspace_hash":"00".repeat(32),"workspace_version":1},
        "deadline_millis":300000,"output_limit":8388608
    });
    if console {
        let stdin = if action == "run" {
            json!({"kind":"submitted"})
        } else {
            json!({"kind":"closed"})
        };
        payload["console"] = json!({"stdin":stdin,"stdout":{"kind":"console"}});
    }
    envelope(json!({"type":"controlled_command_started","payload":payload}))
}

#[test]
fn controlled_output_preserves_invalid_utf8_and_terminal_bytes_exactly() {
    // Canonical lower-case hex keeps original arbitrary bytes within existing
    // bounded JSON string limits, without lossy UTF-8 conversion.
    let bytes = envelope(json!({"type": "controlled_command_output", "payload": {
        "command_id": "command-2", "stream": "stdout", "offset": 0,
        "bytes_hex": "ff001b5d35323b630a"
    }}));
    let decoded = decode_envelope(&bytes, DecodePolicy::RejectUnsupported);
    assert!(
        decoded.is_ok(),
        "byte-exact command evidence must decode: {decoded:?}"
    );
    let DecodeOutcome::Decoded(decoded) = decoded.unwrap() else {
        panic!("not decoded")
    };
    let encoded = encode_envelope(&decoded).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(value["event"]["payload"]["bytes_hex"], "ff001b5d35323b630a");
    assert!(!encoded.contains(&0x1b));
    assert_eq!(
        decode_envelope(&encoded, DecodePolicy::RejectUnsupported).unwrap(),
        DecodeOutcome::Decoded(decoded)
    );
}

#[test]
fn historical_output_and_finish_remain_readable_without_completeness_claims() {
    for event in [
        json!({"type": "cargo_output", "payload": {
            "command_id": "old", "stream": "stderr", "output": ""}}),
        json!({"type": "cargo_command_finished", "payload": {
            "command_id": "old", "exit_code": 0, "success": true}}),
    ] {
        let DecodeOutcome::Decoded(decoded) =
            decode_envelope(&envelope(event.clone()), DecodePolicy::RejectUnsupported).unwrap()
        else {
            panic!("not decoded")
        };
        let encoded: serde_json::Value =
            serde_json::from_slice(&encode_envelope(&decoded).unwrap()).unwrap();
        assert_eq!(encoded["event"], event);
        assert!(encoded["event"]["payload"].get("completeness").is_none());
    }
}

#[test]
fn controlled_output_rejects_noncanonical_or_overbound_bytes() {
    for hex in [
        "FF".to_owned(),
        "f".to_owned(),
        "gg".to_owned(),
        "00".repeat(32_769),
    ] {
        let bytes = envelope(json!({"type": "controlled_command_output", "payload": {
            "command_id": "command-2", "stream": "stdout", "offset": 0,
            "bytes_hex": hex
        }}));
        assert!(decode_envelope(&bytes, DecodePolicy::RejectUnsupported).is_err());
    }
}

#[test]
fn cancellation_before_launch_records_unavailable_streams_without_inventing_empty_capture() {
    for reason in ["cancelled", "quit"] {
        let bytes = envelope(json!({"type":"controlled_command_finished","payload":{
            "command_id":"command-2", "after":{"checkpoint_sequence":2,"checkpoint_event_hash":"00".repeat(32),"workspace_hash":"00".repeat(32),"workspace_version":1},
            "started_millis":1,"finished_millis":2,
            "outcome":{"kind":"terminated","reason":reason,"signal":null},
            "stdout":{"bytes":0,"completeness":"unavailable"},
            "stderr":{"bytes":0,"completeness":"unavailable"}
        }}));
        assert!(
            decode_envelope(&bytes, DecodePolicy::RejectUnsupported).is_ok(),
            "unlaunched {reason} needs truthful unavailable capture"
        );
    }
}

#[test]
fn termination_reason_requires_matching_incomplete_capture_evidence() {
    for (reason, stdout_completeness) in [
        ("output_limit", "complete"),
        ("capture_failure", "complete"),
    ] {
        let bytes = envelope(json!({"type":"controlled_command_finished","payload":{
            "command_id":"command-2", "after":{"checkpoint_sequence":2,"checkpoint_event_hash":"00".repeat(32),"workspace_hash":"00".repeat(32),"workspace_version":1},
            "started_millis":1,"finished_millis":2,
            "outcome":{"kind":"terminated","reason":reason,"signal":null},
            "stdout":{"bytes":1,"completeness":stdout_completeness},
            "stderr":{"bytes":0,"completeness":"complete"}
        }}));
        assert!(
            decode_envelope(&bytes, DecodePolicy::RejectUnsupported).is_err(),
            "{reason} accepted without its required incomplete stream"
        );
    }
}

#[test]
fn console_route_and_redirected_capture_have_bounded_wire_behavior() {
    let started = envelope(json!({"type":"controlled_command_started","payload":{
        "command_id":"command-2", "action":"run",
        "argv":["/trusted/rustup","run","fixture","/trusted/cargo","run","--frozen"],
        "environment":{"policy_version":1,"retained_names":["HOME","PATH"]},
        "selected_toolchain":"fixture",
        "tools":[
            {"component":"rustup","executable":"/trusted/rustup","version":"rustup fixture"},
            {"component":"rustc","executable":"/trusted/rustc","version":"rustc fixture"},
            {"component":"cargo","executable":"/trusted/cargo","version":"cargo fixture"},
            {"component":"rustdoc","executable":"/trusted/rustdoc","version":"rustdoc fixture"}
        ],
        "before":{"checkpoint_sequence":2,"checkpoint_event_hash":"00".repeat(32),
            "workspace_hash":"00".repeat(32),"workspace_version":1},
        "deadline_millis":300000,"output_limit":8388608,
        "console":{"stdin":{"kind":"file","path":"inputs/a.txt"},
            "stdout":{"kind":"file","path":"outputs/a.txt"}}
    }}));
    assert!(decode_envelope(&started, DecodePolicy::RejectUnsupported).is_ok());

    let finished = envelope(json!({"type":"controlled_command_finished","payload":{
        "command_id":"command-2",
        "after":{"checkpoint_sequence":2,"checkpoint_event_hash":"00".repeat(32),
            "workspace_hash":"00".repeat(32),"workspace_version":1},
        "started_millis":1,"finished_millis":2,"outcome":{"kind":"exited","code":0},
        "stdout":{"bytes":0,"completeness":"unavailable","mode":"redirected"},
        "stderr":{"bytes":0,"completeness":"complete"}
    }}));
    assert!(decode_envelope(&finished, DecodePolicy::RejectUnsupported).is_ok());
}

#[test]
fn console_run_accepts_natural_stdout_argv_and_retains_historical_forms() {
    for tail in [
        &["--locked"][..],
        &["--release", "--locked"][..],
        &["--message-format=json", "--locked"][..],
        &["--frozen"][..],
    ] {
        let bytes = cargo_start("run", "run", tail, true);
        assert!(
            decode_envelope(&bytes, DecodePolicy::RejectUnsupported).is_ok(),
            "rejected console Run tail {tail:?}"
        );
    }
}

#[test]
fn every_console_action_accepts_natural_argv() {
    for (action, tail) in [
        ("build", vec!["--locked"]),
        ("check", vec!["--locked"]),
        ("test", vec!["--locked"]),
        ("run", vec!["--locked"]),
        ("clippy", vec!["--locked"]),
        ("doc", vec!["--locked"]),
        ("add", vec!["serde@1.0.0"]),
        ("remove", vec!["serde"]),
        ("update", vec![]),
    ] {
        let bytes = cargo_start(action, action, &tail, true);
        // Clippy uses its separately resolved driver.
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        if action == "clippy" {
            value["event"]["payload"]["tools"].as_array_mut().unwrap().push(json!({"component":"clippy","executable":"/trusted/cargo","version":"clippy fixture"}));
        }
        assert!(
            decode_envelope(
                &serde_json::to_vec(&value).unwrap(),
                DecodePolicy::RejectUnsupported
            )
            .is_ok(),
            "{action}"
        );
    }
}

#[test]
fn historical_controlled_events_reencode_without_new_default_fields() {
    let original = envelope(json!({"type":"controlled_command_finished","payload":{
        "command_id":"command-2",
        "after":{"checkpoint_sequence":2,"checkpoint_event_hash":"00".repeat(32),
            "workspace_hash":"00".repeat(32),"workspace_version":1},
        "started_millis":1,"finished_millis":2,"outcome":{"kind":"exited","code":0},
        "stdout":{"bytes":0,"completeness":"complete"},
        "stderr":{"bytes":0,"completeness":"complete"}
    }}));
    let DecodeOutcome::Decoded(decoded) =
        decode_envelope(&original, DecodePolicy::RejectUnsupported).unwrap()
    else {
        panic!("not decoded")
    };
    let value: serde_json::Value =
        serde_json::from_slice(&encode_envelope(&decoded).unwrap()).unwrap();
    assert!(value["event"]["payload"]["stdout"].get("mode").is_none());
    assert!(value["event"]["payload"].get("console").is_none());
}

#[test]
fn comparison_events_have_exact_golden_json_and_closed_error_reasons() {
    let expected = "22".repeat(32);
    let actual = "33".repeat(32);
    let events = [
        json!({"type":"test_case_compared","payload":{
            "command_id":"command-2","case":"sample_1",
            "expected_blake3":expected,"actual_blake3":expected,
            "outcome":{"kind":"pass"}
        }}),
        json!({"type":"test_case_compared","payload":{
            "command_id":"command-2","case":"sample_1",
            "expected_blake3":expected,"actual_blake3":actual,
            "outcome":{"kind":"mismatch","line":2,"expected_len":3,"actual_len":4}
        }}),
        json!({"type":"test_case_compared","payload":{
            "command_id":"command-2","case":"sample_1",
            "expected_blake3":expected,"actual_blake3":null,
            "outcome":{"kind":"error","reason":"launch_failed"}
        }}),
    ];

    for event in events {
        let bytes = envelope(event.clone());
        let DecodeOutcome::Decoded(decoded) =
            decode_envelope(&bytes, DecodePolicy::RejectUnsupported).unwrap()
        else {
            panic!("comparison event was skipped")
        };
        let encoded: serde_json::Value =
            serde_json::from_slice(&encode_envelope(&decoded).unwrap()).unwrap();
        assert_eq!(encoded["event"], event);
    }

    for reason in [
        "launch_failed",
        "nonzero_exit",
        "terminated",
        "capture_truncated",
        "capture_unavailable",
        "capture_read_failed",
        "expected_unreadable",
        "expected_oversized",
    ] {
        let event = json!({"type":"test_case_compared","payload":{
            "command_id":"command-2","case":"sample_1",
            "expected_blake3":expected,"actual_blake3":null,
            "outcome":{"kind":"error","reason":reason}
        }});
        let bytes = envelope(event.clone());
        let DecodeOutcome::Decoded(decoded) =
            decode_envelope(&bytes, DecodePolicy::RejectUnsupported).unwrap()
        else {
            panic!("comparison error was skipped")
        };
        let encoded: serde_json::Value =
            serde_json::from_slice(&encode_envelope(&decoded).unwrap()).unwrap();
        assert_eq!(encoded["event"], event);
    }

    let unknown = json!({"type":"test_case_compared","payload":{
        "command_id":"command-2","case":"sample_1",
        "expected_blake3":expected,"actual_blake3":null,
        "outcome":{"kind":"error","reason":"arbitrary_detail"}
    }});
    assert!(decode_envelope(&envelope(unknown), DecodePolicy::RejectUnsupported).is_err());
}

#[test]
fn comparison_event_validation_is_bounded_and_semantic() {
    let expected = "22".repeat(32);
    let actual = "33".repeat(32);
    let invalid = [
        json!({"type":"test_case_compared","payload":{
            "command_id":"command-2","case":"sample",
            "expected_blake3":expected,"actual_blake3":expected,
            "outcome":{"kind":"pass"},"surplus":true
        }}),
        json!({"type":"test_case_compared","payload":{
            "command_id":"command-2","case":"sample",
            "expected_blake3":expected,"actual_blake3":actual,
            "outcome":{"kind":"mismatch","line":1,"expected_len":3,"actual_len":4,
                "surplus":true}
        }}),
        json!({"type":"test_case_compared","payload":{
            "command_id":"command-2","case":"bad.name",
            "expected_blake3":expected,"actual_blake3":actual,
            "outcome":{"kind":"pass"}
        }}),
        json!({"type":"test_case_compared","payload":{
            "command_id":"command-2","case":"sample",
            "expected_blake3":expected,"actual_blake3":expected,
            "outcome":{"kind":"mismatch","line":1,"expected_len":3,"actual_len":4}
        }}),
        json!({"type":"test_case_compared","payload":{
            "command_id":"command-2","case":"sample",
            "expected_blake3":expected,"actual_blake3":actual,
            "outcome":{"kind":"mismatch","line":0,"expected_len":3,"actual_len":4}
        }}),
        json!({"type":"test_case_compared","payload":{
            "command_id":"command-2","case":"sample",
            "expected_blake3":expected,"actual_blake3":actual,
            "outcome":{"kind":"mismatch","line":1,"expected_len":1048577,"actual_len":4}
        }}),
        json!({"type":"test_case_compared","payload":{
            "command_id":"command-2","case":"sample",
            "expected_blake3":expected,"actual_blake3":null,
            "outcome":{"kind":"pass"}
        }}),
    ];
    for event in invalid {
        assert!(
            decode_envelope(&envelope(event.clone()), DecodePolicy::RejectUnsupported).is_err(),
            "accepted invalid comparison {event}"
        );
    }

    let maximum_actual_line = json!({"type":"test_case_compared","payload":{
        "command_id":"command-2","case":"sample",
        "expected_blake3":expected,"actual_blake3":actual,
        "outcome":{"kind":"mismatch","line":1,"expected_len":1048576,"actual_len":8388608}
    }});
    assert!(
        decode_envelope(
            &envelope(maximum_actual_line),
            DecodePolicy::RejectUnsupported
        )
        .is_ok(),
        "captured stdout permits a larger line than an expected case file"
    );
    let oversized_actual_line = json!({"type":"test_case_compared","payload":{
        "command_id":"command-2","case":"sample",
        "expected_blake3":expected,"actual_blake3":actual,
        "outcome":{"kind":"mismatch","line":1,"expected_len":0,"actual_len":8388609}
    }});
    assert!(
        decode_envelope(
            &envelope(oversized_actual_line),
            DecodePolicy::RejectUnsupported
        )
        .is_err()
    );
}

#[test]
fn comparison_mismatch_expected_size_includes_preceding_newlines() {
    use rustrace_model::{MAX_TEST_CASE_COMPARISON_LINE, MAX_TEST_CASE_EXPECTED_LINE_BYTES};

    for (line, expected_len, accepted) in [
        (2, MAX_TEST_CASE_EXPECTED_LINE_BYTES - 1, true),
        (MAX_TEST_CASE_COMPARISON_LINE, 0, true),
        (2, MAX_TEST_CASE_EXPECTED_LINE_BYTES, false),
    ] {
        let event = json!({"type":"test_case_compared","payload":{
            "command_id":"command-2","case":"sample",
            "expected_blake3":"22".repeat(32),"actual_blake3":"33".repeat(32),
            "outcome":{"kind":"mismatch","line":line,"expected_len":expected_len,"actual_len":1}
        }});
        let result = decode_envelope(&envelope(event), DecodePolicy::RejectUnsupported);
        assert_eq!(
            result.is_ok(),
            accepted,
            "line {line} / expected_len {expected_len} must include preceding LF bytes: {result:?}"
        );
    }
}

#[test]
fn dependency_tool_edit_origin_is_an_additive_v1_value() {
    let original = envelope(json!({"type":"file_edited","payload":{
        "document_id":"document-1","version_before":1,"version_after":2,
        "origin":"dependency_tool",
        "edits":[{"start_byte":0,"end_byte":0,"inserted_text":"itoa = \"1\"\n"}],
        "selection_before":{"anchor_byte":0,"active_byte":0},
        "selection_after":{"anchor_byte":0,"active_byte":0},
        "hash_before":"00".repeat(32),"hash_after":"11".repeat(32)
    }}));
    let DecodeOutcome::Decoded(decoded) =
        decode_envelope(&original, DecodePolicy::RejectUnsupported).unwrap()
    else {
        panic!("not decoded")
    };
    let encoded: serde_json::Value =
        serde_json::from_slice(&encode_envelope(&decoded).unwrap()).unwrap();
    assert_eq!(encoded["event"]["payload"]["origin"], "dependency_tool");
}

#[test]
fn format_is_menu_only_in_v1_command_evidence() {
    let payload = json!({
        "command_id":"command-2", "action":"format",
        "argv":["/trusted/rustup","run","fixture","/trusted/cargo-fmt","fmt"],
        "environment":{"policy_version":1,"retained_names":["HOME","PATH"]},
        "selected_toolchain":"fixture",
        "tools":[
            {"component":"rustup","executable":"/trusted/rustup","version":"rustup fixture"},
            {"component":"rustc","executable":"/trusted/rustc","version":"rustc fixture"},
            {"component":"cargo","executable":"/trusted/cargo","version":"cargo fixture"},
            {"component":"rustdoc","executable":"/trusted/rustdoc","version":"rustdoc fixture"},
            {"component":"cargo_fmt","executable":"/trusted/cargo-fmt","version":"rustfmt fixture"},
            {"component":"rustfmt","executable":"/trusted/rustfmt","version":"rustfmt fixture"}
        ],
        "before":{"checkpoint_sequence":2,"checkpoint_event_hash":"00".repeat(32),
            "workspace_hash":"00".repeat(32),"workspace_version":1},
        "deadline_millis":300000,"output_limit":8388608
    });

    let menu = envelope(json!({"type":"controlled_command_started","payload":payload}));
    assert!(decode_envelope(&menu, DecodePolicy::RejectUnsupported).is_ok());

    let mut console_payload = payload;
    console_payload["console"] = json!({"stdin":{"kind":"closed"},"stdout":{"kind":"console"}});
    let console = envelope(json!({"type":"controlled_command_started","payload":console_payload}));
    assert!(
        decode_envelope(&console, DecodePolicy::RejectUnsupported).is_err(),
        "console-routed Format must remain outside the v1 command surface"
    );
}

#[test]
fn doc_and_dependency_command_actions_have_bounded_v1_evidence() {
    for (action, subcommand, tail, console) in [
        (
            "doc",
            "doc",
            vec!["--message-format=json", "--locked"],
            None,
        ),
        (
            "add",
            "add",
            vec!["serde@1.0.229"],
            Some(json!({"stdin":{"kind":"closed"},"stdout":{"kind":"console"}})),
        ),
        (
            "remove",
            "remove",
            vec!["serde"],
            Some(json!({"stdin":{"kind":"closed"},"stdout":{"kind":"console"}})),
        ),
        (
            "update",
            "update",
            vec![],
            Some(json!({"stdin":{"kind":"closed"},"stdout":{"kind":"console"}})),
        ),
    ] {
        let mut argv = vec![
            json!("/trusted/rustup"),
            json!("run"),
            json!("fixture"),
            json!("/trusted/cargo"),
            json!(subcommand),
        ];
        argv.extend(tail.into_iter().map(serde_json::Value::from));
        let mut payload = json!({
            "command_id":"command-2", "action":action, "argv":argv,
            "environment":{"policy_version":1,"retained_names":["HOME","PATH"]},
            "selected_toolchain":"fixture",
            "tools":[
                {"component":"rustup","executable":"/trusted/rustup","version":"rustup fixture"},
                {"component":"rustc","executable":"/trusted/rustc","version":"rustc fixture"},
                {"component":"cargo","executable":"/trusted/cargo","version":"cargo fixture"},
                {"component":"rustdoc","executable":"/trusted/rustdoc","version":"rustdoc fixture"}
            ],
            "before":{"checkpoint_sequence":2,"checkpoint_event_hash":"00".repeat(32),
                "workspace_hash":"00".repeat(32),"workspace_version":1},
            "deadline_millis":300000,"output_limit":8388608
        });
        if let Some(console) = console {
            payload["console"] = console;
        }
        let started = envelope(json!({"type":"controlled_command_started","payload":payload}));
        assert!(
            decode_envelope(&started, DecodePolicy::RejectUnsupported).is_ok(),
            "rejected {action} evidence"
        );
    }
}

#[test]
fn doc_and_dependency_v1_evidence_rejects_every_live_grammar_escape() {
    for (action, subcommand, tail) in [
        ("doc", "doc", vec![]),
        ("doc", "doc", vec!["--frozen"]),
        ("doc", "doc", vec!["--open"]),
        ("add", "add", vec![]),
        ("add", "add", vec!["--git"]),
        ("add", "add", vec!["../serde"]),
        ("add", "add", vec!["ser/de"]),
        ("add", "add", vec!["-serde"]),
        ("add", "add", vec!["1serde"]),
        ("add", "add", vec!["serde🙂"]),
        ("add", "add", vec!["serde@1"]),
        ("add", "add", vec!["serde@1.2"]),
        ("add", "add", vec!["serde@01.2.3"]),
        ("add", "add", vec!["serde@1.2.3-01"]),
        ("add", "add", vec!["serde@1.2.3+"]),
        ("add", "add", vec!["serde@1.2.3+build+other"]),
        ("add", "add", vec!["serde@>=1.0.0"]),
        ("add", "add", vec!["serde@1.0.0,2.0.0"]),
        ("add", "add", vec!["serde", "--git"]),
        ("add", "add", vec!["serde", "--features", "derive"]),
        ("add", "add", vec!["serde", "--path", "../serde"]),
        ("add", "add", vec!["serde", "--registry", "private"]),
        ("remove", "remove", vec![]),
        ("remove", "remove", vec!["--dev"]),
        ("remove", "remove", vec!["serde@1.0.0"]),
        ("remove", "remove", vec!["serde", "--dev"]),
        ("update", "update", vec!["serde"]),
        ("update", "update", vec!["--precise", "1.0.0"]),
    ] {
        for console in [false, true] {
            let bytes = cargo_start(action, subcommand, &tail, console);
            assert!(
                decode_envelope(&bytes, DecodePolicy::RejectUnsupported).is_err(),
                "accepted action={action} tail={tail:?} console={console}"
            );
        }
    }

    for argument in ["a".repeat(65), format!("serde@1.2.3+{}", "a".repeat(129))] {
        for console in [false, true] {
            let bytes = cargo_start("add", "add", &[&argument], console);
            assert!(
                decode_envelope(&bytes, DecodePolicy::RejectUnsupported).is_err(),
                "accepted overbound argument console={console}: {argument:?}"
            );
        }
    }
}

#[test]
fn dependency_v1_evidence_accepts_live_name_and_full_semver_boundaries() {
    let longest_name = "a".repeat(64);
    for (action, subcommand, argument) in [
        ("add", "add", "serde"),
        ("add", "add", "serde@1.2.3-alpha.1+build.5"),
        ("add", "add", longest_name.as_str()),
        ("remove", "remove", "serde_json"),
    ] {
        for console in [false, true] {
            let bytes = cargo_start(action, subcommand, &[argument], console);
            assert!(
                decode_envelope(&bytes, DecodePolicy::RejectUnsupported).is_ok(),
                "rejected action={action} argument={argument:?} console={console}"
            );
        }
    }
    for console in [false, true] {
        for (action, subcommand, tail) in [
            ("doc", "doc", &["--message-format=json", "--locked"][..]),
            ("update", "update", &[][..]),
        ] {
            let bytes = cargo_start(action, subcommand, tail, console);
            assert!(
                decode_envelope(&bytes, DecodePolicy::RejectUnsupported).is_ok(),
                "rejected action={action} tail={tail:?} console={console}"
            );
        }
    }
}
