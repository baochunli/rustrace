#![cfg(unix)]

#[path = "support/test_home.rs"]
mod test_home;

#[test]
fn offline_source_installer() {
    let home = test_home::TestHome::new(false);
    let output = home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/install_sh_tests.py"
        ))
        .arg("InstallerTests")
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
