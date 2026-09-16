use std::error::Error;
use std::fmt;
use std::io::{self, Write};

use rustrace_model::{
    DecodeError, DocumentId, Hash, MAX_FILE_EDITED_TRANSACTION_BYTES, TextEdit, preflight_json,
};
use serde::Deserialize;

use crate::buffer::validate_transaction_structure;
use crate::{EditOrigin, EditorTransaction, SelectionState, TransactionError};

/// Largest transaction that fits every valid `FileEdited` event envelope.
pub const MAX_TRANSACTION_JSON_BYTES: usize = MAX_FILE_EDITED_TRANSACTION_BYTES;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransactionEncodeError {
    PayloadTooLarge { maximum: usize },
    Serialization { message: String },
    Validation(TransactionError),
}

impl fmt::Display for TransactionEncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge { maximum } => write!(
                formatter,
                "encoded editor transaction exceeds the {maximum}-byte maximum"
            ),
            Self::Serialization { message } => {
                write!(formatter, "could not encode editor transaction: {message}")
            }
            Self::Validation(error) => error.fmt(formatter),
        }
    }
}

impl Error for TransactionEncodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Validation(error) => Some(error),
            Self::PayloadTooLarge { .. } | Self::Serialization { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransactionDecodeError {
    PayloadTooLarge {
        actual: usize,
        maximum: usize,
    },
    JsonPreflight(DecodeError),
    Typed {
        message: String,
        line: usize,
        column: usize,
    },
    Validation(TransactionError),
}

impl fmt::Display for TransactionDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge { actual, maximum } => write!(
                formatter,
                "encoded editor transaction is {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::JsonPreflight(error) => error.fmt(formatter),
            Self::Typed {
                message,
                line,
                column,
            } => write!(
                formatter,
                "invalid editor transaction JSON at line {line}, column {column}: {message}"
            ),
            Self::Validation(error) => error.fmt(formatter),
        }
    }
}

impl Error for TransactionDecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::JsonPreflight(error) => Some(error),
            Self::Validation(error) => Some(error),
            Self::PayloadTooLarge { .. } | Self::Typed { .. } => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEditorTransaction {
    document_id: DocumentId,
    version_before: u64,
    version_after: u64,
    origin: EditOrigin,
    edits: Vec<TextEdit>,
    selection_before: SelectionState,
    selection_after: SelectionState,
    hash_before: Hash,
    hash_after: Hash,
}

impl From<WireEditorTransaction> for EditorTransaction {
    fn from(wire: WireEditorTransaction) -> Self {
        Self {
            document_id: wire.document_id,
            version_before: wire.version_before,
            version_after: wire.version_after,
            origin: wire.origin,
            edits: wire.edits,
            selection_before: wire.selection_before,
            selection_after: wire.selection_after,
            hash_before: wire.hash_before,
            hash_after: wire.hash_after,
        }
    }
}

struct CappedWriter<W> {
    inner: W,
    written: usize,
    exceeded: bool,
}

impl<W> CappedWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            written: 0,
            exceeded: false,
        }
    }

    fn into_inner(self) -> W {
        self.inner
    }
}

impl<W> Write for CappedWriter<W>
where
    W: Write,
{
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(next) = self.written.checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::other("transaction encoding limit exceeded"));
        };
        if next > MAX_TRANSACTION_JSON_BYTES {
            self.exceeded = true;
            return Err(io::Error::other("transaction encoding limit exceeded"));
        }
        let count = self.inner.write(bytes)?;
        self.written += count;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn map_encode_error(error: serde_json::Error, exceeded: bool) -> TransactionEncodeError {
    if exceeded {
        TransactionEncodeError::PayloadTooLarge {
            maximum: MAX_TRANSACTION_JSON_BYTES,
        }
    } else {
        TransactionEncodeError::Serialization {
            message: error.to_string(),
        }
    }
}

fn encoded_transaction_len(
    transaction: &EditorTransaction,
) -> Result<usize, TransactionEncodeError> {
    validate_transaction_structure(transaction).map_err(TransactionEncodeError::Validation)?;
    let mut writer = CappedWriter::new(io::sink());
    if let Err(error) = serde_json::to_writer(&mut writer, transaction) {
        return Err(map_encode_error(error, writer.exceeded));
    }
    Ok(writer.written)
}

pub(crate) fn validate_transaction_encodable(
    transaction: &EditorTransaction,
) -> Result<(), TransactionError> {
    encode_transaction(transaction)
        .map(|_| ())
        .map_err(|error| match error {
            TransactionEncodeError::PayloadTooLarge { maximum } => {
                TransactionError::TransactionTooLarge { maximum }
            }
            TransactionEncodeError::Serialization { message } => {
                TransactionError::TransactionSerialization { message }
            }
            TransactionEncodeError::Validation(error) => error,
        })
}

/// Encodes one transaction as compact canonical JSON with bounded allocations,
/// then applies the same lexical preflight used by the decoder.
pub fn encode_transaction(
    transaction: &EditorTransaction,
) -> Result<Vec<u8>, TransactionEncodeError> {
    let encoded_len = encoded_transaction_len(transaction)?;
    let mut writer = CappedWriter::new(Vec::with_capacity(encoded_len));
    if let Err(error) = serde_json::to_writer(&mut writer, transaction) {
        return Err(map_encode_error(error, writer.exceeded));
    }
    debug_assert_eq!(writer.written, encoded_len);
    let encoded = writer.into_inner();
    preflight_json(&encoded).map_err(|error| {
        TransactionEncodeError::Validation(TransactionError::CodecPreflight(error))
    })?;
    Ok(encoded)
}

/// Decodes a transaction only after raw-size and lexical JSON preflight.
///
/// Snapshot-dependent bounds and hashes are validated later by
/// [`crate::EditorBuffer::apply_transaction`].
pub fn decode_transaction(encoded: &[u8]) -> Result<EditorTransaction, TransactionDecodeError> {
    if encoded.len() > MAX_TRANSACTION_JSON_BYTES {
        return Err(TransactionDecodeError::PayloadTooLarge {
            actual: encoded.len(),
            maximum: MAX_TRANSACTION_JSON_BYTES,
        });
    }
    preflight_json(encoded).map_err(TransactionDecodeError::JsonPreflight)?;
    let wire: WireEditorTransaction =
        serde_json::from_slice(encoded).map_err(|error| TransactionDecodeError::Typed {
            message: error.to_string(),
            line: error.line(),
            column: error.column(),
        })?;
    let transaction = EditorTransaction::from(wire);
    validate_transaction_structure(&transaction).map_err(TransactionDecodeError::Validation)?;
    Ok(transaction)
}
