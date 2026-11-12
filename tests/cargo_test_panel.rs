//! Real Cargo coverage for source-discovered F4 tests and their journal evidence.
use rustrace::{
    session::{ProductionSession, TestCaseOutcome, create_bundle},
    verify::verify_path,
};
use std::{
    fs,
    time::{Duration, Instant},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "cargo-panel"
assignment_version = "v1"
title = "Cargo panel"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["Cargo.toml", "Cargo.lock", "src/**/*.rs", "tests/**/*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

#[test]
fn cargo_panel_runs_exact_tests_and_verifies_evidence() {
    let root =
        std::env::temp_dir().join(format!("rustrace-real-cargo-panel-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("work/src")).unwrap();
    fs::create_dir_all(root.join("work/tests")).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    let workspace = root.join("work");
    fs::write(workspace.join("Cargo.toml"), "[package]\nname = \"panel-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n").unwrap();
    fs::write(
        workspace.join("Cargo.lock"),
        "version = 4\n[[package]]\nname = \"panel-fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
    let source = b"#[test] fn pass() {}\n#[test] fn pass_longer() { panic!(\"must not run\"); }\nmod nested { #[test] fn fails() { panic!(\"failure output\"); } }\n#[test] #[ignore] fn ignored() {}\n#[cfg(any())] #[test] fn disabled() {}\n";
    fs::write(workspace.join("tests/public_cli.rs"), source).unwrap();
    let mut session = ProductionSession::start(&workspace, MANIFEST).unwrap();
    let cases = session.list_test_cases().unwrap();
    assert_eq!(cases.len(), 5);
    for (name, expected) in [
        ("cargo:public_cli::pass", "pass"),
        ("cargo:public_cli::nested::fails", "fail"),
        ("cargo:public_cli::ignored", "error"),
        ("cargo:public_cli::disabled", "error"),
    ] {
        let case = cases
            .iter()
            .find(|case| case.name() == name)
            .unwrap()
            .clone();
        session.start_test_case(case).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while session.command_active() {
            assert!(Instant::now() < deadline, "Cargo panel test deadline");
            session.poll_command().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        let result = session.take_test_case_result().unwrap();
        assert!(
            matches!(
                (&result.outcome, expected),
                (TestCaseOutcome::Pass, "pass")
                    | (TestCaseOutcome::Fail(_), "fail")
                    | (TestCaseOutcome::Error(_), "error")
            ),
            "{name}: {result:?}"
        );
    }
    // A listed case whose source changed must be refreshed before execution.
    fs::write(
        workspace.join("tests/public_cli.rs"),
        b"#[test] fn replacement() {}\n",
    )
    .unwrap();
    assert!(session.start_test_case(cases[0].clone()).is_err());
    // Restore initial source before finalizing; command evidence remains replayable.
    fs::write(workspace.join("tests/public_cli.rs"), source).unwrap();
    let receipt = session.finalize("student-1").unwrap();
    let bundle = create_bundle(&receipt, &root.join("submission.zip"))
        .unwrap()
        .path;
    let report = verify_path(&bundle, None);
    assert!(report.is_clean(), "{report:#?}");
}
