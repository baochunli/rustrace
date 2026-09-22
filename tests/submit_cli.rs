#[path = "support/test_home.rs"]
mod test_home;
use rustrace::{
    session::ProductionSession,
    tui::EditorCommand,
    verify::{VerificationIssueKind, VerificationStatus, verify_path},
};
use rustrace_model::{RprovPackageState, RprovRecoveryGap};
use rustrace_workspace::rprov_import::import_rprov;
use std::{
    fs,
    io::{Cursor, Read},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Bundle CLI"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["**"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

struct Fixture {
    base: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new(prefix: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "rustrace-{prefix}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = base.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("main.rs"), "A").unwrap();
        Self {
            base: fs::canonicalize(base).unwrap(),
            workspace: fs::canonicalize(workspace).unwrap(),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[test]
fn submit_cli_builds_importable_bundle_at_explicit_output_without_opening_the_tui() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("submit-cli");
    ProductionSession::start(&fixture.workspace, MANIFEST)
        .unwrap()
        .finalize("student-1")
        .unwrap();
    let destination = fixture.base.join("submitted.zip");

    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("submit")
        .arg(&fixture.workspace)
        .arg("--student-id")
        .arg("student-1")
        .arg("--output")
        .arg(&destination)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(destination.to_str().unwrap()), "{stdout}");
    let hash = stdout.split_whitespace().next().unwrap();
    assert_eq!(hash.len(), 64);
    import_rprov(Cursor::new(fs::read(destination).unwrap())).unwrap();
}

#[test]
fn submit_cli_revision_corrects_student_id_without_changing_parent() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("submit-cli-correct-id");
    let parent = ProductionSession::start(&fixture.workspace, MANIFEST)
        .unwrap()
        .finalize("YOUR_UTORID")
        .unwrap();
    let child = fixture.base.join("revision");
    fs::create_dir(&child).unwrap();
    for (path, bytes) in parent.final_workspace() {
        fs::write(child.join(path.as_str()), bytes).unwrap();
    }
    ProductionSession::start_revision(&fixture.workspace, &child, MANIFEST)
        .unwrap()
        .quit()
        .unwrap();

    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("submit")
        .arg(&child)
        .args(["--student-id", "correct-id"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let destination = fixture.base.join("correct-id-assignment.zip");
    let imported = import_rprov(Cursor::new(fs::read(&destination).unwrap())).unwrap();
    assert_eq!(imported.manifest().student_id, "correct-id");
    assert_eq!(
        imported.manifest().package_state,
        RprovPackageState::CleanFinalized
    );
    assert_eq!(imported.manifest().segments.len(), 2);
    assert_eq!(
        imported.manifest().segments[0],
        parent.manifest().segments[0]
    );
    let report = verify_path(&destination, None);
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    let rustrace::session::FinalizationStatus::Finalized(unchanged) =
        ProductionSession::recover_finalization(&fixture.workspace).unwrap()
    else {
        panic!("parent must remain finalized")
    };
    assert_eq!(unchanged.manifest(), parent.manifest());
}

#[test]
fn submit_cli_refuses_a_student_id_that_differs_from_the_receipt() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("submit-cli-student-mismatch");
    ProductionSession::start(&fixture.workspace, MANIFEST)
        .unwrap()
        .finalize("student-1")
        .unwrap();
    let destination = fixture.base.join("must-not-exist.zip");

    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("submit")
        .arg(&fixture.workspace)
        .arg("--student-id")
        .arg("student-2")
        .arg("--output")
        .arg(&destination)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(!destination.exists());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("differs from finalized receipt student ID student-1")
    );
}

#[test]
fn submit_cli_uses_the_student_assignment_default_name_next_to_workspace() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("submit-cli-default");
    ProductionSession::start(&fixture.workspace, MANIFEST)
        .unwrap()
        .finalize("student-1")
        .unwrap();
    let destination = fixture.base.join("student-1-assignment.zip");

    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("submit")
        .arg(&fixture.workspace)
        .arg("--student-id")
        .arg("student-1")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains(destination.to_str().unwrap())
    );
    import_rprov(Cursor::new(fs::read(destination).unwrap())).unwrap();
}

#[test]
fn submit_cli_refuses_damaged_session_with_preservation_guidance() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("submit-cli-incomplete");
    ProductionSession::start(&fixture.workspace, MANIFEST)
        .unwrap()
        .quit()
        .unwrap();
    fs::write(fixture.workspace.join(".rustrace/session.json"), "{}").unwrap();
    let destination = fixture.base.join("must-not-exist.zip");

    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("submit")
        .arg(&fixture.workspace)
        .arg("--student-id")
        .arg("student-1")
        .arg("--output")
        .arg(&destination)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(!destination.exists());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("original preserved") && stdout.contains("Use --inspect or --abandon"),
        "{stdout}"
    );
}

#[test]
fn submit_cli_headlessly_finalizes_saved_session_and_reuses_identical_receipt() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("submit-cli-headless-finalize");
    let mut session = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
    session.execute(EditorCommand::Insert('B')).unwrap();
    session.save_all().unwrap();
    session.quit().unwrap();
    let first = fixture.base.join("first-finalized.zip");
    let second = fixture.base.join("second-finalized.zip");

    for destination in [&first, &second] {
        let output = test_home
            .command(env!("CARGO_BIN_EXE_rustrace"))
            .arg("submit")
            .arg(&fixture.workspace)
            .arg("--student-id")
            .arg("student-1")
            .arg("--output")
            .arg(destination)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    assert!(
        !fixture
            .workspace
            .join(".rustrace/finalization-incomplete.json")
            .exists(),
        "a clean headless finalization must not retain an incomplete-recovery label"
    );
    assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
    let imported = import_rprov(Cursor::new(fs::read(first).unwrap())).unwrap();
    let path = rustrace_model::WorkspacePath::new("main.rs").unwrap();
    let mut source = Vec::new();
    imported
        .open_outer_source(&path)
        .unwrap()
        .read_to_end(&mut source)
        .unwrap();
    assert_eq!(source, b"BA");
}

#[test]
fn submit_cli_exports_only_opted_in_representable_incomplete_recovery() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("submit-cli-incomplete-export");
    let mut session = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
    fs::write(fixture.workspace.join("main.rs"), "external bytes").unwrap();
    session.save_all().unwrap();
    fs::remove_file(&session.evidence_paths()[0]).unwrap();
    assert!(session.finalize("student-1").is_err());

    let refused_path = fixture.base.join("refused.zip");
    let refused = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("submit")
        .arg(&fixture.workspace)
        .arg("--student-id")
        .arg("student-1")
        .arg("--output")
        .arg(&refused_path)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(!refused_path.exists());
    let refused_text = String::from_utf8(refused.stdout).unwrap();
    assert!(
        refused_text.contains("--allow-incomplete"),
        "{refused_text}"
    );

    let first = fixture.base.join("incomplete-first.zip");
    let second = fixture.base.join("incomplete-second.zip");
    for destination in [&first, &second] {
        let output = test_home
            .command(env!("CARGO_BIN_EXE_rustrace"))
            .arg("submit")
            .arg(&fixture.workspace)
            .arg("--student-id")
            .arg("student-1")
            .arg("--allow-incomplete")
            .arg("--output")
            .arg(destination)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("INCOMPLETE RECOVERY EXPORT"), "{text}");
        assert!(text.contains("referenced evidence"), "{text}");
    }
    assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());

    let imported = import_rprov(Cursor::new(fs::read(&first).unwrap())).unwrap();
    let RprovPackageState::RecoveryIncomplete { gaps, .. } = &imported.manifest().package_state
    else {
        panic!("opted-in recovery export was not visibly incomplete");
    };
    assert!(
        gaps.iter()
            .all(|gap| matches!(gap, RprovRecoveryGap::MissingEvidence { .. }))
    );
    assert!(!gaps.is_empty());

    let report = verify_path(&first, None);
    assert_eq!(report.package_structure, VerificationStatus::Failed);
    assert!(!report.is_clean());
    assert!(report.issues.iter().any(|issue| {
        issue.kind == VerificationIssueKind::PackageStructure
            && issue.detail.contains("recovery-incomplete")
    }));
    let verified = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("verify")
        .arg(&first)
        .output()
        .unwrap();
    assert!(!verified.status.success());
    let verified_text = String::from_utf8(verified.stdout).unwrap();
    assert!(
        verified_text.contains("Package structure        FAILED")
            && verified_text.contains("INCOMPLETE RECOVERY")
            && verified_text.contains("cannot pass clean verification"),
        "{verified_text}"
    );

    let records: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.workspace.join(".rustrace/export-records.json")).unwrap(),
    )
    .unwrap();
    let records = records["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|record| record["incomplete"] == true));
}

#[test]
fn submit_cli_refuses_allow_incomplete_for_raw_only_capture() {
    let test_home = test_home::TestHome::new(false);
    let parent = Fixture::new("submit-cli-raw-parent");
    let mut first = ProductionSession::start(&parent.workspace, MANIFEST).unwrap();
    first.execute(EditorCommand::Insert('B')).unwrap();
    let parent_receipt = first.finalize("student-1").unwrap();

    let child = Fixture::new("submit-cli-raw-child");
    fs::remove_dir_all(&child.workspace).unwrap();
    fs::create_dir(&child.workspace).unwrap();
    for (path, bytes) in parent_receipt.final_workspace() {
        fs::write(child.workspace.join(path.as_str()), bytes).unwrap();
    }
    let mut second =
        ProductionSession::start_revision(&parent.workspace, &child.workspace, MANIFEST).unwrap();
    second.execute(EditorCommand::Insert('C')).unwrap();
    fs::write(
        child.workspace.join(".rustrace/parent.json"),
        b"damaged before capture",
    )
    .unwrap();
    assert!(second.finalize("student-1").is_err());

    let destination = child.base.join("must-not-exist.zip");
    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("submit")
        .arg(&child.workspace)
        .arg("--student-id")
        .arg("student-1")
        .arg("--allow-incomplete")
        .arg("--output")
        .arg(&destination)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!destination.exists());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("no bundle was created"), "{text}");
    assert!(!text.contains("INCOMPLETE RECOVERY EXPORT"), "{text}");
}
