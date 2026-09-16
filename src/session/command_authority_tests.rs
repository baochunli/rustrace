//! Independent authority Red. Only the active-state bit is scaffolded; no
//! command guards or execution behavior are implemented in the Red commit.
use super::*;
use crate::tui::{TerminalOperations, TerminalSession};
use rustrace_journal::Journal;
use std::{
    io,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
    },
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "runner"
assignment_version = "v1"
title = "Runner"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> (Fixture, ProductionSession) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "rustrace-command-authority-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    fs::write(root.join("main.rs"), "A").unwrap();
    let session = ProductionSession::start(&root, MANIFEST).unwrap();
    (Fixture(root), session)
}

struct RestorationProbe(Arc<AtomicBool>);

impl TerminalOperations for RestorationProbe {
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn enter_alternate_screen(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn enable_bracketed_paste(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn enable_mouse_capture(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn disable_mouse_capture(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn disable_bracketed_paste(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        self.0.store(true, Ordering::Release);
        Ok(())
    }
}

#[cfg(unix)]
fn process_group_exists(pid: i32) -> bool {
    pid > 0
        && (unsafe { libc::kill(-pid, 0) } == 0
            || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

#[cfg(unix)]
fn process_exists(pid: i32) -> bool {
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

#[cfg(unix)]
fn read_fixture_pid(path: &Path) -> Option<i32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(unix)]
struct FullCaptureProcesses {
    owned_pid_path: PathBuf,
    escaped_pid_path: PathBuf,
    escaped_pid: Option<i32>,
}

#[cfg(unix)]
impl FullCaptureProcesses {
    fn wait_for_pid(path: &Path) -> Option<i32> {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(pid) = read_fixture_pid(path) {
                return Some(pid);
            }
            if Instant::now() >= until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn cleanup_escaped(&mut self) -> bool {
        let pid = self
            .escaped_pid
            .or_else(|| Self::wait_for_pid(&self.escaped_pid_path));
        self.escaped_pid = pid;
        let Some(pid) = pid else {
            return false;
        };
        let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
        let until_absent = Instant::now() + Duration::from_secs(3);
        while process_group_exists(pid) && Instant::now() < until_absent {
            let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
            std::thread::sleep(Duration::from_millis(5));
        }
        !process_group_exists(pid)
    }
}

#[cfg(unix)]
impl Drop for FullCaptureProcesses {
    fn drop(&mut self) {
        let _ = self.cleanup_escaped();
        if let Some(pid) = read_fixture_pid(&self.owned_pid_path) {
            let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        let _ = fs::remove_file(&self.owned_pid_path);
        let _ = fs::remove_file(&self.escaped_pid_path);
    }
}

#[cfg(unix)]
struct FailsafeProcessGroup {
    spawned: Arc<AtomicI32>,
}

#[cfg(unix)]
impl Drop for FailsafeProcessGroup {
    fn drop(&mut self) {
        let until_spawned = Instant::now() + Duration::from_secs(1);
        let pid = loop {
            let pid = self.spawned.load(Ordering::Acquire);
            if pid > 0 || Instant::now() >= until_spawned {
                break pid;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        if pid <= 0 {
            return;
        }
        let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
        let until_absent = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until_absent {
            let mut status = 0;
            let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if (waited == pid
                || (waited == -1
                    && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)))
                && !process_group_exists(pid)
            {
                break;
            }
            let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(unix)]
struct GroupRestorationProbe {
    restored: Arc<AtomicBool>,
    restored_while_group_live: Arc<AtomicBool>,
    spawned: Arc<AtomicI32>,
}

#[cfg(unix)]
impl TerminalOperations for GroupRestorationProbe {
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn enter_alternate_screen(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn enable_bracketed_paste(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn enable_mouse_capture(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn disable_mouse_capture(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn disable_bracketed_paste(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        let pid = self.spawned.load(Ordering::Acquire);
        if process_group_exists(pid) {
            self.restored_while_group_live
                .store(true, Ordering::Release);
        }
        self.restored.store(true, Ordering::Release);
        Ok(())
    }
}

#[cfg(unix)]
struct LifecycleRestorationProbe {
    restored: Arc<AtomicBool>,
    mouse_disabled: Arc<AtomicBool>,
    restored_before_command_finish: Arc<AtomicBool>,
    confirmation: Arc<AtomicBool>,
    activity_path: PathBuf,
}

#[cfg(unix)]
impl TerminalOperations for LifecycleRestorationProbe {
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn enter_alternate_screen(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn enable_bracketed_paste(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn enable_mouse_capture(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn disable_mouse_capture(&mut self) -> io::Result<()> {
        self.mouse_disabled.store(true, Ordering::Release);
        Ok(())
    }

    fn disable_bracketed_paste(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        assert!(
            self.mouse_disabled.load(Ordering::Acquire),
            "mouse capture must be disabled before raw mode"
        );
        let inactive = fs::read(&self.activity_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|value| value["active"] == false);
        if !self.confirmation.load(Ordering::Acquire) || !inactive {
            self.restored_before_command_finish
                .store(true, Ordering::Release);
        }
        self.restored.store(true, Ordering::Release);
        Ok(())
    }
}

#[test]
fn command_authority_blocks_direct_editor_noops_and_clipboard_without_poison() {
    let (_fixture, mut session) = fixture();
    session.effects.0.borrow_mut().command_active = true;
    let sequence = session.effects.0.borrow().sequence;
    for command in [
        EditorCommand::Insert('x'),
        EditorCommand::Undo,
        EditorCommand::Redo,
        EditorCommand::Copy,
        EditorCommand::Cut,
        EditorCommand::Paste,
        EditorCommand::PasteExternal(String::new()),
        EditorCommand::Search(String::new()),
    ] {
        assert!(
            session.execute(command.clone()).is_err(),
            "active command accepted {command:?}"
        );
        assert!(
            session
                .workspace_mut()
                .execute_editor(command.clone())
                .is_err(),
            "workspace gateway accepted {command:?}"
        );
    }
    assert!(
        session
            .reject_paste(
                PasteInputChannel::TerminalBracketed,
                PasteRejectionReason::ExternalInput,
            )
            .is_err(),
        "active command accepted a terminal paste rejection event"
    );
    assert_eq!(
        session.effects.0.borrow().sequence,
        sequence,
        "active command admitted non-command evidence"
    );
    assert_eq!(session.workspace().active_buffer().text(), "A");
    assert_eq!(session.workspace().active_buffer().version(), 0);
    assert!(
        session.recovery_reason().is_none(),
        "active authority is not recording poison"
    );
    assert_eq!(
        session.execute(EditorCommand::RequestQuit).unwrap(),
        EditorOutcome::Quit
    );
    session.effects.0.borrow_mut().command_active = false;
    session.execute(EditorCommand::Insert('x')).unwrap();
    session.quit().unwrap();
}

#[test]
fn command_authority_blocks_file_save_budget_and_boundary_entry_before_preflight() {
    let (fixture, mut session) = fixture();
    session.effects.0.borrow_mut().command_active = true;
    let sequence = session.effects.0.borrow().sequence;
    assert!(session.create_file("new.rs").is_err());
    assert!(session.rename_selected("main.rs").is_err());
    assert!(session.delete_selected().is_err());
    assert!(session.confirm_delete().is_err());
    assert!(session.save_all().is_err());
    assert_eq!(
        session.save_all_and_check().unwrap(),
        ExplicitSaveOutcome::RunnerBusy
    );
    assert!(session.capture_boundary().is_err());
    assert!(session.recheck_external().is_err());
    assert!(session.set_budgets(SessionBudgets::default()).is_err());
    assert!(session.workspace_mut().save_all().is_err());
    assert!(session.workspace_mut().save_active().is_err());
    assert!(session.workspace_mut().create_file("new.rs").is_err());
    assert_eq!(session.effects.0.borrow().sequence, sequence);
    assert_eq!(fs::read(fixture.0.join("main.rs")).unwrap(), b"A");
    assert!(!fixture.0.join("new.rs").exists());
    session.quit().unwrap();
}

#[test]
fn active_command_tick_does_not_autosave_or_restore_inflight_tool_changes() {
    let (fixture, mut session) = fixture();
    session.execute(EditorCommand::Insert('B')).unwrap();
    session.effects.0.borrow_mut().command_active = true;
    session.last_save = Instant::now() - Duration::from_secs(3);
    session.last_recheck = Instant::now() - Duration::from_secs(3);
    fs::write(fixture.0.join("main.rs"), b"tool in flight").unwrap();
    let sequence = session.effects.0.borrow().sequence;
    session.tick().unwrap();
    assert_eq!(
        fs::read(fixture.0.join("main.rs")).unwrap(),
        b"tool in flight"
    );
    assert_eq!(session.workspace().active_buffer().text(), "BA");
    assert_eq!(session.effects.0.borrow().sequence, sequence);
    session.quit().unwrap();
}

#[test]
fn active_command_keeps_direct_viewport_follow_unchanged() {
    let (_fixture, mut session) = fixture();
    session.execute(EditorCommand::Insert('B')).unwrap();
    let before = *session.workspace().active_viewport();
    session.effects.0.borrow_mut().command_active = true;
    session.workspace_mut().follow_cursor(1, 1);
    assert_eq!(*session.workspace().active_viewport(), before);
    session.quit().unwrap();
}

#[test]
fn quit_bounds_stalled_preparation_without_clearing_activity() {
    const STALL: Duration = Duration::from_secs(4);
    const TIMING_TOLERANCE: Duration = Duration::from_secs(1);

    let (_fixture, mut session) = fixture();
    let (cancel, worker_finished) = session.install_stalled_preparing_for_test(STALL).unwrap();
    let started = Instant::now();
    let first_turn = session.prepare_terminal_quit();
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(MAX_COMMAND_CLEANUP_MILLIS + 500) + TIMING_TOLERANCE,
        "stalled Preparing worker kept quit polling for {elapsed:?}"
    );
    assert!(matches!(first_turn, TerminalQuit::CleanupPending));
    assert!(!session.command_terminal_restoration_safe());
    assert_eq!(cancel.load(Ordering::Acquire), 2);

    let completion_deadline = Instant::now() + STALL;
    while !worker_finished.load(Ordering::Acquire) && Instant::now() < completion_deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(worker_finished.load(Ordering::Acquire));
    let second_turn = session.prepare_terminal_quit();
    assert!(matches!(second_turn, TerminalQuit::Ready(Err(_))));
    assert!(session.command_terminal_restoration_safe());
}

#[cfg(unix)]
#[test]
fn cleanup_uncertain_running_does_not_restore_terminal_before_reap() {
    let (_fixture, mut session) = fixture();
    let group_reaped = session
        .install_cleanup_uncertain_running_for_test()
        .unwrap();
    let restored = Arc::new(AtomicBool::new(false));
    let mut terminal = Some(
        TerminalSession::enter(RestorationProbe(restored.clone()))
            .expect("terminal setup succeeds"),
    );

    assert!(matches!(
        session.prepare_terminal_quit(),
        TerminalQuit::CleanupPending
    ));
    assert!(!group_reaped.load(Ordering::Acquire));
    if session.command_terminal_restoration_safe() {
        drop(terminal.take());
    }
    assert!(
        !restored.load(Ordering::Acquire),
        "terminal restored while the owned Running group was explicitly unreaped"
    );

    group_reaped.store(true, Ordering::Release);
    assert!(session.poll_command().unwrap());
    assert!(session.command_terminal_restoration_safe());
    drop(terminal.take());
    assert!(restored.load(Ordering::Acquire));
}

fn poll_until_transition(session: &mut ProductionSession) -> Result<bool> {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        match session.poll_command() {
            Ok(false) if Instant::now() < until => std::thread::sleep(Duration::from_millis(2)),
            result => return result,
        }
    }
}

#[cfg(unix)]
fn trusted_sleep_command() -> Command {
    let mut command = Command::new("python3");
    command
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/command_child.py"
        ))
        .arg("sleep");
    command
}

#[cfg(unix)]
#[test]
fn delayed_reaping_records_a_replay_valid_finish_after_confirmation() {
    let (fixture, mut session) = fixture();
    let (confirmation, dropped_while_armed) = session
        .install_process_result_tuple_for_test(false, true)
        .unwrap();
    assert!(poll_until_transition(&mut session).unwrap());
    assert!(!session.command_terminal_restoration_safe());

    std::thread::sleep(Duration::from_millis(MAX_COMMAND_CLEANUP_MILLIS + 100));
    confirmation.store(true, Ordering::Release);
    assert!(
        session.poll_command().unwrap(),
        "Reaping did not enter Recording"
    );
    assert!(session.command_terminal_restoration_safe());
    assert!(
        session.poll_command().unwrap(),
        "Recording did not persist finish"
    );
    assert!(!session.command_active());
    assert!(!dropped_while_armed.load(Ordering::Acquire));

    let (journal_path, session_id) = {
        let authority = session.effects.0.borrow();
        (
            authority.owner.display_path().to_path_buf(),
            authority.metadata.session_id.clone(),
        )
    };
    let mut journal = Journal::open_read_only_no_follow(&journal_path).unwrap();
    let events = journal.read_events(&session_id, 1, 64).unwrap();
    let (envelope_millis, finish) = events
        .iter()
        .find_map(|envelope| match &envelope.event {
            Event::ControlledCommandFinished(finish) => Some((envelope.monotonic_millis, finish)),
            _ => None,
        })
        .expect("durable controlled-command finish");
    assert!(
        finish.finished_millis.saturating_sub(finish.started_millis)
            <= 1 + MAX_COMMAND_CLEANUP_MILLIS
    );
    assert!(envelope_millis > finish.finished_millis);
    drop(journal);

    session.quit().unwrap();
    let resumed = ProductionSession::resume(&fixture.0, MANIFEST, ResumeChoice::Resume).unwrap();
    resumed.quit().unwrap();
}

#[cfg(unix)]
#[test]
fn full_capture_allowance_remains_replay_valid_after_deadline() {
    const DEADLINE: Duration = Duration::from_millis(250);
    const PRE_SPAWN_DELAY: Duration = Duration::from_millis(25);

    let (fixture, mut session) = fixture();
    let fixture_name = fixture.0.file_name().unwrap().to_string_lossy();
    let fixture_parent = fixture.0.parent().unwrap();
    let owned_pid_path = fixture_parent.join(format!("{fixture_name}.owned-pid"));
    let escaped_pid_path = fixture_parent.join(format!("{fixture_name}.escaped-pid"));
    let mut processes = FullCaptureProcesses {
        owned_pid_path: owned_pid_path.clone(),
        escaped_pid_path: escaped_pid_path.clone(),
        escaped_pid: None,
    };
    let mut command = Command::new("python3");
    command
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/command_child.py"
        ))
        .arg("deadline-escaped-pipes")
        .arg(&owned_pid_path)
        .arg(&escaped_pid_path);
    session
        .install_full_capture_running_for_test(command, DEADLINE, PRE_SPAWN_DELAY)
        .unwrap();

    let until_recording = Instant::now() + Duration::from_secs(5);
    while !session.command_terminal_restoration_safe() && Instant::now() < until_recording {
        session.poll_command().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    let finish = session
        .recording_finish_for_test()
        .expect("full-window runner result reached Recording");
    let duration = finish.finished_millis.saturating_sub(finish.started_millis);
    assert!(
        duration > DEADLINE.as_millis() as u64 + MAX_COMMAND_CLEANUP_MILLIS,
        "truthful duration {duration} did not exceed the former replay ceiling"
    );
    assert!(matches!(
        finish.outcome,
        CommandOutcome::Terminated {
            reason: CommandTermination::Deadline,
            ..
        }
    ));
    assert_eq!(finish.stdout.completeness, CaptureCompleteness::ReadFailed);
    assert_eq!(finish.stderr.completeness, CaptureCompleteness::ReadFailed);

    let owned_pid = FullCaptureProcesses::wait_for_pid(&owned_pid_path)
        .expect("trusted fixture recorded the owned leader PID");
    assert!(!process_exists(owned_pid), "owned leader remained live");
    assert!(
        !process_group_exists(owned_pid),
        "owned process group remained live"
    );
    assert!(
        processes.cleanup_escaped(),
        "escaped pipe-holder group cleanup was not confirmed"
    );

    assert!(
        session.poll_command().unwrap(),
        "Recording did not persist the truthful finish"
    );
    assert!(!session.command_active());
    assert!(session.recovery_reason().is_none());
    let marker: serde_json::Value = serde_json::from_slice(
        &session
            .effects
            .0
            .borrow()
            .owner
            .read_artifact("command-activity.json", METADATA_LIMIT)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(marker["active"], false);

    let (journal_path, session_id) = {
        let authority = session.effects.0.borrow();
        (
            authority.owner.display_path().to_path_buf(),
            authority.metadata.session_id.clone(),
        )
    };
    let mut journal = Journal::open_read_only_no_follow(&journal_path).unwrap();
    let events = journal.read_events(&session_id, 1, 64).unwrap();
    let durable_start = events
        .iter()
        .find_map(|envelope| match &envelope.event {
            Event::ControlledCommandStarted(start) => Some(start),
            _ => None,
        })
        .expect("durable controlled-command start");
    let (finish_envelope_millis, durable_finish) = events
        .iter()
        .find_map(|envelope| match &envelope.event {
            Event::ControlledCommandFinished(finish) => Some((envelope.monotonic_millis, finish)),
            _ => None,
        })
        .expect("durable controlled-command finish");
    assert_eq!(durable_finish.command_id, finish.command_id);
    assert_eq!(durable_finish.started_millis, finish.started_millis);
    assert_eq!(durable_finish.finished_millis, finish.finished_millis);
    assert_eq!(durable_finish.outcome, finish.outcome);
    assert_eq!(durable_finish.stdout, finish.stdout);
    assert_eq!(durable_finish.stderr, finish.stderr);
    assert!(
        durable_finish.after.checkpoint_sequence > finish.after.checkpoint_sequence,
        "durable finish did not link the production post-command checkpoint"
    );
    assert!(
        durable_finish
            .finished_millis
            .saturating_sub(durable_finish.started_millis)
            > durable_start.deadline_millis + MAX_COMMAND_CLEANUP_MILLIS
    );
    assert!(finish_envelope_millis >= durable_finish.finished_millis);
    drop(journal);

    session.quit().unwrap();
    let resumed = ProductionSession::resume(&fixture.0, MANIFEST, ResumeChoice::Resume).unwrap();
    assert!(resumed.recovery_reason().is_none());
    assert!(!resumed.command_active());
    resumed.quit().unwrap();
}

#[test]
fn replay_invalid_finish_is_rejected_before_durable_submission() {
    let (_fixture, mut session) = fixture();
    let finish = session.replay_invalid_finish_for_test().unwrap();
    let (journal_path, session_id) = {
        let authority = session.effects.0.borrow();
        (
            authority.owner.display_path().to_path_buf(),
            authority.metadata.session_id.clone(),
        )
    };
    let mut journal = Journal::open_read_only_no_follow(&journal_path).unwrap();
    let before = journal.verify_session_chain(&session_id).unwrap();
    drop(journal);

    let rejected = session
        .effects
        .0
        .borrow_mut()
        .append(Event::ControlledCommandFinished(finish));
    assert!(rejected.is_err());

    let mut journal = Journal::open_read_only_no_follow(&journal_path).unwrap();
    let after = journal.verify_session_chain(&session_id).unwrap();
    assert_eq!(after.event_count, before.event_count);
    assert_eq!(after.final_hash, before.final_hash);
}

fn empty_comparison(case: &str) -> TestCaseComparison {
    let hash = crate::console::hash_bytes(b"");
    TestCaseComparison {
        case: TestCase::new(case).unwrap(),
        outcome: TestCaseOutcome::Pass,
        expected_blake3: Some(hash),
        actual_blake3: Some(hash),
    }
}

#[test]
fn compared_finish_rejects_invalid_comparison_without_persisting_either_event() {
    for missing_expected in [false, true] {
        let (_fixture, mut session) = fixture();
        let mut comparison = empty_comparison(if missing_expected { "input" } else { "other" });
        if missing_expected {
            comparison.expected_blake3 = None;
        }
        session
            .install_compared_finish_for_test(comparison)
            .unwrap();
        assert!(session.poll_command().is_err());
        assert!(session.command_active());
        assert!(session.recovery_reason().is_some());
        let authority = session.effects.0.borrow();
        assert!(
            authority
                .replay
                .as_ref()
                .unwrap()
                .controlled_command_pending()
        );
        let mut journal =
            Journal::open_read_only_no_follow(authority.owner.display_path()).unwrap();
        let events = journal
            .read_events(&authority.metadata.session_id, 1, 1000)
            .unwrap();
        assert!(
            !events.iter().any(|envelope| matches!(
                envelope.event,
                Event::ControlledCommandFinished(_) | Event::TestCaseCompared(_)
            )),
            "invalid comparison must not leave a committed finish or comparison"
        );
        let tail = events.last().unwrap();
        assert_eq!(authority.sequence, tail.sequence);
        assert_eq!(authority.hash, tail.event_hash);
        assert_eq!(
            authority.replay.as_ref().unwrap().last_event_hash(),
            tail.event_hash
        );
    }
}

fn prepared_compared_finish(
    session: &mut ProductionSession,
) -> (ControlledCommandFinished, TestCaseCompared) {
    session
        .install_compared_finish_for_test(empty_comparison("input"))
        .unwrap();
    let finish = session.checkpointed_recording_finish_for_test().unwrap();
    let hash = crate::console::hash_bytes(b"");
    let comparison = TestCaseCompared {
        command_id: finish.command_id.clone(),
        case: "input".into(),
        expected_blake3: hash,
        actual_blake3: Some(hash),
        outcome: TestCaseComparisonOutcome::Pass,
    };
    (finish, comparison)
}

fn assert_compared_prefix_unchanged(authority: &Authority, sequence: u64, hash: Hash, millis: u64) {
    assert_eq!(authority.sequence, sequence);
    assert_eq!(authority.hash, hash);
    assert_eq!(authority.persisted_millis, millis);
    let replay = authority.replay.as_ref().unwrap();
    assert_eq!(replay.next_sequence(), sequence + 1);
    assert_eq!(replay.last_event_hash(), hash);
    assert!(replay.controlled_command_pending());
    assert!(authority.command_active);
    assert!(authority.poison.is_some());
    let mut journal = Journal::open_read_only_no_follow(authority.owner.display_path()).unwrap();
    let chain = journal
        .verify_session_chain(&authority.metadata.session_id)
        .unwrap();
    assert_eq!(chain.event_count, sequence);
    assert_eq!(chain.final_hash, hash);
}

#[test]
fn compared_finish_prevalidates_both_events_before_accessing_writer() {
    for invalid in [
        "finish",
        "comparison_shape",
        "comparison_case",
        "comparison_actual",
    ] {
        let (_fixture, mut session) = fixture();
        let (mut finish, mut comparison) = prepared_compared_finish(&mut session);
        match invalid {
            "finish" => finish.after.workspace_hash = Hash::zero(),
            "comparison_shape" => comparison.case = "invalid!".into(),
            "comparison_case" => comparison.case = "other".into(),
            "comparison_actual" => comparison.actual_blake3 = Some(Hash::zero()),
            _ => unreachable!(),
        }
        let mut authority = session.effects.0.borrow_mut();
        authority.writer.take().unwrap().shutdown().unwrap();
        let (sequence, hash, millis) = (
            authority.sequence,
            authority.hash,
            authority.persisted_millis,
        );
        let error = authority
            .append_compared_finish(finish, comparison)
            .unwrap_err()
            .to_string();
        assert!(
            !error.contains("writer closed"),
            "{invalid}: must reject evidence before write: {error}"
        );
        assert_compared_prefix_unchanged(&authority, sequence, hash, millis);
    }
}

#[test]
fn compared_finish_failed_receipt_keeps_cursor_replay_and_command_ownership() {
    let (_fixture, mut session) = fixture();
    let (finish, comparison) = prepared_compared_finish(&mut session);
    let mut authority = session.effects.0.borrow_mut();
    let connection = rusqlite::Connection::open(authority.owner.display_path()).unwrap();
    // This trigger causes schema rejection in the worker: it exercises a real
    // failed receipt, not the second-INSERT rollback proven in journal tests.
    connection
        .execute_batch(
            "CREATE TRIGGER fail_compared_finish BEFORE INSERT ON events
         BEGIN SELECT RAISE(ABORT, 'injected pair recording failure'); END;",
        )
        .unwrap();
    let (sequence, hash, millis) = (
        authority.sequence,
        authority.hash,
        authority.persisted_millis,
    );
    let error = authority
        .append_compared_finish(finish, comparison)
        .unwrap_err()
        .to_string();
    assert!(error.contains("unexpected table, index, view, or trigger"));
    connection
        .execute_batch("DROP TRIGGER fail_compared_finish")
        .unwrap();
    assert_compared_prefix_unchanged(&authority, sequence, hash, millis);
    drop(authority);
    assert!(session.execute(EditorCommand::Insert('X')).is_err());
    assert!(session.command_outcome().is_none());
    let activity: serde_json::Value = serde_json::from_slice(
        &fs::read(
            session
                .workspace()
                .root()
                .join(".rustrace/command-activity.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(activity["active"], true);
}

#[test]
fn compared_finish_rejects_acknowledged_sequence_or_hash_without_publishing() {
    for same_sequence in [false, true] {
        let (_fixture, mut session) = fixture();
        let (finish, comparison) = prepared_compared_finish(&mut session);
        let mut authority = session.effects.0.borrow_mut();
        let (sequence, hash, millis) = (
            authority.sequence,
            authority.hash,
            authority.persisted_millis,
        );
        authority.writer.take().unwrap().shutdown().unwrap();
        // A deliberately misbound real writer assigns either a different
        // sequence, or the same sequence against a different durable chain.
        let id = authority.metadata.session_id.clone();
        let mut other = Journal::open_in_memory().unwrap();
        other.create_or_resume_session(&id).unwrap();
        if same_sequence {
            let mut previous = Hash::zero();
            for current in 1..=sequence {
                let envelope = EventEnvelope {
                    format_version: FORMAT_VERSION_V1,
                    session_id: id.clone(),
                    sequence: current,
                    monotonic_millis: 0,
                    wall_clock_utc: None,
                    previous_event_hash: Hash::zero(),
                    event_hash: Hash::zero(),
                    event: Event::FileFocused(FileFocused {
                        document_id: DocumentId::new("other").unwrap(),
                    }),
                }
                .seal(previous)
                .unwrap();
                other.append_event(&id, &envelope).unwrap();
                previous = envelope.event_hash;
            }
        }
        authority.writer = Some(JournalWriter::spawn(1, other).unwrap());
        let error = authority
            .append_compared_finish(finish, comparison)
            .unwrap_err()
            .to_string();
        assert!(error.contains("persisted compared finish identity differs from prevalidation"));
        assert_compared_prefix_unchanged(&authority, sequence, hash, millis);
    }
}

#[test]
fn compared_finish_success_publishes_both_events_and_releases_ownership() {
    let (fixture, mut session) = fixture();
    session
        .install_compared_finish_for_test(empty_comparison("input"))
        .unwrap();
    session.poll_command().unwrap();
    assert!(!session.command_active());
    assert!(session.recovery_reason().is_none());
    assert!(matches!(
        session.command_outcome(),
        Some(CommandOutcome::Exited { code: 0 })
    ));
    assert_eq!(
        session.take_test_case_result().unwrap().outcome,
        TestCaseOutcome::Pass
    );
    let authority = session.effects.0.borrow();
    let mut journal = Journal::open_read_only_no_follow(authority.owner.display_path()).unwrap();
    let events = journal
        .read_events(&authority.metadata.session_id, 1, 1000)
        .unwrap();
    let tail = events.last().unwrap();
    let first = &events[events.len() - 2];
    assert!(matches!(first.event, Event::ControlledCommandFinished(_)));
    assert!(matches!(tail.event, Event::TestCaseCompared(_)));
    assert_eq!(tail.sequence, first.sequence + 1);
    assert_eq!(tail.monotonic_millis, first.monotonic_millis);
    assert_eq!(tail.previous_event_hash, first.event_hash);
    assert_eq!(authority.sequence, tail.sequence);
    assert_eq!(authority.hash, tail.event_hash);
    assert_eq!(authority.persisted_millis, tail.monotonic_millis);
    assert_eq!(
        authority.replay.as_ref().unwrap().last_event_hash(),
        tail.event_hash
    );
    assert_eq!(
        authority.replay.as_ref().unwrap().next_sequence(),
        tail.sequence + 1
    );
    assert!(
        !authority
            .replay
            .as_ref()
            .unwrap()
            .controlled_command_pending()
    );
    let genesis = journal
        .load_checkpoint(&authority.metadata.session_id, 1)
        .unwrap()
        .unwrap();
    let mut reconstructed = ReplayEngine::from_initial_checkpoint(genesis).unwrap();
    for envelope in events.iter().skip(1) {
        reconstructed.apply(envelope).unwrap();
    }
    assert_eq!(
        reconstructed.next_sequence(),
        authority.replay.as_ref().unwrap().next_sequence()
    );
    assert_eq!(
        reconstructed.last_event_hash(),
        authority.replay.as_ref().unwrap().last_event_hash()
    );
    assert_eq!(
        reconstructed.current_workspace_hash(),
        authority.replay.as_ref().unwrap().current_workspace_hash()
    );
    let activity: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.0.join(".rustrace/command-activity.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(activity["active"], false);
    drop(journal);
    drop(authority);
    session.quit().unwrap();
}

#[test]
fn compared_finish_reserves_headroom_for_two_events() {
    let (_fixture, mut session) = fixture();
    let (finish, comparison) = prepared_compared_finish(&mut session);
    let mut authority = session.effects.0.borrow_mut();
    let (sequence, hash, millis) = (
        authority.sequence,
        authority.hash,
        authority.persisted_millis,
    );
    authority.budgets.events = sequence + 3;
    assert!(
        authority.headroom(0).is_ok(),
        "ordinary append still has headroom"
    );
    let error = authority
        .append_compared_finish(finish, comparison)
        .unwrap_err()
        .to_string();
    assert!(error.contains("event budget exhausted"));
    assert_compared_prefix_unchanged(&authority, sequence, hash, millis);
}

#[cfg(unix)]
#[test]
fn panicking_running_worker_never_marks_live_group_terminal_safe() {
    let (_fixture, mut session) = fixture();
    let spawned = Arc::new(AtomicI32::new(0));
    let _failsafe = FailsafeProcessGroup {
        spawned: spawned.clone(),
    };
    session
        .install_panicking_running_for_test(trusted_sleep_command(), spawned.clone())
        .unwrap();
    let until_spawned = Instant::now() + Duration::from_secs(2);
    while spawned.load(Ordering::Acquire) <= 0 && Instant::now() < until_spawned {
        std::thread::sleep(Duration::from_millis(2));
    }

    let restored = Arc::new(AtomicBool::new(false));
    let restored_while_group_live = Arc::new(AtomicBool::new(false));
    let mut terminal = Some(
        TerminalSession::enter(GroupRestorationProbe {
            restored: restored.clone(),
            restored_while_group_live: restored_while_group_live.clone(),
            spawned: spawned.clone(),
        })
        .unwrap(),
    );

    for _ in 0..3 {
        let _ = session.prepare_terminal_quit();
        if session.command_terminal_restoration_safe() {
            drop(terminal.take());
            break;
        }
    }

    assert!(restored.load(Ordering::Acquire));
    assert!(
        !restored_while_group_live.load(Ordering::Acquire),
        "terminal restored while the panicking worker's owned group was live"
    );
    let pid = spawned.load(Ordering::Acquire);
    assert!(pid > 0, "panic injection did not observe a spawned child");
    assert!(
        !process_group_exists(pid),
        "owned panic fixture group survived"
    );
}

#[cfg(unix)]
#[test]
fn inconsistent_process_cleanup_tuples_preserve_or_block_authority() {
    let mut violations = Vec::new();
    for (cleanup_confirmed, with_pending_cleanup) in
        [(true, false), (false, true), (true, true), (false, false)]
    {
        let (_fixture, mut session) = fixture();
        let (confirmation, dropped_while_armed) = session
            .install_process_result_tuple_for_test(cleanup_confirmed, with_pending_cleanup)
            .unwrap();
        let transition = poll_until_transition(&mut session);
        let safe = session.command_terminal_restoration_safe();

        if with_pending_cleanup && safe {
            violations.push(format!(
                "tuple ({cleanup_confirmed}, Some) discarded cleanup authority"
            ));
        }
        if !cleanup_confirmed && !with_pending_cleanup && safe {
            violations.push("tuple (false, None) declared restoration safe".to_owned());
        }
        if dropped_while_armed.load(Ordering::Acquire) {
            violations.push(format!(
                "tuple ({cleanup_confirmed}, {with_pending_cleanup}) dropped an armed handle"
            ));
        }

        if with_pending_cleanup && !safe {
            confirmation.store(true, Ordering::Release);
            let _ = session.poll_command();
            let _ = session.poll_command();
        } else if cleanup_confirmed && !with_pending_cleanup {
            assert!(transition.is_ok());
            let _ = session.poll_command();
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("; "));
}

#[cfg(unix)]
#[test]
fn resolver_pending_cleanup_remains_owned_until_confirmation() {
    let (_fixture, mut session) = fixture();
    let (confirmation, dropped_while_armed) =
        session.install_uncertain_resolver_for_test().unwrap();
    let started = Instant::now();
    let first_turn = session.prepare_terminal_quit();
    let elapsed = started.elapsed();
    let remained_pending = matches!(first_turn, TerminalQuit::CleanupPending);
    let remained_unsafe = !session.command_terminal_restoration_safe();
    let dropped_before_confirmation = dropped_while_armed.load(Ordering::Acquire);

    confirmation.store(true, Ordering::Release);
    for _ in 0..3 {
        let _ = session.prepare_terminal_quit();
        if session.command_terminal_restoration_safe() {
            break;
        }
    }

    assert!(
        elapsed < Duration::from_millis(MAX_COMMAND_CLEANUP_MILLIS + 500) + Duration::from_secs(1)
    );
    assert!(
        remained_pending,
        "resolver cleanup authority was not retained"
    );
    assert!(remained_unsafe, "Preparing was declared terminal-safe");
    assert!(
        !dropped_before_confirmation,
        "resolver dropped an armed cleanup handle"
    );
}

fn panic_payload_text(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic")
}

#[cfg(unix)]
#[test]
fn editor_loop_unwind_and_error_wait_for_cleanup_before_terminal_restore() {
    let mut violations = Vec::new();
    for initial_panics in [true, false] {
        let (fixture, mut session) = fixture();
        let (confirmation, _dropped) = session
            .install_process_result_tuple_for_test(false, true)
            .unwrap();
        assert!(poll_until_transition(&mut session).unwrap());

        let restored = Arc::new(AtomicBool::new(false));
        let mouse_disabled = Arc::new(AtomicBool::new(false));
        let restored_before_command_finish = Arc::new(AtomicBool::new(false));
        let operations = LifecycleRestorationProbe {
            restored: restored.clone(),
            mouse_disabled: mouse_disabled.clone(),
            restored_before_command_finish: restored_before_command_finish.clone(),
            confirmation: confirmation.clone(),
            activity_path: fixture.0.join(".rustrace/command-activity.json"),
        };
        let cleanup_confirmation = confirmation.clone();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::work_editor::run_editor_with_for_test(
                session,
                "Runner",
                operations,
                move |_session, _title, quit_pending| {
                    if !quit_pending {
                        if initial_panics {
                            panic!("initial editor panic");
                        }
                        return Err(io::Error::other("ordinary editor error").into());
                    }
                    cleanup_confirmation.store(true, Ordering::Release);
                    if initial_panics {
                        Err(io::Error::other("cleanup screen error").into())
                    } else {
                        panic!("cleanup editor panic");
                    }
                },
            )
        }));
        let expected_panic = if initial_panics {
            "initial editor panic"
        } else {
            "cleanup editor panic"
        };
        match unwind {
            Err(payload) if panic_payload_text(payload.as_ref()) == expected_panic => {}
            Err(payload) => violations.push(format!(
                "unexpected panic payload: {}",
                panic_payload_text(payload.as_ref())
            )),
            Ok(result) => violations.push(format!("expected {expected_panic:?}, got {result:?}")),
        }
        if !restored.load(Ordering::Acquire) {
            violations.push(format!("terminal was not restored for {expected_panic}"));
        }
        if !mouse_disabled.load(Ordering::Acquire) {
            violations.push(format!(
                "mouse capture was not disabled for {expected_panic}"
            ));
        }
        if restored_before_command_finish.load(Ordering::Acquire) {
            violations.push(format!(
                "terminal restored before confirmed cleanup and inactive marker for {expected_panic}"
            ));
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("; "));
}
