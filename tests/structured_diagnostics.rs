use rustrace::diagnostics::{
    DiagnosticEvidenceIssue, DiagnosticOutcome, derive_command_diagnostics,
};
use rustrace_model::{
    CaptureCompleteness, CommandCapture, CommandCaptureMode, CommandEnvironment, CommandId,
    CommandOutcome, CommandTool, CommandToolKind, CommandTreeLink, ControlledAction,
    ControlledCommandFinished, ControlledCommandOutput, ControlledCommandStarted, Hash,
    MAX_COMMAND_CHUNK_BYTES, OutputStream,
};

fn hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; 32])
}

fn started(action: ControlledAction) -> ControlledCommandStarted {
    let subcommand = match action {
        ControlledAction::Build => "build",
        ControlledAction::Check => "check",
        ControlledAction::Test => "test",
        ControlledAction::Run => "run",
        ControlledAction::Clippy => "clippy",
        ControlledAction::Format => "fmt",
        ControlledAction::Doc => "doc",
        ControlledAction::Add => "add",
        ControlledAction::Remove => "remove",
        ControlledAction::Update => "update",
    };
    ControlledCommandStarted {
        command_id: CommandId::new("command-42").unwrap(),
        action,
        argv: vec![
            "/rustup".into(),
            "run".into(),
            "1.98.1".into(),
            "/cargo".into(),
            subcommand.into(),
            "--message-format=json".into(),
            "--locked".into(),
        ],
        environment: CommandEnvironment {
            policy_version: 1,
            retained_names: vec![],
        },
        selected_toolchain: "1.98.1".into(),
        tools: vec![
            CommandTool {
                component: CommandToolKind::Rustup,
                executable: "/rustup".into(),
                version: "rustup 1.28.2".into(),
            },
            CommandTool {
                component: CommandToolKind::Rustc,
                executable: "/rustc".into(),
                version: "rustc 1.98.1".into(),
            },
            CommandTool {
                component: CommandToolKind::Cargo,
                executable: "/cargo".into(),
                version: "cargo 1.98.1".into(),
            },
            CommandTool {
                component: CommandToolKind::Rustdoc,
                executable: "/rustdoc".into(),
                version: "rustdoc 1.98.1".into(),
            },
        ],
        before: CommandTreeLink {
            checkpoint_sequence: 40,
            checkpoint_event_hash: hash(1),
            workspace_hash: hash(2),
            workspace_version: 23,
        },
        deadline_millis: 10_000,
        output_limit: 8 * 1024 * 1024,
        console: None,
    }
}

fn finished(
    start: &ControlledCommandStarted,
    outcome: CommandOutcome,
    stdout: CommandCapture,
    stderr: CommandCapture,
) -> ControlledCommandFinished {
    ControlledCommandFinished {
        command_id: start.command_id.clone(),
        after: CommandTreeLink {
            checkpoint_sequence: 45,
            checkpoint_event_hash: hash(3),
            workspace_hash: start.before.workspace_hash,
            workspace_version: start.before.workspace_version,
        },
        started_millis: 100,
        finished_millis: 200,
        outcome,
        stdout,
        stderr,
    }
}

fn complete(bytes: usize) -> CommandCapture {
    CommandCapture {
        bytes: bytes as u64,
        completeness: CaptureCompleteness::Complete,
        mode: CommandCaptureMode::Captured,
    }
}

fn chunks(
    start: &ControlledCommandStarted,
    stream: OutputStream,
    bytes: &[u8],
    sizes: &[usize],
) -> Vec<ControlledCommandOutput> {
    let mut result = Vec::new();
    let mut offset = 0;
    for size in sizes.iter().copied() {
        if offset == bytes.len() {
            break;
        }
        let end = (offset + size).min(bytes.len());
        result.push(
            ControlledCommandOutput::from_bytes(
                start.command_id.clone(),
                stream,
                offset as u64,
                &bytes[offset..end],
            )
            .unwrap(),
        );
        offset = end;
    }
    while offset < bytes.len() {
        let end = (offset + MAX_COMMAND_CHUNK_BYTES).min(bytes.len());
        result.push(
            ControlledCommandOutput::from_bytes(
                start.command_id.clone(),
                stream,
                offset as u64,
                &bytes[offset..end],
            )
            .unwrap(),
        );
        offset = end;
    }
    result
}

fn derive(
    action: ControlledAction,
    stdout: &[u8],
    stderr: &[u8],
    outcome: CommandOutcome,
) -> rustrace::diagnostics::CommandDiagnostics {
    let start = started(action);
    let mut output = chunks(&start, OutputStream::Stdout, stdout, &[7, 13, 29]);
    output.extend(chunks(&start, OutputStream::Stderr, stderr, &[5, 11]));
    let finish = finished(
        &start,
        outcome,
        complete(stdout.len()),
        complete(stderr.len()),
    );
    derive_command_diagnostics(&start, &output, &finish)
}

fn compiler_message(level: &str, message: &str) -> String {
    serde_json::json!({
        "reason": "compiler-message",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": {
            "kind": ["bin"], "crate_types": ["bin"], "name": "student",
            "src_path": "/work/src/main.rs", "edition": "2024"
        },
        "message": {
            "rendered": format!("{level}: {message}\n"), "message": message,
            "code": null, "level": level, "spans": [], "children": []
        }
    })
    .to_string()
}

fn compiler_message_value(message: &str) -> serde_json::Value {
    serde_json::from_str(&compiler_message("warning", message)).unwrap()
}

fn diagnostic_child(message: &str, children: Option<serde_json::Value>) -> serde_json::Value {
    let mut child = serde_json::json!({
        "message": message,
        "code": null,
        "level": "note",
        "spans": [],
        "rendered": null
    });
    if let Some(children) = children {
        child["children"] = children;
    }
    child
}

fn four_level_children(deepest_children: Option<serde_json::Value>) -> serde_json::Value {
    let level_four = diagnostic_child("level four", deepest_children);
    let level_three = diagnostic_child("level three", Some(serde_json::json!([level_four])));
    let level_two = diagnostic_child("level two", Some(serde_json::json!([level_three])));
    let level_one = diagnostic_child("level one", Some(serde_json::json!([level_two])));
    serde_json::json!([level_one])
}

fn diagnostic_span(is_primary: bool) -> serde_json::Value {
    serde_json::json!({
        "file_name": "/work/src/main.rs",
        "byte_start": 0,
        "byte_end": 1,
        "line_start": 1,
        "line_end": 1,
        "column_start": 1,
        "column_end": 2,
        "is_primary": is_primary,
        "text": [],
        "label": null,
        "suggested_replacement": null,
        "suggestion_applicability": null,
        "expansion": null
    })
}

fn compiler_message_with_span(
    message: &str,
    span: serde_json::Value,
    child: bool,
) -> serde_json::Value {
    let mut record = compiler_message_value(message);
    if child {
        let mut child = diagnostic_child("child span", Some(serde_json::json!([])));
        child["spans"] = serde_json::json!([span]);
        record["message"]["children"] = serde_json::json!([child]);
    } else {
        record["message"]["spans"] = serde_json::json!([span]);
    }
    record
}

#[test]
fn review4_required_nullable_span_keys_reject_missing_values_per_record() {
    let mut missing_label = diagnostic_span(true);
    missing_label.as_object_mut().unwrap().remove("label");
    let missing_label = compiler_message_with_span("missing primary label", missing_label, false);

    let mut missing_replacement = diagnostic_span(false);
    missing_replacement
        .as_object_mut()
        .unwrap()
        .remove("suggested_replacement");
    let missing_replacement = compiler_message_with_span(
        "missing secondary suggested replacement",
        missing_replacement,
        false,
    );

    let mut missing_applicability = diagnostic_span(false);
    missing_applicability
        .as_object_mut()
        .unwrap()
        .remove("suggestion_applicability");
    let missing_applicability = compiler_message_with_span(
        "missing child suggestion applicability",
        missing_applicability,
        true,
    );

    let malformed = [missing_label, missing_replacement, missing_applicability]
        .map(|record| record.to_string());
    let stdout = format!(
        "{}\n{{\"reason\":\"build-finished\",\"success\":true}}\n",
        malformed.join("\n")
    );
    let result = derive(
        ControlledAction::Check,
        stdout.as_bytes(),
        b"",
        CommandOutcome::Exited { code: 0 },
    );

    assert_eq!(result.issues, vec![DiagnosticEvidenceIssue::Malformed]);
    assert_eq!(result.outcome, DiagnosticOutcome::Unknown);
    assert!(!result.known_empty());
    assert_eq!(result.diagnostics.len(), malformed.len());
    assert_eq!(result.diagnostics[2].children.len(), 1);
    assert_eq!(result.output.len(), malformed.len());
    for (line, expected) in result.output.iter().zip(&malformed) {
        assert_eq!(line.stream, OutputStream::Stdout);
        assert_eq!(line.original_bytes().unwrap(), expected.as_bytes());
    }
}

#[test]
fn review4_required_nullable_span_keys_reject_wrong_types() {
    let mut wrong_label = diagnostic_span(true);
    wrong_label["label"] = serde_json::json!(false);
    let wrong_label = compiler_message_with_span("wrong primary label", wrong_label, false);

    let mut wrong_replacement = diagnostic_span(false);
    wrong_replacement["suggested_replacement"] = serde_json::json!([]);
    let wrong_replacement = compiler_message_with_span(
        "wrong secondary suggested replacement",
        wrong_replacement,
        false,
    );

    let mut wrong_applicability = diagnostic_span(false);
    wrong_applicability["suggestion_applicability"] = serde_json::json!(7);
    let wrong_applicability = compiler_message_with_span(
        "wrong child suggestion applicability",
        wrong_applicability,
        true,
    );

    let malformed =
        [wrong_label, wrong_replacement, wrong_applicability].map(|record| record.to_string());
    let stdout = format!(
        "{}\n{{\"reason\":\"build-finished\",\"success\":true}}\n",
        malformed.join("\n")
    );
    let result = derive(
        ControlledAction::Check,
        stdout.as_bytes(),
        b"",
        CommandOutcome::Exited { code: 0 },
    );

    assert_eq!(result.issues, vec![DiagnosticEvidenceIssue::Malformed]);
    assert_eq!(result.outcome, DiagnosticOutcome::Unknown);
    assert_eq!(result.diagnostics.len(), malformed.len());
    assert_eq!(result.diagnostics[2].children.len(), 1);
    assert_eq!(result.output.len(), malformed.len());
    for (line, expected) in result.output.iter().zip(&malformed) {
        assert_eq!(line.stream, OutputStream::Stdout);
        assert_eq!(line.original_bytes().unwrap(), expected.as_bytes());
    }
}

#[test]
fn review4_required_nullable_span_keys_accept_null_and_strings() {
    let null_span = diagnostic_span(true);

    let mut string_span = diagnostic_span(false);
    string_span["label"] = serde_json::json!("");
    string_span["suggested_replacement"] = serde_json::json!("");
    string_span["suggestion_applicability"] = serde_json::json!("FutureApplicability");

    let mut child_span = diagnostic_span(false);
    child_span["label"] = serde_json::json!("child label");
    child_span["suggested_replacement"] = serde_json::json!("replacement");
    child_span["suggestion_applicability"] = serde_json::json!("");

    let mut record = compiler_message_with_span("nullable span controls", null_span, false);
    record["message"]["spans"]
        .as_array_mut()
        .unwrap()
        .push(string_span);
    let child_record = compiler_message_with_span("child control", child_span, true);
    record["message"]["children"] = child_record["message"]["children"].clone();
    let stdout = format!("{record}\n{{\"reason\":\"build-finished\",\"success\":true}}\n");
    let result = derive(
        ControlledAction::Check,
        stdout.as_bytes(),
        b"",
        CommandOutcome::Exited { code: 0 },
    );

    assert!(result.issues.is_empty(), "{:?}", result.issues);
    assert_eq!(result.outcome, DiagnosticOutcome::Success);
    assert!(result.output.is_empty());
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(result.diagnostics[0].spans.len(), 2);
    assert_eq!(result.diagnostics[0].spans[0].label, None);
    assert_eq!(result.diagnostics[0].spans[0].suggested_replacement, None);
    assert_eq!(
        result.diagnostics[0].spans[0].suggestion_applicability,
        None
    );
    assert_eq!(result.diagnostics[0].spans[1].label.as_deref(), Some(""));
    assert_eq!(
        result.diagnostics[0].spans[1]
            .suggested_replacement
            .as_deref(),
        Some("")
    );
    assert_eq!(
        result.diagnostics[0].spans[1]
            .suggestion_applicability
            .as_deref(),
        Some("FutureApplicability")
    );
    assert_eq!(result.diagnostics[0].children.len(), 1);
    assert_eq!(result.diagnostics[0].children[0].spans.len(), 1);
    assert_eq!(
        result.diagnostics[0].children[0].spans[0]
            .suggestion_applicability
            .as_deref(),
        Some("")
    );
}

#[test]
fn review3_nested_rejections_retain_every_recognized_record_once() {
    let mut wrong_span_list = compiler_message_value("wrong span list");
    wrong_span_list["message"]["spans"] = serde_json::json!(true);

    let mut missing_span_field = compiler_message_value("missing span field");
    missing_span_field["message"]["spans"] = serde_json::json!([{
        "byte_start": 0,
        "byte_end": 0,
        "line_start": 1,
        "line_end": 1,
        "column_start": 1,
        "column_end": 1,
        "is_primary": true
    }]);

    let mut wrong_child_item = compiler_message_value("wrong child item");
    wrong_child_item["message"]["children"] = serde_json::json!([true]);

    let mut wrong_child_span_list = compiler_message_value("wrong child span list");
    let mut child = diagnostic_child("child with wrong spans", Some(serde_json::json!([])));
    child["spans"] = serde_json::json!(true);
    wrong_child_span_list["message"]["children"] = serde_json::json!([child]);

    let mut wrong_nested_child_list = compiler_message_value("wrong nested child list");
    wrong_nested_child_list["message"]["children"] = serde_json::json!([diagnostic_child(
        "child with wrong children",
        Some(serde_json::json!(true))
    )]);

    let partial_artifact = serde_json::json!({
        "reason": "compiler-artifact",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": {
            "kind": ["bin"], "crate_types": ["bin"], "name": "student",
            "src_path": "/work/src/main.rs", "edition": "2024"
        },
        "profile": {
            "opt_level": "0", "debuginfo": null, "debug_assertions": true,
            "overflow_checks": true, "test": false
        },
        "features": [true, "retained-feature"],
        "filenames": ["/work/target/debug/student"],
        "executable": null,
        "fresh": true
    });

    let malformed = [
        wrong_span_list,
        missing_span_field,
        wrong_child_item,
        wrong_child_span_list,
        wrong_nested_child_list,
        partial_artifact,
    ]
    .map(|value| value.to_string());
    let stdout = format!(
        "{}\n{{\"reason\":\"build-finished\",\"success\":true}}\n",
        malformed.join("\n")
    );
    let result = derive(
        ControlledAction::Check,
        stdout.as_bytes(),
        b"",
        CommandOutcome::Exited { code: 0 },
    );

    assert_eq!(result.issues, vec![DiagnosticEvidenceIssue::Malformed]);
    assert_eq!(result.outcome, DiagnosticOutcome::Unknown);
    assert!(!result.known_empty());
    assert_eq!(
        result.diagnostics.len(),
        4,
        "useful partial data is retained"
    );
    assert_eq!(result.artifacts.len(), 1);
    assert_eq!(result.artifacts[0].features, ["retained-feature"]);
    assert_eq!(result.output.len(), malformed.len());
    for (line, expected) in result.output.iter().zip(&malformed) {
        assert_eq!(line.stream, OutputStream::Stdout);
        assert_eq!(line.original_bytes().unwrap(), expected.as_bytes());
    }
}

#[test]
fn review3_deepest_retained_child_requires_bounded_children_shape() {
    let mut valid = compiler_message_value("valid deepest child");
    valid["message"]["children"] = four_level_children(Some(serde_json::json!([])));
    let valid_line = valid.to_string();
    let valid_stdout =
        format!("{valid_line}\n{{\"reason\":\"build-finished\",\"success\":true}}\n");
    let valid_result = derive(
        ControlledAction::Check,
        valid_stdout.as_bytes(),
        b"",
        CommandOutcome::Exited { code: 0 },
    );
    assert!(valid_result.issues.is_empty(), "{:?}", valid_result.issues);
    assert_eq!(valid_result.outcome, DiagnosticOutcome::Success);
    assert_eq!(valid_result.diagnostics.len(), 1);
    assert!(valid_result.output.is_empty());

    for (name, deepest_children, issue) in [
        (
            "missing deepest children",
            None,
            DiagnosticEvidenceIssue::Malformed,
        ),
        (
            "wrong deepest children type",
            Some(serde_json::json!(true)),
            DiagnosticEvidenceIssue::Malformed,
        ),
        (
            "nonempty children beyond bound",
            Some(serde_json::json!([true])),
            DiagnosticEvidenceIssue::Oversized,
        ),
    ] {
        let mut message = compiler_message_value(name);
        message["message"]["children"] = four_level_children(deepest_children);
        let malformed = message.to_string();
        let stdout = format!("{malformed}\n{{\"reason\":\"build-finished\",\"success\":true}}\n");
        let result = derive(
            ControlledAction::Check,
            stdout.as_bytes(),
            b"",
            CommandOutcome::Exited { code: 0 },
        );

        assert!(
            result.issues.contains(&issue),
            "{name}: {:?}",
            result.issues
        );
        assert_eq!(result.outcome, DiagnosticOutcome::Unknown, "{name}");
        assert_eq!(
            result.diagnostics.len(),
            1,
            "{name}: keep bounded partial data"
        );
        assert_eq!(result.output.len(), 1, "{name}: retain once");
        assert_eq!(
            result.output[0].original_bytes().unwrap(),
            malformed.as_bytes()
        );
    }
}

#[test]
fn review3_real_blank_records_remain_protocol_or_output_evidence() {
    let message = compiler_message("warning", "blank record protocol");
    let terminal = "{\"reason\":\"build-finished\",\"success\":true}";
    let pre_terminal = format!("\r\n{message}\r\n\r\n{terminal}\r\n");
    let pre_terminal_result = derive(
        ControlledAction::Check,
        pre_terminal.as_bytes(),
        b"",
        CommandOutcome::Exited { code: 0 },
    );
    assert!(
        pre_terminal_result
            .issues
            .contains(&DiagnosticEvidenceIssue::Unexpected)
    );
    assert_eq!(pre_terminal_result.outcome, DiagnosticOutcome::Unknown);
    assert_eq!(pre_terminal_result.output.len(), 2);
    assert!(pre_terminal_result.output.iter().all(|line| {
        line.stream == OutputStream::Stdout && line.original_bytes().unwrap().is_empty()
    }));

    for action in [ControlledAction::Run, ControlledAction::Test] {
        let post_terminal = format!("{terminal}\r\n\r\n");
        let result = derive(
            action,
            post_terminal.as_bytes(),
            b"",
            CommandOutcome::Exited { code: 0 },
        );
        assert!(result.issues.is_empty(), "{action:?}: {:?}", result.issues);
        assert_eq!(result.outcome, DiagnosticOutcome::Success, "{action:?}");
        assert_eq!(result.output.len(), 1, "{action:?}");
        assert_eq!(result.output[0].stream, OutputStream::Stdout);
        assert!(result.output[0].original_bytes().unwrap().is_empty());
    }

    let stdout = format!("{terminal}\r\n");
    let stderr_result = derive(
        ControlledAction::Check,
        stdout.as_bytes(),
        b"\r\n\n",
        CommandOutcome::Exited { code: 0 },
    );
    assert!(
        stderr_result.issues.is_empty(),
        "{:?}",
        stderr_result.issues
    );
    assert_eq!(stderr_result.output.len(), 2);
    assert!(stderr_result.output.iter().all(|line| {
        line.stream == OutputStream::Stderr && line.original_bytes().unwrap().is_empty()
    }));
}

#[test]
fn review3_empty_capture_and_final_delimiter_do_not_invent_blank_records() {
    let empty = derive(
        ControlledAction::Check,
        b"",
        b"",
        CommandOutcome::Exited { code: 0 },
    );
    assert_eq!(empty.issues, vec![DiagnosticEvidenceIssue::Missing]);
    assert_eq!(empty.outcome, DiagnosticOutcome::Unknown);
    assert!(!empty.known_empty());
    assert!(empty.output.is_empty());

    for ending in ["\n", "\r\n"] {
        let stdout = format!("{{\"reason\":\"build-finished\",\"success\":true}}{ending}");
        let result = derive(
            ControlledAction::Check,
            stdout.as_bytes(),
            b"",
            CommandOutcome::Exited { code: 0 },
        );
        assert!(result.issues.is_empty(), "{ending:?}: {:?}", result.issues);
        assert_eq!(result.outcome, DiagnosticOutcome::Success, "{ending:?}");
        assert!(result.known_empty(), "{ending:?}");
        assert!(result.output.is_empty(), "{ending:?}");
    }
}

#[test]
fn retains_real_cargo_error_warning_target_artifact_spans_and_suggestions() {
    let compiler_error = serde_json::json!({
        "reason": "compiler-message",
        "package_id": "path+file:///work#student@0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": {
            "kind": ["bin"], "crate_types": ["bin"], "name": "student",
            "src_path": "/work/src/main.rs", "edition": "2024",
            "doc": true, "doctest": false, "test": true
        },
        "message": {
            "rendered": "error[E0308]: mismatched types\n --> src/main.rs:2:13\n",
            "message": "mismatched types",
            "code": {"code": "E0308", "explanation": null},
            "level": "error",
            "spans": [{
                "file_name": "src/main.rs", "byte_start": 25, "byte_end": 31,
                "line_start": 2, "line_end": 2, "column_start": 13, "column_end": 19,
                "is_primary": true, "text": [], "label": "expected u32",
                "suggested_replacement": null, "suggestion_applicability": null,
                "expansion": null
            }],
            "children": [{
                "message": "convert the value", "code": null, "level": "help",
                "spans": [{
                    "file_name": "src/main.rs", "byte_start": 25, "byte_end": 31,
                    "line_start": 2, "line_end": 2, "column_start": 13, "column_end": 19,
                    "is_primary": false, "text": [], "label": null,
                    "suggested_replacement": "value.into()",
                    "suggestion_applicability": "MaybeIncorrect", "expansion": null
                }],
                "children": [], "rendered": null
            }]
        }
    });
    let warning = serde_json::json!({
        "reason": "compiler-message", "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": {
            "kind": ["bin"], "crate_types": ["bin"], "name": "student",
            "src_path": "/work/src/main.rs", "edition": "2024"
        },
        "message": {
            "rendered": "warning: unused variable: `crab`\n", "message": "unused variable: `crab`",
            "code": {"code": "unused_variables", "explanation": null}, "level": "warning",
            "spans": [], "children": []
        }
    });
    let artifact = serde_json::json!({
        "reason": "compiler-artifact", "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": {
            "kind": ["bin"], "crate_types": ["bin"], "name": "student",
            "src_path": "/work/src/main.rs", "edition": "2024"
        },
        "profile": {"opt_level":"0", "debuginfo":2, "debug_assertions":true,
                    "overflow_checks":true, "test":false},
        "features": [], "filenames": ["/work/target/debug/student"],
        "executable": "/work/target/debug/student", "fresh": false
    });
    let stdout = format!(
        "{compiler_error}\n{warning}\n{artifact}\n{{\"reason\":\"build-finished\",\"success\":false}}\n"
    );

    let result = derive(
        ControlledAction::Check,
        stdout.as_bytes(),
        b"",
        CommandOutcome::Exited { code: 101 },
    );

    assert!(result.issues.is_empty(), "{:?}", result.issues);
    assert_eq!(result.outcome, DiagnosticOutcome::CompilerErrors);
    assert_eq!(result.identity.command_id.as_str(), "command-42");
    assert_eq!(
        result.identity.workspace,
        started(ControlledAction::Check).before
    );
    assert_eq!(result.identity.argv[5], "--message-format=json");
    assert_eq!(result.diagnostics.len(), 2);
    let error = &result.diagnostics[0];
    assert_eq!(error.code.as_deref(), Some("E0308"));
    assert_eq!(error.level, "error");
    assert!(error.rendered.as_deref().unwrap().contains("src/main.rs"));
    assert_eq!(error.target.name, "student");
    assert_eq!(error.spans.len(), 1);
    assert!(error.spans[0].is_primary);
    assert_eq!(
        error.children[0].spans[0].suggested_replacement.as_deref(),
        Some("value.into()")
    );
    assert_eq!(result.artifacts.len(), 1);
    assert_eq!(
        result.artifacts[0].filenames,
        ["/work/target/debug/student"]
    );
    assert_eq!(
        result.artifacts[0].executable.as_deref(),
        Some("/work/target/debug/student")
    );
}

#[test]
fn distinguishes_complete_empty_compiler_errors_and_generic_nonzero() {
    let empty = derive(
        ControlledAction::Check,
        b"{\"reason\":\"build-finished\",\"success\":true}\n",
        b"",
        CommandOutcome::Exited { code: 0 },
    );
    assert!(empty.known_empty());
    assert_eq!(empty.outcome, DiagnosticOutcome::Success);

    let generic = derive(
        ControlledAction::Test,
        b"{\"reason\":\"build-finished\",\"success\":true}\ntest failed\n",
        b"assertion failed\n",
        CommandOutcome::Exited { code: 101 },
    );
    assert!(generic.known_empty());
    assert_eq!(generic.outcome, DiagnosticOutcome::NonzeroExit);
    assert_eq!(generic.output.len(), 2);
}

#[test]
fn empty_or_unterminated_structured_output_never_claims_known_empty_or_success() {
    for stdout in [
        b"".as_slice(),
        b"student output without Cargo messages\n".as_slice(),
        b"{\"reason\":\"compiler-artifact\"}\n".as_slice(),
    ] {
        let result = derive(
            ControlledAction::Run,
            stdout,
            b"",
            CommandOutcome::Exited { code: 0 },
        );
        assert!(
            result.issues.contains(&DiagnosticEvidenceIssue::Missing),
            "stdout={stdout:?}; issues={:?}",
            result.issues
        );
        assert!(!result.known_empty());
        assert_eq!(result.outcome, DiagnosticOutcome::Unknown);
    }
}

#[test]
fn a_failed_build_message_cannot_be_promoted_to_success_by_exit_zero() {
    let result = derive(
        ControlledAction::Check,
        b"{\"reason\":\"build-finished\",\"success\":false}\n",
        b"",
        CommandOutcome::Exited { code: 0 },
    );
    assert!(result.issues.contains(&DiagnosticEvidenceIssue::Unexpected));
    assert!(!result.known_empty());
    assert_eq!(result.outcome, DiagnosticOutcome::Unknown);
}

#[test]
fn incomplete_capture_and_execution_failure_never_claim_known_empty_or_success() {
    let start = started(ControlledAction::Check);
    let stdout = b"{\"reason\":\"build-finished\",\"success\":true}\n";
    let output = chunks(&start, OutputStream::Stdout, stdout, &[9]);
    for (completeness, expected) in [
        (
            CaptureCompleteness::Truncated,
            DiagnosticEvidenceIssue::Truncated,
        ),
        (
            CaptureCompleteness::ReadFailed,
            DiagnosticEvidenceIssue::ReadFailed,
        ),
    ] {
        let finish = finished(
            &start,
            CommandOutcome::Exited { code: 0 },
            CommandCapture {
                bytes: stdout.len() as u64,
                completeness,
                mode: CommandCaptureMode::Captured,
            },
            complete(0),
        );
        let result = derive_command_diagnostics(&start, &output, &finish);
        assert!(result.issues.contains(&expected));
        assert!(!result.known_empty());
        assert_eq!(result.outcome, DiagnosticOutcome::Unknown);
    }

    let finish = finished(
        &start,
        CommandOutcome::LaunchFailed { os_code: Some(2) },
        CommandCapture {
            bytes: 0,
            completeness: CaptureCompleteness::Unavailable,
            mode: CommandCaptureMode::Captured,
        },
        CommandCapture {
            bytes: 0,
            completeness: CaptureCompleteness::Unavailable,
            mode: CommandCaptureMode::Captured,
        },
    );
    let result = derive_command_diagnostics(&start, &[], &finish);
    assert!(
        result
            .issues
            .contains(&DiagnosticEvidenceIssue::Unavailable)
    );
    assert!(
        result
            .issues
            .contains(&DiagnosticEvidenceIssue::ExecutionFailed)
    );
    assert_eq!(result.outcome, DiagnosticOutcome::ExecutionFailed);
}

#[test]
fn malformed_deep_oversized_unexpected_and_invalid_utf8_are_bounded_and_explicit() {
    let cases: Vec<(Vec<u8>, DiagnosticEvidenceIssue)> = vec![
        (b"{not-json}\n".to_vec(), DiagnosticEvidenceIssue::Malformed),
        (
            format!("{}0{}\n", "[".repeat(18), "]".repeat(18)).into_bytes(),
            DiagnosticEvidenceIssue::Malformed,
        ),
        (
            format!(
                "{{\"reason\":\"future-cargo-message\",\"payload\":\"{}\"}}\n",
                "x".repeat(300_000)
            )
            .into_bytes(),
            DiagnosticEvidenceIssue::Oversized,
        ),
        (
            b"{\"reason\":\"future-cargo-message\"}\n".to_vec(),
            DiagnosticEvidenceIssue::Unexpected,
        ),
        (
            b"{\"reason\":\"compiler-message\",\"message\":\xff}\n".to_vec(),
            DiagnosticEvidenceIssue::InvalidUtf8,
        ),
    ];
    for (stdout, expected) in cases {
        let result = derive(
            ControlledAction::Check,
            &stdout,
            b"",
            CommandOutcome::Exited { code: 0 },
        );
        assert!(
            result.issues.contains(&expected),
            "expected {expected:?}: {:?}",
            result.issues
        );
        assert_eq!(result.outcome, DiagnosticOutcome::Unknown);
        assert!(!result.known_empty());
        assert!(
            result
                .output
                .iter()
                .all(|line| line.bytes_hex.len() <= 8192)
        );
    }
}

#[test]
fn run_program_stdout_can_coexist_with_structured_messages() {
    let result = derive(
        ControlledAction::Run,
        b"{\"reason\":\"build-finished\",\"success\":true}\nhello from student\n",
        b"student stderr\n",
        CommandOutcome::Exited { code: 0 },
    );
    assert!(result.issues.is_empty(), "{:?}", result.issues);
    assert!(result.known_empty());
    assert_eq!(result.output.len(), 2);
    assert_eq!(result.output[0].stream, OutputStream::Stdout);
    assert_eq!(
        result.output[0].original_bytes().unwrap(),
        b"hello from student"
    );
    assert_eq!(result.output[1].stream, OutputStream::Stderr);
    assert_eq!(
        result.output[1].original_bytes().unwrap(),
        b"student stderr"
    );
}

#[test]
fn run_closes_the_cargo_stream_and_retains_all_post_terminal_stdout_as_bytes() {
    let forged_diagnostic = compiler_message("error", "forged by program stdout");
    let forged_artifact = serde_json::json!({
        "reason": "compiler-artifact",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": {
            "kind": ["bin"], "crate_types": ["bin"], "name": "student",
            "src_path": "/work/src/main.rs", "edition": "2024"
        },
        "profile": {
            "opt_level": "0", "debuginfo": 0, "debug_assertions": true,
            "overflow_checks": true, "test": false
        },
        "features": [], "filenames": ["/forged"], "executable": "/forged",
        "fresh": false
    });
    let mut stdout = format!(
        "{{\"reason\":\"build-finished\",\"success\":true}}\n{forged_diagnostic}\n{forged_artifact}\n"
    )
    .into_bytes();
    stdout.extend_from_slice(b"program bytes: \xff\xfe\n");

    for action in [ControlledAction::Run, ControlledAction::Test] {
        let result = derive(action, &stdout, b"", CommandOutcome::Exited { code: 0 });

        assert!(result.issues.is_empty(), "{action:?}: {:?}", result.issues);
        assert_eq!(result.outcome, DiagnosticOutcome::Success);
        assert!(result.known_empty());
        assert_eq!(result.structured_messages, 1);
        assert!(result.diagnostics.is_empty());
        assert!(result.artifacts.is_empty());
        assert_eq!(result.output.len(), 3);
        assert_eq!(
            result.output[0].original_bytes().unwrap(),
            forged_diagnostic.as_bytes()
        );
        assert_eq!(
            result.output[1].original_bytes().unwrap(),
            forged_artifact.to_string().as_bytes()
        );
        assert_eq!(
            result.output[2].original_bytes().unwrap(),
            b"program bytes: \xff\xfe"
        );
    }
}

#[test]
fn non_runtime_trailing_stdout_is_retained_but_explicitly_unexpected() {
    for trailing in [
        compiler_message("warning", "late warning"),
        "plain trailing output".to_owned(),
    ] {
        let stdout = format!("{{\"reason\":\"build-finished\",\"success\":true}}\n{trailing}\n");
        let result = derive(
            ControlledAction::Check,
            stdout.as_bytes(),
            b"",
            CommandOutcome::Exited { code: 0 },
        );

        assert_eq!(result.structured_messages, 1);
        assert!(result.diagnostics.is_empty());
        assert!(result.issues.contains(&DiagnosticEvidenceIssue::Unexpected));
        assert_eq!(result.outcome, DiagnosticOutcome::Unknown);
        assert_eq!(result.output.len(), 1);
        assert_eq!(
            result.output[0].original_bytes().unwrap(),
            trailing.as_bytes()
        );
    }
}

#[test]
fn terminal_status_diagnostic_severity_action_and_exit_must_be_consistent() {
    struct Case {
        action: ControlledAction,
        success: bool,
        level: Option<&'static str>,
        exit: i32,
        expected: DiagnosticOutcome,
        unexpected: bool,
    }
    let cases = [
        Case {
            action: ControlledAction::Check,
            success: true,
            level: Some("error"),
            exit: 0,
            expected: DiagnosticOutcome::Unknown,
            unexpected: true,
        },
        Case {
            action: ControlledAction::Check,
            success: true,
            level: None,
            exit: 101,
            expected: DiagnosticOutcome::Unknown,
            unexpected: true,
        },
        Case {
            action: ControlledAction::Run,
            success: true,
            level: None,
            exit: 101,
            expected: DiagnosticOutcome::NonzeroExit,
            unexpected: false,
        },
        Case {
            action: ControlledAction::Check,
            success: false,
            level: Some("failure-note"),
            exit: 101,
            expected: DiagnosticOutcome::CompilerErrors,
            unexpected: false,
        },
        Case {
            action: ControlledAction::Check,
            success: false,
            level: Some("error: internal compiler error"),
            exit: 101,
            expected: DiagnosticOutcome::CompilerErrors,
            unexpected: false,
        },
        Case {
            action: ControlledAction::Check,
            success: false,
            level: Some("warning"),
            exit: 101,
            expected: DiagnosticOutcome::NonzeroExit,
            unexpected: false,
        },
    ];

    for case in cases {
        let diagnostic = case
            .level
            .map(|level| format!("{}\n", compiler_message(level, "message")))
            .unwrap_or_default();
        let stdout = format!(
            "{diagnostic}{{\"reason\":\"build-finished\",\"success\":{}}}\n",
            case.success
        );
        let result = derive(
            case.action,
            stdout.as_bytes(),
            b"",
            CommandOutcome::Exited { code: case.exit },
        );

        assert_eq!(
            result.outcome, case.expected,
            "action={:?}, success={}, level={:?}, exit={}, issues={:?}",
            case.action, case.success, case.level, case.exit, result.issues
        );
        assert_eq!(
            result.issues.contains(&DiagnosticEvidenceIssue::Unexpected),
            case.unexpected,
            "action={:?}, success={}, level={:?}, exit={}, issues={:?}",
            case.action,
            case.success,
            case.level,
            case.exit,
            result.issues
        );
    }
}

#[test]
fn unknown_primary_diagnostic_severity_is_not_trusted() {
    let stdout = format!(
        "{}\n{{\"reason\":\"build-finished\",\"success\":true}}\n",
        compiler_message("ice", "undocumented severity")
    );
    let result = derive(
        ControlledAction::Check,
        stdout.as_bytes(),
        b"",
        CommandOutcome::Exited { code: 0 },
    );

    assert!(result.issues.contains(&DiagnosticEvidenceIssue::Unexpected));
    assert_eq!(result.outcome, DiagnosticOutcome::Unknown);
}

#[test]
fn recognized_reasons_reject_missing_and_wrong_type_required_payloads() {
    let target = serde_json::json!({
        "kind": ["bin"],
        "crate_types": ["bin"],
        "name": "student",
        "src_path": "/work/src/main.rs",
        "edition": "2024"
    });
    let message = serde_json::json!({
        "rendered": null,
        "message": "warning",
        "code": null,
        "level": "warning",
        "spans": [],
        "children": []
    });
    let profile = serde_json::json!({
        "opt_level": "0",
        "debuginfo": null,
        "debug_assertions": true,
        "overflow_checks": true,
        "test": false
    });
    let mut missing_message_target = serde_json::json!({
        "reason": "compiler-message",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": target.clone(),
        "message": message.clone()
    });
    missing_message_target
        .as_object_mut()
        .unwrap()
        .remove("target");
    let wrong_message = serde_json::json!({
        "reason": "compiler-message",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": target.clone(),
        "message": true
    });
    let mut missing_message_code = serde_json::json!({
        "reason": "compiler-message",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": target.clone(),
        "message": message.clone()
    });
    missing_message_code["message"]
        .as_object_mut()
        .unwrap()
        .remove("code");
    let mut missing_message_rendered = serde_json::json!({
        "reason": "compiler-message",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": target.clone(),
        "message": message
    });
    missing_message_rendered["message"]
        .as_object_mut()
        .unwrap()
        .remove("rendered");
    let mut missing_artifact_profile = serde_json::json!({
        "reason": "compiler-artifact",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": target.clone(),
        "profile": profile,
        "features": [],
        "filenames": [],
        "executable": null,
        "fresh": true
    });
    missing_artifact_profile
        .as_object_mut()
        .unwrap()
        .remove("profile");
    let wrong_artifact_target = serde_json::json!({
        "reason": "compiler-artifact",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": false,
        "profile": {
            "opt_level": "0", "debuginfo": 0, "debug_assertions": true,
            "overflow_checks": true, "test": false
        },
        "features": [], "filenames": [], "executable": null, "fresh": true
    });
    let missing_artifact_executable = serde_json::json!({
        "reason": "compiler-artifact",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": target.clone(),
        "profile": {
            "opt_level": "0", "debuginfo": 0, "debug_assertions": true,
            "overflow_checks": true, "test": false
        },
        "features": [], "filenames": [], "fresh": true
    });
    let wrong_artifact_debuginfo = serde_json::json!({
        "reason": "compiler-artifact",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": target,
        "profile": {
            "opt_level": "0", "debuginfo": false, "debug_assertions": true,
            "overflow_checks": true, "test": false
        },
        "features": [], "filenames": [], "executable": null, "fresh": true
    });
    let missing_build_script_payload = serde_json::json!({
        "reason": "build-script-executed"
    });
    let wrong_build_script_environment = serde_json::json!({
        "reason": "build-script-executed",
        "package_id": "student 0.1.0",
        "linked_libs": [],
        "linked_paths": [],
        "cfgs": [],
        "env": true,
        "out_dir": "/work/target/debug/build/student/out"
    });
    let missing_terminal_success = serde_json::json!({
        "reason": "build-finished"
    });
    let wrong_terminal_success = serde_json::json!({
        "reason": "build-finished",
        "success": "yes"
    });

    for (name, malformed) in [
        ("compiler-message missing target", missing_message_target),
        ("compiler-message wrong message type", wrong_message),
        (
            "compiler-message missing nullable code",
            missing_message_code,
        ),
        (
            "compiler-message missing nullable rendered",
            missing_message_rendered,
        ),
        (
            "compiler-artifact missing profile",
            missing_artifact_profile,
        ),
        ("compiler-artifact wrong target type", wrong_artifact_target),
        (
            "compiler-artifact missing nullable executable",
            missing_artifact_executable,
        ),
        (
            "compiler-artifact wrong debuginfo type",
            wrong_artifact_debuginfo,
        ),
        (
            "build-script-executed missing payload",
            missing_build_script_payload,
        ),
        (
            "build-script-executed wrong environment type",
            wrong_build_script_environment,
        ),
        ("build-finished missing success", missing_terminal_success),
        ("build-finished wrong success type", wrong_terminal_success),
    ] {
        let malformed = malformed.to_string();
        let stdout = format!("{malformed}\n{{\"reason\":\"build-finished\",\"success\":true}}\n");
        let result = derive(
            ControlledAction::Check,
            stdout.as_bytes(),
            b"",
            CommandOutcome::Exited { code: 0 },
        );

        assert!(
            result.issues.contains(&DiagnosticEvidenceIssue::Malformed),
            "{name}: {:?}",
            result.issues
        );
        assert_eq!(result.outcome, DiagnosticOutcome::Unknown, "{name}");
        assert!(!result.known_empty(), "{name}");
        assert!(
            result.output.iter().any(|line| line
                .original_bytes()
                .is_ok_and(|bytes| bytes == malformed.as_bytes())),
            "{name}: malformed recognized bytes were not retained"
        );
    }
}

#[test]
fn documented_nullable_fields_and_valid_build_script_payload_are_accepted() {
    let target = serde_json::json!({
        "kind": ["bin"], "crate_types": ["bin"], "name": "student",
        "src_path": "/work/src/main.rs", "edition": "2024"
    });
    let diagnostic = serde_json::json!({
        "reason": "compiler-message",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": target.clone(),
        "message": {
            "rendered": null, "message": "warning", "code": null,
            "level": "warning", "spans": [], "children": []
        }
    });
    let null_debuginfo = serde_json::json!({
        "reason": "compiler-artifact",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": target.clone(),
        "profile": {
            "opt_level": "0", "debuginfo": null, "debug_assertions": true,
            "overflow_checks": true, "test": false
        },
        "features": [], "filenames": [], "executable": null, "fresh": true
    });
    let string_debuginfo = serde_json::json!({
        "reason": "compiler-artifact",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": target,
        "profile": {
            "opt_level": "0", "debuginfo": "line-tables-only",
            "debug_assertions": true, "overflow_checks": true, "test": false
        },
        "features": [], "filenames": [], "executable": null, "fresh": true
    });
    let build_script = serde_json::json!({
        "reason": "build-script-executed",
        "package_id": "student 0.1.0",
        "linked_libs": ["static=student"],
        "linked_paths": ["native=/work/lib"],
        "cfgs": ["student_cfg"],
        "env": [["STUDENT_OUT", "value"]],
        "out_dir": "/work/target/debug/build/student/out"
    });
    let stdout = format!(
        "{diagnostic}\n{null_debuginfo}\n{string_debuginfo}\n{build_script}\n{{\"reason\":\"build-finished\",\"success\":true}}\n"
    );
    let result = derive(
        ControlledAction::Check,
        stdout.as_bytes(),
        b"",
        CommandOutcome::Exited { code: 0 },
    );

    assert!(result.issues.is_empty(), "{:?}", result.issues);
    assert_eq!(result.outcome, DiagnosticOutcome::Success);
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(result.artifacts.len(), 2);
    assert_eq!(
        result.artifacts[0].profile.as_ref().unwrap().debuginfo,
        "null"
    );
    assert_eq!(
        result.artifacts[1].profile.as_ref().unwrap().debuginfo,
        "\"line-tables-only\""
    );
}
