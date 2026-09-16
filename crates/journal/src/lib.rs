//! Crash-safe local provenance storage for Rustrace.
//!
//! The journal owns SQLite framing, transactional sequence and hash-chain
//! enforcement, deterministic bounded workspace checkpoints, and explicit
//! integrity verification. Headless replay remains a later Phase 2
//! responsibility.

#![forbid(unsafe_code)]

mod checkpoint;
mod error;
mod schedule;
mod schema;
mod worker;

use std::{fs::OpenOptions, path::Path, time::Duration};

pub use checkpoint::*;
use chrono::{DateTime, Utc};
pub use error::{ChainMismatch, CheckpointCorruption, Corruption, EventCorruption, JournalError};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, Statement, Transaction, TransactionBehavior,
    params, types::ValueRef,
};
use rustrace_model::{
    DecodeOutcome, DecodePolicy, Event, EventEnvelope, FORMAT_VERSION_V1, Hash, MAX_ENVELOPE_BYTES,
    MAX_IDENTIFIER_BYTES, SessionId, compute_event_hash, decode_envelope, encode_envelope,
};
pub use schedule::*;
use schema::{StorageKind, initialize, validate_read_only, validate_schema};
pub use worker::*;

pub const SCHEMA_VERSION: u32 = 1;
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_EVENTS_PER_READ: usize = 1024;
pub const MAX_READ_BYTES: usize = 16 * MAX_ENVELOPE_BYTES;
pub const MAX_CHECKPOINTS_PER_READ: usize = 8;
pub const MAX_CHECKPOINT_READ_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_JOURNAL_SEQUENCE: u64 = i64::MAX as u64 - 1;
const SQLITE_LENGTH_LIMIT_BYTES: usize = MAX_CHECKPOINT_ENCODED_BYTES + 4 * 1024;

macro_rules! event_metadata_sql {
    ($predicate:literal) => {
        concat!(
            "SELECT ",
            "typeof(session_id) = 'text' AS session_is_text, ",
            "length(CAST(session_id AS BLOB)) AS session_length, ",
            "substr(CAST(session_id AS BLOB), 1, 129) AS session_prefix, ",
            "sequence, format_version, ",
            "typeof(previous_event_hash) = 'blob' AS previous_hash_is_blob, ",
            "length(previous_event_hash) AS previous_hash_length, ",
            "substr(previous_event_hash, 1, 33) AS previous_hash_prefix, ",
            "typeof(event_hash) = 'blob' AS event_hash_is_blob, ",
            "length(event_hash) AS event_hash_length, ",
            "substr(event_hash, 1, 33) AS event_hash_prefix, ",
            "typeof(payload) = 'blob' AS payload_is_blob, ",
            "length(payload) AS payload_length ",
            "FROM events WHERE ",
            $predicate
        )
    };
}

const EVENT_METADATA_SQL: &str =
    event_metadata_sql!("session_id = ?1 AND sequence >= ?2 ORDER BY sequence LIMIT ?3");
const EXACT_EVENT_METADATA_SQL: &str = event_metadata_sql!("session_id = ?1 AND sequence = ?2");
const EVENT_PAYLOAD_SQL: &str =
    "SELECT payload FROM events WHERE session_id = ?1 AND sequence = ?2";
const NONPOSITIVE_EVENT_SQL: &str =
    "SELECT 1 FROM events WHERE session_id = ?1 AND sequence < 1 LIMIT 1";
const SESSION_LOOKUP_SQL: &str = "SELECT typeof(session_id) = 'text' AS session_is_text, \
     length(CAST(session_id AS BLOB)) AS session_length, \
     substr(CAST(session_id AS BLOB), 1, 129) AS session_prefix, next_sequence, ended \
     FROM sessions WHERE session_id = ?1";
const UNFINISHED_SESSION_SQL: &str = "SELECT typeof(session_id) = 'text' AS session_is_text, \
     length(CAST(session_id AS BLOB)) AS session_length, \
     substr(CAST(session_id AS BLOB), 1, 129) AS session_prefix, next_sequence, ended \
     FROM sessions WHERE ended = 0 LIMIT 1";

macro_rules! checkpoint_metadata_sql {
    ($predicate:literal) => {
        concat!(
            "SELECT ",
            "typeof(session_id) = 'text' AS session_is_text, ",
            "length(CAST(session_id AS BLOB)) AS session_length, ",
            "substr(CAST(session_id AS BLOB), 1, 129) AS session_prefix, ",
            "sequence, ",
            "typeof(payload) = 'blob' AS payload_is_blob, ",
            "length(payload) AS payload_length ",
            "FROM checkpoints WHERE ",
            $predicate
        )
    };
}

const EXACT_CHECKPOINT_METADATA_SQL: &str =
    checkpoint_metadata_sql!("session_id = ?1 AND sequence = ?2");
const CHECKPOINT_METADATA_PAGE_SQL: &str =
    checkpoint_metadata_sql!("session_id = ?1 AND sequence >= ?2 ORDER BY sequence LIMIT ?3");
const LATEST_CHECKPOINT_METADATA_SQL: &str =
    checkpoint_metadata_sql!("session_id = ?1 AND sequence <= ?2 ORDER BY sequence DESC LIMIT 1");
const CHECKPOINT_VERIFICATION_PAGE_SQL: &str =
    checkpoint_metadata_sql!("session_id = ?1 AND sequence > ?2 ORDER BY sequence LIMIT ?3");
const NONPOSITIVE_CHECKPOINT_SQL: &str =
    "SELECT 1 FROM checkpoints WHERE session_id = ?1 AND sequence < 1 LIMIT 1";
const CHECKPOINT_PAYLOAD_SQL: &str =
    "SELECT payload FROM checkpoints WHERE session_id = ?1 AND sequence = ?2";

#[derive(Clone, Debug)]
struct EventMetadata {
    sequence: u64,
    format_version: u32,
    previous_event_hash: Hash,
    event_hash: Hash,
    payload_length: u64,
}

#[derive(Clone, Debug)]
struct CheckpointMetadata {
    sequence: u64,
    payload_length: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionState {
    pub session_id: SessionId,
    pub next_sequence: u64,
    pub ended: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionOpen {
    Created(SessionState),
    Resumed(SessionState),
}

/// Result of verifying one complete session in a single SQLite snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedSessionChain {
    pub event_count: u64,
    pub final_hash: Hash,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredCheckpoint {
    pub owning_event: EventEnvelope,
    pub snapshot: CheckpointSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedSessionCheckpoints {
    pub checkpoint_count: u64,
    pub latest_sequence: Option<u64>,
}

pub struct Journal {
    connection: Connection,
}

impl Journal {
    /// Creates a new file-backed journal without replacing an existing path.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        Self::create_with_flags(path.as_ref(), false)
    }

    /// Creates a journal and asks SQLite to reject symbolic links while
    /// opening its absolute, already-anchored path.
    pub fn create_no_follow(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        Self::create_with_flags(path.as_ref(), true)
    }

    fn create_with_flags(path: &Path, no_follow: bool) -> Result<Self, JournalError> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|source| JournalError::Io {
                operation: "create journal file",
                source,
            })?;
        drop(file);
        Self::open_connection(path, StorageKind::File, no_follow)
    }

    /// Opens an existing file-backed journal and validates it at the boundary.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        Self::open_connection(path.as_ref(), StorageKind::File, false)
    }

    /// Opens an existing journal and asks SQLite to reject symbolic links in
    /// the supplied absolute, already-anchored path.
    pub fn open_no_follow(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        Self::open_connection(path.as_ref(), StorageKind::File, true)
    }

    /// Opens a controlled live session using standard WAL/FULL durability,
    /// retaining WAL evidence on close instead of implicitly checkpointing and
    /// unlinking sidecars. The caller owns the cooperative writer lock and
    /// validates canonical authority; restart reconciliation is separate.
    pub fn open_retained_no_follow(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let mut connection = Connection::open_with_flags(path, flags)
            .map_err(|error| error::database_error("open retained journal", error))?;
        let retained = connection
            .set_db_config(
                rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                true,
            )
            .map_err(|error| error::database_error("retain WAL evidence on close", error))?;
        if !retained {
            return Err(JournalError::StorageConfiguration {
                setting: "SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE",
                expected: "enabled",
                actual: "disabled".to_owned(),
            });
        }
        initialize(&mut connection, StorageKind::File)?;
        Ok(Self { connection })
    }

    /// Opens and validates an existing journal without following its final
    /// path component or changing any persisted database state.
    pub fn open_read_only_no_follow(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = Connection::open_with_flags(path, flags)
            .map_err(|error| error::database_error("open read-only journal", error))?;
        validate_read_only(&connection)?;
        Ok(Self { connection })
    }

    /// Opens a transient journal. SQLite uses `journal_mode=memory`; all other
    /// checks and `synchronous=FULL` remain enabled.
    pub fn open_in_memory() -> Result<Self, JournalError> {
        let mut connection = Connection::open_in_memory()
            .map_err(|error| error::database_error("open in-memory journal", error))?;
        initialize(&mut connection, StorageKind::Memory)?;
        Ok(Self { connection })
    }

    fn open_connection(
        path: &Path,
        kind: StorageKind,
        no_follow: bool,
    ) -> Result<Self, JournalError> {
        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE;
        if no_follow {
            flags |= OpenFlags::SQLITE_OPEN_NOFOLLOW;
        }
        let mut connection = Connection::open_with_flags(path, flags)
            .map_err(|error| error::database_error("open journal", error))?;
        initialize(&mut connection, kind)?;
        Ok(Self { connection })
    }

    /// Resumes the sole unfinished session, or creates `new_session_id` when
    /// none exists. More than one unfinished session is reported as ambiguous.
    pub fn create_or_resume_session(
        &mut self,
        new_session_id: &SessionId,
    ) -> Result<SessionOpen, JournalError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error::database_error("begin create-or-resume transaction", error))?;
        let count: i64 = transaction
            .query_row("SELECT COUNT(*) FROM sessions WHERE ended = 0", [], |row| {
                row.get(0)
            })
            .map_err(|error| error::database_error("count unfinished sessions", error))?;
        let count = u64::try_from(count).map_err(|_| {
            JournalError::CorruptStorage(Corruption::Session {
                session_id: None,
                detail: "unfinished session count is outside u64".to_owned(),
            })
        })?;

        let result = match count {
            0 => {
                if query_session_state(&transaction, new_session_id)?.is_some() {
                    return Err(JournalError::SessionAlreadyExists {
                        session_id: new_session_id.clone(),
                    });
                }
                validate_schema(&transaction)?;
                transaction
                    .execute(
                        "INSERT INTO sessions (session_id, next_sequence, ended) \
                         VALUES (?1, 1, 0)",
                        [new_session_id.as_str()],
                    )
                    .map_err(|error| error::database_error("create session", error))?;
                SessionOpen::Created(SessionState {
                    session_id: new_session_id.clone(),
                    next_sequence: 1,
                    ended: false,
                })
            }
            1 => SessionOpen::Resumed(query_only_unfinished_session(&transaction)?),
            _ => return Err(JournalError::AmbiguousUnfinishedSessions { count }),
        };
        transaction
            .commit()
            .map_err(|error| error::database_error("commit create-or-resume transaction", error))?;
        Ok(result)
    }

    pub fn inspect_session(&self, session_id: &SessionId) -> Result<SessionState, JournalError> {
        query_session_state(&self.connection, session_id)?.ok_or_else(|| {
            JournalError::SessionNotFound {
                session_id: session_id.clone(),
            }
        })
    }

    /// Persists one complete canonical envelope. A successful return occurs
    /// only after the `synchronous=FULL` SQLite commit has completed.
    pub fn append_event(
        &mut self,
        session_id: &SessionId,
        envelope: &EventEnvelope,
    ) -> Result<(), JournalError> {
        if matches!(envelope.event, Event::WorkspaceCheckpoint(_)) {
            return Err(JournalError::CheckpointRequiresAtomicAppend);
        }
        if &envelope.session_id != session_id {
            return Err(JournalError::WrongSession {
                expected: session_id.clone(),
                actual: envelope.session_id.clone(),
            });
        }
        if envelope.sequence > MAX_JOURNAL_SEQUENCE {
            return Err(JournalError::SequenceOutOfRange {
                actual: envelope.sequence,
                maximum: MAX_JOURNAL_SEQUENCE,
            });
        }
        let payload =
            encode_envelope(envelope).map_err(|source| JournalError::InvalidEvent { source })?;
        self.append_with_hook(session_id, envelope, &payload, || Ok(()))
    }

    /// Assigns the authoritative sequence/hash chain and commits one ordinary
    /// event. Used by the single-owner FIFO journal writer.
    pub(crate) fn append_submitted_event(
        &mut self,
        submission: EventSubmission,
    ) -> Result<EventEnvelope, JournalError> {
        if matches!(submission.event, Event::WorkspaceCheckpoint(_)) {
            return Err(JournalError::CheckpointRequiresAtomicAppend);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error::database_error("begin submitted event transaction", error))?;
        let state =
            query_session_state(&transaction, &submission.session_id)?.ok_or_else(|| {
                JournalError::SessionNotFound {
                    session_id: submission.session_id.clone(),
                }
            })?;
        if state.ended {
            return Err(JournalError::SessionEnded {
                session_id: submission.session_id,
            });
        }
        if state.next_sequence > MAX_JOURNAL_SEQUENCE {
            return Err(JournalError::SequenceOutOfRange {
                actual: state.next_sequence,
                maximum: MAX_JOURNAL_SEQUENCE,
            });
        }

        validate_schema(&transaction)?;
        let previous_event_hash =
            authoritative_previous_hash(&transaction, &submission.session_id, state.next_sequence)?;
        let envelope = EventEnvelope {
            format_version: FORMAT_VERSION_V1,
            session_id: submission.session_id.clone(),
            sequence: state.next_sequence,
            monotonic_millis: submission.monotonic_millis,
            wall_clock_utc: submission.wall_clock_utc,
            previous_event_hash: Hash::zero(),
            event_hash: Hash::zero(),
            event: submission.event,
        }
        .seal(previous_event_hash)
        .map_err(|source| JournalError::InvalidEvent { source })?;
        let payload =
            encode_envelope(&envelope).map_err(|source| JournalError::InvalidEvent { source })?;
        enforce_append_chain(&transaction, &submission.session_id, &envelope)?;
        let sequence = to_sql_sequence(envelope.sequence)?;
        insert_submitted_event(&transaction, &envelope, &payload)?;
        let next_sequence =
            envelope
                .sequence
                .checked_add(1)
                .ok_or(JournalError::SequenceOutOfRange {
                    actual: envelope.sequence,
                    maximum: MAX_JOURNAL_SEQUENCE,
                })?;
        let next_sequence_sql =
            i64::try_from(next_sequence).map_err(|_| JournalError::SequenceOutOfRange {
                actual: envelope.sequence,
                maximum: MAX_JOURNAL_SEQUENCE,
            })?;
        let changed = transaction
            .execute(
                "UPDATE sessions SET next_sequence = ?1 \
                 WHERE session_id = ?2 AND next_sequence = ?3 AND ended = 0",
                params![next_sequence_sql, submission.session_id.as_str(), sequence],
            )
            .map_err(|error| error::database_error("advance submitted event sequence", error))?;
        if changed != 1 {
            return Err(session_corruption(
                Some(submission.session_id.to_string()),
                "submitted event could not advance next_sequence exactly once",
            ));
        }
        verify_append(
            &transaction,
            &submission.session_id,
            &envelope,
            &payload,
            next_sequence,
        )?;
        transaction
            .commit()
            .map_err(|error| error::database_error("commit submitted event", error))?;
        Ok(envelope)
    }

    /// Assigns and durably commits exactly two ordinary events in one step.
    /// Neither row nor the session cursor survives a failed append.
    pub(crate) fn append_submitted_event_pair(
        &mut self,
        submissions: [EventSubmission; 2],
    ) -> Result<[EventEnvelope; 2], JournalError> {
        self.append_submitted_event_pair_with_hook(submissions, |_, _| Ok(()))
    }

    fn append_submitted_event_pair_with_hook<F>(
        &mut self,
        submissions: [EventSubmission; 2],
        after_first_insert: F,
    ) -> Result<[EventEnvelope; 2], JournalError>
    where
        F: FnOnce(&Transaction<'_>, &EventEnvelope) -> Result<(), JournalError>,
    {
        let [first, second] = submissions;
        if first.session_id != second.session_id {
            return Err(JournalError::WrongSession {
                expected: first.session_id,
                actual: second.session_id,
            });
        }
        if matches!(first.event, Event::WorkspaceCheckpoint(_))
            || matches!(second.event, Event::WorkspaceCheckpoint(_))
        {
            return Err(JournalError::CheckpointRequiresAtomicAppend);
        }
        let session_id = first.session_id.clone();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                error::database_error("begin submitted event pair transaction", error)
            })?;
        let state = query_session_state(&transaction, &session_id)?.ok_or_else(|| {
            JournalError::SessionNotFound {
                session_id: session_id.clone(),
            }
        })?;
        if state.ended {
            return Err(JournalError::SessionEnded { session_id });
        }
        let second_sequence = state
            .next_sequence
            .checked_add(1)
            .filter(|sequence| *sequence <= MAX_JOURNAL_SEQUENCE)
            .ok_or(JournalError::SequenceOutOfRange {
                actual: state.next_sequence.saturating_add(1),
                maximum: MAX_JOURNAL_SEQUENCE,
            })?;
        validate_schema(&transaction)?;
        let previous_hash =
            authoritative_previous_hash(&transaction, &session_id, state.next_sequence)?;
        let first = submitted_envelope(first, state.next_sequence, previous_hash)?;
        let second = submitted_envelope(second, second_sequence, first.event_hash)?;
        let pair = [first, second];
        let payloads = [
            encode_envelope(&pair[0]).map_err(|source| JournalError::InvalidEvent { source })?,
            encode_envelope(&pair[1]).map_err(|source| JournalError::InvalidEvent { source })?,
        ];
        enforce_append_chain(&transaction, &session_id, &pair[0])?;
        insert_submitted_event(&transaction, &pair[0], &payloads[0])?;
        after_first_insert(&transaction, &pair[0])?;
        enforce_append_chain(&transaction, &session_id, &pair[1])?;
        insert_submitted_event(&transaction, &pair[1], &payloads[1])?;

        let next_sequence = second_sequence + 1;
        let changed = transaction
            .execute(
                "UPDATE sessions SET next_sequence = ?1 \
             WHERE session_id = ?2 AND next_sequence = ?3 AND ended = 0",
                params![
                    next_sequence as i64,
                    session_id.as_str(),
                    to_sql_sequence(state.next_sequence)?
                ],
            )
            .map_err(|error| {
                error::database_error("advance submitted event pair sequence", error)
            })?;
        if changed != 1 {
            return Err(session_corruption(
                Some(session_id.to_string()),
                "submitted event pair could not advance next_sequence exactly once",
            ));
        }
        // Recheck both rows after the second INSERT, not only its tail.
        for (envelope, payload) in pair.iter().zip(&payloads) {
            verify_append(&transaction, &session_id, envelope, payload, next_sequence)?;
        }
        transaction
            .commit()
            .map_err(|error| error::database_error("commit submitted event pair", error))?;
        Ok(pair)
    }

    /// Creates, seals, and stores a checkpoint event and its compressed payload
    /// in one `IMMEDIATE` transaction.
    pub fn append_checkpoint(
        &mut self,
        session_id: &SessionId,
        monotonic_millis: u64,
        wall_clock_utc: Option<DateTime<Utc>>,
        snapshot: &CheckpointSnapshot,
    ) -> Result<EventEnvelope, JournalError> {
        self.append_checkpoint_with_hook(
            session_id,
            monotonic_millis,
            wall_clock_utc,
            snapshot,
            || Ok(()),
        )
    }

    fn append_checkpoint_with_hook<F>(
        &mut self,
        session_id: &SessionId,
        monotonic_millis: u64,
        wall_clock_utc: Option<DateTime<Utc>>,
        snapshot: &CheckpointSnapshot,
        after_checkpoint_insert: F,
    ) -> Result<EventEnvelope, JournalError>
    where
        F: FnOnce() -> Result<(), JournalError>,
    {
        snapshot
            .validate()
            .map_err(|source| JournalError::InvalidCheckpoint {
                source: Box::new(source),
            })?;
        if snapshot.session_id() != session_id {
            return Err(JournalError::WrongSession {
                expected: session_id.clone(),
                actual: snapshot.session_id().clone(),
            });
        }
        let checkpoint_payload =
            encode_checkpoint(snapshot).map_err(|source| JournalError::InvalidCheckpoint {
                source: Box::new(source),
            })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error::database_error("begin checkpoint append transaction", error))?;
        let state = query_session_state(&transaction, session_id)?.ok_or_else(|| {
            JournalError::SessionNotFound {
                session_id: session_id.clone(),
            }
        })?;
        if state.ended {
            return Err(JournalError::SessionEnded {
                session_id: session_id.clone(),
            });
        }
        if snapshot.event_sequence() != state.next_sequence {
            return Err(JournalError::UnexpectedSequence {
                session_id: session_id.clone(),
                expected: state.next_sequence,
                actual: snapshot.event_sequence(),
            });
        }

        validate_schema(&transaction)?;
        let previous_event_hash =
            authoritative_previous_hash(&transaction, session_id, snapshot.event_sequence())?;
        let envelope = EventEnvelope {
            format_version: FORMAT_VERSION_V1,
            session_id: session_id.clone(),
            sequence: snapshot.event_sequence(),
            monotonic_millis,
            wall_clock_utc,
            previous_event_hash: Hash::zero(),
            event_hash: Hash::zero(),
            event: Event::WorkspaceCheckpoint(snapshot.event_payload()),
        }
        .seal(previous_event_hash)
        .map_err(|source| JournalError::InvalidEvent { source })?;
        let event_payload =
            encode_envelope(&envelope).map_err(|source| JournalError::InvalidEvent { source })?;
        enforce_append_chain(&transaction, session_id, &envelope)?;
        let sequence = to_sql_sequence(envelope.sequence)?;
        transaction
            .execute(
                "INSERT INTO events (session_id, sequence, format_version, \
                 previous_event_hash, event_hash, payload) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session_id.as_str(),
                    sequence,
                    i64::from(envelope.format_version),
                    envelope.previous_event_hash.as_bytes().as_slice(),
                    envelope.event_hash.as_bytes().as_slice(),
                    event_payload,
                ],
            )
            .map_err(|error| error::database_error("insert checkpoint event", error))?;
        transaction
            .execute(
                "INSERT INTO checkpoints (session_id, sequence, payload) VALUES (?1, ?2, ?3)",
                params![session_id.as_str(), sequence, checkpoint_payload],
            )
            .map_err(|error| error::database_error("insert checkpoint payload", error))?;

        after_checkpoint_insert()?;

        let next_sequence =
            envelope
                .sequence
                .checked_add(1)
                .ok_or(JournalError::SequenceOutOfRange {
                    actual: envelope.sequence,
                    maximum: MAX_JOURNAL_SEQUENCE,
                })?;
        let next_sequence_sql =
            i64::try_from(next_sequence).map_err(|_| JournalError::SequenceOutOfRange {
                actual: envelope.sequence,
                maximum: MAX_JOURNAL_SEQUENCE,
            })?;
        let changed = transaction
            .execute(
                "UPDATE sessions SET next_sequence = ?1 \
                 WHERE session_id = ?2 AND next_sequence = ?3 AND ended = 0",
                params![next_sequence_sql, session_id.as_str(), sequence],
            )
            .map_err(|error| error::database_error("advance checkpoint sequence", error))?;
        if changed != 1 {
            return Err(session_corruption(
                Some(session_id.to_string()),
                "checkpoint append could not advance next_sequence exactly once",
            ));
        }
        verify_append(
            &transaction,
            session_id,
            &envelope,
            &event_payload,
            next_sequence,
        )?;
        let stored = query_exact_checkpoint_metadata(&transaction, session_id, envelope.sequence)?
            .ok_or_else(|| {
                checkpoint_corruption(
                    session_id,
                    Some(envelope.sequence),
                    CheckpointCorruption::MissingPayload,
                )
            })?;
        with_checkpoint_payload(&transaction, session_id, &stored, |payload| {
            if payload == checkpoint_payload {
                Ok(())
            } else {
                Err(checkpoint_corruption(
                    session_id,
                    Some(envelope.sequence),
                    CheckpointCorruption::ColumnMismatch { field: "payload" },
                ))
            }
        })?;
        transaction
            .commit()
            .map_err(|error| error::database_error("commit checkpoint append", error))?;
        Ok(envelope)
    }

    fn append_with_hook<F>(
        &mut self,
        session_id: &SessionId,
        envelope: &EventEnvelope,
        payload: &[u8],
        after_insert: F,
    ) -> Result<(), JournalError>
    where
        F: FnOnce() -> Result<(), JournalError>,
    {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error::database_error("begin event append transaction", error))?;
        let state = query_session_state(&transaction, session_id)?.ok_or_else(|| {
            JournalError::SessionNotFound {
                session_id: session_id.clone(),
            }
        })?;
        if state.ended {
            return Err(JournalError::SessionEnded {
                session_id: session_id.clone(),
            });
        }
        if envelope.sequence != state.next_sequence {
            return Err(JournalError::UnexpectedSequence {
                session_id: session_id.clone(),
                expected: state.next_sequence,
                actual: envelope.sequence,
            });
        }

        validate_schema(&transaction)?;
        enforce_append_chain(&transaction, session_id, envelope)?;
        let sequence = to_sql_sequence(envelope.sequence)?;
        transaction
            .execute(
                "INSERT INTO events (session_id, sequence, format_version, \
                 previous_event_hash, event_hash, payload) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session_id.as_str(),
                    sequence,
                    i64::from(envelope.format_version),
                    envelope.previous_event_hash.as_bytes().as_slice(),
                    envelope.event_hash.as_bytes().as_slice(),
                    payload,
                ],
            )
            .map_err(|error| error::database_error("insert event", error))?;

        after_insert()?;

        let next_sequence =
            envelope
                .sequence
                .checked_add(1)
                .ok_or(JournalError::SequenceOutOfRange {
                    actual: envelope.sequence,
                    maximum: MAX_JOURNAL_SEQUENCE,
                })?;
        let next_sequence_sql =
            i64::try_from(next_sequence).map_err(|_| JournalError::SequenceOutOfRange {
                actual: envelope.sequence,
                maximum: MAX_JOURNAL_SEQUENCE,
            })?;
        let changed = transaction
            .execute(
                "UPDATE sessions SET next_sequence = ?1 \
                 WHERE session_id = ?2 AND next_sequence = ?3 AND ended = 0",
                params![next_sequence_sql, session_id.as_str(), sequence],
            )
            .map_err(|error| error::database_error("advance session sequence", error))?;
        if changed != 1 {
            return Err(JournalError::CorruptStorage(Corruption::Session {
                session_id: Some(session_id.to_string()),
                detail: "append could not advance next_sequence exactly once".to_owned(),
            }));
        }
        verify_append(&transaction, session_id, envelope, payload, next_sequence)?;
        transaction
            .commit()
            .map_err(|error| error::database_error("commit event append", error))
    }

    /// Reads a bounded page in ascending sequence order.
    pub fn read_events(
        &mut self,
        session_id: &SessionId,
        first_sequence: u64,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, JournalError> {
        if first_sequence == 0 {
            return Err(JournalError::InvalidReadStart {
                actual: first_sequence,
            });
        }
        if first_sequence > MAX_JOURNAL_SEQUENCE {
            return Err(JournalError::SequenceOutOfRange {
                actual: first_sequence,
                maximum: MAX_JOURNAL_SEQUENCE,
            });
        }
        if limit > MAX_EVENTS_PER_READ {
            return Err(JournalError::ReadLimitTooLarge {
                actual: limit,
                maximum: MAX_EVENTS_PER_READ,
            });
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(|error| error::database_error("begin event read transaction", error))?;
        let state = query_session_state(&transaction, session_id)?.ok_or_else(|| {
            JournalError::SessionNotFound {
                session_id: session_id.clone(),
            }
        })?;
        if limit == 0 || first_sequence >= state.next_sequence {
            transaction
                .commit()
                .map_err(|error| error::database_error("commit empty event read", error))?;
            return Ok(Vec::new());
        }
        let first_sql =
            i64::try_from(first_sequence).map_err(|_| JournalError::SequenceOutOfRange {
                actual: first_sequence,
                maximum: MAX_JOURNAL_SEQUENCE,
            })?;
        let limit_sql = i64::try_from(limit).map_err(|_| JournalError::ReadLimitTooLarge {
            actual: limit,
            maximum: MAX_EVENTS_PER_READ,
        })?;
        let mut statement = transaction
            .prepare(EVENT_METADATA_SQL)
            .map_err(|error| error::database_error("prepare event read", error))?;
        let mut rows = statement
            .query(params![session_id.as_str(), first_sql, limit_sql])
            .map_err(|error| error::database_error("query events", error))?;
        let mut metadata = Vec::with_capacity(limit.min(16));
        let mut expected = first_sequence;
        let mut batch_bytes = 0u64;
        let mut truncated_by_bytes = false;
        let maximum_batch_bytes = u64::try_from(MAX_READ_BYTES).unwrap_or(u64::MAX);
        while let Some(row) = rows
            .next()
            .map_err(|error| error::database_error("read event row", error))?
        {
            let event_metadata = decode_event_metadata(row, session_id, expected)?;
            let next_batch_bytes = batch_bytes
                .checked_add(event_metadata.payload_length)
                .ok_or(JournalError::ReadBatchTooLarge {
                    actual: u64::MAX,
                    maximum: MAX_READ_BYTES,
                })?;
            if next_batch_bytes > maximum_batch_bytes {
                truncated_by_bytes = true;
                break;
            }
            batch_bytes = next_batch_bytes;
            expected = expected.checked_add(1).ok_or_else(|| {
                corrupt_event(
                    session_id,
                    Some(expected),
                    EventCorruption::InvalidColumn { field: "sequence" },
                )
            })?;
            metadata.push(event_metadata);
        }

        let limit_u64 = u64::try_from(limit).map_err(|_| JournalError::ReadLimitTooLarge {
            actual: limit,
            maximum: MAX_EVENTS_PER_READ,
        })?;
        let expected_end = state
            .next_sequence
            .min(first_sequence.saturating_add(limit_u64));
        if !truncated_by_bytes && expected != expected_end {
            return Err(corrupt_event(
                session_id,
                None,
                EventCorruption::SequenceGap {
                    expected,
                    actual: state.next_sequence,
                },
            ));
        }
        drop(rows);
        drop(statement);

        let mut payload_statement = transaction
            .prepare(EVENT_PAYLOAD_SQL)
            .map_err(|error| error::database_error("prepare event payload read", error))?;
        let mut events = Vec::with_capacity(metadata.len());
        for event_metadata in &metadata {
            events.push(read_event_payload(
                &mut payload_statement,
                session_id,
                event_metadata,
            )?);
        }
        drop(payload_statement);
        transaction
            .commit()
            .map_err(|error| error::database_error("commit event read", error))?;
        Ok(events)
    }

    /// Loads and verifies the checkpoint at exactly `sequence`.
    ///
    /// A non-checkpoint event or an absent event returns `None`; a checkpoint
    /// event missing its payload is corruption.
    pub fn load_checkpoint(
        &mut self,
        session_id: &SessionId,
        sequence: u64,
    ) -> Result<Option<StoredCheckpoint>, JournalError> {
        to_sql_sequence(sequence)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(|error| error::database_error("begin checkpoint load", error))?;
        validate_schema(&transaction)?;
        require_session(&transaction, session_id)?;
        let metadata = query_exact_checkpoint_metadata(&transaction, session_id, sequence)?;
        let result = match metadata {
            Some(metadata) => Some(load_stored_checkpoint(&transaction, session_id, &metadata)?),
            None => {
                let event = load_optional_event(&transaction, session_id, sequence)?;
                if matches!(
                    event.as_ref().map(|event| &event.event),
                    Some(Event::WorkspaceCheckpoint(_))
                ) {
                    return Err(checkpoint_corruption(
                        session_id,
                        Some(sequence),
                        CheckpointCorruption::MissingPayload,
                    ));
                }
                None
            }
        };
        transaction
            .commit()
            .map_err(|error| error::database_error("commit checkpoint load", error))?;
        Ok(result)
    }

    /// Loads the newest stored checkpoint whose owning sequence is no greater
    /// than `sequence`.
    pub fn latest_checkpoint_at_or_before(
        &mut self,
        session_id: &SessionId,
        sequence: u64,
    ) -> Result<Option<StoredCheckpoint>, JournalError> {
        let sequence = to_sql_sequence(sequence)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(|error| error::database_error("begin checkpoint seek", error))?;
        validate_schema(&transaction)?;
        require_session(&transaction, session_id)?;
        let metadata = query_one_checkpoint_metadata(
            &transaction,
            LATEST_CHECKPOINT_METADATA_SQL,
            params![session_id.as_str(), sequence],
            session_id,
        )?;
        let result = metadata
            .as_ref()
            .map(|metadata| load_stored_checkpoint(&transaction, session_id, metadata))
            .transpose()?;
        transaction
            .commit()
            .map_err(|error| error::database_error("commit checkpoint seek", error))?;
        Ok(result)
    }

    /// Loads a bounded ascending checkpoint page.
    pub fn list_checkpoints(
        &mut self,
        session_id: &SessionId,
        first_sequence: u64,
        limit: usize,
    ) -> Result<Vec<StoredCheckpoint>, JournalError> {
        if first_sequence == 0 {
            return Err(JournalError::InvalidReadStart {
                actual: first_sequence,
            });
        }
        let first_sequence = to_sql_sequence(first_sequence)?;
        if limit > MAX_CHECKPOINTS_PER_READ {
            return Err(JournalError::CheckpointReadLimitTooLarge {
                actual: limit,
                maximum: MAX_CHECKPOINTS_PER_READ,
            });
        }
        let limit_sql =
            i64::try_from(limit).map_err(|_| JournalError::CheckpointReadLimitTooLarge {
                actual: limit,
                maximum: MAX_CHECKPOINTS_PER_READ,
            })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(|error| error::database_error("begin checkpoint list", error))?;
        validate_schema(&transaction)?;
        require_session(&transaction, session_id)?;
        if limit == 0 {
            transaction
                .commit()
                .map_err(|error| error::database_error("commit empty checkpoint list", error))?;
            return Ok(Vec::new());
        }
        let mut statement = transaction
            .prepare(CHECKPOINT_METADATA_PAGE_SQL)
            .map_err(|error| error::database_error("prepare checkpoint page", error))?;
        let mut rows = statement
            .query(params![session_id.as_str(), first_sequence, limit_sql])
            .map_err(|error| error::database_error("query checkpoint page", error))?;
        let mut metadata = Vec::with_capacity(limit);
        let mut previous = None;
        let mut batch_bytes = 0u64;
        while let Some(row) = rows
            .next()
            .map_err(|error| error::database_error("read checkpoint page", error))?
        {
            let item = decode_checkpoint_metadata(row, session_id)?;
            if previous.is_some_and(|previous| item.sequence <= previous) {
                return Err(checkpoint_corruption(
                    session_id,
                    Some(item.sequence),
                    CheckpointCorruption::InvalidColumn { field: "sequence" },
                ));
            }
            batch_bytes = batch_bytes.checked_add(item.payload_length).ok_or(
                JournalError::CheckpointReadBatchTooLarge {
                    actual: u64::MAX,
                    maximum: MAX_CHECKPOINT_READ_BYTES,
                },
            )?;
            if batch_bytes > MAX_CHECKPOINT_READ_BYTES as u64 {
                return Err(JournalError::CheckpointReadBatchTooLarge {
                    actual: batch_bytes,
                    maximum: MAX_CHECKPOINT_READ_BYTES,
                });
            }
            previous = Some(item.sequence);
            metadata.push(item);
        }
        drop(rows);
        drop(statement);
        let mut checkpoints = Vec::with_capacity(metadata.len());
        for item in &metadata {
            checkpoints.push(load_stored_checkpoint(&transaction, session_id, item)?);
        }
        transaction
            .commit()
            .map_err(|error| error::database_error("commit checkpoint list", error))?;
        Ok(checkpoints)
    }

    /// Verifies every checkpoint/event ownership relationship in one SQLite
    /// read snapshot while retaining at most one decoded checkpoint at a time.
    pub fn verify_session_checkpoints(
        &mut self,
        session_id: &SessionId,
    ) -> Result<VerifiedSessionCheckpoints, JournalError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(|error| error::database_error("begin checkpoint verification", error))?;
        validate_schema(&transaction)?;
        let state = require_session(&transaction, session_id)?;

        reject_nonpositive_event_sequence(&transaction, session_id)?;
        let nonpositive_checkpoint: Option<i64> = transaction
            .query_row(NONPOSITIVE_CHECKPOINT_SQL, [session_id.as_str()], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|error| {
                error::database_error("check nonpositive checkpoint sequences", error)
            })?;
        if nonpositive_checkpoint.is_some() {
            return Err(checkpoint_corruption(
                session_id,
                None,
                CheckpointCorruption::InvalidColumn { field: "sequence" },
            ));
        }

        let mut checkpoint_count = 0u64;
        let mut latest_sequence = None;
        let mut checkpoint_cursor = 0u64;
        loop {
            let page = read_checkpoint_metadata_page(
                &transaction,
                session_id,
                checkpoint_cursor,
                MAX_CHECKPOINTS_PER_READ,
            )?;
            if page.is_empty() {
                break;
            }
            for metadata in page {
                if metadata.sequence >= state.next_sequence {
                    load_checkpoint_owner(&transaction, session_id, metadata.sequence)?;
                    return Err(checkpoint_corruption(
                        session_id,
                        Some(metadata.sequence),
                        CheckpointCorruption::SequenceOutsideSessionTail {
                            next_sequence: state.next_sequence,
                        },
                    ));
                }
                load_stored_checkpoint(&transaction, session_id, &metadata)?;
                checkpoint_count = checkpoint_count.checked_add(1).ok_or_else(|| {
                    session_corruption(
                        Some(session_id.to_string()),
                        "verified checkpoint count overflowed u64",
                    )
                })?;
                latest_sequence = Some(metadata.sequence);
                checkpoint_cursor = metadata.sequence;
            }
        }

        let mut sequence = 1u64;
        loop {
            let events =
                read_event_metadata_page(&transaction, session_id, sequence, MAX_EVENTS_PER_READ)?;
            if events.is_empty() {
                break;
            }
            let mut payload_statement = transaction
                .prepare(EVENT_PAYLOAD_SQL)
                .map_err(|error| error::database_error("prepare checkpoint owner scan", error))?;
            for metadata in &events {
                let event = read_event_payload(&mut payload_statement, session_id, metadata)?;
                if matches!(event.event, Event::WorkspaceCheckpoint(_))
                    && query_exact_checkpoint_metadata(&transaction, session_id, metadata.sequence)?
                        .is_none()
                {
                    return Err(checkpoint_corruption(
                        session_id,
                        Some(metadata.sequence),
                        CheckpointCorruption::MissingPayload,
                    ));
                }
                sequence = metadata.sequence.checked_add(1).ok_or_else(|| {
                    corrupt_event(
                        session_id,
                        Some(metadata.sequence),
                        EventCorruption::InvalidColumn { field: "sequence" },
                    )
                })?;
            }
        }

        if sequence < state.next_sequence {
            return Err(corrupt_event(
                session_id,
                Some(sequence),
                EventCorruption::MissingRow,
            ));
        }
        if sequence > state.next_sequence {
            return Err(session_corruption(
                Some(session_id.to_string()),
                "next_sequence excludes an existing event row",
            ));
        }

        transaction
            .commit()
            .map_err(|error| error::database_error("commit checkpoint verification", error))?;
        Ok(VerifiedSessionCheckpoints {
            checkpoint_count,
            latest_sequence,
        })
    }

    /// Verifies one complete session in a single SQLite read snapshot.
    ///
    /// Rows are visited in bounded metadata pages and each at-most-1-MiB
    /// payload is decoded, checked, and discarded before the next payload.
    /// Empty sessions return the zero genesis hash.
    pub fn verify_session_chain(
        &mut self,
        session_id: &SessionId,
    ) -> Result<VerifiedSessionChain, JournalError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(|error| error::database_error("begin hash-chain verification", error))?;
        validate_schema(&transaction)?;
        let state = query_session_state(&transaction, session_id)?.ok_or_else(|| {
            JournalError::SessionNotFound {
                session_id: session_id.clone(),
            }
        })?;
        let nonpositive_event: Option<i64> = transaction
            .query_row(NONPOSITIVE_EVENT_SQL, [session_id.as_str()], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|error| error::database_error("check nonpositive event sequences", error))?;
        if nonpositive_event.is_some() {
            return Err(corrupt_event(
                session_id,
                None,
                EventCorruption::InvalidColumn { field: "sequence" },
            ));
        }

        let mut expected_sequence = 1u64;
        let mut event_count = 0u64;
        let mut previous_hash = Hash::zero();
        while expected_sequence < state.next_sequence {
            let remaining = state.next_sequence - expected_sequence;
            let page_limit_u64 = remaining.min(MAX_EVENTS_PER_READ as u64);
            let page_limit = usize::try_from(page_limit_u64).map_err(|_| {
                session_corruption(
                    Some(session_id.to_string()),
                    "verification page size does not fit in usize",
                )
            })?;
            let metadata =
                read_event_metadata_page(&transaction, session_id, expected_sequence, page_limit)?;
            if metadata.is_empty() {
                return Err(corrupt_event(
                    session_id,
                    Some(expected_sequence),
                    EventCorruption::MissingRow,
                ));
            }

            let mut payload_statement = transaction
                .prepare(EVENT_PAYLOAD_SQL)
                .map_err(|error| error::database_error("prepare hash-chain payload read", error))?;
            for event_metadata in &metadata {
                let envelope =
                    read_event_payload(&mut payload_statement, session_id, event_metadata)?;
                verify_chain_link(session_id, &envelope, previous_hash)?;
                previous_hash = envelope.event_hash;
                event_count = event_count.checked_add(1).ok_or_else(|| {
                    session_corruption(
                        Some(session_id.to_string()),
                        "verified event count overflowed u64",
                    )
                })?;
                expected_sequence = expected_sequence.checked_add(1).ok_or_else(|| {
                    corrupt_event(
                        session_id,
                        Some(expected_sequence),
                        EventCorruption::InvalidColumn { field: "sequence" },
                    )
                })?;
            }
        }

        let extra = read_event_metadata_page(&transaction, session_id, expected_sequence, 1)?;
        if !extra.is_empty() {
            return Err(session_corruption(
                Some(session_id.to_string()),
                "next_sequence excludes an existing event row",
            ));
        }

        transaction
            .commit()
            .map_err(|error| error::database_error("commit hash-chain verification", error))?;
        Ok(VerifiedSessionChain {
            event_count,
            final_hash: previous_hash,
        })
    }

    /// Marks a session ended. Repeating the call is explicitly idempotent.
    pub fn end_session(&mut self, session_id: &SessionId) -> Result<SessionState, JournalError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error::database_error("begin end-session transaction", error))?;
        let mut state = query_session_state(&transaction, session_id)?.ok_or_else(|| {
            JournalError::SessionNotFound {
                session_id: session_id.clone(),
            }
        })?;
        if !state.ended {
            validate_schema(&transaction)?;
            let changed = transaction
                .execute(
                    "UPDATE sessions SET ended = 1 WHERE session_id = ?1 AND ended = 0",
                    [session_id.as_str()],
                )
                .map_err(|error| error::database_error("end session", error))?;
            if changed != 1 {
                return Err(JournalError::CorruptStorage(Corruption::Session {
                    session_id: Some(session_id.to_string()),
                    detail: "end transition did not update exactly one row".to_owned(),
                }));
            }
            state.ended = true;
        }
        transaction
            .commit()
            .map_err(|error| error::database_error("commit end-session transaction", error))?;
        Ok(state)
    }
}

fn require_session(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<SessionState, JournalError> {
    query_session_state(connection, session_id)?.ok_or_else(|| JournalError::SessionNotFound {
        session_id: session_id.clone(),
    })
}

fn query_exact_checkpoint_metadata(
    connection: &Connection,
    session_id: &SessionId,
    sequence: u64,
) -> Result<Option<CheckpointMetadata>, JournalError> {
    let sequence = to_sql_sequence(sequence)?;
    query_one_checkpoint_metadata(
        connection,
        EXACT_CHECKPOINT_METADATA_SQL,
        params![session_id.as_str(), sequence],
        session_id,
    )
}

fn query_one_checkpoint_metadata(
    connection: &Connection,
    sql: &'static str,
    parameters: impl rusqlite::Params,
    session_id: &SessionId,
) -> Result<Option<CheckpointMetadata>, JournalError> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| error::database_error("prepare checkpoint metadata", error))?;
    let mut rows = statement
        .query(parameters)
        .map_err(|error| error::database_error("query checkpoint metadata", error))?;
    let Some(row) = rows
        .next()
        .map_err(|error| error::database_error("read checkpoint metadata", error))?
    else {
        return Ok(None);
    };
    Ok(Some(decode_checkpoint_metadata(row, session_id)?))
}

fn read_checkpoint_metadata_page(
    connection: &Connection,
    session_id: &SessionId,
    after_sequence: u64,
    limit: usize,
) -> Result<Vec<CheckpointMetadata>, JournalError> {
    let after_sql = i64::try_from(after_sequence).map_err(|_| {
        checkpoint_corruption(
            session_id,
            Some(after_sequence),
            CheckpointCorruption::InvalidColumn { field: "sequence" },
        )
    })?;
    let limit_sql = i64::try_from(limit).map_err(|_| {
        session_corruption(
            Some(session_id.to_string()),
            "checkpoint verification page limit does not fit in SQLite INTEGER",
        )
    })?;
    let mut statement = connection
        .prepare(CHECKPOINT_VERIFICATION_PAGE_SQL)
        .map_err(|error| error::database_error("prepare checkpoint verification page", error))?;
    let mut rows = statement
        .query(params![session_id.as_str(), after_sql, limit_sql])
        .map_err(|error| error::database_error("query checkpoint verification page", error))?;
    let mut page = Vec::with_capacity(limit.min(16));
    let mut cursor = after_sequence;
    while let Some(row) = rows
        .next()
        .map_err(|error| error::database_error("read checkpoint verification row", error))?
    {
        let metadata = decode_checkpoint_metadata(row, session_id)?;
        if metadata.sequence <= cursor {
            return Err(checkpoint_corruption(
                session_id,
                Some(metadata.sequence),
                CheckpointCorruption::InvalidColumn { field: "sequence" },
            ));
        }
        cursor = metadata.sequence;
        page.push(metadata);
    }
    Ok(page)
}

fn reject_nonpositive_event_sequence(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<(), JournalError> {
    let found: Option<i64> = connection
        .query_row(NONPOSITIVE_EVENT_SQL, [session_id.as_str()], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|error| error::database_error("check nonpositive event sequences", error))?;
    if found.is_some() {
        return Err(corrupt_event(
            session_id,
            None,
            EventCorruption::InvalidColumn { field: "sequence" },
        ));
    }
    Ok(())
}

fn decode_checkpoint_metadata(
    row: &Row<'_>,
    requested_session: &SessionId,
) -> Result<CheckpointMetadata, JournalError> {
    let stored_session = decode_session_id(row, 0, 1, 2).map_err(|_| {
        checkpoint_corruption(
            requested_session,
            None,
            CheckpointCorruption::InvalidColumn {
                field: "session_id",
            },
        )
    })?;
    if stored_session != *requested_session {
        return Err(checkpoint_corruption(
            requested_session,
            None,
            CheckpointCorruption::ColumnMismatch {
                field: "session_id",
            },
        ));
    }
    let sequence = checkpoint_integer_column(row, 3, requested_session, None, "sequence")?;
    let sequence = u64::try_from(sequence).map_err(|_| {
        checkpoint_corruption(
            requested_session,
            None,
            CheckpointCorruption::InvalidColumn { field: "sequence" },
        )
    })?;
    if sequence == 0 || sequence > MAX_JOURNAL_SEQUENCE {
        return Err(checkpoint_corruption(
            requested_session,
            Some(sequence),
            CheckpointCorruption::InvalidColumn { field: "sequence" },
        ));
    }
    let payload_is_blob =
        checkpoint_integer_column(row, 4, requested_session, Some(sequence), "payload")?;
    if payload_is_blob != 1 {
        return Err(checkpoint_corruption(
            requested_session,
            Some(sequence),
            CheckpointCorruption::InvalidColumn { field: "payload" },
        ));
    }
    let payload_length =
        checkpoint_integer_column(row, 5, requested_session, Some(sequence), "length(payload)")?;
    let payload_length = u64::try_from(payload_length).map_err(|_| {
        checkpoint_corruption(
            requested_session,
            Some(sequence),
            CheckpointCorruption::InvalidColumn {
                field: "length(payload)",
            },
        )
    })?;
    if payload_length > MAX_CHECKPOINT_ENCODED_BYTES as u64 {
        return Err(checkpoint_corruption(
            requested_session,
            Some(sequence),
            CheckpointCorruption::OversizedPayload {
                actual: payload_length,
                maximum: MAX_CHECKPOINT_ENCODED_BYTES,
            },
        ));
    }
    Ok(CheckpointMetadata {
        sequence,
        payload_length,
    })
}

fn checkpoint_integer_column(
    row: &Row<'_>,
    index: usize,
    session_id: &SessionId,
    sequence: Option<u64>,
    field: &'static str,
) -> Result<i64, JournalError> {
    match row
        .get_ref(index)
        .map_err(|error| error::database_error("read checkpoint integer column", error))?
    {
        ValueRef::Integer(value) => Ok(value),
        _ => Err(checkpoint_corruption(
            session_id,
            sequence,
            CheckpointCorruption::InvalidColumn { field },
        )),
    }
}

fn load_optional_event(
    connection: &Connection,
    session_id: &SessionId,
    sequence: u64,
) -> Result<Option<EventEnvelope>, JournalError> {
    let sequence_sql = to_sql_sequence(sequence)?;
    let mut statement = connection
        .prepare(EXACT_EVENT_METADATA_SQL)
        .map_err(|error| error::database_error("prepare checkpoint owner", error))?;
    let mut rows = statement
        .query(params![session_id.as_str(), sequence_sql])
        .map_err(|error| error::database_error("query checkpoint owner", error))?;
    let Some(row) = rows
        .next()
        .map_err(|error| error::database_error("read checkpoint owner", error))?
    else {
        return Ok(None);
    };
    let metadata = decode_event_metadata(row, session_id, sequence)?;
    drop(rows);
    drop(statement);
    let mut payload_statement = connection
        .prepare(EVENT_PAYLOAD_SQL)
        .map_err(|error| error::database_error("prepare checkpoint owner payload", error))?;
    read_event_payload(&mut payload_statement, session_id, &metadata).map(Some)
}

fn load_stored_checkpoint(
    connection: &Connection,
    session_id: &SessionId,
    metadata: &CheckpointMetadata,
) -> Result<StoredCheckpoint, JournalError> {
    let owning_event = load_checkpoint_owner(connection, session_id, metadata.sequence)?;
    let state = require_session(connection, session_id)?;
    if metadata.sequence >= state.next_sequence {
        return Err(checkpoint_corruption(
            session_id,
            Some(metadata.sequence),
            CheckpointCorruption::SequenceOutsideSessionTail {
                next_sequence: state.next_sequence,
            },
        ));
    }
    let snapshot = with_checkpoint_payload(connection, session_id, metadata, |payload| {
        decode_checkpoint(payload).map_err(|source| {
            checkpoint_corruption(
                session_id,
                Some(metadata.sequence),
                CheckpointCorruption::MalformedPayload {
                    source: Box::new(source),
                },
            )
        })
    })?;
    snapshot.verify_owner(&owning_event).map_err(|source| {
        let kind = match source {
            CheckpointError::OwnerMismatch { field } => {
                CheckpointCorruption::OwnerMismatch { field }
            }
            source => CheckpointCorruption::MalformedPayload {
                source: Box::new(source),
            },
        };
        checkpoint_corruption(session_id, Some(metadata.sequence), kind)
    })?;
    Ok(StoredCheckpoint {
        owning_event,
        snapshot,
    })
}

fn load_checkpoint_owner(
    connection: &Connection,
    session_id: &SessionId,
    sequence: u64,
) -> Result<EventEnvelope, JournalError> {
    let owning_event = load_optional_event(connection, session_id, sequence)?.ok_or_else(|| {
        checkpoint_corruption(
            session_id,
            Some(sequence),
            CheckpointCorruption::OrphanPayload,
        )
    })?;
    let expected_previous_hash = authoritative_previous_hash(connection, session_id, sequence)?;
    verify_chain_link(session_id, &owning_event, expected_previous_hash)?;
    if !matches!(owning_event.event, Event::WorkspaceCheckpoint(_)) {
        return Err(checkpoint_corruption(
            session_id,
            Some(sequence),
            CheckpointCorruption::WrongOwningEventType,
        ));
    }
    Ok(owning_event)
}

fn with_checkpoint_payload<T>(
    connection: &Connection,
    session_id: &SessionId,
    metadata: &CheckpointMetadata,
    inspect: impl FnOnce(&[u8]) -> Result<T, JournalError>,
) -> Result<T, JournalError> {
    #[cfg(test)]
    CHECKPOINT_PAYLOAD_FETCH_COUNT.with(|count| count.set(count.get() + 1));
    let sequence = to_sql_sequence(metadata.sequence)?;
    let mut statement = connection
        .prepare(CHECKPOINT_PAYLOAD_SQL)
        .map_err(|error| error::database_error("prepare checkpoint payload", error))?;
    let mut rows = statement
        .query(params![session_id.as_str(), sequence])
        .map_err(|error| error::database_error("query checkpoint payload", error))?;
    let row = rows
        .next()
        .map_err(|error| error::database_error("read checkpoint payload row", error))?
        .ok_or_else(|| {
            checkpoint_corruption(
                session_id,
                Some(metadata.sequence),
                CheckpointCorruption::MissingPayload,
            )
        })?;
    let value = row
        .get_ref(0)
        .map_err(|error| error::database_error("read checkpoint payload", error))?;
    let ValueRef::Blob(payload) = value else {
        return Err(checkpoint_corruption(
            session_id,
            Some(metadata.sequence),
            CheckpointCorruption::InvalidColumn { field: "payload" },
        ));
    };
    if payload.len() as u64 != metadata.payload_length {
        return Err(checkpoint_corruption(
            session_id,
            Some(metadata.sequence),
            CheckpointCorruption::ColumnMismatch {
                field: "length(payload)",
            },
        ));
    }
    inspect(payload)
}

fn submitted_envelope(
    submission: EventSubmission,
    sequence: u64,
    previous_hash: Hash,
) -> Result<EventEnvelope, JournalError> {
    EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: submission.session_id,
        sequence,
        monotonic_millis: submission.monotonic_millis,
        wall_clock_utc: submission.wall_clock_utc,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event: submission.event,
    }
    .seal(previous_hash)
    .map_err(|source| JournalError::InvalidEvent { source })
}

fn insert_submitted_event(
    transaction: &Transaction<'_>,
    envelope: &EventEnvelope,
    payload: &[u8],
) -> Result<(), JournalError> {
    transaction
        .execute(
            "INSERT INTO events (session_id, sequence, format_version, \
         previous_event_hash, event_hash, payload) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                envelope.session_id.as_str(),
                to_sql_sequence(envelope.sequence)?,
                i64::from(envelope.format_version),
                envelope.previous_event_hash.as_bytes().as_slice(),
                envelope.event_hash.as_bytes().as_slice(),
                payload,
            ],
        )
        .map_err(|error| error::database_error("insert submitted event", error))?;
    Ok(())
}

fn enforce_append_chain(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    envelope: &EventEnvelope,
) -> Result<(), JournalError> {
    let expected_previous_hash =
        authoritative_previous_hash(transaction, session_id, envelope.sequence)?;

    verify_chain_link(session_id, envelope, expected_previous_hash)
}

fn authoritative_previous_hash(
    connection: &Connection,
    session_id: &SessionId,
    sequence: u64,
) -> Result<Hash, JournalError> {
    if sequence == 1 {
        Ok(Hash::zero())
    } else {
        let previous_sequence = sequence.checked_sub(1).ok_or_else(|| {
            corrupt_event(
                session_id,
                Some(sequence),
                EventCorruption::InvalidColumn { field: "sequence" },
            )
        })?;
        let previous_sql = to_sql_sequence(previous_sequence)?;
        let mut statement = connection
            .prepare(EXACT_EVENT_METADATA_SQL)
            .map_err(|error| error::database_error("prepare prior event hash read", error))?;
        let mut rows = statement
            .query(params![session_id.as_str(), previous_sql])
            .map_err(|error| error::database_error("query prior event hash", error))?;
        let row = rows
            .next()
            .map_err(|error| error::database_error("read prior event hash", error))?
            .ok_or_else(|| {
                corrupt_event(
                    session_id,
                    Some(previous_sequence),
                    EventCorruption::MissingRow,
                )
            })?;
        Ok(decode_event_metadata(row, session_id, previous_sequence)?.event_hash)
    }
}

fn verify_chain_link(
    session_id: &SessionId,
    envelope: &EventEnvelope,
    expected_previous_hash: Hash,
) -> Result<(), JournalError> {
    if envelope.previous_event_hash != expected_previous_hash {
        let kind = if envelope.sequence == 1 {
            ChainMismatch::GenesisPreviousHash {
                expected: expected_previous_hash,
                actual: envelope.previous_event_hash,
            }
        } else {
            ChainMismatch::PreviousHash {
                expected: expected_previous_hash,
                actual: envelope.previous_event_hash,
            }
        };
        return Err(chain_error(session_id, envelope.sequence, kind));
    }

    let expected_event_hash = compute_event_hash(expected_previous_hash, envelope)
        .map_err(|source| JournalError::InvalidEvent { source })?;
    if envelope.event_hash != expected_event_hash {
        return Err(chain_error(
            session_id,
            envelope.sequence,
            ChainMismatch::EventHash {
                expected: expected_event_hash,
                actual: envelope.event_hash,
            },
        ));
    }
    Ok(())
}

fn query_session_state(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<Option<SessionState>, JournalError> {
    let mut statement = connection
        .prepare(SESSION_LOOKUP_SQL)
        .map_err(|error| error::database_error("prepare session lookup", error))?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(|error| error::database_error("query session", error))?;
    let Some(row) = rows
        .next()
        .map_err(|error| error::database_error("read session", error))?
    else {
        return Ok(None);
    };
    Ok(Some(decode_session_row(row)?))
}

fn query_only_unfinished_session(
    transaction: &Transaction<'_>,
) -> Result<SessionState, JournalError> {
    let mut statement = transaction
        .prepare(UNFINISHED_SESSION_SQL)
        .map_err(|error| error::database_error("prepare resumable session lookup", error))?;
    let mut rows = statement
        .query([])
        .map_err(|error| error::database_error("query resumable session", error))?;
    let row = rows
        .next()
        .map_err(|error| error::database_error("read resumable session", error))?
        .ok_or_else(|| {
            JournalError::CorruptStorage(Corruption::Session {
                session_id: None,
                detail: "unfinished session count changed inside transaction".to_owned(),
            })
        })?;
    decode_session_row(row)
}

fn decode_session_row(row: &Row<'_>) -> Result<SessionState, JournalError> {
    let session_id = decode_session_id(row, 0, 1, 2)?;
    let next_sequence = integer_column(row, 3, "next_sequence", &session_id)?;
    let next_sequence = u64::try_from(next_sequence).map_err(|_| {
        JournalError::CorruptStorage(Corruption::Session {
            session_id: Some(session_id.to_string()),
            detail: "next_sequence is negative".to_owned(),
        })
    })?;
    if next_sequence == 0 || next_sequence > i64::MAX as u64 {
        return Err(JournalError::CorruptStorage(Corruption::Session {
            session_id: Some(session_id.to_string()),
            detail: "next_sequence is outside the journal range".to_owned(),
        }));
    }
    let ended = match integer_column(row, 4, "ended", &session_id)? {
        0 => false,
        1 => true,
        _ => {
            return Err(JournalError::CorruptStorage(Corruption::Session {
                session_id: Some(session_id.to_string()),
                detail: "ended is neither 0 nor 1".to_owned(),
            }));
        }
    };
    Ok(SessionState {
        session_id,
        next_sequence,
        ended,
    })
}

pub(crate) fn decode_session_id(
    row: &Row<'_>,
    type_index: usize,
    length_index: usize,
    prefix_index: usize,
) -> Result<SessionId, JournalError> {
    let is_text = match row
        .get_ref(type_index)
        .map_err(|error| error::database_error("read session_id type", error))?
    {
        ValueRef::Integer(value) => value == 1,
        _ => false,
    };
    if !is_text {
        return Err(session_corruption(None, "session_id is not text"));
    }

    let length = match row
        .get_ref(length_index)
        .map_err(|error| error::database_error("read session_id length", error))?
    {
        ValueRef::Integer(value) => value,
        _ => {
            return Err(session_corruption(
                None,
                "session_id length is not an integer",
            ));
        }
    };
    let length = usize::try_from(length)
        .map_err(|_| session_corruption(None, "session_id has a negative byte length"))?;
    if !(1..=MAX_IDENTIFIER_BYTES).contains(&length) {
        return Err(session_corruption(
            None,
            format!("session_id byte length {length} is outside 1..={MAX_IDENTIFIER_BYTES}"),
        ));
    }

    let prefix = row
        .get_ref(prefix_index)
        .map_err(|error| error::database_error("read bounded session_id", error))?;
    let ValueRef::Blob(prefix) = prefix else {
        return Err(session_corruption(None, "session_id prefix is not a blob"));
    };
    if prefix.len() != length {
        return Err(session_corruption(
            None,
            "session_id length changed while reading",
        ));
    }
    let session_text = std::str::from_utf8(prefix)
        .map_err(|_| session_corruption(None, "session_id is not UTF-8"))?;
    SessionId::new(session_text)
        .map_err(|source| session_corruption(Some(session_text.to_owned()), source.to_string()))
}

fn decode_event_metadata(
    row: &Row<'_>,
    requested_session: &SessionId,
    expected_sequence: u64,
) -> Result<EventMetadata, JournalError> {
    let stored_session = decode_session_id(row, 0, 1, 2).map_err(|_| {
        corrupt_event(
            requested_session,
            Some(expected_sequence),
            EventCorruption::InvalidColumn {
                field: "session_id",
            },
        )
    })?;
    if &stored_session != requested_session {
        return Err(corrupt_event(
            requested_session,
            Some(expected_sequence),
            EventCorruption::ColumnMismatch {
                field: "session_id",
            },
        ));
    }

    let stored_sequence =
        event_integer_column(row, 3, requested_session, expected_sequence, "sequence")?;
    let stored_sequence = u64::try_from(stored_sequence).map_err(|_| {
        corrupt_event(
            requested_session,
            Some(expected_sequence),
            EventCorruption::InvalidColumn { field: "sequence" },
        )
    })?;
    if stored_sequence == 0 || stored_sequence > MAX_JOURNAL_SEQUENCE {
        return Err(corrupt_event(
            requested_session,
            Some(stored_sequence),
            EventCorruption::InvalidColumn { field: "sequence" },
        ));
    }
    if stored_sequence != expected_sequence {
        return Err(corrupt_event(
            requested_session,
            Some(stored_sequence),
            EventCorruption::SequenceGap {
                expected: expected_sequence,
                actual: stored_sequence,
            },
        ));
    }

    let stored_format =
        event_integer_column(row, 4, requested_session, stored_sequence, "format_version")?;
    let stored_format = u32::try_from(stored_format).map_err(|_| {
        corrupt_event(
            requested_session,
            Some(stored_sequence),
            EventCorruption::InvalidColumn {
                field: "format_version",
            },
        )
    })?;
    let previous_event_hash = hash_metadata_column(
        row,
        5,
        6,
        7,
        requested_session,
        stored_sequence,
        "previous_event_hash",
    )?;
    let event_hash = hash_metadata_column(
        row,
        8,
        9,
        10,
        requested_session,
        stored_sequence,
        "event_hash",
    )?;

    let payload_is_blob = event_integer_column(
        row,
        11,
        requested_session,
        stored_sequence,
        "typeof(payload)",
    )?;
    if payload_is_blob != 1 {
        return Err(corrupt_event(
            requested_session,
            Some(stored_sequence),
            EventCorruption::InvalidColumn { field: "payload" },
        ));
    }
    let payload_length = event_integer_column(
        row,
        12,
        requested_session,
        stored_sequence,
        "length(payload)",
    )?;
    let payload_length = u64::try_from(payload_length).map_err(|_| {
        corrupt_event(
            requested_session,
            Some(stored_sequence),
            EventCorruption::InvalidColumn {
                field: "length(payload)",
            },
        )
    })?;
    let maximum_payload_bytes = u64::try_from(MAX_ENVELOPE_BYTES).unwrap_or(u64::MAX);
    if payload_length > maximum_payload_bytes {
        return Err(corrupt_event(
            requested_session,
            Some(stored_sequence),
            EventCorruption::OversizedPayload {
                actual: payload_length,
                maximum: MAX_ENVELOPE_BYTES,
            },
        ));
    }

    Ok(EventMetadata {
        sequence: stored_sequence,
        format_version: stored_format,
        previous_event_hash,
        event_hash,
        payload_length,
    })
}

fn read_event_metadata_page(
    connection: &Connection,
    session_id: &SessionId,
    first_sequence: u64,
    limit: usize,
) -> Result<Vec<EventMetadata>, JournalError> {
    let first_sql = i64::try_from(first_sequence).map_err(|_| {
        session_corruption(
            Some(session_id.to_string()),
            "verification sequence does not fit in SQLite INTEGER",
        )
    })?;
    let limit_sql = i64::try_from(limit).map_err(|_| {
        session_corruption(
            Some(session_id.to_string()),
            "verification page limit does not fit in SQLite INTEGER",
        )
    })?;
    let mut statement = connection
        .prepare(EVENT_METADATA_SQL)
        .map_err(|error| error::database_error("prepare event metadata page", error))?;
    let mut rows = statement
        .query(params![session_id.as_str(), first_sql, limit_sql])
        .map_err(|error| error::database_error("query event metadata page", error))?;
    let mut metadata = Vec::with_capacity(limit.min(16));
    let mut expected_sequence = first_sequence;
    while let Some(row) = rows
        .next()
        .map_err(|error| error::database_error("read event metadata page", error))?
    {
        metadata.push(decode_event_metadata(row, session_id, expected_sequence)?);
        expected_sequence = expected_sequence.checked_add(1).ok_or_else(|| {
            corrupt_event(
                session_id,
                Some(expected_sequence),
                EventCorruption::InvalidColumn { field: "sequence" },
            )
        })?;
    }
    Ok(metadata)
}

fn read_event_payload(
    statement: &mut Statement<'_>,
    requested_session: &SessionId,
    metadata: &EventMetadata,
) -> Result<EventEnvelope, JournalError> {
    with_event_payload(statement, requested_session, metadata, |payload| {
        decode_event_payload(payload, requested_session, metadata)
    })
}

fn decode_event_payload(
    payload: &[u8],
    requested_session: &SessionId,
    metadata: &EventMetadata,
) -> Result<EventEnvelope, JournalError> {
    let stored_sequence = metadata.sequence;
    let decoded = decode_envelope(payload, DecodePolicy::RejectUnsupported).map_err(|error| {
        corrupt_event(
            requested_session,
            Some(stored_sequence),
            EventCorruption::MalformedPayload {
                detail: error.to_string(),
            },
        )
    })?;
    let DecodeOutcome::Decoded(envelope) = decoded else {
        return Err(corrupt_event(
            requested_session,
            Some(stored_sequence),
            EventCorruption::MalformedPayload {
                detail: "unsupported event passed reject policy".to_owned(),
            },
        ));
    };

    for (matches, field) in [
        (envelope.session_id == *requested_session, "session_id"),
        (envelope.sequence == stored_sequence, "sequence"),
        (
            envelope.format_version == metadata.format_version,
            "format_version",
        ),
        (
            envelope.previous_event_hash == metadata.previous_event_hash,
            "previous_event_hash",
        ),
        (envelope.event_hash == metadata.event_hash, "event_hash"),
    ] {
        if !matches {
            return Err(corrupt_event(
                requested_session,
                Some(stored_sequence),
                EventCorruption::ColumnMismatch { field },
            ));
        }
    }
    let canonical = encode_envelope(&envelope).map_err(|error| {
        corrupt_event(
            requested_session,
            Some(stored_sequence),
            EventCorruption::MalformedPayload {
                detail: error.to_string(),
            },
        )
    })?;
    if canonical != payload {
        return Err(corrupt_event(
            requested_session,
            Some(stored_sequence),
            EventCorruption::NonCanonicalPayload,
        ));
    }
    Ok(envelope)
}

fn integer_column(
    row: &Row<'_>,
    index: usize,
    field: &'static str,
    session_id: &SessionId,
) -> Result<i64, JournalError> {
    match row
        .get_ref(index)
        .map_err(|error| error::database_error("read integer column", error))?
    {
        ValueRef::Integer(value) => Ok(value),
        _ => Err(JournalError::CorruptStorage(Corruption::Session {
            session_id: Some(session_id.to_string()),
            detail: format!("{field} is not an integer"),
        })),
    }
}

fn event_integer_column(
    row: &Row<'_>,
    index: usize,
    session_id: &SessionId,
    sequence: u64,
    field: &'static str,
) -> Result<i64, JournalError> {
    match row
        .get_ref(index)
        .map_err(|error| error::database_error("read event integer column", error))?
    {
        ValueRef::Integer(value) => Ok(value),
        _ => Err(corrupt_event(
            session_id,
            Some(sequence),
            EventCorruption::InvalidColumn { field },
        )),
    }
}

fn hash_metadata_column(
    row: &Row<'_>,
    type_index: usize,
    length_index: usize,
    prefix_index: usize,
    session_id: &SessionId,
    sequence: u64,
    field: &'static str,
) -> Result<Hash, JournalError> {
    if event_integer_column(row, type_index, session_id, sequence, field)? != 1 {
        return Err(corrupt_event(
            session_id,
            Some(sequence),
            EventCorruption::InvalidColumn { field },
        ));
    }
    let length = event_integer_column(row, length_index, session_id, sequence, field)?;
    if length != Hash::LENGTH as i64 {
        return Err(corrupt_event(
            session_id,
            Some(sequence),
            EventCorruption::InvalidColumn { field },
        ));
    }
    let value = row
        .get_ref(prefix_index)
        .map_err(|error| error::database_error("read event hash", error))?;
    let ValueRef::Blob(bytes) = value else {
        return Err(corrupt_event(
            session_id,
            Some(sequence),
            EventCorruption::InvalidColumn { field },
        ));
    };
    let bytes: [u8; Hash::LENGTH] = bytes.try_into().map_err(|_| {
        corrupt_event(
            session_id,
            Some(sequence),
            EventCorruption::InvalidColumn { field },
        )
    })?;
    Ok(Hash::from_bytes(bytes))
}

fn with_event_payload<T>(
    statement: &mut Statement<'_>,
    session_id: &SessionId,
    metadata: &EventMetadata,
    inspect: impl FnOnce(&[u8]) -> Result<T, JournalError>,
) -> Result<T, JournalError> {
    #[cfg(test)]
    PAYLOAD_FETCH_COUNT.with(|count| count.set(count.get() + 1));
    let sequence = to_sql_sequence(metadata.sequence)?;
    let mut rows = statement
        .query(params![session_id.as_str(), sequence])
        .map_err(|error| error::database_error("query event payload", error))?;
    let row = rows
        .next()
        .map_err(|error| error::database_error("read event payload row", error))?
        .ok_or_else(|| {
            corrupt_event(
                session_id,
                Some(metadata.sequence),
                EventCorruption::MissingRow,
            )
        })?;
    let value = row
        .get_ref(0)
        .map_err(|error| error::database_error("read event payload", error))?;
    let ValueRef::Blob(payload) = value else {
        return Err(corrupt_event(
            session_id,
            Some(metadata.sequence),
            EventCorruption::InvalidColumn { field: "payload" },
        ));
    };
    let payload_length = u64::try_from(payload.len()).map_err(|_| {
        corrupt_event(
            session_id,
            Some(metadata.sequence),
            EventCorruption::InvalidColumn {
                field: "length(payload)",
            },
        )
    })?;
    if payload_length != metadata.payload_length {
        return Err(corrupt_event(
            session_id,
            Some(metadata.sequence),
            EventCorruption::ColumnMismatch {
                field: "length(payload)",
            },
        ));
    }
    inspect(payload)
}

#[cfg(test)]
std::thread_local! {
    static PAYLOAD_FETCH_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static CHECKPOINT_PAYLOAD_FETCH_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn verify_append(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    envelope: &EventEnvelope,
    payload: &[u8],
    next_sequence: u64,
) -> Result<(), JournalError> {
    let sequence = to_sql_sequence(envelope.sequence)?;
    let metadata = {
        let mut statement = transaction
            .prepare(EXACT_EVENT_METADATA_SQL)
            .map_err(|error| error::database_error("prepare appended event verification", error))?;
        let mut rows = statement
            .query(params![session_id.as_str(), sequence])
            .map_err(|error| error::database_error("query appended event metadata", error))?;
        let row = rows
            .next()
            .map_err(|error| error::database_error("read appended event metadata", error))?
            .ok_or_else(|| {
                corrupt_event(
                    session_id,
                    Some(envelope.sequence),
                    EventCorruption::MissingRow,
                )
            })?;
        decode_event_metadata(row, session_id, envelope.sequence)?
    };
    for (matches, field) in [
        (
            metadata.format_version == envelope.format_version,
            "format_version",
        ),
        (
            metadata.previous_event_hash == envelope.previous_event_hash,
            "previous_event_hash",
        ),
        (metadata.event_hash == envelope.event_hash, "event_hash"),
    ] {
        if !matches {
            return Err(corrupt_event(
                session_id,
                Some(envelope.sequence),
                EventCorruption::ColumnMismatch { field },
            ));
        }
    }

    let mut payload_statement = transaction
        .prepare(EVENT_PAYLOAD_SQL)
        .map_err(|error| error::database_error("prepare appended payload verification", error))?;
    with_event_payload(
        &mut payload_statement,
        session_id,
        &metadata,
        |stored_payload| {
            if stored_payload == payload {
                Ok(())
            } else {
                Err(corrupt_event(
                    session_id,
                    Some(envelope.sequence),
                    EventCorruption::ColumnMismatch { field: "payload" },
                ))
            }
        },
    )?;
    drop(payload_statement);

    let state = query_session_state(transaction, session_id)?.ok_or_else(|| {
        session_corruption(
            Some(session_id.to_string()),
            "session row disappeared during append",
        )
    })?;
    if state.next_sequence != next_sequence || state.ended {
        return Err(session_corruption(
            Some(session_id.to_string()),
            "session state changed unexpectedly during append",
        ));
    }
    Ok(())
}

fn session_corruption(session_id: Option<String>, detail: impl Into<String>) -> JournalError {
    JournalError::CorruptStorage(Corruption::Session {
        session_id,
        detail: detail.into(),
    })
}

fn corrupt_event(
    session_id: &SessionId,
    sequence: Option<u64>,
    kind: EventCorruption,
) -> JournalError {
    JournalError::CorruptStorage(Corruption::Event {
        session_id: session_id.to_string(),
        sequence,
        kind,
    })
}

fn checkpoint_corruption(
    session_id: &SessionId,
    sequence: Option<u64>,
    kind: CheckpointCorruption,
) -> JournalError {
    JournalError::CorruptStorage(Corruption::Checkpoint {
        session_id: session_id.to_string(),
        sequence,
        kind,
    })
}

fn chain_error(session_id: &SessionId, sequence: u64, kind: ChainMismatch) -> JournalError {
    JournalError::Chain {
        session_id: session_id.clone(),
        sequence,
        kind,
    }
}

fn to_sql_sequence(sequence: u64) -> Result<i64, JournalError> {
    if sequence > MAX_JOURNAL_SEQUENCE {
        return Err(JournalError::SequenceOutOfRange {
            actual: sequence,
            maximum: MAX_JOURNAL_SEQUENCE,
        });
    }
    i64::try_from(sequence).map_err(|_| JournalError::SequenceOutOfRange {
        actual: sequence,
        maximum: MAX_JOURNAL_SEQUENCE,
    })
}

#[cfg(test)]
mod tests;
