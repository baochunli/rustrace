#[path = "support/test_home.rs"]
mod test_home;
use std::{collections::BTreeSet, fs, path::Path};

const PRE_MOVE_BUILD_COMMIT: &str = "4a8bc65e670f14e711dc87b395a04cdafdb2352a";

#[test]
fn repository_root_is_the_rustrace_package_and_only_lists_crate_members() {
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|path| path.join("Cargo.lock").is_file() && path.join("crates").is_dir())
        .expect("repository root");
    let manifest: toml::Value = fs::read_to_string(repository_root.join("Cargo.toml"))
        .expect("read root manifest")
        .parse()
        .expect("parse root manifest");

    assert_eq!(manifest["package"]["name"].as_str(), Some("rustrace"));
    assert_eq!(
        manifest["package"]["default-run"].as_str(),
        Some("rustrace")
    );
    assert!(
        repository_root.join("src/main.rs").is_file(),
        "the implicit rustrace binary must live at src/main.rs"
    );

    let actual = manifest["workspace"]["members"]
        .as_array()
        .expect("workspace members")
        .iter()
        .map(|member| member.as_str().expect("string workspace member"))
        .collect::<BTreeSet<_>>();
    let expected = BTreeSet::from([
        "crates/editor",
        "crates/journal",
        "crates/model",
        "crates/replay",
        "crates/workspace",
    ]);
    assert_eq!(actual, expected);
}

#[test]
fn verbose_version_pins_cargo_version_formats_target_and_one_build_commit() {
    let test_home = test_home::TestHome::new(false);
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        host => panic!("unsupported T10.25 validation host: {host:?}"),
    };
    let expected = format!(
        "rustrace {}\nbuild commit: {PRE_MOVE_BUILD_COMMIT}\nevent format: 1\npackage format: 1\nassignment format: 2\ntarget: {target}\n",
        env!("CARGO_PKG_VERSION")
    );
    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .args(["--version", "--verbose"])
        .output()
        .expect("run rustrace --version --verbose");
    assert!(
        output.status.success(),
        "version command failed: {output:?}"
    );
    let current = String::from_utf8(output.stdout).expect("version output is UTF-8");

    let stable_lines = |text: &str| {
        text.lines()
            .filter(|line| !line.starts_with("build commit: "))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(stable_lines(&current), stable_lines(&expected));

    let build_lines = current
        .lines()
        .filter(|line| line.starts_with("build commit: "))
        .collect::<Vec<_>>();
    assert_eq!(build_lines.len(), 1, "verbose output: {current:?}");
}
