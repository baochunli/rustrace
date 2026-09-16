use std::error::Error;
use std::fmt;

use crate::{EditOrigin, EditorTransaction};

/// Immutable downstream integration points for one transaction.
///
/// Provenance persistence runs after complete preflight but before mutation;
/// failure leaves editor and downstream state unchanged. After a successful
/// commit, the editor invokes the remaining hooks exactly once, in declaration
/// order, with the same reference. Defaults make it cheap for a consumer to
/// implement only the adapter that exists in the current phase.
pub trait EditorEffects {
    /// Optional live input policy, checked before payload validation or no-op
    /// elimination by mutating paste/programmatic entry points. Historical
    /// replay and generic editor fixtures do not acquire production authority.
    fn check_input_origin(&mut self, _origin: EditOrigin) -> Result<(), EditorEffectError> {
        Ok(())
    }

    fn record_provenance(
        &mut self,
        _transaction: &EditorTransaction,
    ) -> Result<(), EditorEffectError> {
        Ok(())
    }

    fn update_tree_sitter(&mut self, _transaction: &EditorTransaction) {}

    fn send_lsp_did_change(&mut self, _transaction: &EditorTransaction) {}

    fn record_replay(&mut self, _transaction: &EditorTransaction) {}
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NoopEditorEffects;

impl EditorEffects for NoopEditorEffects {}

impl<F> EditorEffects for F
where
    F: FnMut(&EditorTransaction),
{
    fn record_provenance(
        &mut self,
        transaction: &EditorTransaction,
    ) -> Result<(), EditorEffectError> {
        self(transaction);
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditorEffectError {
    message: String,
}

impl EditorEffectError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for EditorEffectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for EditorEffectError {}
