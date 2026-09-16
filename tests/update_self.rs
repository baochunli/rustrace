#![cfg(unix)]
#[path = "support/test_home.rs"]
mod test_home;
use rustrace::update::UpdateState;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use test_home::TestHome;

struct Fixture {
    home: TestHome,
    root: PathBuf,
    binary: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let home = TestHome::new(false);
        let root = home.root.join("tools");
        fs::create_dir_all(&root).unwrap();
        let binary = home.root.join("install/bin/rustrace");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_rustrace"), &binary).unwrap();
        script(
            &root.join("curl"),
            &format!(
                "#!/usr/bin/python3\n{}",
                include_str!("support/update_curl.py")
            ),
        );
        script(&root.join("cargo"), include_str!("support/update_cargo.py"));
        fs::write(root.join("mode"), "success").unwrap();
        fs::write(root.join("cargo-mode"), "success").unwrap();
        let f = Self { home, root, binary };
        f.manifest("99.0.0");
        f.receipt("cargo-git");
        f
    }
    fn state(&self) -> PathBuf {
        self.home.root.join("state/rustrace/update-state.json")
    }
    fn receipt_path(&self) -> PathBuf {
        self.home.root.join("state/rustrace/install.json")
    }
    fn receipt(&self, method: &str) {
        fs::write(self.receipt_path(),serde_json::to_vec(&serde_json::json!({"schema_version":1,"method":method,"path":self.binary,"repository":"https://github.com/baochunli/rustrace","version":env!("CARGO_PKG_VERSION"),"tag":format!("v{}",env!("CARGO_PKG_VERSION"))})).unwrap()).unwrap();
    }
    fn manifest(&self, version: &str) {
        fs::write(self.root.join("latest.json"),serde_json::to_vec(&serde_json::json!({"schema_version":1,"version":version,"tag":format!("v{version}"),"commit":"a".repeat(40),"event_format":1,"package_format":1,"assignment_format":2,"source":{"repository":"https://github.com/baochunli/rustrace","tag":format!("v{version}")},"targets":{}})).unwrap()).unwrap();
    }
    fn command(&self) -> Command {
        let mut cmd = self.home.command(&self.binary);
        cmd.arg("update")
            .env("PATH", &self.root)
            .env("CARGO_HOME", self.home.root.join("cargo"))
            .env("CARGO_INSTALL_ROOT", self.home.root.join("wrong-root"))
            .env(
                "UPDATE_TARGET",
                rustrace::version::version_metadata().target(),
            );
        cmd
    }
    fn run(&self) -> Output {
        self.command().output().unwrap()
    }
}
fn script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn equal_and_older_do_not_build() {
    let f = Fixture::new();
    let before = fs::read(f.receipt_path()).unwrap();
    for version in [env!("CARGO_PKG_VERSION"), "0.0.0"] {
        f.manifest(version);
        let out = f.run();
        assert_eq!(out.status.code(), Some(0), "{}", text(&out));
        assert_eq!(
            text(&out),
            format!(
                "Checking for updates...\nAlready up to date ({}).\n",
                env!("CARGO_PKG_VERSION")
            )
        );
    }
    assert!(!f.root.join("cargo.json").exists());
    assert_eq!(fs::read(f.receipt_path()).unwrap(), before);
}
#[test]
fn up_to_date_refreshes_stale_receipt_from_running_binary() {
    let f = Fixture::new();
    let mut receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(f.receipt_path()).unwrap()).unwrap();
    receipt["version"] = serde_json::json!("0.0.0");
    receipt["tag"] = serde_json::json!("v0.0.0");
    fs::write(f.receipt_path(), serde_json::to_vec(&receipt).unwrap()).unwrap();
    f.manifest(env!("CARGO_PKG_VERSION"));
    let out = f.run();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out).ends_with(&format!(
            "Already up to date ({}).\n",
            env!("CARGO_PKG_VERSION")
        )),
        "{}",
        text(&out)
    );
    receipt["version"] = serde_json::json!(env!("CARGO_PKG_VERSION"));
    receipt["tag"] = serde_json::json!(format!("v{}", env!("CARGO_PKG_VERSION")));
    let updated: serde_json::Value =
        serde_json::from_slice(&fs::read(f.receipt_path()).unwrap()).unwrap();
    assert_eq!(updated, receipt);
    assert!(!f.root.join("cargo.json").exists());
}

#[test]
fn up_to_date_preserves_receipt_newer_than_running_binary() {
    let f = Fixture::new();
    let mut receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(f.receipt_path()).unwrap()).unwrap();
    receipt["version"] = serde_json::json!("99.0.0");
    receipt["tag"] = serde_json::json!("v99.0.0");
    let before = serde_json::to_vec(&receipt).unwrap();
    fs::write(f.receipt_path(), &before).unwrap();
    let out = f.run();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).ends_with("Already up to date (99.0.0).\n"));
    assert_eq!(fs::read(f.receipt_path()).unwrap(), before);
    assert!(!f.root.join("cargo.json").exists());
}

#[test]
fn newer_builds_exact_arguments_validates_and_updates_receipt_and_cache() {
    let f = Fixture::new();
    let out = f.run();
    assert!(
        out.status.success(),
        "{} {}",
        text(&out),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text(&out).contains("Building 99.0.0 from source (this takes a few minutes)...\n"));
    assert!(text(&out).ends_with("Installed 99.0.0. Restart Rustrace to use it.\n"));
    assert!(text(&out).contains("fixture cargo stdout"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("fixture cargo stderr"));
    let call: serde_json::Value =
        serde_json::from_slice(&fs::read(f.root.join("cargo.json")).unwrap()).unwrap();
    assert_eq!(
        call["argv"],
        serde_json::json!([
            "+1.98.1",
            "install",
            "--git",
            "https://github.com/baochunli/rustrace",
            "--tag",
            "v99.0.0",
            "rustrace",
            "--locked",
            "--force"
        ])
    );
    assert_eq!(call["root"], serde_json::json!(f.home.root.join("install")));
    assert_eq!(call["auto_install"], "0");
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(f.receipt_path()).unwrap()).unwrap();
    assert_eq!(receipt["version"], "99.0.0");
    assert_eq!(receipt["tag"], "v99.0.0");
    let state = UpdateState::load(&f.state());
    assert!(!state.checks_enabled);
    assert_eq!(state.latest.unwrap().version, "99.0.0");
    assert!(
        f.home
            .command(&f.binary)
            .args(["--version", "--verbose"])
            .output()
            .unwrap()
            .status
            .success()
    );
}
#[test]
fn cargo_failure_leaves_binary_and_receipt_unchanged() {
    let f = Fixture::new();
    let binary = fs::read(&f.binary).unwrap();
    let receipt = fs::read(f.receipt_path()).unwrap();
    fs::write(f.root.join("cargo-mode"), "failure").unwrap();
    let out = f.run();
    assert!(!out.status.success());
    assert!(text(&out).contains("Update failed; the previous Rustrace is unchanged."));
    assert_eq!(fs::read(&f.binary).unwrap(), binary);
    assert_eq!(fs::read(f.receipt_path()).unwrap(), receipt);
}
#[test]
fn invalid_installed_identity_never_rewrites_receipt() {
    for mode in ["version", "target", "event", "package", "assignment"] {
        let f = Fixture::new();
        let receipt = fs::read(f.receipt_path()).unwrap();
        let binary = fs::read(&f.binary).unwrap();
        fs::write(f.root.join("cargo-mode"), mode).unwrap();
        let out = f.run();
        assert!(!out.status.success(), "accepted {mode}: {}", text(&out));
        assert!(text(&out).contains("Installed binary validation failed"));
        assert_ne!(
            fs::read(&f.binary).unwrap(),
            binary,
            "Cargo replaced the binary"
        );
        let disclosure = format!(
            "The newly built binary is already installed at {}, but its identity could not be confirmed.\n",
            f.binary.display()
        );
        assert!(text(&out).contains(&disclosure), "{}", text(&out));
        assert!(text(&out).contains("cargo +1.98.1 install --git https://github.com/baochunli/rustrace --tag v99.0.0 rustrace --locked --force\n"), "{}", text(&out));
        assert_eq!(fs::read(f.receipt_path()).unwrap(), receipt);
    }
}
#[test]
fn unsupported_installations_print_fresh_remedy_without_any_writes() {
    for mode in ["missing", "cargo", "wrong-path", "malformed"] {
        let f = Fixture::new();
        match mode {
            "missing" => fs::remove_file(f.receipt_path()).unwrap(),
            "wrong-path" => {
                let mut v: serde_json::Value =
                    serde_json::from_slice(&fs::read(f.receipt_path()).unwrap()).unwrap();
                v["path"] = serde_json::json!(f.home.root.join("other/bin/rustrace"));
                fs::write(f.receipt_path(), serde_json::to_vec(&v).unwrap()).unwrap();
            }
            "malformed" => fs::write(f.receipt_path(), "{}").unwrap(),
            _ => f.receipt(mode),
        }
        let state = fs::read(f.state()).unwrap();
        let before = fs::read(f.receipt_path()).ok();
        let out = f.run();
        assert_eq!(out.status.code(), Some(2));
        assert!(text(&out).contains("cargo +1.98.1 install --git https://github.com/baochunli/rustrace --tag v99.0.0 rustrace --locked --force"),"{}",text(&out));
        assert_eq!(fs::read(f.state()).unwrap(), state);
        assert_eq!(fs::read(f.receipt_path()).ok(), before);
        assert!(!f.root.join("cargo.json").exists());
        assert!(
            !f.state()
                .parent()
                .unwrap()
                .join(".update-state.lock")
                .exists()
        );
        assert_eq!(
            fs::read_to_string(f.root.join("requests.jsonl"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }
}
#[test]
fn unsupported_installations_print_placeholder_remedy_when_offline_without_writes() {
    for method in ["missing", "cargo"] {
        let f = Fixture::new();
        if method == "missing" {
            fs::remove_file(f.receipt_path()).unwrap();
        } else {
            f.receipt(method);
        }
        fs::write(f.root.join("mode"), "offline").unwrap();
        let binary = fs::read(&f.binary).unwrap();
        let state = fs::read(f.state()).unwrap();
        let receipt = fs::read(f.receipt_path()).ok();
        let out = f.run();
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
        assert_eq!(fs::read(&f.binary).unwrap(), binary);
        assert_eq!(fs::read(f.state()).unwrap(), state);
        assert_eq!(fs::read(f.receipt_path()).ok(), receipt);
        assert!(!f.root.join("cargo.json").exists());
        assert!(
            !f.state()
                .parent()
                .unwrap()
                .join(".update-state.lock")
                .exists()
        );
        assert_eq!(
            fs::read_to_string(f.root.join("requests.jsonl"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        let stdout = text(&out);
        let lines: Vec<_> = stdout.lines().collect();
        assert!(
            lines[1].starts_with("Update check unavailable:"),
            "{stdout}"
        );
        assert!(stdout.ends_with("cargo +1.98.1 install --git https://github.com/baochunli/rustrace --tag vX.Y.Z rustrace --locked --force\nSee docs/installation.md for installation instructions.\n"), "{stdout}");
        assert_eq!(lines.len(), 4, "{stdout}");
    }
}

#[test]
fn active_workspace_lock_refuses_before_network_or_state_writes() {
    use std::os::fd::AsRawFd;
    let f = Fixture::new();
    let workspace = f.home.root.join("workspace");
    fs::create_dir_all(workspace.join(".rustrace")).unwrap();
    let lock = fs::File::create(workspace.join(".rustrace/writer.lock")).unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let state = fs::read(f.state()).unwrap();
    let out = f.command().current_dir(&workspace).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(text(&out), "Quit Rustrace before updating.\n");
    assert!(!f.root.join("requests.jsonl").exists());
    assert_eq!(fs::read(f.state()).unwrap(), state);
}
// Bound the command independently of the updater's own deadlines, so a bad
// filesystem node cannot hang the integration test indefinitely.
fn bounded_output(mut command: Command) -> Output {
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("update hung while probing writer.lock");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

fn assert_nonregular_session_lock_is_ignored(mode: &str) {
    use std::{
        ffi::CString,
        os::{fd::AsRawFd, unix::ffi::OsStrExt},
    };
    let f = Fixture::new();
    f.manifest(env!("CARGO_PKG_VERSION"));
    let workspace = f.home.root.join("workspace");
    fs::create_dir_all(workspace.join(".rustrace")).unwrap();
    let path = workspace.join(".rustrace/writer.lock");
    let target = f.home.root.join("unrelated.lock");
    let lock = fs::File::create(&target).unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    if mode == "fifo" {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    } else {
        std::os::unix::fs::symlink(&target, &path).unwrap();
    }
    let mut command = f.command();
    command.current_dir(&workspace);
    let out = bounded_output(command);
    assert_eq!(out.status.code(), Some(0), "{mode}: {}", text(&out));
    assert!(!text(&out).contains("Quit Rustrace before updating."));
    assert!(f.root.join("requests.jsonl").exists());
    assert_eq!(fs::read(&target).unwrap(), b"");
}

#[test]
fn active_session_probe_ignores_fifo_without_hanging() {
    assert_nonregular_session_lock_is_ignored("fifo");
}

#[test]
fn active_session_probe_does_not_follow_locked_symlink() {
    assert_nonregular_session_lock_is_ignored("symlink");
}

#[test]
fn active_session_probe_stops_at_nearest_workspace_directory() {
    use std::os::fd::AsRawFd;
    let f = Fixture::new();
    f.manifest(env!("CARGO_PKG_VERSION"));
    let parent = f.home.root.join("parent");
    let workspace = parent.join("nested");
    fs::create_dir_all(parent.join(".rustrace")).unwrap();
    fs::create_dir_all(workspace.join(".rustrace")).unwrap();
    let lock = fs::File::create(parent.join(".rustrace/writer.lock")).unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    // The nearest directory has no lock; its open error must end the walk.
    let mut command = f.command();
    command.current_dir(&workspace);
    let out = bounded_output(command);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(!text(&out).contains("Quit Rustrace before updating."));
}

#[test]
fn concurrent_updaters_hold_lock_through_build_and_second_waits() {
    use std::time::{Duration, Instant};
    let f = Fixture::new();
    fs::write(f.root.join("cargo-mode"), "wait-failure").unwrap();
    let mut first = f
        .command()
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f.root.join("cargo.json").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let second = f
        .command()
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        fs::read_to_string(f.root.join("requests.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    fs::write(f.root.join("release-cargo"), "").unwrap();
    assert!(!first.wait().unwrap().success());
    let out = second.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert_eq!(
        fs::read_to_string(f.root.join("requests.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

fn copy_sources(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_sources(&entry.path(), &destination.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
        }
    }
}

fn build_session_fixture(f: &Fixture) -> PathBuf {
    let source = f.home.root.join("source");
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    copy_sources(&repository.join("src"), &source.join("src"));
    for name in ["editor", "journal", "model", "replay", "workspace"] {
        let relative = Path::new("crates").join(name);
        copy_sources(
            &repository.join(&relative).join("src"),
            &source.join(&relative).join("src"),
        );
        fs::copy(
            repository.join(&relative).join("Cargo.toml"),
            source.join(&relative).join("Cargo.toml"),
        )
        .unwrap();
    }
    let manifest = fs::read_to_string(repository.join("Cargo.toml"))
        .unwrap()
        .replace(
            "default-run = \"rustrace\"",
            "default-run = \"session-fixture\"\nautobins = false",
        )
        .replace("\nversion.workspace = true", "\nversion = \"99.0.0\"");
    fs::write(
        source.join("Cargo.toml"),
        format!("{manifest}\n[[bin]]\nname = \"session-fixture\"\npath = \"src/main.rs\"\n"),
    )
    .unwrap();
    fs::write(
        source.join("src/main.rs"),
        include_str!("support/update_session_main.rs"),
    )
    .unwrap();
    fs::write(
        source.join("src/manifest.toml"),
        include_bytes!("support/update_session_manifest.toml"),
    )
    .unwrap();
    fs::write(
        source.join("build.rs"),
        format!(
            "fn main() {{ println!(\"cargo:rustc-env=RUSTRACE_BUILD_ID={};{}\"); }}",
            "b".repeat(40),
            rustrace::version::version_metadata().target()
        ),
    )
    .unwrap();
    let lock = fs::read_to_string(repository.join("Cargo.lock"))
        .unwrap()
        .replace(
            &format!(
                "name = \"rustrace\"\nversion = \"{}\"",
                env!("CARGO_PKG_VERSION")
            ),
            "name = \"rustrace\"\nversion = \"99.0.0\"",
        );
    fs::write(source.join("Cargo.lock"), lock).unwrap();
    // Reuse only the caller's disposable build/cache directories. The fixture
    // binary has a distinct name so it cannot replace the tested CLI artifact.
    let mut build = f.home.command("cargo");
    build
        .args(["+1.98.1", "build", "--manifest-path"])
        .arg(source.join("Cargo.toml"))
        .args(["--bin", "session-fixture", "--locked", "--offline"])
        .env(
            "CARGO_HOME",
            std::env::var_os("CARGO_HOME").expect("run tests with temporary CARGO_HOME"),
        );
    let output = build.output().unwrap();
    assert!(
        output.status.success(),
        "fixture build: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| source.join("target"));
    target.join("debug/session-fixture")
}

#[test]
fn running_session_keeps_old_identity_and_updated_binary_records_new_identity() {
    use rustrace::session::ProductionSession;
    let f = Fixture::new();
    let replacement = build_session_fixture(&f);
    let old_root = f.home.root.join("old-workspace");
    let new_root = f.home.root.join("new-workspace");
    for root in [&old_root, &new_root] {
        fs::create_dir(root).unwrap();
        fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
    }
    let session = ProductionSession::start(
        &old_root,
        include_bytes!("support/update_session_manifest.toml"),
    )
    .unwrap();
    let old = session.metadata().clone();
    let out = f
        .command()
        .env("UPDATE_REPLACEMENT", replacement)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{} {}",
        text(&out),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(session.metadata().client_version, old.client_version);
    assert_eq!(session.metadata().build_identity, old.build_identity);
    session.finalize("student").unwrap();
    let preserved = ProductionSession::read_metadata(&old_root).unwrap();
    assert_eq!(preserved.client_version, old.client_version);
    assert_eq!(preserved.build_identity, old.build_identity);
    let out = f
        .home
        .command(&f.binary)
        .args(["--fixture-session"])
        .arg(&new_root)
        .env("CARGO_HOME", f.home.root.join("cargo"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let new = ProductionSession::read_metadata(&new_root).unwrap();
    assert_eq!(new.client_version, "99.0.0");
    assert_eq!(
        new.build_identity,
        format!(
            "{};{}",
            "b".repeat(40),
            rustrace::version::version_metadata().target()
        )
    );
    assert_ne!(new.build_identity, preserved.build_identity);
    assert_ne!(new.client_version, preserved.client_version);
}

#[test]
fn real_cargo_installs_tiny_local_tagged_crate() {
    let f = Fixture::new();
    let source = f.home.root.join("tiny-crate");
    fs::create_dir_all(source.join("src")).unwrap();
    fs::write(
        source.join("Cargo.toml"),
        "[package]\nname = \"rustrace\"\nversion = \"99.0.0\"\nedition = \"2024\"\n[workspace]\n",
    )
    .unwrap();
    let version = format!(
        "rustrace 99.0.0\nbuild commit: {}\nevent format: 1\npackage format: 1\nassignment format: 2\ntarget: {}",
        "a".repeat(40),
        rustrace::version::version_metadata().target()
    );
    fs::write(source.join("src/main.rs"),format!("fn main() {{ assert_eq!(std::env::args().skip(1).collect::<Vec<_>>(), [\"--version\", \"--verbose\"]); println!({version:?}); }}\n")).unwrap();
    fs::create_dir(f.home.root.join("cargo")).unwrap();
    let mut lock = f.home.command("cargo");
    lock.args(["+1.98.1", "generate-lockfile", "--offline"])
        .current_dir(&source)
        .env("CARGO_HOME", f.home.root.join("cargo"));
    let out = lock.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
        vec!["tag", "v99.0.0"],
    ] {
        let out = f
            .home
            .command("git")
            .args(args)
            .current_dir(&source)
            .env("CARGO_HOME", f.home.root.join("cargo"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let mut receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(f.receipt_path()).unwrap()).unwrap();
    receipt["repository"] = serde_json::json!(format!("file://{}", source.display()));
    fs::write(f.receipt_path(), serde_json::to_vec(&receipt).unwrap()).unwrap();
    let tooling = std::env::var_os("PATH").unwrap();
    let cargo = std::env::split_paths(&tooling)
        .map(|dir| dir.join("cargo"))
        .find(|path| path.is_file())
        .expect("real cargo required");
    fs::remove_file(f.root.join("cargo")).unwrap();
    std::os::unix::fs::symlink(cargo, f.root.join("cargo")).unwrap();
    let path = std::env::join_paths(
        std::iter::once(f.root.clone()).chain(std::env::split_paths(&tooling)),
    )
    .unwrap();
    // Fresh local Git checkout needs Cargo's offline mode disabled; there are
    // no dependencies, and both the manifest and Git source remain local.
    let out = f
        .command()
        .env("PATH", path)
        .env("CARGO_NET_OFFLINE", "false")
        .env("CARGO_TARGET_DIR", f.home.root.join("tiny-target"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{} {}",
        text(&out),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text(&out).contains("Installed 99.0.0. Restart Rustrace to use it."));
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(f.receipt_path()).unwrap()).unwrap();
    assert_eq!(receipt["version"], "99.0.0");
    let out = f
        .home
        .command(&f.binary)
        .args(["--version", "--verbose"])
        .env("CARGO_HOME", f.home.root.join("cargo"))
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(text(&out).starts_with("rustrace 99.0.0\n"));
}

#[test]
fn python_test_home_does_not_derive_tooling_paths_without_home() {
    let home = TestHome::new(false);
    let out = home
        .command("python3")
        .arg("-c")
        .arg(include_str!("support/test_home_unset.py"))
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support"))
        .env_remove("HOME")
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
