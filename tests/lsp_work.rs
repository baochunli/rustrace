//! Initial behavioral Reds through the real work CLI, not a standalone transport.
//! RESUMED LIGHT PREP / UNEXECUTED: these sources are not Red evidence until run.
#![cfg(unix)]

#[path = "support/test_home.rs"]
mod test_home;
use rustrace::session::{ProductionSession, read_recovery_evidence};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::PathBuf};

fn fixture(mode: &str, expect_reconciled: bool) -> (PathBuf, Vec<Value>, Vec<Value>) {
    let test_home = test_home::TestHome::new(false);
    let base = std::env::var_os("RUSTRACE_LSP_EVIDENCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let root = base.join(format!("t51-{mode}-{}", std::process::id()));
    fs::create_dir(&root).expect("create unique evidence directory; never overwrite first run");
    eprintln!("T5.1 {mode} retained evidence: {}", root.display());
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/lsp_work.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .arg(mode)
        .arg(&root)
        .output()
        .expect("start installed Python PTY fixture");
    fs::write(root.join("driver.stdout"), &output.stdout).unwrap();
    fs::write(root.join("driver.stderr"), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "production {mode} lifecycle failed: {}\n{}",
        root.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    // Validate actual durable replay and saved/disk equality before LSP assertions.
    let inspected = ProductionSession::inspect(&root.join("assignment.work")).unwrap();
    let expected: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(root.join("expected.json")).unwrap()).unwrap();
    let logical: BTreeMap<_, _> = inspected
        .logical
        .iter()
        .map(|(path, bytes)| {
            (
                path.as_str().to_owned(),
                String::from_utf8(bytes.clone()).unwrap(),
            )
        })
        .collect();
    assert_eq!(logical, expected, "production edit/replay precondition");
    assert_eq!(inspected.saved, inspected.logical);
    if expect_reconciled {
        assert_eq!(inspected.disk, inspected.logical);
    } else {
        assert_ne!(inspected.disk, inspected.logical);
    }
    let transcript = match fs::read_to_string(root.join("tools/frames.jsonl")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => panic!("cannot read retained transcript: {error}"),
    };
    let frames = transcript
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let calls = fs::read_to_string(root.join("tools/calls.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (root, frames, calls)
}

fn submit_and_verify(root: &std::path::Path) -> PathBuf {
    let test_home = test_home::TestHome::new(false);
    let bundle = root.join("submission.zip");
    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("submit")
        .arg(root.join("assignment.work"))
        .args(["--student-id", "student-1", "--output"])
        .arg(&bundle)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "submit failed ({:?}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let report = rustrace::verify::verify_path(&bundle, None);
    assert!(report.is_clean(), "{report:?}");
    bundle
}

fn normalized_completion_events(root: &std::path::Path) -> Vec<Value> {
    let workspace = root.join("assignment.work");
    let metadata = ProductionSession::read_metadata(&workspace).unwrap();
    let pinned = rustrace_workspace::hash::PinnedWorkspaceRoot::open(&workspace).unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .open_journal_file(&metadata.session_id)
        .unwrap();
    let mut journal =
        rustrace_journal::Journal::open_read_only_no_follow(owner.display_path()).unwrap();
    journal
        .read_events(&metadata.session_id, 1, 1_000)
        .unwrap()
        .into_iter()
        .filter_map(|stored| {
            matches!(
                stored.event,
                rustrace_model::Event::FileEdited(_)
                    | rustrace_model::Event::LspCompletionRequested(_)
                    | rustrace_model::Event::LspCompletionAccepted(_)
            )
            .then(|| {
                let mut event = serde_json::to_value(stored.event).unwrap();
                if let Some(document_id) = event
                    .get_mut("payload")
                    .and_then(Value::as_object_mut)
                    .and_then(|payload| payload.get_mut("document_id"))
                {
                    *document_id = Value::String("document".into());
                }
                event
            })
        })
        .collect()
}

fn replay_source(bundle: &std::path::Path) -> BTreeMap<String, Vec<u8>> {
    let mut replay = rustrace::replay_tui::ReplayController::open(bundle).unwrap();
    let last = replay.positions().last().expect("final replay event");
    replay.select(last).unwrap();
    replay
        .selected_event()
        .expect("selected final event")
        .source
        .iter()
        .cloned()
        .collect()
}

fn initial_source(root: &std::path::Path) -> BTreeMap<String, Vec<u8>> {
    let initial: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(root.join("initial.json")).unwrap()).unwrap();
    initial
        .into_iter()
        .map(|(path, text)| (path, text.into_bytes()))
        .collect()
}

#[test]
fn production_lsp_opens_every_document_after_initialization() {
    let (root, frames, _) = fixture("open", true);
    let initialize: Vec<_> = frames
        .iter()
        .enumerate()
        .filter(|(_, frame)| frame["method"] == "initialize")
        .collect();
    assert_eq!(
        initialize.len(),
        1,
        "R1 behavioral Red: real work must start exactly one language service"
    );
    assert_eq!(
        initialize[0].0, 0,
        "initialize must be the first client message"
    );
    assert!(
        initialize[0].1["id"].is_number(),
        "initialize needs a checked request ID"
    );
    let parameters = &initialize[0].1["params"];
    let folders = parameters["workspaceFolders"]
        .as_array()
        .expect("initialize supplies the selected workspace folder");
    assert_eq!(folders.len(), 1);
    assert_eq!(parameters["rootUri"], folders[0]["uri"]);
    assert!(
        folders[0]["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("/assignment.work"))
    );
    assert_eq!(
        parameters["capabilities"]["workspace"]["workspaceFolders"],
        true
    );
    assert_eq!(
        parameters["capabilities"]["workspace"]["configuration"],
        true
    );
    let options = &parameters["initializationOptions"];
    assert_eq!(options["checkOnSave"], false);
    assert_eq!(options["cargo"]["autoreload"], false);
    assert_eq!(options["cargo"]["buildScripts"]["enable"], false);
    assert_eq!(options["cargo"]["buildScripts"]["rebuildOnSave"], false);
    assert_eq!(options["cargo"]["buildScripts"]["useRustcWrapper"], false);
    assert_eq!(
        options["cargo"]["metadataExtraArgs"],
        serde_json::json!(["--locked", "--offline"])
    );
    assert_eq!(options["procMacro"]["enable"], false);
    let initialized: Vec<_> = frames
        .iter()
        .enumerate()
        .filter(|(_, frame)| frame["method"] == "initialized")
        .collect();
    assert_eq!(initialized.len(), 1, "initialized is sent exactly once");
    assert_eq!(
        initialized[0].0, 1,
        "initialized precedes document notifications"
    );
    assert_eq!(initialized[0].1["params"], serde_json::json!({}));
    let initial: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(root.join("initial.json")).unwrap()).unwrap();
    assert!(initial.contains_key("Cargo.toml"));
    assert!(frames.iter().all(|frame| {
        frame["method"] != "textDocument/didOpen"
            || !frame["params"]["textDocument"]["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("/Cargo.toml"))
    }));
    for (name, text) in initial
        .into_iter()
        .filter(|(name, _)| name.ends_with(".rs"))
    {
        let opens: Vec<_> = frames
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                f["method"] == "textDocument/didOpen"
                    && f["params"]["textDocument"]["uri"]
                        .as_str()
                        .unwrap()
                        .ends_with(&format!("/{name}"))
            })
            .collect();
        assert_eq!(opens.len(), 1, "exactly one open for {name}");
        assert!(opens[0].0 > initialized[0].0);
        assert_eq!(opens[0].1["params"]["textDocument"]["text"], text);
        assert!(
            opens[0].1["params"]["textDocument"]["version"]
                .as_i64()
                .is_some()
        );
    }
}

#[test]
fn production_live_diagnostics_render_and_clear_without_recorded_provenance() {
    let (root, frames, _) = fixture("diagnostics", true);
    assert!(
        frames
            .iter()
            .any(|frame| frame["method"] == "textDocument/didChange")
    );
    let shown = fs::read_to_string(root.join("tools/diagnostic-screen.txt")).unwrap();
    assert!(shown.contains("│X// naïve"));
    assert!(shown.contains("live problem"));
    let cleared = fs::read_to_string(root.join("tools/diagnostic-cleared-screen.txt")).unwrap();
    assert!(!cleared.contains("live problem"));

    let workspace = root.join("assignment.work");
    let metadata = ProductionSession::read_metadata(&workspace).unwrap();
    let pinned = rustrace_workspace::hash::PinnedWorkspaceRoot::open(&workspace).unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .open_journal_file(&metadata.session_id)
        .unwrap();
    let mut journal =
        rustrace_journal::Journal::open_read_only_no_follow(owner.display_path()).unwrap();
    let events = journal.read_events(&metadata.session_id, 1, 1_000).unwrap();
    assert!(events.iter().all(|event| {
        !serde_json::to_string(&event.event)
            .unwrap()
            .contains("live problem")
    }));
    drop(journal);
    drop(owner);
    drop(pinned);
    let bundle = submit_and_verify(&root);
    assert_eq!(replay_source(&bundle), initial_source(&root));
}

#[test]
fn malformed_live_diagnostic_notification_is_silent_and_nonfatal() {
    let (root, _, _) = fixture("diagnostics_malformed", true);
    let screen = fs::read_to_string(root.join("tools/malformed-diagnostic-screen.txt")).unwrap();
    assert!(!screen.contains("malformed live hint"));
}

#[test]
fn production_live_diagnostics_are_rust_only_in_a_mixed_workspace() {
    let (root, frames, _) = fixture("diagnostics_rust_only", true);
    let opens = frames
        .iter()
        .filter(|frame| frame["method"] == "textDocument/didOpen")
        .collect::<Vec<_>>();
    assert_eq!(
        opens.len(),
        2,
        "only the two Rust documents should be opened"
    );
    assert!(opens.iter().all(|frame| {
        frame["params"]["textDocument"]["languageId"] == "rust"
            && frame["params"]["textDocument"]["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with(".rs"))
    }));
    assert!(frames.iter().all(|frame| {
        frame["method"] != "textDocument/didOpen"
            || !frame["params"]["textDocument"]["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("/Cargo.toml"))
    }));
    assert!(frames.iter().all(|frame| {
        frame["method"] != "textDocument/didChange"
            || !frame["params"]["textDocument"]["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("/Cargo.toml"))
    }));

    let cargo = fs::read_to_string(root.join("tools/cargo-diagnostic-screen.txt")).unwrap();
    assert!(!cargo.contains("TOML live problem"), "{cargo}");
    let rust = fs::read_to_string(root.join("tools/rust-diagnostic-screen.txt")).unwrap();
    assert!(rust.contains("Rust live problem"), "{rust}");
}

#[test]
fn production_manual_completion_is_safe_single_transaction_at_80x24() {
    let (root, frames, _) = fixture("completion", true);
    let requests = frames
        .iter()
        .filter(|frame| frame["method"] == "textDocument/completion")
        .collect::<Vec<_>>();
    assert_eq!(
        requests.len(),
        1,
        "only explicit Ctrl-Space requests completion"
    );
    assert_eq!(requests[0]["params"]["context"]["triggerKind"], 1);
    assert_eq!(
        requests[0]["params"]["position"],
        serde_json::json!({
            "line": 0,
            "character": 0
        })
    );
    assert_eq!(
        serde_json::from_slice::<Value>(
            &fs::read(root.join("tools/completion-before-trigger.json")).unwrap()
        )
        .unwrap()["requests"],
        0
    );
    let screen = fs::read_to_string(root.join("tools/completion-screen.txt")).unwrap();
    assert!(screen.contains("malicious\\u{1b}[2J\\nlabel"));
    assert!(!screen.contains('\u{1b}'));

    let workspace = root.join("assignment.work");
    let metadata = ProductionSession::read_metadata(&workspace).unwrap();
    let pinned = rustrace_workspace::hash::PinnedWorkspaceRoot::open(&workspace).unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .open_journal_file(&metadata.session_id)
        .unwrap();
    let mut journal =
        rustrace_journal::Journal::open_read_only_no_follow(owner.display_path()).unwrap();
    let events = journal.read_events(&metadata.session_id, 1, 100).unwrap();
    let requested = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                rustrace_model::Event::LspCompletionRequested(_)
            )
        })
        .count();
    let accepted = events
        .iter()
        .filter(|event| matches!(event.event, rustrace_model::Event::LspCompletionAccepted(_)))
        .count();
    let completion_edits = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                rustrace_model::Event::FileEdited(rustrace_model::EditorTransaction {
                    origin: rustrace_model::EditOrigin::Completion,
                    ..
                })
            )
        })
        .count();
    assert_eq!((requested, accepted, completion_edits), (1, 1, 1));
    let accepted = events
        .iter()
        .find_map(|event| match &event.event {
            rustrace_model::Event::LspCompletionAccepted(accepted) => Some(accepted),
            _ => None,
        })
        .unwrap();
    assert_eq!(accepted.label, "malicious\u{1b}[2J\nlabel");
    assert_eq!(accepted.document_version, 2);
    assert_eq!(accepted.primary_edit.start_byte, 0);
    assert_eq!(accepted.primary_edit.end_byte, 0);
    assert_eq!(accepted.primary_edit.inserted_text, "λ");
    assert!(accepted.additional_edits.is_empty());
}

#[test]
fn production_mouse_completion_matches_enter_provenance_verify_and_replay() {
    let (mouse_root, _, _) = fixture("completion_mouse", true);
    let (keyboard_root, _, _) = fixture("completion_mouse_control", true);

    let mouse_events = normalized_completion_events(&mouse_root);
    let keyboard_events = normalized_completion_events(&keyboard_root);
    assert_eq!(mouse_events, keyboard_events);
    assert_eq!(
        mouse_events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "lsp_completion_requested",
            "lsp_completion_accepted",
            "file_edited",
        ]
    );

    let mouse_bundle = submit_and_verify(&mouse_root);
    let keyboard_bundle = submit_and_verify(&keyboard_root);
    assert_eq!(
        replay_source(&mouse_bundle),
        replay_source(&keyboard_bundle)
    );
}

#[test]
fn production_automatic_completion_debounces_and_bounds_one_inflight_request() {
    let (root, frames, _) = fixture("completion_automatic", true);
    let requests = frames
        .iter()
        .filter(|frame| frame["method"] == "textDocument/completion")
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 2, "one initial and one deferred request");
    assert_eq!(
        requests[0]["params"]["position"],
        serde_json::json!({"line": 0, "character": 20})
    );
    assert_eq!(
        requests[1]["params"]["position"],
        serde_json::json!({"line": 0, "character": 21})
    );
    let screen = fs::read_to_string(root.join("tools/automatic-completion-screen.txt")).unwrap();
    assert!(screen.contains("auto-fresh"), "{screen}");
    assert!(!screen.contains("auto-stale"), "{screen}");

    let workspace = root.join("assignment.work");
    let metadata = ProductionSession::read_metadata(&workspace).unwrap();
    let pinned = rustrace_workspace::hash::PinnedWorkspaceRoot::open(&workspace).unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .open_journal_file(&metadata.session_id)
        .unwrap();
    let mut journal =
        rustrace_journal::Journal::open_read_only_no_follow(owner.display_path()).unwrap();
    let events = journal.read_events(&metadata.session_id, 1, 100).unwrap();
    let requested = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                rustrace_model::Event::LspCompletionRequested(_)
            )
        })
        .count();
    let accepted = events
        .iter()
        .filter(|event| matches!(event.event, rustrace_model::Event::LspCompletionAccepted(_)))
        .count();
    let completion_edits = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                rustrace_model::Event::FileEdited(rustrace_model::EditorTransaction {
                    origin: rustrace_model::EditOrigin::Completion,
                    ..
                })
            )
        })
        .count();
    assert_eq!((requested, accepted, completion_edits), (2, 1, 1));
    drop(journal);
    drop(owner);
    drop(pinned);

    let (control_root, control_frames, _) = fixture("completion_automatic_control", true);
    assert_eq!(
        control_frames
            .iter()
            .filter(|frame| frame["method"] == "textDocument/completion")
            .count(),
        2,
        "Ctrl-Space control issues the same two version-bound requests"
    );
    assert_eq!(
        normalized_completion_events(&root),
        normalized_completion_events(&control_root),
        "automatic and Ctrl-Space acceptance must record identical edit/completion events"
    );
    let automatic_bundle = submit_and_verify(&root);
    let control_bundle = submit_and_verify(&control_root);
    assert_eq!(
        replay_source(&automatic_bundle),
        replay_source(&control_bundle)
    );
}

fn assert_deferred_automatic_completion_is_cancelled(mode: &str) {
    let (root, frames, _) = fixture(mode, true);
    let requests = frames
        .iter()
        .filter(|frame| frame["method"] == "textDocument/completion")
        .count();
    assert_eq!(
        requests, 1,
        "{mode}: navigation or console entry must cancel deferred automatic completion"
    );
    let screen =
        fs::read_to_string(root.join("tools/automatic-completion-cancelled-screen.txt")).unwrap();
    assert!(!screen.contains("auto-fresh"), "{mode}: {screen}");
    assert!(!screen.contains("auto-stale"), "{mode}: {screen}");
    assert!(!screen.contains(" COMPLETE "), "{mode}: {screen}");
}

#[test]
fn production_deferred_automatic_completion_is_cancelled_by_keyboard_navigation() {
    assert_deferred_automatic_completion_is_cancelled("completion_automatic_keyboard_cancel");
}

#[test]
fn production_deferred_automatic_completion_is_cancelled_by_mouse_navigation() {
    assert_deferred_automatic_completion_is_cancelled("completion_automatic_mouse_cancel");
}

#[test]
fn production_deferred_automatic_completion_is_cancelled_by_console_entry_and_return() {
    assert_deferred_automatic_completion_is_cancelled("completion_automatic_console_cancel");
}

#[test]
fn production_mouse_and_keyboard_navigation_cancel_the_typing_trigger_equally() {
    let requests = [
        "completion_trigger_mouse_cancel",
        "completion_trigger_keyboard_cancel",
    ]
    .into_iter()
    .map(|mode| {
        let (_, frames, _) = fixture(mode, true);
        (
            mode,
            frames
                .iter()
                .filter(|frame| frame["method"] == "textDocument/completion")
                .count(),
        )
    })
    .collect::<Vec<_>>();
    assert_eq!(
        requests,
        vec![
            ("completion_trigger_mouse_cancel", 0),
            ("completion_trigger_keyboard_cancel", 0),
        ],
        "mouse and keyboard navigation must both disarm the typing timer"
    );
}

#[test]
fn production_automatic_degradation_is_silent_while_ctrl_space_stays_explicit() {
    for (mode, expected_requests, control_message) in [
        ("completion_silence_missing", 0, "completion unavailable"),
        ("completion_silence_crash", 2, "completion requested"),
        ("completion_silence_timeout", 2, "completion requested"),
        ("completion_silence_empty", 2, "completion requested"),
    ] {
        let (root, frames, _) = fixture(mode, true);
        let automatic =
            fs::read_to_string(root.join("tools/automatic-silence-screen.txt")).unwrap();
        assert!(
            !automatic.contains("completion requested"),
            "{mode}: {automatic}"
        );
        assert!(!automatic.contains("action failed"), "{mode}: {automatic}");
        assert!(!automatic.contains(" COMPLETE "), "{mode}: {automatic}");

        let control = fs::read_to_string(root.join("tools/manual-completion-screen.txt")).unwrap();
        assert!(control.contains(control_message), "{mode}: {control}");
        assert!(
            control.contains(if mode == "completion_silence_missing" {
                "action failed"
            } else {
                "notice"
            }),
            "{mode}: {control}"
        );
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["method"] == "textDocument/completion")
                .count(),
            expected_requests,
            "{mode}"
        );
    }
}

#[test]
fn production_completion_navigation_keeps_selected_row_visible_at_80x24() {
    let (root, frames, _) = fixture("completion_navigation", true);
    let requests = frames
        .iter()
        .filter(|frame| frame["method"] == "textDocument/completion")
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 1, "only Ctrl-Space requests completion");
    assert_eq!(requests[0]["params"]["context"]["triggerKind"], 1);

    let third = fs::read_to_string(root.join("tools/completion-third-screen.txt")).unwrap();
    assert!(third.contains(" COMPLETE "));
    for item in 0..4 {
        assert!(third.contains(&format!("item-{item}")));
    }

    let last = fs::read_to_string(root.join("tools/completion-last-screen.txt")).unwrap();
    assert!(last.contains(" COMPLETE "));
    for item in 0..4 {
        assert!(last.contains(&format!("item-{item}")));
    }

    let workspace = root.join("assignment.work");
    let metadata = ProductionSession::read_metadata(&workspace).unwrap();
    let pinned = rustrace_workspace::hash::PinnedWorkspaceRoot::open(&workspace).unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .open_journal_file(&metadata.session_id)
        .unwrap();
    let mut journal =
        rustrace_journal::Journal::open_read_only_no_follow(owner.display_path()).unwrap();
    let events = journal.read_events(&metadata.session_id, 1, 100).unwrap();
    let accepted = events
        .iter()
        .filter_map(|event| match &event.event {
            rustrace_model::Event::LspCompletionAccepted(accepted) => Some(accepted),
            _ => None,
        })
        .collect::<Vec<_>>();
    let completion_edits = events
        .iter()
        .filter_map(|event| match &event.event {
            rustrace_model::Event::FileEdited(transaction)
                if transaction.origin == rustrace_model::EditOrigin::Completion =>
            {
                Some(transaction)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let undo_edits = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                rustrace_model::Event::FileEdited(rustrace_model::EditorTransaction {
                    origin: rustrace_model::EditOrigin::Undo,
                    ..
                })
            )
        })
        .count();
    let redo_edits = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                rustrace_model::Event::FileEdited(rustrace_model::EditorTransaction {
                    origin: rustrace_model::EditOrigin::Redo,
                    ..
                })
            )
        })
        .count();

    assert_eq!((accepted.len(), completion_edits.len()), (1, 1));
    assert_eq!((undo_edits, redo_edits), (1, 1));
    assert_eq!(accepted[0].label, "item-3");
    assert_eq!(accepted[0].primary_edit.inserted_text, "D");
    assert_eq!(
        completion_edits[0].edits,
        vec![accepted[0].primary_edit.clone()]
    );
}

#[test]
fn production_rejected_paste_retires_hidden_completion_at_80x24() {
    let (root, frames, _) = fixture("completion_paste_rejection", true);
    let requests = frames
        .iter()
        .filter(|frame| frame["method"] == "textDocument/completion")
        .count();
    assert_eq!(requests, 1, "Ctrl-Space sends one completion request");

    let screen =
        fs::read_to_string(root.join("tools/completion-paste-rejected-screen.txt")).unwrap();
    assert!(screen.contains("Paste blocked:"));
    assert!(!screen.contains("malicious\\u{1b}[2J\\nlabel"));

    let workspace = root.join("assignment.work");
    let metadata = ProductionSession::read_metadata(&workspace).unwrap();
    let pinned = rustrace_workspace::hash::PinnedWorkspaceRoot::open(&workspace).unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .open_journal_file(&metadata.session_id)
        .unwrap();
    let mut journal =
        rustrace_journal::Journal::open_read_only_no_follow(owner.display_path()).unwrap();
    let events = journal.read_events(&metadata.session_id, 1, 100).unwrap();
    let rejected = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                rustrace_model::Event::PasteRejected(rustrace_model::PasteRejected {
                    reason: rustrace_model::PasteRejectionReason::ExternalInput,
                    channel: rustrace_model::PasteInputChannel::TerminalBracketed,
                })
            )
        })
        .count();
    let accepted = events
        .iter()
        .filter(|event| matches!(event.event, rustrace_model::Event::LspCompletionAccepted(_)))
        .count();
    let completion_edits = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                rustrace_model::Event::FileEdited(rustrace_model::EditorTransaction {
                    origin: rustrace_model::EditOrigin::Completion,
                    ..
                })
            )
        })
        .count();
    assert_eq!((rejected, accepted, completion_edits), (1, 0, 0));

    let initial: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(root.join("initial.json")).unwrap()).unwrap();
    assert_eq!(
        fs::read_to_string(workspace.join("main.rs")).unwrap(),
        initial["main.rs"],
        "rejected paste and the retired completion must not change final source"
    );
}

fn assert_view_transition_retires_delayed_completion(mode: &str) {
    let (root, frames, _) = fixture(mode, true);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["method"] == "textDocument/completion")
            .count(),
        1,
        "the fixture must issue one explicit completion request"
    );
    let returned = fs::read_to_string(root.join("tools/completion-return-screen.txt")).unwrap();
    let authority: Value =
        serde_json::from_slice(&fs::read(root.join("tools/completion-authority.json")).unwrap())
            .unwrap();
    assert_eq!(
        authority["old_response_retired"], true,
        "old completion regained authority after the view transition"
    );
    assert!(
        !returned.contains("item-0"),
        "the delayed result must remain inert after returning to the workspace"
    );

    let workspace = root.join("assignment.work");
    let metadata = ProductionSession::read_metadata(&workspace).unwrap();
    let pinned = rustrace_workspace::hash::PinnedWorkspaceRoot::open(&workspace).unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .open_journal_file(&metadata.session_id)
        .unwrap();
    let mut journal =
        rustrace_journal::Journal::open_read_only_no_follow(owner.display_path()).unwrap();
    let events = journal.read_events(&metadata.session_id, 1, 100).unwrap();
    let requested = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                rustrace_model::Event::LspCompletionRequested(_)
            )
        })
        .count();
    let accepted = events
        .iter()
        .filter(|event| matches!(event.event, rustrace_model::Event::LspCompletionAccepted(_)))
        .count();
    let completion_edits = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                rustrace_model::Event::FileEdited(rustrace_model::EditorTransaction {
                    origin: rustrace_model::EditOrigin::Completion,
                    ..
                })
            )
        })
        .count();
    assert_eq!((requested, accepted, completion_edits), (1, 0, 0));
}

#[test]
fn production_console_transition_retires_delayed_completion_at_80x24() {
    assert_view_transition_retires_delayed_completion("completion_console_retirement");
}

#[test]
fn production_test_case_modal_retires_delayed_completion_at_80x24() {
    assert_view_transition_retires_delayed_completion("completion_test_cases_retirement");
}

#[test]
fn production_lsp_tracks_edits_versions_rename_and_delete() {
    let (root, frames, _) = fixture("edit", true);
    let changes: Vec<_> = frames
        .iter()
        .filter(|f| f["method"] == "textDocument/didChange")
        .collect();
    assert!(
        !changes.is_empty(),
        "R2 behavioral Red: durable production edits must reach LSP"
    );
    let initial: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(root.join("initial.json")).unwrap()).unwrap();
    let mut open: BTreeMap<String, (i64, String)> = BTreeMap::new();
    let mut opened = Vec::new();
    let mut changed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut closed = Vec::new();
    for frame in &frames {
        let document = &frame["params"]["textDocument"];
        match frame["method"].as_str() {
            Some("textDocument/didOpen") => {
                let uri = document["uri"].as_str().unwrap().to_owned();
                assert!(
                    !open.contains_key(&uri),
                    "duplicate open after focus change"
                );
                if uri.ends_with("/renamed.rs") {
                    assert!(!open.keys().any(|old| old.ends_with("/other.rs")));
                    assert_eq!(document["text"], format!("Y{}", initial["other.rs"]));
                }
                open.insert(
                    uri.clone(),
                    (
                        document["version"].as_i64().unwrap(),
                        document["text"].as_str().unwrap().to_owned(),
                    ),
                );
                opened.push(uri);
            }
            Some("textDocument/didChange") => {
                let uri = document["uri"].as_str().unwrap();
                let (version, text) = open.get_mut(uri).expect("change requires open document");
                let next = document["version"].as_i64().unwrap();
                assert!(next > *version, "versions advance for undo/redo too");
                let edits = frame["params"]["contentChanges"].as_array().unwrap();
                assert_eq!(edits.len(), 1);
                assert!(
                    edits[0].get("range").is_none(),
                    "whole-document synchronization"
                );
                *text = edits[0]["text"].as_str().unwrap().to_owned();
                *version = next;
                changed
                    .entry(uri.to_owned())
                    .or_default()
                    .push(text.clone());
            }
            Some("textDocument/didClose") => {
                let uri = document["uri"].as_str().unwrap();
                assert!(open.remove(uri).is_some(), "close requires open document");
                closed.push(uri.to_owned());
            }
            _ => {}
        }
    }
    let main = changed
        .iter()
        .find(|(uri, _)| uri.ends_with("/main.rs"))
        .unwrap()
        .1;
    assert_eq!(
        main,
        &vec![
            format!("X{}", initial["main.rs"]),
            initial["main.rs"].clone(),
            format!("X{}", initial["main.rs"])
        ]
    );
    let other = changed
        .iter()
        .find(|(uri, _)| uri.ends_with("/other.rs"))
        .unwrap()
        .1;
    assert_eq!(other, &vec![format!("Y{}", initial["other.rs"])]);
    let new = changed
        .iter()
        .find(|(uri, _)| uri.ends_with("/new.rs"))
        .unwrap()
        .1;
    assert_eq!(new, &vec!["Z".to_owned()]);
    for name in ["main.rs", "other.rs", "renamed.rs", "new.rs"] {
        assert_eq!(
            opened
                .iter()
                .filter(|uri| uri.ends_with(&format!("/{name}")))
                .count(),
            1
        );
    }
    for name in ["other.rs", "renamed.rs"] {
        assert_eq!(
            closed
                .iter()
                .filter(|uri| uri.ends_with(&format!("/{name}")))
                .count(),
            1
        );
    }
}

#[test]
fn production_f8_is_inert_at_unsafe_boundaries_and_reloads_once_when_safe() {
    let (root, frames, _) = fixture("reload_gate", true);
    let reloads = frames
        .iter()
        .filter(|frame| frame["method"] == "rust-analyzer/reloadWorkspace")
        .collect::<Vec<_>>();
    assert_eq!(reloads.len(), 1);
    assert_eq!(reloads[0]["params"], Value::Null);
    let id = &reloads[0]["id"];
    let server_frames = fs::read_to_string(root.join("tools/server-frames.jsonl")).unwrap();
    assert!(
        server_frames
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .any(|frame| frame["id"] == *id && frame["result"].is_null())
    );
}

#[test]
fn production_rejects_server_mutation_requests_and_answers_configuration() {
    let (_, frames, _) = fixture("requests", true);
    for id in ["apply", "execute"] {
        let response = frames.iter().find(|frame| frame["id"] == id).unwrap();
        assert_eq!(response["error"]["code"], -32601);
        assert!(response["result"].is_null());
    }
    let configuration = frames.iter().find(|frame| frame["id"] == "config").unwrap();
    assert_eq!(configuration["result"].as_array().unwrap().len(), 1);
    assert_eq!(configuration["result"][0]["checkOnSave"], false);
}

#[test]
fn production_crash_restarts_with_fresh_resolution_and_latest_documents() {
    let (root, frames, calls) = fixture("crash", true);
    assert!(
        frames
            .iter()
            .filter(|frame| frame["method"] == "initialize")
            .count()
            >= 2
    );
    assert!(
        calls
            .iter()
            .filter(
                |call| call["tool"] == "rustup" && call["args"] == serde_json::json!(["--version"])
            )
            .count()
            >= 3
    );
    assert!(
        fs::read_dir(root.join("assignment.work/.rustrace"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("language-server-resolution-"))
            .count()
            >= 2
    );
}

#[test]
fn production_environment_is_finite_for_every_probe_and_server_launch() {
    let (root, _, calls) = fixture("policy", true);
    assert!(
        calls
            .iter()
            .all(|call| call["forbidden_present"] == serde_json::json!([]))
    );
    let launch = calls
        .iter()
        .find(|call| call["tool"] == "rust-analyzer" && call["args"] == serde_json::json!([]))
        .unwrap();
    let env = &launch["controlled_env"];
    assert_eq!(env["RUSTUP_AUTO_INSTALL"], "0");
    assert_eq!(env["RUSTUP_TOOLCHAIN"], "pinned");
    assert!(env.get("CARGO_NET_OFFLINE").is_none());
    assert_eq!(env["RUSTC_WRAPPER"], "");
    assert_eq!(env["RUSTC_WORKSPACE_WRAPPER"], "");
    assert!(env["CARGO"].as_str().unwrap().ends_with("/tools/cargo"));
    assert!(env["RUSTC"].as_str().unwrap().ends_with("/tools/rustc"));
    assert!(env["RUSTDOC"].as_str().unwrap().ends_with("/tools/rustdoc"));
    let workspace = fs::canonicalize(root.join("assignment.work")).unwrap();
    assert_eq!(
        env["CARGO_TARGET_DIR"],
        workspace.join("target").to_string_lossy().as_ref()
    );
}

#[test]
fn production_restores_server_source_mutation_and_stops_on_unmanaged_mutation() {
    let (restored, _, _) = fixture("mutate", true);
    assert!(
        fs::read_dir(restored.join("assignment.work/.rustrace"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().starts_with("evidence-"))
    );

    let (invalid, frames, _) = fixture("invalid", false);
    assert_eq!(
        fs::read_to_string(invalid.join("assignment.work/evil.txt")).unwrap(),
        "unmanaged server mutation\n"
    );
    assert!(!frames.iter().any(|frame| frame["method"] == "shutdown"));
}

#[test]
fn production_degrades_without_optional_service_for_bad_server_inputs() {
    for mode in ["missing", "unsupported", "malformed", "flood"] {
        let (_, frames, _) = fixture(mode, true);
        if mode == "missing" {
            assert!(frames.is_empty());
        }
        if mode == "unsupported" {
            assert!(
                !frames
                    .iter()
                    .any(|frame| frame["method"] == "textDocument/didOpen")
            );
        }
    }
}

#[test]
fn production_quit_is_finite_with_blocked_stdin_and_descendant_pipes() {
    let (_, blocked, _) = fixture("blocked", true);
    assert!(blocked.iter().any(|frame| frame["method"] == "shutdown"));
    let (_, descendant, _) = fixture("descendant", true);
    assert!(descendant.iter().any(|frame| frame["method"] == "exit"));
}

#[test]
fn production_quit_is_finite_during_startup_and_with_blocked_stdin() {
    for mode in ["resolve_blocked", "init_blocked", "blocked_stdin"] {
        let (root, frames, _) = fixture(mode, true);
        if mode == "resolve_blocked" {
            assert!(frames.is_empty());
        } else {
            assert!(frames.iter().any(|frame| frame["method"] == "initialize"));
            assert!(!frames.iter().any(|frame| frame["method"] == "shutdown"));
        }
        if mode == "blocked_stdin" {
            assert!(
                root.join("tools/blocked-stdin-ready").is_file(),
                "fixture must block after receiving initialized"
            );
        }
    }
}

#[test]
fn production_reconciles_every_language_service_side_effect_boundary() {
    for mode in [
        "mutate_resolution",
        "mutate_reload",
        "mutate_shutdown",
        "mutate_exit",
    ] {
        let (root, frames, _) = fixture(mode, true);
        let initial: BTreeMap<String, String> =
            serde_json::from_slice(&fs::read(root.join("initial.json")).unwrap()).unwrap();
        let evidence_paths = fs::read_dir(root.join("assignment.work/.rustrace"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("evidence-")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            evidence_paths.len(),
            1,
            "{mode} retains one exact observation"
        );
        let evidence = read_recovery_evidence(&fs::read(&evidence_paths[0]).unwrap()).unwrap();
        let main = rustrace_model::WorkspacePath::new("main.rs").unwrap();
        assert_eq!(evidence.saved[&main], initial["main.rs"].as_bytes());
        assert_eq!(evidence.logical[&main], initial["main.rs"].as_bytes());
        assert_eq!(evidence.disk[&main], b"SERVER MUTATION\n");
        if mode == "mutate_resolution" {
            assert_eq!(
                fs::read(root.join("tools/main-at-server-launch.txt")).unwrap(),
                initial["main.rs"].as_bytes(),
                "launch must follow restoration"
            );
            assert_eq!(
                frames
                    .iter()
                    .filter(|frame| frame["method"] == "initialize")
                    .count(),
                1,
                "unresolved mutation must not cause a replacement launch"
            );
        }
    }
}

#[test]
fn production_stops_queued_language_writes_before_command_ownership() {
    let (root, frames, _) = fixture("command_barrier", true);
    let state: Value =
        serde_json::from_slice(&fs::read(root.join("tools/command-server-state.json")).unwrap())
            .unwrap();
    assert_eq!(state["command_marker"]["active"], true);
    assert_eq!(state["first_server_alive"], false);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["method"] == "rustrace/commandBoundary")
            .count(),
        0,
        "confirmed process cleanup replaces the synthetic protocol fence"
    );
}

#[test]
fn production_stops_analyzer_before_command_and_resyncs_latest_documents() {
    let (root, frames, _) = fixture("command_quiescence", true);
    let initial: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(root.join("initial.json")).unwrap()).unwrap();
    let expected = format!("X{}", initial["main.rs"]);
    let state: Value =
        serde_json::from_slice(&fs::read(root.join("tools/command-server-state.json")).unwrap())
            .unwrap();
    assert_eq!(
        state["command_marker"]["active"], true,
        "regression must observe the durable active command boundary"
    );
    let observed = fs::read(root.join("tools/command-observed.bin")).unwrap();
    let mutation = root.join("tools/mutation.jsonl");
    if mutation.is_file() {
        assert_eq!(
            observed, b"SERVER DURING COMMAND\n",
            "behavioral Red must prove Cargo consumed the autonomous write"
        );
    }
    assert!(
        !mutation.exists(),
        "H1: a stopped analyzer cannot mutate after command ownership begins"
    );
    assert_eq!(
        state["first_server_alive"], false,
        "the first analyzer must be reaped before the command child starts"
    );
    assert_eq!(
        observed,
        expected.as_bytes(),
        "Cargo sees the canonical pre-tree"
    );
    assert!(
        frames
            .iter()
            .filter(|frame| frame["method"] == "initialize")
            .count()
            >= 2,
        "completed command starts fresh installed resolution and LSP generation"
    );
    assert!(
        frames.iter().any(|frame| {
            frame["method"] == "textDocument/didOpen"
                && frame["params"]["textDocument"]["text"] == expected
        }),
        "fresh generation receives the latest document text"
    );
}

#[test]
fn production_failed_and_cancelled_command_setup_restart_latest_documents() {
    for mode in ["command_failed", "command_cancelled"] {
        let (root, frames, _) = fixture(mode, true);
        let initial: BTreeMap<String, String> =
            serde_json::from_slice(&fs::read(root.join("initial.json")).unwrap()).unwrap();
        let expected = format!("X{}", initial["main.rs"]);
        let state: Value = serde_json::from_slice(
            &fs::read(root.join("tools/command-server-state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state["command_marker"]["active"], true, "mode={mode}");
        assert_eq!(state["first_server_alive"], false, "mode={mode}");
        let marker: Value = serde_json::from_slice(
            &fs::read(root.join("assignment.work/.rustrace/command-activity.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(marker["active"], false, "mode={mode}");
        assert!(
            frames
                .iter()
                .filter(|frame| frame["method"] == "initialize")
                .count()
                >= 2,
            "mode={mode}"
        );
        assert!(
            frames.iter().any(|frame| {
                frame["method"] == "textDocument/didOpen"
                    && frame["params"]["textDocument"]["text"] == expected
            }),
            "mode={mode}"
        );
    }
}

#[test]
fn production_quit_keeps_stopped_analyzer_outside_command_ownership() {
    let (root, frames, _) = fixture("quit_command", true);
    let state: Value =
        serde_json::from_slice(&fs::read(root.join("tools/command-server-state.json")).unwrap())
            .unwrap();
    assert_eq!(state["command_marker"]["active"], true);
    assert_eq!(state["first_server_alive"], false);
    assert!(!frames.iter().any(|frame| frame["method"] == "shutdown"));
}

#[test]
fn production_bounds_partial_oversized_frames_and_crash_retries() {
    for mode in ["partial", "oversized"] {
        let (_, frames, _) = fixture(mode, true);
        assert!(frames.iter().any(|frame| frame["method"] == "initialize"));
        assert!(
            !frames
                .iter()
                .any(|frame| frame["method"] == "textDocument/didOpen")
        );
    }
    let (root, frames, _) = fixture("crash_loop", true);
    assert_eq!(
        fs::read_to_string(root.join("tools/server-launches.jsonl"))
            .unwrap()
            .lines()
            .count(),
        6
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["method"] == "initialize")
            .count(),
        6
    );
}
