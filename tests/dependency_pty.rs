#![cfg(unix)]

#[path = "support/test_home.rs"]
mod test_home;
use rustrace::{
    replay_tui::ReplayController,
    session::{ProductionSession, ResumeChoice, create_bundle},
    verify::verify_path,
};
use rustrace_model::{ControlledAction, EditOrigin, Event};
use std::{fs, path::PathBuf};

#[test]
fn real_cargo_add_and_locked_check_preserve_replay_and_verify_parity() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/dependency_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .env_remove("TMPDIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}; stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let root = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("ROOT="))
        .map(PathBuf::from)
        .expect("driver reports retained fixture root");
    let work = root.join("assignment.work");
    let manifest = fs::read(work.join(".rustrace/manifest.toml")).unwrap();
    let session_metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(work.join(".rustrace/session.json")).unwrap()).unwrap();
    let id =
        rustrace_model::SessionId::new(session_metadata["session_id"].as_str().unwrap()).unwrap();

    let inspection = ProductionSession::inspect(&work).unwrap();
    assert!(
        inspection.logical[&rustrace_model::WorkspacePath::new("Cargo.toml").unwrap()]
            .windows(4)
            .any(|bytes| bytes == b"itoa")
    );
    let session = ProductionSession::resume(&work, &manifest, ResumeChoice::Resume).unwrap();
    let receipt = session.finalize("student-1").unwrap();
    let bundle = root.join("dependency-pty.zip");
    create_bundle(&receipt, &bundle).unwrap();
    let report = verify_path(&bundle, None);
    assert!(report.is_clean(), "{report:?}");
    assert!(
        ReplayController::open(&bundle)
            .unwrap()
            .timeline_available()
    );

    let pinned = rustrace_workspace::hash::PinnedWorkspaceRoot::open(&work).unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .open_journal_file(&id)
        .unwrap();
    let mut journal =
        rustrace_journal::Journal::open_read_only_no_follow(owner.display_path()).unwrap();
    let events = journal.read_events(&id, 1, 1000).unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.event,
        Event::ControlledCommandStarted(start) if start.action == ControlledAction::Add
    )));
    assert!(events.iter().any(|event| matches!(
        &event.event,
        Event::FileEdited(transaction) if transaction.origin == EditOrigin::DependencyTool
    )));
    fs::remove_dir_all(root).unwrap();
}
