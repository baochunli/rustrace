use std::process::Command;

#[test]
fn release_contract_scripts() {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/release_contract.py"
        ))
        .output()
        .expect("run Python release contract tests");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new("sh")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/release_test.sh"
        ))
        .output()
        .expect("run shell release fixtures");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn workflow_keeps_source_releases_draft_and_dry_runs_in_actions() {
    let workflow = include_str!("../.github/workflows/release.yml");
    assert!(workflow.contains("tags: ['v*']"));
    assert!(workflow.contains("workflow_dispatch:"));
    let (proof_jobs, release_job) = workflow
        .split_once("\n  release:\n")
        .expect("final release job");
    assert!(!proof_jobs.contains("contents: write"));
    assert!(release_job.contains("contents: write"));
    assert!(proof_jobs.contains("./scripts/check-release-tag.sh"));
    assert!(proof_jobs.contains("./scripts/release.sh"));
    assert!(proof_jobs.contains("./scripts/verify-release.sh"));
    assert!(release_job.contains("./scripts/manifest.sh --verify"));
    let creation = release_job
        .split_once("      - name: Create draft release\n")
        .expect("draft creation step")
        .1;
    assert!(creation.contains("if: github.event_name == 'push'"));
    assert!(creation.contains("gh release create"));
    assert!(creation.contains("--draft --verify-tag"));
    assert!(creation.contains("dist/latest.json"));
    assert!(!creation.contains("dist/*"));
    assert!(!creation.contains(".tar.gz"));
}

#[test]
fn clean_machine_workflow_pins_runners_and_fixture_install() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow_path = root.join(".github/workflows/install-smoke.yml");
    let workflow = std::fs::read_to_string(&workflow_path).expect("clean-machine workflow");
    for required in [
        "workflow_dispatch:",
        "cron: '17 9 * * 1'",
        "runner: [macos-latest, ubuntu-latest, ubuntu-22.04]",
        "runs-on: ${{ matrix.runner }}",
        "contents: read",
        "sudo apt-get install -y build-essential pkg-config curl git gawk mawk",
        "./scripts/install-smoke.sh",
    ] {
        assert!(
            workflow.contains(required),
            "workflow must contain {required}"
        );
    }
    assert!(!workflow.contains("actions/cache"));
    assert!(!workflow.contains("secrets."));
    let script = std::fs::read_to_string(root.join("scripts/install-smoke.sh")).unwrap();
    for required in [
        "HOME=$smoke_root/home",
        "CARGO_HOME=$smoke_root/cargo",
        "RUSTUP_HOME=$smoke_root/rustup",
        "CARGO_TARGET_DIR=$smoke_root/target",
        "XDG_CONFIG_HOME=$smoke_root/config",
        "XDG_STATE_HOME=$smoke_root/state",
        "PATH=$smoke_root/tools:/usr/bin:/bin:/usr/sbin:/sbin",
        "command -v rustup",
        "command -v cargo",
        "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh",
        "./scripts/manifest.sh",
        "RUSTRACE_MANIFEST_URL=",
        "RUSTRACE_SOURCE_REPOSITORY=",
        "./scripts/install.sh",
        "--version --verbose",
        "update --check",
        "is up to date.",
        "du -sh",
        "GITHUB_STEP_SUMMARY",
        "SelectedAwkTests",
        "pathlib.Path.cwd().as_uri()",
        "git tag \"$tag\" \"$commit\"",
        "cmp \"$metadata/version.txt\" \"$smoke_root/installed-version.txt\"",
        "[ -s \"$SMOKE_REQUEST_LOG\" ]",
    ] {
        assert!(
            script.contains(required),
            "smoke script must contain {required}"
        );
    }
    match Command::new("actionlint").arg(&workflow_path).output() {
        Ok(output) => assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("actionlint is not installed; workflow lint skipped");
        }
        Err(error) => panic!("run actionlint: {error}"),
    }
}
