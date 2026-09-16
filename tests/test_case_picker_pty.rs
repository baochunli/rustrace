#![cfg(unix)]

#[path = "support/test_home.rs"]
mod test_home;
#[test]
fn packaged_test_case_picker_runs_pass_and_fail_serially_in_a_real_v2_workspace() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/test_case_picker_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
