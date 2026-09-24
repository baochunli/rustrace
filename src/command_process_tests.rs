//! New bounded process API Red. These tests intentionally precede its code;
//! the initial missing-API compile result must not be called behavioral Red.
use super::*;
use rustrace_model::{CaptureCompleteness, CommandOutcome, CommandTermination};
use std::{
    fs::{self, File},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicI32, AtomicU8, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

#[test]
fn submitted_unicode_lines_and_eof_stream_to_child_without_recording_input() {
    let (sender, receiver) = mpsc::sync_channel(8);
    sender.send(StdinMessage::Line("café 🦀".into())).unwrap();
    sender.send(StdinMessage::Line(String::new())).unwrap();
    sender.send(StdinMessage::Eof).unwrap();
    drop(sender);
    let live = LiveOutput::new(32 * 1024);
    let result = execute_with_io(
        fixture("stdin"),
        limits(),
        Arc::new(AtomicU8::new(0)),
        ProcessIo {
            stdin: ProcessStdin::Submitted(receiver),
            stdout: ProcessStdout::Captured,
            live: Some(live.clone()),
        },
    );
    assert_eq!(result.outcome, CommandOutcome::Exited { code: 0 });
    assert_eq!(result.stdout.bytes, "stdout:café 🦀\n\n".as_bytes());
    assert_eq!(result.stderr.bytes, b"stderr:done");
    let displayed = live.snapshot();
    assert!(
        displayed
            .windows("café 🦀\n\n".len())
            .any(|part| part == "café 🦀\n\n".as_bytes())
    );
    assert!(displayed.ends_with(b"stderr:done"));
}

#[test]
fn direct_file_stdin_and_stdout_preserve_natural_bytes_and_unavailable_capture() {
    let root = std::env::temp_dir().join(format!("rustrace-command-files-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root).unwrap();
    fs::write(root.join("input"), b"file\x00input\xff").unwrap();
    let output = File::create(root.join("output")).unwrap();
    let live = LiveOutput::new(32 * 1024);
    let result = execute_with_io(
        fixture("stdin"),
        limits(),
        Arc::new(AtomicU8::new(0)),
        ProcessIo {
            stdin: ProcessStdin::File(File::open(root.join("input")).unwrap()),
            stdout: ProcessStdout::File(output),
            live: Some(live.clone()),
        },
    );
    assert_eq!(result.outcome, CommandOutcome::Exited { code: 0 });
    assert_eq!(
        fs::read(root.join("output")).unwrap(),
        b"stdout:file\x00input\xff"
    );
    assert!(result.stdout.bytes.is_empty());
    assert_eq!(result.stdout.completeness, CaptureCompleteness::Unavailable);
    assert_eq!(result.stderr.bytes, b"stderr:done");
    assert_eq!(live.snapshot(), b"stderr:done");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn redirected_launch_failure_honestly_leaves_the_preopened_output_truncated() {
    let root = std::env::temp_dir().join(format!(
        "rustrace-command-launch-output-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root).unwrap();
    let path = root.join("output");
    fs::write(&path, b"previous").unwrap();
    let output = File::options().write(true).open(&path).unwrap();
    output.set_len(0).unwrap();
    let result = execute_with_io(
        Command::new("/rustrace-fixture-does-not-exist"),
        limits(),
        Arc::new(AtomicU8::new(0)),
        ProcessIo {
            stdin: ProcessStdin::Closed,
            stdout: ProcessStdout::File(output),
            live: None,
        },
    );
    assert!(matches!(
        result.outcome,
        CommandOutcome::LaunchFailed { .. }
    ));
    assert_eq!(result.stdout.completeness, CaptureCompleteness::Unavailable);
    assert!(fs::read(&path).unwrap().is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn live_console_projection_discards_oldest_bytes_at_its_fixed_bound() {
    let live = LiveOutput::new(8);
    live.push(b"12345");
    live.push(b"67890");
    assert_eq!(live.snapshot(), b"34567890");
    let positioned = live.positioned_snapshot();
    assert_eq!(positioned.start, 2);
    assert_eq!(positioned.bytes, b"34567890");
    assert_ne!(positioned.id, LiveOutput::new(8).positioned_snapshot().id);
}

#[cfg(unix)]
#[test]
fn submitted_input_pending_bytes_remain_bounded_when_the_child_does_not_read() {
    use std::process::Stdio;

    let mut child = Command::new("python3")
        .args(["-c", "import time; time.sleep(20)"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let (sender, receiver) = mpsc::sync_channel(128);
    for _ in 0..128 {
        sender
            .send(StdinMessage::Line("x".repeat(MAX_STDIN_LINE_BYTES)))
            .unwrap();
    }
    let mut input = SubmittedInput::new(receiver, child.stdin.take().unwrap());
    for _ in 0..128 {
        input.poll();
    }
    let bounded = input.pending.len() <= MAX_PENDING_STDIN_BYTES;
    drop(sender);
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(bounded);
}

#[cfg(unix)]
#[test]
fn submitted_eof_after_buffered_lines_flushes_without_reopening_or_panicking() {
    use std::process::Stdio;

    let mut child = Command::new("python3")
        .args(["-c", "import time; time.sleep(20)"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let (sender, receiver) = mpsc::sync_channel(17);
    for _ in 0..16 {
        sender
            .send(StdinMessage::Line("x".repeat(MAX_STDIN_LINE_BYTES)))
            .unwrap();
    }
    sender.send(StdinMessage::Eof).unwrap();
    let mut input = SubmittedInput::new(receiver, child.stdin.take().unwrap());
    for _ in 0..128 {
        input.poll();
    }
    let eof = input.eof;
    let disconnected = input.receiver.is_none();
    let bounded = input.pending.len() <= MAX_PENDING_STDIN_BYTES;
    let later_rejected = matches!(
        sender.try_send(StdinMessage::Line("later".into())),
        Err(mpsc::TrySendError::Disconnected(_))
    );
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(eof);
    assert!(disconnected);
    assert!(bounded);
    assert!(later_rejected);
}

fn fixture(mode: &str) -> Command {
    let mut command = Command::new("python3");
    command
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/command_child.py"
        ))
        .arg(mode);
    command
}

fn limits() -> ProcessLimits {
    ProcessLimits {
        deadline: Duration::from_secs(3),
        output_bytes: 32 * 1024,
    }
}

#[cfg(unix)]
fn group_exists(pid: i32) -> bool {
    pid > 0
        && (unsafe { libc::kill(-pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

#[cfg(unix)]
struct PanicFixtureCleanup(Arc<AtomicI32>);

#[cfg(unix)]
impl Drop for PanicFixtureCleanup {
    fn drop(&mut self) {
        let until_spawned = Instant::now() + Duration::from_secs(1);
        let pid = loop {
            let pid = self.0.load(Ordering::Acquire);
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
                || waited == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
                && !group_exists(pid)
            {
                break;
            }
            let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(unix)]
#[test]
fn panic_after_spawn_retains_owned_group_until_reaped() {
    let spawned = Arc::new(AtomicI32::new(0));
    let _cleanup = PanicFixtureCleanup(spawned.clone());
    let worker = std::thread::spawn({
        let spawned = spawned.clone();
        move || {
            execute_panicking_after_spawn_for_test(
                fixture("sleep"),
                limits(),
                Arc::new(AtomicU8::new(0)),
                spawned,
                ProcessCleanupHandoff::new(),
            )
        }
    });
    assert!(worker.join().is_err(), "after-spawn hook did not panic");
    let pid = spawned.load(Ordering::Acquire);
    assert!(pid > 0, "after-spawn hook did not publish the child PID");
    assert!(
        !group_exists(pid),
        "owned group survived process-worker unwind"
    );
}

#[test]
fn cancellation_before_spawn_does_not_invent_empty_captured_streams() {
    for value in [1, 2] {
        let result = execute(fixture("bytes"), limits(), Arc::new(AtomicU8::new(value)));
        assert_eq!(result.stdout.completeness, CaptureCompleteness::Unavailable);
        assert_eq!(result.stderr.completeness, CaptureCompleteness::Unavailable);
        assert!(result.stdout.bytes.is_empty() && result.stderr.bytes.is_empty());
        assert!(matches!(
            result.outcome,
            CommandOutcome::Terminated { signal: None, .. }
        ));
        assert!(result.cleanup_confirmed);
    }
}

#[cfg(unix)]
#[test]
fn read_failure_retains_the_original_partial_capture() {
    struct FailsAfterByte(bool);
    impl std::io::Read for FailsAfterByte {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            if self.0 {
                return Err(std::io::Error::other("trusted read fault"));
            }
            self.0 = true;
            bytes[0] = 0xff;
            Ok(1)
        }
    }
    impl std::os::fd::AsRawFd for FailsAfterByte {
        fn as_raw_fd(&self) -> std::os::fd::RawFd {
            -1
        }
    }
    let mut pipe = Pipe {
        reader: FailsAfterByte(false),
        capture: CapturedStream {
            bytes: Vec::new(),
            completeness: CaptureCompleteness::Complete,
        },
        done: false,
    };
    pipe.drain_with_live(&mut 100, None);
    assert_eq!(pipe.capture.bytes, [0xff]);
    assert_eq!(pipe.capture.completeness, CaptureCompleteness::ReadFailed);
    assert!(pipe.done);
}

#[test]
fn process_original_dual_stream_bytes_eof_and_nonzero_exit() {
    let result = execute(fixture("bytes"), limits(), Arc::new(AtomicU8::new(0)));
    assert_eq!(result.outcome, CommandOutcome::Exited { code: 7 });
    assert_eq!(result.stdout.bytes, b"\xff\0\x1b]52;c;fixture\x07\n");
    assert_eq!(result.stderr.bytes, b"stderr\xff\0");
    assert_eq!(result.stdout.completeness, CaptureCompleteness::Complete);
    assert_eq!(result.stderr.completeness, CaptureCompleteness::Complete);
    assert!(result.cleanup_confirmed);
}

#[test]
fn process_literal_arguments_and_launch_failure_are_distinct() {
    let mut command = fixture("argv");
    command.args(["a b", "$(not-a-command)", "'quoted'", "", "*.rs"]);
    let result = execute(command, limits(), Arc::new(AtomicU8::new(0)));
    let args: Vec<String> = serde_json::from_slice(&result.stdout.bytes).unwrap();
    assert_eq!(args, ["a b", "$(not-a-command)", "'quoted'", "", "*.rs"]);
    let result = execute(
        Command::new("/rustrace-fixture-does-not-exist"),
        limits(),
        Arc::new(AtomicU8::new(0)),
    );
    assert!(matches!(
        result.outcome,
        CommandOutcome::LaunchFailed { .. }
    ));
    assert_eq!(result.stdout.completeness, CaptureCompleteness::Unavailable);
    assert_eq!(result.stderr.completeness, CaptureCompleteness::Unavailable);
    assert!(result.cleanup_confirmed);
}

#[test]
fn process_huge_dual_output_cancels_at_shared_cap_without_lossy_capture() {
    let result = execute(fixture("huge"), limits(), Arc::new(AtomicU8::new(0)));
    assert!(matches!(
        result.outcome,
        CommandOutcome::Terminated {
            reason: CommandTermination::OutputLimit,
            ..
        }
    ));
    assert_eq!(
        result.stdout.bytes.len() + result.stderr.bytes.len(),
        limits().output_bytes
    );
    assert!(
        [&result.stdout, &result.stderr]
            .iter()
            .any(|s| s.completeness == CaptureCompleteness::Truncated)
    );
    for stream in [&result.stdout, &result.stderr] {
        assert!(
            stream
                .bytes
                .iter()
                .enumerate()
                .all(|(i, b)| *b == (i % 256) as u8)
        );
    }
    assert!(result.cleanup_confirmed);
}

#[test]
fn process_deadline_cancel_and_quit_have_distinct_bounded_outcomes() {
    for (cancel_value, expected) in [
        (0, CommandTermination::Deadline),
        (1, CommandTermination::Cancelled),
        (2, CommandTermination::Quit),
    ] {
        let cancel = Arc::new(AtomicU8::new(0));
        let signal = cancel.clone();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            signal.store(cancel_value, Ordering::Release);
        });
        let before = Instant::now();
        let result = execute(
            fixture("sleep"),
            ProcessLimits {
                deadline: Duration::from_millis(250),
                ..limits()
            },
            cancel,
        );
        sender.join().unwrap();
        assert!(before.elapsed() < Duration::from_secs(3));
        assert!(
            matches!(result.outcome, CommandOutcome::Terminated { reason, .. } if reason == expected)
        );
        assert!(result.cleanup_confirmed);
    }
}

#[cfg(unix)]
#[test]
fn process_own_descendants_and_inherited_pipes_cannot_hold_completion_open() {
    for mode in ["descendants", "retain"] {
        let pid_path = std::env::temp_dir().join(format!(
            "rustrace-runner-descendant-{}-{mode}",
            std::process::id()
        ));
        let mut command = fixture(mode);
        command.arg(&pid_path);
        let before = Instant::now();
        let result = execute(
            command,
            ProcessLimits {
                deadline: Duration::from_secs(2),
                ..limits()
            },
            Arc::new(AtomicU8::new(0)),
        );
        assert!(before.elapsed() < Duration::from_secs(5));
        assert!(result.cleanup_confirmed);
        let pid: i32 = std::str::from_utf8(&result.stdout.bytes)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(1);
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "own fixture descendant survived"
        );
        let _ = std::fs::remove_file(pid_path);
    }
}

#[cfg(unix)]
#[test]
fn escaped_pipe_holder_does_not_invent_owned_group_cleanup_uncertainty() {
    struct KillProcessGroup(i32);
    impl Drop for KillProcessGroup {
        fn drop(&mut self) {
            let _ = unsafe { libc::kill(-self.0, libc::SIGKILL) };
        }
    }

    let pid_path = std::env::temp_dir().join(format!(
        "rustrace-runner-escaped-pipe-{}",
        std::process::id()
    ));
    let mut command = fixture("escaped-pipe");
    command.arg(&pid_path);
    let before = Instant::now();
    let result = execute(command, limits(), Arc::new(AtomicU8::new(0)));
    let pid: i32 = fs::read_to_string(&pid_path).unwrap().parse().unwrap();
    let cleanup = KillProcessGroup(pid);

    assert!(before.elapsed() < Duration::from_secs(5));
    assert!(result.cleanup_confirmed);
    assert!(result.pending_cleanup.is_none());
    assert!(matches!(
        result.outcome,
        CommandOutcome::Terminated {
            reason: CommandTermination::CaptureFailure,
            ..
        }
    ));
    assert_eq!(result.stdout.completeness, CaptureCompleteness::ReadFailed);
    assert_eq!(result.stderr.completeness, CaptureCompleteness::ReadFailed);
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);

    drop(cleanup);
    let until = Instant::now() + Duration::from_secs(2);
    while unsafe { libc::kill(-pid, 0) } == 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_ne!(
        unsafe { libc::kill(-pid, 0) },
        0,
        "escaped fixture process group survived cleanup"
    );
    let _ = fs::remove_file(pid_path);
}
