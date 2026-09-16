//! Shared editor transactions and the single Rustrace document mutation core.
//!
//! All transaction offsets are UTF-8 byte offsets in the document snapshot
//! identified by `version_before`. Edits are sorted by `(start_byte,
//! end_byte)`, may touch but must not overlap, and are applied from the end of
//! the snapshot toward the beginning. This also gives simultaneous zero-width
//! insertions at one offset deterministic vector order.
//!
//! A transaction is fully preflighted, durably accepted by its fallible
//! provenance hook, committed once, and then passed to the remaining
//! [`EditorEffects`] hooks. The hooks are deliberately adapters only: journal,
//! parser, LSP, and durable replay machinery belong outside this crate.

#![forbid(unsafe_code)]

mod buffer;
mod codec;
pub mod display;
mod effects;
pub mod position;
mod replay;
mod transaction;

pub use buffer::{CursorPosition, EditorBuffer, Movement, TransactionError};
pub use codec::{
    MAX_TRANSACTION_JSON_BYTES, TransactionDecodeError, TransactionEncodeError, decode_transaction,
    encode_transaction,
};
pub use effects::{EditorEffectError, EditorEffects, NoopEditorEffects};
pub use replay::{ReplayDocument, ReplayError, replay_transactions};
pub use rustrace_model::{EditOrigin, EditorTransaction, SelectionState};
pub use transaction::{DOCUMENT_HASH_DOMAIN, document_hash};
