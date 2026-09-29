//! T4.3 preparation only. Execution/barriers/evidence belong to T4.2.
//! It validates allowed commands and prepares bounded arguments and environments.
use rustrace_model::{
    WorkspacePath, are_valid_test_case_args, is_valid_console_test_tail,
    is_valid_crates_io_dependency_spec, is_valid_crates_io_name, is_valid_test_case_arg,
};
use std::{
    fmt,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CargoAction {
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

impl CargoAction {
    pub(crate) fn subcommand(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Check => "check",
            Self::Test => "test",
            Self::Run => "run",
            Self::Clippy => "clippy",
            Self::Format => "fmt",
            Self::Doc => "doc",
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Update => "update",
        }
    }

    pub const fn is_dependency(self) -> bool {
        matches!(self, Self::Add | Self::Remove | Self::Update)
    }

    const fn is_compile(self) -> bool {
        matches!(
            self,
            Self::Build | Self::Check | Self::Test | Self::Run | Self::Clippy | Self::Doc
        )
    }
}

/// A parsed manual console command. Redirection paths stay outside argv and
/// are opened by the session's sibling test-case authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsoleCommand {
    pub action: CargoAction,
    /// The literal Cargo command: `cargo run` or `cargo run --release` for a
    /// Run, never its program arguments.
    pub argv: Vec<String>,
    /// A Run's literal program arguments, passed after `--`. Empty for every
    /// other action.
    pub args: Vec<String>,
    pub stdin: Option<WorkspacePath>,
    pub stdout: Option<WorkspacePath>,
}

/// Parse the bounded literal console grammar before any effect.
///
/// A Run is `cargo run [--release] [-- ARG...] [< IN] [> OUT]`. Tokens are
/// separated by ASCII whitespace and taken literally: there are no quotes,
/// escapes, wildcards, pipes, or variables, so an argument cannot contain a
/// space. `--` must be followed by at least one argument. Arguments end at
/// the first `<` or `>` token, so redirections always follow the arguments
/// and are never passed to the program; an argument may not contain `<` or
/// `>` at all, and one redirection before `--` is refused. The two
/// redirections may appear in either order, each at most once. Arguments
/// obey the packaged `NAME.args` bounds: at most 64, each 1 to 1024 bytes.
pub fn parse_console_command(input: &str) -> Result<ConsoleCommand, PreparationError> {
    if input.is_empty()
        || input.len() > 4096
        || input.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '\'' | '"'
                        | '\\'
                        | '$'
                        | '`'
                        | '|'
                        | '&'
                        | ';'
                        | '*'
                        | '?'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '('
                        | ')'
                        | '!'
                        | '~'
                        | '#'
                )
        })
    {
        return Err(PreparationError::UnsupportedCommand);
    }
    let tokens = input.split_ascii_whitespace().collect::<Vec<_>>();
    let action = match tokens.as_slice() {
        ["cargo", "build"] => CargoAction::Build,
        ["cargo", "check"] => CargoAction::Check,
        ["cargo", "test", tail @ ..] if is_valid_console_test_tail(tail) => CargoAction::Test,
        ["cargo", "clippy"] => CargoAction::Clippy,
        ["cargo", "doc"] => CargoAction::Doc,
        ["cargo", "add", dependency] if is_valid_crates_io_dependency_spec(dependency) => {
            CargoAction::Add
        }
        ["cargo", "remove", name] if is_valid_crates_io_name(name) => CargoAction::Remove,
        ["cargo", "update"] => CargoAction::Update,
        ["cargo", "run", ..] => CargoAction::Run,
        _ => return Err(PreparationError::UnsupportedCommand),
    };
    if action != CargoAction::Run {
        return Ok(ConsoleCommand {
            action,
            argv: tokens.into_iter().map(str::to_owned).collect(),
            args: Vec::new(),
            stdin: None,
            stdout: None,
        });
    }

    let mut index = 2;
    let release = tokens.get(index) == Some(&"--release");
    if release {
        index += 1;
    }
    let mut args = Vec::new();
    if tokens.get(index) == Some(&"--") {
        index += 1;
        while let Some(argument) = tokens.get(index)
            && !matches!(*argument, "<" | ">")
        {
            if argument.contains(['<', '>']) || !is_valid_test_case_arg(argument) {
                return Err(PreparationError::UnsupportedCommand);
            }
            args.push((*argument).to_owned());
            index += 1;
        }
        if args.is_empty() || !are_valid_test_case_args(&args) {
            return Err(PreparationError::UnsupportedCommand);
        }
    }
    let mut stdin = None;
    let mut stdout = None;
    while index < tokens.len() {
        let redirect = tokens[index];
        let Some(value) = tokens.get(index + 1) else {
            return Err(PreparationError::UnsupportedCommand);
        };
        if !matches!(redirect, "<" | ">")
            || matches!(*value, "<" | ">" | "--release")
            || value.contains(['<', '>'])
        {
            return Err(PreparationError::UnsupportedCommand);
        }
        let path = WorkspacePath::new(*value).map_err(|_| PreparationError::UnsupportedCommand)?;
        let destination = if redirect == "<" {
            &mut stdin
        } else {
            &mut stdout
        };
        if destination.replace(path).is_some() {
            return Err(PreparationError::UnsupportedCommand);
        }
        index += 2;
    }
    let mut argv = vec!["cargo".to_owned(), "run".to_owned()];
    if release {
        argv.push("--release".to_owned());
    }
    if stdin == stdout && stdin.is_some() {
        return Err(PreparationError::UnsupportedCommand);
    }
    Ok(ConsoleCommand {
        action,
        argv,
        args,
        stdin,
        stdout,
    })
}

/// Execution-time inputs from the trusted installed-tool resolver, NOT labels.
/// The consumer must gate rustup >= 1.28.1 with T4.1's safe bootstrap, verify
/// the installed selection, and obtain each path with `rustup which
/// --toolchain SELECTED PROGRAM`. Never synthesize paths from version strings,
/// substitute PATH proxies, or treat an old observation as fresh resolution.
/// Preparation validates syntax; it neither resolves nor attests these inputs.
#[derive(Clone)]
pub struct ResolvedTools {
    pub rustup: PathBuf,
    pub selection: String,
    pub cargo: PathBuf,
    pub rustc: PathBuf,
    pub rustdoc: PathBuf,
    pub cargo_clippy: Option<PathBuf>,
    /// (cargo-fmt, rustfmt), both resolved in the selected toolchain.
    pub formatter: Option<(PathBuf, PathBuf)>,
}

/// Bounded, nonsecret metadata for T4.2's future schema mapping.
/// Only fixed names/policy constants: never environment values, paths or hashes
/// of values. Tools/versions and tree/prefix links are separate evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnvironmentSummary {
    pub policy_version: u32,
    pub retained_names: Vec<&'static str>,
    pub compiler_and_tools: &'static str,
    pub wrappers_and_extra_flags: &'static str,
    pub cargo_network: &'static str,
    pub cargo_lockfile: &'static str,
    pub cargo_output: &'static str,
    pub configuration_files: &'static str,
}

/// A mutable standard Command is a preparation result, not execution authority.
/// Consumers must record the final argv/summary and use their accepted barriers;
/// changing its environment/arguments invalidates this preparation's summary.
pub struct PreparedCargoCommand {
    pub command: Command,
    pub summary: EnvironmentSummary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparationError {
    UnsupportedCommand,
    InvalidToolSelection,
    InvalidPath,
    MissingOptionalTool,
}

impl fmt::Display for PreparationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedCommand => {
                "unsupported Cargo command; allowed: cargo build, cargo check, cargo test [FILTER] [-- OUTPUT_OPTION] (OUTPUT_OPTION: --nocapture, --no-capture, or --show-output), cargo run [--release] [-- ARGS] [< IN] [> OUT], cargo clippy, cargo doc, cargo add NAME, cargo add NAME@VERSION, cargo remove NAME, cargo update"
            }
            Self::InvalidToolSelection => {
                "expected an explicitly resolved installed toolchain name"
            }
            Self::InvalidPath => "expected a bounded absolute tool or workspace path",
            Self::MissingOptionalTool => {
                "selected optional component is unavailable; repair it outside Rustrace"
            }
        })
    }
}

impl std::error::Error for PreparationError {}

/// Validate the literal instructor vector before reading environment or
/// preparing a process. No filesystem writes, tool probes, or spawn occur.
/// All compiling actions normalize locking/offline switches to `--locked`.
/// Format and dependency actions require separate transaction ownership.
pub fn prepare(
    action: CargoAction,
    instructor_argv: &[String],
    tools: &ResolvedTools,
    workspace: &Path,
) -> Result<PreparedCargoCommand, PreparationError> {
    let deny_warnings = validate_command(action, instructor_argv)?;
    prepare_inner(
        action,
        tools,
        workspace,
        false,
        true,
        deny_warnings,
        instructor_argv.get(2..).unwrap_or_default(),
    )
}

/// Prepare a parsed console action run in the workspace. Redirect paths
/// never enter this argv.
pub fn prepare_console(
    request: &ConsoleCommand,
    tools: &ResolvedTools,
    workspace: &Path,
) -> Result<PreparedCargoCommand, PreparationError> {
    prepare_console_in(request, tools, workspace, None)
}

/// Prepare a parsed console action. A Run with `fixtures` runs from that
/// format 3 fixture folder (see [`PreparedCargoCommand::run_from_directory`]);
/// otherwise it runs in the workspace. A Run's program arguments end the
/// argument vector as `-- ARG...`, after every Cargo option. They need only
/// the recorded-argument bounds: a packaged `NAME.args` may hold spaces,
/// quotes, `<`, or `>`, which only the typed console grammar excludes.
pub fn prepare_console_in(
    request: &ConsoleCommand,
    tools: &ResolvedTools,
    workspace: &Path,
    fixtures: Option<&Path>,
) -> Result<PreparedCargoCommand, PreparationError> {
    let release = request.argv.as_slice() == ["cargo", "run", "--release"];
    let valid = match request.action {
        CargoAction::Build => request.argv.as_slice() == ["cargo", "build"],
        CargoAction::Check => request.argv.as_slice() == ["cargo", "check"],
        CargoAction::Test => matches!(request.argv.as_slice(), [cargo, test, tail @ ..]
            if cargo == "cargo" && test == "test" && is_valid_console_test_tail(tail)),
        CargoAction::Clippy => request.argv.as_slice() == ["cargo", "clippy"],
        CargoAction::Run => request.argv.as_slice() == ["cargo", "run"] || release,
        CargoAction::Format => false,
        CargoAction::Doc => request.argv.as_slice() == ["cargo", "doc"],
        CargoAction::Add => matches!(request.argv.as_slice(), [cargo, add, dependency]
            if cargo == "cargo" && add == "add" && is_valid_crates_io_dependency_spec(dependency)),
        CargoAction::Remove => matches!(request.argv.as_slice(), [cargo, remove, name]
            if cargo == "cargo" && remove == "remove" && is_valid_crates_io_name(name)),
        CargoAction::Update => request.argv.as_slice() == ["cargo", "update"],
    };
    let run = request.action == CargoAction::Run;
    if !valid
        || !run
            && (request.stdin.is_some()
                || request.stdout.is_some()
                || !request.args.is_empty()
                || fixtures.is_some())
        || !are_valid_test_case_args(&request.args)
    {
        return Err(PreparationError::UnsupportedCommand);
    }
    let mut prepared = prepare_inner(
        request.action,
        tools,
        workspace,
        release,
        false,
        false,
        request.argv.get(2..).unwrap_or_default(),
    )?;
    if let Some(fixtures) = fixtures {
        prepared.run_from_directory(workspace, fixtures)?;
    }
    if !request.args.is_empty() {
        prepared.command.arg("--").args(&request.args);
    }
    Ok(prepared)
}

fn prepare_inner(
    action: CargoAction,
    tools: &ResolvedTools,
    workspace: &Path,
    release: bool,
    structured_output: bool,
    deny_warnings: bool,
    literal_tail: &[String],
) -> Result<PreparedCargoCommand, PreparationError> {
    if !crate::toolchain::valid_name(&tools.selection) {
        return Err(PreparationError::InvalidToolSelection);
    }
    for path in [
        workspace,
        &tools.rustup,
        &tools.cargo,
        &tools.rustc,
        &tools.rustdoc,
    ] {
        validate_path(path)?;
    }
    let executable = match action {
        CargoAction::Clippy => tools
            .cargo_clippy
            .as_deref()
            .ok_or(PreparationError::MissingOptionalTool)?,
        CargoAction::Format => {
            let (driver, formatter) = tools
                .formatter
                .as_ref()
                .ok_or(PreparationError::MissingOptionalTool)?;
            validate_path(formatter)?;
            driver
        }
        _ => &tools.cargo,
    };
    validate_path(executable)?;
    let mut command = Command::new(&tools.rustup);
    let retained_names = retain_execution_environment(&mut command);
    command
        .args(["run", tools.selection.as_str()])
        .arg(executable)
        .arg(action.subcommand())
        .current_dir(workspace)
        .stdin(Stdio::null())
        .env("RUSTUP_AUTO_INSTALL", "0")
        .env("RUSTUP_TOOLCHAIN", &tools.selection)
        .env("CARGO", &tools.cargo)
        .env("RUSTC", &tools.rustc)
        .env("RUSTDOC", &tools.rustdoc)
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .env("CARGO_ENCODED_RUSTFLAGS", "")
        .env("CARGO_ENCODED_RUSTDOCFLAGS", "")
        .env("CARGO_TARGET_DIR", workspace.join("target"))
        .env(
            "CARGO_BUILD_BUILD_DIR",
            build_dir_from(workspace, workspace)?,
        )
        .env("CARGO_TERM_COLOR", "never")
        .env("CARGO_TERM_PROGRESS_WHEN", "never");
    if action.is_dependency() {
        command.args(literal_tail);
    }
    if action == CargoAction::Format {
        let (_, formatter) = tools.formatter.as_ref().expect("validated formatter");
        command.env("RUSTFMT", formatter);
    } else if action.is_compile() {
        if release {
            command.arg("--release");
        }
        // Structured output and lockfile immutability are controller-owned.
        // Console actions keep natural output for display; Run also preserves
        // exact program stdout for redirection and packaged-case comparison.
        if structured_output {
            command.arg("--message-format=json");
        }
        command.arg("--locked");
        if action == CargoAction::Test && !structured_output {
            command.args(literal_tail);
        }
    }
    if deny_warnings {
        command.args(["--", "-D", "warnings"]);
    }
    Ok(PreparedCargoCommand {
        command,
        summary: EnvironmentSummary {
            policy_version: 1,
            retained_names,
            compiler_and_tools: "explicit resolved selected executables",
            wrappers_and_extra_flags: "empty; Clippy may set its own driver wrapper",
            cargo_network: "network allowed; registry traffic unrecorded",
            cargo_lockfile: if action == CargoAction::Format {
                "no mutation authorized"
            } else if action.is_dependency() {
                "dependency action changes recorded through editor transactions"
            } else {
                "locked"
            },
            cargo_output: "workspace/target",
            configuration_files: "ambient Cargo/rustfmt files may influence execution; values unrecorded",
        },
    })
}

pub(crate) fn validate_command(
    action: CargoAction,
    argv: &[String],
) -> Result<bool, PreparationError> {
    if action == CargoAction::Build {
        return Err(PreparationError::UnsupportedCommand);
    }
    if argv.len() < 2 || argv[0] != "cargo" || argv[1] != action.subcommand() {
        return Err(PreparationError::UnsupportedCommand);
    }
    let mut tail = &argv[2..];
    if action == CargoAction::Format {
        return tail
            .is_empty()
            .then_some(false)
            .ok_or(PreparationError::UnsupportedCommand);
    }
    if action.is_dependency() {
        let allowed = match (action, tail) {
            (CargoAction::Add, [dependency]) => is_valid_crates_io_dependency_spec(dependency),
            (CargoAction::Remove, [name]) => is_valid_crates_io_name(name),
            (CargoAction::Update, []) => true,
            _ => false,
        };
        return allowed
            .then_some(false)
            .ok_or(PreparationError::UnsupportedCommand);
    }
    let deny_warnings = action == CargoAction::Clippy
        && tail.ends_with(&["--".into(), "-D".into(), "warnings".into()]);
    if deny_warnings {
        tail = &tail[..tail.len() - 3];
    }
    let allowed = match tail {
        [] => true,
        [flag] => matches!(flag.as_str(), "--locked" | "--offline" | "--frozen"),
        [first, second] => {
            (first == "--locked" && second == "--offline")
                || (first == "--offline" && second == "--locked")
        }
        _ => false,
    };
    allowed
        .then_some(deny_warnings)
        .ok_or(PreparationError::UnsupportedCommand)
}

/// `CARGO_BUILD_BUILD_DIR` for a command run from `working_directory`: always
/// `WORKSPACE/target`, spelled relative to the working directory.
///
/// Cargo substitutes `{workspace-root}`, `{cargo-cache-home}`, and
/// `{workspace-path-hash}` in `build.build-dir` and then refuses any brace
/// left in the result, including braces inside a substituted or absolute
/// workspace path (Cargo 1.98.1 reports "unexpected variable"). A relative
/// spelling never includes the workspace's ancestors. From the workspace it
/// is exactly `target`, as for every command before format 3; from a format
/// 3 fixture folder it is `../../WORKSPACE_NAME/target`, and format 3 refuses
/// workspace names containing braces. Both paths must be absolute and
/// normalized.
pub fn build_dir_from(
    workspace: &Path,
    working_directory: &Path,
) -> Result<PathBuf, PreparationError> {
    let mut relative = workspace_from(workspace, working_directory)?;
    relative.push("target");
    Ok(relative)
}

/// `workspace` spelled relative to `working_directory`, lexically. Both must be
/// absolute and normalized, and the result may not contain a brace.
fn workspace_from(workspace: &Path, working_directory: &Path) -> Result<PathBuf, PreparationError> {
    validate_path(workspace)?;
    validate_path(working_directory)?;
    let workspace = workspace.components().collect::<Vec<_>>();
    let working_directory = working_directory.components().collect::<Vec<_>>();
    let common = workspace
        .iter()
        .zip(&working_directory)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in common..working_directory.len() {
        relative.push("..");
    }
    for component in &workspace[common..] {
        relative.push(component.as_os_str());
    }
    let spelled = relative.to_str().ok_or(PreparationError::InvalidPath)?;
    if spelled.contains(['{', '}']) {
        return Err(PreparationError::InvalidPath);
    }
    Ok(relative)
}

impl PreparedCargoCommand {
    /// Runs a prepared console Run from `directory`, a format 3 fixture folder
    /// `WORKSPACE_PARENT/NAME.test-cases/files`, while Cargo still builds
    /// `workspace` into `WORKSPACE/target`. It appends
    /// `--manifest-path ../../WORKSPACE_NAME/Cargo.toml` after `--locked`, so
    /// call it before appending any `-- ARG...`. The recorded argument vector
    /// thus names only the workspace directory, never its absolute location.
    pub fn run_from_directory(
        &mut self,
        workspace: &Path,
        directory: &Path,
    ) -> Result<(), PreparationError> {
        let relative = workspace_from(workspace, directory)?;
        let mut components = relative.components();
        let fixture_sibling = matches!(
            (
                components.next(),
                components.next(),
                components.next(),
                components.next()
            ),
            (
                Some(Component::ParentDir),
                Some(Component::ParentDir),
                Some(Component::Normal(_)),
                None
            )
        );
        if !fixture_sibling {
            return Err(PreparationError::InvalidPath);
        }
        self.command
            .current_dir(directory)
            .env("CARGO_BUILD_BUILD_DIR", relative.join("target"))
            .arg("--manifest-path")
            .arg(relative.join("Cargo.toml"));
        Ok(())
    }
}

fn validate_path(path: &Path) -> Result<(), PreparationError> {
    if path.is_absolute()
        && path
            .to_str()
            .is_some_and(|s| s.len() <= 4096 && !s.chars().any(char::is_control))
        && !path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        Ok(())
    } else {
        Err(PreparationError::InvalidPath)
    }
}

/// Exact finite allowlist, shared with discovery and available to T5.1.
/// HOME/homes locate installed tools/caches/config; PATH locates platform
/// linkers/utilities; temporary directories support normal tool operation.
/// No inherited Cargo/compiler/profile/target/credential/logging variables.
/// Call before explicit selection/command-specific controls are applied.
pub fn retain_execution_environment(command: &mut Command) -> Vec<&'static str> {
    command.env_clear();
    let mut retained = Vec::new();
    for name in [
        "HOME",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "PATH",
        "TMPDIR",
        "TMP",
        "TEMP",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
            retained.push(name);
        }
    }
    retained
}
