//! T4.2 extension of the T3.6/T3.7 deterministic workloads. Nominal schedule
//! positions select inputs only; every production timestamp remains real.
use super::{MANIFEST, wait_command};
use rustrace::{
    cargo_policy::CargoAction,
    session::{ProductionSession, ResumeChoice, SessionBudgets},
    session_fixture::{FixtureAction, representative_hour},
    tui::EditorCommand,
};
use rustrace_editor::Movement;
use rustrace_model::{CaptureCompleteness, CommandOutcome, CommandTermination, EditOrigin, Event};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

const OUTPUT_LIMIT: u64 = 1024;
const SESSION_OUTPUT: u64 = 2 * OUTPUT_LIMIT + 4 * 32;

fn budgets() -> SessionBudgets {
    SessionBudgets {
        output_per_command: OUTPUT_LIMIT,
        output_per_session: SESSION_OUTPUT,
        ..SessionBudgets::default()
    }
}

fn restart(
    root: &Path,
    session: ProductionSession,
    times: &mut Vec<Duration>,
) -> ProductionSession {
    let expected = session.workspace().logical_files().unwrap();
    let before = session.persisted_millis();
    let clock = Instant::now();
    session.quit().unwrap();
    let mut session = ProductionSession::resume(root, MANIFEST, ResumeChoice::Resume).unwrap();
    assert_eq!(session.workspace().logical_files().unwrap(), expected);
    assert!(session.persisted_millis() >= before);
    session.set_budgets(budgets()).unwrap();
    times.push(clock.elapsed());
    session
}

fn external(root: &Path, session: &mut ProductionSession, index: usize) {
    let expected = session.workspace().logical_files().unwrap();
    let path = session.workspace().active_path();
    let version = session.workspace().active_buffer().version();
    if index.is_multiple_of(3) {
        fs::remove_file(root.join(path.as_str())).unwrap();
    } else {
        fs::write(root.join(path.as_str()), b"rejected external replacement").unwrap();
    }
    assert!(session.recheck_external().unwrap());
    assert_eq!(session.workspace().logical_files().unwrap(), expected);
    assert_eq!(session.workspace().active_buffer().version(), version);
    assert!(!session.recheck_external().unwrap());
}

fn cycle(root: &Path, session: &mut ProductionSession, index: usize, times: &mut Vec<Duration>) {
    let (action, mode) = [
        (CargoAction::Check, "bytes"),
        (CargoAction::Test, "source"),
        (CargoAction::Run, "sleep"),
        (CargoAction::Check, "huge"),
        (CargoAction::Clippy, "bytes"),
        (CargoAction::Format, "format"),
        (CargoAction::Run, "huge"),
    ][index];
    let expected = session.workspace().logical_files().unwrap();
    let config = serde_json::json!({"mode":mode,"exit":7,
        "source_path":session.workspace().active_path().as_str(),
        "change_lock":root.join("Cargo.lock").exists()});
    fs::write(
        root.join("target/runner-fixture.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let invocation = root.join("target/invocation.json");
    if invocation.exists() {
        fs::remove_file(&invocation).unwrap();
    }
    let clock = Instant::now();
    session.start_command(action).unwrap();
    assert!(session.execute(EditorCommand::Insert('X')).is_err());
    if mode == "sleep" {
        let until = Instant::now() + Duration::from_secs(15);
        while !invocation.exists() && Instant::now() < until {
            session.poll_command().unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(invocation.exists(), "cancel must exercise a launched child");
        session.cancel_command();
    }
    assert!(wait_command(session).is_none());
    assert!(!session.command_active());
    assert!(session.recovery_reason().is_none());
    if mode == "format" {
        assert_eq!(
            session.command_outcome(),
            Some(&CommandOutcome::Exited { code: 0 })
        );
        let formatted = session.workspace().logical_files().unwrap();
        assert_ne!(
            formatted, expected,
            "Format must change its dedicated fixture"
        );
        assert_eq!(
            formatted.keys().collect::<Vec<_>>(),
            expected.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            formatted.values().map(Vec::len).sum::<usize>(),
            expected.values().map(Vec::len).sum::<usize>()
        );
        times.push(clock.elapsed());
        return;
    }
    let expected_outcome = match mode {
        "sleep" => CommandTermination::Cancelled,
        "huge" => CommandTermination::OutputLimit,
        _ => {
            assert_eq!(
                session.command_outcome(),
                Some(&CommandOutcome::Exited { code: 7 })
            );
            assert_eq!(session.workspace().logical_files().unwrap(), expected);
            times.push(clock.elapsed());
            return;
        }
    };
    assert!(
        matches!(session.command_outcome(), Some(CommandOutcome::Terminated { reason, .. }) if *reason == expected_outcome)
    );
    assert_eq!(session.workspace().logical_files().unwrap(), expected);
    times.push(clock.elapsed());
}

pub(super) fn run(root: &Path, mut session: ProductionSession, mode: &str, open_time: Duration) {
    session.set_budgets(budgets()).unwrap();
    let id = session.session_id().clone();
    let maximum = mode == "maximum_commands";
    let initial = session.workspace().logical_files().unwrap();
    let clock = Instant::now();
    let mut edits = Vec::new();
    let mut saves = Vec::new();
    let mut restarts = Vec::new();
    let mut commands = Vec::new();
    let mut reconciliations = Vec::new();
    let mut command_index = 0;
    let inputs;
    if maximum {
        // One bounded edit/save/command cycle per command scenario; the
        // existing session_probe retains its separate 200-pair maximum run.
        inputs = 14;
        for index in 0..7 {
            session
                .execute(EditorCommand::Move {
                    movement: Movement::DocumentStart,
                    selecting: false,
                })
                .unwrap();
            let start = Instant::now();
            session.execute(EditorCommand::DeleteForward).unwrap();
            session.execute(EditorCommand::Insert('/')).unwrap();
            edits.push(start.elapsed());
            {
                let start = Instant::now();
                session.save_all().unwrap();
                session.capture_boundary().unwrap();
                saves.push(start.elapsed());
            }
            {
                let start = Instant::now();
                external(root, &mut session, index);
                reconciliations.push(start.elapsed());
            }
            cycle(root, &mut session, command_index, &mut commands);
            command_index += 1;
            if command_index == 4 {
                session = restart(root, session, &mut restarts);
            }
        }
    } else {
        let schedule = representative_hour(54);
        inputs = schedule.len();
        assert_eq!(inputs, 3600);
        session
            .execute(EditorCommand::Move {
                movement: Movement::DocumentEnd,
                selecting: false,
            })
            .unwrap();
        for step in schedule {
            let start = Instant::now();
            match step.action {
                FixtureAction::Insert(c) => {
                    session.execute(EditorCommand::Insert(c)).unwrap();
                    edits.push(start.elapsed());
                }
                FixtureAction::Paste(text) => {
                    session.execute(EditorCommand::NextBuffer).unwrap();
                    assert_eq!(session.workspace().active_buffer().text(), text);
                    session.execute(EditorCommand::SelectAll).unwrap();
                    session.execute(EditorCommand::Copy).unwrap();
                    session.execute(EditorCommand::PreviousBuffer).unwrap();
                    session.execute(EditorCommand::Paste).unwrap();
                    edits.push(start.elapsed());
                }
                FixtureAction::Undo => {
                    session.execute(EditorCommand::Undo).unwrap();
                    edits.push(start.elapsed());
                }
                FixtureAction::Redo => {
                    session.execute(EditorCommand::Redo).unwrap();
                    edits.push(start.elapsed());
                }
                FixtureAction::Save => {
                    session.save_all().unwrap();
                    saves.push(start.elapsed());
                }
                FixtureAction::Checkpoint => {
                    session.capture_boundary().unwrap();
                    saves.push(start.elapsed());
                }
                FixtureAction::External => {
                    external(root, &mut session, (step.at_millis / 97_000) as usize);
                    reconciliations.push(start.elapsed());
                }
                FixtureAction::Restart => {
                    session = restart(root, session, &mut restarts);
                }
            }
            session.tick().unwrap();
            if step.at_millis.is_multiple_of(600_000) {
                cycle(root, &mut session, command_index, &mut commands);
                command_index += 1;
                if command_index == 4 {
                    session = restart(root, session, &mut restarts);
                }
            }
        }
        cycle(root, &mut session, command_index, &mut commands);
        command_index += 1;
    }
    assert_eq!(
        command_index, 7,
        "representative fixture must include all seven command/format cycles"
    );
    session.save_all().unwrap();
    session = restart(root, session, &mut restarts);
    let invocation = fs::read(root.join("target/invocation.json")).unwrap();
    let error = session.start_command(CargoAction::Check).unwrap_err();
    assert!(
        error.to_string().contains("output budget exhausted"),
        "{error}"
    );
    assert!(!session.command_active());
    assert!(session.recovery_reason().is_none());
    assert_eq!(
        fs::read(root.join("target/invocation.json")).unwrap(),
        invocation
    );
    let expected = session.workspace().logical_files().unwrap();
    if maximum {
        assert_eq!(expected.len(), 256);
        assert_eq!(
            expected.values().map(Vec::len).sum::<usize>(),
            10 * 1024 * 1024
        );
        assert_eq!(expected.values().map(Vec::len).max(), Some(1024 * 1024));
        assert_eq!(
            expected.keys().collect::<Vec<_>>(),
            initial.keys().collect::<Vec<_>>()
        );
    }
    let health = session.health().unwrap();
    assert!(health.events <= budgets().events);
    assert!(health.storage_bytes <= budgets().storage_bytes);
    let persisted = session.persisted_millis();
    session.quit().unwrap();
    let inspect_clock = Instant::now();
    let inspected = ProductionSession::inspect(root).unwrap();
    assert_eq!(inspected.logical, expected);
    assert_eq!(inspected.saved, expected);
    assert_eq!(inspected.disk, expected);
    let inspect_time = inspect_clock.elapsed();
    let pinned = rustrace_workspace::hash::PinnedWorkspaceRoot::open(root).unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .open_journal_file(&id)
        .unwrap();
    let mut journal =
        rustrace_journal::Journal::open_read_only_no_follow(owner.display_path()).unwrap();
    let mut sequence = 1;
    let mut starts = 0;
    let mut finishes = 0;
    let mut captured = 0;
    let mut limits = 0;
    let mut cancelled = 0;
    let mut formatted = 0;
    let mut format_edits = 0;
    let mut finished_bytes = 0;
    loop {
        let events = journal.read_events(&id, sequence, 256).unwrap();
        if events.is_empty() {
            break;
        }
        sequence += events.len() as u64;
        for event in events {
            match event.event {
                Event::ControlledCommandStarted(_) => starts += 1,
                Event::ControlledCommandOutput(chunk) => {
                    captured += chunk.original_bytes().unwrap().len() as u64
                }
                Event::ControlledCommandFinished(finish) => {
                    finishes += 1;
                    finished_bytes += finish.stdout.bytes + finish.stderr.bytes;
                    match finish.outcome {
                        CommandOutcome::Terminated {
                            reason: CommandTermination::OutputLimit,
                            ..
                        } => {
                            limits += 1;
                            assert_eq!(finish.stdout.bytes + finish.stderr.bytes, OUTPUT_LIMIT);
                            assert!(
                                finish.stdout.completeness == CaptureCompleteness::Truncated
                                    || finish.stderr.completeness == CaptureCompleteness::Truncated
                            );
                        }
                        CommandOutcome::Terminated {
                            reason: CommandTermination::Cancelled,
                            ..
                        } => cancelled += 1,
                        CommandOutcome::Exited { code: 7 } => {
                            assert_eq!(finish.stdout.bytes + finish.stderr.bytes, 32)
                        }
                        CommandOutcome::Exited { code: 0 } => {
                            formatted += 1;
                            assert_eq!(finish.stdout.bytes + finish.stderr.bytes, 32)
                        }
                        other => panic!("unexpected fixture outcome {other:?}"),
                    }
                }
                Event::FileEdited(transaction) if transaction.origin == EditOrigin::Formatter => {
                    format_edits += 1;
                    assert_eq!(transaction.version_after, transaction.version_before + 1);
                }
                _ => {}
            }
        }
    }
    assert_eq!(
        (starts, finishes, limits, cancelled, formatted),
        (7, 7, 2, 1, 1)
    );
    assert_eq!(format_edits, 1);
    assert_eq!(captured, SESSION_OUTPUT);
    assert_eq!(finished_bytes, captured);
    if std::env::var_os("RUSTRACE_MEASURE_COMMAND_MIX").is_some() {
        println!(
            "mode={mode} seed=54 inputs={inputs} commands={commands:?} files={} bytes={} max_file={} starts={starts} finishes={finishes} capture_bytes={captured} limit_outcomes={limits} cancelled={cancelled} formatted={formatted} formatter_edits={format_edits} events={} storage_bytes={} undo_bytes={} persisted_millis={persisted} run_ms={} open_ms={:.3} inspect_ms={:.3} final_exact_replay=true",
            expected.len(),
            expected.values().map(Vec::len).sum::<usize>(),
            expected.values().map(Vec::len).max().unwrap(),
            health.events,
            health.storage_bytes,
            health.undo_bytes,
            clock.elapsed().as_millis(),
            open_time.as_secs_f64() * 1000.0,
            inspect_time.as_secs_f64() * 1000.0
        );
        for (name, samples) in [
            ("edit", &mut edits),
            ("save_boundary", &mut saves),
            ("restart", &mut restarts),
            ("command", &mut commands),
            ("P2", &mut reconciliations),
        ] {
            samples.sort();
            if !samples.is_empty() {
                let rank = |p: usize| {
                    samples[(samples.len() * p).div_ceil(100).saturating_sub(1)].as_secs_f64()
                        * 1000.0
                };
                println!(
                    "{name} n={} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3}",
                    samples.len(),
                    rank(50),
                    rank(95),
                    rank(99)
                );
            }
        }
    }
}
