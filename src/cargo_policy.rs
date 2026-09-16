//! T4.3 preparation only. Execution/barriers/evidence belong to T4.2.
//! It validates allowed commands and prepares bounded arguments and environments.
use rustrace_model::{WorkspacePath, is_valid_crates_io_dependency_spec, is_valid_crates_io_name};
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
/// are opened by the session's fixed sibling test-case authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsoleCommand {
    pub action: CargoAction,
    pub argv: Vec<String>,
    pub stdin: Option<WorkspacePath>,
    pub stdout: Option<WorkspacePath>,
}

/// Parse the bounded literal T5.7 console grammar before any effect.
pub fn parse_console_command(_input: &str) -> Result<ConsoleCommand, PreparationError> {
    let input = _input;
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
        ["cargo", "test"] => CargoAction::Test,
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
            stdin: None,
            stdout: None,
        });
    }

    let mut index = 2;
    let release = tokens.get(index) == Some(&"--release");
    if release {
        index += 1;
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
                "unsupported Cargo command; allowed: cargo build, cargo check, cargo test, cargo run, cargo clippy, cargo doc, cargo add NAME, cargo add NAME@VERSION, cargo remove NAME, cargo update"
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

/// Prepare a parsed console action. Redirect paths never enter this argv.
pub fn prepare_console(
    request: &ConsoleCommand,
    tools: &ResolvedTools,
    workspace: &Path,
) -> Result<PreparedCargoCommand, PreparationError> {
    let release = request.argv.as_slice() == ["cargo", "run", "--release"];
    let valid = match request.action {
        CargoAction::Build => request.argv.as_slice() == ["cargo", "build"],
        CargoAction::Check => request.argv.as_slice() == ["cargo", "check"],
        CargoAction::Test => request.argv.as_slice() == ["cargo", "test"],
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
    if !valid
        || request.action != CargoAction::Run
            && (request.stdin.is_some() || request.stdout.is_some())
    {
        return Err(PreparationError::UnsupportedCommand);
    }
    prepare_inner(
        request.action,
        tools,
        workspace,
        release,
        false,
        false,
        request.argv.get(2..).unwrap_or_default(),
    )
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
        // Cargo template-expands this setting before resolving it against cwd.
        // Keep literal braces in the workspace path out of that template.
        .env("CARGO_BUILD_BUILD_DIR", "target")
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
