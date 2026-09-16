use std::{error::Error, fmt, io};

use rusqlite::ffi::ErrorCode;
use rustrace_model::{EncodeError, Hash, SessionId};

use crate::CheckpointError;

/// The exact hash-chain inconsistency found at an append or verify boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChainMismatch {
    GenesisPreviousHash { expected: Hash, actual: Hash },
    PreviousHash { expected: Hash, actual: Hash },
    EventHash { expected: Hash, actual: Hash },
}

/// A storage inconsistency discovered at an initialization or read boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Corruption {
    DatabaseFile {
        detail: String,
    },
    IntegrityCheck {
        detail: String,
    },
    ForeignKeyViolation,
    Schema {
        detail: String,
    },
    Session {
        session_id: Option<String>,
        detail: String,
    },
    Event {
        session_id: String,
        sequence: Option<u64>,
        kind: EventCorruption,
    },
    Checkpoint {
        session_id: String,
        sequence: Option<u64>,
        kind: CheckpointCorruption,
    },
}

/// The bounded event-row problem found while loading canonical payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventCorruption {
    OversizedPayload { actual: u64, maximum: usize },
    MalformedPayload { detail: String },
    MissingRow,
    InvalidColumn { field: &'static str },
    ColumnMismatch { field: &'static str },
    NonCanonicalPayload,
    SequenceGap { expected: u64, actual: u64 },
}

/// The bounded checkpoint-row problem found while loading or verifying.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckpointCorruption {
    OversizedPayload { actual: u64, maximum: usize },
    MalformedPayload { source: Box<CheckpointError> },
    MissingPayload,
    OrphanPayload,
    WrongOwningEventType,
    InvalidColumn { field: &'static str },
    ColumnMismatch { field: &'static str },
    OwnerMismatch { field: &'static str },
    SequenceOutsideSessionTail { next_sequence: u64 },
}

#[derive(Debug)]
pub enum JournalError {
    Io {
        operation: &'static str,
        source: io::Error,
    },
    Database {
        operation: &'static str,
        source: rusqlite::Error,
    },
    CorruptStorage(Corruption),
    UnsupportedSchemaVersion {
        found: u32,
        supported: u32,
    },
    InvalidSchemaVersion {
        found: i64,
    },
    UnversionedSchema,
    InvalidEvent {
        source: EncodeError,
    },
    InvalidCheckpoint {
        source: Box<CheckpointError>,
    },
    CheckpointRequiresAtomicAppend,
    Chain {
        session_id: SessionId,
        sequence: u64,
        kind: ChainMismatch,
    },
    SessionNotFound {
        session_id: SessionId,
    },
    SessionAlreadyExists {
        session_id: SessionId,
    },
    AmbiguousUnfinishedSessions {
        count: u64,
    },
    SessionEnded {
        session_id: SessionId,
    },
    WrongSession {
        expected: SessionId,
        actual: SessionId,
    },
    UnexpectedSequence {
        session_id: SessionId,
        expected: u64,
        actual: u64,
    },
    SequenceOutOfRange {
        actual: u64,
        maximum: u64,
    },
    InvalidReadStart {
        actual: u64,
    },
    ReadLimitTooLarge {
        actual: usize,
        maximum: usize,
    },
    ReadBatchTooLarge {
        actual: u64,
        maximum: usize,
    },
    CheckpointReadLimitTooLarge {
        actual: usize,
        maximum: usize,
    },
    CheckpointReadBatchTooLarge {
        actual: u64,
        maximum: usize,
    },
    StorageConfiguration {
        setting: &'static str,
        expected: &'static str,
        actual: String,
    },
}

impl fmt::Display for Corruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseFile { detail } => write!(formatter, "invalid SQLite file: {detail}"),
            Self::IntegrityCheck { detail } => {
                write!(formatter, "SQLite quick_check failed: {detail}")
            }
            Self::ForeignKeyViolation => formatter.write_str("SQLite foreign key check failed"),
            Self::Schema { detail } => write!(formatter, "invalid journal schema: {detail}"),
            Self::Session { session_id, detail } => match session_id {
                Some(session_id) => write!(formatter, "invalid session {session_id:?}: {detail}"),
                None => write!(formatter, "invalid session row: {detail}"),
            },
            Self::Event {
                session_id,
                sequence,
                kind,
            } => match sequence {
                Some(sequence) => write!(
                    formatter,
                    "invalid event row for session {session_id:?} at sequence {sequence}: {kind}"
                ),
                None => write!(
                    formatter,
                    "invalid event row for session {session_id:?}: {kind}"
                ),
            },
            Self::Checkpoint {
                session_id,
                sequence,
                kind,
            } => match sequence {
                Some(sequence) => write!(
                    formatter,
                    "invalid checkpoint row for session {session_id:?} at sequence {sequence}: {kind}"
                ),
                None => write!(
                    formatter,
                    "invalid checkpoint row for session {session_id:?}: {kind}"
                ),
            },
        }
    }
}

impl fmt::Display for EventCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OversizedPayload { actual, maximum } => write!(
                formatter,
                "payload is {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::MalformedPayload { detail } => write!(formatter, "malformed payload: {detail}"),
            Self::MissingRow => formatter.write_str("event row is missing"),
            Self::InvalidColumn { field } => write!(formatter, "column {field} is invalid"),
            Self::ColumnMismatch { field } => {
                write!(formatter, "column {field} disagrees with the payload")
            }
            Self::NonCanonicalPayload => formatter.write_str("payload is not canonical encoding"),
            Self::SequenceGap { expected, actual } => {
                write!(formatter, "expected sequence {expected}, found {actual}")
            }
        }
    }
}

impl fmt::Display for CheckpointCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OversizedPayload { actual, maximum } => write!(
                formatter,
                "payload is {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::MalformedPayload { source } => write!(formatter, "malformed payload: {source}"),
            Self::MissingPayload => formatter.write_str("owning checkpoint event has no payload"),
            Self::OrphanPayload => formatter.write_str("payload has no owning event"),
            Self::WrongOwningEventType => {
                formatter.write_str("payload owner is not a workspace_checkpoint event")
            }
            Self::InvalidColumn { field } => write!(formatter, "column {field} is invalid"),
            Self::ColumnMismatch { field } => {
                write!(formatter, "column {field} disagrees with its owner")
            }
            Self::OwnerMismatch { field } => {
                write!(
                    formatter,
                    "payload disagrees with owning event field {field}"
                )
            }
            Self::SequenceOutsideSessionTail { next_sequence } => write!(
                formatter,
                "checkpoint sequence is outside 1..{next_sequence} for the session"
            ),
        }
    }
}

impl fmt::Display for JournalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::Database { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::CorruptStorage(corruption) => corruption.fmt(formatter),
            Self::UnsupportedSchemaVersion { found, supported } => write!(
                formatter,
                "journal schema version {found} is newer than supported version {supported}"
            ),
            Self::InvalidSchemaVersion { found } => {
                write!(formatter, "journal schema version {found} is invalid")
            }
            Self::UnversionedSchema => formatter.write_str(
                "database has application tables but no recognized journal schema version",
            ),
            Self::InvalidEvent { source } => {
                write!(formatter, "event is not persistable: {source}")
            }
            Self::InvalidCheckpoint { source } => {
                write!(formatter, "checkpoint is not persistable: {source}")
            }
            Self::CheckpointRequiresAtomicAppend => formatter
                .write_str("workspace_checkpoint events must use the atomic checkpoint append API"),
            Self::Chain {
                session_id,
                sequence,
                kind,
            } => write!(
                formatter,
                "event hash chain mismatch for session {session_id} at sequence {sequence}: {kind}"
            ),
            Self::SessionNotFound { session_id } => {
                write!(formatter, "session {session_id} does not exist")
            }
            Self::SessionAlreadyExists { session_id } => {
                write!(
                    formatter,
                    "session {session_id} already exists and is ended"
                )
            }
            Self::AmbiguousUnfinishedSessions { count } => write!(
                formatter,
                "journal contains {count} unfinished sessions; automatic resume is ambiguous"
            ),
            Self::SessionEnded { session_id } => {
                write!(formatter, "session {session_id} has ended")
            }
            Self::WrongSession { expected, actual } => write!(
                formatter,
                "event belongs to session {actual}; expected session {expected}"
            ),
            Self::UnexpectedSequence {
                session_id,
                expected,
                actual,
            } => write!(
                formatter,
                "session {session_id} expects sequence {expected}, received {actual}"
            ),
            Self::SequenceOutOfRange { actual, maximum } => write!(
                formatter,
                "event sequence {actual} exceeds journal maximum {maximum}"
            ),
            Self::InvalidReadStart { actual } => {
                write!(
                    formatter,
                    "event reads must start at sequence 1 or later, got {actual}"
                )
            }
            Self::ReadLimitTooLarge { actual, maximum } => write!(
                formatter,
                "event read limit {actual} exceeds maximum {maximum}"
            ),
            Self::ReadBatchTooLarge { actual, maximum } => write!(
                formatter,
                "event read batch is {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::CheckpointReadLimitTooLarge { actual, maximum } => write!(
                formatter,
                "checkpoint read limit {actual} exceeds maximum {maximum}"
            ),
            Self::CheckpointReadBatchTooLarge { actual, maximum } => write!(
                formatter,
                "checkpoint read batch is {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::StorageConfiguration {
                setting,
                expected,
                actual,
            } => write!(
                formatter,
                "SQLite setting {setting} is {actual:?}; expected {expected}"
            ),
        }
    }
}

impl fmt::Display for ChainMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GenesisPreviousHash { expected, actual } => write!(
                formatter,
                "genesis previous hash is {actual}; expected {expected}"
            ),
            Self::PreviousHash { expected, actual } => write!(
                formatter,
                "previous hash is {actual}; authoritative prior event hash is {expected}"
            ),
            Self::EventHash { expected, actual } => {
                write!(
                    formatter,
                    "event hash is {actual}; computed hash is {expected}"
                )
            }
        }
    }
}

impl Error for JournalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Database { source, .. } => Some(source),
            Self::InvalidEvent { source } => Some(source),
            Self::InvalidCheckpoint { source } => Some(source.as_ref()),
            _ => None,
        }
    }
}

pub(crate) fn database_error(operation: &'static str, source: rusqlite::Error) -> JournalError {
    let corrupt = matches!(
        &source,
        rusqlite::Error::SqliteFailure(error, _)
            if matches!(
                error.code,
                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase | ErrorCode::TooBig
            )
    );
    if corrupt {
        JournalError::CorruptStorage(Corruption::DatabaseFile {
            detail: source.to_string(),
        })
    } else {
        JournalError::Database { operation, source }
    }
}
