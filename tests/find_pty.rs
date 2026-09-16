#![cfg(unix)]

#[path = "support/test_home.rs"]
mod test_home;
#[test]
fn find_panel_replaces_one_then_all_in_the_real_80x24_tui() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/find_pty.py"
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

#[test]
fn find_replace_all_over_vector_limit_in_the_real_80x24_tui() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/find_replace_limit_pty.py"
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
