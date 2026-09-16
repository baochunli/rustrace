#![cfg(unix)]

#[path = "support/test_home.rs"]
mod test_home;
#[cfg(target_os = "macos")]
use rustrace::ghostty::GhosttyKeyBindings;
use rustrace_workspace::hash::PinnedWorkspaceRoot;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};

#[cfg(target_os = "macos")]
use std::os::unix::fs::MetadataExt;

const BLOCK_BYTES: usize = 512;
const GHOSTTY_DEFAULTS: &str = r#"keybind = super+arrow_up=jump_to_prompt:-1
keybind = super+arrow_down=jump_to_prompt:1
keybind = super+arrow_left=text:\\x01
keybind = super+arrow_right=text:\\x05
keybind = super+backspace=text:\\x15
keybind = alt+arrow_left=esc:b
keybind = alt+arrow_right=esc:f
keybind = super+f=start_search
keybind = super+z=undo
keybind = super+k=clear_screen
keybind = super+home=scroll_to_top
keybind = super+end=scroll_to_bottom
"#;
const GHOSTTY_UNBOUND: &str = r#"keybind = super+arrow_up=unbind
keybind = super+arrow_down=unbind
keybind = super+arrow_left=unbind
keybind = super+arrow_right=unbind
keybind = super+backspace=unbind
keybind = alt+arrow_left=unbind
keybind = alt+arrow_right=unbind
keybind = super+f=unbind
keybind = super+z=unbind
keybind = super+k=unbind
keybind = super+home=unbind
keybind = super+end=unbind
"#;
const MANIFEST: &str = r#"format_version = 1
course_id = "ECE1724"
assignment_id = "doctor"
assignment_version = "2026-09-08"
title = "Doctor acceptance fixture"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["Cargo.toml", "src/**/*.rs"]

[commands]
check = ["cargo", "check", "--locked"]
test = ["cargo", "test", "--locked"]
run = ["cargo", "run", "--locked"]
clippy = ["cargo", "clippy", "--locked"]
format = ["cargo", "fmt"]
"#;

struct Fixture {
    home: test_home::TestHome,
    root: PathBuf,
    package: PathBuf,
    workspace: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rustrace-doctor-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let package = root.join("assignment.rta");
        fs::write(&package, valid_package()).unwrap();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("installed"), b"1.98.1\n").unwrap();
        for (name, output) in [
            (
                "rustc",
                "rustc 1.98.1 (fixture)\nrelease: 1.98.1\nhost: test-host\n",
            ),
            ("cargo", "cargo 1.98.1 (fixture)\n"),
            ("rust-analyzer", "rust-analyzer 1.98.1 (fixture)\n"),
            ("rustfmt", "rustfmt 1.98.1 (fixture)\n"),
            ("cargo-clippy", "clippy 1.98.1 (fixture)\n"),
        ] {
            write_script(&bin.join(name), &format!("printf '{output}'"));
        }
        write_script(
            &bin.join("rustup"),
            r#"
root=${0%/*}
[ "$RUSTUP_AUTO_INSTALL" = 0 ] || exit 91
case "$1 $2" in
  '--version ') printf 'rustup 1.28.2 (fixture)\n' ;;
  'toolchain list') while IFS= read -r line; do printf '%s\n' "$line"; done < "$root/installed" ;;
  'show active-toolchain') printf '1.98.1 (environment override)\n' ;;
  'which --toolchain')
    [ -f "$root/$4" ] || { printf 'component unavailable\n' >&2; exit 1; }
    printf '%s/%s\n' "$root" "$4"
    ;;
  run\ *) shift 2; exec "$@" ;;
  *) printf 'forbidden fixture command\n' >&2; exit 90 ;;
esac
"#,
        );
        Self {
            home: test_home::TestHome::new(false),
            workspace: root.join("assignment.work"),
            root,
            package,
            bin,
        }
    }

    fn run(&self) -> Output {
        self.command().output().unwrap()
    }

    fn command(&self) -> Command {
        let mut command = self.home.command(env!("CARGO_BIN_EXE_rustrace"));
        command
            .arg("doctor")
            .arg(&self.package)
            .arg("--workspace")
            .arg(&self.workspace)
            .env("PATH", &self.bin)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("RUSTUP_AUTO_INSTALL", "1")
            .env_remove("RUSTUP_TOOLCHAIN");
        command
    }

    fn write_config(&self, contents: &str) {
        let directory = self.root.join("config/rustrace");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("config.toml"), contents).unwrap();
    }

    fn run_in_pty(&self, columns: u16, rows: u16) -> Output {
        let test_home = test_home::TestHome::new(false);
        test_home
            .command("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/doctor_pty.py"
            ))
            .arg(columns.to_string())
            .arg(rows.to_string())
            .arg(&self.bin)
            .arg("--fixture-config-home")
            .arg(self.root.join("config"))
            .arg(env!("CARGO_BIN_EXE_rustrace"))
            .arg("doctor")
            .arg(&self.package)
            .arg("--workspace")
            .arg(&self.workspace)
            .env("RUSTUP_AUTO_INSTALL", "1")
            .env("TERM", "xterm-256color")
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .unwrap()
    }

    fn write_ghostty(&self, output: &str) {
        let quoted = output.replace('\'', "'\\''");
        write_script(
            &self.bin.join("ghostty"),
            &format!("[ \"$1\" = +list-keybinds ] || exit 90\nprintf '%s' '{quoted}'"),
        );
    }

    fn run_with_ghostty(&self) -> Output {
        self.command()
            .env("TERM_PROGRAM", "ghostty")
            .env("GHOSTTY_BIN_DIR", &self.bin)
            .output()
            .unwrap()
    }

    fn run_with_ghostty_in_pty(&self) -> Output {
        let test_home = test_home::TestHome::new(false);
        test_home
            .command("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/doctor_pty.py"
            ))
            .args(["80", "24"])
            .arg(&self.bin)
            .arg("--fixture-config-home")
            .arg(self.root.join("config"))
            .arg(env!("CARGO_BIN_EXE_rustrace"))
            .arg("doctor")
            .arg(&self.package)
            .arg("--workspace")
            .arg(&self.workspace)
            .env("RUSTUP_AUTO_INSTALL", "1")
            .env("TERM", "xterm-256color")
            .env("TERM_PROGRAM", "ghostty")
            .env("GHOSTTY_BIN_DIR", &self.bin)
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .unwrap()
    }

    #[cfg(target_os = "macos")]
    fn run_with_real_ghostty_in_pty(&self, ghostty_bin_dir: &Path, xdg: &Path) -> Output {
        let test_home = test_home::TestHome::new(false);
        test_home
            .command("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/doctor_pty.py"
            ))
            .args(["80", "24"])
            .arg(&self.bin)
            .arg("--fixture-config-home")
            .arg(xdg)
            .arg(env!("CARGO_BIN_EXE_rustrace"))
            .arg("doctor")
            .arg(&self.package)
            .arg("--workspace")
            .arg(&self.workspace)
            .env("RUSTUP_AUTO_INSTALL", "1")
            .env("TERM", "xterm-256color")
            .env("TERM_PROGRAM", "ghostty")
            .env("GHOSTTY_BIN_DIR", ghostty_bin_dir)
            .env("XDG_CONFIG_HOME", xdg)
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        make_owner_writable(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn clean_environment_reports_every_check_ok_and_exits_zero() {
    let fixture = Fixture::new("clean");

    let output = fixture.run();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains(&format!("rustrace {}", env!("CARGO_PKG_VERSION"))));
    for check in [
        "OK terminal: not checked",
        "OK workspace:",
        "OK disk space:",
        "OK rustup:",
        "OK assignment toolchain:",
        "OK Cargo:",
        "OK rustfmt:",
        "OK Clippy:",
        "OK rust-analyzer:",
        "OK starter package: self-contained package",
        "OK session ownership:",
        "OK filesystem capabilities:",
        "OK cumulative storage headroom:",
        "Doctor summary:",
        "0 WARNING, 0 BLOCKER",
    ] {
        assert!(stdout.contains(check), "missing {check:?} in:\n{stdout}");
    }
    assert!(!fixture.workspace.exists());
    assert_eq!(directory_names(&fixture.root), ["assignment.rta", "bin"]);
    assert!(!stdout.contains("host-name checks"), "{stdout}");
    assert!(
        stdout.contains("host-name collision rejection is checked at session start, not by doctor"),
        "{stdout}"
    );
}

#[test]
fn doctor_reports_the_effective_theme_and_every_palette_fields_source() {
    let fixture = Fixture::new("theme");
    fixture.write_config(
        r##"[theme]
name = "catppuccin-latte"

[theme.custom]
accent = "#010203"
error_bg = "#040506"
tab_active_fg = "#070809"
"##,
    );

    let output = fixture.run();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("OK theme: effective catppuccin-latte; auto_switch = false"),
        "{stdout}"
    );
    for field in rustrace::tui::theme::PALETTE_FIELD_NAMES {
        assert!(
            stdout.contains(&format!("  {field} = ")),
            "missing {field}:\n{stdout}"
        );
    }
    assert!(
        stdout.contains("accent = #010203 (source: custom)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("error_bg = #040506 (source: custom)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("tab_active_fg = #070809 (source: custom)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("red = #d20f39 (source: catppuccin-latte built-in)"),
        "{stdout}"
    );
}

#[test]
fn doctor_reports_invalid_theme_configuration_as_one_warning_entry() {
    let fixture = Fixture::new("theme-invalid");
    fixture.write_config("[theme.custom]\naccent = \"blue\"\n");

    let output = fixture.run();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("WARNING theme:"), "{stdout}");
    assert!(stdout.contains("config.toml"), "{stdout}");
    assert!(stdout.contains("1 WARNING, 0 BLOCKER"), "{stdout}");
}

#[cfg(target_os = "macos")]
#[test]
fn ghostty_doctor_reports_owned_rewritten_and_passed_setup_keys() {
    let fixture = Fixture::new("ghostty-defaults");
    fixture.write_ghostty(GHOSTTY_DEFAULTS);

    let defaults = fixture.run_with_ghostty();
    let stdout = String::from_utf8(defaults.stdout).unwrap();
    assert!(defaults.status.success(), "{stdout}");
    for line in [
        "Ghostty key super+arrow_up: owned (jump_to_prompt:-1)",
        r"Ghostty key super+arrow_left: rewritten (text:\\x01)",
        "Ghostty key alt+arrow_right: rewritten (esc:f)",
        "Ghostty key super+f: owned (start_search)",
        "Run `rustrace doctor --write-ghostty-keys` to write the recommended unbind snippet.",
    ] {
        assert!(stdout.contains(line), "missing {line:?} in:\n{stdout}");
    }
    assert!(stdout.contains("0 WARNING, 0 BLOCKER"), "{stdout}");

    fixture.write_ghostty("keybind = super+f=text:x\n");
    let arbitrary_rewrite = fixture.run_with_ghostty();
    let stdout = String::from_utf8(arbitrary_rewrite.stdout).unwrap();
    assert!(arbitrary_rewrite.status.success(), "{stdout}");
    assert!(
        stdout.contains("Ghostty key super+f: rewritten (text:x)"),
        "{stdout}"
    );

    fixture.write_ghostty(GHOSTTY_UNBOUND);
    let unbound = fixture.run_with_ghostty();
    let stdout = String::from_utf8(unbound.stdout).unwrap();
    assert!(unbound.status.success(), "{stdout}");
    for key in [
        "super+arrow_up",
        "super+arrow_down",
        "super+arrow_left",
        "super+arrow_right",
        "super+backspace",
        "alt+arrow_left",
        "alt+arrow_right",
        "super+f",
        "super+z",
        "super+k",
        "super+home",
        "super+end",
    ] {
        assert!(
            stdout.contains(&format!("Ghostty key {key}: passed")),
            "{stdout}"
        );
    }
    assert!(
        !stdout.contains("--write-ghostty-keys` to write"),
        "{stdout}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn ghostty_doctor_cli_failure_is_informational_and_assumes_defaults() {
    let fixture = Fixture::new("ghostty-missing");
    write_script(&fixture.bin.join("ghostty"), "exit 127");

    let output = fixture.run_with_ghostty();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("Ghostty CLI unavailable; assuming Ghostty defaults"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Ghostty key super+arrow_left: rewritten"),
        "{stdout}"
    );
    assert!(stdout.contains("Ghostty key super+f: owned"), "{stdout}");
    assert!(stdout.contains("0 WARNING, 0 BLOCKER"), "{stdout}");
}

#[cfg(target_os = "macos")]
#[test]
fn ghostty_doctor_section_is_visible_in_an_attached_terminal() {
    let fixture = Fixture::new("ghostty-pty");
    fixture.write_ghostty(GHOSTTY_DEFAULTS);

    let output = fixture.run_with_ghostty_in_pty();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("OK terminal: 80x24"), "{stdout}");
    assert!(stdout.contains("Ghostty key setup"), "{stdout}");
    assert!(stdout.contains("Ghostty key super+f: owned"), "{stdout}");
    assert!(stdout.contains("Doctor summary:"), "{stdout}");
}

#[cfg(target_os = "macos")]
#[test]
fn real_ghostty_defaults_are_exact_translations_in_the_doctor_pty_probe() {
    let ghostty = Path::new("/Applications/Ghostty.app/Contents/MacOS/ghostty");
    if !ghostty.is_file() {
        return;
    }

    let fixture = Fixture::new("ghostty-real-pty");
    let xdg = fixture.root.join("empty-xdg");
    fs::create_dir(&xdg).unwrap();
    let list = Command::new(ghostty)
        .arg("+list-keybinds")
        .env("XDG_CONFIG_HOME", &xdg)
        .output()
        .unwrap();
    let list_stdout = String::from_utf8(list.stdout).unwrap();
    assert!(list.status.success(), "{list_stdout}");

    let bindings = GhosttyKeyBindings::from_list_output(&list_stdout);
    for key in ["super+arrow_left", "super+arrow_right", "super+backspace"] {
        assert!(
            bindings.command_hint_available(key),
            "real Ghostty binding was not classified as an exact translation for {key}:\n{list_stdout}"
        );
    }

    let ghostty_bin_dir = ghostty.parent().unwrap();
    let output = fixture.run_with_real_ghostty_in_pty(ghostty_bin_dir, &xdg);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stdout}");
    for line in [
        r"Ghostty key super+arrow_left: rewritten (text:\\x01)",
        r"Ghostty key super+arrow_right: rewritten (text:\\x05)",
        r"Ghostty key super+backspace: rewritten (text:\\x15)",
    ] {
        assert!(stdout.contains(line), "missing {line:?} in:\n{stdout}");
    }
}

#[test]
fn ghostty_writer_uses_xdg_and_writes_the_complete_marked_snippet() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("ghostty-write-xdg");
    let xdg = fixture.root.join("xdg");
    let home = fixture.root.join("home");
    fs::create_dir(&home).unwrap();

    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .args(["doctor", "--write-ghostty-keys"])
        .env("XDG_CONFIG_HOME", &xdg)
        .env("HOME", &home)
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success(), "{stdout}");
    let path = xdg.join("ghostty/rustrace-keys");
    let contents = fs::read_to_string(&path).unwrap();
    assert!(
        contents.starts_with("# Rustrace Ghostty key setup\n"),
        "{contents}"
    );
    assert_eq!(contents.matches("keybind = ").count(), 12, "{contents}");
    for line in GHOSTTY_UNBOUND.lines() {
        assert!(contents.contains(line), "missing {line:?} in:\n{contents}");
    }
    assert!(!home.join(".config/ghostty/rustrace-keys").exists());
    assert!(stdout.contains(&path.display().to_string()), "{stdout}");
    assert!(stdout.contains("config-file = rustrace-keys"), "{stdout}");
    assert!(stdout.contains("⌘⇧,"), "{stdout}");
    assert!(
        stdout.contains("Ghostty's own super+shift+,=reload_config binding"),
        "{stdout}"
    );
    assert!(!stdout.contains("Reload Ghostty with ⌘"), "{stdout}");
}

#[test]
fn ghostty_writer_falls_back_to_home_and_identical_rewrite_is_a_no_op() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("ghostty-write-home");
    let home = fixture.root.join("home");
    fs::create_dir(&home).unwrap();
    let path = home.join(".config/ghostty/rustrace-keys");
    let run = || {
        test_home
            .command(env!("CARGO_BIN_EXE_rustrace"))
            .args(["doctor", "--write-ghostty-keys"])
            .env_remove("XDG_CONFIG_HOME")
            .env("HOME", &home)
            .output()
            .unwrap()
    };

    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o400);
    fs::set_permissions(&path, permissions).unwrap();
    let second = run();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(&path, permissions).unwrap();

    assert!(
        second.status.success(),
        "identical snippet must not be rewritten: {}",
        String::from_utf8_lossy(&second.stdout)
    );
    assert!(
        String::from_utf8(second.stdout)
            .unwrap()
            .contains("already up to date"),
    );
}

#[test]
fn ghostty_writer_refuses_to_overwrite_an_unmarked_file() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("ghostty-write-refuse");
    let xdg = fixture.root.join("xdg");
    let path = xdg.join("ghostty/rustrace-keys");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "# managed by the student\nkeybind = super+f=unbind\n",
    )
    .unwrap();

    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .args(["doctor", "--write-ghostty-keys"])
        .env("XDG_CONFIG_HOME", &xdg)
        .env("HOME", fixture.root.join("home"))
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(!output.status.success(), "{stdout}");
    assert!(stdout.contains("refusing to overwrite"), "{stdout}");
    assert!(
        stdout.contains("does not have the Rustrace marker"),
        "{stdout}"
    );
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "# managed by the student\nkeybind = super+f=unbind\n"
    );
}

#[test]
fn resolvable_workspace_root_symlink_is_accepted() {
    let fixture = Fixture::new("workspace-root-symlink");
    let real_workspace = fixture.root.join("real-workspace");
    fs::create_dir(&real_workspace).unwrap();
    symlink(&real_workspace, &fixture.workspace).unwrap();

    let output = fixture.run();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("OK workspace:"), "{stdout}");
    assert!(!stdout.contains("BLOCKER workspace:"), "{stdout}");
}

#[test]
fn existing_workspace_is_not_used_for_the_write_probe() {
    let fixture = Fixture::new("existing-workspace-write-probe");
    fs::create_dir(&fixture.workspace).unwrap();
    fs::write(fixture.workspace.join("Cargo.toml"), b"[package]\n").unwrap();
    let before = fs::metadata(&fixture.workspace)
        .unwrap()
        .modified()
        .unwrap();
    thread::sleep(Duration::from_millis(20));

    let output = fixture.run();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let after = fs::metadata(&fixture.workspace)
        .unwrap()
        .modified()
        .unwrap();

    assert!(output.status.success(), "{stdout}");
    assert_eq!(
        after, before,
        "doctor mutated the managed workspace directory"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn finder_locked_existing_workspace_is_a_blocker_with_exit_one() {
    let fixture = Fixture::new("finder-locked-workspace");
    fs::create_dir(&fixture.workspace).unwrap();
    set_user_immutable(&fixture.workspace, true);
    let write_error = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(fixture.workspace.join("cannot-write"))
        .unwrap_err();

    let output = fixture.run();
    set_user_immutable(&fixture.workspace, false);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(
        write_error.raw_os_error(),
        Some(libc::EPERM),
        "fixture must be genuinely unwritable"
    );
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("BLOCKER workspace:"), "{stdout}");
    assert!(
        stdout.contains("Make the workspace directory writable"),
        "{stdout}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn existing_workspace_on_distinct_filesystem_is_measured_there() {
    let fixture = Fixture::new("distinct-workspace-filesystem");
    let volume = MountedVolume::new(&fixture.root);
    let real_workspace = volume.mount.join("real-workspace");
    fs::create_dir(&real_workspace).unwrap();
    symlink(&real_workspace, &fixture.workspace).unwrap();
    assert_ne!(
        fs::metadata(&fixture.root).unwrap().dev(),
        fs::metadata(&real_workspace).unwrap().dev(),
        "fixture must use a distinct filesystem"
    );

    let output = fixture.run();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let canonical_workspace = fs::canonicalize(&real_workspace).unwrap();

    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("BLOCKER disk space:"), "{stdout}");
    assert!(
        stdout.contains("BLOCKER cumulative storage headroom:"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "workspace filesystem at {}",
            canonical_workspace.display()
        )),
        "{stdout}"
    );
}

#[test]
fn missing_rust_analyzer_is_a_warning_with_action_and_exit_zero() {
    let fixture = Fixture::new("missing-ra");
    fs::remove_file(fixture.bin.join("rust-analyzer")).unwrap();

    let output = fixture.run();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("WARNING rust-analyzer:"), "{stdout}");
    assert!(
        stdout.contains("WARNING rust-analyzer: resolve probe failed (component unavailable)"),
        "failed probe must be a plain phrase, not a Debug-formatted status: {stdout}"
    );
    assert!(
        stdout.contains("rustup component add rust-analyzer"),
        "{stdout}"
    );
    assert!(stdout.contains("1 WARNING, 0 BLOCKER"), "{stdout}");
}

#[test]
fn missing_pinned_toolchain_is_a_blocker_with_exit_one() {
    let fixture = Fixture::new("missing-toolchain");

    fs::write(fixture.bin.join("installed"), b"stable-test-host\n").unwrap();
    let output = fixture.run();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("BLOCKER assignment toolchain:"), "{stdout}");
    assert!(
        stdout.contains("rustup toolchain install 1.98.1"),
        "{stdout}"
    );
    assert!(stdout.contains("BLOCKER"), "{stdout}");
}

#[test]
fn unwritable_existing_workspace_is_a_blocker_with_exit_one() {
    let fixture = Fixture::new("unwritable");
    fs::create_dir(&fixture.workspace).unwrap();
    fs::set_permissions(&fixture.workspace, fs::Permissions::from_mode(0o500)).unwrap();

    let output = fixture.run();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("BLOCKER workspace:"), "{stdout}");
    assert!(
        stdout.contains("Make the workspace directory writable"),
        "{stdout}"
    );
}

#[test]
fn corrupt_assignment_package_is_a_blocker_and_scratch_is_removed() {
    let fixture = Fixture::new("corrupt");
    fs::write(&fixture.package, b"not an assignment archive").unwrap();

    let output = fixture.run();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("BLOCKER starter package:"), "{stdout}");
    assert!(stdout.contains("Re-download assignment.rta"), "{stdout}");
    assert_eq!(directory_names(&fixture.root), ["assignment.rta", "bin"]);
}

#[test]
fn non_self_contained_starters_are_blockers_with_the_structure_remedy() {
    for (cargo, remedy) in [
        (
            "[package]\n",
            "add an empty [workspace] table to starter/Cargo.toml",
        ),
        (
            "[package]\n[workspace]\nmembers = ['other']\n",
            "workspace.members",
        ),
        (
            "[package]\nworkspace = 'parent'\n[workspace]\n",
            "package.workspace",
        ),
    ] {
        let fixture = Fixture::new("non-self-contained");
        fs::write(&fixture.package, package_with_starter(cargo.as_bytes())).unwrap();
        let output = fixture.run();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1), "{stdout}");
        assert!(stdout.contains("BLOCKER starter package: validation failed: assignment starter must be a self-contained package:"), "{stdout}");
        assert!(stdout.contains(remedy), "{stdout}");
        assert!(stdout.contains("  Remedy: Ask the course staff for a repackaged assignment whose starter Cargo.toml declares an empty [workspace] table, then re-download."), "{stdout}");
        assert!(!fixture.workspace.exists());
        assert_eq!(directory_names(&fixture.root), ["assignment.rta", "bin"]);
    }
}

#[test]
fn unreadable_assignment_package_has_a_local_access_remedy() {
    let fixture = Fixture::new("unreadable-package");
    fs::set_permissions(&fixture.package, fs::Permissions::from_mode(0o000)).unwrap();

    let output = fixture.run();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("BLOCKER starter package:"), "{stdout}");
    assert!(
        stdout.contains("Make assignment.rta readable and its directory writable"),
        "{stdout}"
    );
    assert!(!stdout.contains("Re-download assignment.rta"), "{stdout}");
}

#[test]
fn staging_failure_names_the_workspace_parent_in_its_remedy() {
    let mut fixture = Fixture::new("unwritable-staging-parent");
    let workspace_parent = fixture.root.join("workspace-parent");
    fs::create_dir(&workspace_parent).unwrap();
    let canonical_parent = fs::canonicalize(&workspace_parent).unwrap();
    fixture.workspace = workspace_parent.join("assignment.work");
    fs::set_permissions(&workspace_parent, fs::Permissions::from_mode(0o500)).unwrap();

    let output = fixture.run();
    fs::set_permissions(&workspace_parent, fs::Permissions::from_mode(0o700)).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("BLOCKER starter package:"), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "Make the workspace parent {} writable",
            canonical_parent.display()
        )),
        "{stdout}"
    );
    assert!(
        !stdout.contains("Make assignment.rta readable and its directory writable"),
        "{stdout}"
    );
}

#[test]
fn workspace_held_by_a_live_session_is_reported_as_a_blocker() {
    let fixture = Fixture::new("live-owner");
    fs::create_dir(&fixture.workspace).unwrap();
    fs::write(fixture.workspace.join("Cargo.toml"), b"[package]\n").unwrap();
    let pinned = PinnedWorkspaceRoot::open(&fixture.workspace).unwrap();
    let _owner = pinned
        .open_state_directory()
        .unwrap()
        .lock_for_inspection()
        .unwrap();

    let output = fixture.run();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("BLOCKER session ownership:"), "{stdout}");
    assert!(stdout.contains("live session"), "{stdout}");
}

#[test]
fn broken_workspace_and_state_symlinks_are_blockers() {
    for state_entry in [false, true] {
        let fixture = Fixture::new(if state_entry {
            "broken-state"
        } else {
            "broken-workspace"
        });
        if state_entry {
            fs::create_dir(&fixture.workspace).unwrap();
            symlink("missing-state-target", fixture.workspace.join(".rustrace")).unwrap();
        } else {
            symlink("missing-workspace-target", &fixture.workspace).unwrap();
        }

        let output = fixture.run();
        let stdout = String::from_utf8(output.stdout).unwrap();

        assert_eq!(output.status.code(), Some(1), "{stdout}");
        assert!(stdout.contains("BLOCKER"), "{stdout}");
        assert!(
            stdout.contains(if state_entry {
                "BLOCKER session ownership:"
            } else {
                "BLOCKER workspace:"
            }),
            "{stdout}"
        );
    }
}

#[test]
fn attached_terminal_requires_80_by_24_and_detects_supported_capabilities() {
    let fixture = Fixture::new("terminal");

    let too_small = fixture.run_in_pty(79, 24);
    let too_small_stdout = String::from_utf8(too_small.stdout).unwrap();
    assert_eq!(too_small.status.code(), Some(1), "{too_small_stdout}");
    assert!(
        too_small_stdout.contains("BLOCKER terminal: 79x24"),
        "{too_small_stdout}"
    );

    let supported = fixture.run_in_pty(80, 24);
    let supported_stdout = String::from_utf8(supported.stdout).unwrap();
    assert!(supported.status.success(), "{supported_stdout}");
    assert!(
        supported_stdout.contains("OK terminal: 80x24; color and bracketed-paste support detected"),
        "{supported_stdout}"
    );
}

#[test]
fn tool_output_is_rendered_through_safe_display() {
    let fixture = Fixture::new("safe-display");
    write_script(
        &fixture.bin.join("cargo"),
        "printf 'cargo 1.98.1 (fixture)\\033[31m\\n'",
    );

    let output = fixture.run();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success(), "{stdout}");
    assert!(!stdout.contains('\u{1b}'), "{stdout:?}");
    assert!(stdout.contains(r"\u{1b}"), "{stdout}");
}

fn valid_package() -> Vec<u8> {
    package_with_starter(b"[package]\nname = \"doctor-fixture\"\n[workspace]\n")
}

fn package_with_starter(cargo: &[u8]) -> Vec<u8> {
    let entries = [
        ("assignment.toml", false, MANIFEST.as_bytes()),
        ("starter/", true, &[][..]),
        ("starter/Cargo.toml", false, cargo),
        ("starter/src/", true, &[][..]),
        ("starter/src/main.rs", false, b"fn main() {}\n"),
    ];
    let mut archive = Vec::new();
    for (path, directory, contents) in entries {
        let mut header = [0_u8; BLOCK_BYTES];
        header[..path.len()].copy_from_slice(path.as_bytes());
        write_octal(&mut header[100..108], if directory { 0o755 } else { 0o644 });
        write_octal(&mut header[108..116], 0);
        write_octal(&mut header[116..124], 0);
        write_octal(&mut header[124..136], contents.len() as u64);
        write_octal(&mut header[136..148], 0);
        header[148..156].fill(b' ');
        header[156] = if directory { b'5' } else { b'0' };
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        archive.extend_from_slice(&header);
        archive.extend_from_slice(contents);
        archive.resize(archive.len().next_multiple_of(BLOCK_BYTES), 0);
    }
    archive.resize(archive.len() + 2 * BLOCK_BYTES, 0);
    archive
}

fn write_octal(field: &mut [u8], value: u64) {
    let encoded = format!("{:0width$o}\0", value, width = field.len() - 1);
    field.copy_from_slice(encoded.as_bytes());
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(target_os = "macos")]
fn set_user_immutable(path: &Path, immutable: bool) {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let flags = if immutable { libc::UF_IMMUTABLE } else { 0 };
    let result = unsafe { libc::chflags(path.as_ptr(), flags) };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
}

#[cfg(target_os = "macos")]
struct MountedVolume {
    image: PathBuf,
    mount: PathBuf,
}

#[cfg(target_os = "macos")]
impl MountedVolume {
    fn new(root: &Path) -> Self {
        let image = root.join("doctor-volume.dmg");
        let mount = root.join("mounted-volume");
        fs::create_dir(&mount).unwrap();
        let create = Command::new("/usr/bin/hdiutil")
            .args([
                "create", "-quiet", "-size", "100m", "-fs", "APFS", "-volname",
            ])
            .arg("rustrace-doctor-test")
            .arg(&image)
            .output()
            .unwrap();
        assert!(
            create.status.success(),
            "hdiutil create failed: {}",
            String::from_utf8_lossy(&create.stderr)
        );
        let attach = Command::new("/usr/bin/hdiutil")
            .args(["attach", "-quiet", "-nobrowse", "-mountpoint"])
            .arg(&mount)
            .arg(&image)
            .output()
            .unwrap();
        assert!(
            attach.status.success(),
            "hdiutil attach failed: {}",
            String::from_utf8_lossy(&attach.stderr)
        );
        Self { image, mount }
    }
}

#[cfg(target_os = "macos")]
impl Drop for MountedVolume {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/hdiutil")
            .args(["detach", "-quiet"])
            .arg(&self.mount)
            .status();
        let _ = fs::remove_file(&self.image);
    }
}

fn make_owner_writable(path: &Path) {
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(permissions.mode() | 0o700);
        let _ = fs::set_permissions(path, permissions);
    }
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                make_owner_writable(&entry.path());
            }
        }
    }
}

fn directory_names(path: &Path) -> Vec<String> {
    let mut names = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn version_advisory_is_cache_only_unknown_current_available_and_curl_missing() {
    use rustrace::update::{ReleaseIdentity, UpdateState};
    let fixture = Fixture::new("version-cache");
    write_script(
        &fixture.bin.join("curl"),
        "printf late > \"${0%/*}/unexpected-request\"; exit 90",
    );
    let state_path = fixture.home.root.join("state/rustrace/update-state.json");
    let installed = env!("CARGO_PKG_VERSION");
    let mut state = UpdateState::default();
    for enabled in [true, false] {
        state.checks_enabled = enabled;
        state.save(&state_path).unwrap();
        let output = fixture.run();
        let text = String::from_utf8(output.stdout).unwrap();
        let on = if enabled { "on" } else { "off" };
        assert!(text.lines().any(|line| line == format!("OK Rustrace version: {installed} (no update check recorded; automatic checks {on})")), "{text}");
    }
    state.last_success = Some(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    );
    for enabled in [true, false] {
        state.checks_enabled = enabled;
        let on = if enabled { "on" } else { "off" };
        for version in [installed, "99.0.0", "0.0.0"] {
            state.latest = Some(ReleaseIdentity {
                version: version.into(),
                tag: format!("v{version}"),
                commit: "a".repeat(40),
            });
            state.save(&state_path).unwrap();
            let text = String::from_utf8(fixture.run().stdout).unwrap();
            let expected = if version == "99.0.0" {
                format!(
                    "WARNING Rustrace version: v99.0.0 is available; run rustrace update (automatic checks {on})"
                )
            } else {
                format!(
                    "OK Rustrace version: {installed} is the latest known (checked just now; automatic checks {on})"
                )
            };
            assert!(text.lines().any(|line| line == expected), "{text}");
            assert!(!text.contains("BLOCKER Rustrace version"));
        }
    }
    fs::remove_file(fixture.bin.join("curl")).unwrap();
    let text = String::from_utf8(fixture.run().stdout).unwrap();
    assert!(
        text.contains("update check unavailable: curl is not installed"),
        "{text}"
    );
    assert!(!fixture.bin.join("unexpected-request").exists());
    assert_eq!(
        UpdateState::load(&state_path),
        state,
        "doctor must not write cache"
    );
}

#[test]
fn doctor_pty_uses_the_fixture_theme_config() {
    let fixture = Fixture::new("pty-theme-invalid");
    fixture.write_config("[theme.custom]\naccent = \"blue\"\n");
    let output = fixture.run_in_pty(80, 24);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("WARNING theme:"), "{stdout}");
    assert!(stdout.contains("config.toml"), "{stdout}");
}
