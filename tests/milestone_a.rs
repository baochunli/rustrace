use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use rustrace::editor::Movement;
use rustrace::milestone_a::{MilestoneError, OneFileSession, verify_and_replay};
use rustrace_journal::Journal;
use rustrace_model::{
    EditOrigin, Event, MAX_INSERTED_TEXT_BYTES, SelectionState, SessionId, WorkspacePath,
    document_hash,
};
use rustrace_workspace::hash::hash_entries;

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(label: &str) -> Self {
        let serial = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rustrace-milestone-a-{label}-{}-{serial}",
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

fn session_id(label: &str) -> SessionId {
    SessionId::new(label).unwrap()
}

#[test]
fn unicode_edit_session_crosses_a_fresh_connection_and_replays_every_state() {
    let directory = TempDirectory::new("complete");
    let session_id = session_id("milestone-a-complete");
    let starter = "fn main() {\n    println!(\"Hello\");\n}\n";
    let pasted = "\nfn greet() {\n    println!(\"héllo 🦀\");\n";
    let mut session = OneFileSession::start(directory.path(), session_id.clone(), starter).unwrap();

    assert_eq!(session.persisted_event_count(), 1);
    assert!(session.move_cursor(Movement::DocumentEnd, false).unwrap());
    assert!(session.paste(pasted).unwrap());
    assert!(session.insert_char('}').unwrap());
    assert!(session.undo().unwrap());
    assert!(session.redo().unwrap());

    let count_before_noops = session.persisted_event_count();
    assert!(!session.delete_forward().unwrap());
    assert!(matches!(
        session.paste(&"x".repeat(MAX_INSERTED_TEXT_BYTES + 1)),
        Err(MilestoneError::Editor(_))
    ));
    assert_eq!(session.persisted_event_count(), count_before_noops);

    let final_text = format!("{starter}{pasted}}}");
    let final_version = session.version();
    let final_selection = session.selection();
    let final_document_hash = session.document_hash();
    assert_eq!(session.text(), final_text);
    assert_eq!(final_version, 4);

    let finalized = session.finish().unwrap();
    assert_eq!(finalized.event_count(), 8);
    assert_eq!(
        fs::read(finalized.source_path()).unwrap(),
        final_text.as_bytes()
    );

    // This is a logical restart boundary: verification receives the immutable
    // receipt and opens a fresh Journal connection itself.
    let report = verify_and_replay(&finalized).unwrap();
    assert_eq!(report.event_count(), 8);
    assert_eq!(report.checkpoint_count(), 2);
    assert_eq!(report.steps().len(), report.event_count() as usize);
    assert!(report.finalized());

    let source_path = WorkspacePath::new("main.rs").unwrap();
    let after_paste = format!("{starter}{pasted}");
    let starter_end = u64::try_from(starter.len()).unwrap();
    let pasted_end = u64::try_from(after_paste.len()).unwrap();
    let final_end = u64::try_from(final_text.len()).unwrap();
    let expected_states = [
        (starter, 0, SelectionState::caret(0)),
        (starter, 0, SelectionState::caret(starter_end)),
        (after_paste.as_str(), 1, SelectionState::caret(pasted_end)),
        (final_text.as_str(), 2, SelectionState::caret(final_end)),
        (after_paste.as_str(), 3, SelectionState::caret(pasted_end)),
        (final_text.as_str(), 4, SelectionState::caret(final_end)),
        (final_text.as_str(), 4, SelectionState::caret(final_end)),
        (final_text.as_str(), 4, SelectionState::caret(final_end)),
    ];
    for (index, (step, (expected_text, expected_version, expected_selection))) in
        report.steps().iter().zip(expected_states).enumerate()
    {
        let expected_sequence = u64::try_from(index + 1).unwrap();
        let document = step.state.document(&session_id_to_document_id()).unwrap();
        assert_eq!(step.sequence, expected_sequence);
        assert_eq!(
            step.state.file(&source_path),
            Some(expected_text.as_bytes())
        );
        assert_eq!(document.text(), expected_text);
        assert_eq!(document.version(), expected_version);
        assert_eq!(document.selection(), expected_selection);
        assert_eq!(document.content_hash(), document_hash(expected_text));
        assert_eq!(
            step.state.workspace_hash(),
            hash_entries([(&source_path, expected_text.as_bytes())]).unwrap()
        );
    }

    let edit_steps: Vec<_> = report
        .steps()
        .iter()
        .filter_map(|step| match &step.event {
            Event::FileEdited(transaction) => Some((transaction, &step.state)),
            _ => None,
        })
        .collect();
    assert_eq!(edit_steps.len(), 4);
    assert_eq!(
        edit_steps
            .iter()
            .map(|(transaction, _)| transaction.origin)
            .collect::<Vec<_>>(),
        [
            EditOrigin::Paste,
            EditOrigin::Keyboard,
            EditOrigin::Undo,
            EditOrigin::Redo,
        ]
    );
    assert_eq!(
        edit_steps
            .iter()
            .map(|(transaction, _)| transaction.version_after)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert_eq!(
        edit_steps
            .iter()
            .map(|(_, state)| {
                state
                    .document(&session_id_to_document_id())
                    .unwrap()
                    .text()
                    .to_owned()
            })
            .collect::<Vec<_>>(),
        [
            after_paste.clone(),
            final_text.clone(),
            after_paste,
            final_text.clone(),
        ]
    );

    let final_state = report.final_state().unwrap();
    let document = final_state.document(&session_id_to_document_id()).unwrap();
    assert_eq!(final_state.file(&source_path), Some(final_text.as_bytes()));
    assert_eq!(document.text(), final_text);
    assert_eq!(document.version(), final_version);
    assert_eq!(document.selection(), final_selection);
    assert_eq!(document.content_hash(), final_document_hash);
    assert_eq!(document.content_hash(), document_hash(&final_text));
    assert_eq!(
        report.final_workspace_hash(),
        hash_entries([(&source_path, final_text.as_bytes())]).unwrap()
    );
    assert_eq!(report.source_bytes(), final_text.as_bytes());
}

#[test]
fn tampered_persisted_event_fails_closed_without_executing_source() {
    let directory = TempDirectory::new("tamper");
    let marker = directory.path().join("source-executed");
    let session_id = session_id("milestone-a-tamper");
    let dangerous_source = format!(
        "fn main() {{ std::fs::write({:?}, b\"executed\").unwrap(); }}\n",
        marker
    );
    let session =
        OneFileSession::start(directory.path(), session_id.clone(), &dangerous_source).unwrap();
    let finalized = session.finish().unwrap();

    let connection = Connection::open(finalized.journal_path()).unwrap();
    connection
        .execute(
            "UPDATE events SET payload = CAST('tampered' AS BLOB) \
             WHERE session_id = ?1 AND sequence = ?2",
            (
                session_id.as_str(),
                i64::try_from(finalized.event_count()).unwrap(),
            ),
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        verify_and_replay(&finalized),
        Err(MilestoneError::Integrity(_))
    ));
    assert!(
        !marker.exists(),
        "verification must never execute Rust source"
    );
}

#[test]
fn journal_write_failure_poisoning_prevents_false_finalization() {
    let directory = TempDirectory::new("write-failure");
    let session_id = session_id("milestone-a-write-failure");
    let starter = "fn main() {}\n";
    let mut session = OneFileSession::start(directory.path(), session_id.clone(), starter).unwrap();

    let connection = Connection::open(session.journal_path()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_event_appends \
             BEFORE INSERT ON events BEGIN \
             SELECT RAISE(ABORT, 'injected write failure'); \
             END;",
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        session.insert_char('x'),
        Err(MilestoneError::RecordingFailed { .. })
    ));

    let connection = Connection::open(session.journal_path()).unwrap();
    connection
        .execute_batch("DROP TRIGGER reject_event_appends;")
        .unwrap();
    drop(connection);

    assert!(matches!(
        session.finish(),
        Err(MilestoneError::RecordingFailed { .. })
    ));

    let mut journal = Journal::open(directory.path().join("session.sqlite3")).unwrap();
    let state = journal.inspect_session(&session_id).unwrap();
    assert!(!state.ended);
    assert_eq!(
        journal
            .verify_session_chain(&session_id)
            .unwrap()
            .event_count,
        1
    );
    assert!(matches!(
        journal.read_events(&session_id, 1, 2).unwrap()[0].event,
        Event::WorkspaceCheckpoint(_)
    ));
    assert_eq!(
        fs::read(directory.path().join("main.rs")).unwrap(),
        starter.as_bytes()
    );
}

fn session_id_to_document_id() -> rustrace_model::DocumentId {
    rustrace_model::DocumentId::new("main.rs").unwrap()
}

#[test]
fn select_all_is_one_replayable_selection_action() {
    let directory = TempDirectory::new("select-all");
    let session_id = session_id("milestone-a-select-all");
    let mut session =
        OneFileSession::start(directory.path(), session_id.clone(), "starter").unwrap();

    assert!(session.select_all().unwrap());
    assert_eq!(session.selection(), SelectionState::new(0, 7));
    assert_eq!(session.persisted_event_count(), 2);
    assert!(!session.select_all().unwrap());
    assert_eq!(session.persisted_event_count(), 2);
    assert!(session.paste("fn main() {}\n").unwrap());
    let finalized = session.finish().unwrap();

    let report = verify_and_replay(&finalized).unwrap();
    assert_eq!(report.event_count(), 5);
    assert_eq!(
        report.final_state().unwrap().active_document(),
        Some(&session_id_to_document_id())
    );
    assert_eq!(
        report
            .final_state()
            .unwrap()
            .document(&session_id_to_document_id())
            .unwrap()
            .text(),
        "fn main() {}\n"
    );
}
