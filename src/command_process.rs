//! Bounded owned child execution, with no shell or reader threads.

use rustrace_model::{CaptureCompleteness, CommandOutcome, CommandTermination};
use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Read, Write},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
        mpsc::{Receiver, TryRecvError},
    },
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::process::{Child, Stdio};

#[cfg(test)]
use std::{
    cell::RefCell,
    sync::atomic::{AtomicBool, AtomicI32},
};

#[cfg(all(test, unix))]
thread_local! {
    static PANIC_AFTER_SPAWN: RefCell<Option<Arc<AtomicI32>>> = const { RefCell::new(None) };
}

pub(crate) const MAX_LIVE_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_PENDING_STDIN_BYTES: usize = 64 * 1024;
const MAX_STDIN_LINE_BYTES: usize = 4096;

pub(crate) enum StdinMessage {
    Line(String),
    Eof,
}

pub(crate) enum ProcessStdin {
    Closed,
    File(File),
    Submitted(Receiver<StdinMessage>),
}

pub(crate) enum ProcessStdout {
    Captured,
    File(File),
}

pub(crate) struct ProcessIo {
    pub stdin: ProcessStdin,
    pub stdout: ProcessStdout,
    pub live: Option<LiveOutput>,
}

#[derive(Clone)]
pub(crate) struct LiveOutput {
    inner: Arc<Mutex<LiveBuffer>>,
    id: u64,
}

struct LiveBuffer {
    bytes: VecDeque<u8>,
    limit: usize,
    // Bytes ever pushed; the retained tail starts at `pushed - bytes.len()`.
    pushed: u64,
}

/// A live-output snapshot with a stable position: `id` names the command's
/// buffer and `start` is the absolute offset of `bytes[0]` within its output.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct LiveSnapshot {
    pub id: u64,
    pub start: u64,
    pub bytes: Vec<u8>,
}

impl LiveOutput {
    pub(crate) fn new(limit: usize) -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            inner: Arc::new(Mutex::new(LiveBuffer {
                bytes: VecDeque::with_capacity(limit.min(MAX_LIVE_OUTPUT_BYTES)),
                limit: limit.clamp(1, MAX_LIVE_OUTPUT_BYTES),
                pushed: 0,
            })),
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }

    fn push(&self, bytes: &[u8]) {
        let mut buffer = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let discard = buffer
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(buffer.limit);
        let discard = discard.min(buffer.bytes.len());
        buffer.bytes.drain(..discard);
        let keep_from = bytes.len().saturating_sub(buffer.limit);
        buffer.bytes.extend(&bytes[keep_from..]);
        buffer.pushed = buffer
            .pushed
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    }

    pub(crate) fn positioned_snapshot(&self) -> LiveSnapshot {
        let buffer = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        LiveSnapshot {
            id: self.id,
            start: buffer
                .pushed
                .saturating_sub(u64::try_from(buffer.bytes.len()).unwrap_or(u64::MAX)),
            bytes: buffer.bytes.iter().copied().collect(),
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<u8> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .bytes
            .iter()
            .copied()
            .collect()
    }
}

pub(crate) struct ProcessLimits {
    pub deadline: Duration,
    pub output_bytes: usize,
}

pub(crate) struct CapturedStream {
    pub bytes: Vec<u8>,
    pub completeness: CaptureCompleteness,
}

pub(crate) struct ProcessResult {
    pub outcome: CommandOutcome,
    pub stdout: CapturedStream,
    pub stderr: CapturedStream,
    /// The instant bounded execution and capture stopped. Later owned-process
    /// cleanup confirmation does not change this execution fact.
    pub completed_at: Instant,
    pub cleanup_confirmed: bool,
    pub cleanup_os_code: Option<i32>,
    pub pending_cleanup: Option<PendingProcessCleanup>,
}

pub(crate) struct PendingProcessCleanup {
    #[cfg(unix)]
    state: PendingProcessCleanupState,
    #[cfg(not(unix))]
    _unavailable: (),
}

#[cfg(unix)]
enum PendingProcessCleanupState {
    Background(Arc<Mutex<Option<CleanupProgress>>>),
    Complete(CleanupProgress),
    #[cfg(test)]
    Test {
        confirmation: Arc<AtomicBool>,
        dropped_while_armed: Option<Arc<AtomicBool>>,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct CleanupProgress {
    pub confirmed: bool,
    pub os_code: Option<i32>,
}

#[cfg(unix)]
struct OwnedProcessCleanup {
    child: Option<Child>,
    group: i32,
    armed: bool,
}

#[derive(Clone)]
pub(crate) struct ProcessCleanupHandoff {
    inner: Arc<ProcessCleanupHandoffInner>,
}

struct ProcessCleanupHandoffInner {
    state: Mutex<ProcessCleanupState>,
}

enum ProcessCleanupState {
    NotSpawned,
    #[cfg(unix)]
    Armed,
    #[cfg(unix)]
    Confirmed,
    #[cfg(unix)]
    Pending(PendingProcessCleanup),
    Consumed,
}

pub(crate) enum PanickedProcessCleanup {
    ConfirmedOrNotSpawned,
    #[cfg(unix)]
    Pending(PendingProcessCleanup),
    Ambiguous,
}

impl ProcessCleanupHandoff {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(ProcessCleanupHandoffInner {
                state: Mutex::new(ProcessCleanupState::NotSpawned),
            }),
        }
    }

    #[cfg(unix)]
    fn with_state(&self, update: impl FnOnce(&mut ProcessCleanupState)) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        update(&mut state);
    }

    #[cfg(unix)]
    fn arm(&self) {
        self.with_state(|state| *state = ProcessCleanupState::Armed);
    }

    #[cfg(unix)]
    fn confirm(&self) {
        self.with_state(|state| *state = ProcessCleanupState::Confirmed);
    }

    #[cfg(unix)]
    fn retain_pending(&self, cleanup: PendingProcessCleanup) {
        self.with_state(|state| *state = ProcessCleanupState::Pending(cleanup));
    }

    pub(crate) fn take_after_panic(&self) -> PanickedProcessCleanup {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match std::mem::replace(&mut *state, ProcessCleanupState::Consumed) {
            ProcessCleanupState::NotSpawned => PanickedProcessCleanup::ConfirmedOrNotSpawned,
            #[cfg(unix)]
            ProcessCleanupState::Confirmed => PanickedProcessCleanup::ConfirmedOrNotSpawned,
            #[cfg(unix)]
            ProcessCleanupState::Pending(cleanup) => PanickedProcessCleanup::Pending(cleanup),
            #[cfg(unix)]
            ProcessCleanupState::Armed | ProcessCleanupState::Consumed => {
                PanickedProcessCleanup::Ambiguous
            }
            #[cfg(not(unix))]
            ProcessCleanupState::Consumed => PanickedProcessCleanup::Ambiguous,
        }
    }
}

#[cfg(unix)]
struct SpawnedProcess {
    child: Option<Child>,
    group: i32,
    cleanup_handoff: ProcessCleanupHandoff,
    armed: bool,
}

#[cfg(unix)]
impl SpawnedProcess {
    fn new(child: Child, cleanup_handoff: ProcessCleanupHandoff) -> Self {
        let group = child.id() as i32;
        cleanup_handoff.arm();
        Self {
            child: Some(child),
            group,
            cleanup_handoff,
            armed: true,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("spawned child ownership")
    }

    fn confirm(&mut self) {
        self.cleanup_handoff.confirm();
        self.armed = false;
    }

    fn into_pending(mut self, leader_reaped: bool) -> PendingProcessCleanup {
        let child = (!leader_reaped).then(|| self.child.take().expect("unreaped child ownership"));
        let cleanup = PendingProcessCleanup::new(OwnedProcessCleanup::new(child, self.group));
        self.cleanup_handoff.retain_pending(cleanup.observer());
        self.armed = false;
        cleanup
    }
}

#[cfg(unix)]
impl Drop for SpawnedProcess {
    fn drop(&mut self) {
        if self.armed {
            // This runs on the process worker during unwind. Keeping the worker
            // live makes each caller poll bounded while retaining direct child
            // and process-group ownership until both are confirmed absent.
            let mut cleanup = OwnedProcessCleanup::new(self.child.take(), self.group);
            cleanup.confirm_blocking();
            self.cleanup_handoff.confirm();
            self.armed = false;
        }
    }
}

#[cfg(unix)]
impl OwnedProcessCleanup {
    fn new(child: Option<Child>, group: i32) -> Self {
        Self {
            child,
            group,
            armed: true,
        }
    }

    fn poll(&mut self) -> CleanupProgress {
        let mut os_code = None;
        let mut group_absent = false;
        if unsafe { libc::kill(-self.group, libc::SIGKILL) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                group_absent = true;
            } else {
                os_code = error.raw_os_error();
            }
        }
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(Some(_)) => self.child = None,
                Ok(None) => {}
                Err(error) => os_code = error.raw_os_error(),
            }
        }
        if !group_absent && unsafe { libc::kill(-self.group, 0) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                group_absent = true;
            } else {
                os_code = error.raw_os_error();
            }
        }
        let confirmed = self.child.is_none() && group_absent;
        if confirmed {
            self.armed = false;
        }
        CleanupProgress { confirmed, os_code }
    }

    fn confirm_blocking(&mut self) -> CleanupProgress {
        let mut os_code = None;
        while self.armed {
            let progress = self.poll();
            if progress.os_code.is_some() {
                os_code = progress.os_code;
            }
            if progress.confirmed {
                return CleanupProgress {
                    confirmed: true,
                    os_code,
                };
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        CleanupProgress {
            confirmed: true,
            os_code,
        }
    }
}

#[cfg(unix)]
impl Drop for OwnedProcessCleanup {
    fn drop(&mut self) {
        if self.armed {
            self.confirm_blocking();
        }
    }
}

impl PendingProcessCleanup {
    #[cfg(unix)]
    fn new(mut cleanup: OwnedProcessCleanup) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<OwnedProcessCleanup>(1);
        let completion = Arc::new(Mutex::new(None));
        let worker_completion = completion.clone();
        match std::thread::Builder::new()
            .name("rustrace-process-cleanup".into())
            .spawn(move || {
                let mut cleanup = receiver
                    .recv()
                    .expect("process cleanup ownership sender remains live");
                let progress = cleanup.confirm_blocking();
                *worker_completion
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(progress);
            }) {
            Ok(worker) => match sender.send(cleanup) {
                Ok(()) => {
                    drop(worker);
                    Self {
                        state: PendingProcessCleanupState::Background(completion),
                    }
                }
                Err(error) => {
                    cleanup = error.0;
                    let progress = cleanup.confirm_blocking();
                    let _ = worker.join();
                    Self {
                        state: PendingProcessCleanupState::Complete(progress),
                    }
                }
            },
            Err(_) => {
                // Thread creation failed before ownership moved. Confirm on the
                // process worker, which remains observable as Running.
                Self {
                    state: PendingProcessCleanupState::Complete(cleanup.confirm_blocking()),
                }
            }
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn for_test(confirmation: Arc<AtomicBool>) -> Self {
        Self {
            state: PendingProcessCleanupState::Test {
                confirmation,
                dropped_while_armed: None,
            },
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn for_test_with_drop_probe(
        confirmation: Arc<AtomicBool>,
        dropped_while_armed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            state: PendingProcessCleanupState::Test {
                confirmation,
                dropped_while_armed: Some(dropped_while_armed),
            },
        }
    }

    #[cfg(unix)]
    pub(crate) fn poll(&mut self) -> CleanupProgress {
        match &mut self.state {
            PendingProcessCleanupState::Background(completion) => {
                let progress = *completion.lock().unwrap_or_else(|error| error.into_inner());
                match progress {
                    Some(progress) => {
                        self.state = PendingProcessCleanupState::Complete(progress);
                        progress
                    }
                    None => CleanupProgress {
                        confirmed: false,
                        os_code: None,
                    },
                }
            }
            PendingProcessCleanupState::Complete(progress) => *progress,
            #[cfg(test)]
            PendingProcessCleanupState::Test { confirmation, .. } => CleanupProgress {
                confirmed: confirmation.load(Ordering::Acquire),
                os_code: None,
            },
        }
    }

    #[cfg(not(unix))]
    pub(crate) fn poll(&mut self) -> CleanupProgress {
        CleanupProgress {
            confirmed: false,
            os_code: None,
        }
    }

    #[cfg(unix)]
    fn observer(&self) -> Self {
        let state = match &self.state {
            PendingProcessCleanupState::Background(completion) => {
                PendingProcessCleanupState::Background(completion.clone())
            }
            PendingProcessCleanupState::Complete(progress) => {
                PendingProcessCleanupState::Complete(*progress)
            }
            #[cfg(test)]
            PendingProcessCleanupState::Test { .. } => {
                unreachable!("test cleanup is not created at the process spawn boundary")
            }
        };
        Self { state }
    }
}

#[cfg(unix)]
impl Drop for PendingProcessCleanup {
    fn drop(&mut self) {
        #[cfg(test)]
        if let PendingProcessCleanupState::Test {
            confirmation,
            dropped_while_armed,
        } = &self.state
            && !confirmation.load(Ordering::Acquire)
            && let Some(dropped) = dropped_while_armed
        {
            dropped.store(true, Ordering::Release);
        }
        // Background child/group ownership lives on the cleanup worker; this
        // observer can be dropped without blocking its caller or stopping it.
    }
}

fn launch_failed(error: io::Error) -> ProcessResult {
    ProcessResult {
        outcome: CommandOutcome::LaunchFailed {
            os_code: error.raw_os_error(),
        },
        stdout: CapturedStream {
            bytes: Vec::new(),
            completeness: CaptureCompleteness::Unavailable,
        },
        stderr: CapturedStream {
            bytes: Vec::new(),
            completeness: CaptureCompleteness::Unavailable,
        },
        completed_at: Instant::now(),
        cleanup_confirmed: true,
        cleanup_os_code: None,
        pending_cleanup: None,
    }
}

#[cfg(unix)]
struct Pipe<R> {
    reader: R,
    capture: CapturedStream,
    done: bool,
}

#[cfg(unix)]
impl<R: Read + std::os::fd::AsRawFd> Pipe<R> {
    fn new(reader: R) -> Self {
        let fd = reader.as_raw_fd();
        // These calls only change the parent's owned pipe descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        let failed = flags == -1
            || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1;
        Self {
            reader,
            capture: CapturedStream {
                bytes: Vec::new(),
                completeness: if failed {
                    CaptureCompleteness::ReadFailed
                } else {
                    CaptureCompleteness::Complete
                },
            },
            done: failed,
        }
    }

    fn drain_with_live(&mut self, remaining: &mut usize, live: Option<&LiveOutput>) {
        if self.done {
            return;
        }
        let mut buffer = [0_u8; 8192];
        // Bound each turn so a continuously writing stream cannot starve the
        // other stream, cancellation, child polling, or the deadline.
        for _ in 0..8 {
            match self.reader.read(&mut buffer) {
                Ok(0) => {
                    self.done = true;
                    break;
                }
                Ok(count) => {
                    let keep = count.min(*remaining);
                    self.capture.bytes.extend_from_slice(&buffer[..keep]);
                    if let Some(live) = live {
                        live.push(&buffer[..keep]);
                    }
                    *remaining -= keep;
                    if keep < count || *remaining == 0 {
                        self.capture.completeness = CaptureCompleteness::Truncated;
                        self.done = true;
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.capture.completeness = CaptureCompleteness::ReadFailed;
                    self.done = true;
                    break;
                }
            }
        }
    }
}

#[cfg(unix)]
struct SubmittedInput {
    receiver: Option<Receiver<StdinMessage>>,
    writer: Option<std::process::ChildStdin>,
    pending: VecDeque<u8>,
    eof: bool,
}

#[cfg(unix)]
impl SubmittedInput {
    fn new(receiver: Receiver<StdinMessage>, writer: std::process::ChildStdin) -> Self {
        use std::os::fd::AsRawFd;
        let fd = writer.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        let eof = flags == -1
            || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1;
        Self {
            receiver: (!eof).then_some(receiver),
            writer: (!eof).then_some(writer),
            pending: VecDeque::new(),
            eof,
        }
    }

    fn poll(&mut self) {
        if self.writer.is_none() {
            self.receiver = None;
            return;
        }
        for _ in 0..16 {
            let Some(receiver) = self.receiver.as_ref() else {
                break;
            };
            // Leave backpressure in the fixed-capacity channel unless any
            // valid next line is guaranteed to fit this bounded write buffer.
            if self.pending.len() > MAX_PENDING_STDIN_BYTES - (MAX_STDIN_LINE_BYTES + 1) {
                break;
            }
            match receiver.try_recv() {
                Ok(StdinMessage::Line(line))
                    if line.len() <= MAX_STDIN_LINE_BYTES
                        && !line
                            .chars()
                            .any(|character| matches!(character, '\n' | '\r')) =>
                {
                    self.pending.extend(line.bytes());
                    self.pending.push_back(b'\n');
                }
                Ok(StdinMessage::Line(_)) => {
                    self.eof = true;
                    self.receiver = None;
                    break;
                }
                Ok(StdinMessage::Eof) => {
                    self.eof = true;
                    self.receiver = None;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.eof = true;
                    self.receiver = None;
                    break;
                }
            }
        }
        if !self.pending.is_empty() {
            let (first, _) = self.pending.as_slices();
            let result = self.writer.as_mut().expect("checked writer").write(first);
            match result {
                Ok(0) => {
                    self.writer = None;
                    self.receiver = None;
                }
                Ok(count) => {
                    self.pending.drain(..count);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.pending.clear();
                    self.writer = None;
                    self.receiver = None;
                }
            }
        }
        if self.eof && self.pending.is_empty() {
            self.writer = None;
            self.receiver = None;
        }
    }
}

#[cfg(unix)]
pub(crate) fn execute(
    command: Command,
    limits: ProcessLimits,
    cancel: Arc<AtomicU8>,
) -> ProcessResult {
    execute_with_handoff(command, limits, cancel, ProcessCleanupHandoff::new())
}

#[cfg(unix)]
pub(crate) fn execute_with_handoff(
    command: Command,
    limits: ProcessLimits,
    cancel: Arc<AtomicU8>,
    cleanup_handoff: ProcessCleanupHandoff,
) -> ProcessResult {
    execute_with_io_and_handoff(
        command,
        limits,
        cancel,
        ProcessIo {
            stdin: ProcessStdin::Closed,
            stdout: ProcessStdout::Captured,
            live: None,
        },
        cleanup_handoff,
    )
}

#[cfg(all(test, unix))]
pub(crate) fn execute_with_io(
    command: Command,
    limits: ProcessLimits,
    cancel: Arc<AtomicU8>,
    io: ProcessIo,
) -> ProcessResult {
    execute_with_io_and_handoff(command, limits, cancel, io, ProcessCleanupHandoff::new())
}

#[cfg(unix)]
pub(crate) fn execute_with_io_and_handoff(
    mut command: Command,
    limits: ProcessLimits,
    cancel: Arc<AtomicU8>,
    io: ProcessIo,
    cleanup_handoff: ProcessCleanupHandoff,
) -> ProcessResult {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    if limits.deadline.is_zero()
        || limits.deadline > Duration::from_millis(rustrace_model::MAX_COMMAND_DEADLINE_MILLIS)
        || limits.output_bytes == 0
        || limits.output_bytes as u64 > rustrace_model::MAX_COMMAND_OUTPUT_BYTES
    {
        return launch_failed(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid process bounds",
        ));
    }
    if cancel.load(Ordering::Acquire) != 0 {
        let mut result = launch_failed(io::Error::new(
            io::ErrorKind::Interrupted,
            "cancelled before launch",
        ));
        result.outcome = CommandOutcome::Terminated {
            reason: if cancel.load(Ordering::Acquire) == 2 {
                CommandTermination::Quit
            } else {
                CommandTermination::Cancelled
            },
            signal: None,
        };
        return result;
    }
    let ProcessIo {
        stdin,
        stdout: stdout_mode,
        live,
    } = io;
    let (stdin, submitted) = match stdin {
        ProcessStdin::Closed => (Stdio::null(), None),
        ProcessStdin::File(file) => (Stdio::from(file), None),
        ProcessStdin::Submitted(receiver) => (Stdio::piped(), Some(receiver)),
    };
    command.stdin(stdin);
    let captured_stdout = matches!(stdout_mode, ProcessStdout::Captured);
    command.stdout(match stdout_mode {
        ProcessStdout::Captured => Stdio::piped(),
        ProcessStdout::File(file) => Stdio::from(file),
    });
    command.stderr(Stdio::piped()).process_group(0);
    let started = Instant::now();
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return launch_failed(error),
    };
    let mut process = SpawnedProcess::new(child, cleanup_handoff);
    let group = process.group;
    crate::session::process_probe("command-running");
    #[cfg(test)]
    PANIC_AFTER_SPAWN.with(|slot| {
        if let Some(spawned) = slot.borrow().as_ref() {
            spawned.store(group, Ordering::Release);
            panic!("injected panic after owned process spawn");
        }
    });
    let mut stdout = captured_stdout.then(|| {
        Pipe::new(
            process
                .child_mut()
                .stdout
                .take()
                .expect("requested stdout pipe"),
        )
    });
    let mut stderr = Pipe::new(
        process
            .child_mut()
            .stderr
            .take()
            .expect("requested stderr pipe"),
    );
    let mut submitted = submitted.map(|receiver| {
        SubmittedInput::new(
            receiver,
            process
                .child_mut()
                .stdin
                .take()
                .expect("requested stdin pipe"),
        )
    });
    let mut remaining = limits.output_bytes;
    let mut reason = None;
    let mut status = None;
    let mut cleanup_started = None;
    let mut cleanup_os_code = None;
    let mut group_absent = false;
    loop {
        if let Some(stdin) = &mut submitted {
            stdin.poll();
        }
        if let Some(stdout) = &mut stdout {
            stdout.drain_with_live(&mut remaining, live.as_ref());
        }
        stderr.drain_with_live(&mut remaining, live.as_ref());
        if status.is_none() {
            match process.child_mut().try_wait() {
                Ok(value) => status = value,
                Err(error) => {
                    cleanup_os_code = error.raw_os_error();
                    reason.get_or_insert(CommandTermination::CleanupFailure);
                }
            }
        }
        if reason.is_none() {
            reason = match cancel.load(Ordering::Acquire) {
                2 => Some(CommandTermination::Quit),
                1 => Some(CommandTermination::Cancelled),
                _ if remaining == 0 => Some(CommandTermination::OutputLimit),
                _ if stdout.as_ref().is_some_and(|stdout| {
                    stdout.capture.completeness == CaptureCompleteness::ReadFailed
                }) || stderr.capture.completeness == CaptureCompleteness::ReadFailed =>
                {
                    Some(CommandTermination::CaptureFailure)
                }
                _ if status.is_none() && started.elapsed() >= limits.deadline => {
                    Some(CommandTermination::Deadline)
                }
                _ => None,
            };
        }
        // Kill the owned group even after the leader exits: descendants may
        // still retain pipes or write files. Never target a caller's group.
        if cleanup_started.is_none() && (reason.is_some() || status.is_some()) {
            cleanup_started = Some(Instant::now());
            if unsafe { libc::kill(-group, libc::SIGKILL) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    group_absent = true;
                } else {
                    cleanup_os_code = error.raw_os_error();
                    reason = Some(CommandTermination::CleanupFailure);
                }
            }
        }
        if cleanup_started.is_some() && !group_absent {
            // Pipe EOF and capture health are independent of owned-group
            // absence: an escaped, out-of-scope process may retain a pipe.
            if unsafe { libc::kill(-group, 0) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    group_absent = true;
                } else {
                    cleanup_os_code = error.raw_os_error();
                    reason = Some(CommandTermination::CleanupFailure);
                }
            }
        }
        if status.is_some()
            && group_absent
            && stdout.as_ref().is_none_or(|stdout| stdout.done)
            && stderr.done
        {
            break;
        }
        if cleanup_started.is_some_and(|at| {
            at.elapsed() >= Duration::from_millis(rustrace_model::MAX_COMMAND_CLEANUP_MILLIS)
        }) {
            let reap_confirmed = status.is_some() && group_absent;
            if let Some(stdout) = &mut stdout
                && !stdout.done
            {
                stdout.capture.completeness = CaptureCompleteness::ReadFailed;
            }
            if !stderr.done {
                stderr.capture.completeness = CaptureCompleteness::ReadFailed;
            }
            if reap_confirmed {
                reason.get_or_insert(CommandTermination::CaptureFailure);
            } else {
                reason = Some(CommandTermination::CleanupFailure);
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let cleanup_confirmed = status.is_some() && group_absent;
    let signal = status.as_ref().and_then(|value| value.signal());
    let outcome = if let Some(reason) = reason {
        CommandOutcome::Terminated { reason, signal }
    } else if let Some(code) = status.as_ref().and_then(|value| value.code()) {
        CommandOutcome::Exited { code }
    } else {
        CommandOutcome::Terminated {
            reason: CommandTermination::Signal,
            signal,
        }
    };
    let pending_cleanup = if cleanup_confirmed {
        process.confirm();
        None
    } else {
        Some(process.into_pending(status.is_some()))
    };
    ProcessResult {
        outcome,
        stdout: stdout.map_or(
            CapturedStream {
                bytes: Vec::new(),
                completeness: CaptureCompleteness::Unavailable,
            },
            |stdout| stdout.capture,
        ),
        stderr: stderr.capture,
        completed_at: Instant::now(),
        cleanup_confirmed,
        cleanup_os_code,
        pending_cleanup,
    }
}

#[cfg(all(test, unix))]
pub(crate) fn execute_panicking_after_spawn_for_test(
    command: Command,
    limits: ProcessLimits,
    cancel: Arc<AtomicU8>,
    spawned: Arc<AtomicI32>,
    cleanup_handoff: ProcessCleanupHandoff,
) -> ProcessResult {
    PANIC_AFTER_SPAWN.with(|slot| {
        assert!(slot.borrow().is_none(), "nested process panic injection");
        *slot.borrow_mut() = Some(spawned);
    });
    execute_with_handoff(command, limits, cancel, cleanup_handoff)
}

#[cfg(not(unix))]
pub(crate) fn execute(
    _command: Command,
    _limits: ProcessLimits,
    _cancel: Arc<AtomicU8>,
) -> ProcessResult {
    launch_failed(io::Error::new(
        io::ErrorKind::Unsupported,
        "owned process groups require Unix",
    ))
}

#[cfg(not(unix))]
pub(crate) fn execute_with_io(
    _command: Command,
    _limits: ProcessLimits,
    _cancel: Arc<AtomicU8>,
    _io: ProcessIo,
) -> ProcessResult {
    launch_failed(io::Error::new(
        io::ErrorKind::Unsupported,
        "owned process groups require Unix",
    ))
}

#[cfg(not(unix))]
pub(crate) fn execute_with_handoff(
    command: Command,
    limits: ProcessLimits,
    cancel: Arc<AtomicU8>,
    _cleanup_handoff: ProcessCleanupHandoff,
) -> ProcessResult {
    execute(command, limits, cancel)
}

#[cfg(not(unix))]
pub(crate) fn execute_with_io_and_handoff(
    command: Command,
    limits: ProcessLimits,
    cancel: Arc<AtomicU8>,
    io: ProcessIo,
    _cleanup_handoff: ProcessCleanupHandoff,
) -> ProcessResult {
    execute_with_io(command, limits, cancel, io)
}

#[cfg(test)]
#[path = "command_process_tests.rs"]
mod tests;
