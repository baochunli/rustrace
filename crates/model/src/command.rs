//! Additive controlled-command evidence. Legacy Cargo events remain unchanged.
use crate::{
    CommandId, Hash, MAX_MONOTONIC_MILLIS, MAX_STRING_BYTES, ValidationError, WorkspacePath,
};
use serde::{Deserialize, Serialize};

pub const MAX_COMMAND_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_SESSION_COMMAND_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_COMMAND_CHUNK_BYTES: usize = 32 * 1024;
pub const MAX_COMMAND_DEADLINE_MILLIS: u64 = 300_000;
pub const MAX_COMMAND_CLEANUP_MILLIS: u64 = 2_000;
pub const MAX_TEST_CASE_NAME_BYTES: usize = 64;
pub const MAX_TEST_CASE_EXPECTED_LINE_BYTES: u64 = 1024 * 1024;
pub const MAX_TEST_CASE_COMPARISON_LINE: u64 = MAX_TEST_CASE_EXPECTED_LINE_BYTES + 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlledAction {
    Build,
    Check,
    Test,
    Run,
    Clippy,
    Format,
    Doc,
    Add,
    Remove,
    Update,
}

/// Only names/presence are recorded; policy v1 retains fixed controls and
/// ambient-configuration limitations, never raw values.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandEnvironment {
    pub policy_version: u32,
    pub retained_names: Vec<RetainedEnvironmentName>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum RetainedEnvironmentName {
    #[serde(rename = "HOME")]
    Home,
    #[serde(rename = "CARGO_HOME")]
    CargoHome,
    #[serde(rename = "RUSTUP_HOME")]
    RustupHome,
    #[serde(rename = "PATH")]
    Path,
    #[serde(rename = "TMPDIR")]
    Tmpdir,
    #[serde(rename = "TMP")]
    Tmp,
    #[serde(rename = "TEMP")]
    Temp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandToolKind {
    Rustup,
    Rustc,
    Cargo,
    Rustdoc,
    Clippy,
    CargoFmt,
    Rustfmt,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandTool {
    pub component: CommandToolKind,
    pub executable: String,
    pub version: String,
}

/// The full checkpoint owns exact files and document versions. workspace_version
/// is the last source-mutating envelope sequence, or 1 at genesis; it survives
/// rename/create/delete and does not advance for output or recovery observations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandTreeLink {
    pub checkpoint_sequence: u64,
    pub checkpoint_event_hash: Hash,
    pub workspace_hash: Hash,
    pub workspace_version: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlledCommandStarted {
    pub command_id: CommandId,
    pub action: ControlledAction,
    pub argv: Vec<String>,
    pub environment: CommandEnvironment,
    pub selected_toolchain: String,
    pub tools: Vec<CommandTool>,
    pub before: CommandTreeLink,
    pub deadline_millis: u64,
    pub output_limit: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console: Option<ConsoleCommandRoute>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsoleCommandRoute {
    pub stdin: ConsoleStdinRoute,
    pub stdout: ConsoleStdoutRoute,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConsoleStdinRoute {
    Closed,
    Submitted,
    File { path: WorkspacePath },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConsoleStdoutRoute {
    Console,
    File { path: WorkspacePath },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlledCommandOutput {
    pub command_id: CommandId,
    pub stream: crate::OutputStream,
    pub offset: u64,
    /// Lowercase hex of original bytes. Presentation must never emit these
    /// decoded bytes directly to a terminal. Chunks are independently bounded.
    pub bytes_hex: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureCompleteness {
    Complete,
    Truncated,
    ReadFailed,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandCapture {
    pub bytes: u64,
    pub completeness: CaptureCompleteness,
    #[serde(default, skip_serializing_if = "capture_mode_is_default")]
    pub mode: CommandCaptureMode,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandCaptureMode {
    #[default]
    Captured,
    Redirected,
}

fn capture_mode_is_default(mode: &CommandCaptureMode) -> bool {
    *mode == CommandCaptureMode::Captured
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandTermination {
    Cancelled,
    Quit,
    Deadline,
    OutputLimit,
    CaptureFailure,
    Signal,
    CleanupFailure,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandOutcome {
    Exited {
        code: i32,
    },
    Terminated {
        reason: CommandTermination,
        signal: Option<i32>,
    },
    LaunchFailed {
        os_code: Option<i32>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlledCommandFinished {
    pub command_id: CommandId,
    pub after: CommandTreeLink,
    pub started_millis: u64,
    pub finished_millis: u64,
    pub outcome: CommandOutcome,
    pub stdout: CommandCapture,
    pub stderr: CommandCapture,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestCaseCompared {
    pub command_id: CommandId,
    pub case: String,
    pub expected_blake3: Hash,
    pub actual_blake3: Option<Hash>,
    pub outcome: TestCaseComparisonOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TestCaseComparisonOutcome {
    Pass,
    Mismatch {
        line: u64,
        expected_len: u64,
        actual_len: u64,
    },
    Error {
        reason: TestCaseComparisonError,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestCaseComparisonError {
    LaunchFailed,
    NonzeroExit,
    Terminated,
    CaptureTruncated,
    CaptureUnavailable,
    CaptureReadFailed,
    ExpectedUnreadable,
    ExpectedOversized,
}

fn require(valid: bool, detail: &'static str) -> Result<(), ValidationError> {
    if valid {
        Ok(())
    } else {
        Err(ValidationError::CommandEvidence { detail })
    }
}

/// The bounded crates.io dependency grammar shared by live command parsing and
/// persisted command evidence.
pub fn is_valid_crates_io_dependency_spec(value: &str) -> bool {
    if let Some((name, version)) = value.split_once('@') {
        is_valid_crates_io_name(name) && is_valid_semver(version)
    } else {
        is_valid_crates_io_name(value)
    }
}

pub fn is_valid_crates_io_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn is_valid_semver(value: &str) -> bool {
    if value.is_empty() || value.len() > 128 || !value.is_ascii() {
        return false;
    }
    let (without_build, build) = value
        .split_once('+')
        .map_or((value, None), |(core, build)| (core, Some(build)));
    if value.matches('+').count() > 1
        || build.is_some_and(|part| !is_valid_semver_identifiers(part, false))
    {
        return false;
    }
    let (core, prerelease) = without_build
        .split_once('-')
        .map_or((without_build, None), |(core, pre)| (core, Some(pre)));
    if prerelease.is_some_and(|part| !is_valid_semver_identifiers(part, true)) {
        return false;
    }
    let components = core.split('.').collect::<Vec<_>>();
    components.len() == 3 && components.into_iter().all(is_valid_numeric_identifier)
}

fn is_valid_semver_identifiers(value: &str, reject_numeric_leading_zero: bool) -> bool {
    !value.is_empty()
        && value.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && (!reject_numeric_leading_zero
                    || !identifier.bytes().all(|byte| byte.is_ascii_digit())
                    || is_valid_numeric_identifier(identifier))
        })
}

fn is_valid_numeric_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}

fn is_valid_new_action_tail(action: ControlledAction, tail: &[String]) -> bool {
    match (action, tail) {
        (ControlledAction::Doc, [message_format, locked]) => {
            message_format == "--message-format=json" && locked == "--locked"
        }
        (ControlledAction::Add, [dependency]) => is_valid_crates_io_dependency_spec(dependency),
        (ControlledAction::Remove, [name]) => is_valid_crates_io_name(name),
        (ControlledAction::Update, []) => true,
        _ => false,
    }
}

impl ControlledCommandStarted {
    pub fn validate(&self) -> Result<(), ValidationError> {
        require(
            (1..=MAX_COMMAND_DEADLINE_MILLIS).contains(&self.deadline_millis)
                && self.output_limit <= MAX_COMMAND_OUTPUT_BYTES
                && self.output_limit > 0,
            "command resource bounds",
        )?;
        require(
            self.argv.len() >= 2
                && self.argv.len() <= 40
                && self
                    .argv
                    .iter()
                    .all(|a| !a.is_empty() && a.len() <= MAX_STRING_BYTES && !a.contains('\0')),
            "bounded literal command argv",
        )?;
        require(
            !self.selected_toolchain.is_empty()
                && self.selected_toolchain.len() <= 128
                && self
                    .selected_toolchain
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "bounded selected toolchain",
        )?;
        require(
            self.environment.policy_version == 1
                && self.environment.retained_names.len() <= 7
                && self
                    .environment
                    .retained_names
                    .windows(2)
                    .all(|p| p[0] < p[1]),
            "canonical nonsecret environment summary",
        )?;
        let expected = match self.action {
            ControlledAction::Clippy => 5,
            ControlledAction::Format => 6,
            _ => 4,
        };
        require(
            self.tools.len() == expected
                && self
                    .tools
                    .windows(2)
                    .all(|p| p[0].component < p[1].component),
            "canonical current tool evidence",
        )?;
        for kind in [
            CommandToolKind::Rustup,
            CommandToolKind::Rustc,
            CommandToolKind::Cargo,
            CommandToolKind::Rustdoc,
        ] {
            require(
                self.tools.iter().any(|t| t.component == kind),
                "mandatory current tool evidence",
            )?;
        }
        require(
            match self.action {
                ControlledAction::Clippy => self
                    .tools
                    .iter()
                    .any(|t| t.component == CommandToolKind::Clippy),
                ControlledAction::Format => [CommandToolKind::CargoFmt, CommandToolKind::Rustfmt]
                    .iter()
                    .all(|kind| self.tools.iter().any(|t| t.component == *kind)),
                _ => true,
            },
            "selected action tool evidence",
        )?;
        for tool in &self.tools {
            require(
                tool.executable.starts_with('/')
                    && tool.executable.len() <= MAX_STRING_BYTES
                    && !tool.executable.chars().any(char::is_control)
                    && !tool.version.is_empty()
                    && tool.version.len() <= MAX_STRING_BYTES,
                "bounded exact executable and actual version",
            )?;
        }
        require(
            self.argv.first() == self.tools.first().map(|t| &t.executable),
            "launcher evidence matches argv",
        )?;
        let (kind, subcommand) = match self.action {
            ControlledAction::Build => (CommandToolKind::Cargo, "build"),
            ControlledAction::Check => (CommandToolKind::Cargo, "check"),
            ControlledAction::Test => (CommandToolKind::Cargo, "test"),
            ControlledAction::Run => (CommandToolKind::Cargo, "run"),
            ControlledAction::Clippy => (CommandToolKind::Clippy, "clippy"),
            ControlledAction::Format => (CommandToolKind::CargoFmt, "fmt"),
            ControlledAction::Doc => (CommandToolKind::Cargo, "doc"),
            ControlledAction::Add => (CommandToolKind::Cargo, "add"),
            ControlledAction::Remove => (CommandToolKind::Cargo, "remove"),
            ControlledAction::Update => (CommandToolKind::Cargo, "update"),
        };
        // Evidence linkage only, not a second Cargo argument policy. T4.3
        // alone decides which instructor arguments may be prepared.
        require(
            self.argv.len() >= 5
                && self.argv[1] == "run"
                && self.argv[2] == self.selected_toolchain
                && self
                    .tools
                    .iter()
                    .any(|t| t.component == kind && t.executable == self.argv[3])
                && self.argv[4] == subcommand,
            "argv/tool/action evidence linkage",
        )?;
        match (&self.console, self.action) {
            (None, ControlledAction::Build) => {
                return Err(ValidationError::CommandEvidence {
                    detail: "action is unavailable on this command route",
                });
            }
            (Some(route), ControlledAction::Run) => {
                require(
                    matches!(
                        route.stdin,
                        ConsoleStdinRoute::Submitted | ConsoleStdinRoute::File { .. }
                    ),
                    "Run console stdin route",
                )?;
                if let (
                    ConsoleStdinRoute::File { path: input },
                    ConsoleStdoutRoute::File { path: output },
                ) = (&route.stdin, &route.stdout)
                {
                    require(input != output, "distinct console redirection paths")?;
                }
                let tail = self.argv.get(5..).unwrap_or_default();
                let old = tail == ["--frozen"] || tail == ["--release", "--frozen"];
                let structured = tail == ["--message-format=json", "--locked"]
                    || tail == ["--release", "--message-format=json", "--locked"];
                let natural = tail == ["--locked"] || tail == ["--release", "--locked"];
                require(old || structured || natural, "literal console Run argv")?;
            }
            (Some(_), ControlledAction::Format) => {
                return Err(ValidationError::CommandEvidence {
                    detail: "action is unavailable on this command route",
                });
            }
            (Some(route), action)
                if matches!(
                    action,
                    ControlledAction::Doc
                        | ControlledAction::Add
                        | ControlledAction::Remove
                        | ControlledAction::Update
                ) =>
            {
                require(
                    route.stdin == ConsoleStdinRoute::Closed
                        && route.stdout == ConsoleStdoutRoute::Console
                        && (is_valid_new_action_tail(action, &self.argv[5..])
                            || action == ControlledAction::Doc && self.argv[5..] == ["--locked"]),
                    "literal console Doc/dependency route",
                )?;
            }
            (Some(route), _) => {
                let tail = self.argv.get(5..).unwrap_or_default();
                let old = tail == ["--frozen"];
                let current = tail == ["--message-format=json", "--locked"];
                let natural = tail == ["--locked"];
                require(
                    route.stdin == ConsoleStdinRoute::Closed
                        && route.stdout == ConsoleStdoutRoute::Console
                        && (old || current || natural),
                    "literal non-Run console route",
                )?;
            }
            (None, action)
                if matches!(
                    action,
                    ControlledAction::Doc
                        | ControlledAction::Add
                        | ControlledAction::Remove
                        | ControlledAction::Update
                ) =>
            {
                require(
                    is_valid_new_action_tail(action, &self.argv[5..]),
                    "literal Doc/dependency action argv",
                )?;
            }
            (None, _) => {}
        }
        self.before.validate()
    }
}

impl CommandTreeLink {
    pub fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.workspace_version > 0 && self.workspace_version <= self.checkpoint_sequence,
            "tree link sequence/version bounds",
        )
    }
}

impl ControlledCommandOutput {
    pub fn validate(&self) -> Result<(), ValidationError> {
        require(
            !self.bytes_hex.is_empty()
                && self.bytes_hex.len() <= 2 * MAX_COMMAND_CHUNK_BYTES
                && self.bytes_hex.len().is_multiple_of(2)
                && self
                    .bytes_hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && self.offset <= MAX_COMMAND_OUTPUT_BYTES,
            "canonical bounded original command bytes",
        )
    }
    pub fn from_bytes(
        command_id: CommandId,
        stream: crate::OutputStream,
        offset: u64,
        bytes: &[u8],
    ) -> Result<Self, ValidationError> {
        require(
            !bytes.is_empty() && bytes.len() <= MAX_COMMAND_CHUNK_BYTES,
            "command chunk size",
        )?;
        let mut bytes_hex = String::with_capacity(bytes.len() * 2);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in bytes {
            bytes_hex.push(HEX[(byte >> 4) as usize] as char);
            bytes_hex.push(HEX[(byte & 15) as usize] as char);
        }
        let value = Self {
            command_id,
            stream,
            offset,
            bytes_hex,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn original_bytes(&self) -> Result<Vec<u8>, ValidationError> {
        self.validate()?;
        fn nibble(b: u8) -> u8 {
            if b <= b'9' { b - b'0' } else { b - b'a' + 10 }
        }
        Ok(self
            .bytes_hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| nibble(p[0]) * 16 + nibble(p[1]))
            .collect())
    }
}

impl ControlledCommandFinished {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.after.validate()?;
        require(
            self.started_millis <= self.finished_millis
                && self.finished_millis <= MAX_MONOTONIC_MILLIS,
            "command elapsed time bounds",
        )?;
        require(
            self.stdout.bytes.saturating_add(self.stderr.bytes) <= MAX_COMMAND_OUTPUT_BYTES,
            "command capture size",
        )?;
        for capture in [&self.stdout, &self.stderr] {
            require(
                capture.completeness != CaptureCompleteness::Unavailable || capture.bytes == 0,
                "unavailable capture cannot contain bytes",
            )?;
            require(
                capture.mode != CommandCaptureMode::Redirected
                    || capture.completeness == CaptureCompleteness::Unavailable
                        && capture.bytes == 0,
                "redirected capture is unavailable and empty",
            )?;
        }
        if matches!(
            self.outcome,
            CommandOutcome::Terminated {
                reason: CommandTermination::OutputLimit,
                ..
            }
        ) {
            require(
                [&self.stdout, &self.stderr]
                    .iter()
                    .any(|capture| capture.completeness == CaptureCompleteness::Truncated),
                "output-limit termination requires truncated capture",
            )?;
        }
        if matches!(
            self.outcome,
            CommandOutcome::Terminated {
                reason: CommandTermination::CaptureFailure,
                ..
            }
        ) {
            require(
                [&self.stdout, &self.stderr]
                    .iter()
                    .any(|capture| capture.completeness == CaptureCompleteness::ReadFailed),
                "capture failure requires read-failed capture",
            )?;
        }
        if matches!(self.outcome, CommandOutcome::LaunchFailed { .. }) {
            require(
                [&self.stdout, &self.stderr]
                    .iter()
                    .all(|c| c.completeness == CaptureCompleteness::Unavailable),
                "launch failure requires unavailable streams",
            )?;
        } else if matches!(
            self.outcome,
            CommandOutcome::Terminated {
                reason: CommandTermination::Cancelled | CommandTermination::Quit,
                signal: None
            }
        ) && [&self.stdout, &self.stderr]
            .iter()
            .filter(|capture| capture.mode == CommandCaptureMode::Captured)
            .all(|capture| capture.completeness == CaptureCompleteness::Unavailable)
        {
            // Cancellation may win after the durable start but before spawn.
            // No stream existed; do not invent a complete empty capture.
        } else {
            require(
                [&self.stdout, &self.stderr].iter().all(|capture| {
                    capture.mode == CommandCaptureMode::Redirected
                        || capture.completeness != CaptureCompleteness::Unavailable
                }),
                "launched command must describe each captured stream",
            )?;
        }
        Ok(())
    }
}

impl TestCaseCompared {
    pub fn validate(&self) -> Result<(), ValidationError> {
        require(
            !self.case.is_empty()
                && self.case.len() <= MAX_TEST_CASE_NAME_BYTES
                && self
                    .case
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
            "test-case name grammar",
        )?;
        match self.outcome {
            TestCaseComparisonOutcome::Pass => require(
                self.actual_blake3 == Some(self.expected_blake3),
                "PASS requires equal expected and actual hashes",
            ),
            TestCaseComparisonOutcome::Mismatch {
                line,
                expected_len,
                actual_len,
            } => require(
                self.actual_blake3.is_some()
                    && self.actual_blake3 != Some(self.expected_blake3)
                    && (1..=MAX_TEST_CASE_COMPARISON_LINE).contains(&line)
                    && expected_len <= MAX_TEST_CASE_EXPECTED_LINE_BYTES
                    // Each preceding expected line needs at least one LF byte.
                    && (line - 1).checked_add(expected_len).is_some_and(|minimum_bytes| {
                        minimum_bytes <= MAX_TEST_CASE_EXPECTED_LINE_BYTES
                    })
                    && actual_len <= MAX_COMMAND_OUTPUT_BYTES,
                "bounded mismatch line and lengths with unequal captured bytes",
            ),
            TestCaseComparisonOutcome::Error { .. } => Ok(()),
        }
    }
}
