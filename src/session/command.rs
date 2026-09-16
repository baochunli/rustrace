//! The production authority owns all boundaries; workers only resolve/execute.
use super::*;
use crate::{
    CommandExecution,
    cargo_policy::{self, CargoAction, ConsoleCommand, PreparedCargoCommand, ResolvedTools},
    command_process::{
        self, LiveOutput, PanickedProcessCleanup, PendingProcessCleanup, ProcessCleanupHandoff,
        ProcessIo, ProcessLimits, ProcessResult, ProcessStdin, ProcessStdout, StdinMessage,
    },
    console::{
        OutputDisposition, TestCase, TestCaseComparison, TestCaseDirectory, TestCaseOutcome,
        classify_test_case_result,
    },
    diagnostics::{CommandDiagnostics, derive_command_diagnostics},
    toolchain::{self, ToolchainReport},
};
use std::{
    collections::VecDeque,
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread::JoinHandle,
};

#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicI32};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandActivity {
    version: u32,
    session_id: SessionId,
    sequence: u64,
    event_hash: Hash,
    active: bool,
}

pub(super) fn require_inactive_marker(
    owner: &PinnedJournalFile,
    metadata: &SessionMetadata,
) -> Result<()> {
    let path = owner
        .display_path()
        .parent()
        .ok_or("state directory missing")?
        .join("command-activity.json");
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let marker: CommandActivity =
        serde_json::from_slice(&owner.read_artifact("command-activity.json", METADATA_LIMIT)?)?;
    if marker.version != 1 || marker.session_id != metadata.session_id || marker.active {
        return Err("unfinished command activity; inspect and use linked recovery; restart does not establish resolver or child cleanup".into());
    }
    Ok(())
}

struct Ready {
    prepared: PreparedCargoCommand,
    format_prepare: Option<FormatPrepare>,
    dependency_prepare: Option<DependencyPrepare>,
    report: ToolchainReport,
    tools: Vec<CommandTool>,
    action: CargoAction,
    output_limit: u64,
    console: Option<ConsoleLaunch>,
}

struct ConsoleLaunch {
    request: ConsoleCommand,
    cases: Option<TestCaseDirectory>,
    input: Option<rustrace_workspace::OpenedRegularFile>,
    output: Option<OutputDisposition>,
}

pub(super) struct PendingConsole {
    toolchain: String,
    launch: ConsoleLaunch,
}

struct ConsoleIo {
    route: ConsoleCommandRoute,
    process: ProcessIo,
    sender: Option<SyncSender<StdinMessage>>,
    live: LiveOutput,
}

struct FormatPrepare {
    resolved: ResolvedTools,
    argv: Vec<String>,
}

struct DependencyPrepare {
    resolved: ResolvedTools,
    argv: Vec<String>,
    action: CargoAction,
}

struct FormatRun {
    snapshot: OwnedFormatSnapshot,
    before: Files,
    documents: Vec<crate::tui::FormatterDocumentState>,
    snapshot_sequence: u64,
}

struct DependencyRun {
    snapshot: OwnedFormatSnapshot,
    before: Files,
    documents: Vec<crate::tui::FormatterDocumentState>,
    snapshot_sequence: u64,
}

enum ToolRun {
    Format(FormatRun),
    Dependency(DependencyRun),
}

struct Resolution {
    result: std::result::Result<Ready, String>,
    report: ToolchainReport,
    captures: Vec<serde_json::Value>,
}

struct BlockedCommand {
    _result: ProcessResult,
    _start: ControlledCommandStarted,
    _millis: u64,
    _tool_run: Option<ToolRun>,
}

enum Job {
    Preparing(JoinHandle<Resolution>),
    Running {
        worker: JoinHandle<ProcessResult>,
        panic_cleanup: ProcessCleanupHandoff,
        start: ControlledCommandStarted,
        millis: u64,
        tool_run: Option<ToolRun>,
    },
    Reaping {
        cleanup: PendingProcessCleanup,
        result: ProcessResult,
        start: ControlledCommandStarted,
        millis: u64,
        tool_run: Option<ToolRun>,
    },
    Recording {
        chunks: VecDeque<ControlledCommandOutput>,
        finish: ControlledCommandFinished,
        tool_run: Option<ToolRun>,
        test_case_result: Option<TestCaseComparison>,
    },
    CleanupBlocked {
        command: Option<BlockedCommand>,
        detail: String,
    },
}

const COMMAND_QUIT_POLL_LIMIT: Duration = Duration::from_millis(MAX_COMMAND_CLEANUP_MILLIS + 500);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FormatStatus {
    Applied,
    NoChange,
    Discarded,
    Rejected,
}

pub(super) struct CommandState {
    job: Option<Job>,
    cancel: Arc<AtomicU8>,
    pub(super) modal: bool,
    deadline: Duration,
    outcome: Option<CommandOutcome>,
    pub(super) diagnostics: Option<CommandDiagnostics>,
    pub(super) selected_diagnostic: Option<usize>,
    unpublished_capture: Option<Vec<u8>>,
    format_status: Option<FormatStatus>,
    pub(super) pending_console: Option<PendingConsole>,
    console_stdin: Option<SyncSender<StdinMessage>>,
    console_live: Option<LiveOutput>,
    is_console: bool,
    last_action: Option<CargoAction>,
    test_case: Option<TestCase>,
    test_case_expected: Option<std::result::Result<Vec<u8>, String>>,
    completed_test_case: Option<TestCaseComparison>,
}

impl Default for CommandState {
    fn default() -> Self {
        Self {
            job: None,
            cancel: Arc::new(AtomicU8::new(0)),
            modal: false,
            deadline: Duration::from_millis(MAX_COMMAND_DEADLINE_MILLIS),
            outcome: None,
            diagnostics: None,
            selected_diagnostic: None,
            unpublished_capture: None,
            format_status: None,
            pending_console: None,
            console_stdin: None,
            console_live: None,
            is_console: false,
            last_action: None,
            test_case: None,
            test_case_expected: None,
            completed_test_case: None,
        }
    }
}

impl ProductionSession {
    pub fn command_active(&self) -> bool {
        self.effects.0.borrow().command_active
    }

    pub fn command_outcome(&self) -> Option<&CommandOutcome> {
        self.command.outcome.as_ref()
    }

    /// Bounded derived data for the latest command. Absence is explicitly
    /// missing structured evidence, never an empty diagnostic result.
    pub fn command_diagnostics(&self) -> Option<&CommandDiagnostics> {
        self.command.diagnostics.as_ref()
    }

    /// Bounded original evidence retained in memory if every durable capture
    /// publication failed. This is never terminal presentation or durability.
    pub fn unpublished_command_capture(&self) -> Option<&[u8]> {
        self.command.unpublished_capture.as_deref()
    }

    fn verify_command_context(&self) -> Result<()> {
        self.workspace.root_authority().verify_binding()?;
        let a = self.effects.0.borrow();
        a.owner.verify()?;
        if serde_json::from_slice::<SessionMetadata>(
            &a.owner.read_artifact("session.json", METADATA_LIMIT)?,
        )? != self.metadata
            || digest(&a.owner.read_artifact("manifest.toml", METADATA_LIMIT)?)
                != self.metadata.manifest_hash
        {
            return Err(
                "immutable session or assignment metadata changed; preserve and inspect".into(),
            );
        }
        Ok(())
    }

    /// Fixed text only; raw output belongs to evidence, never terminal output.
    pub fn command_status(&self) -> String {
        if self.command.pending_console.is_some() {
            return "Console: confirm overwrite; output is untouched".into();
        }
        if self.command.job.is_none() {
            match self.command.format_status {
                Some(FormatStatus::Applied) => return "Format: changes recorded and saved".into(),
                Some(FormatStatus::NoChange) => return "Format: no changes".into(),
                Some(FormatStatus::Discarded) => {
                    return "Format: failed/cancelled; returned changes discarded".into();
                }
                Some(FormatStatus::Rejected) => {
                    return "Format: unsafe or invalid result rejected; inspect evidence".into();
                }
                None => {}
            }
            if self.command.last_action == Some(CargoAction::Doc)
                && self.command.outcome == Some(CommandOutcome::Exited { code: 0 })
            {
                return format!(
                    "Doc: generated {}",
                    self.workspace.root().join("target/doc").display()
                );
            }
        }
        if self.command.is_console {
            return match &self.command.job {
                Some(Job::Preparing(_)) => "Console: resolving tools (Esc cancels and closes)",
                Some(Job::Running { .. }) if self.command.console_stdin.is_some() => {
                    "Console: running; Enter sends line, Esc cancels and closes"
                }
                Some(Job::Running { .. }) => {
                    "Console: running; input disabled, Esc cancels and closes"
                }
                Some(Job::Reaping { .. }) => "Console: waiting for owned process cleanup",
                Some(Job::Recording { .. }) => "Console: saving evidence",
                Some(Job::CleanupBlocked { .. }) => {
                    "Console: owned process cleanup cannot be confirmed"
                }
                None if self.command_active() => "Console: recovery required",
                None if matches!(
                    self.command.outcome,
                    Some(CommandOutcome::Exited { code: 0 })
                ) =>
                {
                    "Console: exit 0; evidence recorded"
                }
                None if matches!(self.command.outcome, Some(CommandOutcome::Exited { .. })) => {
                    "Console: nonzero exit; evidence recorded"
                }
                None if self.command.outcome.is_some() => "Console: stopped; evidence recorded",
                None => "F9 console",
            }
            .into();
        }
        match &self.command.job {
            Some(Job::Preparing(_)) => "Command: resolving tools (Esc cancel)",
            Some(Job::Running { .. }) => "Command: running (Esc cancel)",
            Some(Job::Reaping { .. }) => "Command: waiting for owned process cleanup",
            Some(Job::Recording { .. }) => "Command: saving evidence",
            Some(Job::CleanupBlocked { .. }) => {
                "Command: owned process cleanup cannot be confirmed"
            }
            None if self.command_active() => "Command: recovery required",
            None if self.command.outcome.is_some() => {
                match self.command.outcome.as_ref().expect("outcome present") {
                    CommandOutcome::Exited { code: 0 } => "Command: exit 0; capture recorded",
                    CommandOutcome::Exited { .. } => "Command: nonzero exit; capture recorded",
                    CommandOutcome::LaunchFailed { .. } => {
                        "Command: launch failed; evidence recorded"
                    }
                    CommandOutcome::Terminated {
                        reason: CommandTermination::Cancelled,
                        ..
                    } => "Command: cancelled; capture recorded",
                    CommandOutcome::Terminated {
                        reason: CommandTermination::Deadline,
                        ..
                    } => "Command: deadline; capture recorded",
                    CommandOutcome::Terminated { .. } => "Command: terminated; capture recorded",
                }
            }
            None => "F7 commands",
        }
        .into()
    }

    pub fn command_error_status(&self) -> Option<String> {
        if self.command_active() {
            return None;
        }
        let command_failed = !matches!(
            self.command.outcome,
            Some(CommandOutcome::Exited { code: 0 }) | None
        );
        if self.command.last_action != Some(CargoAction::Format)
            && let Some(result) = self.command.diagnostics.as_ref()
        {
            let errors = result
                .diagnostics
                .iter()
                .filter(|diagnostic| {
                    matches!(
                        diagnostic.level.as_str(),
                        "error" | "error: internal compiler error"
                    )
                })
                .count();
            let warnings = result
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.level == "warning")
                .count();
            if errors > 0 || (command_failed && warnings > 0) {
                return Some(format!(
                    "{} · {}",
                    diagnostic_count(errors, "error"),
                    diagnostic_count(warnings, "warning")
                ));
            }
        }
        if command_failed {
            Some(self.command_status())
        } else {
            None
        }
    }

    /// A rejected formatter tree is a completed safety decision, not an
    /// operational command failure. Keep this narrow so the UI never exposes
    /// arbitrary error details or hides failures that retain command ownership.
    pub(crate) fn completed_format_rejection(&self) -> bool {
        self.command.job.is_none()
            && !self.command_active()
            && self.command.format_status == Some(FormatStatus::Rejected)
            && self.command.outcome == Some(CommandOutcome::Exited { code: 0 })
    }

    /// The UI marks its local prompts; workspace confirmations are checked too.
    pub fn set_command_modal(&mut self, modal: bool) {
        if modal || self.command.pending_console.is_some() {
            self.clear_completion();
        }
        self.command.modal = modal || self.command.pending_console.is_some();
    }

    pub fn set_command_deadline(&mut self, deadline: Duration) -> Result<()> {
        self.require_command_idle()?;
        if deadline < Duration::from_millis(1)
            || deadline > Duration::from_millis(MAX_COMMAND_DEADLINE_MILLIS)
        {
            return Err("command deadline must be between 1 and 300000 milliseconds".into());
        }
        self.command.deadline = deadline;
        Ok(())
    }

    pub fn start_command(&mut self, action: CargoAction) -> Result<()> {
        self.clear_completion();
        self.require_command_idle()?;
        let manifest = self.command_manifest()?;
        let argv = match action {
            CargoAction::Build => return Err("Build is not an allowed Cargo command".into()),
            CargoAction::Check => manifest.commands.check,
            CargoAction::Test => manifest.commands.test,
            CargoAction::Run => manifest.commands.run,
            CargoAction::Clippy => manifest.commands.clippy,
            CargoAction::Format => manifest.commands.format,
            CargoAction::Doc => vec!["cargo".into(), "doc".into()],
            CargoAction::Add | CargoAction::Remove => {
                return Err("dependency action requires a crate name".into());
            }
            CargoAction::Update => vec!["cargo".into(), "update".into()],
        };
        cargo_policy::validate_command(action, &argv)?;
        self.begin_command(manifest.toolchain, action, argv, None)
    }

    pub fn start_dependency_command(
        &mut self,
        action: CargoAction,
        dependency: Option<&str>,
    ) -> Result<()> {
        self.clear_completion();
        self.require_command_idle()?;
        let manifest = self.command_manifest()?;
        let mut argv = vec!["cargo".into(), action.subcommand().into()];
        match (action, dependency) {
            (CargoAction::Add | CargoAction::Remove, Some(value)) => argv.push(value.into()),
            (CargoAction::Update, None) => {}
            _ => return Err("dependency action has invalid arguments".into()),
        }
        cargo_policy::validate_command(action, &argv)?;
        self.begin_command(manifest.toolchain, action, argv, None)
    }

    pub fn start_console_command(&mut self, input: &str) -> Result<ConsoleStart> {
        self.clear_completion();
        let request = cargo_policy::parse_console_command(input)?;
        self.require_command_idle()?;
        let manifest = self.command_manifest()?;
        let action = request.action;
        let argv = request.argv.clone();
        let launch = self.prepare_console_launch(request)?;
        if launch.output == Some(OutputDisposition::Overwrite) {
            let path = launch
                .request
                .stdout
                .clone()
                .expect("overwrite route has a path");
            self.command.pending_console = Some(PendingConsole {
                toolchain: manifest.toolchain,
                launch,
            });
            self.command.modal = true;
            return Ok(ConsoleStart::OverwriteConfirmation { path });
        }
        self.begin_command(manifest.toolchain, action, argv, Some(launch))?;
        Ok(ConsoleStart::Started)
    }

    pub fn console_overwrite_pending(&self) -> bool {
        self.command.pending_console.is_some()
    }

    pub fn confirm_console_overwrite(&mut self) -> Result<()> {
        let pending = self
            .command
            .pending_console
            .take()
            .ok_or("no console overwrite confirmation is pending")?;
        self.command.modal = false;
        let action = pending.launch.request.action;
        let argv = pending.launch.request.argv.clone();
        self.begin_command(pending.toolchain, action, argv, Some(pending.launch))
    }

    pub fn cancel_console_overwrite(&mut self) -> bool {
        let cancelled = self.command.pending_console.take().is_some();
        if cancelled {
            self.command.modal = false;
        }
        cancelled
    }

    pub fn console_command_active(&self) -> bool {
        self.command.is_console && self.command.test_case.is_none() && self.command_active()
    }

    pub fn console_accepts_stdin(&self) -> bool {
        self.console_command_active() && self.command.console_stdin.is_some()
    }

    pub fn submit_console_line(&mut self, line: &str) -> Result<bool> {
        if line.len() > crate::console::MAX_CONSOLE_LINE_BYTES || line.chars().any(char::is_control)
        {
            return Err("console input line is invalid or exceeds 4096 bytes".into());
        }
        let sender = self
            .command
            .console_stdin
            .as_ref()
            .ok_or("the active command does not accept console stdin")?;
        match sender.try_send(StdinMessage::Line(line.to_owned())) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => {
                self.command.console_stdin = None;
                Err("child stdin is closed".into())
            }
        }
    }

    pub fn close_console_stdin(&mut self) -> Result<bool> {
        let Some(sender) = self.command.console_stdin.take() else {
            return Ok(false);
        };
        match sender.try_send(StdinMessage::Eof) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => {
                self.command.console_stdin = Some(sender);
                Ok(false)
            }
            Err(TrySendError::Disconnected(_)) => Ok(false),
        }
    }

    pub fn console_output(&self) -> Vec<u8> {
        self.command
            .console_live
            .as_ref()
            .map_or_else(Vec::new, LiveOutput::snapshot)
    }

    pub fn list_test_cases(&self) -> Result<Vec<TestCase>> {
        TestCaseDirectory::open(self.workspace.root())?.list_cases()
    }

    pub fn start_test_case(&mut self, case: TestCase) -> Result<()> {
        self.clear_completion();
        self.require_command_idle()?;
        let manifest = self.command_manifest()?;
        let cases = TestCaseDirectory::open(self.workspace.root())?;
        let input = cases.open_input(&case.input_path())?;
        // Reject bad expected files before launch by design. ExpectedUnreadable and
        // ExpectedOversized remain in the closed vocabulary for stream acceptance only.
        let expected = cases.read_expected(&case)?;
        let request = ConsoleCommand {
            action: CargoAction::Run,
            argv: vec!["cargo".into(), "run".into()],
            stdin: Some(case.input_path()),
            stdout: None,
        };
        let action = request.action;
        let argv = request.argv.clone();
        let launch = ConsoleLaunch {
            request,
            cases: Some(cases),
            input: Some(input),
            output: None,
        };
        self.begin_command(manifest.toolchain, action, argv, Some(launch))?;
        self.command.test_case = Some(case);
        self.command.test_case_expected = Some(Ok(expected));
        Ok(())
    }

    pub fn take_test_case_result(&mut self) -> Option<TestCaseComparison> {
        self.command.completed_test_case.take()
    }

    pub(crate) fn test_case_active(&self) -> bool {
        self.command.test_case.is_some() && self.command_active()
    }

    fn command_manifest(&self) -> Result<AssignmentManifest> {
        self.effects.0.borrow().healthy()?;
        self.verify_command_context()?;
        if self.command.modal || self.workspace.confirmation_pending() || self.external_pending() {
            return Err("finish the current modal or recovery decision before a command".into());
        }
        let bytes = self
            .effects
            .0
            .borrow()
            .owner
            .read_artifact("manifest.toml", METADATA_LIMIT)?;
        if digest(&bytes) != self.metadata.manifest_hash {
            return Err("assignment identity changed".into());
        }
        let manifest = AssignmentManifest::parse(&bytes)?;
        Ok(manifest)
    }

    fn prepare_console_launch(&self, request: ConsoleCommand) -> Result<ConsoleLaunch> {
        let needs_cases = request.stdin.is_some() || request.stdout.is_some();
        let cases = needs_cases
            .then(|| TestCaseDirectory::open(self.workspace.root()))
            .transpose()?;
        let input = match (&cases, &request.stdin) {
            (Some(cases), Some(path)) => Some(cases.open_input(path)?),
            _ => None,
        };
        let output = match (&cases, &request.stdout) {
            (Some(cases), Some(path)) => Some(cases.output_disposition(path)?),
            _ => None,
        };
        Ok(ConsoleLaunch {
            request,
            cases,
            input,
            output,
        })
    }

    fn begin_command(
        &mut self,
        toolchain: String,
        action: CargoAction,
        argv: Vec<String>,
        console: Option<ConsoleLaunch>,
    ) -> Result<()> {
        let is_console = console.is_some();
        let output_limit = {
            let a = self.effects.0.borrow();
            let used = a
                .replay
                .as_ref()
                .ok_or("missing replay")?
                .command_output_bytes();
            let remaining = a.budgets.output_per_session.saturating_sub(used);
            let limit = remaining
                .min(a.budgets.output_per_command)
                .min(MAX_COMMAND_OUTPUT_BYTES);
            if limit == 0 {
                return Err("command output budget exhausted".into());
            }
            a.headroom(limit.saturating_mul(8).saturating_add(80 * 1024 * 1024))?;
            if a.sequence
                .saturating_add(limit.div_ceil(MAX_COMMAND_CHUNK_BYTES as u64))
                .saturating_add(if action == CargoAction::Format || action.is_dependency() {
                    rustrace_workspace::hash::MAX_WORKSPACE_FILES as u64 + 16
                } else {
                    16
                })
                > a.budgets.events
            {
                return Err("insufficient event budget for command evidence".into());
            }
            limit
        };
        self.recheck_external()?;
        self.verify_command_context()?;
        self.stop_language_service_for_command()?;
        // Stopping a workspace-CWD process can expose one final disk effect.
        // Reconcile only after confirmed cleanup, before durable ownership or
        // either command resolver/child can start.
        self.recheck_external()?;
        self.verify_command_context()?;
        let root = self.workspace.root().to_path_buf();
        let cancel = Arc::new(AtomicU8::new(0));
        let signal = cancel.clone();
        self.effects.0.borrow_mut().command_active = true;
        self.record_command_activity(true)?;
        process_probe("command-preparation");
        let worker = std::thread::Builder::new()
            .name("rustrace-command-resolve".into())
            .spawn(move || resolve(root, toolchain, action, argv, output_limit, signal, console))
            .inspect_err(|_| {
                self.effects.0.borrow_mut().poison =
                    Some("command worker did not start; preserve activity evidence".into());
            })?;
        self.command.cancel = cancel;
        self.command.outcome = None;
        self.command.is_console = is_console;
        self.command.last_action = Some(action);
        self.command.console_stdin = None;
        self.command.console_live = None;
        if action != CargoAction::Format {
            self.command.diagnostics = None;
            self.command.selected_diagnostic = None;
        }
        self.command.format_status = None;
        self.command.job = Some(Job::Preparing(worker));
        Ok(())
    }

    fn record_command_activity(&self, active: bool) -> Result<()> {
        let mut a = self.effects.0.borrow_mut();
        let result = (|| {
            let marker = CommandActivity {
                version: 1,
                session_id: self.metadata.session_id.clone(),
                sequence: a.sequence,
                event_hash: a.hash,
                active,
            };
            a.owner.publish_artifact(
                "command-activity.json",
                &serde_json::to_vec(&marker)?,
                true,
            )?;
            Ok(())
        })();
        a.fail(result)
    }

    pub fn cancel_command(&mut self) {
        let _ = self
            .command
            .cancel
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
    }

    pub(super) fn cancel_command_for_quit(&mut self) {
        self.command.cancel.store(2, Ordering::Release);
    }

    pub(super) fn command_quit_requested(&self) -> bool {
        self.command.cancel.load(Ordering::Acquire) == 2
    }

    #[cfg(test)]
    pub(crate) fn install_stalled_console_preparing_for_test(
        &mut self,
        stall: Duration,
    ) -> Result<(Arc<AtomicU8>, Arc<AtomicBool>)> {
        let signals = self.install_stalled_preparing_for_test(stall)?;
        self.command.is_console = true;
        Ok(signals)
    }

    #[cfg(test)]
    pub(crate) fn install_stalled_preparing_for_test(
        &mut self,
        stall: Duration,
    ) -> Result<(Arc<AtomicU8>, Arc<AtomicBool>)> {
        let cancel = Arc::new(AtomicU8::new(0));
        let finished = Arc::new(AtomicBool::new(false));
        let completion = finished.clone();
        let report = ToolchainReport {
            assignment_pin: None,
            selected_toolchain: None,
            working_directory: self.workspace.root().to_path_buf(),
            probes: Vec::new(),
        };
        self.effects.0.borrow_mut().command_active = true;
        self.record_command_activity(true)?;
        let worker = std::thread::Builder::new()
            .name("rustrace-stalled-command-test".into())
            .spawn(move || {
                std::thread::sleep(stall);
                completion.store(true, Ordering::Release);
                Resolution {
                    result: Err("test stalled command preparation".into()),
                    report,
                    captures: Vec::new(),
                }
            })?;
        self.command.cancel = cancel.clone();
        self.command.job = Some(Job::Preparing(worker));
        Ok((cancel, finished))
    }

    #[cfg(all(test, unix))]
    pub(super) fn install_cleanup_uncertain_running_for_test(&mut self) -> Result<Arc<AtomicBool>> {
        let group_reaped = Arc::new(AtomicBool::new(false));
        let (before, millis) = {
            let a = self.effects.0.borrow();
            (
                a.replay
                    .as_ref()
                    .and_then(ReplayEngine::command_tree_link)
                    .ok_or("missing pre-tree")?
                    .clone(),
                a.persisted_millis,
            )
        };
        let start = ControlledCommandStarted {
            command_id: CommandId::new("command-cleanup-uncertain")?,
            action: ControlledAction::Run,
            argv: vec!["/test/rustup".into(), "run".into(), "fixture".into()],
            environment: CommandEnvironment {
                policy_version: 1,
                retained_names: Vec::new(),
            },
            selected_toolchain: "fixture".into(),
            tools: Vec::new(),
            before,
            deadline_millis: 1,
            output_limit: 1,
            console: None,
        };
        let confirmation = group_reaped.clone();
        let worker = std::thread::Builder::new()
            .name("rustrace-cleanup-uncertain-test".into())
            .spawn(move || ProcessResult {
                outcome: CommandOutcome::Terminated {
                    reason: CommandTermination::CleanupFailure,
                    signal: Some(libc::SIGKILL),
                },
                stdout: command_process::CapturedStream {
                    bytes: Vec::new(),
                    completeness: CaptureCompleteness::ReadFailed,
                },
                stderr: command_process::CapturedStream {
                    bytes: Vec::new(),
                    completeness: CaptureCompleteness::ReadFailed,
                },
                completed_at: Instant::now(),
                cleanup_confirmed: false,
                cleanup_os_code: None,
                pending_cleanup: Some(PendingProcessCleanup::for_test(confirmation)),
            })?;
        self.effects.0.borrow_mut().command_active = true;
        self.record_command_activity(true)?;
        self.command.cancel = Arc::new(AtomicU8::new(0));
        self.command.job = Some(Job::Running {
            worker,
            panic_cleanup: ProcessCleanupHandoff::new(),
            start,
            millis,
            tool_run: None,
        });
        Ok(group_reaped)
    }

    #[cfg(test)]
    fn begin_test_command(
        &mut self,
        deadline_millis: u64,
    ) -> Result<(ControlledCommandStarted, u64)> {
        self.begin_test_command_with_case(deadline_millis, None)
    }

    #[cfg(test)]
    fn begin_test_command_with_case(
        &mut self,
        deadline_millis: u64,
        case: Option<&str>,
    ) -> Result<(ControlledCommandStarted, u64)> {
        self.effects.0.borrow_mut().command_active = true;
        self.record_command_activity(true)?;
        self.command_checkpoint()?;
        let mut a = self.effects.0.borrow_mut();
        let mut start = ControlledCommandStarted {
            command_id: CommandId::new(format!("command-{}", a.sequence + 1))?,
            action: if case.is_some() {
                ControlledAction::Run
            } else {
                ControlledAction::Check
            },
            argv: vec![
                "/test/rustup".into(),
                "run".into(),
                "fixture".into(),
                "/test/cargo".into(),
                if case.is_some() {
                    "run".into()
                } else {
                    "check".into()
                },
            ],
            environment: CommandEnvironment {
                policy_version: 1,
                retained_names: Vec::new(),
            },
            selected_toolchain: "fixture".into(),
            tools: vec![
                CommandTool {
                    component: CommandToolKind::Rustup,
                    executable: "/test/rustup".into(),
                    version: "rustup fixture".into(),
                },
                CommandTool {
                    component: CommandToolKind::Rustc,
                    executable: "/test/rustc".into(),
                    version: "rustc fixture".into(),
                },
                CommandTool {
                    component: CommandToolKind::Cargo,
                    executable: "/test/cargo".into(),
                    version: "cargo fixture".into(),
                },
                CommandTool {
                    component: CommandToolKind::Rustdoc,
                    executable: "/test/rustdoc".into(),
                    version: "rustdoc fixture".into(),
                },
            ],
            before: a
                .replay
                .as_ref()
                .and_then(ReplayEngine::command_tree_link)
                .ok_or("missing test pre-tree")?
                .clone(),
            deadline_millis,
            output_limit: 1,
            console: case.map(|case| ConsoleCommandRoute {
                stdin: ConsoleStdinRoute::File {
                    path: WorkspacePath::new(format!("{case}.in")).unwrap(),
                },
                stdout: ConsoleStdoutRoute::Console,
            }),
        };
        if case.is_some() {
            start.argv.push("--frozen".into());
        }
        start.validate()?;
        a.append(Event::ControlledCommandStarted(start.clone()))?;
        let millis = a.persisted_millis;
        Ok((start, millis))
    }

    #[cfg(test)]
    pub(super) fn install_compared_finish_for_test(
        &mut self,
        comparison: TestCaseComparison,
    ) -> Result<()> {
        let (start, millis) = self.begin_test_command_with_case(1, Some("input"))?;
        let capture = CommandCapture {
            bytes: 0,
            completeness: CaptureCompleteness::Complete,
            mode: CommandCaptureMode::Captured,
        };
        self.command.job = Some(Job::Recording {
            chunks: VecDeque::new(),
            finish: ControlledCommandFinished {
                command_id: start.command_id,
                after: start.before,
                started_millis: millis,
                finished_millis: millis,
                outcome: CommandOutcome::Exited { code: 0 },
                stdout: capture.clone(),
                stderr: capture,
            },
            tool_run: None,
            test_case_result: Some(comparison),
        });
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn checkpointed_recording_finish_for_test(
        &mut self,
    ) -> Result<ControlledCommandFinished> {
        self.command_checkpoint()?;
        let mut finish = self
            .recording_finish_for_test()
            .ok_or("missing recording finish")?;
        finish.after = self
            .effects
            .0
            .borrow()
            .replay
            .as_ref()
            .and_then(ReplayEngine::command_tree_link)
            .ok_or("missing recording post-tree")?
            .clone();
        Ok(finish)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn install_process_result_tuple_for_test(
        &mut self,
        cleanup_confirmed: bool,
        with_pending_cleanup: bool,
    ) -> Result<(Arc<AtomicBool>, Arc<AtomicBool>)> {
        let (start, millis) = self.begin_test_command(1)?;
        let confirmation = Arc::new(AtomicBool::new(false));
        let dropped_while_armed = Arc::new(AtomicBool::new(false));
        let pending_cleanup = with_pending_cleanup.then(|| {
            PendingProcessCleanup::for_test_with_drop_probe(
                confirmation.clone(),
                dropped_while_armed.clone(),
            )
        });
        let worker = std::thread::Builder::new()
            .name("rustrace-process-result-tuple-test".into())
            .spawn(move || ProcessResult {
                outcome: CommandOutcome::Terminated {
                    reason: CommandTermination::CleanupFailure,
                    signal: Some(libc::SIGKILL),
                },
                stdout: command_process::CapturedStream {
                    bytes: Vec::new(),
                    completeness: CaptureCompleteness::Complete,
                },
                stderr: command_process::CapturedStream {
                    bytes: Vec::new(),
                    completeness: CaptureCompleteness::Complete,
                },
                completed_at: Instant::now(),
                cleanup_confirmed,
                cleanup_os_code: None,
                pending_cleanup,
            })?;
        self.command.cancel = Arc::new(AtomicU8::new(0));
        self.command.job = Some(Job::Running {
            worker,
            panic_cleanup: ProcessCleanupHandoff::new(),
            start,
            millis,
            tool_run: None,
        });
        Ok((confirmation, dropped_while_armed))
    }

    #[cfg(all(test, unix))]
    pub(crate) fn install_panicking_running_for_test(
        &mut self,
        command: std::process::Command,
        spawned: Arc<AtomicI32>,
    ) -> Result<()> {
        let (start, millis) = self.begin_test_command(3_000)?;
        let cancel = Arc::new(AtomicU8::new(0));
        let signal = cancel.clone();
        let panic_cleanup = ProcessCleanupHandoff::new();
        let worker_cleanup = panic_cleanup.clone();
        let worker = std::thread::Builder::new()
            .name("rustrace-panicking-command-test".into())
            .spawn(move || {
                command_process::execute_panicking_after_spawn_for_test(
                    command,
                    ProcessLimits {
                        deadline: Duration::from_secs(3),
                        output_bytes: 1024,
                    },
                    signal,
                    spawned,
                    worker_cleanup,
                )
            })?;
        self.command.cancel = cancel;
        self.command.job = Some(Job::Running {
            worker,
            panic_cleanup,
            start,
            millis,
            tool_run: None,
        });
        Ok(())
    }

    #[cfg(all(test, unix))]
    pub(crate) fn install_full_capture_running_for_test(
        &mut self,
        command: std::process::Command,
        deadline: Duration,
        pre_spawn_delay: Duration,
    ) -> Result<()> {
        let deadline_millis = deadline.as_millis() as u64;
        let (start, millis) = self.begin_test_command(deadline_millis)?;
        std::thread::sleep(pre_spawn_delay);
        let cancel = Arc::new(AtomicU8::new(0));
        let signal = cancel.clone();
        let panic_cleanup = ProcessCleanupHandoff::new();
        let worker_cleanup = panic_cleanup.clone();
        let worker = std::thread::Builder::new()
            .name("rustrace-full-capture-test".into())
            .spawn(move || {
                command_process::execute_with_handoff(
                    command,
                    ProcessLimits {
                        deadline,
                        output_bytes: 1,
                    },
                    signal,
                    worker_cleanup,
                )
            })?;
        self.command.cancel = cancel;
        self.command.job = Some(Job::Running {
            worker,
            panic_cleanup,
            start,
            millis,
            tool_run: None,
        });
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn recording_finish_for_test(&self) -> Option<ControlledCommandFinished> {
        match &self.command.job {
            Some(Job::Recording { finish, .. }) => Some(finish.clone()),
            _ => None,
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn install_uncertain_resolver_for_test(
        &mut self,
    ) -> Result<(Arc<AtomicBool>, Arc<AtomicBool>)> {
        let confirmation = Arc::new(AtomicBool::new(false));
        let dropped_while_armed = Arc::new(AtomicBool::new(false));
        let worker_confirmation = confirmation.clone();
        let worker_dropped = dropped_while_armed.clone();
        let cancel = Arc::new(AtomicU8::new(0));
        let worker_cancel = cancel.clone();
        let root = self.workspace.root().to_path_buf();
        let worker = std::thread::Builder::new()
            .name("rustrace-uncertain-resolver-test".into())
            .spawn(move || {
                let cleanup_uncertain = std::cell::Cell::new(false);
                let captures = RefCell::new(Vec::new());
                let result = ProcessResult {
                    outcome: CommandOutcome::Terminated {
                        reason: CommandTermination::CleanupFailure,
                        signal: Some(libc::SIGKILL),
                    },
                    stdout: command_process::CapturedStream {
                        bytes: Vec::new(),
                        completeness: CaptureCompleteness::ReadFailed,
                    },
                    stderr: command_process::CapturedStream {
                        bytes: Vec::new(),
                        completeness: CaptureCompleteness::ReadFailed,
                    },
                    completed_at: Instant::now(),
                    cleanup_confirmed: false,
                    cleanup_os_code: None,
                    pending_cleanup: Some(PendingProcessCleanup::for_test_with_drop_probe(
                        worker_confirmation,
                        worker_dropped,
                    )),
                };
                let _ = observe_resolution_process(
                    result,
                    Duration::from_millis(1),
                    &cleanup_uncertain,
                    &captures,
                    &worker_cancel,
                );
                Resolution {
                    result: Err(if cleanup_uncertain.get() {
                        "cleanup uncertain during test tool discovery".into()
                    } else {
                        "test tool discovery stopped".into()
                    }),
                    report: ToolchainReport {
                        assignment_pin: None,
                        selected_toolchain: None,
                        working_directory: root,
                        probes: Vec::new(),
                    },
                    captures: captures.into_inner(),
                }
            })?;
        self.effects.0.borrow_mut().command_active = true;
        self.record_command_activity(true)?;
        self.command.cancel = cancel;
        self.command.job = Some(Job::Preparing(worker));
        Ok((confirmation, dropped_while_armed))
    }

    #[cfg(test)]
    pub(super) fn replay_invalid_finish_for_test(&mut self) -> Result<ControlledCommandFinished> {
        let (start, millis) = self.begin_test_command(1)?;
        self.command_checkpoint()?;
        let after = self
            .effects
            .0
            .borrow()
            .replay
            .as_ref()
            .and_then(ReplayEngine::command_tree_link)
            .ok_or("missing test post-tree")?
            .clone();
        let finish = ControlledCommandFinished {
            command_id: start.command_id,
            after,
            started_millis: millis.saturating_add(1),
            finished_millis: millis.saturating_add(1),
            outcome: CommandOutcome::Exited { code: 0 },
            stdout: CommandCapture {
                bytes: 0,
                completeness: CaptureCompleteness::Complete,
                mode: CommandCaptureMode::Captured,
            },
            stderr: CommandCapture {
                bytes: 0,
                completeness: CaptureCompleteness::Complete,
                mode: CommandCaptureMode::Captured,
            },
        };
        finish.validate()?;
        Ok(finish)
    }

    pub(crate) fn command_terminal_restoration_safe(&self) -> bool {
        matches!(self.command.job, None | Some(Job::Recording { .. }))
    }

    pub(super) fn shutdown_command(&mut self) -> Result<()> {
        self.command.pending_console = None;
        self.command.modal = false;
        self.cancel_command_for_quit();
        let until = Instant::now() + COMMAND_QUIT_POLL_LIMIT;
        let mut failure = None;
        while self.command.job.is_some() {
            if Instant::now() >= until {
                let message = match &self.command.job {
                    Some(Job::Preparing(_)) => {
                        // The resolver only returns data. Quit cancellation
                        // prevents assignment launch while this owned worker is
                        // retained for a later bounded cleanup turn.
                        "command preparation did not finish within 2500 ms; cancellation and unfinished activity remain preserved"
                    }
                    Some(Job::Recording { .. }) => {
                        // The process worker is already joined, but incomplete
                        // persistence must remain explicit and unrecoverable as
                        // an ordinary resume.
                        "command recording did not finish within 2500 ms; unfinished activity remains preserved"
                    }
                    Some(Job::Running { .. }) => {
                        "command worker did not finish cleanup within 2500 ms; owned process cleanup remains pending"
                    }
                    Some(Job::Reaping { .. }) => {
                        "owned process cleanup did not finish within 2500 ms; cleanup remains pending"
                    }
                    Some(Job::CleanupBlocked { .. }) => {
                        "owned process cleanup cannot be confirmed; guarded terminal state remains active"
                    }
                    None => break,
                };
                if matches!(self.command.job, Some(Job::Recording { .. })) {
                    self.command.job = None;
                }
                return Err(message.into());
            }
            if let Err(error) = self.poll_command() {
                failure.get_or_insert(error);
            }
            if self.command.job.is_some() {
                std::thread::sleep(
                    Duration::from_millis(2).min(until.saturating_duration_since(Instant::now())),
                );
            }
        }
        if let Some(error) = failure {
            Err(error)
        } else {
            Ok(())
        }
    }

    pub fn poll_command(&mut self) -> Result<bool> {
        let Some(job) = self.command.job.take() else {
            return Ok(false);
        };
        let result = self.advance_command(job);
        if let Err(error) = &result
            && self.command_active()
        {
            self.effects.0.borrow_mut().poison.get_or_insert_with(|| {
                format!("command evidence incomplete; preserve and inspect: {error}")
            });
        }
        result
    }

    fn advance_command(&mut self, job: Job) -> Result<bool> {
        match job {
            Job::Preparing(worker) if !worker.is_finished() => {
                self.command.job = Some(Job::Preparing(worker));
                Ok(false)
            }
            Job::Preparing(worker) => {
                let resolution = worker.join().map_err(|_| "tool resolver worker panicked")?;
                {
                    let a = self.effects.0.borrow();
                    let observation = serde_json::to_vec(
                        &serde_json::json!({"version":1,"session_id":self.metadata.session_id,
                        "sequence":a.sequence,"event_hash":a.hash,"report":resolution.report,"captures":resolution.captures}),
                    )?;
                    if observation.len() > ARTIFACT_LIMIT {
                        return Err("resolution evidence exceeds artifact bound".into());
                    }
                    let name = format!("command-resolution-{:020}.json", a.sequence);
                    drop(a);
                    self.publish_command_artifact(&name, observation)?;
                }
                self.verify_command_context()?;
                let ready = match resolution.result {
                    Ok(ready) => ready,
                    Err(detail) => {
                        // No command boundary or child was launched. Resolver
                        // cleanup uncertainty uses a distinct failure prefix.
                        if detail.starts_with("cleanup uncertain") {
                            self.command.job = Some(Job::CleanupBlocked {
                                command: None,
                                detail: detail.clone(),
                            });
                            return Err(detail.into());
                        }
                        self.save_all_with_hook(|_| {})?;
                        self.command_checkpoint()?;
                        self.clear_command_ownership()?;
                        self.complete_test_case_error("command preparation failed");
                        return Err(detail.into());
                    }
                };
                if self.command.cancel.load(Ordering::Acquire) != 0 {
                    self.save_all_with_hook(|_| {})?;
                    self.command_checkpoint()?;
                    self.clear_command_ownership()?;
                    self.complete_test_case_error("cancelled before launch");
                    return Ok(true);
                }
                self.launch_ready(ready)?;
                Ok(true)
            }
            Job::Running {
                worker,
                panic_cleanup,
                start,
                millis,
                tool_run,
            } if !worker.is_finished() => {
                self.command.job = Some(Job::Running {
                    worker,
                    panic_cleanup,
                    start,
                    millis,
                    tool_run,
                });
                Ok(false)
            }
            Job::Running {
                worker,
                panic_cleanup,
                start,
                millis,
                tool_run,
            } => {
                self.finish_running_command(worker, panic_cleanup, start, millis, tool_run)?;
                Ok(true)
            }
            Job::Reaping {
                mut cleanup,
                mut result,
                start,
                millis,
                tool_run,
            } => {
                let progress = cleanup.poll();
                if !progress.confirmed {
                    if progress.os_code.is_some() {
                        result.cleanup_os_code = progress.os_code;
                    }
                    self.command.job = Some(Job::Reaping {
                        cleanup,
                        result,
                        start,
                        millis,
                        tool_run,
                    });
                    return Ok(false);
                }
                result.cleanup_confirmed = true;
                result.cleanup_os_code = progress.os_code.or(result.cleanup_os_code);
                self.preserve_capture(start, millis, result, tool_run)?;
                Ok(true)
            }
            Job::Recording {
                mut chunks,
                finish,
                tool_run,
                test_case_result,
            } => {
                for _ in 0..2 {
                    let Some(chunk) = chunks.pop_front() else {
                        break;
                    };
                    self.effects
                        .0
                        .borrow_mut()
                        .append(Event::ControlledCommandOutput(chunk))?;
                }
                if !chunks.is_empty() {
                    self.command.job = Some(Job::Recording {
                        chunks,
                        finish,
                        tool_run,
                        test_case_result,
                    });
                    return Ok(true);
                }
                let tool_warning = match tool_run {
                    Some(ToolRun::Format(run)) => self.finalize_format(&finish, run)?,
                    Some(ToolRun::Dependency(run)) => self.finalize_dependency(&finish, run)?,
                    None => None,
                };
                self.finish_command(finish, test_case_result.as_ref())?;
                self.command.test_case = None;
                self.command.test_case_expected = None;
                self.command.completed_test_case = test_case_result;
                if let Some(warning) = tool_warning {
                    return Err(warning.into());
                }
                Ok(true)
            }
            Job::CleanupBlocked { command, detail } => {
                self.command.job = Some(Job::CleanupBlocked { command, detail });
                Ok(false)
            }
        }
    }

    fn finish_running_command(
        &mut self,
        worker: JoinHandle<ProcessResult>,
        panic_cleanup: ProcessCleanupHandoff,
        start: ControlledCommandStarted,
        millis: u64,
        tool_run: Option<ToolRun>,
    ) -> Result<()> {
        self.command.console_stdin = None;
        let mut result = match worker.join() {
            Ok(result) => result,
            Err(_) => {
                let result = ProcessResult {
                    outcome: CommandOutcome::Terminated {
                        reason: CommandTermination::CleanupFailure,
                        signal: None,
                    },
                    stdout: command_process::CapturedStream {
                        bytes: Vec::new(),
                        completeness: CaptureCompleteness::Unavailable,
                    },
                    stderr: command_process::CapturedStream {
                        bytes: Vec::new(),
                        completeness: CaptureCompleteness::Unavailable,
                    },
                    completed_at: Instant::now(),
                    cleanup_confirmed: false,
                    cleanup_os_code: None,
                    pending_cleanup: None,
                };
                return match panic_cleanup.take_after_panic() {
                    PanickedProcessCleanup::ConfirmedOrNotSpawned => {
                        let mut result = result;
                        result.cleanup_confirmed = true;
                        self.preserve_capture(start, millis, result, tool_run)
                    }
                    #[cfg(unix)]
                    PanickedProcessCleanup::Pending(cleanup) => {
                        self.command.job = Some(Job::Reaping {
                            cleanup,
                            result,
                            start,
                            millis,
                            tool_run,
                        });
                        Ok(())
                    }
                    PanickedProcessCleanup::Ambiguous => {
                        self.command.job = Some(Job::CleanupBlocked {
                            command: Some(BlockedCommand {
                                _result: result,
                                _start: start,
                                _millis: millis,
                                _tool_run: tool_run,
                            }),
                            detail: "command worker panicked with ambiguous process ownership"
                                .into(),
                        });
                        Err("command worker panicked with ambiguous process ownership".into())
                    }
                };
            }
        };
        if let Some(cleanup) = result.pending_cleanup.take() {
            if result.cleanup_confirmed {
                result.cleanup_confirmed = false;
                result.outcome = CommandOutcome::Terminated {
                    reason: CommandTermination::CleanupFailure,
                    signal: None,
                };
            }
            self.command.job = Some(Job::Reaping {
                cleanup,
                result,
                start,
                millis,
                tool_run,
            });
            Ok(())
        } else if result.cleanup_confirmed {
            self.preserve_capture(start, millis, result, tool_run)
        } else {
            self.command.job = Some(Job::CleanupBlocked {
                command: Some(BlockedCommand {
                    _result: result,
                    _start: start,
                    _millis: millis,
                    _tool_run: tool_run,
                }),
                detail: "process cleanup result lost owned cleanup authority".into(),
            });
            Err("process cleanup result lost owned cleanup authority".into())
        }
    }

    fn launch_ready(&mut self, mut ready: Ready) -> Result<()> {
        self.verify_command_context()?;
        self.save_all_with_hook(|_| {})?;
        self.command_checkpoint()?;
        let tool_run = if let Some(format_prepare) = ready.format_prepare.take() {
            match self.prepare_format_run(format_prepare) {
                Ok((prepared, run)) => {
                    ready.prepared = prepared;
                    Some(ToolRun::Format(run))
                }
                Err(error) => {
                    self.clear_command_ownership()?;
                    return Err(error);
                }
            }
        } else if let Some(dependency_prepare) = ready.dependency_prepare.take() {
            match self.prepare_dependency_run(dependency_prepare) {
                Ok((prepared, run)) => {
                    ready.prepared = prepared;
                    Some(ToolRun::Dependency(run))
                }
                Err(error) => {
                    self.clear_command_ownership()?;
                    return Err(error);
                }
            }
        } else {
            None
        };
        let console = match ready.console.take().map(prepare_console_io).transpose() {
            Ok(console) => console,
            Err(error) => {
                self.clear_command_ownership()?;
                return Err(error);
            }
        };
        let mut a = self.effects.0.borrow_mut();
        let argv = std::iter::once(ready.prepared.command.get_program())
            .chain(ready.prepared.command.get_args())
            .map(|arg| {
                arg.to_str()
                    .map(str::to_owned)
                    .ok_or("non-UTF-8 prepared argv")
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let start = ControlledCommandStarted {
            command_id: CommandId::new(format!("command-{}", a.sequence + 1))?,
            action: action_model(ready.action),
            argv,
            environment: CommandEnvironment {
                policy_version: ready.prepared.summary.policy_version,
                retained_names: ready
                    .prepared
                    .summary
                    .retained_names
                    .iter()
                    .map(|name| environment_name(name))
                    .collect::<Result<Vec<_>>>()?,
            },
            selected_toolchain: ready
                .report
                .selected_toolchain
                .clone()
                .ok_or("no selected toolchain")?,
            tools: ready.tools,
            before: a
                .replay
                .as_ref()
                .and_then(ReplayEngine::command_tree_link)
                .ok_or("missing pre-tree")?
                .clone(),
            deadline_millis: self.command.deadline.as_millis() as u64,
            output_limit: ready.output_limit,
            console: console.as_ref().map(|console| console.route.clone()),
        };
        start.validate()?;
        process_probe("command-prestart");
        a.append(Event::ControlledCommandStarted(start.clone()))?;
        process_probe("command-start");
        let millis = a.persisted_millis;
        drop(a);
        if let Some(ToolRun::Format(run)) = &tool_run {
            self.publish_command_artifact(
                &format!("{}-format-before.bin", start.command_id.as_str()),
                format_snapshot_bytes(
                    &self.metadata.session_id,
                    run.snapshot_sequence,
                    &run.before,
                )?,
            )?;
        }
        if let Some(ToolRun::Dependency(run)) = &tool_run {
            self.publish_command_artifact(
                &format!("{}-dependency-before.bin", start.command_id.as_str()),
                format_snapshot_bytes(
                    &self.metadata.session_id,
                    run.snapshot_sequence,
                    &run.before,
                )?,
            )?;
        }
        let limits = ProcessLimits {
            deadline: self.command.deadline,
            output_bytes: ready.output_limit as usize,
        };
        let cancel = self.command.cancel.clone();
        let (process_io, sender, live) = match console {
            Some(console) => (Some(console.process), console.sender, Some(console.live)),
            None => (None, None, None),
        };
        let panic_cleanup = ProcessCleanupHandoff::new();
        let worker_cleanup = panic_cleanup.clone();
        let worker = std::thread::Builder::new()
            .name("rustrace-command".into())
            .spawn(move || match process_io {
                Some(process_io) => command_process::execute_with_io_and_handoff(
                    ready.prepared.command,
                    limits,
                    cancel,
                    process_io,
                    worker_cleanup,
                ),
                None => command_process::execute_with_handoff(
                    ready.prepared.command,
                    limits,
                    cancel,
                    worker_cleanup,
                ),
            })?;
        self.runtime_toolchain = Some(ready.report);
        self.command.console_stdin = sender;
        self.command.console_live = live;
        self.command.job = Some(Job::Running {
            worker,
            panic_cleanup,
            start,
            millis,
            tool_run,
        });
        Ok(())
    }

    fn preserve_capture(
        &mut self,
        start: ControlledCommandStarted,
        millis: u64,
        result: ProcessResult,
        tool_run: Option<ToolRun>,
    ) -> Result<()> {
        let test_case_result = self.command.test_case.clone().map(|case| {
            classify_test_case_result(
                case,
                &result.outcome,
                result.stdout.completeness,
                &result.stdout.bytes,
                self.command
                    .test_case_expected
                    .take()
                    .unwrap_or_else(|| Err("expected output snapshot is unavailable".to_owned())),
            )
        });
        let mut chunks = VecDeque::new();
        for (stream, capture) in [
            (OutputStream::Stdout, &result.stdout),
            (OutputStream::Stderr, &result.stderr),
        ] {
            for (index, bytes) in capture.bytes.chunks(MAX_COMMAND_CHUNK_BYTES).enumerate() {
                chunks.push_back(ControlledCommandOutput::from_bytes(
                    start.command_id.clone(),
                    stream,
                    (index * MAX_COMMAND_CHUNK_BYTES) as u64,
                    bytes,
                )?);
            }
        }
        let a = self.effects.0.borrow();
        let finished_millis = a.offset.saturating_add(
            result
                .completed_at
                .saturating_duration_since(a.started)
                .as_millis() as u64,
        );
        let finish =
            ControlledCommandFinished {
                command_id: start.command_id.clone(),
                after: start.before.clone(),
                started_millis: millis,
                finished_millis,
                outcome: result.outcome,
                stdout: CommandCapture {
                    bytes: result.stdout.bytes.len() as u64,
                    completeness: result.stdout.completeness,
                    mode: if start.console.as_ref().is_some_and(|route| {
                        matches!(route.stdout, ConsoleStdoutRoute::File { .. })
                    }) {
                        CommandCaptureMode::Redirected
                    } else {
                        CommandCaptureMode::Captured
                    },
                },
                stderr: CommandCapture {
                    bytes: result.stderr.bytes.len() as u64,
                    completeness: result.stderr.completeness,
                    mode: CommandCaptureMode::Captured,
                },
            };
        let diagnostics = (start.console.is_none()
            && matches!(
                start.action,
                ControlledAction::Build
                    | ControlledAction::Check
                    | ControlledAction::Test
                    | ControlledAction::Run
                    | ControlledAction::Clippy
                    | ControlledAction::Doc
            ))
        .then(|| derive_command_diagnostics(&start, chunks.make_contiguous(), &finish));
        // Publish original bytes and execution facts before journal/reconcile
        // can fail. This artifact is factual capture, not a completed boundary.
        let artifact = serde_json::to_vec(
            &serde_json::json!({"version":1,"start":start,"capture":chunks,
            "execution":{"started_millis":millis,"finished_millis":finished_millis,"outcome":finish.outcome,
            "stdout":finish.stdout,"stderr":finish.stderr,"cleanup_confirmed":result.cleanup_confirmed,"cleanup_os_code":result.cleanup_os_code},
            "diagnostics":diagnostics.as_ref()}),
        )?;
        drop(a);
        self.publish_command_artifact(
            &format!("{}-capture.json", finish.command_id.as_str()),
            artifact,
        )?;
        if let Some(diagnostics) = diagnostics {
            self.command.diagnostics = Some(diagnostics);
            self.command.selected_diagnostic = None;
        }
        if !result.cleanup_confirmed {
            return Err("owned child cleanup uncertain; capture preserved".into());
        }
        process_probe("command-capture");
        self.command.job = Some(Job::Recording {
            chunks,
            finish,
            tool_run,
            test_case_result,
        });
        Ok(())
    }

    fn prepare_format_run(
        &mut self,
        prepare: FormatPrepare,
    ) -> Result<(PreparedCargoCommand, FormatRun)> {
        let (before, documents) = self.workspace.formatter_prestate()?;
        let snapshot_sequence = self.effects.0.borrow().sequence;
        let snapshot = OwnedFormatSnapshot::create(&before)?;
        let prepared = cargo_policy::prepare(
            CargoAction::Format,
            &prepare.argv,
            &prepare.resolved,
            snapshot.path(),
        )?;
        Ok((
            prepared,
            FormatRun {
                snapshot,
                before,
                documents,
                snapshot_sequence,
            },
        ))
    }

    fn prepare_dependency_run(
        &mut self,
        prepare: DependencyPrepare,
    ) -> Result<(PreparedCargoCommand, DependencyRun)> {
        let (before, documents) = self.workspace.dependency_prestate()?;
        let snapshot_sequence = self.effects.0.borrow().sequence;
        let snapshot = OwnedFormatSnapshot::create(&before)?;
        let prepared = cargo_policy::prepare(
            prepare.action,
            &prepare.argv,
            &prepare.resolved,
            snapshot.path(),
        )?;
        Ok((
            prepared,
            DependencyRun {
                snapshot,
                before,
                documents,
                snapshot_sequence,
            },
        ))
    }

    /// Returns a user-visible warning only for a safely rejected returned tree.
    /// Operational uncertainty remains an error with active command ownership.
    fn finalize_format(
        &mut self,
        finish: &ControlledCommandFinished,
        run: FormatRun,
    ) -> Result<Option<String>> {
        let returned = run.snapshot.read_returned();
        let cleanup = run.snapshot.cleanup();
        let returned = match returned {
            Ok(files) => files,
            Err(error) => {
                let detail =
                    format!("formatter returned an unsafe, unreadable or oversized tree: {error}");
                self.publish_format_result(finish, None, "rejected", 0, Some(&detail))?;
                self.command.format_status = Some(FormatStatus::Rejected);
                cleanup?;
                return Ok(Some(detail));
            }
        };
        let returned_hash = hash_entries(
            returned
                .iter()
                .map(|(path, contents)| (path, contents.as_slice())),
        )?;
        self.publish_command_artifact(
            &format!("{}-format-returned.bin", finish.command_id.as_str()),
            format_snapshot_bytes(&self.metadata.session_id, run.snapshot_sequence, &returned)?,
        )?;
        cleanup?;

        if finish.outcome != (CommandOutcome::Exited { code: 0 })
            || finish.stdout.completeness != CaptureCompleteness::Complete
            || finish.stderr.completeness != CaptureCompleteness::Complete
        {
            self.publish_format_result(
                finish,
                Some(returned_hash),
                "discarded_process_failure",
                0,
                None,
            )?;
            self.command.format_status = Some(FormatStatus::Discarded);
            return Ok(None);
        }

        self.verify_command_context()?;
        self.recheck_with_hook(|_| Ok(()))?;
        let changes =
            match self
                .workspace
                .preflight_formatter_changes(&run.before, &returned, &run.documents)
            {
                Ok(changes) => changes,
                Err(error) => {
                    let detail =
                        format!("formatter returned changes rejected before publication: {error}");
                    self.publish_format_result(
                        finish,
                        Some(returned_hash),
                        "rejected",
                        0,
                        Some(&detail),
                    )?;
                    self.command.format_status = Some(FormatStatus::Rejected);
                    return Ok(Some(detail));
                }
            };
        let transaction_bytes: Result<u64> = changes.iter().try_fold(0_u64, |total, change| {
            let encoded = rustrace_editor::encode_transaction(&change.transaction)?;
            total
                .checked_add(encoded.len() as u64)
                .ok_or_else(|| "formatter transaction budget overflow".into())
        });
        let transaction_bytes: u64 = match transaction_bytes {
            Ok(bytes) => bytes,
            Err(error) => {
                let detail = format!("formatter transaction preflight rejected: {error}");
                self.publish_format_result(
                    finish,
                    Some(returned_hash),
                    "rejected",
                    0,
                    Some(&detail),
                )?;
                self.command.format_status = Some(FormatStatus::Rejected);
                return Ok(Some(detail));
            }
        };
        {
            let a = self.effects.0.borrow();
            if a.sequence
                .saturating_add(changes.len() as u64)
                .saturating_add(3)
                > a.budgets.events
            {
                let detail = "formatter result exceeds remaining event budget";
                drop(a);
                self.publish_format_result(
                    finish,
                    Some(returned_hash),
                    "rejected",
                    0,
                    Some(detail),
                )?;
                self.command.format_status = Some(FormatStatus::Rejected);
                return Ok(Some(detail.into()));
            }
            if let Err(error) = a.headroom(
                transaction_bytes
                    .saturating_mul(3)
                    .saturating_add(40 * 1024 * 1024),
            ) {
                let detail = format!("formatter result exceeds storage/reserve budget: {error}");
                drop(a);
                self.publish_format_result(
                    finish,
                    Some(returned_hash),
                    "rejected",
                    0,
                    Some(&detail),
                )?;
                self.command.format_status = Some(FormatStatus::Rejected);
                return Ok(Some(detail));
            }
        }
        process_probe("format-preflight");

        let changed_count = changes.len();
        let mut expected = run.before;
        for change in changes {
            if read_pinned_workspace(self.workspace.root_authority())? != expected {
                return Err(
                    "unowned live-workspace change during formatter publication; preserve and inspect"
                        .into(),
                );
            }
            let path = change.path.clone();
            let after = change.after.clone();
            self.effects.0.borrow_mut().formatter_permit = true;
            let applied = self.workspace.apply_formatter_change(change);
            self.effects.0.borrow_mut().formatter_permit = false;
            applied?;
            expected.insert(path, after);
            if read_pinned_workspace(self.workspace.root_authority())? != expected {
                return Err("formatter disk publication could not be verified".into());
            }
            self.persist_baseline()?;
            process_probe("format-baseline");
        }
        if self.workspace.logical_files()? != returned {
            return Err("formatter accepted prefix differs from returned tree".into());
        }
        self.publish_format_result(
            finish,
            Some(returned_hash),
            if changed_count == 0 {
                "no_change"
            } else {
                "accepted"
            },
            changed_count,
            None,
        )?;
        self.command.format_status = Some(if changed_count == 0 {
            FormatStatus::NoChange
        } else {
            FormatStatus::Applied
        });
        Ok(None)
    }

    /// Returns a user-visible warning only when a successful dependency tool
    /// returns a tree that cannot be published as bounded editor transactions.
    fn finalize_dependency(
        &mut self,
        finish: &ControlledCommandFinished,
        run: DependencyRun,
    ) -> Result<Option<String>> {
        let returned = run.snapshot.read_returned();
        let cleanup = run.snapshot.cleanup();
        let returned = match returned {
            Ok(files) => files,
            Err(error) => {
                let detail = format!(
                    "dependency command returned an unsafe, unreadable or oversized tree: {error}"
                );
                self.publish_dependency_result(finish, None, "rejected", 0, Some(&detail))?;
                cleanup?;
                return Ok(Some(detail));
            }
        };
        let returned_hash = hash_entries(
            returned
                .iter()
                .map(|(path, contents)| (path, contents.as_slice())),
        )?;
        self.publish_command_artifact(
            &format!("{}-dependency-returned.bin", finish.command_id.as_str()),
            format_snapshot_bytes(&self.metadata.session_id, run.snapshot_sequence, &returned)?,
        )?;
        cleanup?;

        if finish.outcome != (CommandOutcome::Exited { code: 0 })
            || finish.stdout.completeness != CaptureCompleteness::Complete
            || finish.stderr.completeness != CaptureCompleteness::Complete
        {
            self.publish_dependency_result(
                finish,
                Some(returned_hash),
                "discarded_process_failure",
                0,
                None,
            )?;
            return Ok(None);
        }

        self.verify_command_context()?;
        self.recheck_with_hook(|_| Ok(()))?;
        let changes = match self.workspace.preflight_dependency_changes(
            &run.before,
            &returned,
            &run.documents,
        ) {
            Ok(changes) => changes,
            Err(error) => {
                let detail = format!("dependency changes rejected before publication: {error}");
                self.publish_dependency_result(
                    finish,
                    Some(returned_hash),
                    "rejected",
                    0,
                    Some(&detail),
                )?;
                return Ok(Some(detail));
            }
        };
        let transaction_bytes: Result<u64> = changes.iter().try_fold(0_u64, |total, change| {
            let encoded = rustrace_editor::encode_transaction(&change.transaction)?;
            total
                .checked_add(encoded.len() as u64)
                .ok_or_else(|| "dependency transaction budget overflow".into())
        });
        let transaction_bytes = match transaction_bytes {
            Ok(bytes) => bytes,
            Err(error) => {
                let detail = format!("dependency transaction preflight rejected: {error}");
                self.publish_dependency_result(
                    finish,
                    Some(returned_hash),
                    "rejected",
                    0,
                    Some(&detail),
                )?;
                return Ok(Some(detail));
            }
        };
        {
            let a = self.effects.0.borrow();
            if a.sequence
                .saturating_add(changes.len() as u64)
                .saturating_add(3)
                > a.budgets.events
            {
                let detail = "dependency result exceeds remaining event budget";
                drop(a);
                self.publish_dependency_result(
                    finish,
                    Some(returned_hash),
                    "rejected",
                    0,
                    Some(detail),
                )?;
                return Ok(Some(detail.into()));
            }
            if let Err(error) = a.headroom(
                transaction_bytes
                    .saturating_mul(3)
                    .saturating_add(40 * 1024 * 1024),
            ) {
                let detail = format!("dependency result exceeds storage/reserve budget: {error}");
                drop(a);
                self.publish_dependency_result(
                    finish,
                    Some(returned_hash),
                    "rejected",
                    0,
                    Some(&detail),
                )?;
                return Ok(Some(detail));
            }
        }
        process_probe("dependency-preflight");

        let changed_count = changes.len();
        let mut expected = run.before;
        for change in changes {
            if read_pinned_workspace(self.workspace.root_authority())? != expected {
                return Err(
                    "unowned live-workspace change during dependency publication; preserve and inspect"
                        .into(),
                );
            }
            let path = change.path.clone();
            let after = change.after.clone();
            self.effects.0.borrow_mut().dependency_tool_permit = true;
            let applied = self.workspace.apply_dependency_change(change);
            self.effects.0.borrow_mut().dependency_tool_permit = false;
            applied?;
            expected.insert(path, after);
            if read_pinned_workspace(self.workspace.root_authority())? != expected {
                return Err("dependency disk publication could not be verified".into());
            }
            self.persist_baseline()?;
            process_probe("dependency-baseline");
        }
        if self.workspace.logical_files()? != returned {
            return Err("dependency accepted prefix differs from returned tree".into());
        }
        self.publish_dependency_result(
            finish,
            Some(returned_hash),
            if changed_count == 0 {
                "no_change"
            } else {
                "accepted"
            },
            changed_count,
            None,
        )?;
        Ok(None)
    }

    fn publish_format_result(
        &mut self,
        finish: &ControlledCommandFinished,
        returned_hash: Option<Hash>,
        decision: &str,
        changed_documents: usize,
        detail: Option<&str>,
    ) -> Result<()> {
        let a = self.effects.0.borrow();
        let detail = detail.map(|value| {
            let mut end = value.len().min(4096);
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            value[..end].to_owned()
        });
        let result = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "command_id": finish.command_id,
            "decision": decision,
            "changed_documents": changed_documents,
            "returned_workspace_hash": returned_hash,
            "prefix_sequence": a.sequence,
            "prefix_event_hash": a.hash,
            "detail": detail,
        }))?;
        drop(a);
        self.publish_command_artifact(
            &format!("{}-format-result.json", finish.command_id.as_str()),
            result,
        )
    }

    fn publish_dependency_result(
        &mut self,
        finish: &ControlledCommandFinished,
        returned_hash: Option<Hash>,
        decision: &str,
        changed_documents: usize,
        detail: Option<&str>,
    ) -> Result<()> {
        let a = self.effects.0.borrow();
        let detail = detail.map(|value| {
            let mut end = value.len().min(4096);
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            value[..end].to_owned()
        });
        let result = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "command_id": finish.command_id,
            "decision": decision,
            "changed_documents": changed_documents,
            "returned_workspace_hash": returned_hash,
            "prefix_sequence": a.sequence,
            "prefix_event_hash": a.hash,
            "detail": detail,
        }))?;
        drop(a);
        self.publish_command_artifact(
            &format!("{}-dependency-result.json", finish.command_id.as_str()),
            result,
        )
    }

    fn publish_command_artifact(&mut self, name: &str, artifact: Vec<u8>) -> Result<()> {
        let a = self.effects.0.borrow();
        if let Err(error) = a.owner.publish_artifact(name, &artifact, false) {
            // Preserve an unexpected occupant; a separate immutable artifact
            // retains original facts without claiming normal completion.
            let rescue = format!("command-capture-recovery-{}.json", digest(&artifact));
            if a.owner.publish_artifact(&rescue, &artifact, false).is_err() {
                self.command.unpublished_capture = Some(artifact);
            }
            return Err(error.into());
        }
        Ok(())
    }

    fn command_checkpoint(&mut self) -> Result<()> {
        self.persist_baseline()?;
        let input = self
            .workspace
            .checkpoint_input(self.metadata.session_id.clone())?;
        self.effects.0.borrow_mut().checkpoint(input, true)?;
        self.persist_baseline()
    }

    fn finish_command(
        &mut self,
        mut finish: ControlledCommandFinished,
        test_case_result: Option<&TestCaseComparison>,
    ) -> Result<()> {
        self.command.console_stdin = None;
        self.verify_command_context()?;
        self.recheck_with_hook(|_| Ok(()))?;
        self.command_checkpoint()?;
        process_probe("command-postcheckpoint");
        let mut a = self.effects.0.borrow_mut();
        finish.after = a
            .replay
            .as_ref()
            .and_then(ReplayEngine::command_tree_link)
            .ok_or("missing post-tree")?
            .clone();
        if let Some(comparison) = test_case_result {
            let comparison = test_case_event(&finish, comparison)?;
            a.append_compared_finish(finish.clone(), comparison)?;
        } else {
            a.append(Event::ControlledCommandFinished(finish.clone()))?;
        }
        process_probe("command-finish");
        drop(a);
        self.persist_baseline()?;
        self.record_command_activity(false)?;
        self.command.outcome = Some(finish.outcome);
        self.effects.0.borrow_mut().command_active = false;
        self.resume_language_service_after_command();
        Ok(())
    }

    fn clear_command_ownership(&mut self) -> Result<()> {
        self.command.console_stdin = None;
        self.record_command_activity(false)?;
        self.effects.0.borrow_mut().command_active = false;
        self.resume_language_service_after_command();
        Ok(())
    }

    fn complete_test_case_error(&mut self, reason: &str) {
        let Some(case) = self.command.test_case.take() else {
            return;
        };
        let expected_blake3 = self
            .command
            .test_case_expected
            .take()
            .and_then(std::result::Result::ok)
            .as_deref()
            .map(crate::console::hash_bytes);
        self.command.completed_test_case = Some(TestCaseComparison {
            case,
            outcome: TestCaseOutcome::Error(reason.to_owned()),
            expected_blake3,
            actual_blake3: None,
        });
    }
}

fn test_case_event(
    finish: &ControlledCommandFinished,
    comparison: &TestCaseComparison,
) -> Result<TestCaseCompared> {
    let expected_blake3 = comparison
        .expected_blake3
        .ok_or("finished test case is missing its expected snapshot hash")?;
    let execution_error = match finish.outcome {
        CommandOutcome::LaunchFailed { .. } => Some(TestCaseComparisonError::LaunchFailed),
        CommandOutcome::Exited { code } if code != 0 => Some(TestCaseComparisonError::NonzeroExit),
        CommandOutcome::Terminated { .. } => Some(TestCaseComparisonError::Terminated),
        CommandOutcome::Exited { code: 0 } => match finish.stdout.completeness {
            CaptureCompleteness::Complete => None,
            CaptureCompleteness::Truncated => Some(TestCaseComparisonError::CaptureTruncated),
            CaptureCompleteness::Unavailable => Some(TestCaseComparisonError::CaptureUnavailable),
            CaptureCompleteness::ReadFailed => Some(TestCaseComparisonError::CaptureReadFailed),
        },
        CommandOutcome::Exited { .. } => unreachable!("nonzero exit handled above"),
    };
    let outcome = match (execution_error, &comparison.outcome) {
        (Some(reason), TestCaseOutcome::Error(_)) => TestCaseComparisonOutcome::Error { reason },
        (None, TestCaseOutcome::Pass) => TestCaseComparisonOutcome::Pass,
        (None, TestCaseOutcome::Fail(mismatch)) => TestCaseComparisonOutcome::Mismatch {
            line: mismatch.line,
            expected_len: mismatch.expected_len as u64,
            actual_len: mismatch.actual_len as u64,
        },
        _ => return Err("test-case result contradicts finished command evidence".into()),
    };
    let event = TestCaseCompared {
        command_id: finish.command_id.clone(),
        case: comparison.case.name().to_owned(),
        expected_blake3,
        actual_blake3: comparison.actual_blake3,
        outcome,
    };
    event.validate()?;
    Ok(event)
}

fn diagnostic_count(count: usize, label: &str) -> String {
    format!("{count} {label}{}", if count == 1 { "" } else { "s" })
}

fn prepare_console_io(mut launch: ConsoleLaunch) -> Result<ConsoleIo> {
    let input_identity = launch.input.as_ref().map(|input| input.identity());
    let stdout = match (&launch.request.stdout, launch.output) {
        (Some(path), Some(disposition)) => {
            let mut opened = launch
                .cases
                .as_ref()
                .ok_or("missing test-case authority for output route")?
                .open_output(path, disposition)?;
            if input_identity.is_some_and(|identity| identity == opened.identity()) {
                return Err("console input and output refer to the same file".into());
            }
            opened.file_mut().set_len(0)?;
            ProcessStdout::File(opened.into_file())
        }
        (None, None) => ProcessStdout::Captured,
        _ => return Err("incomplete console output setup".into()),
    };
    let (stdin, sender) = if let Some(input) = launch.input.take() {
        (ProcessStdin::File(input.into_file()), None)
    } else if launch.request.action == CargoAction::Run {
        let (sender, receiver) = mpsc::sync_channel(64);
        (ProcessStdin::Submitted(receiver), Some(sender))
    } else {
        (ProcessStdin::Closed, None)
    };
    let route = ConsoleCommandRoute {
        stdin: match &launch.request.stdin {
            Some(path) => ConsoleStdinRoute::File { path: path.clone() },
            None if launch.request.action == CargoAction::Run => ConsoleStdinRoute::Submitted,
            None => ConsoleStdinRoute::Closed,
        },
        stdout: match &launch.request.stdout {
            Some(path) => ConsoleStdoutRoute::File { path: path.clone() },
            None => ConsoleStdoutRoute::Console,
        },
    };
    let live = LiveOutput::new(command_process::MAX_LIVE_OUTPUT_BYTES);
    Ok(ConsoleIo {
        route,
        process: ProcessIo {
            stdin,
            stdout,
            live: Some(live.clone()),
        },
        sender,
        live,
    })
}

fn action_model(action: CargoAction) -> ControlledAction {
    match action {
        CargoAction::Build => ControlledAction::Build,
        CargoAction::Check => ControlledAction::Check,
        CargoAction::Test => ControlledAction::Test,
        CargoAction::Run => ControlledAction::Run,
        CargoAction::Clippy => ControlledAction::Clippy,
        CargoAction::Format => ControlledAction::Format,
        CargoAction::Doc => ControlledAction::Doc,
        CargoAction::Add => ControlledAction::Add,
        CargoAction::Remove => ControlledAction::Remove,
        CargoAction::Update => ControlledAction::Update,
    }
}

fn environment_name(name: &str) -> Result<RetainedEnvironmentName> {
    Ok(match name {
        "HOME" => RetainedEnvironmentName::Home,
        "CARGO_HOME" => RetainedEnvironmentName::CargoHome,
        "RUSTUP_HOME" => RetainedEnvironmentName::RustupHome,
        "PATH" => RetainedEnvironmentName::Path,
        "TMPDIR" => RetainedEnvironmentName::Tmpdir,
        "TMP" => RetainedEnvironmentName::Tmp,
        "TEMP" => RetainedEnvironmentName::Temp,
        _ => return Err("unexpected policy environment name".into()),
    })
}

fn resolve(
    root: PathBuf,
    pin: String,
    action: CargoAction,
    argv: Vec<String>,
    output_limit: u64,
    cancel: Arc<AtomicU8>,
    console: Option<ConsoleLaunch>,
) -> Resolution {
    use std::cell::Cell;
    let cleanup_uncertain = Cell::new(false);
    let captures = RefCell::new(Vec::new());
    let rustup = toolchain::find_rustup(&root);
    let until = Instant::now() + Duration::from_secs(30);
    let report = toolchain::discover_with_executor(
        &root,
        (!pin.is_empty()).then_some(pin.as_str()),
        &rustup,
        Some(action),
        64 * 1024,
        &|command| {
            let deadline = until
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(3));
            let result = command_process::execute(
                command,
                ProcessLimits {
                    deadline,
                    output_bytes: 64 * 1024,
                },
                cancel.clone(),
            );
            observe_resolution_process(result, deadline, &cleanup_uncertain, &captures, &cancel)
        },
    );
    let prepared_result = (|| {
        if cleanup_uncertain.get() {
            return Err("cleanup uncertain during tool discovery; preserve workspace".into());
        }
        if report.has_blockers() {
            return Err("current toolchain unavailable; inspect tool installation and assignment selection, then retry".into());
        }
        let mut tools = Vec::new();
        for (component, kind) in [
            ("rustup", CommandToolKind::Rustup),
            ("rustc", CommandToolKind::Rustc),
            ("cargo", CommandToolKind::Cargo),
            ("rustdoc", CommandToolKind::Rustdoc),
            ("clippy", CommandToolKind::Clippy),
            ("cargo-fmt", CommandToolKind::CargoFmt),
            ("rustfmt", CommandToolKind::Rustfmt),
        ] {
            if component == "clippy" && action != CargoAction::Clippy {
                continue;
            }
            if matches!(component, "cargo-fmt" | "rustfmt") && action != CargoAction::Format {
                continue;
            }
            let version = report.version(component).ok_or_else(|| format!("{component} unavailable for selected action; repair installed component outside Rustrace"))?;
            let path = if component == "rustup" {
                rustup.to_str().ok_or("invalid rustup path")?
            } else {
                report
                    .probes
                    .iter()
                    .find(|probe| {
                        probe.component == component
                            && probe.purpose == "resolve"
                            && probe.status == toolchain::ProbeStatus::Available
                    })
                    .map(|probe| probe.stdout.trim())
                    .ok_or("tool path unavailable")?
            };
            tools.push(CommandTool {
                component: kind,
                executable: path.into(),
                version: version.into(),
            });
        }
        let path = |kind| {
            tools
                .iter()
                .find(|tool| tool.component == kind)
                .map(|tool| PathBuf::from(&tool.executable))
        };
        let resolved = ResolvedTools {
            rustup,
            selection: report
                .selected_toolchain
                .clone()
                .ok_or("selection unavailable")?,
            cargo: path(CommandToolKind::Cargo).ok_or("cargo unavailable")?,
            rustc: path(CommandToolKind::Rustc).ok_or("rustc unavailable")?,
            rustdoc: path(CommandToolKind::Rustdoc).ok_or("rustdoc unavailable")?,
            cargo_clippy: path(CommandToolKind::Clippy),
            formatter: match (
                path(CommandToolKind::CargoFmt),
                path(CommandToolKind::Rustfmt),
            ) {
                (Some(driver), Some(formatter)) => Some((driver, formatter)),
                _ => None,
            },
        };
        let prepared = if let Some(console) = &console {
            cargo_policy::prepare_console(&console.request, &resolved, &root)
        } else {
            cargo_policy::prepare(action, &argv, &resolved, &root)
        }
        .map_err(|error| error.to_string())?;
        Ok(Ready {
            prepared,
            format_prepare: (action == CargoAction::Format).then_some(FormatPrepare {
                resolved: resolved.clone(),
                argv: argv.clone(),
            }),
            dependency_prepare: action.is_dependency().then_some(DependencyPrepare {
                resolved,
                argv,
                action,
            }),
            report: report.clone(),
            tools,
            action,
            output_limit,
            console,
        })
    })();
    Resolution {
        result: prepared_result,
        report,
        captures: captures.into_inner(),
    }
}

fn observe_resolution_process(
    mut result: ProcessResult,
    deadline: Duration,
    cleanup_uncertain: &std::cell::Cell<bool>,
    captures: &RefCell<Vec<serde_json::Value>>,
    cancel: &AtomicU8,
) -> CommandExecution {
    let inconsistent_pending = result.cleanup_confirmed && result.pending_cleanup.is_some();
    if let Some(mut cleanup) = result.pending_cleanup.take() {
        if inconsistent_pending {
            result.cleanup_confirmed = false;
            result.outcome = CommandOutcome::Terminated {
                reason: CommandTermination::CleanupFailure,
                signal: None,
            };
        }
        cancel.store(1, Ordering::Release);
        loop {
            let progress = cleanup.poll();
            if progress.os_code.is_some() {
                result.cleanup_os_code = progress.os_code;
            }
            if progress.confirmed {
                result.cleanup_confirmed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    } else if !result.cleanup_confirmed {
        cancel.store(1, Ordering::Release);
        cleanup_uncertain.set(true);
    }
    captures.borrow_mut().push(serde_json::json!({
        "stdout_hex":raw_hex(&result.stdout.bytes), "stderr_hex":raw_hex(&result.stderr.bytes),
        "outcome":result.outcome, "stdout_completeness":result.stdout.completeness,
        "stderr_completeness":result.stderr.completeness,
        "cleanup_confirmed":result.cleanup_confirmed,"cleanup_os_code":result.cleanup_os_code
    }));
    let complete = result.stdout.completeness == CaptureCompleteness::Complete
        && result.stderr.completeness == CaptureCompleteness::Complete;
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
}

fn raw_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(HEX[(byte >> 4) as usize] as char);
        text.push(HEX[(byte & 15) as usize] as char);
    }
    text
}

static NEXT_FORMAT_SNAPSHOT: AtomicU64 = AtomicU64::new(1);

struct OwnedFormatSnapshot {
    path: Option<PathBuf>,
}

impl OwnedFormatSnapshot {
    fn create(files: &Files) -> Result<Self> {
        let parent = std::env::temp_dir();
        let mut path = None;
        for _ in 0..32 {
            let candidate = parent.join(format!(
                "rustrace-format-{}-{}",
                std::process::id(),
                NEXT_FORMAT_SNAPSHOT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => {
                    path = Some(candidate);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        let mut snapshot = Self {
            path: Some(path.ok_or("could not allocate an owned formatter snapshot")?),
        };
        let result = (|| {
            for (workspace_path, bytes) in files {
                let destination = snapshot.path().join(workspace_path.as_str());
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&destination)?;
                file.write_all(bytes)?;
                file.sync_all()?;
            }
            if snapshot.read_returned()? != *files {
                return Err("formatter snapshot copy verification failed".into());
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = snapshot.cleanup_inner();
            return Err(error);
        }
        Ok(snapshot)
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("owned formatter snapshot path")
    }

    fn read_returned(&self) -> Result<Files> {
        let root = PinnedWorkspaceRoot::open(self.path())?;
        Ok(read_pinned_workspace(&root)?)
    }

    fn cleanup(mut self) -> Result<()> {
        self.cleanup_inner()?;
        Ok(())
    }

    fn cleanup_inner(&mut self) -> std::io::Result<()> {
        if let Some(path) = self.path.take() {
            fs::remove_dir_all(path)?;
        }
        Ok(())
    }
}

impl Drop for OwnedFormatSnapshot {
    fn drop(&mut self) {
        let _ = self.cleanup_inner();
    }
}

fn format_snapshot_bytes(id: &SessionId, sequence: u64, files: &Files) -> Result<Vec<u8>> {
    let bytes = encode_checkpoint(&files_snapshot(id, sequence, files)?)?;
    if bytes.len() > ARTIFACT_LIMIT {
        return Err("formatter state artifact exceeds bounded evidence limit".into());
    }
    Ok(bytes)
}
