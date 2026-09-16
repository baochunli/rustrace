use std::error::Error;
use std::fmt;

use rustrace_model::{DocumentId, Hash, SelectionState, document_hash};

use crate::buffer::{apply_recorded_transaction, validate_recorded_selection};
use crate::codec::validate_transaction_encodable;
use crate::{EditorBuffer, EditorEffects, EditorTransaction, TransactionError};

/// Headless state for one open document restored from a checkpoint.
///
/// This type never runs editor effects and does not retain undo/redo history.
/// Recorded transactions still receive the production editor's complete
/// version, selection, range, size, and before/after hash validation.
/// Workspace-level file and aggregate-size limits remain the caller's
/// responsibility.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayDocument {
    document_id: DocumentId,
    text: String,
    version: u64,
    selection: SelectionState,
    hash: Hash,
}

impl ReplayDocument {
    pub fn new(
        document_id: DocumentId,
        text: String,
        version: u64,
        selection: SelectionState,
    ) -> Result<Self, TransactionError> {
        validate_recorded_selection(&text, selection)?;
        let hash = document_hash(&text);
        Ok(Self {
            document_id,
            text,
            version,
            selection,
            hash,
        })
    }

    pub fn document_id(&self) -> &DocumentId {
        &self.document_id
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn version(&self) -> u64 {
        self.version
    }

    pub const fn selection(&self) -> SelectionState {
        self.selection
    }

    pub const fn hash(&self) -> Hash {
        self.hash
    }

    pub fn set_selection(&mut self, selection: SelectionState) -> Result<bool, TransactionError> {
        validate_recorded_selection(&self.text, selection)?;
        if selection == self.selection {
            return Ok(false);
        }
        self.selection = selection;
        Ok(true)
    }

    pub fn apply_transaction(
        &mut self,
        transaction: &EditorTransaction,
    ) -> Result<bool, TransactionError> {
        let Some(after_text) = apply_recorded_transaction(
            &self.document_id,
            &self.text,
            self.version,
            self.selection,
            transaction,
        )?
        else {
            return Ok(false);
        };
        self.text = after_text;
        self.version = transaction.version_after;
        self.selection = transaction.selection_after;
        self.hash = transaction.hash_after;
        Ok(true)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplayError {
    Transaction {
        index: usize,
        source: TransactionError,
    },
    NoOpTransaction {
        index: usize,
    },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transaction { index, source } => {
                write!(formatter, "transaction {index} failed replay: {source}")
            }
            Self::NoOpTransaction { index } => {
                write!(formatter, "transaction {index} is a non-committing no-op")
            }
        }
    }
}

impl Error for ReplayError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transaction { source, .. } => Some(source),
            Self::NoOpTransaction { .. } => None,
        }
    }
}

/// Reconstructs a document from transactions emitted by the editor core.
///
/// Selection-only UI events are not transactions. For each entry this helper
/// validates and stages its embedded pre-edit selection, then calls the same
/// strict [`EditorBuffer::apply_transaction`] gateway used by the live editor.
pub fn replay_transactions<E>(
    document_id: DocumentId,
    initial: &str,
    transactions: &[EditorTransaction],
    effects: E,
) -> Result<EditorBuffer<E>, ReplayError>
where
    E: EditorEffects,
{
    let mut editor = EditorBuffer::new(document_id, initial, effects);
    for (index, transaction) in transactions.iter().enumerate() {
        validate_transaction_encodable(transaction)
            .map_err(|source| ReplayError::Transaction { index, source })?;
        editor
            .set_selection(transaction.selection_before)
            .map_err(|source| ReplayError::Transaction { index, source })?;
        let committed = editor
            .apply_transaction(transaction.clone())
            .map_err(|source| ReplayError::Transaction { index, source })?;
        if !committed {
            return Err(ReplayError::NoOpTransaction { index });
        }
    }
    Ok(editor)
}
