#![cfg(unix)]

#[path = "support/test_home.rs"]
mod test_home;
#[test]
fn custom_theme_file_controls_colours_in_a_real_80x24_terminal() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/theme_config_pty.py"
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
