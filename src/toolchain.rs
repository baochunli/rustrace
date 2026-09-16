//! Bounded observations of installed tools, never installation or build commands.
use crate::display;
use crate::{CommandExecution, SystemCommandRunner};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolchainReport {
    pub assignment_pin: Option<String>,
    pub selected_toolchain: Option<String>,
    pub working_directory: PathBuf,
    pub probes: Vec<ToolProbe>,
}

/// Immutable session runtime metadata; no historical tool versions are implied.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeToolchainMetadata {
    pub version: u32,
    pub session_id: rustrace_model::SessionId,
    pub manifest_hash: rustrace_model::Hash,
    pub sequence: u64,
    pub event_hash: rustrace_model::Hash,
    pub report: ToolchainReport,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolProbe {
    pub component: String,
    pub purpose: String,
    pub required: bool,
    pub argv: Vec<String>,
    pub status: ProbeStatus,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timeout_ms: Option<u64>,
    pub output_limited: bool,
    pub detail: String,
    pub remediation: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    Available,
    NotFound,
    Failed,
    TimedOut,
    InvalidOutput,
}

impl ToolchainReport {
    pub fn has_blockers(&self) -> bool {
        self.probes
            .iter()
            .any(|probe| probe.required && probe.status != ProbeStatus::Available)
    }

    pub fn version(&self, component: &str) -> Option<&str> {
        self.probes
            .iter()
            .find(|probe| {
                probe.component == component
                    && probe.purpose == "version"
                    && probe.status == ProbeStatus::Available
            })
            .map(|probe| probe.stdout.trim())
    }

    /// Safe, compact display only. Exact bounded probe bytes remain in metadata.
    pub fn summary(&self) -> String {
        let mut summary = format!(
            "Tools: {}",
            display::label(
                self.selected_toolchain.as_deref().unwrap_or("unavailable"),
                240
            )
        );
        for component in ["rustc", "cargo"] {
            if let Some(version) = self.version(component) {
                let bounded = display::label(version, 320);
                let short = bounded
                    .split_whitespace()
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ");
                summary.push_str(&format!(" | {short}"));
            }
        }
        display_text(&summary, 240)
    }

    pub fn write_diagnostics(&self, output: &mut impl Write) -> std::io::Result<()> {
        writeln!(
            output,
            "Toolchain: {} (observed at this startup)",
            display_text(
                self.selected_toolchain.as_deref().unwrap_or("unavailable"),
                160
            )
        )?;
        for probe in &self.probes {
            if probe.status == ProbeStatus::Available && probe.purpose == "version" {
                writeln!(
                    output,
                    "[ok] {}: {}",
                    display::label(&probe.component, 160),
                    display_text(&probe.stdout, 320)
                )?;
            } else if probe.status != ProbeStatus::Available {
                writeln!(
                    output,
                    "[{}] {} {}: {:?}; {}",
                    if probe.required { "error" } else { "warning" },
                    display::label(&probe.component, 160),
                    display::label(&probe.purpose, 160),
                    probe.status,
                    display_text(&probe.detail, 160)
                )?;
                if !probe.stderr.is_empty() {
                    writeln!(output, "  stderr: {}", display_text(&probe.stderr, 160))?;
                }
                writeln!(output, "  {}", display_text(&probe.remediation, 320))?;
            }
        }
        Ok(())
    }
}

pub fn discover(root: &Path, pin: Option<&str>) -> ToolchainReport {
    discover_using(
        root,
        pin,
        &find_rustup(root),
        &SystemCommandRunner::default(),
    )
}

pub(crate) fn find_rustup(root: &Path) -> PathBuf {
    // Retain the exact launcher selected from PATH, including relative PATH
    // entries interpreted in the workspace where the probe runs.
    let rustup = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| root.join(directory).join("rustup"))
            .find(|candidate| {
                candidate.metadata().is_ok_and(|metadata| {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    }
                    #[cfg(not(unix))]
                    metadata.is_file()
                })
            })
    });
    rustup.unwrap_or_else(|| PathBuf::from("rustup"))
}

fn discover_using(
    root: &Path,
    pin: Option<&str>,
    rustup: &Path,
    runner: &SystemCommandRunner,
) -> ToolchainReport {
    discover_with_executor(
        root,
        pin,
        rustup,
        None,
        runner.capture_limit(),
        &|mut command| runner.run_command(&mut command),
    )
}

/// Same safe bootstrap and installed-selection validation as startup, with an
/// owned bounded executor and only the components needed by this command.
pub(crate) fn discover_with_executor(
    root: &Path,
    pin: Option<&str>,
    rustup: &Path,
    action: Option<crate::cargo_policy::CargoAction>,
    capture_limit: usize,
    runner: &dyn Fn(Command) -> CommandExecution,
) -> ToolchainReport {
    discover_for_purpose(
        root,
        pin,
        rustup,
        action.map_or(DiscoveryPurpose::Startup, DiscoveryPurpose::Command),
        capture_limit,
        runner,
    )
}

/// Fresh installed-only resolution for the long-lived optional language server.
/// This deliberately shares discovery's safe bootstrap and exact executable
/// rules without treating the server as a Cargo command or resolving unrelated
/// optional actions.
pub(crate) fn discover_language_server_with_executor(
    root: &Path,
    pin: Option<&str>,
    rustup: &Path,
    capture_limit: usize,
    runner: &dyn Fn(Command) -> CommandExecution,
) -> ToolchainReport {
    discover_for_purpose(
        root,
        pin,
        rustup,
        DiscoveryPurpose::LanguageServer,
        capture_limit,
        runner,
    )
}

#[derive(Clone, Copy)]
enum DiscoveryPurpose {
    Startup,
    Command(crate::cargo_policy::CargoAction),
    LanguageServer,
}

fn discover_for_purpose(
    root: &Path,
    pin: Option<&str>,
    rustup: &Path,
    purpose: DiscoveryPurpose,
    capture_limit: usize,
    runner: &dyn Fn(Command) -> CommandExecution,
) -> ToolchainReport {
    let command_versions = !matches!(purpose, DiscoveryPurpose::Startup);
    let mut discovery = Discovery {
        rustup,
        runner,
        capture_limit,
        command_versions,
        report: ToolchainReport {
            assignment_pin: pin.map(str::to_owned),
            selected_toolchain: None,
            working_directory: root.to_path_buf(),
            probes: Vec::new(),
        },
    };
    if pin.is_some_and(|pin| !valid_name(pin)) || rustup.to_str().is_none() {
        discovery.report.probes.push(ToolProbe {
            component: "toolchain".into(), purpose: "selection".into(), required: true,
            argv: Vec::new(), status: ProbeStatus::InvalidOutput,
            stdout: String::new(), stderr: String::new(),
            detail: "unsupported toolchain name or non-UTF-8 launcher path".into(),
            exit_code: None, timeout_ms: None, output_limited: false,
            remediation: "Use a named installed rustup toolchain (letters, digits, '.', '_' or '-'); ask the assignment author to correct an invalid pin.".into(),
        });
        return discovery.report;
    }
    let Some(version) = discovery.run("rustup", "version", true, &["--version"]) else {
        return discovery.report;
    };
    if !supported_manager(&version) {
        discovery.invalid(
            "unsupported rustup release; normal discovery requires rustup 1.28.1 or newer",
        );
        discovery.report.probes.last_mut().expect("version probe").remediation =
            "Update rustup manually to release 1.28.1 or newer (rustup self update, or your package manager), then resume. Rustrace does not update or install tools.".into();
        return discovery.report;
    }
    let Some(installed) = discovery.run("toolchain", "installed", true, &["toolchain", "list"])
    else {
        return discovery.report;
    };
    let Some(active) = discovery.run(
        "toolchain",
        "selection",
        true,
        &["show", "active-toolchain"],
    ) else {
        return discovery.report;
    };
    let selected = active.split_whitespace().next().unwrap_or_default();
    if !valid_name(selected)
        || active.trim().lines().count() != 1
        || !installed
            .lines()
            .any(|line| line.split_whitespace().next() == Some(selected))
        || pin.is_some_and(|pin| selected != pin && !selected.starts_with(&format!("{pin}-")))
    {
        discovery.invalid("active toolchain is not an installed match for the requested selection; no fallback was executed");
        return discovery.report;
    }
    discovery.report.selected_toolchain = Some(selected.to_owned());
    let mut components = vec![
        ("rustc", "rustc", true, &["-vV"][..]),
        ("cargo", "cargo", true, &["-V"][..]),
        ("rust-analyzer", "rust-analyzer", false, &["--version"][..]),
        ("rustfmt", "rustfmt", false, &["--version"][..]),
        // Invoke the resolved Clippy driver with Cargo's subcommand argument.
        // `cargo clippy -V` can instead find an unrelated cargo-home/PATH driver.
        ("clippy", "cargo-clippy", false, &["clippy", "-V"][..]),
    ];
    match purpose {
        DiscoveryPurpose::Startup => {}
        DiscoveryPurpose::Command(action) => {
            components.retain(|(component, _, _, _)| {
                matches!(*component, "rustc" | "cargo")
                    || (*component == "clippy"
                        && action == crate::cargo_policy::CargoAction::Clippy)
                    || (*component == "rustfmt"
                        && action == crate::cargo_policy::CargoAction::Format)
            });
            components.push(("rustdoc", "rustdoc", true, &["--version"]));
            if action == crate::cargo_policy::CargoAction::Format {
                components.push(("cargo-fmt", "cargo-fmt", false, &["fmt", "--version"]));
            }
        }
        DiscoveryPurpose::LanguageServer => {
            components.retain(|(component, _, _, _)| {
                matches!(*component, "rustc" | "cargo" | "rust-analyzer")
            });
            components.push(("rustdoc", "rustdoc", true, &["--version"]));
        }
    }
    for (component, executable, required, args) in components {
        let Some(path) = discovery.run(
            component,
            "resolve",
            required,
            &["which", "--toolchain", selected, executable],
        ) else {
            continue;
        };
        let path = path.trim();
        if !Path::new(path).is_absolute() || path.chars().any(char::is_control) {
            discovery.invalid("rustup did not return one absolute executable path");
            continue;
        }
        let mut invocation = vec!["run", selected, path];
        invocation.extend_from_slice(args);
        if let Some(version) = discovery.run(component, "version", required, &invocation) {
            if !recognizable_version(
                &version,
                if component == "cargo-fmt" {
                    "rustfmt"
                } else {
                    component
                },
            ) {
                discovery.invalid("executable did not return recognizable version output");
            } else if component == "rustc" {
                let release = version
                    .lines()
                    .find_map(|line| line.strip_prefix("release: "));
                let host = version.lines().find_map(|line| line.strip_prefix("host: "));
                if release.is_none_or(str::is_empty)
                    || host.is_none_or(str::is_empty)
                    || pin.is_some_and(|pin| {
                        pin.bytes()
                            .all(|byte| byte.is_ascii_digit() || byte == b'.')
                            && release.is_some_and(|release| {
                                release != pin && !release.starts_with(&format!("{pin}."))
                            })
                    })
                {
                    discovery.invalid("rustc verbose version is incomplete or does not match the assignment version pin");
                }
            }
        }
    }
    discovery.report
}

struct Discovery<'a> {
    rustup: &'a Path,
    runner: &'a dyn Fn(Command) -> CommandExecution,
    capture_limit: usize,
    command_versions: bool,
    report: ToolchainReport,
}

impl Discovery<'_> {
    fn run(
        &mut self,
        component: &str,
        purpose: &str,
        required: bool,
        args: &[&str],
    ) -> Option<String> {
        let mut command = Command::new(self.rustup);
        crate::cargo_policy::retain_execution_environment(&mut command);
        // Rustup tracing logs configuration URLs, which can contain credentials.
        // Keep ordinary diagnostics without inheriting that logging channel.
        command
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env_remove("RUSTUP_LOG")
            .args(args)
            .current_dir(&self.report.working_directory);
        if component == "rustup" && purpose == "version" {
            // Legacy rustup's version handler can install its active official
            // toolchain, ignoring AUTO_INSTALL=0. A custom name takes its local
            // lookup-only branch and overrides inherited env/directory/default
            // selection. Never expose the assignment pin before version gating.
            command.env("RUSTUP_TOOLCHAIN", "rustrace-discovery-bootstrap");
        } else if let Some(selection) = self
            .report
            .selected_toolchain
            .as_deref()
            .or(self.report.assignment_pin.as_deref())
        {
            command.env("RUSTUP_TOOLCHAIN", selection);
        } else if let Some(selection) = std::env::var_os("RUSTUP_TOOLCHAIN") {
            // Unpinned discovery alone asks rustup to observe actual selection.
            command.env("RUSTUP_TOOLCHAIN", selection);
        }
        let (mut status, stdout, stderr, exit_code, timeout_ms) = match (self.runner)(command) {
            CommandExecution::NotFound => (
                ProbeStatus::NotFound,
                String::new(),
                "executable not found".into(),
                None,
                None,
            ),
            CommandExecution::Succeeded { stdout, stderr } => {
                (ProbeStatus::Available, stdout, stderr, Some(0), None)
            }
            CommandExecution::Failed {
                exit_code,
                stdout,
                stderr,
            } => (ProbeStatus::Failed, stdout, stderr, exit_code, None),
            CommandExecution::TimedOut {
                timeout,
                stdout,
                stderr,
            } => (
                ProbeStatus::TimedOut,
                stdout,
                stderr,
                None,
                Some(timeout.as_millis().min(u128::from(u64::MAX)) as u64),
            ),
        };
        let output_limited =
            stdout.len() >= self.capture_limit || stderr.len() >= self.capture_limit;
        let version_oversize = self.command_versions
            && purpose == "version"
            && stdout.trim().len() > rustrace_model::MAX_STRING_BYTES;
        if status == ProbeStatus::Available
            && (stdout.trim().is_empty()
                || output_limited
                || version_oversize
                || stdout.contains('\u{fffd}')
                || stderr.contains('\u{fffd}'))
        {
            status = ProbeStatus::InvalidOutput;
        }
        let value = (status == ProbeStatus::Available).then(|| stdout.clone());
        let remediation = if status == ProbeStatus::Available {
            String::new()
        } else {
            self.remediation(component)
        };
        self.report.probes.push(ToolProbe {
            component: component.into(),
            purpose: purpose.into(),
            required,
            argv: std::iter::once(self.rustup.to_string_lossy().into_owned())
                .chain(args.iter().map(|arg| (*arg).to_owned()))
                .collect(),
            status,
            stdout,
            stderr,
            exit_code,
            timeout_ms,
            output_limited,
            remediation,
            detail: if version_oversize {
                "version output exceeds the retained command-evidence bound".into()
            } else if status == ProbeStatus::InvalidOutput {
                "empty, capture-limit or invalid UTF-8 output; no complete result established"
                    .into()
            } else if component == "rustup" && purpose == "version" {
                "bootstrap override: RUSTUP_TOOLCHAIN=rustrace-discovery-bootstrap (custom, non-distributable)".into()
            } else {
                String::new()
            },
        });
        value
    }

    fn invalid(&mut self, reason: &str) {
        let component = self
            .report
            .probes
            .last()
            .expect("completed probe")
            .component
            .clone();
        let remediation = self.remediation(&component);
        let probe = self.report.probes.last_mut().expect("completed probe");
        probe.status = ProbeStatus::InvalidOutput;
        probe.detail = reason.into();
        probe.remediation = remediation;
    }

    fn remediation(&self, component: &str) -> String {
        let selection = self
            .report
            .selected_toolchain
            .as_deref()
            .or(self.report.assignment_pin.as_deref())
            .unwrap_or("stable");
        match component {
            "rustup" => "Install or repair rustup from https://rustup.rs, then restart the terminal and resume this preserved assignment.".into(),
            "rust-analyzer" => format!("completion unavailable; editing and recording remain available. To enable language services, run rustup component add --toolchain {selection} rust-analyzer yourself."),
            "rustfmt" => format!("Format unavailable. To enable it, run rustup component add --toolchain {selection} rustfmt yourself."),
            "clippy" => format!("Clippy unavailable. To enable it, run rustup component add --toolchain {selection} clippy yourself."),
            _ => format!("Install or repair the requested toolchain with rustup toolchain install {selection}, or correct the rustup override for unpinned work, then resume. Rustrace installs nothing."),
        }
    }
}

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

fn recognizable_version(output: &str, component: &str) -> bool {
    output.starts_with(&format!("{component} "))
        && output.split_whitespace().nth(1).is_some_and(|version| {
            version.starts_with(|character: char| character.is_ascii_digit())
        })
}

fn supported_manager(output: &str) -> bool {
    let Some(version) = output
        .strip_prefix("rustup ")
        .and_then(|rest| rest.split_whitespace().next())
    else {
        return false;
    };
    let mut parts = version.split('.').map(|part| {
        (!part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| part.parse::<u64>().ok())
            .flatten()
    });
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None)
            if (major, minor, patch) >= (1, 28, 1)
    )
}

fn display_text(text: &str, limit: usize) -> String {
    display::label(text, limit)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, Instant},
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "rustrace-tools-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::write(root.join("active"), "stable-test-host\n").unwrap();
            fs::write(
                root.join("installed"),
                "pinned\nstable-test-host (default)\n",
            )
            .unwrap();
            let fixture = Self(root);
            fixture.script("rustup", RUSTUP);
            fixture.script(
                "rustc",
                "printf 'rustc 1.98.1 (actual)\\nrelease: 1.98.1\\nhost: test-host\\n'",
            );
            fixture.script("cargo", "printf 'cargo 1.97.2 (actual cargo)\\n'");
            fixture.script(
                "rust-analyzer",
                "printf 'rust-analyzer 0.0.0 (actual server)\\n'",
            );
            fixture.script("rustfmt", "printf 'rustfmt 1.8.0 (actual formatter)\\n'");
            fixture.script("cargo-clippy", "printf 'clippy 0.1.98 (actual clippy)\\n'");
            fixture
        }
        fn script(&self, name: &str, body: &str) {
            let file = self.0.join(name);
            fs::write(&file, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fn probe(&self, pin: Option<&str>) -> ToolchainReport {
            discover_using(
                &self.0,
                pin,
                &self.0.join("rustup"),
                &SystemCommandRunner::with_limits(Duration::from_secs(2), 1024),
            )
        }
        fn calls(&self) -> String {
            fs::read_to_string(self.0.join("calls")).unwrap_or_default()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const RUSTUP: &str = r#"
root=${0%/*}
printf '%s|%s|' "$RUSTUP_AUTO_INSTALL" "$RUSTUP_TOOLCHAIN" >> "$root/calls"
printf '<%s>' "$@" >> "$root/calls"
printf '\n' >> "$root/calls"
[ "$RUSTUP_AUTO_INSTALL" = 0 ] || exit 91
case "$1 $2" in
  '--version ') printf 'rustup 1.28.2 (actual manager)\n' ;;
  'toolchain list') while IFS= read -r line; do printf '%s\n' "$line"; done < "$root/installed" ;;
  'show active-toolchain')
    if [ "$RUSTUP_TOOLCHAIN" = pinned ]; then printf 'pinned (environment override)\n'
    else IFS= read -r active < "$root/active"; printf '%s (default)\n' "$active"; fi ;;
  'which --toolchain')
    [ -f "$root/$4" ] || { printf 'component unavailable\n' >&2; exit 1; }
    printf '%s/%s\n' "$root" "$4" ;;
  run\ *) selected=$2; shift 2; export RUSTUP_TOOLCHAIN=$selected; exec "$@" ;;
  *) printf 'forbidden command\n' >&2; exit 90 ;;
esac
"#;

    #[test]
    fn discovery_child_drops_compiler_and_cargo_overrides() {
        const CHILD: &str = "RUSTRACE_T43_DISCOVERY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let fixture = Fixture::new();
            fixture.script("rustup", &format!(
                "[ -z \"${{RUSTC+x}}${{RUSTFLAGS+x}}${{CARGO_BUILD_RUSTC+x}}${{CARGO_REGISTRY_TOKEN+x}}${{RUSTUP_LOG+x}}\" ] || exit 89\n{RUSTUP}"
            ));
            assert!(!fixture.probe(Some("pinned")).has_blockers());
            return;
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "toolchain::tests::discovery_child_drops_compiler_and_cargo_overrides",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("RUSTC", "synthetic-compiler")
            .env("RUSTFLAGS", "synthetic-flags")
            .env("CARGO_BUILD_RUSTC", "synthetic-override")
            .env("CARGO_REGISTRY_TOKEN", "synthetic-secret")
            .env("RUSTUP_LOG", "trace")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn only_supported_manager_releases_reach_normal_discovery() {
        for (version, supported) in [
            ("1.27.1", false),
            ("1.28.0", false),
            ("1.28.1-beta.1", false),
            ("1.28.1", true),
            ("1.28.2", true),
            ("1.29.1", true),
            ("2.0.0", true),
        ] {
            let fixture = Fixture::new();
            fixture.script("rustup", &RUSTUP.replace("1.28.2", version));
            let report = fixture.probe(Some("pinned"));
            assert_eq!(!report.has_blockers(), supported, "{version}: {report:#?}");
            assert_eq!(
                fixture.calls().contains("<toolchain><list>"),
                supported,
                "{version}: normal discovery must require a supported manager"
            );
        }
    }

    #[test]
    fn pin_selects_exact_installed_executables_and_records_actual_versions() {
        let fixture = Fixture::new();
        let report = fixture.probe(Some("pinned"));
        assert_eq!(
            report.selected_toolchain.as_deref(),
            Some("pinned"),
            "{report:#?}"
        );
        assert!(!report.has_blockers(), "{report:#?}");
        assert_eq!(report.version("cargo"), Some("cargo 1.97.2 (actual cargo)"));
        assert_eq!(
            report.version("clippy"),
            Some("clippy 0.1.98 (actual clippy)")
        );
        assert!(report.version("rustc").unwrap().contains("release: 1.98.1"));
        for (component, executable, args) in [
            ("rustc", "rustc", vec!["-vV"]),
            ("cargo", "cargo", vec!["-V"]),
            ("rust-analyzer", "rust-analyzer", vec!["--version"]),
            ("rustfmt", "rustfmt", vec!["--version"]),
            ("clippy", "cargo-clippy", vec!["clippy", "-V"]),
        ] {
            let probe = report
                .probes
                .iter()
                .find(|p| p.component == component && p.purpose == "version")
                .unwrap();
            let mut expected = vec![
                fixture.0.join("rustup").display().to_string(),
                "run".into(),
                "pinned".into(),
                fixture.0.join(executable).display().to_string(),
            ];
            expected.extend(args.into_iter().map(str::to_owned));
            assert_eq!(probe.argv, expected);
        }
        assert!(
            fixture
                .calls()
                .contains("0|pinned|<show><active-toolchain>")
        );
        assert!(!fixture.calls().contains("--install"));
    }

    #[test]
    fn no_pin_uses_rustups_actual_active_selection_not_first_installed() {
        let fixture = Fixture::new();
        let report = fixture.probe(None);
        assert_eq!(
            report.selected_toolchain.as_deref(),
            Some("stable-test-host")
        );
        assert!(!report.has_blockers(), "{report:#?}");
        fs::write(fixture.0.join("active"), "pinned\n").unwrap();
        let overridden = fixture.probe(None);
        assert_eq!(overridden.selected_toolchain.as_deref(), Some("pinned"));
        assert!(fixture.calls().contains("<run><pinned>"));
    }

    #[test]
    fn missing_manager_and_uninstalled_pin_block_without_tool_execution() {
        let fixture = Fixture::new();
        fs::remove_file(fixture.0.join("rustup")).unwrap();
        let missing = fixture.probe(Some("pinned"));
        assert!(missing.has_blockers());
        assert_eq!(missing.probes[0].status, ProbeStatus::NotFound);
        assert!(missing.probes[0].remediation.contains("rustup.rs"));
        fixture.script("rustup", RUSTUP);
        fs::write(fixture.0.join("installed"), "stable-test-host (default)\n").unwrap();
        let absent = fixture.probe(Some("pinned"));
        assert!(absent.has_blockers());
        assert!(!fixture.calls().contains("<run>"));
        assert!(
            absent
                .probes
                .iter()
                .any(|p| p.remediation.contains("toolchain install"))
        );
    }

    #[test]
    fn launchers_must_answer_successfully_and_optional_failure_only_degrades() {
        let fixture = Fixture::new();
        fixture.script("rust-analyzer", "printf 'server missing\\n' >&2; exit 42");
        fs::remove_file(fixture.0.join("rustfmt")).unwrap();
        fs::remove_file(fixture.0.join("cargo-clippy")).unwrap();
        let report = fixture.probe(Some("pinned"));
        assert!(!report.has_blockers());
        assert!(report.version("rustc").is_some());
        assert!(report.version("rust-analyzer").is_none());
        assert_eq!(
            report
                .probes
                .iter()
                .find(|p| p.component == "rust-analyzer" && p.purpose == "version")
                .unwrap()
                .exit_code,
            Some(42)
        );
        assert_eq!(
            report
                .probes
                .iter()
                .filter(|p| !p.required && p.status != ProbeStatus::Available)
                .count(),
            3
        );
        fixture.script("cargo", "printf 'cargo unusable\\n' >&2; exit 7");
        let failed = fixture.probe(Some("pinned"));
        assert!(failed.has_blockers());
        assert!(failed.version("cargo").is_none());
    }

    #[test]
    fn timeout_and_incomplete_output_are_never_reported_as_versions() {
        let fixture = Fixture::new();
        fixture.script("rustc", "printf 'rustc partial'; while :; do :; done");
        let began = Instant::now();
        let timeout = fixture.probe(Some("pinned"));
        assert!(began.elapsed() < Duration::from_secs(6));
        assert!(timeout.has_blockers());
        assert!(
            timeout
                .probes
                .iter()
                .any(|p| p.status == ProbeStatus::TimedOut)
        );
        fixture.script(
            "rustc",
            "printf 'rustc '; i=0; while [ \"$i\" -lt 3000 ]; do printf x; i=$((i+1)); done",
        );
        let bounded = fixture.probe(Some("pinned"));
        assert!(bounded.has_blockers());
        assert!(bounded.version("rustc").is_none());
        assert!(bounded.probes.iter().all(|p| p.stdout.len() <= 1024));
        fixture.script("rustc", "exit 0");
        assert!(fixture.probe(Some("pinned")).has_blockers());
    }

    #[test]
    fn invalid_pin_and_wrong_selection_never_fall_back_or_execute_shell_text() {
        let fixture = Fixture::new();
        let invalid = fixture.probe(Some("$(touch injected)"));
        assert!(invalid.has_blockers());
        assert!(!fixture.0.join("injected").exists());
        let mismatched = fixture.probe(Some("missing"));
        assert!(mismatched.has_blockers());
        assert!(!fixture.calls().contains("<run>"));
    }

    #[test]
    fn exact_version_pin_mismatch_is_visible_and_cannot_claim_the_requested_version() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("active"), "1.98.1\n").unwrap();
        fs::write(fixture.0.join("installed"), "1.98.1\n").unwrap();
        let matching = fixture.probe(Some("1.98.1"));
        assert!(!matching.has_blockers(), "{matching:#?}");
        fixture.script(
            "rustc",
            "printf 'rustc 1.99.0 (unexpected)\\nrelease: 1.99.0\\nhost: test-host\\n'",
        );
        let mismatched = fixture.probe(Some("1.98.1"));
        assert!(mismatched.has_blockers());
        assert!(mismatched.version("rustc").is_none());
        let probe = mismatched
            .probes
            .iter()
            .find(|p| p.component == "rustc" && p.purpose == "version")
            .unwrap();
        assert_eq!(probe.status, ProbeStatus::InvalidOutput);
        assert!(probe.stdout.contains("1.99.0"));
        assert!(
            probe.stderr.is_empty(),
            "validation messages are not fabricated child stderr"
        );
        assert!(probe.detail.contains("does not match"));
    }

    #[test]
    fn incomplete_installed_list_and_relative_resolution_do_not_execute_tools() {
        let fixture = Fixture::new();
        let script = RUSTUP.replace(
            "'toolchain list')",
            "'toolchain list') i=0; while [ \"$i\" -lt 2000 ]; do printf x; i=$((i+1)); done;",
        );
        fixture.script("rustup", &script);
        let report = fixture.probe(Some("pinned"));
        assert!(report.has_blockers());
        assert!(report.probes.iter().any(|p| p.output_limited));
        assert!(!fixture.calls().contains("<run>"));
        fixture.script(
            "rustup",
            &RUSTUP.replace(
                "printf '%s/%s\\n' \"$root\" \"$4\"",
                "printf '%s\\n' \"$4\"",
            ),
        );
        let report = fixture.probe(Some("pinned"));
        assert!(report.has_blockers());
        assert!(!fixture.calls().contains("<run>"));
    }
}
