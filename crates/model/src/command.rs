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
/// Maximum program arguments in one packaged `NAME.args` file or console Run route.
pub const MAX_TEST_CASE_ARGS: usize = 64;
/// Maximum UTF-8 bytes in one program argument.
pub const MAX_TEST_CASE_ARG_BYTES: usize = 1024;
/// Maximum bytes in one packaged `NAME.args` file. It also bounds the combined
/// argument bytes of one recorded console Run route.
pub const MAX_TEST_CASE_ARGS_FILE_BYTES: usize = 8 * 1024;

const TEST_CASE_ARGS_DOMAIN: &[u8] = b"rustrace.test-case-args.v1";

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

/// How one console command was connected. The three trailing fields were added
/// for assignment format 3 and are omitted at their defaults, so format 2
/// routes keep their exact historical bytes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsoleCommandRoute {
    pub stdin: ConsoleStdinRoute,
    pub stdout: ConsoleStdoutRoute,
    /// Literal program arguments; a Run argv ends with `--` and exactly these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Where the program ran. `Fixtures` pins the deployed fixture tree.
    #[serde(default, skip_serializing_if = "ConsoleWorkingDirectory::is_workspace")]
    pub working_directory: ConsoleWorkingDirectory,
    /// The packaged format 3 case this Run executes. Only format 3 picker runs
    /// carry it; it is the one route on which a Run may have closed stdin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_case: Option<String>,
}

impl ConsoleCommandRoute {
    /// A route with no program arguments, in the workspace, for no packaged case.
    pub fn new(stdin: ConsoleStdinRoute, stdout: ConsoleStdoutRoute) -> Self {
        Self {
            stdin,
            stdout,
            args: Vec::new(),
            working_directory: ConsoleWorkingDirectory::Workspace,
            test_case: None,
        }
    }

    /// True when the route uses none of the format 3 additions.
    pub fn is_plain(&self) -> bool {
        self.args.is_empty() && self.working_directory.is_workspace() && self.test_case.is_none()
    }

    /// The route of a format 3 packaged-case Run: stdin is the case's own
    /// `NAME.in` when it has one and closed otherwise, stdout is captured for
    /// comparison, and a fixture hash selects the `test-cases/files/` working
    /// directory. The Run argv must then end with
    /// `--locked [--manifest-path PATH] [-- ARG...]` to validate.
    pub fn packaged_case(
        case: &str,
        has_input: bool,
        args: Vec<String>,
        fixtures_blake3: Option<Hash>,
    ) -> Result<Self, ValidationError> {
        require(is_valid_test_case_name(case), "test-case name grammar")?;
        require(
            are_valid_test_case_args(&args),
            "bounded literal console Run arguments",
        )?;
        let stdin = if has_input {
            ConsoleStdinRoute::File {
                path: WorkspacePath::new(format!("{case}.in"))
                    .expect("a valid case name makes a valid input path"),
            }
        } else {
            ConsoleStdinRoute::Closed
        };
        Ok(Self {
            stdin,
            stdout: ConsoleStdoutRoute::Console,
            args,
            working_directory: fixtures_blake3
                .map_or(ConsoleWorkingDirectory::Workspace, |fixtures_blake3| {
                    ConsoleWorkingDirectory::Fixtures { fixtures_blake3 }
                }),
            test_case: Some(case.to_owned()),
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConsoleWorkingDirectory {
    #[default]
    Workspace,
    /// The sibling `test-cases/files/` tree, with its fixture-tree hash
    /// computed immediately before launch.
    Fixtures { fixtures_blake3: Hash },
}

impl ConsoleWorkingDirectory {
    pub fn is_workspace(&self) -> bool {
        *self == Self::Workspace
    }

    pub fn fixtures_blake3(&self) -> Option<Hash> {
        match self {
            Self::Workspace => None,
            Self::Fixtures { fixtures_blake3 } => Some(*fixtures_blake3),
        }
    }
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
    /// The packaged format 3 case identity the Run used. Absent for format 2
    /// comparisons, whose bytes stay unchanged; present exactly when the
    /// compared Run's console route names the case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation: Option<TestCaseInvocation>,
}

/// What a format 3 comparison claims about its packaged case, for replay to
/// link to the Run route and for verify to compare with a reference package.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestCaseInvocation {
    /// [`test_case_args_blake3`] of the route's program arguments.
    pub args_blake3: Hash,
    /// Closed, or the BLAKE3 of the `NAME.in` bytes read before launch.
    pub stdin: TestCaseStdin,
    /// The fixture-tree hash when the Run used `test-cases/files/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixtures_blake3: Option<Hash>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TestCaseStdin {
    Closed,
    File { blake3: Hash },
}

impl TestCaseInvocation {
    /// The invocation block for a comparison of a Run on `route`, given the
    /// BLAKE3 of the `NAME.in` bytes that were read before launch. Returns
    /// `None` unless the route names a packaged case and the input hash is
    /// present exactly when the route reads `NAME.in`.
    pub fn for_route(route: &ConsoleCommandRoute, input_blake3: Option<Hash>) -> Option<Self> {
        route.test_case.as_ref()?;
        let stdin = match (&route.stdin, input_blake3) {
            (ConsoleStdinRoute::Closed, None) => TestCaseStdin::Closed,
            (ConsoleStdinRoute::File { .. }, Some(blake3)) => TestCaseStdin::File { blake3 },
            _ => return None,
        };
        Some(Self {
            args_blake3: test_case_args_blake3(&route.args),
            stdin,
            fixtures_blake3: route.working_directory.fixtures_blake3(),
        })
    }
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

/// Packaged test-case names: 1 to 64 ASCII letters, digits, `-`, or `_`.
pub fn is_valid_test_case_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_TEST_CASE_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// One literal program argument: nonempty, bounded, with no control character
/// (which also excludes NUL, CR, LF, and tab).
pub fn is_valid_test_case_arg(argument: &str) -> bool {
    !argument.is_empty()
        && argument.len() <= MAX_TEST_CASE_ARG_BYTES
        && !argument.chars().any(char::is_control)
}

/// A recorded argument list: every argument valid, at most
/// [`MAX_TEST_CASE_ARGS`] of them, and at most
/// [`MAX_TEST_CASE_ARGS_FILE_BYTES`] argument bytes in total. Every list parsed
/// from a valid `NAME.args` file satisfies this bound.
pub fn are_valid_test_case_args<T: AsRef<str>>(arguments: &[T]) -> bool {
    arguments.len() <= MAX_TEST_CASE_ARGS
        && arguments
            .iter()
            .all(|argument| is_valid_test_case_arg(argument.as_ref()))
        && arguments
            .iter()
            .map(|argument| argument.as_ref().len())
            .sum::<usize>()
            <= MAX_TEST_CASE_ARGS_FILE_BYTES
}

/// Why a packaged `NAME.args` file is invalid. Lines are one-based.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestCaseArgsError {
    TooLarge { actual: usize },
    InvalidUtf8,
    MissingFinalNewline,
    EmptyArgument { line: usize },
    ControlCharacter { line: usize },
    ArgumentTooLong { line: usize },
    TooManyArguments,
}

impl std::fmt::Display for TestCaseArgsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { actual } => write!(
                formatter,
                "is {actual} bytes; the limit is {MAX_TEST_CASE_ARGS_FILE_BYTES} bytes"
            ),
            Self::InvalidUtf8 => formatter.write_str("is not valid UTF-8"),
            Self::MissingFinalNewline => {
                formatter.write_str("must end every argument line with LF")
            }
            Self::EmptyArgument { line } => write!(formatter, "line {line} is empty"),
            Self::ControlCharacter { line } => write!(
                formatter,
                "line {line} contains a control character such as CR, tab, or NUL"
            ),
            Self::ArgumentTooLong { line } => write!(
                formatter,
                "line {line} exceeds the {MAX_TEST_CASE_ARG_BYTES}-byte argument limit"
            ),
            Self::TooManyArguments => {
                write!(formatter, "has more than {MAX_TEST_CASE_ARGS} arguments")
            }
        }
    }
}

impl std::error::Error for TestCaseArgsError {}

/// Parses a packaged `NAME.args` file: UTF-8 text holding one literal argument
/// per LF-terminated line, with no shell parsing. An empty file has no
/// arguments. Blank lines, a missing final LF, and control characters
/// (including CR) are rejected rather than normalized.
pub fn parse_test_case_args(bytes: &[u8]) -> Result<Vec<String>, TestCaseArgsError> {
    if bytes.len() > MAX_TEST_CASE_ARGS_FILE_BYTES {
        return Err(TestCaseArgsError::TooLarge {
            actual: bytes.len(),
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| TestCaseArgsError::InvalidUtf8)?;
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let body = text
        .strip_suffix('\n')
        .ok_or(TestCaseArgsError::MissingFinalNewline)?;
    let mut arguments = Vec::new();
    for (index, argument) in body.split('\n').enumerate() {
        let line = index + 1;
        if argument.is_empty() {
            return Err(TestCaseArgsError::EmptyArgument { line });
        }
        if argument.chars().any(char::is_control) {
            return Err(TestCaseArgsError::ControlCharacter { line });
        }
        if argument.len() > MAX_TEST_CASE_ARG_BYTES {
            return Err(TestCaseArgsError::ArgumentTooLong { line });
        }
        if arguments.len() == MAX_TEST_CASE_ARGS {
            return Err(TestCaseArgsError::TooManyArguments);
        }
        arguments.push(argument.to_owned());
    }
    Ok(arguments)
}

/// Domain-separated BLAKE3 of an argument list: the domain
/// `rustrace.test-case-args.v1`, the count as a big-endian `u32`, then each
/// argument as a big-endian `u32` byte length followed by its UTF-8 bytes.
pub fn test_case_args_blake3<T: AsRef<str>>(arguments: &[T]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(TEST_CASE_ARGS_DOMAIN);
    hasher.update(&(arguments.len() as u32).to_be_bytes());
    for argument in arguments {
        let argument = argument.as_ref().as_bytes();
        hasher.update(&(argument.len() as u32).to_be_bytes());
        hasher.update(argument);
    }
    Hash::from_bytes(*hasher.finalize().as_bytes())
}

/// The recorded `--manifest-path` value of a fixture-directory Run: an
/// absolute, normalized path to the workspace `Cargo.toml`, with no empty,
/// `.`, or `..` component.
fn is_valid_run_manifest_path(path: &str) -> bool {
    path.len() <= MAX_STRING_BYTES
        && !path.chars().any(char::is_control)
        && path.strip_prefix('/').is_some_and(|relative| {
            relative.ends_with("Cargo.toml")
                && relative
                    .split('/')
                    .all(|component| !matches!(component, "" | "." | ".."))
                && relative.rsplit('/').next() == Some("Cargo.toml")
        })
}

/// The natural console Run tail, in fixed order:
/// `[--release] --locked [--manifest-path PATH] [-- ARG...]`. The manifest
/// path appears exactly for a fixture working directory, and `-- ARG...`
/// exactly when the route has arguments, which it must equal.
fn is_valid_natural_run_tail(tail: &[String], route: &ConsoleCommandRoute) -> bool {
    let mut rest = tail;
    if rest.first().is_some_and(|flag| flag == "--release") {
        rest = &rest[1..];
    }
    let Some((locked, after_locked)) = rest.split_first() else {
        return false;
    };
    if locked != "--locked" {
        return false;
    }
    rest = after_locked;
    if !route.working_directory.is_workspace() {
        let [flag, path, after_path @ ..] = rest else {
            return false;
        };
        if flag != "--manifest-path" || !is_valid_run_manifest_path(path) {
            return false;
        }
        rest = after_path;
    }
    if route.args.is_empty() {
        rest.is_empty()
    } else {
        rest.split_first()
            .is_some_and(|(separator, arguments)| separator == "--" && arguments == route.args)
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

/// The bounded literal arguments accepted after a manual `cargo test`.
pub fn is_valid_console_test_tail<T: AsRef<str>>(tail: &[T]) -> bool {
    fn is_valid_filter(value: &str) -> bool {
        (1..=256).contains(&value.len())
            && value.as_bytes()[0] != b'-'
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'-'))
    }

    fn is_valid_output_option(value: &str) -> bool {
        matches!(value, "--nocapture" | "--no-capture" | "--show-output")
    }

    match tail {
        [] => true,
        [filter] => is_valid_filter(filter.as_ref()),
        [separator, option] => {
            separator.as_ref() == "--" && is_valid_output_option(option.as_ref())
        }
        [filter, separator, option] => {
            is_valid_filter(filter.as_ref())
                && separator.as_ref() == "--"
                && is_valid_output_option(option.as_ref())
        }
        _ => false,
    }
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
    /// The launcher and Cargo arguments, without a trailing `-- ARG...` of
    /// program arguments recorded on the console route. A program argument
    /// that spells a Cargo flag therefore never looks like a Cargo option.
    pub fn cargo_argv(&self) -> &[String] {
        match &self.console {
            Some(route) if !route.args.is_empty() => {
                let program = route.args.len() + 1;
                self.argv
                    .len()
                    .checked_sub(program)
                    .filter(|split| {
                        self.argv[*split] == "--" && self.argv[*split + 1..] == route.args
                    })
                    .map_or(&self.argv[..], |split| &self.argv[..split])
            }
            _ => &self.argv,
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        require(
            (1..=MAX_COMMAND_DEADLINE_MILLIS).contains(&self.deadline_millis)
                && self.output_limit <= MAX_COMMAND_OUTPUT_BYTES
                && self.output_limit > 0,
            "command resource bounds",
        )?;
        // Program arguments after `--` are bounded separately by the route.
        let program_arguments = self
            .console
            .as_ref()
            .map_or(0, |route| route.args.len().min(MAX_TEST_CASE_ARGS));
        require(
            self.argv.len() >= 2
                && self.argv.len() <= 40 + program_arguments
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
        if let Some(route) = &self.console {
            require(
                self.action == ControlledAction::Run || route.is_plain(),
                "program arguments, fixtures, and test cases require a console Run",
            )?;
        }
        match (&self.console, self.action) {
            (None, ControlledAction::Build) => {
                return Err(ValidationError::CommandEvidence {
                    detail: "action is unavailable on this command route",
                });
            }
            (Some(route), ControlledAction::Run) => {
                require(
                    are_valid_test_case_args(&route.args),
                    "bounded literal console Run arguments",
                )?;
                if let Some(case) = &route.test_case {
                    // A format 3 packaged case: stdin is its `NAME.in` or,
                    // for a case without one, closed. Stdout is compared.
                    require(is_valid_test_case_name(case), "test-case name grammar")?;
                    require(
                        route.stdout == ConsoleStdoutRoute::Console
                            && match &route.stdin {
                                ConsoleStdinRoute::Closed => true,
                                ConsoleStdinRoute::File { path } => {
                                    path.as_str().strip_suffix(".in") == Some(case.as_str())
                                }
                                ConsoleStdinRoute::Submitted => false,
                            },
                        "packaged test-case Run route",
                    )?;
                } else {
                    require(
                        matches!(
                            route.stdin,
                            ConsoleStdinRoute::Submitted | ConsoleStdinRoute::File { .. }
                        ),
                        "Run console stdin route",
                    )?;
                }
                if let (
                    ConsoleStdinRoute::File { path: input },
                    ConsoleStdoutRoute::File { path: output },
                ) = (&route.stdin, &route.stdout)
                {
                    require(input != output, "distinct console redirection paths")?;
                }
                let tail = self.argv.get(5..).unwrap_or_default();
                // Historical `--frozen` and structured forms never carry the
                // format 3 route additions.
                let old = tail == ["--frozen"] || tail == ["--release", "--frozen"];
                let structured = tail == ["--message-format=json", "--locked"]
                    || tail == ["--release", "--message-format=json", "--locked"];
                require(
                    (old || structured) && route.is_plain()
                        || is_valid_natural_run_tail(tail, route),
                    "literal console Run argv",
                )?;
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
                let console_test = self.action == ControlledAction::Test
                    && matches!(tail, [locked, test_tail @ ..]
                        if locked == "--locked" && is_valid_console_test_tail(test_tail));
                require(
                    route.stdin == ConsoleStdinRoute::Closed
                        && route.stdout == ConsoleStdoutRoute::Console
                        && (old || current || natural || console_test),
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
            is_valid_test_case_name(&self.case),
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
