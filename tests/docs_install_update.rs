//! Install/update/release documentation contract, separate from student guides.
#[path = "support/test_home.rs"]
mod test_home;
use std::{fs, path::PathBuf};
fn read_doc(name: &str) -> String {
    fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("docs")
            .join(name),
    )
    .unwrap()
}
fn unwrapped(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn usage_line() -> String {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let line = stdout
        .lines()
        .find(|line| line.starts_with("Usage: "))
        .unwrap_or_else(|| panic!("no usage line in {stdout:?}"));
    line.trim_start_matches("Usage: ").to_owned()
}

#[test]
fn release_docs_explain_source_distribution_and_immutable_publication() {
    let releasing = unwrapped(&read_doc("releasing.md"));
    for wording in [
        "[workspace.package]",
        "Cargo.lock",
        "git tag -a vX.Y.Z",
        "git push origin vX.Y.Z",
        "draft",
        "latest.json",
        "gh release edit vX.Y.Z --draft=false",
        "Never move a tag",
        "bump",
        "targets",
        "https://github.com/baochunli/rustrace/releases/latest/download/latest.json",
    ] {
        assert!(
            releasing.contains(wording),
            "release procedure must mention {wording}"
        );
    }
    for path in [
        "README.md",
        "docs/installation.md",
        "docs/student-guide.md",
        "docs/supported-environment.md",
    ] {
        let text =
            fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap();
        assert!(
            text.contains("latest release tag"),
            "{path} must explain vX.Y.Z"
        );
    }
    let installation = unwrapped(&read_doc("installation.md"));
    assert!(installation.contains("builds from source"));
    assert!(installation.contains("does not distribute prebuilt binaries"));
    for doc in [&releasing, &installation] {
        assert!(doc.contains("`package_format` is the provenance container version"));
        assert!(
            doc.contains("`assignment_format` is the highest accepted assignment package version")
        );
    }
}

#[test]
fn update_docs_disclose_default_daily_request_deadline_offline_and_opt_out() {
    let privacy = unwrapped(&read_doc("privacy.md"));
    for wording in [
        "Automatic update checks are on by default.",
        "https://github.com/baochunli/rustrace/releases/latest/download/latest.json",
        "The GET itself and ordinary IP/request metadata go to GitHub.",
        "No assignment content, student ID, tool output, provenance, or telemetry is sent.",
        "before creating or resuming your recording session",
        "$XDG_STATE_HOME/rustrace/update-state.json",
        "~/.local/state/rustrace/update-state.json",
        "Choose Automatic checks: On/Off in the F7 menu to persist the preference",
        "Automatic checks: On/Off",
        "Changing the preference makes no request.",
        "`config.toml`",
        "journal or submission",
    ] {
        assert!(privacy.contains(wording), "privacy must say: {wording}");
    }
    let environment = unwrapped(&read_doc("supported-environment.md"));
    for wording in [
        "one attempt per 24 hours across launches",
        "Failed attempts count",
        "one-second total deadline, including child cleanup",
        "no retries",
        "before session creation or resume",
        "Offline or malformed metadata preserves the cached status",
        "does not block work",
        "`doctor` reads only the cache",
        "Help, `--version`, replay, status, submit, verify, scan, and privacy stay offline",
        "`curl`",
        "20-second bound",
    ] {
        assert!(
            environment.contains(wording),
            "environment must say: {wording}"
        );
    }
    let student = unwrapped(&read_doc("student-guide.md"));
    assert!(student.contains("rustrace update --check"));
    assert!(!student.contains("rustrace update` without `--check` is not available yet"));
    assert!(student.contains("checks are on by default"));
    let installation = unwrapped(&read_doc("installation.md"));
    assert!(installation.contains("rustrace update --check"));
    for text in [&student, &installation] {
        for wording in [
            "Quit Rustrace before updating.",
            "builds from source",
            "takes a few minutes",
            "Restart Rustrace to use it.",
            "$XDG_STATE_HOME/rustrace/install.json",
            "Cargo-managed copies",
            "If post-install identity validation fails, the new binary is already installed at the reported path; follow the printed manual remedy to repair it.",
            "cargo +1.98.1 install --git https://github.com/baochunli/rustrace --tag vX.Y.Z rustrace --locked --force",
        ] {
            assert!(text.contains(wording), "update docs must say: {wording}");
        }
    }
    assert!(privacy.contains("`rustrace update` makes the same explicit metadata request"));
    assert!(privacy.contains("Cargo contacts the source repository and dependency endpoints"));
    assert!(usage_line().contains("rustrace update | rustrace update --check"));
    assert!(!installation.contains("rustrace update` without `--check` is not available yet"));
}

#[test]
fn contributor_tests_are_documented_as_hermetic() {
    let releasing = include_str!("../docs/releasing.md");
    for required in [
        "Running the tests",
        "hermetic and never contacts the network",
        "temporary `HOME`, `XDG_CONFIG_HOME`",
        "`XDG_STATE_HOME`",
        "automatic update checks disabled",
        "local fake curl fixtures",
    ] {
        assert!(
            releasing.contains(required),
            "missing contributor guidance: {required}"
        );
    }
}

#[test]
fn installer_docs_show_one_command_prerequisites_path_and_uninstall() {
    let one_liner = "curl -fsSL https://raw.githubusercontent.com/baochunli/rustrace/main/scripts/install.sh | sh";
    for path in ["README.md", "docs/installation.md", "docs/student-guide.md"] {
        let text =
            fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap();
        assert!(text.contains(one_liner), "{path} must show the installer");
        let normalized = unwrapped(&text);
        for phrase in ["rustup", "1.98.1", "a few minutes", "new terminal"] {
            assert!(normalized.contains(phrase), "{path} must mention {phrase}");
        }
        assert!(text.find(one_liner).unwrap() < text.find("cargo +1.98.1 install").unwrap());
    }
    let installation = unwrapped(&read_doc("installation.md"));
    for phrase in [
        "cargo uninstall rustrace",
        "install.json",
        "CARGO_INSTALL_ROOT",
        "RUSTRACE_INSTALL_DIR",
        "install.root",
        "config.toml",
        "Precedence:",
        "logical",
        "never installs rustup",
        "clippy",
        "rustfmt",
    ] {
        assert!(
            installation.contains(phrase),
            "installation must mention {phrase}"
        );
    }
}

#[test]
fn clean_machine_prerequisites_and_release_check_are_documented() {
    let environment = unwrapped(&read_doc("supported-environment.md"));
    for required in [
        "xcode-select --install",
        "sudo apt-get install build-essential pkg-config curl git",
        "sudo dnf install gcc gcc-c++ make pkgconf-pkg-config curl git",
        "tree-sitter",
        "bundled SQLite",
        "No signing or notarization",
        "install-smoke.yml",
        "ubuntu-22.04",
        "install time and disk use",
        "measured figures are not published yet",
        "wall time",
        "disk",
    ] {
        assert!(
            environment.contains(required),
            "missing prerequisite: {required}"
        );
    }
    let releasing = unwrapped(&read_doc("releasing.md"));
    for required in [
        "## Clean-machine check",
        "Run workflow",
        "job summary",
        "macos-latest",
        "ubuntu-latest",
        "ubuntu-22.04",
        "up to date",
        "gawk",
        "mawk",
        "system awk",
    ] {
        assert!(
            releasing.contains(required),
            "missing clean-machine guidance: {required}"
        );
    }
}

#[test]
fn reconciled_settings_first_launch_and_no_superseded_instructions() {
    let configuration = unwrapped(&read_doc("configuration.md"));
    for claim in [
        "Update settings are not in `config.toml`.",
        "Automatic checks: On/Off",
        "on by default",
        "$XDG_STATE_HOME/rustrace/update-state.json",
        "~/.local/state/rustrace/update-state.json",
        "persisted immediately",
        "next launch",
    ] {
        assert!(
            configuration.contains(claim),
            "configuration must say: {claim}"
        );
    }
    for name in [
        "installation.md",
        "student-guide.md",
        "supported-environment.md",
    ] {
        let text = unwrapped(&read_doc(name));
        for claim in [
            "On macOS, Linux, and WSL 2, no platform-specific first-launch steps are needed for a locally built binary.",
            "Update dependencies",
            "Update Rustrace",
            "Automatic checks: On/Off",
        ] {
            assert!(text.contains(claim), "{name} must say: {claim}");
        }
    }
    let readme = unwrapped(include_str!("../README.md"));
    for name in [
        "README.md",
        "installation.md",
        "privacy.md",
        "student-guide.md",
        "supported-environment.md",
    ] {
        let text = if name == "README.md" {
            readme.clone()
        } else {
            unwrapped(&read_doc(name))
        };
        for superseded in [
            "interim opt-out",
            "not available yet",
            "edit the existing JSON state file",
            "checks_enabled` to `false`",
        ] {
            assert!(
                !text.contains(superseded),
                "{name} retained superseded guidance: {superseded}"
            );
        }
    }
    for claim in [
        "rustrace update",
        "rustrace update --check",
        "Restart Rustrace",
        "on by default",
        "Automatic checks: On/Off",
        "cargo uninstall rustrace",
    ] {
        assert!(readme.contains(claim), "README must say: {claim}");
    }
    let guide = unwrapped(&read_doc("student-guide.md"));
    assert!(guide.contains("six lines: the version, the build commit, the event format, the package format, the assignment format, and the target triple"));
    let releasing = unwrapped(&read_doc("releasing.md"));
    assert!(releasing.contains("install_update_e2e"));
    assert!(releasing.contains("Update Rustrace NEW"));
    assert!(releasing.contains("● menu"));
}

#[test]
fn install_update_operational_details_are_explicit() {
    let install = unwrapped(&read_doc("installation.md"));
    for claim in [
        "Install rustup 1.28.1 or newer",
        "Xcode Command Line Tools",
        "build-essential",
        "Installation requires curl, Git, and dependency network access",
        "Cargo installs into `~/.cargo/bin` by default",
        "The binary is placed in the root's `bin/` subdirectory",
        "appends a marked block to `~/.zshrc`, or `~/.bashrc` and an existing `~/.bash_profile`",
        "Repeated installs with the same directory do not duplicate the entry",
        "Fish and unknown shells receive a printed instruction",
        "add `--root <install-root>` for a custom root",
        "You may remove the marked Rustrace installer block",
        "Your assignments and other Rustrace state are retained",
        "Sessions already running keep their recorded build identity",
        "A failed Cargo build leaves the previous executable unchanged",
        "An active workspace session prevents updating from that workspace",
        "changing it makes no request",
        "outside assignment provenance",
        "an available update is a warning, never a blocker",
    ] {
        assert!(install.contains(claim), "installation must say: {claim}");
    }
    let privacy = unwrapped(&read_doc("privacy.md"));
    for claim in [
        "one attempt per 24 hours",
        "one-second total deadline and no retries",
        "failed request leaves the cached status usable and does not block your work",
        "20-second bound, even when automatic checks are off",
        "persist the preference for the next launch",
        "saved immediately",
        "only cached state and the installation receipt",
        "it never checks or installs during a session",
    ] {
        assert!(privacy.contains(claim), "privacy must say: {claim}");
    }
    let supported = unwrapped(&read_doc("supported-environment.md"));
    for claim in [
        "Quit Rustrace before updating",
        "Restart Rustrace after success",
        "other copies receive a release-pinned Cargo command",
        "automatic checks on/off",
        "an update is never a blocker",
        "● menu",
    ] {
        assert!(
            supported.contains(claim),
            "supported environment must say: {claim}"
        );
    }
}
