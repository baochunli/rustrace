use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rustrace_model::{SelectionState, WorkspacePath, document_hash};
use rustrace_workspace::hash::hash_entries;
use serde_json::Value;

#[path = "support/milestone_a_process.rs"]
mod milestone_a_process;

const HELPER_MODE: &str = "RUSTRACE_MILESTONE_A_HELPER_MODE";
const HELPER_DIRECTORY: &str = "RUSTRACE_MILESTONE_A_HELPER_DIRECTORY";
const HELPER_SESSION: &str = "RUSTRACE_MILESTONE_A_HELPER_SESSION";
const HELPER_MARKER: &str = "RUSTRACE_MILESTONE_A_HELPER_MARKER";
const HELPER_REPORT: &str = "RUSTRACE_MILESTONE_A_HELPER_REPORT";

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let serial = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rustrace-milestone-a-restart-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn record_exit_and_fresh_process_replay_every_intermediate_state() {
    let directory = TempDirectory::new();
    let marker = directory.path().join("source-executed");
    let session_id = "milestone-a-two-process";
    let record = run_helper("record", directory.path(), session_id, Some(&marker), None);
    assert!(
        record.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&record.stdout),
        String::from_utf8_lossy(&record.stderr),
    );
    assert!(
        !marker.exists(),
        "recording must not execute student source"
    );

    let report_path = directory.path().join("report.json");
    let verify = run_helper(
        "verify",
        directory.path(),
        session_id,
        None,
        Some(&report_path),
    );
    assert!(
        verify.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr),
    );
    assert!(!marker.exists(), "replay must not execute student source");

    let report: Value = serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
    assert_eq!(report["event_count"], 8);
    assert_eq!(report["checkpoint_count"], 2);
    assert_eq!(report["finalized"], true);
    let starter = format!(
        "fn main() {{ std::fs::write({:?}, b\"executed\").unwrap(); }}\n",
        marker
    );
    let pasted = "\n// héllo 🦀\n";
    let after_paste = format!("{starter}{pasted}");
    let final_text = format!("{after_paste}x");
    assert_eq!(
        fs::read(directory.path().join("main.rs")).unwrap(),
        final_text.as_bytes()
    );
    assert_eq!(report["source"], final_text);

    let starter_end = u64::try_from(starter.len()).unwrap();
    let pasted_end = u64::try_from(after_paste.len()).unwrap();
    let final_end = u64::try_from(final_text.len()).unwrap();
    let expected = [
        (starter.as_str(), 0, SelectionState::caret(0)),
        (starter.as_str(), 0, SelectionState::caret(starter_end)),
        (after_paste.as_str(), 1, SelectionState::caret(pasted_end)),
        (final_text.as_str(), 2, SelectionState::caret(final_end)),
        (after_paste.as_str(), 3, SelectionState::caret(pasted_end)),
        (final_text.as_str(), 4, SelectionState::caret(final_end)),
        (final_text.as_str(), 4, SelectionState::caret(final_end)),
        (final_text.as_str(), 4, SelectionState::caret(final_end)),
    ];
    let steps = report["steps"].as_array().unwrap();
    assert_eq!(steps.len(), expected.len());
    let path = WorkspacePath::new("main.rs").unwrap();
    for (index, (step, (text, version, selection))) in steps.iter().zip(expected).enumerate() {
        assert_eq!(step["sequence"], u64::try_from(index + 1).unwrap());
        assert_eq!(step["text"], text);
        assert_eq!(step["version"], version);
        assert_eq!(step["anchor_byte"], selection.anchor_byte);
        assert_eq!(step["active_byte"], selection.active_byte);
        assert_eq!(step["document_hash"], document_hash(text).to_string());
        assert_eq!(
            step["workspace_hash"],
            hash_entries([(&path, text.as_bytes())])
                .unwrap()
                .to_string()
        );
    }
}

fn run_helper(
    mode: &str,
    directory: &Path,
    session_id: &str,
    marker: Option<&Path>,
    report: Option<&Path>,
) -> std::process::Output {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "milestone_a_process_helper", "--nocapture"])
        .env(HELPER_MODE, mode)
        .env(HELPER_DIRECTORY, directory)
        .env(HELPER_SESSION, session_id)
        .env_remove(HELPER_MARKER)
        .env_remove(HELPER_REPORT);
    if let Some(marker) = marker {
        command.env(HELPER_MARKER, marker);
    }
    if let Some(report) = report {
        command.env(HELPER_REPORT, report);
    }
    command.output().unwrap()
}

#[test]
fn milestone_a_process_helper() {
    let Some(mode) = std::env::var_os(HELPER_MODE) else {
        return;
    };
    let directory = std::env::var_os(HELPER_DIRECTORY).expect("helper directory");
    let session_id = std::env::var_os(HELPER_SESSION).expect("helper session");
    let mut arguments = vec![mode, directory, session_id];
    if let Some(marker) = std::env::var_os(HELPER_MARKER) {
        arguments.push(marker);
    }
    let mut writer: Box<dyn std::io::Write> = match std::env::var_os(HELPER_REPORT) {
        Some(path) => Box::new(fs::File::create(path).unwrap()),
        None => Box::new(std::io::sink()),
    };
    milestone_a_process::run(arguments, &mut writer).unwrap();
}
