#![cfg(unix)]
#[path = "support/test_home.rs"]
mod test_home;
use rustrace::update::UpdateState;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};
use test_home::TestHome;

struct Fixture {
    home: TestHome,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let home = TestHome::new(false);
        let root = home.root.clone();
        fs::remove_file(root.join("state/rustrace/update-state.json")).unwrap();
        fs::write(
            root.join("curl"),
            format!(
                "#!/usr/bin/python3\n{}",
                include_str!("support/update_curl.py")
            ),
        )
        .unwrap();
        fs::set_permissions(root.join("curl"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.join("mode"), "success").unwrap();
        Self { home, root }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = self.home.command(env!("CARGO_BIN_EXE_rustrace"));
        cmd.args(args)
            .env("PATH", &self.root)
            .env("CARGO_HOME", self.root.join("cargo"))
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"));
        cmd
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn state(&self) -> PathBuf {
        self.root.join("state/rustrace/update-state.json")
    }
    fn manifest(&self, version: &str) {
        let value = serde_json::json!({"schema_version":1,"version":version,"tag":format!("v{version}"), "commit":"a".repeat(40),"event_format":1,"package_format":1,"assignment_format":2,"source":{"repository":"https://github.com/baochunli/rustrace","tag":format!("v{version}")},"targets":{}});
        fs::write(
            self.root.join("latest.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    }
}
fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn explicit_check_pins_equal_older_newer_and_bypasses_throttle_and_disabled() {
    let f = Fixture::new();
    let installed = env!("CARGO_PKG_VERSION");
    let state = UpdateState {
        checks_enabled: false,
        ..UpdateState::default()
    };
    state.save(&f.state()).unwrap();
    for (version, expected) in [
        (installed, format!("Rustrace {installed} is up to date.\n")),
        (
            "99.0.0",
            format!(
                "Rustrace v99.0.0 is available (installed {installed}). Run: rustrace update\n"
            ),
        ),
        ("0.0.0", format!("Rustrace {installed} is up to date.\n")),
    ] {
        f.manifest(version);
        let output = f.run(&["update", "--check"]);
        assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
        assert_eq!(stdout(&output), expected);
        let state = UpdateState::load(&f.state());
        assert_eq!(state.latest.unwrap().version, version);
        assert!(!state.checks_enabled);
        assert!(state.last_success.is_some());
    }
    let requests: Vec<serde_json::Value> = fs::read_to_string(f.root.join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(requests.len(), 3);
    for request in requests {
        assert_eq!(request["argv"][6], "20");
        assert_eq!(request["argv"][9], rustrace::update::ENDPOINT);
    }
}

#[test]
fn unavailable_is_exit_two_and_preserves_previous_valid_cache() {
    let f = Fixture::new();
    f.manifest("99.0.0");
    assert!(f.run(&["update", "--check"]).status.success());
    for (mode, body, expected) in [
        ("success", "bad JSON", "invalid release manifest"),
        ("offline", "{}", "fixture offline"),
    ] {
        fs::write(f.root.join("mode"), mode).unwrap();
        fs::write(f.root.join("latest.json"), body).unwrap();
        let output = f.run(&["update", "--check"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(stdout(&output).starts_with("Update check unavailable: "));
        assert!(stdout(&output).contains(expected));
        assert_eq!(
            UpdateState::load(&f.state()).latest.unwrap().version,
            "99.0.0"
        );
    }
    fs::remove_file(f.root.join("curl")).unwrap();
    let output = f.run(&["update", "--check"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stdout(&output),
        "Update check unavailable: curl is not installed\n"
    );
    assert_eq!(
        UpdateState::load(&f.state()).latest.unwrap().version,
        "99.0.0"
    );
}

#[test]
fn help_version_and_bad_options_remain_offline() {
    let f = Fixture::new();
    for args in [
        vec![],
        vec!["--help"],
        vec!["--version"],
        vec!["--version", "--verbose"],
        vec!["update", "--bad"],
        vec!["update", "--check", "--check"],
    ] {
        let output = f.run(&args);
        if args != ["--version"] && args != ["--version", "--verbose"] {
            assert_eq!(output.status.code(), Some(2));
            assert!(stdout(&output).contains("rustrace update | rustrace update --check"));
        }
    }
    assert!(!f.state().exists());
    assert!(!f.root.join("requests.jsonl").exists());
}

#[test]
fn explicit_contention_waits_for_full_twenty_second_budget_without_fetching() {
    use std::{
        os::fd::AsRawFd,
        time::{Duration, Instant},
    };
    let fixture = Fixture::new();
    let path = fixture.root.join("state/rustrace/update-state.json");
    UpdateState::default().save(&path).unwrap();
    let before = fs::read(&path).unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.parent().unwrap().join(".update-state.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let started = Instant::now();
    let output = fixture.run(&["update", "--check"]);
    assert!(started.elapsed() >= Duration::from_secs(20));
    assert!(started.elapsed() < Duration::from_secs(22));
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Update check unavailable: another update check is running\n"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(
        !fixture.root.join("requests.jsonl").exists(),
        "curl invoked while lock held"
    );
}
