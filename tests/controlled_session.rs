//! Production API tests precede integration. The first compile failure is an
//! API Red, not evidence that behavioral assertions have executed.
#![cfg(unix)]
use ratatui::{Terminal, backend::TestBackend};
use rustrace::{
    cargo_policy::CargoAction,
    diagnostics::{DiagnosticNavigation, DiagnosticOutcome},
    session::ProductionSession,
    tui::{
        BufferTabViewEntry, EditorCommand, JournalHealth, MainView, MainViewState, RecordingState,
    },
};
use rustrace_model::{CommandOutcome, Event};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "runner"
assignment_version = "v1"
title = "Runner"
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

#[path = "support/command_mix.rs"]
mod command_mix;

#[test]
fn production_command_fixture_parent() {
    run_cases(&[
        "bytes",
        "source",
        "lock",
        "unsafe",
        "out_of_policy",
        "oversize",
        "preparing_drop",
        "generation",
        "missing_clippy",
        "mandatory_failure",
        "boundary_failure",
        "metadata_failure",
        "probe_invalid",
        "version_oversize",
        "capture_collision",
        "diagnostic",
        "diagnostic_filter",
        "warning_success",
        "combined_diagnostics_format",
        "interrupt_command-preparation",
        "interrupt_command-prestart",
        "interrupt_command-start",
        "interrupt_command-capture",
        "interrupt_command-postcheckpoint",
        "interrupt_command-finish",
    ]);
}

#[test]
#[ignore = "deterministic mixed/maximum workload; run explicitly as documented"]
fn controlled_mixed_maximum_workloads() {
    run_cases(&["mixed_schedule", "maximum_commands"]);
}

fn run_cases(modes: &[&str]) {
    for &mode in modes {
        if mode.starts_with("interrupt_") && !cfg!(feature = "process-probes") {
            continue;
        }
        if std::env::var("RUSTRACE_CONTROLLED_CASE").is_ok_and(|selected| selected != mode) {
            continue;
        }
        let root = std::env::temp_dir().join(format!(
            "rustrace-controlled-session-{}-{mode}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::create_dir(root.join("target/bin")).unwrap();
        let bin = root.join("target/bin");
        let script = include_bytes!("support/command_rustup.py");
        fs::write(bin.join("rustup"), script).unwrap();
        fs::set_permissions(bin.join("rustup"), fs::Permissions::from_mode(0o755)).unwrap();
        for generation in ["v1", "v2"] {
            fs::create_dir(bin.join(generation)).unwrap();
            for tool in [
                "rustc",
                "cargo",
                "rustdoc",
                "cargo-clippy",
                "cargo-fmt",
                "rustfmt",
            ] {
                let path = bin.join(generation).join(tool);
                fs::write(&path, script).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        if mode == "maximum_commands" {
            for (path, bytes) in rustrace::session_fixture::maximum_workspace(54) {
                fs::write(root.join(path.as_str()), bytes).unwrap();
            }
        } else {
            fs::write(
                root.join("main.rs"),
                if mode == "diagnostic" {
                    "é🦀X\n".as_bytes()
                } else if mode == "combined_diagnostics_format" {
                    "é🦀X\n// formatter fixture\n".as_bytes()
                } else {
                    b"A"
                },
            )
            .unwrap();
            if mode == "mixed_schedule" {
                fs::write(root.join("reuse.rs"), "// café 東京\n").unwrap();
                // Keep the established main.rs -> reuse.rs clipboard order.
                fs::write(root.join("zz-format.rs"), "// formatter fixture\n").unwrap();
            }
            fs::write(root.join("Cargo.lock"), b"lock fixture").unwrap();
        }
        let config = match mode {
            "missing_clippy" => serde_json::json!({"missing":["cargo-clippy"],"exit":7}),
            "mandatory_failure" => serde_json::json!({"manager":"1.27.0"}),
            "diagnostic"
            | "diagnostic_filter"
            | "warning_success"
            | "combined_diagnostics_format" => {
                let runner_mode = if mode == "diagnostic_filter" {
                    "diagnostic_filter"
                } else if mode == "warning_success" {
                    "warning_success"
                } else {
                    "diagnostic"
                };
                serde_json::json!({
                    "mode": runner_mode,
                    "exit": if mode == "warning_success" { 0 } else { 101 }
                })
            }
            _ => serde_json::json!({"mode":mode,"exit":7}),
        };
        fs::write(
            root.join("target/runner-fixture.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let path = std::env::join_paths(
            std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "production_command_fixture_child", "--nocapture"])
            .env("RUSTRACE_CONTROLLED_FIXTURE", &root)
            .env("RUSTRACE_CONTROLLED_MODE", mode)
            .env("PATH", path)
            .env("UNRELATED_SECRET", "fixture-only-secret")
            .env("RUSTFLAGS", "fixture-only-flags");
        if ["mixed_schedule", "maximum_commands"].contains(&mode) {
            // libtest's worker stack is smaller than the production CLI main
            // thread. The maximum fixture retains the full 10 MiB tree while
            // checking snapshots, so give only this opt-in child the normal
            // production-sized stack without changing limits or assertions.
            child.env("RUST_MIN_STACK", "8388608");
        }
        if let Some(stage) = mode.strip_prefix("interrupt_") {
            child.env("RUSTRACE_INTERRUPT_AT", stage);
        }
        let output = child.output().unwrap();
        if let Some(stage) = mode.strip_prefix("interrupt_") {
            assert_eq!(
                output.status.code(),
                Some(83),
                "stage={stage}; fixture retained {}; stderr={}",
                root.display(),
                String::from_utf8_lossy(&output.stderr)
            );
            let marker: serde_json::Value = serde_json::from_slice(
                &fs::read(root.join(".rustrace/command-activity.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(marker["active"], true);
            let views = ProductionSession::inspect(&root).unwrap();
            assert_eq!(
                views.logical[&rustrace_model::WorkspacePath::new("main.rs").unwrap()],
                b"BA"
            );
            let error =
                ProductionSession::resume(&root, MANIFEST, rustrace::session::ResumeChoice::Resume)
                    .err()
                    .expect("interruption must preserve and block resume");
            assert!(error.to_string().contains("unfinished command"), "{error}");
            let after_launch = [
                "command-capture",
                "command-postcheckpoint",
                "command-finish",
            ]
            .contains(&stage);
            assert_eq!(root.join("target/invocation.json").exists(), after_launch);
            let captures = fs::read_dir(root.join(".rustrace"))
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .ends_with("-capture.json")
                })
                .count();
            assert_eq!(captures, usize::from(after_launch));
            println!("interruption case {stage} preserved and verified");
            fs::remove_dir_all(root).unwrap();
            continue;
        }
        assert!(
            output.status.success(),
            "mode={mode}; fixture retained at {}; stdout={}; stderr={}",
            root.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        println!("production case {mode} passed");
        if std::env::var_os("RUSTRACE_MEASURE_COMMAND_MIX").is_some() {
            println!("{}", String::from_utf8(output.stdout).unwrap());
            println!("fixture retained at {}", root.display());
        } else {
            fs::remove_dir_all(root).unwrap();
        }
    }
}

#[test]
fn production_command_fixture_child() {
    let Some(root) = std::env::var_os("RUSTRACE_CONTROLLED_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    let mode = std::env::var("RUSTRACE_CONTROLLED_MODE").unwrap();
    let opened = Instant::now();
    let mut session = ProductionSession::start(&root, MANIFEST).unwrap();
    let open_time = opened.elapsed();
    if mode == "maximum_commands" {
        let files = session.workspace().logical_files().unwrap();
        assert_eq!(
            files.len(),
            256,
            "command fixture must reach declared maximum"
        );
        assert_eq!(
            files.values().map(Vec::len).sum::<usize>(),
            10 * 1024 * 1024
        );
        assert_eq!(files.values().map(Vec::len).max(), Some(1024 * 1024));
    }
    if ["mixed_schedule", "maximum_commands"].contains(&mode.as_str()) {
        command_mix::run(&root, session, &mode, open_time);
        return;
    }
    let id = session.session_id().clone();
    session.execute(EditorCommand::Insert('B')).unwrap();
    session.set_command_modal(true);
    assert!(session.start_command(CargoAction::Check).is_err());
    session.set_command_modal(false);
    if [
        "missing_clippy",
        "mandatory_failure",
        "probe_invalid",
        "version_oversize",
    ]
    .contains(&mode.as_str())
    {
        session
            .start_command(if mode == "missing_clippy" {
                CargoAction::Clippy
            } else {
                CargoAction::Check
            })
            .unwrap();
        assert!(wait_command(&mut session).is_some());
        assert!(!session.command_active());
        assert!(session.recovery_reason().is_none());
        assert!(!root.join("target/invocation.json").exists());
        let observations = fs::read_dir(root.join(".rustrace"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("command-resolution-")
            })
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        assert_eq!(
            observations.len(),
            1,
            "failed current resolution must preserve its evidence"
        );
        if mode == "probe_invalid" {
            let observation: serde_json::Value =
                serde_json::from_slice(&fs::read(&observations[0]).unwrap()).unwrap();
            assert_eq!(observation["captures"][0]["stdout_hex"], "ff00");
        }
        if mode == "version_oversize" {
            let observation: serde_json::Value =
                serde_json::from_slice(&fs::read(&observations[0]).unwrap()).unwrap();
            let probe = observation["report"]["probes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|probe| probe["component"] == "rustdoc" && probe["purpose"] == "version")
                .unwrap();
            assert_eq!(probe["status"], "invalid_output");
            assert!(probe["stdout"].as_str().unwrap().len() > 4096);
            assert!(
                observation["captures"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|capture| {
                        capture["stdout_hex"].as_str().is_some_and(|hex| {
                            hex.starts_with("72757374646f6320") && hex.len() > 8192
                        })
                    })
            );
            let marker: serde_json::Value = serde_json::from_slice(
                &fs::read(root.join(".rustrace/command-activity.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(marker["active"], false);
            session.execute(EditorCommand::Insert('C')).unwrap();
            session.save_all().unwrap();
            assert_eq!(fs::read(root.join("main.rs")).unwrap(), b"BCA");
        }
        if mode != "missing_clippy" {
            session.quit().unwrap();
            return;
        }
    }
    session.start_command(CargoAction::Check).unwrap();
    if mode == "preparing_drop" {
        let marker: serde_json::Value = serde_json::from_slice(
            &fs::read(root.join(".rustrace/command-activity.json"))
                .expect("resolver must have durable activity marker before start returns"),
        )
        .unwrap();
        assert_eq!(marker["active"], true);
        drop(session);
        // Cancellation can still be in flight. Restart cannot infer cleanup:
        // either the resolver still shares the writer lock, or the marker's
        // owner (this process) is alive.
        let error =
            ProductionSession::resume(&root, MANIFEST, rustrace::session::ResumeChoice::Resume)
                .err()
                .expect("unfinished preparation must block normal resume")
                .to_string();
        assert!(
            error.contains("unfinished command")
                || error.contains("a program a command started before Rustrace was killed"),
            "{error}"
        );
        return;
    }
    assert!(session.command_active());
    assert!(session.start_command(CargoAction::Run).is_err());
    assert!(session.execute(EditorCommand::Insert('X')).is_err());
    assert!(session.create_file("new.rs").is_err());
    assert!(session.save_all().is_err());
    let failure = wait_command(&mut session);
    if mode == "warning_success" {
        assert!(failure.is_none(), "{failure:?}");
        let diagnostics = session.command_diagnostics().expect("derived diagnostics");
        assert_eq!(diagnostics.outcome, DiagnosticOutcome::Success);
        assert_eq!(diagnostics.diagnostics.len(), 1);
        assert_eq!(diagnostics.diagnostics[0].level, "warning");
        assert_eq!(
            session.command_error_status(),
            None,
            "a successful warning-only Check must not raise the ERROR pill"
        );
        session.quit().unwrap();
        return;
    }
    if mode == "diagnostic_filter" {
        assert!(failure.is_none(), "{failure:?}");
        let diagnostics = session.command_diagnostics().expect("derived diagnostics");
        assert_eq!(diagnostics.outcome, DiagnosticOutcome::CompilerErrors);
        assert_eq!(diagnostics.diagnostics.len(), 7);

        let capture_path = fs::read_dir(root.join(".rustrace"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().ends_with("-capture.json"))
            })
            .expect("command capture evidence");
        let evidence = String::from_utf8(fs::read(capture_path).unwrap()).unwrap();
        for retained in [
            "could not compile `student`",
            "For more information about this error",
            "aborting due to 1 previous error",
            "Some errors have detailed explanations",
            "spanned error child note stays in evidence",
        ] {
            assert!(
                evidence.contains(retained),
                "recorded evidence lost {retained:?}: {evidence}"
            );
        }

        let rows = session.diagnostic_display_rows(8);
        let error = session
            .command_error_status()
            .expect("failed Check has an ERROR-pill status");
        assert_eq!(error, "1 error · 1 warning");
        let state = MainViewState::new(
            "Diagnostic display filter",
            vec![BufferTabViewEntry::new("main.rs", true, false)],
            vec![],
            RecordingState::Active,
            JournalHealth::Healthy,
            "saved",
        )
        .with_output_rows(rows)
        .with_error_condition(error);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    MainView::new(
                        &state,
                        session.workspace().active_buffer(),
                        session.workspace().active_viewport(),
                        &[],
                    ),
                    frame.area(),
                );
            })
            .unwrap();
        let mut output = String::new();
        for y in 0..24 {
            for x in 0..80 {
                output.push_str(terminal.backend().buffer().cell((x, y)).unwrap().symbol());
            }
            output.push('\n');
        }
        for visible in [
            "real spanned error",
            "real spanned warning",
            "real spanned note",
            " ERROR ",
            "1 error · 1 warning",
        ] {
            assert!(output.contains(visible), "missing {visible:?}:\n{output}");
        }
        for hidden in [
            "failure-note",
            "could not compile",
            "For more information about this error",
            "aborting due to",
            "Some errors have detailed explanations",
        ] {
            assert!(!output.contains(hidden), "rendered {hidden:?}:\n{output}");
        }
        for (position, level) in [(1, "error"), (2, "warning"), (3, "note"), (1, "error")] {
            assert!(matches!(
                session.select_diagnostic(1).unwrap(),
                DiagnosticNavigation::Navigated { .. }
            ));
            let rows = session.diagnostic_display_rows(8);
            assert!(
                rows[0]
                    .text()
                    .contains(&format!("Diagnostics {position}/3")),
                "visible navigation counter drifted: {rows:?}"
            );
            assert!(
                rows.iter()
                    .any(|row| row.text().starts_with(&format!("> {level}"))),
                "visible navigation selected no {level} row: {rows:?}"
            );
        }
        session.quit().unwrap();
        return;
    }
    if matches!(mode.as_str(), "diagnostic" | "combined_diagnostics_format") {
        assert!(failure.is_none(), "{failure:?}");
        let diagnostics = session.command_diagnostics().expect("derived diagnostics");
        assert!(diagnostics.issues.is_empty(), "{:?}", diagnostics.issues);
        assert_eq!(diagnostics.outcome, DiagnosticOutcome::CompilerErrors);
        assert_eq!(diagnostics.diagnostics.len(), 5);
        assert_eq!(diagnostics.identity.argv[5], "--message-format=json");
        let rows = session.diagnostic_display_rows(4);
        assert_eq!(rows.len(), 4);
        assert!(
            rows[0].text().contains("compiler errors; complete"),
            "{rows:?}"
        );
        assert!(
            rows[1].text().contains("valid Unicode byte span"),
            "{rows:?}"
        );
        let markers = session.active_diagnostic_markers();
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].line, 0);
        assert!(!markers[0].selected);

        assert_eq!(
            session.select_diagnostic(1).unwrap(),
            DiagnosticNavigation::Navigated {
                path: rustrace_model::WorkspacePath::new("main.rs").unwrap(),
                start_byte: 3,
                end_byte: 7,
            }
        );
        assert_eq!(
            session.workspace().active_buffer().selection_state(),
            rustrace_model::SelectionState::new(3, 7)
        );
        assert!(session.active_diagnostic_markers()[0].selected);
        assert_eq!(
            session.select_diagnostic_index(3).unwrap(),
            DiagnosticNavigation::InvalidCoordinates
        );
        assert_eq!(
            session.select_diagnostic_index(0).unwrap(),
            DiagnosticNavigation::Navigated {
                path: rustrace_model::WorkspacePath::new("main.rs").unwrap(),
                start_byte: 3,
                end_byte: 7,
            }
        );
        assert_eq!(
            session.select_diagnostic(1).unwrap(),
            DiagnosticNavigation::InvalidCoordinates
        );
        let rows = session.diagnostic_display_rows(3);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter()
                .skip(1)
                .map(|row| row.diagnostic_index())
                .collect::<Vec<_>>(),
            vec![Some(2), Some(3)],
            "the scrolled diagnostic window lost source indices: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.text().contains("> error[E0308]")
                && row.text().contains("invalid coordinates")),
            "selected fourth diagnostic was paged out: {rows:?}"
        );
        assert_eq!(
            session.select_diagnostic(1).unwrap(),
            DiagnosticNavigation::UnmanagedTarget
        );
        assert_eq!(
            session.select_diagnostic(1).unwrap(),
            DiagnosticNavigation::OutsideWorkspace
        );
        assert_eq!(
            session.select_diagnostic(1).unwrap(),
            DiagnosticNavigation::MissingTarget
        );

        if mode == "combined_diagnostics_format" {
            let prior_identity = session
                .command_diagnostics()
                .expect("diagnostics before Format")
                .identity
                .workspace
                .clone();
            fs::write(
                root.join("target/runner-fixture.json"),
                br#"{"mode":"format_then_diagnostic","exit":0}"#,
            )
            .unwrap();
            session.start_command(CargoAction::Format).unwrap();
            assert!(wait_command(&mut session).is_none());
            assert_eq!(
                fs::read(root.join("main.rs")).unwrap(),
                "Bé🦀X\n// Formatter fixture\n".as_bytes()
            );
            assert_eq!(
                session
                    .command_diagnostics()
                    .expect("Format must retain prior diagnostics as stale")
                    .identity
                    .workspace,
                prior_identity
            );
            assert!(
                session.diagnostic_display_rows(4)[0]
                    .text()
                    .contains("stale for current workspace")
            );
            assert_eq!(
                session.select_diagnostic(1).unwrap(),
                DiagnosticNavigation::NoDiagnostics
            );
            assert_eq!(
                session.select_diagnostic_index(0).unwrap(),
                DiagnosticNavigation::Stale
            );

            fs::write(
                root.join("target/runner-fixture.json"),
                br#"{"mode":"diagnostic","exit":101}"#,
            )
            .unwrap();
            session.start_command(CargoAction::Check).unwrap();
            assert!(wait_command(&mut session).is_none());
            let refreshed = session
                .command_diagnostics()
                .expect("subsequent Cargo diagnostics");
            assert_eq!(refreshed.outcome, DiagnosticOutcome::CompilerErrors);
            assert_ne!(refreshed.identity.workspace, prior_identity);
            assert!(
                !session.diagnostic_display_rows(4)[0]
                    .text()
                    .contains("stale for current workspace")
            );
            assert!(matches!(
                session.select_diagnostic(1).unwrap(),
                DiagnosticNavigation::Navigated { .. }
            ));
            session.quit().unwrap();
            return;
        }

        session.execute(EditorCommand::Insert('Z')).unwrap();
        session.save_all().unwrap();
        assert!(
            session.diagnostic_display_rows(4)[0]
                .text()
                .contains("stale for current workspace")
        );
        assert_eq!(
            session.select_diagnostic(1).unwrap(),
            DiagnosticNavigation::NoDiagnostics
        );
        assert_eq!(
            session.select_diagnostic_index(0).unwrap(),
            DiagnosticNavigation::Stale
        );
        session.quit().unwrap();
        return;
    }
    if mode == "capture_collision" {
        assert!(failure.is_some());
        assert!(session.command_active());
        assert_eq!(
            fs::read(root.join(".rustrace/command-4-capture.json")).unwrap(),
            b"existing unowned artifact"
        );
        let captures = fs::read_dir(root.join(".rustrace"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("command-capture-recovery-")
            })
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        assert_eq!(
            captures.len(),
            1,
            "capture publication failure must preserve original output separately"
        );
        let capture: serde_json::Value =
            serde_json::from_slice(&fs::read(&captures[0]).unwrap()).unwrap();
        assert_eq!(capture["execution"]["stdout"]["bytes"], 24);
        session.quit().unwrap();
        return;
    }
    if [
        "unsafe",
        "out_of_policy",
        "oversize",
        "boundary_failure",
        "metadata_failure",
    ]
    .contains(&mode.as_str())
    {
        assert!(failure.is_some());
        assert!(session.command_active());
        assert!(session.execute(EditorCommand::Insert('X')).is_err());
        assert!(session.recovery_reason().is_some());
        let captures = fs::read_dir(root.join(".rustrace"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.to_string_lossy().ends_with("-capture.json"))
            .collect::<Vec<_>>();
        assert_eq!(
            captures.len(),
            1,
            "original capture must survive a post-boundary failure"
        );
        let capture: serde_json::Value =
            serde_json::from_slice(&fs::read(&captures[0]).unwrap()).unwrap();
        assert_eq!(capture["execution"]["stdout"]["bytes"], 24);
        session.quit().unwrap();
        return;
    }
    assert!(failure.is_none(), "{failure:?}");
    assert!(!session.command_active(), "fixture did not finish");
    assert_eq!(
        session.command_outcome(),
        Some(&CommandOutcome::Exited { code: 7 })
    );
    assert_eq!(fs::read(root.join("main.rs")).unwrap(), b"BA");
    assert_eq!(fs::read(root.join("Cargo.lock")).unwrap(), b"lock fixture");
    let invocation: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("target/invocation.json")).unwrap()).unwrap();
    assert_eq!(
        invocation["argv"],
        serde_json::json!(["check", "--message-format=json", "--locked"])
    );
    assert!(
        invocation["program"]
            .as_str()
            .unwrap()
            .ends_with("v1/cargo")
    );
    if mode == "generation" {
        fs::write(
            root.join("target/runner-fixture.json"),
            br#"{"generation":"v2","exit":7}"#,
        )
        .unwrap();
        session.start_command(CargoAction::Test).unwrap();
        assert!(wait_command(&mut session).is_none());
        let invocation: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("target/invocation.json")).unwrap())
                .unwrap();
        assert_eq!(
            invocation["argv"],
            serde_json::json!(["test", "--message-format=json", "--locked"])
        );
        assert!(
            invocation["program"]
                .as_str()
                .unwrap()
                .ends_with("v2/cargo")
        );
    }
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
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.event, Event::ControlledCommandStarted(_)))
            .count(),
        if mode == "generation" { 2 } else { 1 }
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.event, Event::ControlledCommandFinished(_)))
            .count(),
        if mode == "generation" { 2 } else { 1 }
    );
    for envelope in &events {
        if let Event::ControlledCommandOutput(output) = &envelope.event {
            let target = if output.stream == rustrace_model::OutputStream::Stdout {
                &mut stdout
            } else {
                &mut stderr
            };
            target.extend(output.original_bytes().unwrap());
        }
    }
    let repetitions = if mode == "generation" { 2 } else { 1 };
    assert_eq!(
        stdout,
        b"stdout\xff\0\x1b]52;c;fixture\x07\n".repeat(repetitions)
    );
    assert_eq!(stderr, b"stderr\xfe\n".repeat(repetitions));
    if mode == "generation" {
        let starts = events
            .iter()
            .filter_map(|event| {
                if let Event::ControlledCommandStarted(start) = &event.event {
                    Some(start)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert!(
            starts[0]
                .tools
                .iter()
                .filter(|tool| tool.component != rustrace_model::CommandToolKind::Rustup)
                .all(|tool| tool.version.contains("fixture-v1"))
        );
        assert!(
            starts[1]
                .tools
                .iter()
                .filter(|tool| tool.component != rustrace_model::CommandToolKind::Rustup)
                .all(|tool| tool.version.contains("fixture-v2"))
        );
    }
}

fn wait_command(session: &mut ProductionSession) -> Option<String> {
    let until = Instant::now() + Duration::from_secs(15);
    while session.command_active() && Instant::now() < until {
        if let Err(error) = session.poll_command() {
            return Some(error.to_string());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    session
        .command_active()
        .then(|| "functional fixture hang guard exceeded".into())
}
