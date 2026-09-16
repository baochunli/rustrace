#![cfg(unix)]

#[path = "support/test_home.rs"]
mod test_home;
#[test]
fn pre_session_checks_finish_and_work_starts_offline_or_hanging() {
    let test_home = test_home::TestHome::new(true);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/update_work_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .env_remove("TMPDIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
