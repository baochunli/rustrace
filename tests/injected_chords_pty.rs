#![cfg(all(unix, target_os = "macos"))]

#[path = "support/test_home.rs"]
mod test_home;
#[test]
fn ghostty_probe_controls_injected_chords_in_a_real_80x24_terminal() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/injected_chords_pty.py"
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
