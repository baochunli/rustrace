#![cfg(unix)]

#[path = "support/test_home.rs"]
mod test_home;

#[test]
fn install_notice_update_and_restart() {
    let home = test_home::TestHome::new(true);
    let replacement = replacement_cli(&home);
    let output = home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/install_update_e2e_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .arg(replacement)
        .env_remove("TMPDIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));
}

fn copy_sources(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let destination = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_sources(&path, &destination);
        } else {
            std::fs::copy(path, destination).unwrap();
        }
    }
}

/// Build the real CLI with a newer compiled version before the timed scenario.
/// A distinct binary name prevents replacing Cargo's integration-test CLI.
fn replacement_cli(home: &test_home::TestHome) -> std::path::PathBuf {
    use std::{fs, path::Path};
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = home.root.join("source");
    copy_sources(&repository.join("src"), &source.join("src"));
    copy_sources(&repository.join("crates"), &source.join("crates"));
    fs::copy(repository.join("build.rs"), source.join("build.rs")).unwrap();
    let original = env!("CARGO_PKG_VERSION");
    let major: u64 = original.split('.').next().unwrap().parse().unwrap();
    let version = format!("{}.0.0", major + 1);
    let original_manifest = fs::read_to_string(repository.join("Cargo.toml")).unwrap();
    let fixture_manifest =
        original_manifest.replace("default-run = \"rustrace\"", "autobins = false");
    assert_ne!(
        fixture_manifest, original_manifest,
        "default-run literal changed"
    );
    let manifest = fixture_manifest.replace(
        "\nversion.workspace = true",
        &format!("\nversion = \"{version}\""),
    );
    assert_ne!(
        manifest, fixture_manifest,
        "workspace version literal changed"
    );
    fs::write(
        source.join("Cargo.toml"),
        format!("{manifest}\n[[bin]]\nname = \"install-update-fixture\"\npath = \"src/main.rs\"\n"),
    )
    .unwrap();
    let original_lock = fs::read_to_string(repository.join("Cargo.lock")).unwrap();
    let lock = original_lock.replace(
        &format!("name = \"rustrace\"\nversion = \"{original}\""),
        &format!("name = \"rustrace\"\nversion = \"{version}\""),
    );
    assert_ne!(
        lock, original_lock,
        "lockfile package version literal changed"
    );
    fs::write(source.join("Cargo.lock"), lock).unwrap();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| source.join("target"));
    let output = home
        .command("cargo")
        .args(["+1.98.1", "build", "--manifest-path"])
        .arg(source.join("Cargo.toml"))
        .args(["--bin", "install-update-fixture", "--locked", "--offline"])
        .env(
            "CARGO_HOME",
            std::env::var_os("CARGO_HOME").expect("run with temporary CARGO_HOME"),
        )
        .env("CARGO_TARGET_DIR", &target)
        .env("RUSTUP_AUTO_INSTALL", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "replacement CLI build: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    target.join("debug/install-update-fixture")
}
