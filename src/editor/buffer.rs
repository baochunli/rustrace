//! Compatibility module for the Phase 0 spike UI.
//!
//! The storage and mutation implementation lives exclusively in the shared
//! production editor crate.

pub use rustrace_editor::{
    CursorPosition, EditOrigin, EditorBuffer, EditorEffectError, EditorEffects, EditorTransaction,
    Movement, NoopEditorEffects, ReplayError, SelectionState, TransactionError,
    replay_transactions,
};
