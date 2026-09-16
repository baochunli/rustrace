//! Bounded optional production rust-analyzer lifecycle.
//!
//! The service observes committed editor state. It has no journal appender,
//! command ownership, source mutation authority, or completion/diagnostic UI.

use crate::command_process::{self, ProcessLimits};
use crate::toolchain::{self, ProbeStatus, ToolchainReport};
use crate::{CommandExecution, cargo_policy};
use rustrace_editor::position::{LSP_POSITION_ENCODING, Utf16Position};
use rustrace_model::{
    CommandOutcome, CommandTermination, DocumentId, MAX_INSERTED_TEXT_BYTES, MAX_STRING_BYTES,
    WorkspacePath,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAX_MESSAGE_BYTES: usize = crate::rust_analyzer_spike::MAX_MESSAGE_BYTES;
const MAX_INCOMING_MESSAGES: usize = MAX_MESSAGES_PER_POLL;
const MAX_OUTGOING_MESSAGES: usize = 4;
const MAX_PROTOCOL_OUTBOX: usize =
    rustrace_workspace::hash::MAX_WORKSPACE_FILES + MAX_PENDING_REQUESTS + 16;
const MAX_MESSAGES_PER_POLL: usize = 16;
const MAX_PENDING_REQUESTS: usize = 64;
const MAX_CONFIGURATION_ITEMS: usize = 64;
pub(crate) const MAX_COMPLETION_ITEMS: usize = 16;
pub(crate) const MAX_LIVE_DIAGNOSTICS_PER_DOCUMENT: usize = 64;
pub(crate) const MAX_LIVE_DIAGNOSTIC_MESSAGE_BYTES: usize = 1024;
const MAX_DIAGNOSTIC_DROP_LOGS: usize = 8;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_JSON_DEPTH: usize = 16;
const MAX_JSON_VALUES: usize = 8 * 1024;
const MAX_JSON_ITEMS: usize = 1024;
const MAX_JSON_STRING_BYTES: usize = rustrace_workspace::hash::MAX_WORKSPACE_FILE_BYTES as usize;
const MAX_JSON_KEY_BYTES: usize = 4 * 1024;
const RESOLUTION_DEADLINE: Duration = Duration::from_secs(30);
const PROBE_DEADLINE: Duration = Duration::from_secs(3);
const INITIALIZE_DEADLINE: Duration = Duration::from_secs(30);
const RELOAD_DEADLINE: Duration = Duration::from_secs(30);
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(2);
pub(crate) const COMPLETION_DEADLINE: Duration = Duration::from_secs(3);
const CLEANUP_DEADLINE: Duration = Duration::from_secs(2);
const HEALTHY_RESET: Duration = Duration::from_secs(30);
const MAX_AUTOMATIC_RETRIES: u8 = 5;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DocumentState {
    pub document_id: DocumentId,
    pub path: WorkspacePath,
    pub version: u64,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompletionRequest {
    pub generation: u64,
    pub request_sequence: u64,
    pub document_id: DocumentId,
    pub path: WorkspacePath,
    pub version: u64,
    pub position_byte: u64,
    pub position: Utf16Position,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CompletionEdit {
    Insert(String),
    Replace {
        start: Utf16Position,
        end: Utf16Position,
        new_text: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompletionItem {
    pub label: String,
    pub kind: Option<u8>,
    pub edit: CompletionEdit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompletionResponse {
    pub request: CompletionRequest,
    pub items: Vec<CompletionItem>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LiveDiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LiveDiagnostic {
    pub start: Utf16Position,
    pub end: Utf16Position,
    pub severity: LiveDiagnosticSeverity,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublishedDiagnostics {
    pub generation: u64,
    pub document_id: DocumentId,
    pub path: WorkspacePath,
    pub version: u64,
    pub diagnostics: Vec<LiveDiagnostic>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ResolutionRecord {
    pub generation: u64,
    pub report: ToolchainReport,
    pub launch_available: bool,
    pub detail: String,
}

#[derive(Clone, Debug)]
struct LaunchSpec {
    root: PathBuf,
    rustup: PathBuf,
    selection: String,
    analyzer: PathBuf,
    cargo: PathBuf,
    rustc: PathBuf,
    rustdoc: PathBuf,
}

struct Resolution {
    generation: u64,
    report: ToolchainReport,
    launch: Result<LaunchSpec, String>,
    cleanup_confirmed: bool,
}

struct ResolveJob {
    cancel: Arc<AtomicU8>,
    thread: JoinHandle<Resolution>,
}

impl ResolveJob {
    fn start(generation: u64, root: PathBuf, pin: String) -> io::Result<Self> {
        let cancel = Arc::new(AtomicU8::new(0));
        let signal = Arc::clone(&cancel);
        let thread = thread::Builder::new()
            .name("rustrace-lsp-resolve".into())
            .spawn(move || resolve(generation, root, pin, signal))?;
        Ok(Self { cancel, thread })
    }

    fn cancel(&self) {
        self.cancel.store(2, Ordering::Release);
    }

    fn cancel_and_join(self) -> bool {
        self.cancel();
        // Every probe owns a finite execution and cleanup bound. Joining here
        // reaps that bounded worker instead of racing it with a duplicate
        // outer cutoff and detaching it while it may still access the root.
        self.thread
            .join()
            .is_ok_and(|resolution| resolution.cleanup_confirmed)
    }
}

fn resolve(generation: u64, root: PathBuf, pin: String, cancel: Arc<AtomicU8>) -> Resolution {
    use std::cell::Cell;

    let cleanup_uncertain = Cell::new(false);
    let captures = Cell::new(0_usize);
    let rustup = toolchain::find_rustup(&root);
    let until = Instant::now() + RESOLUTION_DEADLINE;
    let report = toolchain::discover_language_server_with_executor(
        &root,
        Some(&pin),
        &rustup,
        64 * 1024,
        &|command| {
            if cancel.load(Ordering::Acquire) != 0 {
                return CommandExecution::Failed {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: "language-server resolution cancelled".into(),
                };
            }
            captures.set(captures.get().saturating_add(1));
            let deadline = until
                .saturating_duration_since(Instant::now())
                .min(PROBE_DEADLINE);
            let mut result = command_process::execute(
                command,
                ProcessLimits {
                    deadline,
                    output_bytes: 64 * 1024,
                },
                Arc::clone(&cancel),
            );
            let inconsistent_pending = result.cleanup_confirmed && result.pending_cleanup.is_some();
            if let Some(mut cleanup) = result.pending_cleanup.take() {
                cancel.store(1, Ordering::Release);
                if inconsistent_pending {
                    result.cleanup_confirmed = false;
                    result.outcome = CommandOutcome::Terminated {
                        reason: CommandTermination::CleanupFailure,
                        signal: None,
                    };
                }
                loop {
                    let progress = cleanup.poll();
                    if progress.os_code.is_some() {
                        result.cleanup_os_code = progress.os_code;
                    }
                    if progress.confirmed {
                        result.cleanup_confirmed = true;
                        break;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
            }
            cleanup_uncertain.set(cleanup_uncertain.get() || !result.cleanup_confirmed);
            if !result.cleanup_confirmed {
                cancel.store(1, Ordering::Release);
            }
            let complete = result.stdout.completeness
                == rustrace_model::CaptureCompleteness::Complete
                && result.stderr.completeness == rustrace_model::CaptureCompleteness::Complete;
            let stdout = String::from_utf8(result.stdout.bytes);
            let stderr = String::from_utf8(result.stderr.bytes);
            match (result.outcome, stdout, stderr) {
                (CommandOutcome::Exited { code: 0 }, Ok(stdout), Ok(stderr))
                    if complete && !cleanup_uncertain.get() =>
                {
                    CommandExecution::Succeeded { stdout, stderr }
                }
                (
                    CommandOutcome::LaunchFailed {
                        os_code: Some(libc::ENOENT),
                    },
                    _,
                    _,
                ) => CommandExecution::NotFound,
                (
                    CommandOutcome::Terminated {
                        reason: CommandTermination::Deadline,
                        ..
                    },
                    stdout,
                    stderr,
                ) => CommandExecution::TimedOut {
                    timeout: deadline,
                    stdout: stdout.unwrap_or_default(),
                    stderr: stderr.unwrap_or_else(|_| "invalid UTF-8 probe output".into()),
                },
                (_, stdout, stderr) => CommandExecution::Failed {
                    exit_code: None,
                    stdout: stdout.unwrap_or_default(),
                    stderr: stderr.unwrap_or_else(|_| "invalid UTF-8 probe output".into()),
                },
            }
        },
    );
    let launch = launch_spec(&root, &rustup, &report).and_then(|launch| {
        if cleanup_uncertain.get() {
            Err("cleanup was not confirmed during language-server resolution".into())
        } else if captures.get() == 0 {
            Err("language-server resolution executed no bounded probes".into())
        } else {
            Ok(launch)
        }
    });
    Resolution {
        generation,
        report,
        launch,
        cleanup_confirmed: !cleanup_uncertain.get(),
    }
}

fn launch_spec(root: &Path, rustup: &Path, report: &ToolchainReport) -> Result<LaunchSpec, String> {
    if report.has_blockers() {
        return Err("required selected-tool resolution failed".into());
    }
    let selection = report
        .selected_toolchain
        .clone()
        .ok_or_else(|| "selected toolchain is unavailable".to_owned())?;
    let path = |component: &str| {
        report
            .probes
            .iter()
            .find(|probe| {
                probe.component == component
                    && probe.purpose == "resolve"
                    && probe.status == ProbeStatus::Available
            })
            .map(|probe| PathBuf::from(probe.stdout.trim()))
            .ok_or_else(|| format!("selected {component} is unavailable"))
    };
    Ok(LaunchSpec {
        root: root.to_path_buf(),
        rustup: rustup.to_path_buf(),
        selection,
        analyzer: path("rust-analyzer")?,
        cargo: path("cargo")?,
        rustc: path("rustc")?,
        rustdoc: path("rustdoc")?,
    })
}

enum TransportEvent {
    Message(Value),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SendOutcome {
    Sent,
    Full,
}

struct LaunchFailure {
    cleanup_confirmed: bool,
}

#[cfg(unix)]
struct LaunchGuard {
    child: Option<Child>,
    group: i32,
}

#[cfg(unix)]
impl LaunchGuard {
    fn new(child: Child) -> Self {
        let group = child.id() as i32;
        Self {
            child: Some(child),
            group,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("armed language-server launch")
    }

    fn failure(&mut self) -> LaunchFailure {
        LaunchFailure {
            cleanup_confirmed: self.cleanup(),
        }
    }

    fn disarm(mut self) -> Child {
        self.child.take().expect("armed language-server launch")
    }

    fn cleanup(&mut self) -> bool {
        let Some(child) = &mut self.child else {
            return true;
        };
        let mut group_signalled = true;
        unsafe {
            if libc::kill(-self.group, libc::SIGKILL) != 0
                && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
            {
                group_signalled = false;
            }
        }
        let until = Instant::now() + CLEANUP_DEADLINE;
        while Instant::now() < until {
            let leader_reaped = match child.try_wait() {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(_) => return false,
            };
            let group_gone = unsafe { libc::kill(-self.group, 0) } != 0
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if leader_reaped && group_gone {
                self.child.take();
                return group_signalled;
            }
            thread::sleep(Duration::from_millis(2));
        }
        false
    }
}

#[cfg(unix)]
impl Drop for LaunchGuard {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

struct Transport {
    child: Child,
    group: i32,
    outgoing: Option<mpsc::SyncSender<Value>>,
    incoming: Option<mpsc::Receiver<TransportEvent>>,
    fault: Arc<AtomicU8>,
    stderr: Arc<Mutex<Vec<u8>>>,
    threads: Vec<JoinHandle<()>>,
}

impl Transport {
    #[cfg(unix)]
    fn launch(spec: &LaunchSpec) -> Result<Self, LaunchFailure> {
        Self::launch_with_setup(spec, |_| Ok(()))
    }

    #[cfg(unix)]
    fn launch_with_setup(
        spec: &LaunchSpec,
        after_spawn: impl FnOnce(i32) -> Result<(), ()>,
    ) -> Result<Self, LaunchFailure> {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new(&spec.rustup);
        cargo_policy::retain_execution_environment(&mut command);
        command
            .args(["run", spec.selection.as_str()])
            .arg(&spec.analyzer)
            .current_dir(&spec.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env("RUSTUP_TOOLCHAIN", &spec.selection)
            .env("CARGO", &spec.cargo)
            .env("RUSTC", &spec.rustc)
            .env("RUSTDOC", &spec.rustdoc)
            .env("RUSTC_WRAPPER", "")
            .env("RUSTC_WORKSPACE_WRAPPER", "")
            .env("CARGO_ENCODED_RUSTFLAGS", "")
            .env("CARGO_ENCODED_RUSTDOCFLAGS", "")
            .env("CARGO_TARGET_DIR", spec.root.join("target"))
            .env("CARGO_BUILD_BUILD_DIR", "target")
            .env("CARGO_TERM_COLOR", "never")
            .env("CARGO_TERM_PROGRESS_WHEN", "never");
        let child = command.spawn().map_err(|_| LaunchFailure {
            cleanup_confirmed: true,
        })?;
        let mut guard = LaunchGuard::new(child);
        let group = guard.group;
        if after_spawn(group).is_err() {
            return Err(guard.failure());
        }
        let Some(stdin) = guard.child_mut().stdin.take() else {
            return Err(guard.failure());
        };
        let Some(mut stdout) = guard.child_mut().stdout.take() else {
            return Err(guard.failure());
        };
        let Some(mut child_stderr) = guard.child_mut().stderr.take() else {
            return Err(guard.failure());
        };
        let (outgoing, outbound) = mpsc::sync_channel::<Value>(MAX_OUTGOING_MESSAGES);
        let (inbound, incoming) = mpsc::sync_channel(MAX_INCOMING_MESSAGES);
        let fault = Arc::new(AtomicU8::new(0));
        let stderr = Arc::new(Mutex::new(Vec::new()));

        let writer_fault = Arc::clone(&fault);
        let writer = thread::Builder::new()
            .name("rustrace-lsp-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                while let Ok(message) = outbound.recv() {
                    if crate::rust_analyzer_spike::write_frame(&mut stdin, &message).is_err() {
                        writer_fault.store(2, Ordering::Release);
                        break;
                    }
                }
            })
            .map_err(|_| guard.failure())?;
        let reader_fault = Arc::clone(&fault);
        let reader = thread::Builder::new()
            .name("rustrace-lsp-reader".into())
            .spawn(move || read_incoming(&mut stdout, &inbound, &reader_fault))
            .map_err(|_| guard.failure())?;
        let stderr_fault = Arc::clone(&fault);
        let stderr_bytes = Arc::clone(&stderr);
        let stderr_reader = thread::Builder::new()
            .name("rustrace-lsp-stderr".into())
            .spawn(move || {
                let mut buffer = [0_u8; 4096];
                loop {
                    match child_stderr.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            let Ok(mut retained) = stderr_bytes.lock() else {
                                stderr_fault.store(6, Ordering::Release);
                                break;
                            };
                            if retained.len().saturating_add(count) > MAX_STDERR_BYTES {
                                stderr_fault.store(7, Ordering::Release);
                                break;
                            }
                            retained.extend_from_slice(&buffer[..count]);
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            stderr_fault.store(8, Ordering::Release);
                            break;
                        }
                    }
                }
            })
            .map_err(|_| guard.failure())?;
        let child = guard.disarm();
        Ok(Self {
            child,
            group,
            outgoing: Some(outgoing),
            incoming: Some(incoming),
            fault,
            stderr,
            threads: vec![writer, reader, stderr_reader],
        })
    }

    #[cfg(not(unix))]
    fn launch(_spec: &LaunchSpec) -> Result<Self, LaunchFailure> {
        Err(LaunchFailure {
            cleanup_confirmed: true,
        })
    }

    fn send(&self, message: Value) -> Result<SendOutcome, String> {
        let Some(outgoing) = &self.outgoing else {
            return Err("language-server transport is stopping".into());
        };
        match outgoing.try_send(message) {
            Ok(()) => Ok(SendOutcome::Sent),
            Err(mpsc::TrySendError::Full(_)) => Ok(SendOutcome::Full),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                Err("language-server writer stopped".into())
            }
        }
    }

    fn receive(&self) -> Option<TransportEvent> {
        self.incoming.as_ref()?.try_recv().ok()
    }

    fn check(&mut self) -> Result<bool, String> {
        if self.fault.load(Ordering::Acquire) != 0 {
            return Err("language-server transport failed or exceeded a bound".into());
        }
        self.child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|error| format!("language-server status failed: {error}"))
    }

    fn cleanup(&mut self) -> bool {
        self.outgoing.take();
        self.incoming.take();
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.group, libc::SIGKILL);
        }
        let until = Instant::now() + CLEANUP_DEADLINE;
        let mut leader_reaped = false;
        let mut group_gone = false;
        while Instant::now() < until {
            leader_reaped = match self.child.try_wait() {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(_) => return false,
            };
            #[cfg(unix)]
            {
                group_gone = unsafe { libc::kill(-self.group, 0) } != 0
                    && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            }
            #[cfg(not(unix))]
            {
                group_gone = leader_reaped;
            }
            let pipes_closed = self.threads.iter().all(JoinHandle::is_finished);
            if leader_reaped && pipes_closed && group_gone {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let pipes_closed = self.threads.iter().all(JoinHandle::is_finished);
        for thread in self.threads.drain(..) {
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
        let _ = self.stderr.lock().map(|bytes| bytes.len());
        leader_reaped && pipes_closed && group_gone
    }
}

fn read_incoming(
    stdout: &mut impl Read,
    inbound: &mpsc::SyncSender<TransportEvent>,
    fault: &AtomicU8,
) {
    loop {
        match crate::rust_analyzer_spike::read_frame(stdout, MAX_MESSAGE_BYTES) {
            Ok(Some(message)) => {
                if !send_incoming(inbound, message) {
                    break;
                }
            }
            Ok(None) => {
                fault.store(4, Ordering::Release);
                break;
            }
            Err(_) => {
                fault.store(5, Ordering::Release);
                break;
            }
        }
    }
}

fn send_incoming(inbound: &mpsc::SyncSender<TransportEvent>, message: Value) -> bool {
    inbound.send(TransportEvent::Message(message)).is_ok()
}

impl Drop for Transport {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PendingKind {
    Initialize,
    Reload,
    Shutdown,
    Completion(CompletionRequest),
}

#[derive(Clone, Debug)]
struct PendingRequest {
    generation: u64,
    kind: PendingKind,
    deadline: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProtocolState {
    Initializing,
    Ready,
    Stopping,
    ExitSent,
}

#[derive(Clone, Debug)]
struct SyncedDocument {
    path: WorkspacePath,
    version: u64,
    text: String,
}

struct Protocol {
    generation: u64,
    root_uri: String,
    root_name: String,
    state: ProtocolState,
    next_id: i64,
    pending: BTreeMap<i64, PendingRequest>,
    outbox: VecDeque<Value>,
    documents: BTreeMap<DocumentId, SyncedDocument>,
    completions: VecDeque<CompletionResponse>,
    diagnostics: VecDeque<PublishedDiagnostics>,
    diagnostic_drop_logs: usize,
    ready_since: Option<Instant>,
}

impl Protocol {
    fn new(generation: u64, root: &Path, now: Instant) -> Result<Self, String> {
        let root_uri = file_uri(root)?;
        let root_name = root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace")
            .to_owned();
        let mut protocol = Self {
            generation,
            root_uri,
            root_name,
            state: ProtocolState::Initializing,
            next_id: 1,
            pending: BTreeMap::new(),
            outbox: VecDeque::new(),
            documents: BTreeMap::new(),
            completions: VecDeque::new(),
            diagnostics: VecDeque::new(),
            diagnostic_drop_logs: 0,
            ready_since: None,
        };
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "rustrace", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": protocol.root_uri,
            "workspaceFolders": [{"uri": protocol.root_uri, "name": protocol.root_name}],
            "capabilities": {
                "general": {"positionEncodings": [LSP_POSITION_ENCODING]},
                "textDocument": {"publishDiagnostics": {"versionSupport": true}},
                "workspace": {"workspaceFolders": true, "configuration": true}
            },
            "initializationOptions": initialization_options(),
            "trace": "off"
        });
        protocol.request(
            PendingKind::Initialize,
            "initialize",
            params,
            now + INITIALIZE_DEADLINE,
        )?;
        Ok(protocol)
    }

    fn request(
        &mut self,
        kind: PendingKind,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<i64, String> {
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            return Err("language-server pending-request limit reached".into());
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or("language-server request ID overflow")?;
        if self
            .pending
            .insert(
                id,
                PendingRequest {
                    generation: self.generation,
                    kind,
                    deadline,
                },
            )
            .is_some()
        {
            return Err("duplicate language-server request ID".into());
        }
        self.enqueue(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        Ok(id)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.enqueue(json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    fn enqueue(&mut self, message: Value) -> Result<(), String> {
        validate_json(&message)?;
        if serde_json::to_vec(&message)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_MESSAGE_BYTES
        {
            return Err("outgoing language-server message exceeds byte limit".into());
        }
        if self.outbox.len() >= MAX_PROTOCOL_OUTBOX {
            return Err("language-server protocol outbox limit reached".into());
        }
        self.outbox.push_back(message);
        Ok(())
    }

    fn poll(
        &mut self,
        transport: &mut Transport,
        documents: &[DocumentState],
        writes_allowed: bool,
        now: Instant,
    ) -> Result<bool, String> {
        let mut changed = self.flush(transport, writes_allowed)?;
        for _ in 0..MAX_MESSAGES_PER_POLL {
            let Some(TransportEvent::Message(message)) = transport.receive() else {
                break;
            };
            validate_json(&message)?;
            changed = true;
            self.handle(message, now)?;
        }
        if writes_allowed && self.state == ProtocolState::Ready {
            self.reconcile_documents(documents)?;
        }
        self.check_deadlines(now)?;
        changed |= self.flush(transport, writes_allowed)?;
        Ok(changed)
    }

    fn flush(&mut self, transport: &Transport, writes_allowed: bool) -> Result<bool, String> {
        if !writes_allowed {
            return Ok(false);
        }
        let mut sent = false;
        while let Some(message) = self.outbox.pop_front() {
            match transport.send(message.clone())? {
                SendOutcome::Sent => sent = true,
                SendOutcome::Full => {
                    self.outbox.push_front(message);
                    break;
                }
            }
        }
        Ok(sent)
    }

    fn handle(&mut self, message: Value, now: Instant) -> Result<(), String> {
        let object = message
            .as_object()
            .ok_or("language-server message is not an object")?;
        if object.get("jsonrpc") != Some(&Value::String("2.0".into())) {
            return Err("language-server message has an invalid JSON-RPC version".into());
        }
        if let Some(method) = object.get("method").and_then(Value::as_str) {
            if let Some(id) = object.get("id") {
                self.handle_server_request(id.clone(), method, object.get("params"))?;
            } else if method == "textDocument/publishDiagnostics" {
                self.handle_publish_diagnostics(object.get("params"));
            }
            // Other notifications are intentionally drained without becoming
            // product state.
            return Ok(());
        }
        let Some(id) = object.get("id").and_then(Value::as_i64) else {
            // Unknown, stale, string, null and non-integral response IDs have
            // no current request authority.
            return Ok(());
        };
        let Some(pending) = self.pending.remove(&id) else {
            return Ok(());
        };
        if pending.generation != self.generation {
            return Ok(());
        }
        if now >= pending.deadline
            && let PendingKind::Completion(request) = &pending.kind
        {
            self.push_completion(request.clone(), Vec::new());
            return Ok(());
        }
        if object.contains_key("error") {
            return match pending.kind {
                PendingKind::Completion(request) => {
                    self.push_completion(request, Vec::new());
                    Ok(())
                }
                kind => Err(format!("language-server {kind:?} request failed")),
            };
        }
        match pending.kind {
            PendingKind::Initialize => {
                if self.state != ProtocolState::Initializing {
                    return Ok(());
                }
                let result = object
                    .get("result")
                    .and_then(Value::as_object)
                    .ok_or("initialize response result is not an object")?;
                let capabilities = result
                    .get("capabilities")
                    .and_then(Value::as_object)
                    .ok_or("initialize response capabilities are not an object")?;
                let encoding = capabilities
                    .get("positionEncoding")
                    .and_then(Value::as_str)
                    .unwrap_or(LSP_POSITION_ENCODING);
                if encoding != LSP_POSITION_ENCODING {
                    return Err("language server selected an unsupported position encoding".into());
                }
                validate_text_sync(capabilities.get("textDocumentSync"))?;
                self.notify("initialized", json!({}))?;
                self.state = ProtocolState::Ready;
                self.ready_since = Some(now);
            }
            PendingKind::Reload => {
                if object.get("result") != Some(&Value::Null) {
                    return Err("language-server reload returned a non-null result".into());
                }
            }
            PendingKind::Shutdown => {
                if self.state == ProtocolState::Stopping {
                    if object.get("result") != Some(&Value::Null) {
                        return Err("language-server shutdown returned a non-null result".into());
                    }
                    self.notify("exit", Value::Null)?;
                    self.state = ProtocolState::ExitSent;
                }
            }
            PendingKind::Completion(request) => {
                let items = parse_completion_result(object.get("result"));
                self.push_completion(request, items);
            }
        }
        Ok(())
    }

    fn push_completion(&mut self, request: CompletionRequest, items: Vec<CompletionItem>) {
        if self.completions.len() == MAX_PENDING_REQUESTS {
            self.completions.pop_front();
        }
        self.completions
            .push_back(CompletionResponse { request, items });
    }

    fn handle_publish_diagnostics(&mut self, params: Option<&Value>) {
        let Some((uri, version, values)) = parse_publish_diagnostics_params(params) else {
            self.log_dropped_diagnostic();
            return;
        };
        let Some((document_id, document)) = self.documents.iter().find(|(_, document)| {
            document.version == version && document_uri(&self.root_uri, &document.path) == uri
        }) else {
            return;
        };
        let Some(diagnostics) = parse_live_diagnostics(values) else {
            self.log_dropped_diagnostic();
            return;
        };
        let document_id = document_id.clone();
        self.diagnostics
            .retain(|published| published.document_id != document_id);
        if self.diagnostics.len() == rustrace_workspace::hash::MAX_WORKSPACE_FILES {
            self.diagnostics.pop_front();
        }
        self.diagnostics.push_back(PublishedDiagnostics {
            generation: self.generation,
            document_id,
            path: document.path.clone(),
            version,
            diagnostics,
        });
    }

    fn log_dropped_diagnostic(&mut self) {
        if self.diagnostic_drop_logs < MAX_DIAGNOSTIC_DROP_LOGS {
            eprintln!("rustrace: dropped malformed publishDiagnostics notification");
            self.diagnostic_drop_logs += 1;
        }
    }

    fn handle_server_request(
        &mut self,
        id: Value,
        method: &str,
        params: Option<&Value>,
    ) -> Result<(), String> {
        if !matches!(id, Value::String(_) | Value::Number(_)) {
            return Ok(());
        }
        let result = match method {
            "workspace/configuration" => {
                let count = params
                    .and_then(|value| value.get("items"))
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                if count > MAX_CONFIGURATION_ITEMS {
                    return Err("language-server configuration request exceeds item limit".into());
                }
                Some(Value::Array(
                    (0..count).map(|_| initialization_options()).collect(),
                ))
            }
            "workspace/workspaceFolders" => Some(json!([{
                "uri": self.root_uri,
                "name": self.root_name
            }])),
            "window/workDoneProgress/create" | "window/showMessageRequest" => Some(Value::Null),
            _ => None,
        };
        match result {
            Some(result) => self.enqueue(json!({"jsonrpc": "2.0", "id": id, "result": result})),
            None => self.enqueue(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "unsupported by rustrace"}
            })),
        }
    }

    fn reconcile_documents(&mut self, desired: &[DocumentState]) -> Result<(), String> {
        let desired = desired
            .iter()
            .filter(|document| supports_document(&document.path))
            .map(|document| (document.document_id.clone(), document))
            .collect::<BTreeMap<_, _>>();
        let mut close = Vec::new();
        for (id, current) in &self.documents {
            if desired.get(id).is_none_or(|next| next.path != current.path) {
                close.push((id.clone(), current.path.clone()));
            }
        }
        for (id, path) in close {
            self.notify(
                "textDocument/didClose",
                json!({"textDocument": {"uri": document_uri(&self.root_uri, &path)}}),
            )?;
            self.documents.remove(&id);
            self.diagnostics
                .retain(|published| published.document_id != id);
        }
        for (id, next) in desired {
            checked_version(next.version)?;
            match self.documents.get(&id) {
                None => {
                    self.notify(
                        "textDocument/didOpen",
                        json!({"textDocument": {
                            "uri": document_uri(&self.root_uri, &next.path),
                            "languageId": "rust",
                            "version": next.version,
                            "text": next.text
                        }}),
                    )?;
                    self.documents.insert(
                        id,
                        SyncedDocument {
                            path: next.path.clone(),
                            version: next.version,
                            text: next.text.clone(),
                        },
                    );
                }
                Some(current) if current.version != next.version || current.text != next.text => {
                    if next.version <= current.version {
                        return Err("language-server document version did not advance".into());
                    }
                    self.notify(
                        "textDocument/didChange",
                        json!({
                            "textDocument": {
                                "uri": document_uri(&self.root_uri, &next.path),
                                "version": next.version
                            },
                            "contentChanges": [{"text": next.text}]
                        }),
                    )?;
                    self.diagnostics
                        .retain(|published| published.document_id != id);
                    self.documents.insert(
                        id,
                        SyncedDocument {
                            path: next.path.clone(),
                            version: next.version,
                            text: next.text.clone(),
                        },
                    );
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    fn request_reload(&mut self, now: Instant) -> Result<bool, String> {
        if self.state != ProtocolState::Ready
            || self
                .pending
                .values()
                .any(|request| matches!(request.kind, PendingKind::Reload))
        {
            return Ok(false);
        }
        self.request(
            PendingKind::Reload,
            "rust-analyzer/reloadWorkspace",
            Value::Null,
            now + RELOAD_DEADLINE,
        )?;
        Ok(true)
    }

    fn request_completion(
        &mut self,
        document: &DocumentState,
        position_byte: u64,
        position: Utf16Position,
        request_sequence: u64,
        now: Instant,
    ) -> Result<Option<i64>, String> {
        if self.state != ProtocolState::Ready {
            return Ok(None);
        }
        let Some(synced) = self.documents.get(&document.document_id) else {
            return Ok(None);
        };
        if synced.path != document.path
            || synced.version != document.version
            || synced.text != document.text
        {
            return Ok(None);
        }
        let request = CompletionRequest {
            generation: self.generation,
            request_sequence,
            document_id: document.document_id.clone(),
            path: document.path.clone(),
            version: document.version,
            position_byte,
            position,
        };
        let id = self.request(
            PendingKind::Completion(request),
            "textDocument/completion",
            json!({
                "textDocument": {"uri": document_uri(&self.root_uri, &document.path)},
                "position": {"line": position.line, "character": position.character},
                "context": {"triggerKind": 1}
            }),
            now + COMPLETION_DEADLINE,
        )?;
        Ok(Some(id))
    }

    fn begin_shutdown(&mut self, now: Instant) -> Result<bool, String> {
        if self.state != ProtocolState::Ready {
            return Ok(false);
        }
        let documents = std::mem::take(&mut self.documents);
        for (_, document) in documents {
            self.notify(
                "textDocument/didClose",
                json!({"textDocument": {"uri": document_uri(&self.root_uri, &document.path)}}),
            )?;
        }
        self.request(
            PendingKind::Shutdown,
            "shutdown",
            Value::Null,
            now + SHUTDOWN_DEADLINE,
        )?;
        self.state = ProtocolState::Stopping;
        Ok(true)
    }

    fn check_deadlines(&mut self, now: Instant) -> Result<(), String> {
        let expired = self
            .pending
            .iter()
            .filter_map(|(id, request)| (now >= request.deadline).then_some(*id))
            .collect::<Vec<_>>();
        for id in expired {
            let request = self.pending.remove(&id).expect("collected pending request");
            match request.kind {
                PendingKind::Completion(request) => self.push_completion(request, Vec::new()),
                _ => return Err("language-server request deadline elapsed".into()),
            }
        }
        Ok(())
    }
}

fn parse_completion_result(result: Option<&Value>) -> Vec<CompletionItem> {
    let Some(result) = result else {
        return Vec::new();
    };
    let items = match result {
        Value::Null => return Vec::new(),
        Value::Array(items) => items,
        Value::Object(list) => {
            if list.contains_key("itemDefaults") {
                return Vec::new();
            }
            let Some(Value::Array(items)) = list.get("items") else {
                return Vec::new();
            };
            items
        }
        _ => return Vec::new(),
    };
    let mut parsed = Vec::with_capacity(items.len().min(MAX_COMPLETION_ITEMS));
    for item in items {
        if parsed.len() == MAX_COMPLETION_ITEMS {
            break;
        }
        if let Some(item) = parse_completion_item(item) {
            parsed.push(item);
        }
    }
    parsed
}

fn parse_publish_diagnostics_params(params: Option<&Value>) -> Option<(&str, u64, &[Value])> {
    let params = params?.as_object()?;
    let uri = params.get("uri")?.as_str()?;
    let version = params.get("version")?.as_u64()?;
    checked_version(version).ok()?;
    let diagnostics = params.get("diagnostics")?.as_array()?;
    Some((uri, version, diagnostics))
}

fn parse_live_diagnostics(values: &[Value]) -> Option<Vec<LiveDiagnostic>> {
    values
        .iter()
        .take(MAX_LIVE_DIAGNOSTICS_PER_DOCUMENT)
        .map(parse_live_diagnostic)
        .collect()
}

fn parse_live_diagnostic(value: &Value) -> Option<LiveDiagnostic> {
    let diagnostic = value.as_object()?;
    let range = diagnostic.get("range")?.as_object()?;
    let start = parse_utf16_position(range.get("start")?)?;
    let end = parse_utf16_position(range.get("end")?)?;
    let severity = match diagnostic.get("severity") {
        None | Some(Value::Null) => LiveDiagnosticSeverity::Information,
        Some(value) => match value.as_u64()? {
            1 => LiveDiagnosticSeverity::Error,
            2 => LiveDiagnosticSeverity::Warning,
            3 => LiveDiagnosticSeverity::Information,
            4 => LiveDiagnosticSeverity::Hint,
            _ => return None,
        },
    };
    let message = crate::display::label(
        diagnostic.get("message")?.as_str()?,
        MAX_LIVE_DIAGNOSTIC_MESSAGE_BYTES,
    );
    Some(LiveDiagnostic {
        start,
        end,
        severity,
        message,
    })
}

fn parse_completion_item(value: &Value) -> Option<CompletionItem> {
    let item = value.as_object()?;
    if [
        "additionalTextEdits",
        "command",
        "textDocument",
        "workspaceEdit",
        "documentChanges",
        "textEditText",
        "insertTextMode",
    ]
    .iter()
    .any(|field| item.contains_key(*field))
    {
        return None;
    }
    let label = item.get("label")?.as_str()?;
    if label.len() > MAX_STRING_BYTES {
        return None;
    }
    let kind = match item.get("kind") {
        None => None,
        Some(value) => Some(
            u8::try_from(value.as_u64()?)
                .ok()
                .filter(|kind| (1..=25).contains(kind))?,
        ),
    };
    if let Some(format) = item.get("insertTextFormat")
        && format.as_u64() != Some(1)
    {
        return None;
    }
    let insert_text = item.get("insertText");
    let text_edit = item.get("textEdit");
    if insert_text.is_some() && text_edit.is_some() {
        return None;
    }
    let edit = if let Some(value) = text_edit {
        let edit = value.as_object()?;
        if edit.len() != 2 || !edit.contains_key("range") || !edit.contains_key("newText") {
            return None;
        }
        let range = edit.get("range")?.as_object()?;
        if range.len() != 2 || !range.contains_key("start") || !range.contains_key("end") {
            return None;
        }
        let new_text = edit.get("newText")?.as_str()?;
        if new_text.len() > MAX_INSERTED_TEXT_BYTES {
            return None;
        }
        CompletionEdit::Replace {
            start: parse_utf16_position(range.get("start")?)?,
            end: parse_utf16_position(range.get("end")?)?,
            new_text: new_text.to_owned(),
        }
    } else {
        let text = insert_text.map(Value::as_str).unwrap_or(Some(label))?;
        if text.len() > MAX_INSERTED_TEXT_BYTES {
            return None;
        }
        CompletionEdit::Insert(text.to_owned())
    };
    Some(CompletionItem {
        label: label.to_owned(),
        kind,
        edit,
    })
}

fn parse_utf16_position(value: &Value) -> Option<Utf16Position> {
    let position = value.as_object()?;
    if position.len() != 2 || !position.contains_key("line") || !position.contains_key("character")
    {
        return None;
    }
    let line = u32::try_from(position.get("line")?.as_u64()?).ok()?;
    let character = u32::try_from(position.get("character")?.as_u64()?).ok()?;
    (line <= i32::MAX as u32 && character <= i32::MAX as u32)
        .then_some(Utf16Position { line, character })
}

fn checked_version(version: u64) -> Result<i32, String> {
    i32::try_from(version).map_err(|_| "language-server document version exceeds i32".into())
}

fn validate_text_sync(value: Option<&Value>) -> Result<(), String> {
    let Some(Value::Object(options)) = value else {
        return Err("language server did not declare document synchronization options".into());
    };
    if options.get("openClose") != Some(&Value::Bool(true)) {
        return Err("language server did not declare document open/close support".into());
    }
    let kind = options.get("change").and_then(Value::as_u64);
    if !kind.is_some_and(|kind| matches!(kind, 1 | 2)) {
        return Err("language server does not support full document updates".into());
    }
    Ok(())
}

fn initialization_options() -> Value {
    json!({
        "checkOnSave": false,
        "cargo": {
            "autoreload": false,
            "buildScripts": {
                "enable": false,
                "rebuildOnSave": false,
                "useRustcWrapper": false
            },
            "metadataExtraArgs": ["--locked", "--offline"]
        },
        "procMacro": {"enable": false}
    })
}

pub(crate) fn supports_document(path: &WorkspacePath) -> bool {
    Path::new(path.as_str())
        .extension()
        .and_then(|extension| extension.to_str())
        == Some("rs")
}

fn file_uri(path: &Path) -> Result<String, String> {
    let value = path
        .to_str()
        .ok_or("language-server workspace path is not UTF-8")?;
    if !path.is_absolute() {
        return Err("language-server workspace path is not absolute".into());
    }
    Ok(format!("file://{}", percent_encode(value.as_bytes(), true)))
}

fn document_uri(root_uri: &str, path: &WorkspacePath) -> String {
    format!(
        "{root_uri}/{}",
        percent_encode(path.as_str().as_bytes(), true)
    )
}

fn percent_encode(bytes: &[u8], keep_slash: bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut result = String::with_capacity(bytes.len());
    for byte in bytes {
        if byte.is_ascii_alphanumeric()
            || matches!(*byte, b'-' | b'.' | b'_' | b'~')
            || (keep_slash && *byte == b'/')
        {
            result.push(*byte as char);
        } else {
            result.push('%');
            result.push(HEX[(byte >> 4) as usize] as char);
            result.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    result
}

fn validate_json(value: &Value) -> Result<(), String> {
    fn visit(value: &Value, depth: usize, values: &mut usize) -> Result<(), String> {
        if depth > MAX_JSON_DEPTH {
            return Err("language-server JSON nesting exceeds limit".into());
        }
        *values = values
            .checked_add(1)
            .ok_or("language-server JSON value count overflow")?;
        if *values > MAX_JSON_VALUES {
            return Err("language-server JSON value count exceeds limit".into());
        }
        match value {
            Value::String(text) if text.len() > MAX_JSON_STRING_BYTES => {
                Err("language-server JSON string exceeds limit".into())
            }
            Value::Array(items) => {
                if items.len() > MAX_JSON_ITEMS {
                    return Err("language-server JSON array exceeds limit".into());
                }
                for item in items {
                    visit(item, depth + 1, values)?;
                }
                Ok(())
            }
            Value::Object(object) => {
                if object.len() > MAX_JSON_ITEMS
                    || object.keys().any(|key| key.len() > MAX_JSON_KEY_BYTES)
                {
                    return Err("language-server JSON object exceeds limit".into());
                }
                for item in object.values() {
                    visit(item, depth + 1, values)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    visit(value, 0, &mut 0)
}

enum ServiceState {
    Resolving(ResolveJob),
    Pending(Resolution),
    Running {
        transport: Transport,
        protocol: Protocol,
    },
    Backoff {
        until: Instant,
    },
    Unavailable,
    Stopped,
}

pub(crate) struct ServicePoll {
    pub changed: bool,
    pub resolution: Option<ResolutionRecord>,
    pub completions: Vec<CompletionResponse>,
    pub diagnostics: Vec<PublishedDiagnostics>,
    pub diagnostic_generation: Option<u64>,
}

pub(crate) struct LanguageService {
    root: PathBuf,
    pin: String,
    generation: u64,
    retries: u8,
    cleanup_uncertain: bool,
    state: ServiceState,
}

impl LanguageService {
    pub fn start(root: PathBuf, pin: String) -> Self {
        let generation = 1;
        let state = ResolveJob::start(generation, root.clone(), pin.clone())
            .map(ServiceState::Resolving)
            .unwrap_or(ServiceState::Unavailable);
        Self {
            root,
            pin,
            generation,
            retries: 0,
            cleanup_uncertain: false,
            state,
        }
    }

    pub fn poll(
        &mut self,
        documents: &[DocumentState],
        writes_allowed: bool,
        now: Instant,
    ) -> ServicePoll {
        let mut changed = false;
        if matches!(&self.state, ServiceState::Resolving(job) if job.thread.is_finished()) {
            let state = std::mem::replace(&mut self.state, ServiceState::Stopped);
            self.state = match state {
                ServiceState::Resolving(job) => match job.thread.join() {
                    Ok(resolution) => {
                        self.cleanup_uncertain |= !resolution.cleanup_confirmed;
                        ServiceState::Pending(resolution)
                    }
                    Err(_) => {
                        self.cleanup_uncertain = true;
                        ServiceState::Unavailable
                    }
                },
                _ => unreachable!(),
            };
            changed = true;
        }
        if let ServiceState::Pending(resolution) = &self.state {
            return ServicePoll {
                changed: true,
                resolution: Some(ResolutionRecord {
                    generation: resolution.generation,
                    report: resolution.report.clone(),
                    launch_available: resolution.launch.is_ok(),
                    detail: resolution
                        .launch
                        .as_ref()
                        .err()
                        .cloned()
                        .unwrap_or_else(|| "fresh installed tools resolved".into()),
                }),
                completions: Vec::new(),
                diagnostics: Vec::new(),
                diagnostic_generation: None,
            };
        }
        if writes_allowed && matches!(&self.state, ServiceState::Backoff { until } if now >= *until)
        {
            self.begin_resolution();
            return ServicePoll {
                changed: true,
                resolution: None,
                completions: Vec::new(),
                diagnostics: Vec::new(),
                diagnostic_generation: None,
            };
        }
        let mut completions = Vec::new();
        let mut diagnostics = Vec::new();
        let mut diagnostic_generation = None;
        let failed = if let ServiceState::Running {
            transport,
            protocol,
        } = &mut self.state
        {
            if protocol
                .ready_since
                .is_some_and(|ready| now.saturating_duration_since(ready) >= HEALTHY_RESET)
            {
                self.retries = 0;
            }
            match transport.check() {
                Ok(true) => Some("language server exited".to_owned()),
                Err(error) => Some(error),
                Ok(false) => {
                    let result = protocol
                        .poll(transport, documents, writes_allowed, now)
                        .map(|poll_changed| changed |= poll_changed)
                        .err();
                    completions.extend(protocol.completions.drain(..));
                    diagnostics.extend(protocol.diagnostics.drain(..));
                    if protocol.state == ProtocolState::Ready {
                        diagnostic_generation = Some(protocol.generation);
                    }
                    result
                }
            }
        } else {
            None
        };
        if failed.is_some() {
            self.fail_running(now);
            changed = true;
            diagnostics.clear();
            diagnostic_generation = None;
        }
        ServicePoll {
            changed,
            resolution: None,
            completions,
            diagnostics,
            diagnostic_generation,
        }
    }

    pub fn completion_generation(&self, document: &DocumentState) -> Option<u64> {
        let ServiceState::Running { protocol, .. } = &self.state else {
            return None;
        };
        let synced = protocol.documents.get(&document.document_id)?;
        (protocol.state == ProtocolState::Ready
            && synced.path == document.path
            && synced.version == document.version
            && synced.text == document.text)
            .then_some(protocol.generation)
    }

    pub fn request_completion(
        &mut self,
        document: &DocumentState,
        position_byte: u64,
        position: Utf16Position,
        request_sequence: u64,
        now: Instant,
    ) -> bool {
        let ServiceState::Running { protocol, .. } = &mut self.state else {
            return false;
        };
        protocol
            .request_completion(document, position_byte, position, request_sequence, now)
            .is_ok_and(|id| id.is_some())
    }

    pub fn completion_generation_is_ready(&self, generation: u64) -> bool {
        matches!(
            &self.state,
            ServiceState::Running { protocol, .. }
                if protocol.state == ProtocolState::Ready && protocol.generation == generation
        )
    }

    pub fn authorize_resolution(&mut self, published: bool, now: Instant) {
        let state = std::mem::replace(&mut self.state, ServiceState::Stopped);
        let ServiceState::Pending(resolution) = state else {
            self.state = state;
            return;
        };
        if !resolution.cleanup_confirmed {
            self.cleanup_uncertain = true;
            self.state = ServiceState::Unavailable;
            return;
        }
        if !published {
            self.state = ServiceState::Unavailable;
            return;
        }
        let Ok(spec) = resolution.launch else {
            // A missing optional component remains inert until explicit F8.
            self.state = ServiceState::Unavailable;
            return;
        };
        let launch = Transport::launch(&spec).and_then(|mut transport| {
            match Protocol::new(self.generation, &self.root, now) {
                Ok(protocol) => Ok((transport, protocol)),
                Err(_) => Err(LaunchFailure {
                    cleanup_confirmed: transport.cleanup(),
                }),
            }
        });
        self.state = match launch {
            Ok((transport, protocol)) => ServiceState::Running {
                transport,
                protocol,
            },
            Err(failure) if failure.cleanup_confirmed => {
                self.retries = self.retries.saturating_add(1);
                if self.retries <= MAX_AUTOMATIC_RETRIES {
                    self.backoff(now)
                } else {
                    ServiceState::Unavailable
                }
            }
            Err(_) => {
                self.cleanup_uncertain = true;
                ServiceState::Unavailable
            }
        };
    }

    pub fn reload(&mut self, now: Instant) -> bool {
        if self.cleanup_uncertain {
            return false;
        }
        let ready_reload = if let ServiceState::Running { protocol, .. } = &mut self.state
            && protocol.state == ProtocolState::Ready
        {
            Some(protocol.request_reload(now))
        } else {
            None
        };
        if let Some(Ok(_)) = ready_reload {
            return true;
        }
        if !self.cancel_and_stop() {
            self.state = ServiceState::Unavailable;
            return false;
        }
        self.retries = 0;
        self.begin_resolution();
        true
    }

    pub fn stop_for_authority(&mut self) -> bool {
        let cleanup_confirmed = self.cancel_and_stop();
        self.state = ServiceState::Stopped;
        cleanup_confirmed
    }

    pub fn stop_for_command(&mut self) -> bool {
        let cleanup_confirmed = self.cancel_and_stop();
        self.state = ServiceState::Stopped;
        cleanup_confirmed
    }

    pub fn resume_after_command(&mut self) {
        if !self.cleanup_uncertain && matches!(self.state, ServiceState::Stopped) {
            self.begin_resolution();
        }
    }

    pub fn shutdown(&mut self) -> bool {
        let state = std::mem::replace(&mut self.state, ServiceState::Stopped);
        let cleanup_confirmed = match state {
            ServiceState::Resolving(job) => job.cancel_and_join(),
            ServiceState::Running {
                mut transport,
                mut protocol,
            } => {
                let graceful = protocol.begin_shutdown(Instant::now()).unwrap_or(false);
                let until = Instant::now() + SHUTDOWN_DEADLINE;
                while graceful && Instant::now() < until {
                    let now = Instant::now();
                    if protocol.poll(&mut transport, &[], true, now).is_err() {
                        break;
                    }
                    match transport.check() {
                        Ok(true) => break,
                        Ok(false) => thread::sleep(Duration::from_millis(2)),
                        Err(_) => break,
                    }
                }
                transport.cleanup()
            }
            ServiceState::Pending(resolution) => resolution.cleanup_confirmed,
            ServiceState::Backoff { .. } | ServiceState::Unavailable | ServiceState::Stopped => {
                true
            }
        };
        self.cleanup_uncertain |= !cleanup_confirmed;
        !self.cleanup_uncertain
    }

    pub fn status(&self) -> &'static str {
        match self.state {
            ServiceState::Resolving(_) | ServiceState::Pending(_) => "LSP resolving",
            ServiceState::Running { ref protocol, .. } => match protocol.state {
                ProtocolState::Initializing => "LSP initializing",
                ProtocolState::Ready => "LSP ready",
                ProtocolState::Stopping | ProtocolState::ExitSent => "LSP stopping",
            },
            ServiceState::Backoff { .. } => "LSP retry pending",
            ServiceState::Unavailable => "LSP unavailable",
            ServiceState::Stopped => "LSP stopped",
        }
    }

    fn fail_running(&mut self, now: Instant) {
        let cleanup_confirmed = self.cancel_and_stop();
        self.retries = self.retries.saturating_add(1);
        self.state = if cleanup_confirmed && self.retries <= MAX_AUTOMATIC_RETRIES {
            self.backoff(now)
        } else {
            ServiceState::Unavailable
        };
    }

    fn backoff(&self, now: Instant) -> ServiceState {
        let shift = u32::from(self.retries.saturating_sub(1).min(5));
        let millis = 250_u64.saturating_mul(1_u64 << shift).min(8_000);
        ServiceState::Backoff {
            until: now + Duration::from_millis(millis),
        }
    }

    fn begin_resolution(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.state = ResolveJob::start(self.generation, self.root.clone(), self.pin.clone())
            .map(ServiceState::Resolving)
            .unwrap_or(ServiceState::Unavailable);
    }

    fn cancel_and_stop(&mut self) -> bool {
        let state = std::mem::replace(&mut self.state, ServiceState::Stopped);
        let cleanup_confirmed = match state {
            ServiceState::Resolving(job) => job.cancel_and_join(),
            ServiceState::Running { mut transport, .. } => transport.cleanup(),
            ServiceState::Pending(resolution) => resolution.cleanup_confirmed,
            _ => true,
        };
        self.cleanup_uncertain |= !cleanup_confirmed;
        !self.cleanup_uncertain
    }
}

impl Drop for LanguageService {
    fn drop(&mut self) {
        let _ = self.cancel_and_stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_queue_holds_one_full_poll_batch() {
        let (inbound, _incoming) = mpsc::sync_channel(MAX_INCOMING_MESSAGES);

        for id in 0..MAX_MESSAGES_PER_POLL {
            inbound
                .try_send(TransportEvent::Message(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": null
                })))
                .unwrap();
        }
    }

    #[test]
    fn incoming_reader_backpressures_a_small_burst() {
        let (inbound, incoming) = mpsc::sync_channel(MAX_INCOMING_MESSAGES);
        for id in 0..MAX_INCOMING_MESSAGES {
            inbound
                .try_send(TransportEvent::Message(json!({"id": id})))
                .unwrap();
        }
        let (finished, completion) = mpsc::channel();
        let reader = thread::spawn(move || {
            let sent = send_incoming(&inbound, json!({"id": 16}));
            finished.send(sent).unwrap();
        });

        assert_eq!(
            completion.recv_timeout(Duration::from_secs(1)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        assert!(matches!(
            incoming.recv().unwrap(),
            TransportEvent::Message(_)
        ));
        assert_eq!(completion.recv_timeout(Duration::from_secs(1)), Ok(true));
        reader.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn transport_cleanup_releases_a_queue_blocked_reader() {
        use std::os::unix::process::CommandExt;

        let child = Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let group = child.id() as i32;
        let (inbound, incoming) = mpsc::sync_channel(1);
        inbound
            .try_send(TransportEvent::Message(json!({"id": 1})))
            .unwrap();
        let finished = Arc::new(AtomicU8::new(0));
        let reader_finished = Arc::clone(&finished);
        let reader = thread::spawn(move || {
            assert!(!send_incoming(&inbound, json!({"id": 2})));
            reader_finished.store(1, Ordering::Release);
        });
        let mut transport = Transport {
            child,
            group,
            outgoing: None,
            incoming: Some(incoming),
            fault: Arc::new(AtomicU8::new(0)),
            stderr: Arc::new(Mutex::new(Vec::new())),
            threads: vec![reader],
        };

        assert!(transport.cleanup());
        assert_eq!(finished.load(Ordering::Acquire), 1);
    }

    #[cfg(unix)]
    #[test]
    fn unconfirmed_analyzer_group_blocks_command_boundary_after_signal_delivery() {
        use std::os::unix::process::CommandExt;

        let now = Instant::now();
        let mut service = running_service(now);
        let group = match &service.state {
            ServiceState::Running { transport, .. } => transport.group,
            _ => unreachable!("test service must own a running transport"),
        };
        let mut retained_member = Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(group)
            .spawn()
            .unwrap();

        let boundary_confirmed = service.stop_for_command();
        let _ = retained_member.wait();

        assert!(
            !boundary_confirmed,
            "successful SIGKILL delivery is not confirmed process-group absence"
        );
        assert!(
            service.cleanup_uncertain,
            "unconfirmed cleanup must latch conservative authority retention"
        );
        assert_eq!(service.status(), "LSP stopped");
    }

    #[cfg(unix)]
    fn running_service(now: Instant) -> LanguageService {
        use std::os::unix::process::CommandExt;

        let child = Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let group = child.id() as i32;
        let (_inbound, incoming) = mpsc::sync_channel(1);
        let transport = Transport {
            child,
            group,
            outgoing: None,
            incoming: Some(incoming),
            fault: Arc::new(AtomicU8::new(0)),
            stderr: Arc::new(Mutex::new(Vec::new())),
            threads: Vec::new(),
        };
        let mut protocol = protocol(now);
        initialize(&mut protocol, now);
        protocol.outbox.clear();
        LanguageService {
            root: PathBuf::from("/owned/work space"),
            pin: "stable".into(),
            generation: 7,
            retries: 0,
            cleanup_uncertain: false,
            state: ServiceState::Running {
                transport,
                protocol,
            },
        }
    }

    fn protocol(now: Instant) -> Protocol {
        Protocol::new(7, Path::new("/owned/work space"), now).unwrap()
    }

    fn initialize(protocol: &mut Protocol, now: Instant) {
        let initialize = protocol.outbox.pop_front().unwrap();
        assert_eq!(initialize["method"], "initialize");
        protocol
            .handle(
                json!({
                    "jsonrpc": "2.0",
                    "id": initialize["id"],
                    "result": {"capabilities": {
                        "positionEncoding": "utf-16",
                        "textDocumentSync": {"openClose": true, "change": 2}
                    }}
                }),
                now,
            )
            .unwrap();
        assert_eq!(protocol.state, ProtocolState::Ready);
        assert_eq!(
            protocol.outbox.pop_front().unwrap()["method"],
            "initialized"
        );
    }

    fn document(index: u64, version: u64, text: &str) -> DocumentState {
        DocumentState {
            document_id: DocumentId::new(format!("document-{index}")).unwrap(),
            path: WorkspacePath::new(format!("src/file-{index}.rs")).unwrap(),
            version,
            text: text.to_owned(),
        }
    }

    fn failed_launch_resolution() -> Resolution {
        Resolution {
            generation: 1,
            report: ToolchainReport {
                assignment_pin: Some("stable".into()),
                selected_toolchain: Some("stable".into()),
                working_directory: PathBuf::from("/owned/work space"),
                probes: Vec::new(),
            },
            launch: Ok(LaunchSpec {
                root: PathBuf::from("/owned/work space"),
                rustup: PathBuf::from("/definitely/missing/rustup"),
                selection: "stable".into(),
                analyzer: PathBuf::from("/definitely/missing/rust-analyzer"),
                cargo: PathBuf::from("/definitely/missing/cargo"),
                rustc: PathBuf::from("/definitely/missing/rustc"),
                rustdoc: PathBuf::from("/definitely/missing/rustdoc"),
            }),
            cleanup_confirmed: true,
        }
    }

    #[test]
    fn initialize_rejects_malformed_result_and_sync_capability() {
        let now = Instant::now();
        let mut missing_result_object = protocol(now);
        assert!(
            missing_result_object
                .handle(json!({"jsonrpc": "2.0", "id": 1, "result": null}), now)
                .is_err(),
            "R3: initialize result must be an object"
        );

        let mut malformed_sync = protocol(now);
        assert!(
            malformed_sync
                .handle(
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {"capabilities": {"textDocumentSync": "incremental"}}
                    }),
                    now
                )
                .is_err(),
            "R3: malformed synchronization capability must not grant authority"
        );
    }

    #[test]
    fn text_sync_requires_explicit_open_close_and_supported_change_kind() {
        assert!(validate_text_sync(None).is_err());
        for rejected in [
            Value::Null,
            json!(2),
            json!("incremental"),
            json!({"change": 2}),
            json!({"openClose": null, "change": 2}),
            json!({"openClose": "yes", "change": 2}),
            json!({"openClose": false, "change": 2}),
            json!({"openClose": true}),
            json!({"openClose": true, "change": null}),
            json!({"openClose": true, "change": "full"}),
            json!({"openClose": true, "change": 0}),
            json!({"openClose": true, "change": 3}),
        ] {
            assert!(validate_text_sync(Some(&rejected)).is_err());
        }
        assert!(validate_text_sync(Some(&json!({"openClose": true, "change": 1}))).is_ok());
        assert!(validate_text_sync(Some(&json!({"openClose": true, "change": 2}))).is_ok());
    }

    #[test]
    fn complete_resync_is_bounded_by_workspace_policy_instead_of_transport_window() {
        let now = Instant::now();
        let mut protocol = protocol(now);
        initialize(&mut protocol, now);
        let documents = (0..32)
            .map(|index| document(index, 1, &format!("pub const V{index}: u64 = {index};\n")))
            .collect::<Vec<_>>();
        protocol
            .reconcile_documents(&documents)
            .expect("R4: every current document must fit the bounded resync queue");
        assert_eq!(protocol.documents.len(), documents.len());
        assert_eq!(protocol.outbox.len(), documents.len());
    }

    #[test]
    fn maximum_policy_source_survives_worst_case_json_expansion_and_resync() {
        let now = Instant::now();
        let mut protocol = protocol(now);
        initialize(&mut protocol, now);
        let maximum = usize::try_from(rustrace_workspace::hash::MAX_WORKSPACE_FILE_BYTES).unwrap();
        let first = "\u{1}".repeat(maximum);
        protocol
            .reconcile_documents(&[document(1, 1, &first)])
            .expect("maximum valid source must fit didOpen");
        let opened = protocol.outbox.pop_front().unwrap();
        assert_eq!(opened["method"], "textDocument/didOpen");
        assert_eq!(opened["params"]["textDocument"]["text"], first);
        assert!(serde_json::to_vec(&opened).unwrap().len() <= MAX_MESSAGE_BYTES);

        let second = "\u{2}".repeat(maximum);
        protocol
            .reconcile_documents(&[document(1, 2, &second)])
            .expect("maximum valid source must fit full resynchronization");
        let changed = protocol.outbox.pop_front().unwrap();
        assert_eq!(changed["method"], "textDocument/didChange");
        assert_eq!(changed["params"]["contentChanges"][0]["text"], second);
        assert!(serde_json::to_vec(&changed).unwrap().len() <= MAX_MESSAGE_BYTES);
    }

    #[test]
    fn cleanup_confirmed_launch_failures_have_one_initial_attempt_and_five_retries() {
        let now = Instant::now();
        let mut service = LanguageService {
            root: PathBuf::from("/owned/work space"),
            pin: "stable".into(),
            generation: 1,
            retries: 0,
            cleanup_uncertain: false,
            state: ServiceState::Unavailable,
        };
        for attempt in 1..=usize::from(MAX_AUTOMATIC_RETRIES) + 1 {
            service.state = ServiceState::Pending(failed_launch_resolution());
            service.authorize_resolution(true, now);
            if attempt <= usize::from(MAX_AUTOMATIC_RETRIES) {
                assert!(matches!(service.state, ServiceState::Backoff { .. }));
            } else {
                assert!(
                    matches!(service.state, ServiceState::Unavailable),
                    "launch retry sequence exceeded six total attempts"
                );
            }
        }
        assert!(service.reload(now), "explicit F8 starts a fresh sequence");
        assert_eq!(service.retries, 0);
        assert!(!matches!(service.state, ServiceState::Unavailable));
        assert!(service.stop_for_authority());
    }

    #[test]
    fn stale_replies_and_mutation_requests_have_no_authority() {
        let now = Instant::now();
        let mut protocol = protocol(now);
        let pending = protocol.pending.len();
        protocol
            .handle(
                json!({"jsonrpc": "2.0", "id": 999, "result": {"ignored": true}}),
                now,
            )
            .unwrap();
        assert_eq!(protocol.pending.len(), pending);
        assert_eq!(protocol.state, ProtocolState::Initializing);

        protocol
            .handle(
                json!({
                    "jsonrpc": "2.0",
                    "id": "server-edit",
                    "method": "workspace/applyEdit",
                    "params": {"edit": {"changes": {}}}
                }),
                now,
            )
            .unwrap();
        let rejection = protocol.outbox.pop_back().unwrap();
        assert_eq!(rejection["id"], "server-edit");
        assert_eq!(rejection["error"]["code"], -32601);
    }

    #[test]
    fn reload_is_null_correlated_and_coalesced() {
        let now = Instant::now();
        let mut protocol = protocol(now);
        initialize(&mut protocol, now);
        assert!(protocol.request_reload(now).unwrap());
        assert!(!protocol.request_reload(now).unwrap());
        let request = protocol.outbox.pop_front().unwrap();
        assert_eq!(request["method"], "rust-analyzer/reloadWorkspace");
        assert_eq!(request["params"], Value::Null);
        protocol
            .handle(
                json!({"jsonrpc": "2.0", "id": request["id"], "result": null}),
                now,
            )
            .unwrap();
        assert!(protocol.pending.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn repeated_service_reload_keeps_one_request_and_generation() {
        let now = Instant::now();
        let mut service = running_service(now);
        assert!(service.reload(now));
        assert!(service.reload(now));
        assert_eq!(service.generation, 7);
        let ServiceState::Running { protocol, .. } = &service.state else {
            panic!("coalesced reload replaced the running generation");
        };
        assert_eq!(
            protocol
                .pending
                .values()
                .filter(|request| request.kind == PendingKind::Reload)
                .count(),
            1
        );
    }

    #[test]
    fn authority_stop_waits_for_resolver_completion() {
        let cancel = Arc::new(AtomicU8::new(0));
        let worker_cancel = Arc::clone(&cancel);
        let finished = Arc::new(AtomicU8::new(0));
        let worker_finished = Arc::clone(&finished);
        let thread = thread::spawn(move || {
            while worker_cancel.load(Ordering::Acquire) == 0 {
                thread::yield_now();
            }
            thread::sleep(Duration::from_millis(25));
            worker_finished.store(1, Ordering::Release);
            Resolution {
                generation: 1,
                report: ToolchainReport {
                    assignment_pin: None,
                    selected_toolchain: None,
                    working_directory: PathBuf::from("/owned/work space"),
                    probes: Vec::new(),
                },
                launch: Err("cancelled test resolution".into()),
                cleanup_confirmed: true,
            }
        });
        let mut service = LanguageService {
            root: PathBuf::from("/owned/work space"),
            pin: "stable".into(),
            generation: 1,
            retries: 0,
            cleanup_uncertain: false,
            state: ServiceState::Resolving(ResolveJob { cancel, thread }),
        };
        assert!(service.stop_for_authority());
        assert_eq!(finished.load(Ordering::Acquire), 1);
        assert_eq!(service.status(), "LSP stopped");
    }

    #[test]
    fn authority_stop_does_not_detach_cleanup_confirmed_resolver() {
        let cancel = Arc::new(AtomicU8::new(0));
        let worker_cancel = Arc::clone(&cancel);
        let finished = Arc::new(AtomicU8::new(0));
        let worker_finished = Arc::clone(&finished);
        let thread = thread::spawn(move || {
            while worker_cancel.load(Ordering::Acquire) == 0 {
                thread::yield_now();
            }
            // The owned probe has its own bounded cleanup. A second cutoff at
            // the same boundary must not detach this resolver before it can
            // report that cleanup completed.
            thread::sleep(CLEANUP_DEADLINE + Duration::from_millis(25));
            worker_finished.store(1, Ordering::Release);
            Resolution {
                generation: 1,
                report: ToolchainReport {
                    assignment_pin: None,
                    selected_toolchain: None,
                    working_directory: PathBuf::from("/owned/work space"),
                    probes: Vec::new(),
                },
                launch: Err("cancelled test resolution".into()),
                cleanup_confirmed: true,
            }
        });
        let mut service = LanguageService {
            root: PathBuf::from("/owned/work space"),
            pin: "stable".into(),
            generation: 1,
            retries: 0,
            cleanup_uncertain: false,
            state: ServiceState::Resolving(ResolveJob { cancel, thread }),
        };

        assert!(service.stop_for_authority());
        assert_eq!(finished.load(Ordering::Acquire), 1);
        assert_eq!(service.status(), "LSP stopped");
    }

    #[test]
    fn command_preparation_stops_active_resolver() {
        let cancel = Arc::new(AtomicU8::new(0));
        let worker_cancel = Arc::clone(&cancel);
        let finished = Arc::new(AtomicU8::new(0));
        let worker_finished = Arc::clone(&finished);
        let thread = thread::spawn(move || {
            while worker_cancel.load(Ordering::Acquire) == 0 {
                thread::yield_now();
            }
            worker_finished.store(1, Ordering::Release);
            Resolution {
                generation: 1,
                report: ToolchainReport {
                    assignment_pin: None,
                    selected_toolchain: None,
                    working_directory: PathBuf::from("/owned/work space"),
                    probes: Vec::new(),
                },
                launch: Err("cancelled test resolution".into()),
                cleanup_confirmed: true,
            }
        });
        let mut service = LanguageService {
            root: PathBuf::from("/owned/work space"),
            pin: "stable".into(),
            generation: 1,
            retries: 0,
            cleanup_uncertain: false,
            state: ServiceState::Resolving(ResolveJob { cancel, thread }),
        };

        assert!(service.stop_for_command());
        assert_eq!(
            finished.load(Ordering::Acquire),
            1,
            "command preparation must cancel and join an active resolver"
        );
        assert_eq!(service.status(), "LSP stopped");
    }

    #[test]
    fn command_stop_leaves_nonexecuting_states_inert() {
        let states = [
            ServiceState::Pending(Resolution {
                generation: 1,
                report: ToolchainReport {
                    assignment_pin: None,
                    selected_toolchain: None,
                    working_directory: PathBuf::from("/owned/work space"),
                    probes: Vec::new(),
                },
                launch: Err("optional analyzer unavailable".into()),
                cleanup_confirmed: true,
            }),
            ServiceState::Backoff {
                until: Instant::now() + Duration::from_secs(30),
            },
            ServiceState::Unavailable,
            ServiceState::Stopped,
        ];
        for state in states {
            let mut service = LanguageService {
                root: PathBuf::from("/owned/work space"),
                pin: "stable".into(),
                generation: 1,
                retries: 3,
                cleanup_uncertain: false,
                state,
            };
            assert!(service.stop_for_command());
            assert_eq!(service.status(), "LSP stopped");
            assert_eq!(
                service.retries, 3,
                "command stop does not reset retry bounds"
            );
        }
    }

    #[test]
    fn cleanup_uncertainty_latches_and_blocks_restart() {
        let mut service = LanguageService {
            root: PathBuf::from("/owned/work space"),
            pin: "stable".into(),
            generation: 1,
            retries: 0,
            cleanup_uncertain: true,
            state: ServiceState::Unavailable,
        };
        assert!(!service.reload(Instant::now()));
        assert_eq!(service.generation, 1);
        assert!(matches!(service.state, ServiceState::Unavailable));
        assert!(!service.stop_for_authority());
    }

    #[test]
    fn unresolved_probe_cleanup_blocks_authority_boundary() {
        let mut service = LanguageService {
            root: PathBuf::from("/owned/work space"),
            pin: "stable".into(),
            generation: 1,
            retries: 0,
            cleanup_uncertain: false,
            state: ServiceState::Pending(Resolution {
                generation: 1,
                report: ToolchainReport {
                    assignment_pin: None,
                    selected_toolchain: None,
                    working_directory: PathBuf::from("/owned/work space"),
                    probes: Vec::new(),
                },
                launch: Err("unconfirmed probe cleanup".into()),
                cleanup_confirmed: false,
            }),
        };
        assert!(!service.stop_for_authority());
        assert!(service.cleanup_uncertain);
        assert!(!service.shutdown());
    }

    #[cfg(unix)]
    #[test]
    fn setup_failure_kills_and_reaps_spawned_process_group() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "rustrace-lsp-launch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let rustup = root.join("rustup");
        std::fs::write(&rustup, "#!/bin/sh\nexec sleep 30\n").unwrap();
        let mut permissions = std::fs::metadata(&rustup).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&rustup, permissions).unwrap();
        let spec = LaunchSpec {
            root: root.clone(),
            rustup,
            selection: "stable".into(),
            analyzer: PathBuf::from("/unused/rust-analyzer"),
            cargo: PathBuf::from("/unused/cargo"),
            rustc: PathBuf::from("/unused/rustc"),
            rustdoc: PathBuf::from("/unused/rustdoc"),
        };
        let mut group = 0;
        let failure = match Transport::launch_with_setup(&spec, |spawned| {
            group = spawned;
            Err(())
        }) {
            Ok(mut transport) => {
                let _ = transport.cleanup();
                panic!("injected setup failure unexpectedly launched")
            }
            Err(failure) => failure,
        };
        assert!(failure.cleanup_confirmed);
        assert_ne!(group, 0);
        assert_eq!(unsafe { libc::kill(-group, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn document_recreation_closes_old_identity_before_opening_new_identity() {
        let now = Instant::now();
        let mut protocol = protocol(now);
        initialize(&mut protocol, now);
        protocol
            .reconcile_documents(&[document(1, 1, "old")])
            .unwrap();
        protocol.outbox.clear();
        let mut replacement = document(2, 0, "new");
        replacement.path = WorkspacePath::new("src/file-1.rs").unwrap();
        protocol.reconcile_documents(&[replacement]).unwrap();
        assert_eq!(protocol.outbox.len(), 2);
        assert_eq!(protocol.outbox[0]["method"], "textDocument/didClose");
        assert_eq!(protocol.outbox[1]["method"], "textDocument/didOpen");
    }

    #[test]
    fn json_shape_and_request_deadlines_are_finite() {
        let mut nested = Value::Null;
        for _ in 0..=MAX_JSON_DEPTH {
            nested = json!([nested]);
        }
        assert!(validate_json(&nested).is_err());

        let now = Instant::now();
        let mut protocol = protocol(now);
        assert!(protocol.check_deadlines(now).is_ok());
        assert!(protocol.check_deadlines(now + INITIALIZE_DEADLINE).is_err());
    }
}

#[cfg(test)]
#[path = "language_service_completion_tests.rs"]
mod completion_tests;

#[cfg(test)]
#[path = "language_service_diagnostics_tests.rs"]
mod diagnostics_tests;
