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
pub use widget::{
    DiagnosticLineMarker, DiagnosticMarkerKind, EditorWidget, LiveDiagnosticSpan,
    line_number_gutter_width, split_line_number_gutter,
};

pub(crate) use highlight::SyntaxState;
