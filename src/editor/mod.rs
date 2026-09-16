mod buffer;
mod highlight;
mod viewport;
mod widget;

pub use buffer::{
    CursorPosition, EditOrigin, EditorBuffer, EditorEffectError, EditorEffects, EditorTransaction,
    Movement, NoopEditorEffects, ReplayError, SelectionState, TransactionError,
    replay_transactions,
};
pub use highlight::{
    HighlightError, HighlightKind, HighlightSpan, RustHighlighter, SyntaxEffects, highlight_path,
};
pub use viewport::Viewport;
pub use widget::{DiagnosticLineMarker, DiagnosticMarkerKind, EditorWidget, LiveDiagnosticSpan};

pub(crate) use highlight::SyntaxState;
