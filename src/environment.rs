use std::io::{ErrorKind, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAX_EVIDENCE_CHARS: usize = 160;
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
pub const DEFAULT_CAPTURE_LIMIT_BYTES: usize = 16 * 1024;
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const PIPE_DRAIN_GRACE: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Requirement {
    Required,
    Optional,
}

impl Requirement {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Optional => "optional",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolStatus {
    Available,
    NotFound,
    Failed,
    TimedOut,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

impl DiagnosticLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "ok",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExecutionEvidence {
    pub exit_code: Option<i32>,
    pub timeout_ms: Option<u64>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentDiagnostic {
    pub component: String,
    pub command: String,
    pub requirement: Requirement,
    pub status: ToolStatus,
    pub level: DiagnosticLevel,
    pub message: String,
    pub evidence: ExecutionEvidence,
    pub remediation: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnvironmentReport {
    diagnostics: Vec<ComponentDiagnostic>,
}

impl EnvironmentReport {
    pub fn diagnostics(&self) -> &[ComponentDiagnostic] {
        &self.diagnostics
    }

    pub fn has_blockers(&self) -> bool {
        self.required_issue_count() > 0
    }

    pub fn required_issue_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.requirement == Requirement::Required
                    && diagnostic.status != ToolStatus::Available
            })
            .count()
    }

    pub fn optional_issue_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.requirement == Requirement::Optional
                    && diagnostic.status != ToolStatus::Available
            })
            .count()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProbeCommand {
    pub program: &'static str,
    pub args: &'static [&'static str],
}

impl ProbeCommand {
    pub fn invocation(&self) -> String {
        std::iter::once(self.program)
            .chain(self.args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandExecution {
    NotFound,
    Succeeded {
        stdout: String,
        stderr: String,
    },
    Failed {
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    TimedOut {
        timeout: Duration,
        stdout: String,
        stderr: String,
    },
}

pub trait CommandRunner {
    fn run(&self, command: &ProbeCommand) -> CommandExecution;
}

#[derive(Clone, Copy, Debug)]
pub struct SystemCommandRunner {
    timeout: Duration,
    capture_limit: usize,
}

impl SystemCommandRunner {
    pub const fn with_limits(timeout: Duration, capture_limit: usize) -> Self {
        Self {
            timeout,
            capture_limit,
        }
    }
}

impl Default for SystemCommandRunner {
    fn default() -> Self {
        Self::with_limits(DEFAULT_PROBE_TIMEOUT, DEFAULT_CAPTURE_LIMIT_BYTES)
    }
}

impl CommandRunner for SystemCommandRunner {
    fn run(&self, probe: &ProbeCommand) -> CommandExecution {
        self.run_command(Command::new(probe.program).args(probe.args))
    }
}

impl SystemCommandRunner {
    pub(crate) fn capture_limit(&self) -> usize {
        self.capture_limit
    }

    /// Shared bounded probe execution; callers provide literal arguments and cwd.
    /// This is not the assignment command runner or its environment policy.
    pub(crate) fn run_command(&self, command: &mut Command) -> CommandExecution {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .env("RUSTUP_AUTO_INSTALL", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let mut child = match child {
            Err(error) if error.kind() == ErrorKind::NotFound => return CommandExecution::NotFound,
            Err(error) => {
                return CommandExecution::Failed {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: error.to_string(),
                };
            }
            Ok(child) => child,
        };

        let stdout = Capture::spawn(
            child.stdout.take().expect("configured child stdout pipe"),
            self.capture_limit,
        );
        let stderr = Capture::spawn(
            child.stderr.take().expect("configured child stderr pipe"),
            self.capture_limit,
        );
        let deadline = Instant::now() + self.timeout;
        let mut child_outcome = wait_for_child(&mut child, deadline);
        if matches!(child_outcome, ChildOutcome::Exited(_))
            && let Some(error) = kill_probe_group(child.id())
        {
            child_outcome = ChildOutcome::Failed(error);
        }
        let capture_deadline = if matches!(child_outcome, ChildOutcome::Exited(_)) {
            deadline
        } else {
            Instant::now() + PIPE_DRAIN_GRACE
        };
        let (stdout, stdout_finished) = stdout.finish_by(capture_deadline);
        let (mut stderr, stderr_finished) = stderr.finish_by(capture_deadline);

        if !stdout_finished || !stderr_finished {
            return CommandExecution::TimedOut {
                timeout: self.timeout,
                stdout,
                stderr,
            };
        }

        match child_outcome {
            ChildOutcome::Exited(status) if status.success() => {
                CommandExecution::Succeeded { stdout, stderr }
            }
            ChildOutcome::Exited(status) => CommandExecution::Failed {
                exit_code: status.code(),
                stdout,
                stderr,
            },
            ChildOutcome::TimedOut(cleanup_error) => {
                append_error(&mut stderr, cleanup_error);
                CommandExecution::TimedOut {
                    timeout: self.timeout,
                    stdout,
                    stderr,
                }
            }
            ChildOutcome::Failed(error) => {
                append_error(&mut stderr, Some(error));
                CommandExecution::Failed {
                    exit_code: None,
                    stdout,
                    stderr,
                }
            }
        }
    }
}

enum ChildOutcome {
    Exited(ExitStatus),
    TimedOut(Option<String>),
    Failed(String),
}

fn wait_for_child(child: &mut Child, deadline: Instant) -> ChildOutcome {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return ChildOutcome::Exited(status),
            Ok(None) if Instant::now() >= deadline => {
                return ChildOutcome::TimedOut(kill_and_reap(child));
            }
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(remaining.min(WAIT_POLL_INTERVAL));
            }
            Err(error) => {
                let mut message = format!("failed to wait for probe: {error}");
                if let Some(cleanup_error) = kill_and_reap(child) {
                    message.push_str("; ");
                    message.push_str(&cleanup_error);
                }
                return ChildOutcome::Failed(message);
            }
        }
    }
}

fn kill_and_reap(child: &mut Child) -> Option<String> {
    let group_error = kill_probe_group(child.id());
    let kill_error = child
        .kill()
        .err()
        .filter(|error| error.kind() != ErrorKind::InvalidInput);
    let wait_error = child.wait().err();

    match (kill_error, wait_error) {
        (None, None) => group_error,
        (Some(kill), None) => Some(format!("failed to kill timed-out probe: {kill}")),
        (None, Some(wait)) => Some(format!("failed to reap timed-out probe: {wait}")),
        (Some(kill), Some(wait)) => Some(format!(
            "failed to kill timed-out probe: {kill}; failed to reap it: {wait}"
        )),
    }
}

fn kill_probe_group(id: u32) -> Option<String> {
    #[cfg(unix)]
    {
        // Every probe starts a fresh process group. Clean up inherited children
        // as well as its leader so version launchers cannot leave pipes/tasks.
        let id = i32::try_from(id).ok()?;
        if unsafe { libc::kill(-id, libc::SIGKILL) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Some(format!("failed to clean up probe process group: {error}"));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = id;
    None
}

struct Capture {
    bytes: Arc<Mutex<Vec<u8>>>,
    finished: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    fn spawn(reader: impl Read + Send + 'static, limit: usize) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::with_capacity(limit.min(8 * 1024))));
        let finished = Arc::new(AtomicBool::new(false));
        let thread_bytes = Arc::clone(&bytes);
        let thread_finished = Arc::clone(&finished);
        let thread = thread::spawn(move || {
            read_bounded(reader, limit, &thread_bytes);
            thread_finished.store(true, Ordering::Release);
        });

        Self {
            bytes,
            finished,
            thread: Some(thread),
        }
    }

    fn finish_by(mut self, deadline: Instant) -> (String, bool) {
        while !self.finished.load(Ordering::Acquire) && Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            thread::sleep(remaining.min(WAIT_POLL_INTERVAL));
        }

        let finished = self.finished.load(Ordering::Acquire);
        if finished && let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }

        let bytes = self
            .bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (String::from_utf8_lossy(&bytes).into_owned(), finished)
    }
}

fn read_bounded(mut reader: impl Read, limit: usize, captured: &Arc<Mutex<Vec<u8>>>) {
    let mut buffer = [0_u8; 8 * 1024];

    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => return,
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let mut captured = captured
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let remaining = limit.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..count.min(remaining)]);
    }
}

fn append_error(stderr: &mut String, error: Option<String>) {
    let Some(error) = error else {
        return;
    };
    if !stderr.is_empty() {
        stderr.push('\n');
    }
    stderr.push_str(&error);
}

struct ToolSpec {
    component: &'static str,
    command: ProbeCommand,
    requirement: Requirement,
    remediation: &'static str,
}

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        component: "rustup",
        command: ProbeCommand {
            program: "rustup",
            args: &["--version"],
        },
        requirement: Requirement::Required,
        remediation: "Install rustup from https://rustup.rs, then restart the terminal.",
    },
    ToolSpec {
        component: "Rust compiler",
        command: ProbeCommand {
            program: "rustc",
            args: &["-vV"],
        },
        requirement: Requirement::Required,
        remediation: "Install a Rust toolchain with `rustup toolchain install stable`.",
    },
    ToolSpec {
        component: "Cargo",
        command: ProbeCommand {
            program: "cargo",
            args: &["-V"],
        },
        requirement: Requirement::Required,
        remediation: "Install a Rust toolchain with `rustup toolchain install stable`.",
    },
    ToolSpec {
        component: "rust-analyzer",
        command: ProbeCommand {
            program: "rust-analyzer",
            args: &["--version"],
        },
        requirement: Requirement::Optional,
        remediation: "Enable language services with `rustup component add rust-analyzer`.",
    },
    ToolSpec {
        component: "rustfmt",
        command: ProbeCommand {
            program: "rustfmt",
            args: &["--version"],
        },
        requirement: Requirement::Optional,
        remediation: "Enable formatting with `rustup component add rustfmt`.",
    },
    ToolSpec {
        component: "Clippy",
        command: ProbeCommand {
            program: "cargo",
            args: &["clippy", "-V"],
        },
        requirement: Requirement::Optional,
        remediation: "Enable linting with `rustup component add clippy`.",
    },
];

pub fn probe_environment(runner: &dyn CommandRunner) -> EnvironmentReport {
    let diagnostics = TOOLS
        .iter()
        .map(|tool| diagnostic_for(tool, runner.run(&tool.command)))
        .collect();

    EnvironmentReport { diagnostics }
}

fn diagnostic_for(tool: &ToolSpec, execution: CommandExecution) -> ComponentDiagnostic {
    let invocation = tool.command.invocation();
    let (status, message, evidence) = match execution {
        CommandExecution::NotFound => (
            ToolStatus::NotFound,
            format!("executable was not found for `{invocation}`"),
            ExecutionEvidence::default(),
        ),
        CommandExecution::Succeeded { stdout, stderr } => (
            ToolStatus::Available,
            format!("version probe succeeded: `{invocation}`"),
            evidence(Some(0), None, stdout, stderr),
        ),
        CommandExecution::Failed {
            exit_code,
            stdout,
            stderr,
        } => (
            ToolStatus::Failed,
            format!("version probe failed: `{invocation}`"),
            evidence(exit_code, None, stdout, stderr),
        ),
        CommandExecution::TimedOut {
            timeout,
            stdout,
            stderr,
        } => (
            ToolStatus::TimedOut,
            format!(
                "version probe timed out after {} ms: `{invocation}`",
                timeout.as_millis()
            ),
            evidence(None, Some(timeout), stdout, stderr),
        ),
    };
    let level = match (status, tool.requirement) {
        (ToolStatus::Available, _) => DiagnosticLevel::Info,
        (_, Requirement::Required) => DiagnosticLevel::Error,
        (_, Requirement::Optional) => DiagnosticLevel::Warning,
    };

    ComponentDiagnostic {
        component: tool.component.to_owned(),
        command: invocation,
        requirement: tool.requirement,
        status,
        level,
        message,
        evidence,
        remediation: if status == ToolStatus::Available {
            String::new()
        } else {
            tool.remediation.to_owned()
        },
    }
}

fn evidence(
    exit_code: Option<i32>,
    timeout: Option<Duration>,
    stdout: String,
    stderr: String,
) -> ExecutionEvidence {
    ExecutionEvidence {
        exit_code,
        timeout_ms: timeout.map(|timeout| u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
        stdout: concise_output(&stdout),
        stderr: concise_output(&stderr),
    }
}

fn concise_output(output: &str) -> Option<String> {
    let mut normalized = String::new();
    let mut pending_space = false;
    let mut count = 0;

    // This remains the historical compact observation snippet. Stop deriving
    // after the bounded prefix; the terminal boundary independently uses the
    // shared display policy. Raw probe capture/metadata is not rewritten.
    for character in output.chars().take(DEFAULT_CAPTURE_LIMIT_BYTES) {
        if character.is_whitespace() || character.is_control() {
            pending_space = !normalized.is_empty();
            continue;
        }

        if pending_space {
            normalized.push(' ');
            count += 1;
            pending_space = false;
        }
        normalized.push(character);
        count += 1;
        if count > MAX_EVIDENCE_CHARS {
            break;
        }
    }

    if normalized.is_empty() {
        return None;
    }

    let mut characters = normalized.chars();
    let concise: String = characters.by_ref().take(MAX_EVIDENCE_CHARS).collect();
    if characters.next().is_some() {
        Some(format!("{concise}…"))
    } else {
        Some(concise)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NothingAvailable;

    impl CommandRunner for NothingAvailable {
        fn run(&self, _command: &ProbeCommand) -> CommandExecution {
            CommandExecution::NotFound
        }
    }

    #[test]
    fn injected_runner_controls_the_probe_without_host_commands() {
        let report = probe_environment(&NothingAvailable);

        assert_eq!(report.required_issue_count(), 3);
        assert_eq!(report.optional_issue_count(), 3);
        assert!(report.has_blockers());
    }
}
