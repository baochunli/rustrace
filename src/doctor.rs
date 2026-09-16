//! Pre-session diagnostics. This command creates only removable scratch state.

use crate::{
    display,
    session::SessionBudgets,
    toolchain::{ProbeStatus, ToolchainReport},
    tui::theme::{
        EffectiveTheme, PALETTE_FIELD_NAMES, ThemeColorSource, ThemeConfig, format_color,
        resolve_theme,
    },
    work,
};
use rustrace_model::{SessionId, WorkspacePath};
use rustrace_workspace::{
    assignment_package::AssignmentPackageError,
    create_workspace_file_in,
    hash::{PinnedWorkspaceRoot, hash_workspace},
    remove_workspace_file_in, rename_workspace_file_in, write_workspace_file_in,
};
use std::{
    error::Error,
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::{
    ffi::CString,
    os::{fd::AsRawFd, unix::ffi::OsStrExt, unix::fs::MetadataExt},
};

const USAGE: &str = "Usage: rustrace doctor assignment.rta [--workspace DIR] | rustrace doctor --write-ghostty-keys";
const MINIMUM_COLUMNS: u16 = 80;
const MINIMUM_ROWS: u16 = 24;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Level {
    Ok,
    Warning,
    Blocker,
}

impl Level {
    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warning => "WARNING",
            Self::Blocker => "BLOCKER",
        }
    }
}

#[derive(Debug)]
struct Check {
    name: &'static str,
    level: Level,
    detail: String,
    remedy: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            level: Level::Ok,
            detail: detail.into(),
            remedy: None,
        }
    }

    fn warning(name: &'static str, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            name,
            level: Level::Warning,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }

    fn blocker(name: &'static str, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            name,
            level: Level::Blocker,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }
}

pub fn run_doctor(args: &[String], output: &mut impl Write) -> Result<u8, String> {
    if args == ["--write-ghostty-keys"] {
        return write_ghostty_keys(output);
    }
    let (package, requested_workspace) = parse_args(args)?;
    crate::update::write_doctor_advisory(output).map_err(|error| error.to_string())?;
    let terminal = terminal_check();
    let loaded_config = crate::config::load_config();
    let (theme_config, config_warning) = loaded_config.map_or_else(
        || (ThemeConfig::default(), None),
        |loaded| (loaded.theme, loaded.warning),
    );
    let colorterm = std::env::var("COLORTERM").ok();
    let effective_theme = resolve_theme(&theme_config, colorterm.as_deref(), None);
    let theme = match config_warning {
        Some(warning) => Check::warning(
            "theme",
            format!("{warning}; effective {}", effective_theme.name),
            "Fix ~/.config/rustrace/config.toml, then rerun doctor.",
        ),
        None => Check::ok(
            "theme",
            format!(
                "effective {}; auto_switch = {}",
                effective_theme.name, theme_config.auto_switch
            ),
        ),
    };
    let (workspace, workspace_resolution) = resolve_workspace(&requested_workspace);
    let ownership = workspace
        .as_deref()
        .map(session_ownership_check)
        .unwrap_or_else(|| {
            Check::blocker(
                "session ownership",
                "not checked because the workspace location is unavailable",
                "Choose a workspace below an existing writable local directory, then rerun doctor.",
            )
        });
    let workspace_check = match workspace.as_deref() {
        Some(workspace) => writable_workspace_check(workspace),
        None => workspace_resolution,
    };

    let (scratch_base, filesystem_target, filesystem_scope) =
        filesystem_probe_location(workspace.as_deref(), &package);
    let mut scratch = scratch_base.and_then(|base| work::unique_sibling(&base, "doctor").ok());
    let mut extracted = None;
    let mut starter = Check::blocker(
        "starter package",
        "not checked because no safe scratch location is available",
        "Move the package beside an existing writable local directory, then rerun doctor.",
    );

    if let Some(path) = scratch.as_deref() {
        match work::extract_package(&package, path) {
            Ok(assignment) => {
                starter = Check::ok(
                    "starter package",
                    format!(
                        "self-contained package: manifest and {} starter files validated and extracted safely",
                        assignment.starter_files
                    ),
                );
                extracted = Some(assignment);
            }
            Err(error) => {
                starter = Check::blocker(
                    "starter package",
                    format!("validation failed: {error}"),
                    starter_package_remedy(error.as_ref(), path),
                );
            }
        }
    }

    let (disk, filesystem, cumulative, toolchain) =
        if let (Some(scratch), Some(extracted)) = (scratch.as_deref(), extracted.as_ref()) {
            let toolchain = Some(crate::toolchain::discover(
                scratch,
                Some(&extracted.manifest.toolchain),
            ));
            let (disk, filesystem, cumulative) =
                filesystem_checks(scratch, &filesystem_target, &filesystem_scope);
            (disk, filesystem, cumulative, toolchain)
        } else {
            (
                Check::blocker(
                    "disk space",
                    "not checked because the starter could not be extracted",
                    "Fix the starter package or workspace location, then rerun doctor.",
                ),
                Check::blocker(
                    "filesystem capabilities",
                    "not checked because the starter could not be extracted",
                    "Use a supported local filesystem and rerun doctor with a valid package.",
                ),
                Check::blocker(
                    "cumulative storage headroom",
                    "not checked because available local storage could not be measured",
                    "Fix the starter package or workspace location, then rerun doctor.",
                ),
                None,
            )
        };

    let pin = extracted
        .as_ref()
        .map(|assignment| assignment.manifest.toolchain.as_str());
    let tools = tool_checks(toolchain.as_ref(), pin);

    if let Some(path) = scratch.take()
        && path.try_exists().unwrap_or(true)
        && let Err(error) = fs::remove_dir_all(&path)
    {
        starter = Check::blocker(
            "starter package",
            format!("scratch cleanup failed after validation: {error}"),
            format!(
                "Remove the temporary directory {}, then rerun doctor.",
                path.display()
            ),
        );
    }

    let mut checks = vec![terminal, theme, workspace_check, disk];
    checks.extend(tools);
    checks.extend([starter, ownership, filesystem, cumulative]);
    let ghostty =
        crate::ghostty::applies_to_current_terminal().then(crate::ghostty::inspect_current);
    write_report(&checks, &effective_theme, ghostty.as_ref(), output)
        .map_err(|error| error.to_string())?;
    Ok(
        if checks.iter().any(|check| check.level == Level::Blocker) {
            1
        } else {
            0
        },
    )
}

fn write_ghostty_keys(output: &mut impl Write) -> Result<u8, String> {
    match crate::ghostty::write_keys_from_environment() {
        Ok((path, outcome)) => {
            let action = match outcome {
                crate::ghostty::WriteOutcome::Written => "wrote",
                crate::ghostty::WriteOutcome::UpToDate => "already up to date at",
            };
            writeln!(output, "Ghostty keys {action} {}", path.display())
                .map_err(|error| error.to_string())?;
            writeln!(output, "Add this line to the Ghostty config:")
                .map_err(|error| error.to_string())?;
            writeln!(output, "config-file = rustrace-keys").map_err(|error| error.to_string())?;
            writeln!(
                output,
                "Reload using Ghostty's own super+shift+,=reload_config binding (⌘⇧,)."
            )
            .map_err(|error| error.to_string())?;
            Ok(0)
        }
        Err(error) => {
            writeln!(output, "Ghostty keys not written: {error}")
                .map_err(|write_error| write_error.to_string())?;
            Ok(1)
        }
    }
}

fn parse_args(args: &[String]) -> Result<(PathBuf, PathBuf), String> {
    let package = args.first().ok_or_else(|| USAGE.to_owned())?;
    let package = PathBuf::from(package);
    let mut workspace = package.with_extension("work");
    let mut index = 1;
    while index < args.len() {
        if args[index] != "--workspace" || index + 1 >= args.len() {
            return Err(USAGE.to_owned());
        }
        workspace = PathBuf::from(&args[index + 1]);
        index += 2;
    }
    Ok((package, workspace))
}

fn resolve_workspace(requested: &Path) -> (Option<PathBuf>, Check) {
    let Some(name) = requested.file_name() else {
        return (
            None,
            Check::blocker(
                "workspace",
                "the workspace path must name a directory",
                "Pass --workspace with a directory name below an existing local directory.",
            ),
        );
    };
    let parent = requested
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match fs::canonicalize(parent) {
        Ok(parent) if parent.is_dir() => {
            let workspace = parent.join(name);
            (
                Some(workspace.clone()),
                Check::ok(
                    "workspace",
                    format!("location {} is available", workspace.display()),
                ),
            )
        }
        Ok(_) => (
            None,
            Check::blocker(
                "workspace",
                "the workspace parent is not a directory",
                "Choose a workspace below an existing writable local directory.",
            ),
        ),
        Err(error) => (
            None,
            Check::blocker(
                "workspace",
                format!("the workspace parent is unavailable: {error}"),
                "Create the parent directory or choose an existing writable local directory.",
            ),
        ),
    }
}

fn fallback_scratch_base(package: &Path) -> Result<PathBuf, String> {
    let parent = package
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent).map_err(|error| error.to_string())?;
    Ok(parent.join("assignment.work"))
}

fn filesystem_probe_location(
    workspace: Option<&Path>,
    package: &Path,
) -> (Option<PathBuf>, PathBuf, String) {
    if let Some(workspace) = workspace {
        if let Ok(pinned) = PinnedWorkspaceRoot::open(workspace) {
            let canonical = pinned.path().to_owned();
            return (
                Some(canonical.clone()),
                canonical.clone(),
                format!("workspace filesystem at {}", canonical.display()),
            );
        }
        let parent = workspace.parent().unwrap_or_else(|| Path::new("."));
        return (
            Some(workspace.to_owned()),
            parent.to_owned(),
            format!("workspace-parent filesystem at {}", parent.display()),
        );
    }
    match fallback_scratch_base(package) {
        Ok(base) => {
            let parent = base.parent().unwrap_or_else(|| Path::new(".")).to_owned();
            let scope = format!("workspace-parent filesystem at {}", parent.display());
            (Some(base), parent, scope)
        }
        Err(_) => (None, PathBuf::new(), "unavailable filesystem".to_owned()),
    }
}

fn writable_workspace_check(workspace: &Path) -> Check {
    match fs::metadata(workspace) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Check::blocker(
                    "workspace",
                    format!("{} is not a directory", workspace.display()),
                    "Choose a directory for --workspace, then rerun doctor.",
                );
            }
            if metadata.permissions().readonly() {
                return Check::blocker(
                    "workspace",
                    format!("{} is not writable", workspace.display()),
                    "Make the workspace directory writable, then rerun doctor.",
                );
            }
            let pinned = match PinnedWorkspaceRoot::open(workspace) {
                Ok(pinned) => pinned,
                Err(error) => {
                    return Check::blocker(
                        "workspace",
                        format!("{} has an unsafe binding: {error}", workspace.display()),
                        "Repair the workspace path or choose another local directory, then rerun doctor.",
                    );
                }
            };
            if let Err(error) = pinned.verify_binding() {
                return Check::blocker(
                    "workspace",
                    format!("{} changed during inspection: {error}", workspace.display()),
                    "Repair the workspace path or choose another local directory, then rerun doctor.",
                );
            }
            if let Err(error) = verify_workspace_writable(&pinned) {
                return Check::blocker(
                    "workspace",
                    format!("{} is not writable: {error}", workspace.display()),
                    "Make the workspace directory writable, then rerun doctor.",
                );
            }
            let parent = pinned.path().parent().unwrap_or_else(|| Path::new("."));
            match probe_directory_write(parent) {
                Ok(()) => Check::ok(
                    "workspace",
                    format!(
                        "{} has a stable writable directory binding and its filesystem accepts sibling scratch create, sync, and remove",
                        workspace.display()
                    ),
                ),
                Err(error) => Check::blocker(
                    "workspace",
                    format!(
                        "{} cannot be checked using sibling scratch space: {error}",
                        workspace.display()
                    ),
                    "Make the workspace parent writable or choose another local directory.",
                ),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if fs::symlink_metadata(workspace).is_ok() {
                return Check::blocker(
                    "workspace",
                    format!("{} does not resolve to a directory", workspace.display()),
                    "Repair the workspace link or choose another local directory, then rerun doctor.",
                );
            }
            let parent = workspace.parent().unwrap_or_else(|| Path::new("."));
            match probe_directory_write(parent) {
                Ok(()) => Check::ok(
                    "workspace",
                    format!("{} can be created", workspace.display()),
                ),
                Err(error) => Check::blocker(
                    "workspace",
                    format!("{} cannot be created: {error}", workspace.display()),
                    "Make the workspace parent writable or choose another local directory.",
                ),
            }
        }
        Err(error) => Check::blocker(
            "workspace",
            format!("cannot inspect {}: {error}", workspace.display()),
            "Make the workspace location accessible, then rerun doctor.",
        ),
    }
}

fn verify_workspace_writable(pinned: &PinnedWorkspaceRoot) -> io::Result<()> {
    verify_path_writable(pinned.path())?;
    let state = pinned.path().join(".rustrace");
    match fs::symlink_metadata(&state) {
        Ok(_) => verify_path_writable(&state)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    pinned.verify_binding().map_err(io::Error::other)
}

#[cfg(unix)]
fn verify_path_writable(path: &Path) -> io::Result<()> {
    let path_bytes = path.as_os_str().as_bytes();
    let path_c = CString::new(path_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a null byte"))?;
    if unsafe { libc::access(path_c.as_ptr(), libc::W_OK | libc::X_OK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "macos")]
    {
        let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::stat(path_c.as_ptr(), status.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let flags = unsafe { status.assume_init() }.st_flags;
        if flags & (libc::UF_IMMUTABLE | libc::UF_APPEND | libc::SF_IMMUTABLE | libc::SF_APPEND)
            != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "immutable or append-only filesystem flags are set",
            ));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_path_writable(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "writability checks are supported only on macOS and Linux",
    ))
}

fn starter_package_remedy(error: &(dyn Error + 'static), scratch: &Path) -> String {
    if matches!(
        error.downcast_ref::<AssignmentPackageError>(),
        Some(AssignmentPackageError::StarterPackageStructure { .. })
    ) {
        return "Ask the course staff for a repackaged assignment whose starter Cargo.toml declares an empty [workspace] table, then re-download.".to_owned();
    }
    if error.downcast_ref::<io::Error>().is_some()
        || matches!(
            error.downcast_ref::<AssignmentPackageError>(),
            Some(AssignmentPackageError::ArchiveIo { .. })
        )
    {
        return "Make assignment.rta readable and its directory writable, then rerun doctor."
            .to_owned();
    }
    let mut cause = error.source();
    while let Some(current) = cause {
        if current.downcast_ref::<io::Error>().is_some() {
            let parent = scratch.parent().unwrap_or_else(|| Path::new("."));
            return format!(
                "Make the workspace parent {} writable, then rerun doctor.",
                parent.display()
            );
        }
        cause = current.source();
    }
    "Re-download assignment.rta from the course source, then rerun doctor.".to_owned()
}

fn probe_directory_write(directory: &Path) -> io::Result<()> {
    let path = directory.join(format!(
        ".rustrace-doctor-write-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let result = (|| {
        file.write_all(b"rustrace doctor write check\n")?;
        file.sync_all()?;
        drop(file);
        fs::remove_file(&path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

fn session_ownership_check(workspace: &Path) -> Check {
    let state = workspace.join(".rustrace");
    match fs::symlink_metadata(&state) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Check::ok(
                "session ownership",
                "no existing session owns the workspace",
            );
        }
        Err(error) => {
            return Check::blocker(
                "session ownership",
                format!("cannot inspect existing session state safely: {error}"),
                "Make the existing state accessible or choose another workspace, then rerun doctor.",
            );
        }
        Ok(_) => {}
    }
    let result = (|| {
        let pinned = PinnedWorkspaceRoot::open(workspace)?;
        let mut owner = pinned
            .open_existing_state_directory()?
            .lock_for_inspection()?;
        owner.release_ownership()
    })();
    match result {
        Ok(()) => Check::ok(
            "session ownership",
            "the existing workspace is not owned by a live session",
        ),
        Err(error) => Check::blocker(
            "session ownership",
            format!("a live session may own the workspace, or its state is unsafe: {error}"),
            "Close the live Rustrace session or choose another workspace, then rerun doctor.",
        ),
    }
}

fn filesystem_checks(scratch: &Path, target: &Path, scope: &str) -> (Check, Check, Check) {
    let filesystem = filesystem_capability_check(scratch, target, scope);
    let budgets = SessionBudgets::default();
    match available_storage_bytes(target) {
        Ok(available) => {
            let disk = if available >= budgets.storage_bytes {
                Check::ok(
                    "disk space",
                    format!(
                        "{} MiB available on {scope}; {} MiB cumulative session budget required",
                        mebibytes(available),
                        mebibytes(budgets.storage_bytes)
                    ),
                )
            } else {
                Check::blocker(
                    "disk space",
                    format!(
                        "{} MiB available on {scope}; {} MiB cumulative session budget required",
                        mebibytes(available),
                        mebibytes(budgets.storage_bytes)
                    ),
                    "Free local disk space or choose another local workspace, then rerun doctor.",
                )
            };
            let usable = budgets.storage_bytes.saturating_sub(budgets.reserve_bytes);
            let cumulative = if available >= budgets.storage_bytes {
                Check::ok(
                    "cumulative storage headroom",
                    format!(
                        "{scope} has {} MiB usable before the hard cap with {} MiB reserved",
                        mebibytes(usable),
                        mebibytes(budgets.reserve_bytes)
                    ),
                )
            } else {
                Check::blocker(
                    "cumulative storage headroom",
                    format!("{scope} cannot fit the full session budget and recovery reserve"),
                    "Free enough local space for the 2048 MiB session budget, then rerun doctor.",
                )
            };
            (disk, filesystem, cumulative)
        }
        Err(error) => (
            Check::blocker(
                "disk space",
                format!("available storage on {scope} could not be measured safely: {error}"),
                "Use a supported local filesystem and rerun doctor.",
            ),
            filesystem,
            Check::blocker(
                "cumulative storage headroom",
                format!("headroom on {scope} is unavailable"),
                "Move the workspace to a supported local filesystem, then rerun doctor.",
            ),
        ),
    }
}

fn filesystem_capability_check(scratch: &Path, target: &Path, scope: &str) -> Check {
    match same_filesystem(scratch, target) {
        Ok(true) => {}
        Ok(false) => {
            return Check::warning(
                "filesystem capabilities",
                format!(
                    "{scope} has no safe sibling scratch location; atomic operations and host-name collision rejection are checked at session start, not by doctor"
                ),
                "Rerun doctor after choosing a workspace below a directory on the target filesystem.",
            );
        }
        Err(error) => {
            return Check::blocker(
                "filesystem capabilities",
                format!("cannot identify {scope} safely: {error}"),
                "Move the workspace to a supported local filesystem, then rerun doctor.",
            );
        }
    }
    let result = (|| {
        hash_workspace(scratch)?;
        let pinned = PinnedWorkspaceRoot::open(scratch)?;
        pinned.verify_binding()?;
        let first = WorkspacePath::new("rustrace-doctor-capability.tmp")?;
        let second = WorkspacePath::new("rustrace-doctor-capability-renamed.tmp")?;
        create_workspace_file_in(&pinned, &first)?;
        write_workspace_file_in(&pinned, &first, b"doctor capability probe\n")?;
        rename_workspace_file_in(&pinned, &first, &second)?;
        remove_workspace_file_in(&pinned, &second)?;
        let session_id = SessionId::new("doctor-probe")?;
        let mut owner = pinned
            .open_state_directory()?
            .create_journal_file(&session_id)?;
        owner.available_storage_bytes()?;
        owner.release_ownership()?;
        Ok::<(), Box<dyn std::error::Error>>(())
    })();
    match result {
        Ok(()) => Check::ok(
            "filesystem capabilities",
            format!(
                "safe extraction, pinned traversal, atomic save/rename/remove, exclusive creation, and cooperative locking passed on {scope}; host-name collision rejection is checked at session start, not by doctor"
            ),
        ),
        Err(error) => Check::blocker(
            "filesystem capabilities",
            format!("required checks on {scope} failed: {error}"),
            "Move the workspace to a supported local filesystem, then rerun doctor.",
        ),
    }
}

#[cfg(unix)]
fn same_filesystem(left: &Path, right: &Path) -> io::Result<bool> {
    Ok(fs::metadata(left)?.dev() == fs::metadata(right)?.dev())
}

#[cfg(not(unix))]
fn same_filesystem(_left: &Path, _right: &Path) -> io::Result<bool> {
    Ok(false)
}

#[cfg(unix)]
fn available_storage_bytes(path: &Path) -> io::Result<u64> {
    let directory = fs::File::open(path)?;
    let mut status = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::fstatvfs(directory.as_raw_fd(), status.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let status = unsafe { status.assume_init() };
    let available = u128::from(status.f_bavail).saturating_mul(u128::from(status.f_frsize));
    Ok(available.min(u128::from(u64::MAX)) as u64)
}

#[cfg(not(unix))]
fn available_storage_bytes(_path: &Path) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "storage checks are supported only on macOS and Linux",
    ))
}

fn tool_checks(report: Option<&ToolchainReport>, pin: Option<&str>) -> Vec<Check> {
    let pin = pin.unwrap_or("the assignment toolchain");
    let Some(report) = report else {
        return vec![
            dependent_tool_blocker("rustup"),
            dependent_tool_blocker("assignment toolchain"),
            dependent_tool_blocker("Cargo"),
            dependent_tool_warning(
                "rustfmt",
                "Run rustup component add rustfmt, then rerun doctor.",
            ),
            dependent_tool_warning(
                "Clippy",
                "Run rustup component add clippy, then rerun doctor.",
            ),
            dependent_tool_warning(
                "rust-analyzer",
                "Run rustup component add rust-analyzer, then rerun doctor.",
            ),
        ];
    };

    vec![
        required_component(
            report,
            "rustup",
            "rustup",
            "Install or repair rustup from https://rustup.rs, then rerun doctor.",
        ),
        assignment_toolchain_check(report, pin),
        required_component(
            report,
            "cargo",
            "Cargo",
            format!("Repair the installed {pin} toolchain, then rerun doctor."),
        ),
        optional_component(
            report,
            "rustfmt",
            "rustfmt",
            "Run rustup component add rustfmt, then rerun doctor.",
        ),
        optional_component(
            report,
            "clippy",
            "Clippy",
            "Run rustup component add clippy, then rerun doctor.",
        ),
        optional_component(
            report,
            "rust-analyzer",
            "rust-analyzer",
            "Run rustup component add rust-analyzer, then rerun doctor.",
        ),
    ]
}

fn assignment_toolchain_check(report: &ToolchainReport, pin: &str) -> Check {
    let available = component_available(report, "toolchain")
        && component_available(report, "rustc")
        && report.selected_toolchain.is_some();
    if available {
        Check::ok(
            "assignment toolchain",
            format!(
                "{} is installed and selected without auto-install",
                report.selected_toolchain.as_deref().unwrap_or(pin)
            ),
        )
    } else {
        Check::blocker(
            "assignment toolchain",
            component_failure(report, "toolchain"),
            format!("Run rustup toolchain install {pin}, then rerun doctor."),
        )
    }
}

fn required_component(
    report: &ToolchainReport,
    component: &str,
    name: &'static str,
    remedy: impl Into<String>,
) -> Check {
    if component_available(report, component) {
        Check::ok(name, component_version(report, component))
    } else {
        Check::blocker(name, component_failure(report, component), remedy)
    }
}

fn optional_component(
    report: &ToolchainReport,
    component: &str,
    name: &'static str,
    remedy: impl Into<String>,
) -> Check {
    if component_available(report, component) {
        Check::ok(name, component_version(report, component))
    } else {
        Check::warning(name, component_failure(report, component), remedy)
    }
}

fn component_available(report: &ToolchainReport, component: &str) -> bool {
    let mut probes = report
        .probes
        .iter()
        .filter(|probe| probe.component == component);
    let Some(first) = probes.next() else {
        return false;
    };
    first.status == ProbeStatus::Available
        && probes.all(|probe| probe.status == ProbeStatus::Available)
}

fn component_version(report: &ToolchainReport, component: &str) -> String {
    report
        .version(component)
        .map(str::to_owned)
        .unwrap_or_else(|| "installed and available".to_owned())
}

/// A failed probe as a plain phrase for the student, never an internal
/// status name. The probe's first stderr line, when present, says why.
fn component_failure(report: &ToolchainReport, component: &str) -> String {
    let Some(probe) = report
        .probes
        .iter()
        .find(|probe| probe.component == component && probe.status != ProbeStatus::Available)
    else {
        return "not checked because a required earlier probe failed".to_owned();
    };
    let outcome = match probe.status {
        ProbeStatus::Available => "succeeded",
        ProbeStatus::NotFound => "found no executable",
        ProbeStatus::Failed => "failed",
        ProbeStatus::TimedOut => "timed out",
        ProbeStatus::InvalidOutput => "returned unusable output",
    };
    let reason = probe
        .stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| display::label(line, 512))
        .filter(|line| !line.is_empty())
        .map(|line| format!(" ({line})"))
        .unwrap_or_default();
    format!("{} probe {outcome}{reason}", probe.purpose)
}

fn dependent_tool_blocker(name: &'static str) -> Check {
    Check::blocker(
        name,
        "not checked because the assignment manifest is unavailable",
        "Fix the starter package, then rerun doctor.",
    )
}

fn dependent_tool_warning(name: &'static str, remedy: &'static str) -> Check {
    Check::warning(
        name,
        "not checked because the assignment manifest is unavailable",
        remedy,
    )
}

fn terminal_check() -> Check {
    if !io::stdout().is_terminal() {
        return Check::ok("terminal", "not checked (no TTY attached)");
    }
    let (columns, rows) = match crossterm::terminal::size() {
        Ok(size) => size,
        Err(error) => {
            return Check::blocker(
                "terminal",
                format!("terminal size is unavailable: {error}"),
                "Use a supported interactive terminal and rerun doctor.",
            );
        }
    };
    if columns < MINIMUM_COLUMNS || rows < MINIMUM_ROWS {
        return Check::blocker(
            "terminal",
            format!(
                "{columns}x{rows} attached; at least {MINIMUM_COLUMNS}x{MINIMUM_ROWS} required"
            ),
            "Resize the terminal to at least 80 columns by 24 rows, then rerun doctor.",
        );
    }
    let term = std::env::var("TERM").unwrap_or_default();
    if term == "dumb" {
        return Check::blocker(
            "terminal",
            format!(
                "{columns}x{rows} attached, but TERM=dumb has no required color or paste support"
            ),
            "Use a supported ANSI terminal with bracketed paste, then rerun doctor.",
        );
    }
    if recognizable_terminal(&term) || std::env::var_os("COLORTERM").is_some() {
        Check::ok(
            "terminal",
            format!(
                "{columns}x{rows}; color and bracketed-paste support detected from TERM={term}"
            ),
        )
    } else {
        Check::warning(
            "terminal",
            format!("{columns}x{rows}; color and bracketed-paste support could not be detected"),
            "Use Terminal.app, iTerm2, a mainstream Linux terminal, or Windows Terminal with WSL 2.",
        )
    }
}

fn recognizable_terminal(term: &str) -> bool {
    let term = term.to_ascii_lowercase();
    [
        "ansi", "color", "konsole", "linux", "screen", "tmux", "vt100", "xterm",
    ]
    .iter()
    .any(|name| term.contains(name))
}

fn write_report(
    checks: &[Check],
    theme: &EffectiveTheme,
    ghostty: Option<&crate::ghostty::GhosttyInspection>,
    output: &mut impl Write,
) -> io::Result<()> {
    writeln!(output, "rustrace {}", env!("CARGO_PKG_VERSION"))?;
    for check in checks {
        writeln!(
            output,
            "{} {}: {}",
            check.level.label(),
            check.name,
            display::label(&check.detail, 2048)
        )?;
        if let Some(remedy) = &check.remedy {
            writeln!(output, "  Remedy: {}", display::label(remedy, 2048))?;
        }
    }
    for field in PALETTE_FIELD_NAMES {
        let color = theme
            .palette
            .color(field)
            .expect("palette field inventory matches Palette");
        let source = match theme
            .source(field)
            .expect("palette field inventory has a source")
        {
            ThemeColorSource::BuiltIn(name) => format!("{name} built-in"),
            ThemeColorSource::Custom => "custom".to_owned(),
        };
        writeln!(
            output,
            "  {field} = {} (source: {source})",
            format_color(color)
        )?;
    }
    if let Some(inspection) = ghostty {
        write_ghostty_report(inspection, output)?;
    }
    let ok = checks
        .iter()
        .filter(|check| check.level == Level::Ok)
        .count();
    let warnings = checks
        .iter()
        .filter(|check| check.level == Level::Warning)
        .count();
    let blockers = checks
        .iter()
        .filter(|check| check.level == Level::Blocker)
        .count();
    writeln!(
        output,
        "Doctor summary: {ok} OK, {warnings} WARNING, {blockers} BLOCKER."
    )
}

fn write_ghostty_report(
    inspection: &crate::ghostty::GhosttyInspection,
    output: &mut impl Write,
) -> io::Result<()> {
    writeln!(output, "Ghostty key setup:")?;
    if !inspection.cli_available {
        writeln!(
            output,
            "  Ghostty CLI unavailable; assuming Ghostty defaults"
        )?;
    }
    for (index, key) in crate::ghostty::SETUP_KEYS.iter().enumerate() {
        let status = inspection
            .bindings
            .status(key)
            .expect("setup key has a status");
        match inspection.actions[index].as_deref() {
            Some(action) if status != crate::ghostty::GhosttyBindingStatus::Passed => writeln!(
                output,
                "  Ghostty key {key}: {} ({})",
                status.label(),
                display::label(action, 512)
            )?,
            _ => writeln!(output, "  Ghostty key {key}: {}", status.label())?,
        }
    }
    if crate::ghostty::SETUP_KEYS.iter().any(|key| {
        inspection.bindings.status(key) != Some(crate::ghostty::GhosttyBindingStatus::Passed)
    }) {
        writeln!(
            output,
            "  Run `rustrace doctor --write-ghostty-keys` to write the recommended unbind snippet."
        )?;
    }
    Ok(())
}

const fn mebibytes(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}
