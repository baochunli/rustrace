use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
    thread,
};

use rusqlite::Connection;
use rustrace_journal::{
    Corruption, EventCorruption, Journal, JournalError, MAX_EVENTS_PER_READ, SCHEMA_VERSION,
    SessionOpen,
};
use rustrace_model::{
    DecodeOutcome, DecodePolicy, DocumentId, Event, EventEnvelope, FORMAT_VERSION_V1, FileFocused,
    Hash, SessionId, decode_envelope, encode_envelope,
};

struct TempDatabase {
    directory: PathBuf,
    path: PathBuf,
}

impl TempDatabase {
    fn new(name: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "rustrace-journal-{name}-{}-{serial}",
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

fn session_id(value: &str) -> SessionId {
    SessionId::new(value).unwrap()
}

fn envelope(session_id: &SessionId, sequence: u64) -> EventEnvelope {
    let mut previous = Hash::zero();
    let mut result = None;
    for current in 1..=sequence {
        let envelope = EventEnvelope {
            format_version: FORMAT_VERSION_V1,
            session_id: session_id.clone(),
            sequence: current,
            monotonic_millis: current * 10,
            wall_clock_utc: None,
            previous_event_hash: Hash::zero(),
            event_hash: Hash::zero(),
            event: Event::FileFocused(FileFocused {
                document_id: DocumentId::new("src-main").unwrap(),
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
fn creates_resumes_ends_and_reopens_sessions() {
    let temp = TempDatabase::new("lifecycle");
    let first_id = session_id("session-one");
    let next_id = session_id("session-two");

    let mut journal = Journal::create(temp.path()).unwrap();
    let SessionOpen::Created(created) = journal.create_or_resume_session(&first_id).unwrap() else {
        panic!("new database must create a session");
    };
    assert_eq!(created.session_id, first_id);
    assert_eq!(created.next_sequence, 1);
    assert!(!created.ended);

    journal
        .append_event(&first_id, &envelope(&first_id, 1))
        .unwrap();
    drop(journal);

    let connection = Connection::open(temp.path()).unwrap();
    let stored_payload: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM events WHERE session_id = ?1 AND sequence = 1",
            [first_id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stored_payload,
        encode_envelope(&envelope(&first_id, 1)).unwrap()
    );
    drop(connection);

    let mut reopened = Journal::open(temp.path()).unwrap();
    let SessionOpen::Resumed(resumed) = reopened.create_or_resume_session(&next_id).unwrap() else {
        panic!("unfinished session must be resumed");
    };
    assert_eq!(resumed.session_id, first_id);
    assert_eq!(resumed.next_sequence, 2);

    let ended = reopened.end_session(&first_id).unwrap();
    assert!(ended.ended);
    assert_eq!(reopened.end_session(&first_id).unwrap(), ended);
    assert!(matches!(
        reopened.append_event(&first_id, &envelope(&first_id, 2)),
        Err(JournalError::SessionEnded { .. })
    ));

    let SessionOpen::Created(created) = reopened.create_or_resume_session(&next_id).unwrap() else {
        panic!("an ended session must not be resumed");
    };
    assert_eq!(created.session_id, next_id);
}

#[test]
fn read_only_no_follow_open_validates_without_bootstrap_or_evidence_mutation() {
    let valid = TempDatabase::new("read-only-valid");
    let id = session_id("read-only-session");
    let mut journal = Journal::create(valid.path()).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    journal.append_event(&id, &envelope(&id, 1)).unwrap();
    drop(journal);
    let before = fs::read(valid.path()).unwrap();

    let valid_anchored = fs::canonicalize(valid.path().parent().unwrap())
        .unwrap()
        .join("journal.sqlite3");
    let read_only = Journal::open_read_only_no_follow(&valid_anchored).unwrap();
    assert_eq!(read_only.inspect_session(&id).unwrap().next_sequence, 2);
    drop(read_only);
    assert_eq!(fs::read(valid.path()).unwrap(), before);

    let corrupt = TempDatabase::new("read-only-corrupt");
    fs::write(corrupt.path(), b"corrupt evidence").unwrap();
    let corrupt_anchored = fs::canonicalize(corrupt.path().parent().unwrap())
        .unwrap()
        .join("journal.sqlite3");
    assert!(Journal::open_read_only_no_follow(corrupt_anchored).is_err());
    assert_eq!(fs::read(corrupt.path()).unwrap(), b"corrupt evidence");

    let missing = TempDatabase::new("read-only-missing");
    let missing_anchored = fs::canonicalize(missing.path().parent().unwrap())
        .unwrap()
        .join("journal.sqlite3");
    assert!(Journal::open_read_only_no_follow(missing_anchored).is_err());
    assert!(!missing.path().exists());
}

#[test]
fn appends_exact_canonical_payload_and_rejects_ordering_errors_atomically() {
    let mut journal = Journal::open_in_memory().unwrap();
    let id = session_id("ordered");
    journal.create_or_resume_session(&id).unwrap();
    let first = envelope(&id, 1);
    journal.append_event(&id, &first).unwrap();

    let duplicate = journal.append_event(&id, &first).unwrap_err();
    assert!(matches!(
        duplicate,
        JournalError::UnexpectedSequence {
            expected: 2,
            actual: 1,
            ..
        }
    ));
    assert!(matches!(
        journal.append_event(&id, &envelope(&id, 3)),
        Err(JournalError::UnexpectedSequence {
            expected: 2,
            actual: 3,
            ..
        })
    ));

    let other = session_id("other");
    assert!(matches!(
        journal.append_event(&id, &envelope(&other, 2)),
        Err(JournalError::WrongSession { .. })
    ));
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 2);

    let read = journal.read_events(&id, 1, MAX_EVENTS_PER_READ).unwrap();
    assert_eq!(read, vec![first.clone()]);
    let encoded = encode_envelope(&read[0]).unwrap();
    assert_eq!(encoded, encode_envelope(&first).unwrap());
    assert_eq!(
        decode_envelope(&encoded, DecodePolicy::RejectUnsupported).unwrap(),
        DecodeOutcome::Decoded(first)
    );
}

#[test]
fn concurrent_connections_cannot_commit_the_same_sequence() {
    let temp = TempDatabase::new("concurrent");
    let id = session_id("shared");
    let mut initial = Journal::create(temp.path()).unwrap();
    initial.create_or_resume_session(&id).unwrap();
    drop(initial);

    let barrier = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let path = temp.path().to_owned();
        let barrier = Arc::clone(&barrier);
        let id = id.clone();
        workers.push(thread::spawn(move || {
            let mut journal = Journal::open(path).unwrap();
            barrier.wait();
            journal.append_event(&id, &envelope(&id, 1))
        }));
    }

    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(JournalError::UnexpectedSequence { .. })))
            .count(),
        1
    );

    let mut journal = Journal::open(temp.path()).unwrap();
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 2);
    assert_eq!(journal.read_events(&id, 1, 10).unwrap().len(), 1);
}

#[test]
fn schema_is_versioned_and_contains_phase_two_seams() {
    let temp = TempDatabase::new("schema");
    drop(Journal::create(temp.path()).unwrap());
    let connection = Connection::open(temp.path()).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);

    for table in ["sessions", "events", "checkpoints", "documents", "metadata"] {
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "missing table {table}");
    }

    let events_sql: String = connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'events'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(events_sql.contains("PRIMARY KEY (session_id, sequence)"));
    assert!(events_sql.contains("FOREIGN KEY (session_id) REFERENCES sessions"));
}

#[test]
fn rejects_unknown_newer_schema_version() {
    let temp = TempDatabase::new("future-schema");
    let connection = Connection::open(temp.path()).unwrap();
    connection
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    drop(connection);

    assert!(matches!(
        Journal::open(temp.path()),
        Err(JournalError::UnsupportedSchemaVersion { .. })
    ));
    let connection = Connection::open(temp.path()).unwrap();
    let journal_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode, "delete");
}

#[test]
fn late_future_version_blocks_session_creation_without_mutation() {
    let temp = TempDatabase::new("late-version-create");
    let mut journal = Journal::create(temp.path()).unwrap();
    let connection = Connection::open(temp.path()).unwrap();
    connection
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();

    assert!(matches!(
        journal.create_or_resume_session(&session_id("blocked-create")),
        Err(JournalError::UnsupportedSchemaVersion {
            found: 2,
            supported: SCHEMA_VERSION
        })
    ));
    let session_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(session_count, 0);
}

#[test]
fn late_future_version_blocks_append_without_mutation() {
    let temp = TempDatabase::new("late-version-append");
    let id = session_id("blocked-append");
    let mut journal = Journal::create(temp.path()).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    journal.append_event(&id, &envelope(&id, 1)).unwrap();
    let connection = Connection::open(temp.path()).unwrap();
    connection
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();

    assert!(matches!(
        journal.append_event(&id, &envelope(&id, 2)),
        Err(JournalError::UnsupportedSchemaVersion {
            found: 2,
            supported: SCHEMA_VERSION
        })
    ));
    let next_sequence: i64 = connection
        .query_row(
            "SELECT next_sequence FROM sessions WHERE session_id = ?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    let sequences: Vec<i64> = connection
        .prepare("SELECT sequence FROM events WHERE session_id = ?1 ORDER BY sequence")
        .unwrap()
        .query_map([id.as_str()], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(next_sequence, 2);
    assert_eq!(sequences, vec![1]);
}

#[test]
fn late_future_version_blocks_session_end_without_mutation() {
    let temp = TempDatabase::new("late-version-end");
    let id = session_id("blocked-end");
    let mut journal = Journal::create(temp.path()).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    let connection = Connection::open(temp.path()).unwrap();
    connection
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();

    assert!(matches!(
        journal.end_session(&id),
        Err(JournalError::UnsupportedSchemaVersion {
            found: 2,
            supported: SCHEMA_VERSION
        })
    ));
    let ended: i64 = connection
        .query_row(
            "SELECT ended FROM sessions WHERE session_id = ?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ended, 0);
}

#[test]
fn malformed_database_and_corrupt_event_rows_are_typed_errors() {
    let malformed = TempDatabase::new("malformed-file");
    fs::write(malformed.path(), b"not a sqlite database").unwrap();
    assert!(matches!(
        Journal::open(malformed.path()),
        Err(JournalError::CorruptStorage(
            Corruption::DatabaseFile { .. }
        ))
    ));

    let corrupt = TempDatabase::new("corrupt-row");
    let id = session_id("corrupt");
    let mut journal = Journal::create(corrupt.path()).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    journal.append_event(&id, &envelope(&id, 1)).unwrap();
    drop(journal);

    let connection = Connection::open(corrupt.path()).unwrap();
    connection
        .execute(
            "UPDATE events SET payload = X'7B' WHERE session_id = ?1 AND sequence = 1",
            [id.as_str()],
        )
        .unwrap();
    drop(connection);

    let mut journal = Journal::open(corrupt.path()).unwrap();
    assert!(matches!(
        journal.read_events(&id, 1, 1),
        Err(JournalError::CorruptStorage(Corruption::Event {
            kind: EventCorruption::MalformedPayload { .. },
            ..
        }))
    ));
}

#[test]
fn invalid_missing_and_bounded_requests_do_not_change_session_state() {
    let mut journal = Journal::open_in_memory().unwrap();
    let id = session_id("validation");
    journal.create_or_resume_session(&id).unwrap();

    let mut invalid = envelope(&id, 1);
    invalid.sequence = 0;
    assert!(matches!(
        journal.append_event(&id, &invalid),
        Err(JournalError::InvalidEvent { .. })
    ));
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 1);
    assert!(matches!(
        journal.inspect_session(&session_id("missing")),
        Err(JournalError::SessionNotFound { .. })
    ));
    assert!(matches!(
        journal.read_events(&id, 0, 1),
        Err(JournalError::InvalidReadStart { .. })
    ));
    assert!(matches!(
        journal.read_events(&id, 1, MAX_EVENTS_PER_READ + 1),
        Err(JournalError::ReadLimitTooLarge { .. })
    ));
}

#[test]
fn multiple_unfinished_sessions_are_reported_as_ambiguous() {
    let temp = TempDatabase::new("ambiguous");
    let first = session_id("first");
    let mut journal = Journal::create(temp.path()).unwrap();
    journal.create_or_resume_session(&first).unwrap();
    drop(journal);

    let connection = Connection::open(temp.path()).unwrap();
    connection
        .execute(
            "INSERT INTO sessions (session_id, next_sequence, ended) VALUES ('second', 1, 0)",
            [],
        )
        .unwrap();
    drop(connection);

    let mut journal = Journal::open(temp.path()).unwrap();
    assert!(matches!(
        journal.create_or_resume_session(&session_id("third")),
        Err(JournalError::AmbiguousUnfinishedSessions { count: 2 })
    ));
}

#[test]
fn oversized_and_inconsistent_event_rows_are_rejected_before_decode() {
    let oversized = TempDatabase::new("oversized-row");
    let id = session_id("oversized");
    let mut journal = Journal::create(oversized.path()).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    journal.append_event(&id, &envelope(&id, 1)).unwrap();

    let connection = Connection::open(oversized.path()).unwrap();
    connection
        .execute_batch("PRAGMA ignore_check_constraints = ON;")
        .unwrap();
    connection
        .execute(
            "UPDATE events SET payload = zeroblob(1048577) \
             WHERE session_id = ?1 AND sequence = 1",
            [id.as_str()],
        )
        .unwrap();
    assert!(matches!(
        journal.read_events(&id, 1, 1),
        Err(JournalError::CorruptStorage(Corruption::Event {
            kind: EventCorruption::OversizedPayload {
                actual: 1_048_577,
                maximum: 1_048_576
            },
            ..
        }))
    ));
    drop(connection);
    drop(journal);

    let inconsistent = TempDatabase::new("inconsistent-row");
    let id = session_id("inconsistent");
    let mut journal = Journal::create(inconsistent.path()).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    journal.append_event(&id, &envelope(&id, 1)).unwrap();
    let connection = Connection::open(inconsistent.path()).unwrap();
    connection
        .execute(
            "UPDATE events SET event_hash = zeroblob(32) \
             WHERE session_id = ?1 AND sequence = 1",
            [id.as_str()],
        )
        .unwrap();
    assert!(matches!(
        journal.read_events(&id, 1, 1),
        Err(JournalError::CorruptStorage(Corruption::Event {
            kind: EventCorruption::ColumnMismatch {
                field: "event_hash"
            },
            ..
        }))
    ));
}

#[test]
fn truncated_file_and_incomplete_schema_are_typed_corruption() {
    let truncated = TempDatabase::new("truncated");
    drop(Journal::create(truncated.path()).unwrap());
    fs::OpenOptions::new()
        .write(true)
        .open(truncated.path())
        .unwrap()
        .set_len(64)
        .unwrap();
    assert!(matches!(
        Journal::open(truncated.path()),
        Err(JournalError::CorruptStorage(
            Corruption::DatabaseFile { .. }
        ))
    ));

    let incomplete = TempDatabase::new("incomplete-schema");
    drop(Journal::create(incomplete.path()).unwrap());
    let connection = Connection::open(incomplete.path()).unwrap();
    connection.execute("DROP TABLE metadata", []).unwrap();
    drop(connection);
    assert!(matches!(
        Journal::open(incomplete.path()),
        Err(JournalError::CorruptStorage(Corruption::Schema { .. }))
    ));
}

#[test]
fn constraint_free_lookalike_schema_is_rejected() {
    let temp = TempDatabase::new("lookalike-schema");
    let connection = Connection::open(temp.path()).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE sessions (
                session_id TEXT, next_sequence INTEGER, ended INTEGER
             );
             CREATE TABLE events (
                session_id TEXT, sequence INTEGER, format_version INTEGER,
                previous_event_hash BLOB, event_hash BLOB, payload BLOB
             );
             CREATE TABLE checkpoints (
                session_id TEXT, sequence INTEGER, payload BLOB
             );
             CREATE TABLE documents (
                session_id TEXT, document_id TEXT, path TEXT,
                version INTEGER, content_hash BLOB
             );
             CREATE TABLE metadata (key TEXT, value BLOB);
             PRAGMA user_version = 1;",
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        Journal::open(temp.path()),
        Err(JournalError::CorruptStorage(Corruption::Schema { .. }))
    ));
}

#[test]
fn unexpected_trigger_is_rejected_when_opening() {
    let temp = TempDatabase::new("unexpected-trigger");
    drop(Journal::create(temp.path()).unwrap());
    let connection = Connection::open(temp.path()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER delete_inserted_event
             AFTER INSERT ON events
             BEGIN
                 DELETE FROM events
                 WHERE session_id = NEW.session_id AND sequence = NEW.sequence;
             END;",
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        Journal::open(temp.path()),
        Err(JournalError::CorruptStorage(Corruption::Schema { .. }))
    ));
}

#[test]
fn append_verification_rolls_back_an_event_deleted_by_a_late_trigger() {
    let temp = TempDatabase::new("late-trigger");
    let id = session_id("late-trigger-session");
    let mut journal = Journal::create(temp.path()).unwrap();
    journal.create_or_resume_session(&id).unwrap();

    let connection = Connection::open(temp.path()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER delete_inserted_event
             AFTER INSERT ON events
             BEGIN
                 DELETE FROM events
                 WHERE session_id = NEW.session_id AND sequence = NEW.sequence;
             END;",
        )
        .unwrap();

    assert!(matches!(
        journal.append_event(&id, &envelope(&id, 1)),
        Err(JournalError::CorruptStorage(_))
    ));
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 1);
    let event_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(event_count, 0);
}

#[test]
fn append_rejects_a_late_trigger_before_it_can_delete_prior_history() {
    let temp = TempDatabase::new("late-prior-history-trigger");
    let id = session_id("late-prior-history-trigger-session");
    let first = envelope(&id, 1);
    let mut journal = Journal::create(temp.path()).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    journal.append_event(&id, &first).unwrap();

    let connection = Connection::open(temp.path()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER delete_prior_events
             AFTER INSERT ON events
             BEGIN
                 DELETE FROM events
                 WHERE session_id = NEW.session_id AND sequence < NEW.sequence;
             END;",
        )
        .unwrap();

    assert!(matches!(
        journal.append_event(&id, &envelope(&id, 2)),
        Err(JournalError::CorruptStorage(Corruption::Schema { .. }))
    ));
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 2);
    assert_eq!(journal.read_events(&id, 1, 10).unwrap(), vec![first]);
    let sequences: Vec<i64> = connection
        .prepare("SELECT sequence FROM events ORDER BY sequence")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(sequences, vec![1]);
}

#[test]
fn embedded_nul_session_id_is_rejected_on_open() {
    let temp = TempDatabase::new("nul-session");
    drop(Journal::create(temp.path()).unwrap());
    let connection = Connection::open(temp.path()).unwrap();
    assert!(
        connection
            .execute(
                "INSERT INTO sessions (session_id, next_sequence, ended)
                 VALUES (CAST(X'626164006964' AS TEXT), 1, 0)",
                [],
            )
            .is_err()
    );
    connection
        .execute_batch("PRAGMA ignore_check_constraints = ON;")
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions (session_id, next_sequence, ended)
             VALUES (CAST(X'626164006964' AS TEXT), 1, 0)",
            [],
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        Journal::open(temp.path()),
        Err(JournalError::CorruptStorage(Corruption::Session { .. }))
    ));
}

#[test]
fn oversized_session_id_is_rejected_on_open() {
    let temp = TempDatabase::new("oversized-session");
    drop(Journal::create(temp.path()).unwrap());
    let connection = Connection::open(temp.path()).unwrap();
    connection
        .execute_batch("PRAGMA ignore_check_constraints = ON;")
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions (session_id, next_sequence, ended) VALUES (?1, 1, 0)",
            ["x".repeat(64 * 1024)],
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        Journal::open(temp.path()),
        Err(JournalError::CorruptStorage(Corruption::Session { .. }))
    ));
}

#[test]
fn foreign_key_and_sequence_corruption_are_rejected_on_open() {
    let foreign_key = TempDatabase::new("foreign-key-corruption");
    drop(Journal::create(foreign_key.path()).unwrap());
    let connection = Connection::open(foreign_key.path()).unwrap();
    connection
        .execute_batch("PRAGMA foreign_keys = OFF;")
        .unwrap();
    connection
        .execute(
            "INSERT INTO events (
                session_id, sequence, format_version,
                previous_event_hash, event_hash, payload
             ) VALUES ('missing', 1, 1, zeroblob(32), zeroblob(32), X'7B7D')",
            [],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        Journal::open(foreign_key.path()),
        Err(JournalError::CorruptStorage(
            Corruption::ForeignKeyViolation
        ))
    ));

    let sequence = TempDatabase::new("sequence-corruption");
    let id = session_id("sequence-corrupt");
    let mut journal = Journal::create(sequence.path()).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    journal.append_event(&id, &envelope(&id, 1)).unwrap();
    drop(journal);
    let connection = Connection::open(sequence.path()).unwrap();
    connection
        .execute(
            "UPDATE sessions SET next_sequence = 3 WHERE session_id = ?1",
            [id.as_str()],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        Journal::open(sequence.path()),
        Err(JournalError::CorruptStorage(Corruption::Session { .. }))
    ));
}
