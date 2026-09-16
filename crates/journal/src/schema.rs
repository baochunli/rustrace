use rusqlite::{
    Connection, OptionalExtension, TransactionBehavior, limits::Limit, types::ValueRef,
};

use crate::{
    BUSY_TIMEOUT, Corruption, JournalError, SCHEMA_VERSION, decode_session_id, decode_session_row,
    error::database_error,
};

const SESSIONS_SQL: &str = r#"CREATE TABLE sessions (
    session_id TEXT PRIMARY KEY NOT NULL CHECK (
        length(CAST(session_id AS BLOB)) BETWEEN 1 AND 128
        AND session_id NOT GLOB '*[^A-Za-z0-9_.:-]*'
        AND instr(CAST(session_id AS BLOB), X'00') = 0
    ),
    next_sequence INTEGER NOT NULL CHECK (
        typeof(next_sequence) = 'integer' AND next_sequence >= 1
    ),
    ended INTEGER NOT NULL DEFAULT 0 CHECK (
        typeof(ended) = 'integer' AND ended IN (0, 1)
    )
) STRICT"#;

const EVENTS_SQL: &str = r#"CREATE TABLE events (
    session_id TEXT NOT NULL,
    sequence INTEGER NOT NULL CHECK (
        typeof(sequence) = 'integer' AND sequence >= 1
    ),
    format_version INTEGER NOT NULL CHECK (
        typeof(format_version) = 'integer' AND format_version >= 1
    ),
    previous_event_hash BLOB NOT NULL CHECK (length(previous_event_hash) = 32),
    event_hash BLOB NOT NULL CHECK (length(event_hash) = 32),
    payload BLOB NOT NULL CHECK (length(payload) <= 1048576),
    PRIMARY KEY (session_id, sequence),
    FOREIGN KEY (session_id) REFERENCES sessions(session_id) ON DELETE RESTRICT
) STRICT, WITHOUT ROWID"#;

const CHECKPOINTS_SQL: &str = r#"CREATE TABLE checkpoints (
    session_id TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    payload BLOB NOT NULL,
    PRIMARY KEY (session_id, sequence),
    FOREIGN KEY (session_id, sequence)
        REFERENCES events(session_id, sequence) ON DELETE RESTRICT
) STRICT, WITHOUT ROWID"#;

const DOCUMENTS_SQL: &str = r#"CREATE TABLE documents (
    session_id TEXT NOT NULL,
    document_id TEXT NOT NULL,
    path TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (typeof(version) = 'integer' AND version >= 0),
    content_hash BLOB NOT NULL CHECK (length(content_hash) = 32),
    PRIMARY KEY (session_id, document_id),
    FOREIGN KEY (session_id) REFERENCES sessions(session_id) ON DELETE RESTRICT
) STRICT, WITHOUT ROWID"#;

const METADATA_SQL: &str = r#"CREATE TABLE metadata (
    key TEXT PRIMARY KEY NOT NULL,
    value BLOB NOT NULL
) STRICT, WITHOUT ROWID"#;

const TABLES: [(&str, &str); 5] = [
    ("sessions", SESSIONS_SQL),
    ("events", EVENTS_SQL),
    ("checkpoints", CHECKPOINTS_SQL),
    ("documents", DOCUMENTS_SQL),
    ("metadata", METADATA_SQL),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StorageKind {
    File,
    Memory,
}

pub(crate) fn initialize(
    connection: &mut Connection,
    kind: StorageKind,
) -> Result<(), JournalError> {
    initialize_with_hook(connection, kind, || {})
}

/// Applies connection-local safety limits and validates an existing schema.
/// It deliberately performs no schema bootstrap, journal-mode change, PRAGMA
/// write, or database-wide content scan.
pub(crate) fn validate_read_only(connection: &Connection) -> Result<(), JournalError> {
    configure_length_limit(connection)?;
    configure_busy_timeout(connection)?;
    validate_schema(connection)
}

#[cfg(test)]
pub(crate) fn initialize_with_bootstrap_hook(
    connection: &mut Connection,
    kind: StorageKind,
    before_bootstrap_lock: impl FnOnce(),
) -> Result<(), JournalError> {
    initialize_with_hook(connection, kind, before_bootstrap_lock)
}

fn initialize_with_hook(
    connection: &mut Connection,
    kind: StorageKind,
    before_bootstrap_lock: impl FnOnce(),
) -> Result<(), JournalError> {
    configure_length_limit(connection)?;
    configure_busy_timeout(connection)?;
    before_bootstrap_lock();
    initialize_schema(connection)?;
    configure(connection, kind)?;

    let validation = connection
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .map_err(|error| database_error("begin journal validation", error))?;
    validate_schema(&validation)?;
    validate_session_rows(&validation)?;
    quick_check(&validation)?;
    foreign_key_check(&validation)?;
    validate_session_sequences(&validation)?;
    validation
        .commit()
        .map_err(|error| database_error("commit journal validation", error))
}

fn initialize_schema(connection: &mut Connection) -> Result<(), JournalError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| database_error("begin schema initialization", error))?;
    let version = schema_version(&transaction)?;
    validate_schema_version_range(version)?;
    if version == 0 {
        reject_unversioned_tables(&transaction)?;
        for (_, statement) in TABLES {
            transaction
                .execute_batch(statement)
                .map_err(|error| database_error("create journal schema", error))?;
        }
        transaction
            .pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|error| database_error("set journal schema version", error))?;
    }
    validate_schema(&transaction)?;
    transaction
        .commit()
        .map_err(|error| database_error("commit schema initialization", error))
}

fn configure_length_limit(connection: &Connection) -> Result<(), JournalError> {
    let limit = i32::try_from(crate::SQLITE_LENGTH_LIMIT_BYTES).map_err(|_| {
        JournalError::StorageConfiguration {
            setting: "SQLITE_LIMIT_LENGTH",
            expected: "a signed 32-bit byte count",
            actual: crate::SQLITE_LENGTH_LIMIT_BYTES.to_string(),
        }
    })?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_LENGTH, limit)
        .map_err(|error| database_error("set SQLite length limit", error))?;
    let effective = connection
        .limit(Limit::SQLITE_LIMIT_LENGTH)
        .map_err(|error| database_error("verify SQLite length limit", error))?;
    if effective == limit {
        Ok(())
    } else {
        Err(JournalError::StorageConfiguration {
            setting: "SQLITE_LIMIT_LENGTH",
            expected: "the configured checkpoint row limit",
            actual: effective.to_string(),
        })
    }
}

fn configure_busy_timeout(connection: &Connection) -> Result<(), JournalError> {
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(|error| database_error("set SQLite busy timeout", error))
}

fn configure(connection: &Connection, kind: StorageKind) -> Result<(), JournalError> {
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|error| database_error("enable SQLite foreign keys", error))?;

    let journal_mode = query_pragma_text(connection, "PRAGMA journal_mode = WAL", "journal_mode")?;
    let expected_journal_mode = match kind {
        StorageKind::File => "wal",
        StorageKind::Memory => "memory",
    };
    if !journal_mode.eq_ignore_ascii_case(expected_journal_mode) {
        return Err(JournalError::StorageConfiguration {
            setting: "journal_mode",
            expected: expected_journal_mode,
            actual: journal_mode,
        });
    }

    connection
        .execute_batch("PRAGMA synchronous = FULL;")
        .map_err(|error| database_error("set SQLite synchronous mode", error))?;

    verify_pragma_integer(connection, "foreign_keys", 1, "enabled")?;
    verify_pragma_integer(connection, "synchronous", 2, "FULL (2)")?;
    let timeout_millis = i64::try_from(BUSY_TIMEOUT.as_millis()).map_err(|_| {
        JournalError::StorageConfiguration {
            setting: "busy_timeout",
            expected: "a signed 64-bit millisecond value",
            actual: BUSY_TIMEOUT.as_millis().to_string(),
        }
    })?;
    verify_pragma_integer(
        connection,
        "busy_timeout",
        timeout_millis,
        "5000 milliseconds",
    )
}

fn schema_version(connection: &Connection) -> Result<i64, JournalError> {
    connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| database_error("read journal schema version", error))
}

fn validate_schema_version_range(version: i64) -> Result<(), JournalError> {
    if version > i64::from(SCHEMA_VERSION) {
        let found = u32::try_from(version)
            .map_err(|_| JournalError::InvalidSchemaVersion { found: version })?;
        Err(JournalError::UnsupportedSchemaVersion {
            found,
            supported: SCHEMA_VERSION,
        })
    } else if version < 0 {
        Err(JournalError::InvalidSchemaVersion { found: version })
    } else {
        Ok(())
    }
}

fn require_current_schema_version(connection: &Connection) -> Result<(), JournalError> {
    let version = schema_version(connection)?;
    validate_schema_version_range(version)?;
    if version == i64::from(SCHEMA_VERSION) {
        Ok(())
    } else {
        Err(JournalError::InvalidSchemaVersion { found: version })
    }
}

fn reject_unversioned_tables(connection: &Connection) -> Result<(), JournalError> {
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| database_error("inspect unversioned database", error))?;
    if count == 0 {
        Ok(())
    } else {
        Err(JournalError::UnversionedSchema)
    }
}

pub(crate) fn validate_schema(connection: &Connection) -> Result<(), JournalError> {
    require_current_schema_version(connection)?;
    let unexpected: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE NOT (
                 type = 'table'
                 AND name IN ('sessions', 'events', 'checkpoints', 'documents', 'metadata')
             )
             AND NOT (
                 type = 'index'
                 AND name = 'sqlite_autoindex_sessions_1'
                 AND tbl_name = 'sessions'
                 AND sql IS NULL
             )",
            [],
            |row| row.get(0),
        )
        .map_err(|error| database_error("check unexpected schema objects", error))?;
    if unexpected != 0 {
        return Err(JournalError::CorruptStorage(Corruption::Schema {
            detail: "journal contains an unexpected table, index, view, or trigger".to_owned(),
        }));
    }

    for (table, expected_sql) in TABLES {
        validate_table_definition(connection, table, expected_sql)?;
    }
    Ok(())
}

fn validate_table_definition(
    connection: &Connection,
    table: &'static str,
    expected_sql: &'static str,
) -> Result<(), JournalError> {
    let metadata: Option<(i64, Option<i64>)> = connection
        .query_row(
            "SELECT type = 'table', length(CAST(sql AS BLOB))
             FROM sqlite_schema WHERE name = ?1",
            [table],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| database_error("read table definition metadata", error))?;
    let Some((is_table, sql_length)) = metadata else {
        return Err(JournalError::CorruptStorage(Corruption::Schema {
            detail: format!("required table {table:?} is missing"),
        }));
    };
    let expected_length = i64::try_from(expected_sql.len()).map_err(|_| {
        JournalError::CorruptStorage(Corruption::Schema {
            detail: format!("expected definition for {table:?} is too large"),
        })
    })?;
    if is_table != 1 || sql_length != Some(expected_length) {
        return Err(JournalError::CorruptStorage(Corruption::Schema {
            detail: format!("table {table:?} does not match schema version 1"),
        }));
    }

    let mut statement = connection
        .prepare("SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1")
        .map_err(|error| database_error("prepare table definition validation", error))?;
    let mut rows = statement
        .query([table])
        .map_err(|error| database_error("query table definition", error))?;
    let row = rows
        .next()
        .map_err(|error| database_error("read table definition", error))?
        .ok_or_else(|| {
            JournalError::CorruptStorage(Corruption::Schema {
                detail: format!("required table {table:?} disappeared"),
            })
        })?;
    let value = row
        .get_ref(0)
        .map_err(|error| database_error("read table definition SQL", error))?;
    let ValueRef::Text(actual_sql) = value else {
        return Err(JournalError::CorruptStorage(Corruption::Schema {
            detail: format!("table {table:?} has no SQL definition"),
        }));
    };
    if actual_sql == expected_sql.as_bytes() {
        Ok(())
    } else {
        Err(JournalError::CorruptStorage(Corruption::Schema {
            detail: format!("table {table:?} does not match schema version 1"),
        }))
    }
}

fn quick_check(connection: &Connection) -> Result<(), JournalError> {
    let mut statement = connection
        .prepare("PRAGMA quick_check(1)")
        .map_err(|error| database_error("prepare SQLite quick_check", error))?;
    let mut rows = statement
        .query([])
        .map_err(|error| database_error("run SQLite quick_check", error))?;
    let row = rows
        .next()
        .map_err(|error| database_error("read SQLite quick_check", error))?
        .ok_or_else(|| {
            JournalError::CorruptStorage(Corruption::IntegrityCheck {
                detail: "quick_check returned no result".to_owned(),
            })
        })?;
    let value = row
        .get_ref(0)
        .map_err(|error| database_error("read SQLite quick_check result", error))?;
    let ValueRef::Text(bytes) = value else {
        return Err(JournalError::CorruptStorage(Corruption::IntegrityCheck {
            detail: "quick_check returned a non-text result".to_owned(),
        }));
    };
    const MAX_MESSAGE_BYTES: usize = 16 * 1024;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(JournalError::CorruptStorage(Corruption::IntegrityCheck {
            detail: "quick_check result exceeded 16 KiB".to_owned(),
        }));
    }
    let result = std::str::from_utf8(bytes).map_err(|_| {
        JournalError::CorruptStorage(Corruption::IntegrityCheck {
            detail: "quick_check returned invalid UTF-8".to_owned(),
        })
    })?;
    if result == "ok" {
        Ok(())
    } else {
        Err(JournalError::CorruptStorage(Corruption::IntegrityCheck {
            detail: result.to_owned(),
        }))
    }
}

fn foreign_key_check(connection: &Connection) -> Result<(), JournalError> {
    let violation: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM pragma_foreign_key_check LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| database_error("run SQLite foreign_key_check", error))?;
    if violation.is_some() {
        Err(JournalError::CorruptStorage(
            Corruption::ForeignKeyViolation,
        ))
    } else {
        Ok(())
    }
}

fn validate_session_rows(connection: &Connection) -> Result<(), JournalError> {
    let mut statement = connection
        .prepare(
            "SELECT typeof(session_id) = 'text', length(CAST(session_id AS BLOB)), \
             substr(CAST(session_id AS BLOB), 1, 129), next_sequence, ended \
             FROM sessions",
        )
        .map_err(|error| database_error("prepare session row validation", error))?;
    let mut rows = statement
        .query([])
        .map_err(|error| database_error("validate session rows", error))?;
    while let Some(row) = rows
        .next()
        .map_err(|error| database_error("read session row", error))?
    {
        decode_session_row(row)?;
    }
    Ok(())
}

fn validate_session_sequences(connection: &Connection) -> Result<(), JournalError> {
    let mut statement = connection
        .prepare(
            "SELECT typeof(s.session_id) = 'text', \
                    length(CAST(s.session_id AS BLOB)), \
                    substr(CAST(s.session_id AS BLOB), 1, 129) \
             FROM sessions s LEFT JOIN events e ON e.session_id = s.session_id \
             GROUP BY s.session_id, s.next_sequence \
             HAVING s.next_sequence != COUNT(e.sequence) + 1 \
                OR (COUNT(e.sequence) > 0 \
                    AND (MIN(e.sequence) != 1 OR MAX(e.sequence) != COUNT(e.sequence))) \
             LIMIT 1",
        )
        .map_err(|error| database_error("prepare session sequence check", error))?;
    let mut rows = statement
        .query([])
        .map_err(|error| database_error("run session sequence check", error))?;
    let Some(row) = rows
        .next()
        .map_err(|error| database_error("read session sequence check", error))?
    else {
        return Ok(());
    };
    let session_id = decode_session_id(row, 0, 1, 2)?;
    Err(JournalError::CorruptStorage(Corruption::Session {
        session_id: Some(session_id.to_string()),
        detail: "next_sequence does not describe one contiguous event prefix".to_owned(),
    }))
}

fn query_pragma_text(
    connection: &Connection,
    sql: &'static str,
    setting: &'static str,
) -> Result<String, JournalError> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| database_error("prepare SQLite journal mode", error))?;
    let mut rows = statement
        .query([])
        .map_err(|error| database_error("configure SQLite journal mode", error))?;
    let row = rows
        .next()
        .map_err(|error| database_error("read SQLite journal mode", error))?
        .ok_or_else(|| JournalError::StorageConfiguration {
            setting,
            expected: "a SQLite mode name",
            actual: "no value".to_owned(),
        })?;
    let value = row
        .get_ref(0)
        .map_err(|error| database_error("read SQLite journal mode value", error))?;
    let ValueRef::Text(bytes) = value else {
        Err(JournalError::StorageConfiguration {
            setting,
            expected: "a text SQLite mode name",
            actual: "non-text value".to_owned(),
        })?
    };
    if bytes.len() > 64 {
        return Err(JournalError::StorageConfiguration {
            setting,
            expected: "a bounded SQLite mode name",
            actual: "value longer than 64 bytes".to_owned(),
        });
    }
    let value = std::str::from_utf8(bytes).map_err(|_| JournalError::StorageConfiguration {
        setting,
        expected: "a UTF-8 SQLite mode name",
        actual: "invalid UTF-8".to_owned(),
    })?;
    Ok(value.to_owned())
}

fn verify_pragma_integer(
    connection: &Connection,
    setting: &'static str,
    expected_value: i64,
    expected_description: &'static str,
) -> Result<(), JournalError> {
    let value: i64 = connection
        .pragma_query_value(None, setting, |row| row.get(0))
        .map_err(|error| database_error("verify SQLite setting", error))?;
    if value == expected_value {
        Ok(())
    } else {
        Err(JournalError::StorageConfiguration {
            setting,
            expected: expected_description,
            actual: value.to_string(),
        })
    }
}
