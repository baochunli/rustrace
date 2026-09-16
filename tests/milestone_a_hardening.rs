use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;
use rustrace::editor::Movement;
use rustrace::milestone_a::{
    MAX_MILESTONE_EVENTS, MAX_REPLAY_RETAINED_EVENT_BYTES, MAX_REPLAY_RETAINED_STATE_BYTES,
    MilestoneError, OneFileSession, verify_and_replay, verify_and_replay_from_directory,
};
use rustrace_journal::{CheckpointFile, CheckpointSnapshot, Journal, OpenDocument};
use rustrace_model::{
    DocumentId, EditOrigin, EditorTransaction, Event, EventEnvelope, FORMAT_VERSION_V1,
    FileFocused, Hash, SelectionChanged, SelectionState, SessionId, SubmissionFinalized, TextEdit,
    WorkspacePath, document_hash,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);
static CURRENT_DIRECTORY_LOCK: Mutex<()> = Mutex::new(());

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(label: &str) -> Self {
        let serial = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rustrace-milestone-a-hardening-{label}-{}-{serial}",
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

struct CurrentDirectory {
    original: PathBuf,
    _lock: MutexGuard<'static, ()>,
}

impl CurrentDirectory {
    fn change_to(path: &Path) -> Self {
        let lock = CURRENT_DIRECTORY_LOCK.lock().unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self {
            original,
            _lock: lock,
        }
    }
}

impl Drop for CurrentDirectory {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.original).unwrap();
    }
}

fn session_id(label: &str) -> SessionId {
    SessionId::new(label).unwrap()
}

fn document_id() -> DocumentId {
    DocumentId::new("main.rs").unwrap()
}

fn workspace_path() -> WorkspacePath {
    WorkspacePath::new("main.rs").unwrap()
}

#[test]
fn oversized_starter_is_rejected_before_creating_artifacts() {
    let directory = TempDirectory::new("starter-limit");
    let oversized = "x".repeat(rustrace_workspace::hash::MAX_WORKSPACE_FILE_BYTES as usize + 1);

    assert!(matches!(
        OneFileSession::start(
            directory.path(),
            session_id("oversized-starter"),
            &oversized
        ),
        Err(MilestoneError::LimitExceeded {
            resource: "source bytes",
            ..
        })
    ));
    assert!(!directory.path().join("main.rs").exists());
    assert!(!directory.path().join("session.sqlite3").exists());
}

#[test]
fn prospective_over_limit_edits_are_rejected_before_mutation_or_persistence() {
    let directory = TempDirectory::new("edit-limit");
    let limit = rustrace_workspace::hash::MAX_WORKSPACE_FILE_BYTES as usize;
    let starter = "x".repeat(limit);
    let mut session =
        OneFileSession::start(directory.path(), session_id("edit-limit"), &starter).unwrap();

    let before_count = session.persisted_event_count();
    assert!(matches!(
        session.insert_char('y'),
        Err(MilestoneError::LimitExceeded {
            resource: "source bytes",
            attempted,
            maximum,
        }) if attempted == limit as u64 + 1 && maximum == limit as u64
    ));
    assert_eq!(session.text(), starter);
    assert_eq!(session.version(), 0);
    assert_eq!(session.persisted_event_count(), before_count);
}

#[test]
fn replacement_and_undo_redo_respect_the_exact_file_size_boundary() {
    let directory = TempDirectory::new("history-limit");
    let limit = rustrace_workspace::hash::MAX_WORKSPACE_FILE_BYTES as usize;
    let starter = "x".repeat(limit);
    let mut session =
        OneFileSession::start(directory.path(), session_id("history-limit"), &starter).unwrap();

    assert!(session.move_cursor(Movement::Right, true).unwrap());
    let before_rejected_replace = session.persisted_event_count();
    assert!(matches!(
        session.paste("yz"),
        Err(MilestoneError::LimitExceeded {
            resource: "source bytes",
            ..
        })
    ));
    assert_eq!(session.text(), starter);
    assert_eq!(session.selection(), SelectionState::new(0, 1));
    assert_eq!(session.persisted_event_count(), before_rejected_replace);

    assert!(session.paste("y").unwrap());
    assert_eq!(session.text().len(), limit);
    assert!(session.undo().unwrap());
    assert_eq!(session.text(), starter);
    assert!(session.redo().unwrap());
    assert_eq!(session.text().len(), limit);
}

#[test]
fn relative_workspace_is_anchored_across_current_directory_changes() {
    let parent = TempDirectory::new("relative-parent");
    let elsewhere = TempDirectory::new("relative-elsewhere");
    let cwd = CurrentDirectory::change_to(parent.path());
    let session_id = session_id("relative-anchor");
    let mut session = OneFileSession::start("workspace", session_id, "fn main() {}\n").unwrap();
    assert!(session.source_path().is_absolute());

    std::env::set_current_dir(elsewhere.path()).unwrap();
    assert!(session.select_all().unwrap());
    assert!(
        session
            .paste("fn main() { println!(\"anchored\"); }\n")
            .unwrap()
    );
    let finalized = session.finish().unwrap();
    let report = verify_and_replay(&finalized).unwrap();

    assert_eq!(
        fs::read(parent.path().join("workspace/main.rs")).unwrap(),
        report.source_bytes()
    );
    assert!(!elsewhere.path().join("workspace").exists());
    drop(cwd);
}

#[cfg(unix)]
#[test]
fn source_symlink_substitution_never_modifies_the_external_target() {
    use std::os::unix::fs::symlink;

    let directory = TempDirectory::new("source-symlink");
    let external_directory = TempDirectory::new("external-target");
    let external = external_directory.path().join("external.rs");
    fs::write(&external, b"external must remain unchanged").unwrap();
    let session = OneFileSession::start(
        directory.path(),
        session_id("source-symlink"),
        "fn main() {}\n",
    )
    .unwrap();
    let original = directory.path().join("original-main.rs");
    fs::rename(session.source_path(), &original).unwrap();
    symlink(&external, session.source_path()).unwrap();

    assert!(matches!(
        session.finish(),
        Err(MilestoneError::UnsafeFilesystemEntry { .. })
            | Err(MilestoneError::FilesystemIdentityMismatch { .. })
    ));
    assert_eq!(
        fs::read(&external).unwrap(),
        b"external must remain unchanged"
    );
    assert_eq!(fs::read(&original).unwrap(), b"fn main() {}\n");
}

#[test]
fn source_non_regular_replacement_fails_without_opening_it() {
    let directory = TempDirectory::new("source-non-regular");
    let session = OneFileSession::start(
        directory.path(),
        session_id("source-non-regular"),
        "fn main() {}\n",
    )
    .unwrap();
    let original = directory.path().join("original-main.rs");
    fs::rename(session.source_path(), &original).unwrap();
    fs::create_dir(session.source_path()).unwrap();

    assert!(matches!(
        session.finish(),
        Err(MilestoneError::UnsafeFilesystemEntry { .. })
            | Err(MilestoneError::FilesystemIdentityMismatch { .. })
    ));
    assert_eq!(fs::read(&original).unwrap(), b"fn main() {}\n");
}

#[test]
fn journal_replacement_is_detected_before_marking_the_session_ended() {
    let directory = TempDirectory::new("journal-replacement");
    let session_id = session_id("journal-replacement");
    let session =
        OneFileSession::start(directory.path(), session_id.clone(), "fn main() {}\n").unwrap();
    let original = directory.path().join("original-session.sqlite3");
    fs::rename(session.journal_path(), &original).unwrap();
    fs::write(session.journal_path(), b"replacement").unwrap();

    assert!(matches!(
        session.finish(),
        Err(MilestoneError::FilesystemIdentityMismatch { .. })
            | Err(MilestoneError::UnsafeFilesystemEntry { .. })
    ));

    fs::remove_file(directory.path().join("session.sqlite3")).unwrap();
    fs::rename(&original, directory.path().join("session.sqlite3")).unwrap();
    let journal = Journal::open(directory.path().join("session.sqlite3")).unwrap();
    assert!(!journal.inspect_session(&session_id).unwrap().ended);
}

#[test]
fn replacing_finalized_source_or_journal_invalidates_the_verification_receipt() {
    let source_directory = TempDirectory::new("verify-source-replacement");
    let source_session = OneFileSession::start(
        source_directory.path(),
        session_id("verify-source-replacement"),
        "fn main() {}\n",
    )
    .unwrap()
    .finish()
    .unwrap();
    let original_source = source_directory.path().join("original-main.rs");
    fs::rename(source_session.source_path(), &original_source).unwrap();
    fs::write(source_session.source_path(), b"fn replacement() {}\n").unwrap();
    assert!(matches!(
        verify_and_replay(&source_session),
        Err(MilestoneError::FilesystemIdentityMismatch { .. })
    ));

    let journal_directory = TempDirectory::new("verify-journal-replacement");
    let journal_session = OneFileSession::start(
        journal_directory.path(),
        session_id("verify-journal-replacement"),
        "fn main() {}\n",
    )
    .unwrap()
    .finish()
    .unwrap();
    let original_journal = journal_directory.path().join("original-session.sqlite3");
    fs::rename(journal_session.journal_path(), &original_journal).unwrap();
    fs::copy(&original_journal, journal_session.journal_path()).unwrap();
    assert!(matches!(
        verify_and_replay(&journal_session),
        Err(MilestoneError::FilesystemIdentityMismatch { .. })
    ));
}

#[test]
fn recording_event_budget_is_reserved_for_checkpoint_and_finalization() {
    let directory = TempDirectory::new("recording-event-budget");
    let mut session =
        OneFileSession::start(directory.path(), session_id("recording-budget"), "a").unwrap();

    while session.persisted_event_count() < MAX_MILESTONE_EVENTS - 2 {
        if session.selection().is_caret() {
            assert!(session.select_all().unwrap());
        } else {
            assert!(session.move_cursor(Movement::DocumentEnd, false).unwrap());
        }
    }
    let selection = session.selection();
    assert!(matches!(
        if selection.is_caret() {
            session.select_all()
        } else {
            session.move_cursor(Movement::DocumentEnd, false)
        },
        Err(MilestoneError::LimitExceeded {
            resource: "replay events",
            ..
        })
    ));
    assert_eq!(session.selection(), selection);
    assert_eq!(session.persisted_event_count(), MAX_MILESTONE_EVENTS - 2);

    let finalized = session.finish().unwrap();
    assert_eq!(finalized.event_count(), MAX_MILESTONE_EVENTS);
    assert_eq!(
        verify_and_replay(&finalized).unwrap().steps().len() as u64,
        MAX_MILESTONE_EVENTS
    );
}

#[test]
fn recording_report_byte_budget_is_checked_before_selection_mutation() {
    let directory = TempDirectory::new("recording-report-budget");
    let file_limit = rustrace_workspace::hash::MAX_WORKSPACE_FILE_BYTES as usize;
    let starter = "x".repeat(file_limit);
    let mut session = OneFileSession::start(
        directory.path(),
        session_id("recording-report-budget"),
        &starter,
    )
    .unwrap();

    loop {
        let selection = session.selection();
        let result = if selection.is_caret() {
            session.select_all()
        } else {
            session.move_cursor(Movement::DocumentEnd, false)
        };
        if matches!(
            result,
            Err(MilestoneError::LimitExceeded {
                resource: "replay retained state bytes",
                ..
            })
        ) {
            assert_eq!(session.selection(), selection);
            break;
        }
        assert!(result.unwrap());
    }
    let finalized = session.finish().unwrap();
    let report = verify_and_replay(&finalized).unwrap();
    assert!(report.retained_state_bytes() <= MAX_REPLAY_RETAINED_STATE_BYTES);
}

#[test]
fn fresh_verification_rejects_a_self_consistent_over_budget_event_stream() {
    let directory = TempDirectory::new("verification-event-budget");
    let session_id = session_id("verification-event-budget");
    let finalized = OneFileSession::start(directory.path(), session_id.clone(), "a")
        .unwrap()
        .finish()
        .unwrap();
    rewrite_with_selection_events(
        &finalized,
        &session_id,
        usize::try_from(MAX_MILESTONE_EVENTS).unwrap(),
    );

    assert!(matches!(
        verify_and_replay(&finalized),
        Err(MilestoneError::LimitExceeded {
            resource: "replay events",
            ..
        })
    ));
}

#[test]
fn fresh_verification_checks_aggregate_bytes_before_retaining_each_step() {
    let directory = TempDirectory::new("verification-report-budget");
    let session_id = session_id("verification-report-budget");
    let starter = "x".repeat(rustrace_workspace::hash::MAX_WORKSPACE_FILE_BYTES as usize);
    let finalized = OneFileSession::start(directory.path(), session_id.clone(), &starter)
        .unwrap()
        .finish()
        .unwrap();
    let excessive_steps = usize::try_from(
        MAX_REPLAY_RETAINED_STATE_BYTES / rustrace_workspace::hash::MAX_WORKSPACE_FILE_BYTES + 1,
    )
    .unwrap();
    rewrite_with_selection_events(&finalized, &session_id, excessive_steps);

    assert!(matches!(
        verify_and_replay(&finalized),
        Err(MilestoneError::LimitExceeded {
            resource: "replay retained state bytes",
            ..
        })
    ));
}

#[test]
fn startup_failure_removes_only_the_source_created_by_that_invocation() {
    let directory = TempDirectory::new("startup-cleanup");
    let existing_journal = directory.path().join("session.sqlite3");
    let sentinel = directory.path().join("keep-me");
    fs::write(&existing_journal, b"preexisting journal").unwrap();
    fs::write(&sentinel, b"preexisting sentinel").unwrap();

    assert!(
        OneFileSession::start(
            directory.path(),
            session_id("startup-cleanup"),
            "fn main() {}\n"
        )
        .is_err()
    );
    assert!(!directory.path().join("main.rs").exists());
    assert_eq!(fs::read(existing_journal).unwrap(), b"preexisting journal");
    assert_eq!(fs::read(sentinel).unwrap(), b"preexisting sentinel");
}

#[test]
fn early_event_limit_precedes_chain_decode_and_preserves_evidence() {
    let directory = TempDirectory::new("early-event-limit");
    let session_id = session_id("early-event-limit");
    let finalized = OneFileSession::start(directory.path(), session_id.clone(), "a")
        .unwrap()
        .finish()
        .unwrap();
    let connection = Connection::open(finalized.journal_path()).unwrap();
    connection
        .execute(
            "UPDATE sessions SET next_sequence = ?1 WHERE session_id = ?2",
            (
                i64::try_from(MAX_MILESTONE_EVENTS + 2).unwrap(),
                session_id.as_str(),
            ),
        )
        .unwrap();
    drop(connection);
    let before = fs::read(finalized.journal_path()).unwrap();

    assert!(matches!(
        verify_and_replay_from_directory(directory.path(), &session_id),
        Err(MilestoneError::LimitExceeded {
            resource: "replay events",
            ..
        })
    ));
    assert_eq!(fs::read(finalized.journal_path()).unwrap(), before);
}

#[test]
fn hostile_large_events_hit_the_retained_event_budget_before_replay() {
    let directory = TempDirectory::new("verification-event-bytes");
    let session_id = session_id("verification-event-bytes");
    let finalized = OneFileSession::start(directory.path(), session_id.clone(), "a")
        .unwrap()
        .finish()
        .unwrap();
    rewrite_with_large_replacement_events(&finalized, &session_id, 96, 192 * 1024);

    assert!(matches!(
        verify_and_replay_from_directory(directory.path(), &session_id),
        Err(MilestoneError::LimitExceeded {
            resource: "replay retained event bytes",
            attempted,
            maximum,
        }) if attempted > maximum && maximum == MAX_REPLAY_RETAINED_EVENT_BYTES
    ));
}

#[test]
fn fresh_verification_rejects_events_outside_the_one_file_slice() {
    let directory = TempDirectory::new("unsupported-event");
    let session_id = session_id("unsupported-event");
    let finalized = OneFileSession::start(directory.path(), session_id.clone(), "a")
        .unwrap()
        .finish()
        .unwrap();
    rewrite_with_one_ordinary_event(
        &finalized,
        &session_id,
        Event::FileFocused(FileFocused {
            document_id: document_id(),
        }),
    );

    assert!(matches!(
        verify_and_replay_from_directory(directory.path(), &session_id),
        Err(MilestoneError::UnsupportedMilestoneEvent {
            sequence: 2,
            event: "file_focused",
        })
    ));
}

#[test]
fn replay_report_accessors_are_non_panicking() {
    let directory = TempDirectory::new("report-accessors");
    let finalized = OneFileSession::start(
        directory.path(),
        session_id("report-accessors"),
        "fn main() {}\n",
    )
    .unwrap()
    .finish()
    .unwrap();
    let report = verify_and_replay(&finalized).unwrap();

    assert_eq!(report.steps().len(), report.event_count() as usize);
    assert!(report.final_state().is_some());
    assert_eq!(
        report.final_state().unwrap().file(&workspace_path()),
        Some(report.source_bytes())
    );
}

fn rewrite_with_selection_events(
    finalized: &rustrace::milestone_a::FinalizedSession,
    session_id: &SessionId,
    selection_event_count: usize,
) {
    let connection = Connection::open(finalized.journal_path()).unwrap();
    connection
        .execute(
            "DELETE FROM checkpoints WHERE session_id = ?1 AND sequence >= 2",
            [session_id.as_str()],
        )
        .unwrap();
    connection
        .execute(
            "DELETE FROM events WHERE session_id = ?1 AND sequence >= 2",
            [session_id.as_str()],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE sessions SET next_sequence = 2, ended = 0 WHERE session_id = ?1",
            [session_id.as_str()],
        )
        .unwrap();
    drop(connection);

    let source = fs::read(finalized.source_path()).unwrap();
    let mut journal = Journal::open(finalized.journal_path()).unwrap();
    let genesis = journal.read_events(session_id, 1, 1).unwrap().remove(0);
    let mut previous_hash = genesis.event_hash;
    let mut selection = SelectionState::caret(0);
    for index in 0..selection_event_count {
        selection = if index % 2 == 0 {
            SelectionState::caret(1)
        } else {
            SelectionState::caret(0)
        };
        let sequence = u64::try_from(index).unwrap() + 2;
        let envelope = envelope(
            session_id,
            sequence,
            previous_hash,
            Event::SelectionChanged(SelectionChanged {
                document_id: document_id(),
                anchor_byte: selection.anchor_byte,
                active_byte: selection.active_byte,
            }),
        );
        journal.append_event(session_id, &envelope).unwrap();
        previous_hash = envelope.event_hash;
    }

    let checkpoint_sequence = u64::try_from(selection_event_count).unwrap() + 2;
    let checkpoint = CheckpointSnapshot::new(
        session_id.clone(),
        checkpoint_sequence,
        vec![CheckpointFile {
            path: workspace_path(),
            contents: source.clone(),
        }],
        Some(document_id()),
        vec![OpenDocument {
            document_id: document_id(),
            path: workspace_path(),
            selection,
            version: 0,
        }],
    )
    .unwrap();
    let checkpoint_event = journal
        .append_checkpoint(session_id, checkpoint_sequence, None, &checkpoint)
        .unwrap();
    let final_sequence = checkpoint_sequence + 1;
    let finalization = envelope(
        session_id,
        final_sequence,
        checkpoint_event.event_hash,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: checkpoint.workspace_hash(),
            event_count: final_sequence,
            clean: true,
            warnings: Vec::new(),
        }),
    );
    journal.append_event(session_id, &finalization).unwrap();
    journal.end_session(session_id).unwrap();
}

fn rewrite_with_large_replacement_events(
    finalized: &rustrace::milestone_a::FinalizedSession,
    session_id: &SessionId,
    edit_count: usize,
    edit_bytes: usize,
) {
    let connection = Connection::open(finalized.journal_path()).unwrap();
    connection
        .execute(
            "DELETE FROM checkpoints WHERE session_id = ?1 AND sequence >= 2",
            [session_id.as_str()],
        )
        .unwrap();
    connection
        .execute(
            "DELETE FROM events WHERE session_id = ?1 AND sequence >= 2",
            [session_id.as_str()],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE sessions SET next_sequence = 2, ended = 0 WHERE session_id = ?1",
            [session_id.as_str()],
        )
        .unwrap();
    drop(connection);

    let mut journal = Journal::open(finalized.journal_path()).unwrap();
    let genesis = journal.read_events(session_id, 1, 1).unwrap().remove(0);
    let mut previous_hash = genesis.event_hash;
    let mut previous_text = "a".to_owned();
    let mut selection = SelectionState::caret(0);
    for index in 0..edit_count {
        let next_text = if index % 2 == 0 {
            "x".repeat(edit_bytes)
        } else {
            "y".repeat(edit_bytes)
        };
        let next_selection = SelectionState::caret(u64::try_from(next_text.len()).unwrap());
        let sequence = u64::try_from(index).unwrap() + 2;
        let transaction = EditorTransaction {
            document_id: document_id(),
            version_before: u64::try_from(index).unwrap(),
            version_after: u64::try_from(index + 1).unwrap(),
            origin: EditOrigin::Paste,
            edits: vec![TextEdit {
                start_byte: 0,
                end_byte: u64::try_from(previous_text.len()).unwrap(),
                inserted_text: next_text.clone(),
            }],
            selection_before: selection,
            selection_after: next_selection,
            hash_before: document_hash(&previous_text),
            hash_after: document_hash(&next_text),
        };
        let event = envelope(
            session_id,
            sequence,
            previous_hash,
            Event::FileEdited(transaction),
        );
        journal.append_event(session_id, &event).unwrap();
        previous_hash = event.event_hash;
        previous_text = next_text;
        selection = next_selection;
    }

    let checkpoint_sequence = u64::try_from(edit_count).unwrap() + 2;
    let checkpoint = CheckpointSnapshot::new(
        session_id.clone(),
        checkpoint_sequence,
        vec![CheckpointFile {
            path: workspace_path(),
            contents: previous_text.as_bytes().to_vec(),
        }],
        Some(document_id()),
        vec![OpenDocument {
            document_id: document_id(),
            path: workspace_path(),
            selection,
            version: u64::try_from(edit_count).unwrap(),
        }],
    )
    .unwrap();
    let checkpoint_event = journal
        .append_checkpoint(session_id, checkpoint_sequence, None, &checkpoint)
        .unwrap();
    let final_sequence = checkpoint_sequence + 1;
    let finalization = envelope(
        session_id,
        final_sequence,
        checkpoint_event.event_hash,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: checkpoint.workspace_hash(),
            event_count: final_sequence,
            clean: true,
            warnings: Vec::new(),
        }),
    );
    journal.append_event(session_id, &finalization).unwrap();
    journal.end_session(session_id).unwrap();
}

fn rewrite_with_one_ordinary_event(
    finalized: &rustrace::milestone_a::FinalizedSession,
    session_id: &SessionId,
    event: Event,
) {
    let connection = Connection::open(finalized.journal_path()).unwrap();
    connection
        .execute(
            "DELETE FROM checkpoints WHERE session_id = ?1 AND sequence >= 2",
            [session_id.as_str()],
        )
        .unwrap();
    connection
        .execute(
            "DELETE FROM events WHERE session_id = ?1 AND sequence >= 2",
            [session_id.as_str()],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE sessions SET next_sequence = 2, ended = 0 WHERE session_id = ?1",
            [session_id.as_str()],
        )
        .unwrap();
    drop(connection);

    let source = fs::read(finalized.source_path()).unwrap();
    let mut journal = Journal::open(finalized.journal_path()).unwrap();
    let genesis = journal.read_events(session_id, 1, 1).unwrap().remove(0);
    let ordinary = envelope(session_id, 2, genesis.event_hash, event);
    journal.append_event(session_id, &ordinary).unwrap();
    let checkpoint = CheckpointSnapshot::new(
        session_id.clone(),
        3,
        vec![CheckpointFile {
            path: workspace_path(),
            contents: source,
        }],
        Some(document_id()),
        vec![OpenDocument {
            document_id: document_id(),
            path: workspace_path(),
            selection: SelectionState::caret(0),
            version: 0,
        }],
    )
    .unwrap();
    let checkpoint_event = journal
        .append_checkpoint(session_id, 3, None, &checkpoint)
        .unwrap();
    let finalization = envelope(
        session_id,
        4,
        checkpoint_event.event_hash,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: checkpoint.workspace_hash(),
            event_count: 4,
            clean: true,
            warnings: Vec::new(),
        }),
    );
    journal.append_event(session_id, &finalization).unwrap();
    journal.end_session(session_id).unwrap();
}

fn envelope(
    session_id: &SessionId,
    sequence: u64,
    previous_event_hash: Hash,
    event: Event,
) -> EventEnvelope {
    EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: session_id.clone(),
        sequence,
        monotonic_millis: sequence,
        wall_clock_utc: None,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event,
    }
    .seal(previous_event_hash)
    .unwrap()
}
