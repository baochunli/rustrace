use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use rusqlite::limits::Limit;
use rustrace_model::{
    DocumentId, EditOrigin, EditorTransaction, Event, EventEnvelope, FORMAT_VERSION_V1,
    FileFocused, Hash, SelectionState, SessionId, TextEdit, WorkspacePath, encode_envelope,
};

use super::*;

struct TempDatabase {
    directory: PathBuf,
    path: PathBuf,
}

impl TempDatabase {
    fn new(name: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "rustrace-journal-unit-{name}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("journal.sqlite3");
        Self { directory, path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDatabase {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn session_id() -> SessionId {
    SessionId::new("crash-session").unwrap()
}

fn envelope(sequence: u64) -> EventEnvelope {
    let mut previous = Hash::zero();
    let mut result = None;
    for current in 1..=sequence {
        let envelope = EventEnvelope {
            format_version: FORMAT_VERSION_V1,
            session_id: session_id(),
            sequence: current,
            monotonic_millis: current,
            wall_clock_utc: None,
            previous_event_hash: Hash::zero(),
            event_hash: Hash::zero(),
            event: Event::FileFocused(FileFocused {
                document_id: DocumentId::new("main").unwrap(),
            }),
        }
        .seal(previous)
        .unwrap();
        previous = envelope.event_hash;
        result = Some(envelope);
    }
    result.unwrap()
}

#[test]
fn read_events_paginates_valid_dense_output_before_the_batch_byte_limit() {
    let id = session_id();
    let mut journal = Journal::open_in_memory().unwrap();
    journal.create_or_resume_session(&id).unwrap();
    let mut previous = Hash::zero();
    let inserted_text = "x".repeat(256 * 1024);
    let event_count = 80_u64;
    for sequence in 1..=event_count {
        let event = EventEnvelope {
            format_version: FORMAT_VERSION_V1,
            session_id: id.clone(),
            sequence,
            monotonic_millis: sequence,
            wall_clock_utc: None,
            previous_event_hash: Hash::zero(),
            event_hash: Hash::zero(),
            event: Event::FileEdited(EditorTransaction {
                document_id: DocumentId::new("main").unwrap(),
                version_before: sequence - 1,
                version_after: sequence,
                origin: EditOrigin::Keyboard,
                edits: vec![TextEdit {
                    start_byte: 0,
                    end_byte: 0,
                    inserted_text: inserted_text.clone(),
                }],
                selection_before: SelectionState::caret(0),
                selection_after: SelectionState::caret(inserted_text.len() as u64),
                hash_before: Hash::zero(),
                hash_after: Hash::zero(),
            }),
        }
        .seal(previous)
        .unwrap();
        previous = event.event_hash;
        journal.append_event(&id, &event).unwrap();
    }

    let first = journal.read_events(&id, 1, MAX_EVENTS_PER_READ).unwrap();
    assert!(!first.is_empty());
    assert!(first.len() < event_count as usize);
    let next = first.last().unwrap().sequence + 1;
    let second = journal.read_events(&id, next, MAX_EVENTS_PER_READ).unwrap();
    assert_eq!(first.len() + second.len(), event_count as usize);
    assert_eq!(second.first().unwrap().sequence, next);
}

#[test]
fn injected_error_after_insert_rolls_back_the_whole_append() {
    let id = session_id();
    let event = envelope(1);
    let payload = encode_envelope(&event).unwrap();
    let mut journal = Journal::open_in_memory().unwrap();
    journal.create_or_resume_session(&id).unwrap();

    let result = journal.append_with_hook(&id, &event, &payload, || {
        Err(JournalError::Database {
            operation: "injected failure after event insert",
            source: rusqlite::Error::InvalidQuery,
        })
    });
    assert!(result.is_err());
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 1);
    assert!(journal.read_events(&id, 1, 1).unwrap().is_empty());
}

fn compared_submissions() -> [EventSubmission; 2] {
    let prior = envelope(1);
    let command_id = rustrace_model::CommandId::new("command-2").unwrap();
    let capture = rustrace_model::CommandCapture {
        bytes: 0,
        completeness: rustrace_model::CaptureCompleteness::Complete,
        mode: rustrace_model::CommandCaptureMode::Captured,
    };
    let finish = Event::ControlledCommandFinished(rustrace_model::ControlledCommandFinished {
        command_id: command_id.clone(),
        after: rustrace_model::CommandTreeLink {
            checkpoint_sequence: 1,
            checkpoint_event_hash: prior.event_hash,
            workspace_hash: Hash::zero(),
            workspace_version: 1,
        },
        started_millis: 1,
        finished_millis: 2,
        outcome: rustrace_model::CommandOutcome::Exited { code: 0 },
        stdout: capture.clone(),
        stderr: capture,
    });
    let comparison = Event::TestCaseCompared(rustrace_model::TestCaseCompared {
        command_id,
        case: "input".into(),
        expected_blake3: Hash::zero(),
        actual_blake3: Some(Hash::zero()),
        outcome: rustrace_model::TestCaseComparisonOutcome::Pass,
    });
    [finish, comparison].map(|event| EventSubmission {
        session_id: session_id(),
        monotonic_millis: 2,
        wall_clock_utc: None,
        event,
    })
}

#[test]
fn second_pair_insert_failure_rolls_back_both_events_and_preserves_chain_on_reopen() {
    let temp = TempDatabase::new("atomic-pair");
    let id = session_id();
    let prior = envelope(1);
    let mut journal = Journal::create(temp.path()).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    journal.append_event(&id, &prior).unwrap();
    let before = journal.verify_session_chain(&id).unwrap();
    let mut observed_first_insert = false;
    let rejected = journal.append_submitted_event_pair_with_hook(
        compared_submissions(),
        |transaction, first| {
            let count: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE sequence = ?1",
                    [first.sequence as i64],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "the real first INSERT must have completed");
            observed_first_insert = true;
            transaction
                .execute_batch(
                    "CREATE TRIGGER fail_second_pair_insert BEFORE INSERT ON events
                 WHEN instr(CAST(NEW.payload AS TEXT), '\"type\":\"test_case_compared\"') > 0
                 BEGIN SELECT RAISE(ABORT, 'injected second pair INSERT failure'); END;",
                )
                .unwrap();
            Ok(())
        },
    );
    assert!(observed_first_insert);
    let Err(JournalError::Database { operation, source }) = rejected else {
        panic!("must fail in the real second INSERT, not schema preflight: {rejected:?}");
    };
    assert_eq!(operation, "insert submitted event");
    assert!(
        source
            .to_string()
            .contains("injected second pair INSERT failure")
    );
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 2);
    let after = journal.verify_session_chain(&id).unwrap();
    assert_eq!(after.event_count, before.event_count);
    assert_eq!(after.final_hash, before.final_hash);
    assert_eq!(
        journal.read_events(&id, 1, 10).unwrap(),
        vec![prior.clone()]
    );
    drop(journal);

    let mut journal = Journal::open(temp.path()).unwrap();
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 2);
    assert_eq!(
        journal.read_events(&id, 1, 10).unwrap(),
        vec![prior.clone()]
    );
    let pair = journal
        .append_submitted_event_pair(compared_submissions())
        .unwrap();
    assert_eq!([pair[0].sequence, pair[1].sequence], [2, 3]);
    assert_eq!(pair[0].previous_event_hash, prior.event_hash);
    assert_eq!(pair[1].previous_event_hash, pair[0].event_hash);
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 4);
    let next = journal
        .append_submitted_event(EventSubmission {
            session_id: id.clone(),
            monotonic_millis: 3,
            wall_clock_utc: None,
            event: Event::FileFocused(FileFocused {
                document_id: DocumentId::new("main").unwrap(),
            }),
        })
        .unwrap();
    assert_eq!(next.sequence, 4);
    assert_eq!(next.previous_event_hash, pair[1].event_hash);
    drop(journal);
    let mut journal = Journal::open(temp.path()).unwrap();
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 5);
    assert_eq!(
        journal.read_events(&id, 1, 10).unwrap(),
        vec![prior, pair[0].clone(), pair[1].clone(), next]
    );
    assert_eq!(journal.verify_session_chain(&id).unwrap().event_count, 4);
}

#[test]
fn injected_error_after_first_pair_insert_rolls_back_the_whole_pair() {
    let id = session_id();
    let mut journal = Journal::open_in_memory().unwrap();
    journal.create_or_resume_session(&id).unwrap();
    let result = journal.append_submitted_event_pair_with_hook(compared_submissions(), |_, _| {
        Err(JournalError::Database {
            operation: "injected failure after first pair insert",
            source: rusqlite::Error::InvalidQuery,
        })
    });
    assert!(result.is_err());
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 1);
    assert!(journal.read_events(&id, 1, 10).unwrap().is_empty());
}

#[test]
fn pair_verification_rechecks_first_row_after_second_insert() {
    let id = session_id();
    let mut journal = Journal::open_in_memory().unwrap();
    journal.create_or_resume_session(&id).unwrap();
    let rejected =
        journal.append_submitted_event_pair_with_hook(compared_submissions(), |transaction, _| {
            transaction
                .execute_batch(
                    "CREATE TRIGGER delete_first_pair_event AFTER INSERT ON events
             WHEN NEW.sequence = 2
             BEGIN DELETE FROM events WHERE sequence = 1; END;",
                )
                .unwrap();
            Ok(())
        });
    assert!(matches!(rejected, Err(JournalError::CorruptStorage(_))));
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 1);
    assert!(journal.read_events(&id, 1, 10).unwrap().is_empty());
}

#[test]
fn submitted_pair_rejects_checkpoints_in_either_slot() {
    let id = session_id();
    let snapshot = CheckpointSnapshot::new(id.clone(), 1, vec![], None, vec![]).unwrap();
    let mut journal = Journal::open_in_memory().unwrap();
    journal.create_or_resume_session(&id).unwrap();
    for slot in [0, 1] {
        let mut pair = compared_submissions();
        pair[slot].event = Event::WorkspaceCheckpoint(snapshot.event_payload());
        assert!(matches!(
            journal.append_submitted_event_pair(pair),
            Err(JournalError::CheckpointRequiresAtomicAppend)
        ));
        assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 1);
        assert!(journal.read_events(&id, 1, 10).unwrap().is_empty());
    }
}

#[test]
fn submitted_pair_rejects_wrong_session_invalid_second_event_and_sequence_overflow() {
    let id = session_id();
    let mut journal = Journal::open_in_memory().unwrap();
    journal.create_or_resume_session(&id).unwrap();
    let mut wrong_session = compared_submissions();
    wrong_session[1].session_id = SessionId::new("other").unwrap();
    assert!(matches!(
        journal.append_submitted_event_pair(wrong_session),
        Err(JournalError::WrongSession { .. })
    ));
    let mut invalid = compared_submissions();
    let Event::TestCaseCompared(comparison) = &mut invalid[1].event else {
        unreachable!()
    };
    comparison.case = "invalid!".into();
    assert!(matches!(
        journal.append_submitted_event_pair(invalid),
        Err(JournalError::InvalidEvent { .. })
    ));
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 1);
    assert!(journal.read_events(&id, 1, 10).unwrap().is_empty());
    journal
        .connection
        .execute(
            "UPDATE sessions SET next_sequence = ?1",
            [MAX_JOURNAL_SEQUENCE as i64],
        )
        .unwrap();
    assert!(matches!(
        journal.append_submitted_event_pair(compared_submissions()),
        Err(JournalError::SequenceOutOfRange { .. })
    ));
    assert_eq!(
        journal.inspect_session(&id).unwrap().next_sequence,
        MAX_JOURNAL_SEQUENCE
    );
    let count: i64 = journal
        .connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn injected_error_after_checkpoint_insert_rolls_back_event_and_payload() {
    let id = session_id();
    let snapshot = CheckpointSnapshot::new(
        id.clone(),
        1,
        vec![CheckpointFile {
            path: WorkspacePath::new("src/lib.rs").unwrap(),
            contents: b"pub fn value() {}\n".to_vec(),
        }],
        None,
        vec![],
    )
    .unwrap();
    let mut journal = Journal::open_in_memory().unwrap();
    journal.create_or_resume_session(&id).unwrap();

    let result = journal.append_checkpoint_with_hook(&id, 10, None, &snapshot, || {
        Err(JournalError::Database {
            operation: "injected failure after checkpoint insert",
            source: rusqlite::Error::InvalidQuery,
        })
    });

    assert!(result.is_err());
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 1);
    assert!(journal.read_events(&id, 1, 1).unwrap().is_empty());
    let checkpoint_count: i64 = journal
        .connection
        .query_row("SELECT COUNT(*) FROM checkpoints", [], |row| row.get(0))
        .unwrap();
    assert_eq!(checkpoint_count, 0);
}

#[test]
fn abrupt_process_exit_preserves_only_committed_appends() {
    const MODE: &str = "RUSTRACE_JOURNAL_CRASH_MODE";
    const PATH: &str = "RUSTRACE_JOURNAL_CRASH_PATH";

    if let (Ok(mode), Ok(path)) = (std::env::var(MODE), std::env::var(PATH)) {
        let id = session_id();
        let mut journal = Journal::open(path).unwrap();
        let SessionOpen::Resumed(state) = journal.create_or_resume_session(&id).unwrap() else {
            panic!("child must resume the existing session");
        };
        match mode.as_str() {
            "committed" => {
                assert_eq!(state.next_sequence, 1);
                journal.append_event(&id, &envelope(1)).unwrap();
                std::process::exit(0);
            }
            "inflight" => {
                assert_eq!(state.next_sequence, 2);
                let event = envelope(2);
                let payload = encode_envelope(&event).unwrap();
                let _ = journal.append_with_hook(&id, &event, &payload, || {
                    std::process::exit(86);
                });
                unreachable!();
            }
            other => panic!("unknown crash test mode {other}"),
        }
    }

    let temp = TempDatabase::new("crash");
    let id = session_id();
    let mut journal = Journal::create(temp.path()).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    drop(journal);

    let executable = std::env::current_exe().unwrap();
    let committed = Command::new(&executable)
        .args([
            "--exact",
            "tests::abrupt_process_exit_preserves_only_committed_appends",
        ])
        .env(MODE, "committed")
        .env(PATH, temp.path())
        .status()
        .unwrap();
    assert!(committed.success());

    let mut reopened = Journal::open(temp.path()).unwrap();
    assert_eq!(reopened.inspect_session(&id).unwrap().next_sequence, 2);
    assert_eq!(reopened.read_events(&id, 1, 10).unwrap(), vec![envelope(1)]);
    assert!(matches!(
        reopened.create_or_resume_session(&SessionId::new("unused").unwrap()),
        Ok(SessionOpen::Resumed(SessionState {
            next_sequence: 2,
            ..
        }))
    ));
    drop(reopened);

    let inflight = Command::new(executable)
        .args([
            "--exact",
            "tests::abrupt_process_exit_preserves_only_committed_appends",
        ])
        .env(MODE, "inflight")
        .env(PATH, temp.path())
        .status()
        .unwrap();
    assert_eq!(inflight.code(), Some(86));

    let mut reopened = Journal::open(temp.path()).unwrap();
    assert_eq!(reopened.read_events(&id, 1, 10).unwrap(), vec![envelope(1)]);
    assert_eq!(reopened.inspect_session(&id).unwrap().next_sequence, 2);
    assert!(matches!(
        reopened.create_or_resume_session(&SessionId::new("unused-again").unwrap()),
        Ok(SessionOpen::Resumed(SessionState {
            next_sequence: 2,
            ..
        }))
    ));
}

#[test]
fn connection_settings_are_effective_and_foreign_keys_are_enforced() {
    let id = session_id();
    let mut memory = Journal::open_in_memory().unwrap();
    memory.create_or_resume_session(&id).unwrap();
    assert_eq!(
        memory
            .connection
            .pragma_query_value::<i64, _>(None, "foreign_keys", |row| row.get(0))
            .unwrap(),
        1
    );
    assert_eq!(
        memory
            .connection
            .pragma_query_value::<i64, _>(None, "synchronous", |row| row.get(0))
            .unwrap(),
        2
    );
    assert_eq!(
        memory
            .connection
            .pragma_query_value::<String, _>(None, "journal_mode", |row| row.get(0))
            .unwrap(),
        "memory"
    );
    assert_eq!(
        memory.connection.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap(),
        SQLITE_LENGTH_LIMIT_BYTES as i32
    );

    let result = memory.connection.execute(
        "INSERT INTO events (session_id, sequence, format_version, \
         previous_event_hash, event_hash, payload) \
         VALUES ('missing', 1, 1, zeroblob(32), zeroblob(32), X'7B7D')",
        [],
    );
    assert!(result.is_err());

    let temp = TempDatabase::new("pragmas");
    let file = Journal::create(temp.path()).unwrap();
    assert_eq!(
        file.connection
            .pragma_query_value::<String, _>(None, "journal_mode", |row| row.get(0))
            .unwrap(),
        "wal"
    );
    assert_eq!(
        file.connection
            .pragma_query_value::<i64, _>(None, "busy_timeout", |row| row.get(0))
            .unwrap(),
        5000
    );
}

#[test]
fn bootstrap_rechecks_a_future_version_after_acquiring_the_writer_lock() {
    let temp = TempDatabase::new("bootstrap-version-race");
    fs::File::create(temp.path()).unwrap();
    let mut connection = rusqlite::Connection::open(temp.path()).unwrap();
    let path = temp.path().to_owned();

    let result = schema::initialize_with_bootstrap_hook(
        &mut connection,
        schema::StorageKind::File,
        move || {
            let other = rusqlite::Connection::open(path).unwrap();
            other.pragma_update(None, "user_version", 2).unwrap();
        },
    );
    assert!(matches!(
        result,
        Err(JournalError::UnsupportedSchemaVersion {
            found: 2,
            supported: SCHEMA_VERSION
        })
    ));
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let journal_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2);
    assert_eq!(table_count, 0);
    assert_eq!(journal_mode, "delete");
}

#[test]
fn concurrent_bootstraps_converge_on_schema_version_one() {
    let temp = TempDatabase::new("concurrent-bootstrap");
    fs::File::create(temp.path()).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let path = temp.path().to_owned();
        let barrier = std::sync::Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            let mut connection = rusqlite::Connection::open(path).unwrap();
            schema::initialize_with_bootstrap_hook(
                &mut connection,
                schema::StorageKind::File,
                || {
                    barrier.wait();
                },
            )
        }));
    }

    for worker in workers {
        worker.join().unwrap().unwrap();
    }
    let connection = rusqlite::Connection::open(temp.path()).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, i64::from(SCHEMA_VERSION));
    assert_eq!(table_count, 5);
}

#[test]
fn event_metadata_query_projects_only_bounded_values() {
    let journal = Journal::open_in_memory().unwrap();
    let statement = journal.connection.prepare(EVENT_METADATA_SQL).unwrap();
    assert_eq!(
        statement.column_names(),
        [
            "session_is_text",
            "session_length",
            "session_prefix",
            "sequence",
            "format_version",
            "previous_hash_is_blob",
            "previous_hash_length",
            "previous_hash_prefix",
            "event_hash_is_blob",
            "event_hash_length",
            "event_hash_prefix",
            "payload_is_blob",
            "payload_length",
        ]
    );

    let statement = journal.connection.prepare(SESSION_LOOKUP_SQL).unwrap();
    assert_eq!(
        statement.column_names(),
        [
            "session_is_text",
            "session_length",
            "session_prefix",
            "next_sequence",
            "ended",
        ]
    );
}

#[test]
fn checkpoint_verification_query_is_ordered_keyset_pagination() {
    assert!(CHECKPOINT_VERIFICATION_PAGE_SQL.contains("sequence > ?2"));
    assert!(CHECKPOINT_VERIFICATION_PAGE_SQL.contains("ORDER BY sequence"));
    assert!(CHECKPOINT_VERIFICATION_PAGE_SQL.contains("LIMIT ?3"));
    assert_eq!(MAX_CHECKPOINTS_PER_READ, 8);
}

#[test]
fn oversized_event_cells_are_rejected_before_the_payload_query_runs() {
    let temp = TempDatabase::new("metadata-first");
    let id = session_id();
    let mut journal = Journal::create(temp.path()).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    journal.append_event(&id, &envelope(1)).unwrap();

    let connection = rusqlite::Connection::open(temp.path()).unwrap();
    connection
        .execute_batch("PRAGMA ignore_check_constraints = ON;")
        .unwrap();
    connection
        .execute(
            "UPDATE events SET payload = zeroblob(2097152) \
             WHERE session_id = ?1 AND sequence = 1",
            [id.as_str()],
        )
        .unwrap();
    drop(connection);

    PAYLOAD_FETCH_COUNT.with(|count| count.set(0));
    assert!(matches!(
        journal.verify_session_chain(&id),
        Err(JournalError::CorruptStorage(Corruption::Event {
            kind: EventCorruption::OversizedPayload { .. },
            ..
        }))
    ));
    PAYLOAD_FETCH_COUNT.with(|count| assert_eq!(count.get(), 0));

    PAYLOAD_FETCH_COUNT.with(|count| count.set(0));
    assert!(matches!(
        journal.read_events(&id, 1, 1),
        Err(JournalError::CorruptStorage(Corruption::Event {
            kind: EventCorruption::OversizedPayload { .. },
            ..
        }))
    ));
    PAYLOAD_FETCH_COUNT.with(|count| assert_eq!(count.get(), 0));
    // The connection-wide cell limit now accommodates bounded full-workspace
    // checkpoints. Event APIs still reject this row from scalar metadata
    // before fetching its payload.

    let canonical_payload = encode_envelope(&envelope(1)).unwrap();
    let connection = rusqlite::Connection::open(temp.path()).unwrap();
    connection
        .execute_batch("PRAGMA ignore_check_constraints = ON;")
        .unwrap();
    connection
        .execute(
            "UPDATE events SET payload = ?1, event_hash = zeroblob(65536) \
             WHERE session_id = ?2 AND sequence = 1",
            rusqlite::params![canonical_payload, id.as_str()],
        )
        .unwrap();
    drop(connection);

    PAYLOAD_FETCH_COUNT.with(|count| count.set(0));
    assert!(matches!(
        journal.read_events(&id, 1, 1),
        Err(JournalError::CorruptStorage(Corruption::Event {
            kind: EventCorruption::InvalidColumn {
                field: "event_hash"
            },
            ..
        }))
    ));
    PAYLOAD_FETCH_COUNT.with(|count| assert_eq!(count.get(), 0));
}

#[test]
fn oversized_checkpoint_cells_are_rejected_before_the_payload_query_runs() {
    let temp = TempDatabase::new("checkpoint-metadata-first");
    let id = session_id();
    let mut journal = Journal::create(temp.path()).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    let snapshot = CheckpointSnapshot::new(
        id.clone(),
        1,
        vec![CheckpointFile {
            path: WorkspacePath::new("src/lib.rs").unwrap(),
            contents: b"pub fn value() {}\n".to_vec(),
        }],
        None,
        vec![],
    )
    .unwrap();
    journal.append_checkpoint(&id, 10, None, &snapshot).unwrap();

    let oversized = i64::try_from(MAX_CHECKPOINT_ENCODED_BYTES + 1).unwrap();
    let connection = rusqlite::Connection::open(temp.path()).unwrap();
    connection
        .execute(
            "UPDATE checkpoints SET payload = zeroblob(?1) \
             WHERE session_id = ?2 AND sequence = 1",
            rusqlite::params![oversized, id.as_str()],
        )
        .unwrap();
    drop(connection);

    CHECKPOINT_PAYLOAD_FETCH_COUNT.with(|count| count.set(0));
    assert!(matches!(
        journal.load_checkpoint(&id, 1),
        Err(JournalError::CorruptStorage(Corruption::Checkpoint {
            kind: CheckpointCorruption::OversizedPayload { .. },
            ..
        }))
    ));
    CHECKPOINT_PAYLOAD_FETCH_COUNT.with(|count| assert_eq!(count.get(), 0));
}

#[test]
fn verification_rejects_nonpositive_sequences_before_fetching_payloads() {
    for (name, sequence) in [("zero-sequence", 0_i64), ("negative-sequence", -1_i64)] {
        let temp = TempDatabase::new(name);
        let id = session_id();
        let mut journal = Journal::create(temp.path()).unwrap();
        journal.create_or_resume_session(&id).unwrap();

        let connection = rusqlite::Connection::open(temp.path()).unwrap();
        connection
            .execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        connection
            .execute(
                "INSERT INTO events (
                    session_id, sequence, format_version,
                    previous_event_hash, event_hash, payload
                 ) VALUES (?1, ?2, 1, zeroblob(32), zeroblob(32), ?3)",
                rusqlite::params![id.as_str(), sequence, b"{}".as_slice()],
            )
            .unwrap();
        drop(connection);

        PAYLOAD_FETCH_COUNT.with(|count| count.set(0));
        assert!(matches!(
            journal.verify_session_chain(&id),
            Err(JournalError::CorruptStorage(Corruption::Event {
                sequence: None,
                kind: EventCorruption::InvalidColumn { field: "sequence" },
                ..
            }))
        ));
        PAYLOAD_FETCH_COUNT.with(|count| assert_eq!(count.get(), 0));
    }
}

#[test]
fn checkpoint_verification_rejects_event_sequence_above_the_domain_before_payload_read() {
    let temp = TempDatabase::new("event-sequence-domain");
    let id = session_id();
    let mut journal = Journal::create(temp.path()).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    let connection = rusqlite::Connection::open(temp.path()).unwrap();
    connection
        .execute(
            "INSERT INTO events (
                session_id, sequence, format_version,
                previous_event_hash, event_hash, payload
             ) VALUES (?1, ?2, 1, zeroblob(32), zeroblob(32), X'00')",
            rusqlite::params![id.as_str(), i64::MAX],
        )
        .unwrap();
    drop(connection);

    PAYLOAD_FETCH_COUNT.with(|count| count.set(0));
    assert!(matches!(
        journal.verify_session_checkpoints(&id),
        Err(JournalError::CorruptStorage(Corruption::Event {
            sequence: Some(sequence),
            kind: EventCorruption::InvalidColumn { field: "sequence" },
            ..
        })) if sequence == i64::MAX as u64
    ));
    PAYLOAD_FETCH_COUNT.with(|count| assert_eq!(count.get(), 0));
}
