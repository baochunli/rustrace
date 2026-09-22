#![cfg(unix)]

use rustrace::{
    session::{ConsoleStart, ProductionSession, TestCaseOutcome},
    tui::EditorCommand,
};
use rustrace_model::{
    CaptureCompleteness, CommandCaptureMode, CommandOutcome, ConsoleStdinRoute, ConsoleStdoutRoute,
    ControlledAction, Event, OutputStream, TestCaseComparisonError, TestCaseComparisonOutcome,
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "console"
assignment_version = "v1"
title = "Console"
toolchain = "fixture"
edition = "2024"
allowed_paths = ["*.rs", "Cargo.lock"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

#[test]
fn production_console_session_parent() {
    if std::env::var_os("RUSTRACE_CONSOLE_FIXTURE").is_some() {
        return;
    }
    let (parent, root) = console_fixture("session");
    let output = run_console_child(&root, "production_console_session_child");
    assert!(
        output.status.success(),
        "fixture retained {}; stdout={}; stderr={}",
        parent.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(parent).unwrap();
}

#[test]
fn production_console_recording_failure_parent() {
    if std::env::var_os("RUSTRACE_CONSOLE_FIXTURE").is_some() {
        return;
    }
    let (parent, root) = console_fixture("recording-failure");
    fs::write(
        root.join("target/runner-fixture.json"),
        br#"{"mode":"console_io","mutate_source":false,"echo":false}"#,
    )
    .unwrap();
    let output = run_console_child(&root, "production_console_recording_failure_child");
    assert!(
        output.status.success(),
        "fixture retained {}; stdout={}; stderr={}",
        parent.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(parent).unwrap();
}

#[test]
fn production_console_test_options_parent() {
    if std::env::var_os("RUSTRACE_CONSOLE_FIXTURE").is_some() {
        return;
    }
    let (parent, root) = console_fixture("test-options");
    fs::write(
        root.join("target/runner-fixture.json"),
        br#"{"mode":"console_io","mutate_source":false,"echo":false}"#,
    )
    .unwrap();
    let output = run_console_child(&root, "production_console_test_options_child");
    assert!(
        output.status.success(),
        "fixture retained {}; stdout={}; stderr={}",
        parent.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(parent).unwrap();
}

#[test]
fn production_test_case_session_parent() {
    if std::env::var_os("RUSTRACE_CONSOLE_FIXTURE").is_some() {
        return;
    }
    let (parent, root) = console_fixture("test-case-session");
    let output = run_console_child(&root, "production_test_case_session_child");
    assert!(
        output.status.success(),
        "fixture retained {}; stdout={}; stderr={}",
        parent.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(parent).unwrap();
}

fn console_fixture(name: &str) -> (PathBuf, PathBuf) {
    let parent =
        std::env::temp_dir().join(format!("rustrace-console-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&parent);
    let root = parent.join("assignment.work");
    fs::create_dir_all(root.join("target/bin/v1")).unwrap();
    fs::create_dir(parent.join("test-cases")).unwrap();
    fs::write(root.join("main.rs"), b"A").unwrap();
    fs::write(root.join("Cargo.lock"), b"lock fixture").unwrap();
    fs::write(parent.join("test-cases/input.bin"), b"file\0input\xff").unwrap();
    fs::write(parent.join("test-cases/input.in"), b"file\0input\xff").unwrap();
    fs::write(
        parent.join("test-cases/input.expected"),
        b"stdout:file\0input\xff",
    )
    .unwrap();
    fs::write(parent.join("test-cases/output.bin"), b"preserve").unwrap();
    fs::write(parent.join("test-cases/race-output.bin"), b"preserve").unwrap();
    fs::write(
        root.join("target/runner-fixture.json"),
        br#"{"mode":"console_io","mutate_source":true,"echo":true}"#,
    )
    .unwrap();
    install_tools(&root);
    (parent, root)
}

#[test]
fn production_test_case_session_child() {
    let Some(root) = std::env::var_os("RUSTRACE_CONSOLE_FIXTURE") else {
        return;
    };
    let root = fs::canonicalize(PathBuf::from(root)).unwrap();
    let cases_root = root.parent().unwrap().join("test-cases");
    let mut session = ProductionSession::start(&root, MANIFEST).unwrap();
    let id = session.session_id().clone();
    let case = session
        .list_test_cases()
        .unwrap()
        .into_iter()
        .find(|case| case.name() == "input")
        .unwrap();

    session.start_test_case(case.clone()).unwrap();
    assert!(!session.console_accepts_stdin());
    wait(&mut session);
    let pass = session.take_test_case_result().unwrap();
    assert_eq!(pass.case, case);
    assert_eq!(pass.outcome, TestCaseOutcome::Pass);
    assert!(pass.expected_blake3.is_some());
    assert_eq!(pass.expected_blake3, pass.actual_blake3);

    assert_eq!(
        session
            .start_console_command("cargo run < input.in")
            .unwrap(),
        ConsoleStart::Started
    );
    wait(&mut session);
    assert!(session.take_test_case_result().is_none());

    fs::write(cases_root.join("input.expected"), b"different\n").unwrap();
    session.start_test_case(case.clone()).unwrap();
    wait(&mut session);
    let failed = session.take_test_case_result().unwrap();
    let TestCaseOutcome::Fail(mismatch) = failed.outcome else {
        panic!("expected a mismatch: {failed:?}");
    };
    assert_eq!(mismatch.line, 1);
    assert_eq!(mismatch.expected_len, 9);
    assert_eq!(mismatch.actual_len, b"stdout:file\0input\xff".len());

    fs::write(cases_root.join("input.expected"), b"stdout:file\0input\xff").unwrap();
    fs::write(
        root.join("target/runner-fixture.json"),
        br#"{"mode":"console_io","mutate_source":false,"echo":true,"delay_after_input_millis":300}"#,
    )
    .unwrap();
    let marker = root.join("target/runner-input-read");
    let _ = fs::remove_file(&marker);
    session.start_test_case(case.clone()).unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < until {
        session.poll_command().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        marker.exists(),
        "test runner did not consume the pinned input"
    );
    fs::write(cases_root.join("input.expected"), b"replaced during run").unwrap();
    wait(&mut session);
    assert_eq!(
        session.take_test_case_result().unwrap().outcome,
        TestCaseOutcome::Pass,
        "comparison must use the pre-launch expected snapshot"
    );

    fs::write(
        root.join("target/runner-fixture.json"),
        br#"{"mode":"console_io","mutate_source":false,"echo":true,"exit":7}"#,
    )
    .unwrap();
    session.start_test_case(case.clone()).unwrap();
    wait(&mut session);
    assert!(matches!(
        session.take_test_case_result().unwrap().outcome,
        TestCaseOutcome::Error(_)
    ));

    session.quit().unwrap();
    let pinned = rustrace_workspace::hash::PinnedWorkspaceRoot::open(&root).unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .open_journal_file(&id)
        .unwrap();
    let mut journal =
        rustrace_journal::Journal::open_read_only_no_follow(owner.display_path()).unwrap();
    let events = journal.read_events(&id, 1, 1000).unwrap();
    let starts = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ControlledCommandStarted(start) => Some(start),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 5);
    let (typed, manual) = (starts[0], starts[1]);
    assert_eq!(typed.action, ControlledAction::Run);
    assert_eq!(typed.action, manual.action);
    assert_eq!(typed.argv, manual.argv);
    assert_eq!(typed.environment, manual.environment);
    assert_eq!(typed.selected_toolchain, manual.selected_toolchain);
    assert_eq!(typed.tools, manual.tools);
    assert_eq!(typed.deadline_millis, manual.deadline_millis);
    assert_eq!(typed.output_limit, manual.output_limit);
    assert_eq!(typed.console, manual.console);
    let route = typed.console.as_ref().unwrap();
    assert!(matches!(route.stdin, ConsoleStdinRoute::File { .. }));
    assert!(matches!(route.stdout, ConsoleStdoutRoute::Console));

    let finishes = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ControlledCommandFinished(finish) => Some(finish),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(finishes.len(), 5);
    assert!(
        finishes
            .iter()
            .all(|finish| finish.stdout.mode == CommandCaptureMode::Captured)
    );
    assert!(events.iter().any(|event| matches!(
        &event.event,
        Event::ControlledCommandOutput(output)
            if output.command_id == typed.command_id && output.stream == OutputStream::Stdout
    )));

    let comparisons = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::TestCaseCompared(comparison) => Some(comparison),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(comparisons.len(), 4);
    assert_eq!(comparisons[0].command_id, starts[0].command_id);
    assert_eq!(comparisons[0].case, "input");
    assert!(matches!(
        comparisons[0].outcome,
        TestCaseComparisonOutcome::Pass
    ));
    assert!(matches!(
        comparisons[1].outcome,
        TestCaseComparisonOutcome::Mismatch {
            line: 1,
            expected_len: 9,
            actual_len: 18
        }
    ));
    assert!(matches!(
        comparisons[2].outcome,
        TestCaseComparisonOutcome::Pass
    ));
    assert!(matches!(
        comparisons[3].outcome,
        TestCaseComparisonOutcome::Error {
            reason: TestCaseComparisonError::NonzeroExit
        }
    ));
    for comparison in comparisons {
        let at = events
            .iter()
            .position(|event| matches!(&event.event, Event::TestCaseCompared(candidate) if candidate == comparison))
            .unwrap();
        assert!(matches!(
            events.get(at - 1).map(|event| &event.event),
            Some(Event::ControlledCommandFinished(finish)) if finish.command_id == comparison.command_id
        ));
    }
}

fn run_console_child(root: &Path, exact_test: &str) -> std::process::Output {
    let bin = root.join("target/bin");
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", exact_test, "--nocapture"])
        .env("RUSTRACE_CONSOLE_FIXTURE", root)
        .env("PATH", path)
        .output()
        .unwrap()
}

fn install_tools(root: &Path) {
    let script = include_bytes!("support/command_rustup.py");
    for path in std::iter::once(root.join("target/bin/rustup")).chain(
        [
            "rustc",
            "cargo",
            "rustdoc",
            "rust-analyzer",
            "cargo-clippy",
            "cargo-fmt",
            "rustfmt",
        ]
        .map(|tool| root.join("target/bin/v1").join(tool)),
    ) {
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn production_console_test_options_child() {
    let Some(root) = std::env::var_os("RUSTRACE_CONSOLE_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    let launcher = root.join("target/bin/rustup");
    let root = fs::canonicalize(root).unwrap();
    let mut session = ProductionSession::start(&root, MANIFEST).unwrap();
    let id = session.session_id().clone();

    assert_eq!(
        session
            .start_console_command("cargo test tests::legal_moves -- --show-output")
            .unwrap(),
        ConsoleStart::Started
    );
    assert!(
        !session.console_accepts_stdin(),
        "console Test stdin must be closed"
    );
    wait(&mut session);
    assert!(!session.command_active());
    let outcome = session.command_outcome().cloned().unwrap();
    assert!(
        matches!(outcome, CommandOutcome::Exited { .. }),
        "fake Cargo must reach process completion: {outcome:?}"
    );
    assert!(
        !session.console_output().is_empty(),
        "completed fake Cargo must expose captured output"
    );
    session.quit().unwrap();

    let pinned = rustrace_workspace::hash::PinnedWorkspaceRoot::open(&root).unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .open_journal_file(&id)
        .unwrap();
    let mut journal =
        rustrace_journal::Journal::open_read_only_no_follow(owner.display_path()).unwrap();
    let events = journal.read_events(&id, 1, 1000).unwrap();
    let start = events
        .iter()
        .find_map(|event| match &event.event {
            Event::ControlledCommandStarted(start) => Some(start),
            _ => None,
        })
        .expect("recorded console Test start");
    assert_eq!(start.action, ControlledAction::Test);
    assert_eq!(
        start.argv,
        [
            launcher.to_string_lossy().into_owned(),
            "run".to_owned(),
            "fixture".to_owned(),
            root.join("target/bin/v1/cargo")
                .to_string_lossy()
                .into_owned(),
            "test".to_owned(),
            "--locked".to_owned(),
            "tests::legal_moves".to_owned(),
            "--".to_owned(),
            "--show-output".to_owned(),
        ]
    );
    let route = start.console.as_ref().expect("console route");
    assert_eq!(route.stdin, ConsoleStdinRoute::Closed);
    assert_eq!(route.stdout, ConsoleStdoutRoute::Console);

    assert!(events.iter().any(|event| matches!(
        &event.event,
        Event::ControlledCommandOutput(output)
            if output.command_id == start.command_id && !output.bytes_hex.is_empty()
    )));
    let finish = events
        .iter()
        .find_map(|event| match &event.event {
            Event::ControlledCommandFinished(finish) if finish.command_id == start.command_id => {
                Some(finish)
            }
            _ => None,
        })
        .expect("recorded console Test finish");
    assert_eq!(finish.outcome, outcome);
    assert_eq!(finish.stdout.mode, CommandCaptureMode::Captured);
    assert_eq!(finish.stderr.mode, CommandCaptureMode::Captured);
}

#[test]
fn production_console_session_child() {
    let Some(root) = std::env::var_os("RUSTRACE_CONSOLE_FIXTURE") else {
        return;
    };
    let root = fs::canonicalize(PathBuf::from(root)).unwrap();
    let cases = root.parent().unwrap().join("test-cases");
    let mut session = ProductionSession::start(&root, MANIFEST).unwrap();
    let id = session.session_id().clone();

    assert!(session.start_console_command("cargo run | sh").is_err());
    assert!(!root.join("target/invocation.json").exists());
    assert!(
        session
            .start_console_command("cargo run < missing > output.bin")
            .is_err()
    );
    assert_eq!(fs::read(cases.join("output.bin")).unwrap(), b"preserve");

    let request = session
        .start_console_command("cargo run < input.bin > output.bin")
        .unwrap();
    assert_eq!(
        request,
        ConsoleStart::OverwriteConfirmation {
            path: rustrace_model::WorkspacePath::new("output.bin").unwrap()
        }
    );
    assert!(session.console_overwrite_pending());
    assert!(session.cancel_console_overwrite());
    assert_eq!(fs::read(cases.join("output.bin")).unwrap(), b"preserve");
    assert!(!session.command_active());

    assert!(matches!(
        session
            .start_console_command("cargo run < input.bin > output.bin")
            .unwrap(),
        ConsoleStart::OverwriteConfirmation { .. }
    ));
    session.confirm_console_overwrite().unwrap();
    assert!(!session.console_accepts_stdin());
    assert!(session.execute(EditorCommand::Insert('X')).is_err());
    wait(&mut session);
    assert_eq!(
        session.command_outcome(),
        Some(&CommandOutcome::Exited { code: 0 })
    );
    assert_eq!(
        fs::read(cases.join("output.bin")).unwrap(),
        b"stdout:file\0input\xff"
    );
    assert_eq!(session.console_output(), b"console-stderr");
    assert_eq!(fs::read(root.join("main.rs")).unwrap(), b"A");
    assert!(session.command_diagnostics().is_none());

    session.start_language_service().unwrap();
    wait_for_lsp(&mut session, "LSP ready");
    let first_lsp_pid = lsp_pids(&root)[0];

    assert!(matches!(
        session
            .start_console_command("cargo run > race-output.bin")
            .unwrap(),
        ConsoleStart::OverwriteConfirmation { .. }
    ));
    fs::remove_file(cases.join("race-output.bin")).unwrap();
    fs::create_dir(cases.join("race-output.bin")).unwrap();
    session.confirm_console_overwrite().unwrap();
    let setup_error = wait_for_command_error(&mut session);
    assert!(setup_error.contains("ordinary") || setup_error.contains("directory"));
    assert!(cases.join("race-output.bin").is_dir());
    assert!(!session.command_active());
    wait_for_lsp(&mut session, "LSP ready");
    fs::remove_dir(cases.join("race-output.bin")).unwrap();

    fs::write(
        root.join("target/runner-fixture.json"),
        br#"{"mode":"console_io","mutate_source":true,"echo":false}"#,
    )
    .unwrap();
    assert_eq!(
        session.start_console_command("cargo run").unwrap(),
        ConsoleStart::Started
    );
    assert_eq!(session.language_service_status(), "LSP stopped");
    assert_ne!(unsafe { libc::kill(first_lsp_pid, 0) }, 0);
    let until = Instant::now() + Duration::from_secs(10);
    while session.command_active() && !session.console_accepts_stdin() && Instant::now() < until {
        session.poll_command().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(session.console_accepts_stdin());
    assert!(session.submit_console_line("stdin-secret 🦀").unwrap());
    assert!(session.submit_console_line("").unwrap());
    assert!(session.close_console_stdin().unwrap());
    wait(&mut session);
    assert_eq!(
        session.command_outcome(),
        Some(&CommandOutcome::Exited { code: 0 })
    );
    assert!(session.console_output().starts_with(b"console-complete"));
    assert!(session.console_output().ends_with(b"console-stderr"));
    assert_eq!(fs::read(root.join("main.rs")).unwrap(), b"A");
    wait_for_lsp(&mut session, "LSP ready");
    assert!(lsp_pids(&root).len() >= 2);

    assert_eq!(
        session.start_console_command("cargo doc").unwrap(),
        ConsoleStart::Started
    );
    wait(&mut session);
    assert_eq!(
        session.command_outcome(),
        Some(&CommandOutcome::Exited { code: 0 })
    );
    let invocation: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("target/invocation.json")).unwrap()).unwrap();
    assert_eq!(invocation["argv"], serde_json::json!(["doc", "--locked"]));
    assert!(session.command_status().contains("target/doc"));

    fs::write(
        root.join("target/runner-fixture.json"),
        br#"{"mode":"console_exit_early"}"#,
    )
    .unwrap();
    assert_eq!(
        session.start_console_command("cargo run").unwrap(),
        ConsoleStart::Started
    );
    let until = Instant::now() + Duration::from_secs(15);
    while !session.console_accepts_stdin() && Instant::now() < until {
        session.poll_command().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(session.console_accepts_stdin());
    let until = Instant::now() + Duration::from_secs(15);
    while Instant::now() < until {
        if session.poll_command().unwrap()
            && session.console_output() == b"console-exited-before-stdin"
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(session.command_active());
    assert!(
        !session.console_accepts_stdin(),
        "collected child result retained stale stdin-present state"
    );
    wait(&mut session);
    session.quit().unwrap();

    let pinned = rustrace_workspace::hash::PinnedWorkspaceRoot::open(&root).unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .open_journal_file(&id)
        .unwrap();
    let mut journal =
        rustrace_journal::Journal::open_read_only_no_follow(owner.display_path()).unwrap();
    let events = journal.read_events(&id, 1, 1000).unwrap();
    let starts = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ControlledCommandStarted(start) => Some(start),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 4);
    assert!(starts.iter().all(|start| start.console.is_some()));
    assert!(
        starts
            .iter()
            .filter(|start| start.action == ControlledAction::Run)
            .all(|start| {
                start
                    .argv
                    .last()
                    .is_some_and(|argument| argument == "--locked")
                    && !start
                        .argv
                        .iter()
                        .any(|argument| argument == "--message-format=json")
            })
    );
    assert!(
        starts[2]
            .argv
            .get(5..)
            .is_some_and(|args| args == ["--locked"])
    );
    assert_eq!(starts[2].action, ControlledAction::Doc);
    assert!(matches!(
        starts[0].console.as_ref().unwrap().stdin,
        ConsoleStdinRoute::File { .. }
    ));
    assert!(matches!(
        starts[0].console.as_ref().unwrap().stdout,
        ConsoleStdoutRoute::File { .. }
    ));
    assert!(matches!(
        starts[1].console.as_ref().unwrap().stdin,
        ConsoleStdinRoute::Submitted
    ));
    assert!(matches!(
        starts[3].console.as_ref().unwrap().stdin,
        ConsoleStdinRoute::Submitted
    ));
    assert!(!events.iter().any(|event| match &event.event {
        Event::ControlledCommandOutput(output) => {
            output.command_id == starts[0].command_id && output.stream == OutputStream::Stdout
        }
        _ => false,
    }));
    let first_finish = events
        .iter()
        .find_map(|event| match &event.event {
            Event::ControlledCommandFinished(finish)
                if finish.command_id == starts[0].command_id =>
            {
                Some(finish)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(first_finish.stdout.mode, CommandCaptureMode::Redirected);
    assert_eq!(
        first_finish.stdout.completeness,
        CaptureCompleteness::Unavailable
    );
    let encoded = events
        .iter()
        .flat_map(|event| rustrace_model::encode_envelope(event).unwrap())
        .collect::<Vec<_>>();
    assert!(!String::from_utf8_lossy(&encoded).contains("stdin-secret"));
}

#[test]
fn production_console_recording_failure_child() {
    let Some(root) = std::env::var_os("RUSTRACE_CONSOLE_FIXTURE") else {
        return;
    };
    let root = fs::canonicalize(PathBuf::from(root)).unwrap();
    let mut session = ProductionSession::start(&root, MANIFEST).unwrap();
    let id = session.session_id().clone();
    assert_eq!(
        session.start_console_command("cargo run").unwrap(),
        ConsoleStart::Started
    );
    let until = Instant::now() + Duration::from_secs(15);
    while !session.console_accepts_stdin() && Instant::now() < until {
        session.poll_command().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(session.console_accepts_stdin());

    let journal_path = root
        .join(".rustrace")
        .join(format!("{}.sqlite", id.as_str()));
    let connection = rusqlite::Connection::open(journal_path).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_console_recording BEFORE INSERT ON events
             BEGIN SELECT RAISE(ABORT, 'injected console append failure'); END;",
        )
        .unwrap();

    let private = "PRIVATE_FAILED_CONSOLE_STDIN";
    assert!(session.submit_console_line(private).unwrap());
    assert!(session.close_console_stdin().unwrap());
    let error = wait_for_command_error(&mut session);
    assert!(!error.contains(private));
    assert!(session.command_active());
    assert!(session.recovery_reason().is_some());
    assert_eq!(fs::read(root.join("main.rs")).unwrap(), b"A");
    assert!(session.execute(EditorCommand::Insert('X')).is_err());
    assert!(session.start_console_command("cargo check").is_err());
    assert!(session.unpublished_command_capture().is_none());
    assert!(session.console_output().starts_with(b"console-complete"));
    assert!(session.console_output().ends_with(b"console-stderr"));
    let activity: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".rustrace/command-activity.json")).unwrap())
            .unwrap();
    assert_eq!(activity["active"], true);
    for artifact in fs::read_dir(root.join(".rustrace")).unwrap() {
        let artifact = artifact.unwrap().path();
        if artifact.is_file() && artifact.metadata().unwrap().len() <= 34 * 1024 * 1024 {
            assert!(
                !fs::read(&artifact)
                    .unwrap()
                    .windows(private.len())
                    .any(|bytes| { bytes == private.as_bytes() })
            );
        }
    }

    drop(session);
    connection
        .execute_batch("DROP TRIGGER fail_console_recording")
        .unwrap();
}

fn wait(session: &mut ProductionSession) {
    let until = Instant::now() + Duration::from_secs(15);
    while session.command_active() && Instant::now() < until {
        session.poll_command().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        !session.command_active(),
        "console fixture exceeded hang guard"
    );
}

fn wait_for_lsp(session: &mut ProductionSession, expected: &str) {
    let until = Instant::now() + Duration::from_secs(15);
    while session.language_service_status() != expected && Instant::now() < until {
        session.tick().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(session.language_service_status(), expected);
}

fn wait_for_command_error(session: &mut ProductionSession) -> String {
    let until = Instant::now() + Duration::from_secs(15);
    while session.command_active() && Instant::now() < until {
        match session.poll_command() {
            Ok(_) => std::thread::sleep(Duration::from_millis(2)),
            Err(error) => return error.to_string(),
        }
    }
    panic!("console setup did not report its expected bounded failure");
}

fn lsp_pids(root: &Path) -> Vec<i32> {
    fs::read_to_string(root.join("target/lsp-launches.jsonl"))
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["pid"]
                .as_i64()
                .unwrap() as i32
        })
        .collect()
}
