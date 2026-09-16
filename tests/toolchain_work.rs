#[path = "support/test_home.rs"]
mod test_home;
#[cfg(unix)]
#[test]
fn production_toolchain_discovery_records_and_displays_actual_startup_observations() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/toolchain_work.py"
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

#[cfg(unix)]
#[test]
fn production_discovery_excludes_rustup_logging_credentials() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/toolchain_logging.py"
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

#[cfg(unix)]
#[test]
fn bootstrap_rejects_legacy_before_official_selection_side_effects() {
    bootstrap_fixture("legacy");
}

#[cfg(unix)]
#[test]
fn modern_missing_official_selections_never_contact_distribution_servers() {
    bootstrap_fixture("modern");
}

#[cfg(unix)]
fn bootstrap_fixture(mode: &str) {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/toolchain_bootstrap.py"
        ))
        .arg(mode)
        .arg(std::env::current_exe().unwrap())
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

/// Subprocess-only discovery harness: no process-global environment mutation in tests.
#[test]
fn isolated_discovery_child() {
    let Some(root) = std::env::var_os("RUSTRACE_DISCOVERY_TEST_ROOT") else {
        return;
    };
    let output = std::env::var_os("RUSTRACE_DISCOVERY_TEST_OUTPUT").unwrap();
    let pin = std::env::var("RUSTRACE_DISCOVERY_TEST_PIN").ok();
    let report = rustrace::toolchain::discover(std::path::Path::new(&root), pin.as_deref());
    std::fs::write(output, serde_json::to_vec(&report).unwrap()).unwrap();
}

#[cfg(unix)]
#[test]
fn real_rustup_default_directory_environment_and_assignment_pin_selection() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/toolchain_selection.py"
        ))
        .arg(std::env::current_exe().unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
