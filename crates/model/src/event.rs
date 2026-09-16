//! Version 1 event vocabulary and its bounded JSON wire format.
//!
//! encode_envelope is the canonical encoder. It emits compact UTF-8 JSON with
//! fields in EventEnvelope declaration order. Events use adjacent tagging,
//! identifiers are strings, hashes are 64 lowercase hexadecimal characters,
//! and UTC wall-clock values use RFC 3339. Replay ordering and timing use
//! sequence and monotonic_millis; wall_clock_utc is contextual only.

use std::{
    collections::HashSet,
    error::Error,
    fmt,
    io::{self, Write},
    str::FromStr,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ControlledCommandFinished, ControlledCommandOutput, ControlledCommandStarted, TestCaseCompared,
    ids::{
        CommandId, DocumentId, Hash, HashError, IdentifierError, MAX_IDENTIFIER_BYTES, SessionId,
    },
    path::{MAX_WORKSPACE_PATH_BYTES, WorkspaceDirectory, WorkspacePath},
};

pub const FORMAT_VERSION_V1: u32 = 1;
pub const MAX_ENVELOPE_BYTES: usize = 1024 * 1024;
pub const MAX_STRING_BYTES: usize = 4 * 1024;
pub const MAX_PATH_BYTES: usize = MAX_WORKSPACE_PATH_BYTES;
pub const MAX_INSERTED_TEXT_BYTES: usize = 256 * 1024;
/// One internal selection, bounded by both the clipboard and normal edit limits.
pub const MAX_INTERNAL_CLIPBOARD_BYTES: usize = MAX_INSERTED_TEXT_BYTES;
pub const MAX_PASTE_REJECTION_BYTES: usize = 1024;
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_VECTOR_ITEMS: usize = 1024;
pub const MAX_JSON_NESTING: usize = 16;
pub const MAX_JSON_KEY_BYTES: usize = MAX_STRING_BYTES;
pub const MAX_JSON_STRING_BYTES: usize = MAX_INSERTED_TEXT_BYTES;
pub const MAX_JSON_RAW_STRING_BYTES: usize = MAX_ENVELOPE_BYTES / 2;
pub const MAX_JSON_VALUES: usize = 8 * MAX_VECTOR_ITEMS;
pub const MAX_MONOTONIC_MILLIS: u64 = i64::MAX as u64;

// Canonical JSON outside a FileEdited payload. Empty quoted values mark the
// insertion points for bounded string contents; the payload is inserted
// between the final colon and the two closing braces.
const FILE_EDITED_ENVELOPE_JSON_SHELL: &str = concat!(
    r#"{"format_version":1,"session_id":"","sequence":,"monotonic_millis":,"#,
    r#""wall_clock_utc":"","previous_event_hash":"","event_hash":"","#,
    r#""event":{"type":"file_edited","payload":}}"#,
);

// chrono 0.4's UTC serializer uses RFC 3339 with `Z`, up to a signed six-digit
// year and nine fractional-second digits: +262142-12-31T23:59:59.999999999Z.
const MAX_RFC3339_UTC_BYTES: usize = 33;

const fn decimal_digits(mut value: u64) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

/// Maximum canonical JSON overhead around a `FileEdited` transaction.
///
/// This includes worst-case valid envelope metadata and event framing. Model
/// identifiers cannot contain JSON-escaped characters, hashes have a fixed
/// hexadecimal width, and `Some(DateTime<Utc>)` is longer than `null`.
pub const FILE_EDITED_ENVELOPE_MAX_OVERHEAD_BYTES: usize = FILE_EDITED_ENVELOPE_JSON_SHELL.len()
    + MAX_IDENTIFIER_BYTES
    + decimal_digits(u64::MAX)
    + decimal_digits(MAX_MONOTONIC_MILLIS)
    + MAX_RFC3339_UTC_BYTES
    + 2 * Hash::ENCODED_LENGTH;

/// Largest canonical `EditorTransaction` JSON that remains persistable when
/// wrapped in any valid version 1 `FileEdited` event envelope.
pub const MAX_FILE_EDITED_TRANSACTION_BYTES: usize =
    MAX_ENVELOPE_BYTES - FILE_EDITED_ENVELOPE_MAX_OVERHEAD_BYTES;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EventEnvelope {
    pub format_version: u32,
    pub session_id: SessionId,
    pub sequence: u64,
    pub monotonic_millis: u64,
    pub wall_clock_utc: Option<DateTime<Utc>>,
    pub previous_event_hash: Hash,
    pub event_hash: Hash,
    pub event: Event,
}

/// Canonical version 1 event identity and content used by the hash chain.
///
/// Field order is part of the hash contract. The two chain fields are
/// deliberately absent so hashing cannot be circular or depend on caller-
/// supplied chain values.
#[derive(Serialize)]
struct EventHashMaterial<'a> {
    format_version: u32,
    session_id: &'a SessionId,
    sequence: u64,
    monotonic_millis: u64,
    wall_clock_utc: &'a Option<DateTime<Utc>>,
    event: &'a Event,
}

impl<'a> From<&'a EventEnvelope> for EventHashMaterial<'a> {
    fn from(envelope: &'a EventEnvelope) -> Self {
        Self {
            format_version: envelope.format_version,
            session_id: &envelope.session_id,
            sequence: envelope.sequence,
            monotonic_millis: envelope.monotonic_millis,
            wall_clock_utc: &envelope.wall_clock_utc,
            event: &envelope.event,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEventEnvelope {
    format_version: u32,
    session_id: SessionId,
    sequence: u64,
    monotonic_millis: u64,
    wall_clock_utc: Option<DateTime<Utc>>,
    previous_event_hash: Hash,
    event_hash: Hash,
    event: WireEvent,
}

impl From<WireEventEnvelope> for EventEnvelope {
    fn from(wire: WireEventEnvelope) -> Self {
        Self {
            format_version: wire.format_version,
            session_id: wire.session_id,
            sequence: wire.sequence,
            monotonic_millis: wire.monotonic_millis,
            wall_clock_utc: wire.wall_clock_utc,
            previous_event_hash: wire.previous_event_hash,
            event_hash: wire.event_hash,
            event: wire.event.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Event {
    ControlledCommandStarted(ControlledCommandStarted),
    ControlledCommandOutput(ControlledCommandOutput),
    ControlledCommandFinished(ControlledCommandFinished),
    TestCaseCompared(TestCaseCompared),
    SessionStarted(SessionStarted),
    SessionResumed(SessionResumed),
    SessionEnded(SessionEnded),
    FileCreated(FileCreated),
    FileDeleted(FileDeleted),
    FileRenamed(FileRenamed),
    FileFocused(FileFocused),
    FileEdited(FileEdited),
    ClipboardCopied(ClipboardSource),
    InternalPaste(InternalPaste),
    PasteRejected(PasteRejected),
    SelectionChanged(SelectionChanged),
    ViewportChanged(ViewportChanged),
    CargoCommandStarted(CommandStarted),
    CargoDiagnostic(Diagnostic),
    CargoOutput(CommandOutput),
    CargoCommandFinished(CommandFinished),
    LspCompletionRequested(CompletionRequested),
    LspCompletionAccepted(CompletionAccepted),
    LspCodeActionApplied(CodeActionApplied),
    WorkspaceCheckpoint(Checkpoint),
    ExternalFileChange(ExternalFileChange),
    ExternalObservation(ExternalObservation),
    RecoveryRecorded(RecoveryRecorded),
    SubmissionFinalized(SubmissionFinalized),
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum WireEvent {
    ControlledCommandStarted(ControlledCommandStarted),
    ControlledCommandOutput(ControlledCommandOutput),
    ControlledCommandFinished(ControlledCommandFinished),
    TestCaseCompared(TestCaseCompared),
    SessionStarted(SessionStarted),
    SessionResumed(SessionResumed),
    SessionEnded(SessionEnded),
    FileCreated(FileCreated),
    FileDeleted(FileDeleted),
    FileRenamed(FileRenamed),
    FileFocused(FileFocused),
    FileEdited(WireEditorTransaction),
    ClipboardCopied(ClipboardSource),
    InternalPaste(WireInternalPaste),
    PasteRejected(PasteRejected),
    SelectionChanged(SelectionChanged),
    ViewportChanged(ViewportChanged),
    CargoCommandStarted(CommandStarted),
    CargoDiagnostic(Diagnostic),
    CargoOutput(CommandOutput),
    CargoCommandFinished(CommandFinished),
    LspCompletionRequested(CompletionRequested),
    LspCompletionAccepted(CompletionAccepted),
    LspCodeActionApplied(CodeActionApplied),
    WorkspaceCheckpoint(Checkpoint),
    ExternalFileChange(ExternalFileChange),
    ExternalObservation(ExternalObservation),
    RecoveryRecorded(RecoveryRecorded),
    SubmissionFinalized(SubmissionFinalized),
}

impl From<WireEvent> for Event {
    fn from(event: WireEvent) -> Self {
        match event {
            WireEvent::ControlledCommandStarted(payload) => Self::ControlledCommandStarted(payload),
            WireEvent::ControlledCommandOutput(payload) => Self::ControlledCommandOutput(payload),
            WireEvent::ControlledCommandFinished(payload) => {
                Self::ControlledCommandFinished(payload)
            }
            WireEvent::TestCaseCompared(payload) => Self::TestCaseCompared(payload),
            WireEvent::SessionStarted(payload) => Self::SessionStarted(payload),
            WireEvent::SessionResumed(payload) => Self::SessionResumed(payload),
            WireEvent::SessionEnded(payload) => Self::SessionEnded(payload),
            WireEvent::FileCreated(payload) => Self::FileCreated(payload),
            WireEvent::FileDeleted(payload) => Self::FileDeleted(payload),
            WireEvent::FileRenamed(payload) => Self::FileRenamed(payload),
            WireEvent::FileFocused(payload) => Self::FileFocused(payload),
            WireEvent::FileEdited(payload) => Self::FileEdited(payload.into()),
            WireEvent::ClipboardCopied(payload) => Self::ClipboardCopied(payload),
            WireEvent::InternalPaste(payload) => Self::InternalPaste(InternalPaste {
                source: payload.source,
                transaction: payload.transaction.into(),
            }),
            WireEvent::PasteRejected(payload) => Self::PasteRejected(payload),
            WireEvent::SelectionChanged(payload) => Self::SelectionChanged(payload),
            WireEvent::ViewportChanged(payload) => Self::ViewportChanged(payload),
            WireEvent::CargoCommandStarted(payload) => Self::CargoCommandStarted(payload),
            WireEvent::CargoDiagnostic(payload) => Self::CargoDiagnostic(payload),
            WireEvent::CargoOutput(payload) => Self::CargoOutput(payload),
            WireEvent::CargoCommandFinished(payload) => Self::CargoCommandFinished(payload),
            WireEvent::LspCompletionRequested(payload) => Self::LspCompletionRequested(payload),
            WireEvent::LspCompletionAccepted(payload) => Self::LspCompletionAccepted(payload),
            WireEvent::LspCodeActionApplied(payload) => Self::LspCodeActionApplied(payload),
            WireEvent::WorkspaceCheckpoint(payload) => Self::WorkspaceCheckpoint(payload),
            WireEvent::ExternalFileChange(payload) => Self::ExternalFileChange(payload),
            WireEvent::ExternalObservation(payload) => Self::ExternalObservation(payload),
            WireEvent::RecoveryRecorded(payload) => Self::RecoveryRecorded(payload),
            WireEvent::SubmissionFinalized(payload) => Self::SubmissionFinalized(payload),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStarted {
    pub client_version: String,
    pub starter_workspace_hash: Hash,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionResumed {
    pub last_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionEnded {
    pub final_workspace_hash: Hash,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCreated {
    pub document_id: DocumentId,
    pub path: WorkspacePath,
    pub contents: String,
    pub content_hash: Hash,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDeleted {
    pub document_id: DocumentId,
    pub path: WorkspacePath,
    pub previous_hash: Hash,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRenamed {
    pub document_id: DocumentId,
    pub old_path: WorkspacePath,
    pub new_path: WorkspacePath,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileFocused {
    pub document_id: DocumentId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditOrigin {
    Keyboard,
    Paste,
    Undo,
    Redo,
    Completion,
    AdditionalCompletionEdit,
    Formatter,
    DependencyTool,
    CodeAction,
    FileReload,
    ExternalChange,
    Unknown,
}

/// Anchor and active endpoints in UTF-8 bytes.
///
/// Equal endpoints represent a caret. Keeping both endpoints preserves the
/// direction of a non-empty selection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionState {
    pub anchor_byte: u64,
    pub active_byte: u64,
}

impl SelectionState {
    pub const fn new(anchor_byte: u64, active_byte: u64) -> Self {
        Self {
            anchor_byte,
            active_byte,
        }
    }

    pub const fn caret(offset: u64) -> Self {
        Self::new(offset, offset)
    }

    pub const fn is_caret(self) -> bool {
        self.anchor_byte == self.active_byte
    }
}

/// Complete immutable metadata for one document mutation.
///
/// Deserialization is intentionally available only through a bounded codec:
/// event persistence uses decode_envelope and the editor exposes its own
/// bounded transaction decoder.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EditorTransaction {
    pub document_id: DocumentId,
    pub version_before: u64,
    pub version_after: u64,
    pub origin: EditOrigin,
    pub edits: Vec<TextEdit>,
    pub selection_before: SelectionState,
    pub selection_after: SelectionState,
    pub hash_before: Hash,
    pub hash_after: Hash,
}

/// The persisted file-edit payload is the shared editor transaction itself.
pub type FileEdited = EditorTransaction;

/// A reference within one unchanged recorded session/segment, never a bare
/// package-wide sequence or host path. The event hash binds the complete prefix.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedEventRef {
    pub session_id: SessionId,
    pub sequence: u64,
    pub event_hash: Hash,
}

/// Metadata only: replay derives selected bytes from the exact preceding prefix.
/// For cut this event precedes the ordinary deletion transaction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipboardSource {
    pub prefix: RecordedEventRef,
    pub document_id: DocumentId,
    pub path: WorkspacePath,
    pub version: u64,
    pub content_hash: Hash,
    pub start_byte: u64,
    pub end_byte: u64,
}

/// One normal Paste transaction with a required link to ClipboardCopied.
/// A separate variant prevents missing linkage from downgrading new evidence
/// to an origin-unverified historical FileEdited event. It applies only once.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InternalPaste {
    pub source: RecordedEventRef,
    pub transaction: EditorTransaction,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireInternalPaste {
    source: RecordedEventRef,
    transaction: WireEditorTransaction,
}

/// Fixed metadata only. No rejected text, hash, size, preview or attribution.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasteRejected {
    pub reason: PasteRejectionReason,
    pub channel: PasteInputChannel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasteRejectionReason {
    ExternalInput,
    UnverifiableInput,
    MissingLiveSource,
    OutsideEditor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasteInputChannel {
    TerminalBracketed,
    InternalShortcut,
    ProductionCommand,
    Programmatic,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextEdit {
    pub start_byte: u64,
    pub end_byte: u64,
    pub inserted_text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionChanged {
    pub document_id: DocumentId,
    pub anchor_byte: u64,
    pub active_byte: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewportChanged {
    pub document_id: DocumentId,
    pub top_line: u64,
    pub horizontal_column: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandStarted {
    pub command_id: CommandId,
    pub program: String,
    pub arguments: Vec<String>,
    pub working_directory: WorkspaceDirectory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub command_id: CommandId,
    pub document_id: Option<DocumentId>,
    pub severity: DiagnosticSeverity,
    pub code: Option<String>,
    pub message: String,
    pub range: Option<TextRange>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextRange {
    pub start_line: u64,
    pub start_column: u64,
    pub end_line: u64,
    pub end_column: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandOutput {
    pub command_id: CommandId,
    pub stream: OutputStream,
    pub output: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandFinished {
    pub command_id: CommandId,
    pub exit_code: Option<i32>,
    pub success: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionRequested {
    pub document_id: DocumentId,
    pub document_version: u64,
    pub position_byte: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionAccepted {
    pub document_id: DocumentId,
    pub document_version: u64,
    pub label: String,
    pub primary_edit: TextEdit,
    pub additional_edits: Vec<TextEdit>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeActionApplied {
    pub title: String,
    pub kind: Option<String>,
    pub document_ids: Vec<DocumentId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub workspace_hash: Hash,
    pub documents: Vec<DocumentHash>,
}

pub use Checkpoint as WorkspaceCheckpoint;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentHash {
    pub document_id: DocumentId,
    pub hash: Hash,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalFileChange {
    pub path: WorkspacePath,
    pub previous_contents: Option<String>,
    pub new_contents: Option<String>,
    pub previous_hash: Option<Hash>,
    pub new_hash: Option<Hash>,
}

/// Links exact, bounded saved/observed evidence to the current logical view.
/// Evidence is retained under its content digest; this event does not reload a file.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalObservation {
    pub path: WorkspacePath,
    pub saved_hash: Option<Hash>,
    pub logical_hash: Option<Hash>,
    pub observed_hash: Option<Hash>,
    pub evidence_hash: Hash,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryDecision {
    RestoreLogical,
    AcceptExternal,
    AbandonPreserved,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRecorded {
    pub evidence_hash: Hash,
    pub decision: RecoveryDecision,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmissionFinalized {
    pub final_workspace_hash: Hash,
    pub event_count: u64,
    pub clean: bool,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValidationError {
    CommandEvidence {
        detail: &'static str,
    },
    InvalidClipboardEvidence,
    UnsupportedFormatVersion {
        found: u32,
        supported: u32,
    },
    SequenceMustStartAtOne,
    MonotonicMillisOutOfRange {
        actual: u64,
        maximum: u64,
    },
    LastSequenceMustStartAtOne,
    InvalidVersionTransition {
        before: u64,
        after: u64,
    },
    InvalidByteRange {
        start: u64,
        end: u64,
    },
    EditsNotCanonical {
        previous_index: usize,
        edit_index: usize,
    },
    EditsOverlap {
        previous_index: usize,
        edit_index: usize,
    },
    InvalidTextRange,
    StringTooLong {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    VectorTooLong {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommandEvidence { detail } => {
                write!(formatter, "invalid controlled command evidence: {detail}")
            }
            Self::InvalidClipboardEvidence => {
                formatter.write_str("invalid internal clipboard source or transaction")
            }
            Self::UnsupportedFormatVersion { found, supported } => {
                write!(
                    formatter,
                    "format version {found} is unsupported; supported version is {supported}"
                )
            }
            Self::SequenceMustStartAtOne => formatter.write_str("sequence must start at one"),
            Self::MonotonicMillisOutOfRange { actual, maximum } => write!(
                formatter,
                "monotonic_millis is {actual}; maximum is {maximum}"
            ),
            Self::LastSequenceMustStartAtOne => {
                formatter.write_str("resumed session last_sequence must be at least one")
            }
            Self::InvalidVersionTransition { before, after } => write!(
                formatter,
                "document version must increase: before={before}, after={after}"
            ),
            Self::InvalidByteRange { start, end } => {
                write!(
                    formatter,
                    "text edit byte range is reversed: {start}..{end}"
                )
            }
            Self::EditsNotCanonical {
                previous_index,
                edit_index,
            } => write!(
                formatter,
                "edits are not in canonical order at indices {previous_index} and {edit_index}"
            ),
            Self::EditsOverlap {
                previous_index,
                edit_index,
            } => write!(
                formatter,
                "edits overlap at indices {previous_index} and {edit_index}"
            ),
            Self::InvalidTextRange => formatter.write_str("diagnostic text range is reversed"),
            Self::StringTooLong {
                field,
                actual,
                maximum,
            } => write!(
                formatter,
                "{field} is {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::VectorTooLong {
                field,
                actual,
                maximum,
            } => write!(
                formatter,
                "{field} has {actual} items; maximum is {maximum} items"
            ),
        }
    }
}

impl Error for ValidationError {}

impl EventEnvelope {
    /// Sets the authoritative previous hash and computes this event's hash.
    ///
    /// The returned envelope is also checked for bounded canonical
    /// persistability, so callers cannot obtain a sealed envelope that the
    /// version 1 codec would reject.
    pub fn seal(mut self, previous_event_hash: Hash) -> Result<Self, EncodeError> {
        self.previous_event_hash = previous_event_hash;
        self.event_hash = compute_event_hash(previous_event_hash, &self)?;
        encode_envelope(&self)?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.format_version != FORMAT_VERSION_V1 {
            return Err(ValidationError::UnsupportedFormatVersion {
                found: self.format_version,
                supported: FORMAT_VERSION_V1,
            });
        }
        if self.sequence == 0 {
            return Err(ValidationError::SequenceMustStartAtOne);
        }
        if self.monotonic_millis > MAX_MONOTONIC_MILLIS {
            return Err(ValidationError::MonotonicMillisOutOfRange {
                actual: self.monotonic_millis,
                maximum: MAX_MONOTONIC_MILLIS,
            });
        }
        self.event.validate()
    }
}

impl Event {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::ControlledCommandStarted(event) => event.validate(),
            Self::ControlledCommandOutput(event) => event.validate(),
            Self::ControlledCommandFinished(event) => event.validate(),
            Self::TestCaseCompared(event) => event.validate(),
            Self::SessionStarted(event) => {
                validate_string("event.client_version", &event.client_version)
            }
            Self::SessionResumed(event) => {
                if event.last_sequence == 0 {
                    Err(ValidationError::LastSequenceMustStartAtOne)
                } else {
                    Ok(())
                }
            }
            Self::SessionEnded(_) | Self::FileFocused(_) | Self::SelectionChanged(_) => Ok(()),
            Self::FileCreated(event) => validate_text("event.contents", &event.contents),
            Self::FileDeleted(_) | Self::FileRenamed(_) => Ok(()),
            Self::FileEdited(event) => event.validate(),
            Self::ClipboardCopied(source) => {
                if source.prefix.sequence == 0
                    || source.start_byte >= source.end_byte
                    || source.end_byte - source.start_byte > MAX_INTERNAL_CLIPBOARD_BYTES as u64
                {
                    return Err(ValidationError::InvalidClipboardEvidence);
                }
                Ok(())
            }
            Self::InternalPaste(event) => {
                if event.source.sequence == 0
                    || event.transaction.origin != EditOrigin::Paste
                    || event.transaction.edits.len() != 1
                {
                    return Err(ValidationError::InvalidClipboardEvidence);
                }
                event.transaction.validate()
            }
            Self::PasteRejected(_) => Ok(()),
            Self::ViewportChanged(_) => Ok(()),
            Self::CargoCommandStarted(event) => {
                validate_string("event.program", &event.program)?;
                validate_vector("event.arguments", event.arguments.len())?;
                for argument in &event.arguments {
                    validate_string("event.arguments[]", argument)?;
                }
                Ok(())
            }
            Self::CargoDiagnostic(event) => {
                if let Some(code) = &event.code {
                    validate_string("event.code", code)?;
                }
                validate_string("event.message", &event.message)?;
                if let Some(range) = &event.range {
                    range.validate()?;
                }
                Ok(())
            }
            Self::CargoOutput(event) => {
                validate_limited_string("event.output", &event.output, MAX_OUTPUT_BYTES)
            }
            Self::CargoCommandFinished(_) | Self::LspCompletionRequested(_) => Ok(()),
            Self::LspCompletionAccepted(event) => {
                validate_string("event.label", &event.label)?;
                event.primary_edit.validate()?;
                validate_vector("event.additional_edits", event.additional_edits.len())?;
                for edit in &event.additional_edits {
                    edit.validate()?;
                }
                Ok(())
            }
            Self::LspCodeActionApplied(event) => {
                validate_string("event.title", &event.title)?;
                if let Some(kind) = &event.kind {
                    validate_string("event.kind", kind)?;
                }
                validate_vector("event.document_ids", event.document_ids.len())
            }
            Self::WorkspaceCheckpoint(event) => {
                validate_vector("event.documents", event.documents.len())
            }
            Self::ExternalFileChange(event) => {
                if let Some(contents) = &event.previous_contents {
                    validate_text("event.previous_contents", contents)?;
                }
                if let Some(contents) = &event.new_contents {
                    validate_text("event.new_contents", contents)?;
                }
                Ok(())
            }
            Self::ExternalObservation(_) | Self::RecoveryRecorded(_) => Ok(()),
            Self::SubmissionFinalized(event) => {
                validate_vector("event.warnings", event.warnings.len())?;
                for warning in &event.warnings {
                    validate_string("event.warnings[]", warning)?;
                }
                Ok(())
            }
        }
    }
}

impl EditorTransaction {
    /// Validates transaction invariants that do not require document contents.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.version_before.checked_add(1) != Some(self.version_after) {
            return Err(ValidationError::InvalidVersionTransition {
                before: self.version_before,
                after: self.version_after,
            });
        }
        validate_text_edits(&self.edits)
    }
}

/// Validates size, range ordering, canonical order, and non-overlap.
///
/// Equal-offset insertions retain vector order. A zero-width insertion may
/// precede a replacement at the same offset; all other overlap is rejected.
pub fn validate_text_edits(edits: &[TextEdit]) -> Result<(), ValidationError> {
    validate_vector("transaction.edits", edits.len())?;

    let mut previous: Option<(u64, u64)> = None;
    for (index, edit) in edits.iter().enumerate() {
        edit.validate()?;
        if let Some((previous_start, previous_end)) = previous {
            if (edit.start_byte, edit.end_byte) < (previous_start, previous_end) {
                return Err(ValidationError::EditsNotCanonical {
                    previous_index: index - 1,
                    edit_index: index,
                });
            }
            if edit.start_byte < previous_end {
                return Err(ValidationError::EditsOverlap {
                    previous_index: index - 1,
                    edit_index: index,
                });
            }
        }
        previous = Some((edit.start_byte, edit.end_byte));
    }
    Ok(())
}

impl TextEdit {
    /// Validates the schema-level range ordering and inserted-text limit.
    ///
    /// Whether the offsets are in bounds and on UTF-8 boundaries depends on
    /// the document snapshot and is validated by the editor mutation core.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.end_byte < self.start_byte {
            return Err(ValidationError::InvalidByteRange {
                start: self.start_byte,
                end: self.end_byte,
            });
        }
        validate_inserted_text(&self.inserted_text)
    }
}

/// Validates borrowed inserted text before a caller allocates a [`TextEdit`].
pub fn validate_inserted_text(inserted_text: &str) -> Result<(), ValidationError> {
    validate_limited_string(
        "text_edit.inserted_text",
        inserted_text,
        MAX_INSERTED_TEXT_BYTES,
    )
}

impl TextRange {
    fn validate(&self) -> Result<(), ValidationError> {
        let start = (self.start_line, self.start_column);
        let end = (self.end_line, self.end_column);
        if end < start {
            Err(ValidationError::InvalidTextRange)
        } else {
            Ok(())
        }
    }
}

fn validate_string(field: &'static str, value: &str) -> Result<(), ValidationError> {
    validate_limited_string(field, value, MAX_STRING_BYTES)
}

fn validate_text(field: &'static str, value: &str) -> Result<(), ValidationError> {
    validate_limited_string(field, value, MAX_INSERTED_TEXT_BYTES)
}

fn validate_limited_string(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), ValidationError> {
    if value.len() > maximum {
        Err(ValidationError::StringTooLong {
            field,
            actual: value.len(),
            maximum,
        })
    } else {
        Ok(())
    }
}

fn validate_vector(field: &'static str, actual: usize) -> Result<(), ValidationError> {
    if actual > MAX_VECTOR_ITEMS {
        Err(ValidationError::VectorTooLong {
            field,
            actual,
            maximum: MAX_VECTOR_ITEMS,
        })
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodePolicy {
    RejectUnsupported,
    SkipUnsupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
// Keep the common decoded case allocation-free for replay callers.
#[allow(clippy::large_enum_variant)]
pub enum DecodeOutcome {
    Decoded(EventEnvelope),
    Skipped(SkippedEvent),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkippedEvent {
    pub format_version: u32,
    pub sequence: Option<u64>,
    pub event_variant: Option<String>,
    pub encoded_len: usize,
    pub reason: SkipReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkipReason {
    UnsupportedVersion,
    UnknownEventVariant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EncodeError {
    Validation(ValidationError),
    JsonPreflight(DecodeError),
    Serialization { message: String },
    EnvelopeTooLarge { actual: usize, maximum: usize },
    HashMaterialLengthOutOfRange { actual: usize },
}

impl fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(error) => error.fmt(formatter),
            Self::JsonPreflight(error) => {
                write!(formatter, "canonical event JSON fails preflight: {error}")
            }
            Self::Serialization { message } => {
                write!(formatter, "failed to serialize event envelope: {message}")
            }
            Self::EnvelopeTooLarge { actual, maximum } => write!(
                formatter,
                "encoded event envelope is {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::HashMaterialLengthOutOfRange { actual } => write!(
                formatter,
                "canonical event hash material length {actual} does not fit in a u64"
            ),
        }
    }
}

impl Error for EncodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Validation(error) => Some(error),
            Self::JsonPreflight(error) => Some(error),
            Self::Serialization { .. }
            | Self::EnvelopeTooLarge { .. }
            | Self::HashMaterialLengthOutOfRange { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonStringKind {
    Key,
    Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonStringRepresentation {
    Raw,
    Decoded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonContainerKind {
    Array,
    Object,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeError {
    EnvelopeTooLarge {
        actual: usize,
        maximum: usize,
    },
    NestingTooDeep {
        actual: usize,
        maximum: usize,
    },
    JsonStringTooLong {
        kind: JsonStringKind,
        representation: JsonStringRepresentation,
        actual: usize,
        maximum: usize,
    },
    JsonContainerTooLarge {
        kind: JsonContainerKind,
        actual: usize,
        maximum: usize,
    },
    JsonValueLimitExceeded {
        actual: usize,
        maximum: usize,
    },
    DuplicateObjectKey {
        key: String,
    },
    InvalidJson {
        message: String,
        line: usize,
        column: usize,
    },
    InvalidEnvelope {
        message: String,
    },
    UnsupportedVersion {
        found: u32,
        supported: u32,
    },
    UnknownEventVariant {
        variant: String,
    },
    InvalidIdentifier {
        field: &'static str,
        source: IdentifierError,
    },
    InvalidHash {
        field: &'static str,
        source: HashError,
    },
    InvalidTimestamp {
        field: &'static str,
        value: String,
    },
    Validation(ValidationError),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EnvelopeTooLarge { actual, maximum } => write!(
                formatter,
                "encoded event envelope is {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::NestingTooDeep { actual, maximum } => write!(
                formatter,
                "JSON nesting depth is {actual}; maximum is {maximum}"
            ),
            Self::JsonStringTooLong {
                kind,
                representation,
                actual,
                maximum,
            } => write!(
                formatter,
                "JSON {kind:?} {representation:?} length is {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::JsonContainerTooLarge {
                kind,
                actual,
                maximum,
            } => write!(
                formatter,
                "JSON {kind:?} has {actual} entries; maximum is {maximum}"
            ),
            Self::JsonValueLimitExceeded { actual, maximum } => write!(
                formatter,
                "JSON contains {actual} values; maximum is {maximum}"
            ),
            Self::DuplicateObjectKey { key } => {
                write!(formatter, "JSON object contains duplicate key {key:?}")
            }
            Self::InvalidJson {
                message,
                line,
                column,
            } => write!(
                formatter,
                "invalid JSON at line {line}, column {column}: {message}"
            ),
            Self::InvalidEnvelope { message } => write!(formatter, "invalid envelope: {message}"),
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "format version {found} is unsupported; supported version is {supported}"
            ),
            Self::UnknownEventVariant { variant } => {
                write!(
                    formatter,
                    "event variant {variant:?} is unknown in format version 1"
                )
            }
            Self::InvalidIdentifier { field, source } => {
                write!(formatter, "invalid {field}: {source}")
            }
            Self::InvalidHash { field, source } => write!(formatter, "invalid {field}: {source}"),
            Self::InvalidTimestamp { field, value } => {
                write!(
                    formatter,
                    "invalid RFC 3339 timestamp in {field}: {value:?}"
                )
            }
            Self::Validation(error) => error.fmt(formatter),
        }
    }
}

impl Error for DecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidIdentifier { source, .. } => Some(source),
            Self::InvalidHash { source, .. } => Some(source),
            Self::Validation(error) => Some(error),
            _ => None,
        }
    }
}

struct CappedWriter<W> {
    inner: W,
    written: usize,
    exceeded_at: Option<usize>,
}

impl<W> CappedWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            written: 0,
            exceeded_at: None,
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
            self.exceeded_at = Some(usize::MAX);
            return Err(io::Error::other("event envelope encoding limit exceeded"));
        };
        if next > MAX_ENVELOPE_BYTES {
            self.exceeded_at = Some(next);
            return Err(io::Error::other("event envelope encoding limit exceeded"));
        }
        let count = self.inner.write(bytes)?;
        self.written += count;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn map_encode_error(error: serde_json::Error, exceeded_at: Option<usize>) -> EncodeError {
    match exceeded_at {
        Some(actual) => EncodeError::EnvelopeTooLarge {
            actual,
            maximum: MAX_ENVELOPE_BYTES,
        },
        None => EncodeError::Serialization {
            message: error.to_string(),
        },
    }
}

fn encoded_envelope_len(envelope: &EventEnvelope) -> Result<usize, EncodeError> {
    envelope.validate().map_err(EncodeError::Validation)?;
    let mut writer = CappedWriter::new(io::sink());
    if let Err(error) = serde_json::to_writer(&mut writer, envelope) {
        return Err(map_encode_error(error, writer.exceeded_at));
    }
    if matches!(envelope.event, Event::PasteRejected(_))
        && writer.written > MAX_PASTE_REJECTION_BYTES
    {
        return Err(EncodeError::EnvelopeTooLarge {
            actual: writer.written,
            maximum: MAX_PASTE_REJECTION_BYTES,
        });
    }
    Ok(writer.written)
}

pub fn encode_envelope(envelope: &EventEnvelope) -> Result<Vec<u8>, EncodeError> {
    let encoded_len = encoded_envelope_len(envelope)?;
    let mut writer = CappedWriter::new(Vec::with_capacity(encoded_len));
    if let Err(error) = serde_json::to_writer(&mut writer, envelope) {
        return Err(map_encode_error(error, writer.exceeded_at));
    }
    debug_assert_eq!(writer.written, encoded_len);
    let encoded = writer.into_inner();
    preflight_json(&encoded).map_err(EncodeError::JsonPreflight)?;
    Ok(encoded)
}

/// Encodes the bounded canonical bytes `e_i` used by the event hash chain.
///
/// The compact JSON contains, in order, `format_version`, `session_id`,
/// `sequence`, `monotonic_millis`, `wall_clock_utc`, and `event`. It excludes
/// `previous_event_hash` and `event_hash` regardless of their supplied values.
pub fn encode_event_hash_material(envelope: &EventEnvelope) -> Result<Vec<u8>, EncodeError> {
    envelope.validate().map_err(EncodeError::Validation)?;
    let material = EventHashMaterial::from(envelope);

    let mut counter = CappedWriter::new(io::sink());
    if let Err(error) = serde_json::to_writer(&mut counter, &material) {
        return Err(map_encode_error(error, counter.exceeded_at));
    }

    let mut writer = CappedWriter::new(Vec::with_capacity(counter.written));
    if let Err(error) = serde_json::to_writer(&mut writer, &material) {
        return Err(map_encode_error(error, writer.exceeded_at));
    }
    debug_assert_eq!(writer.written, counter.written);
    let encoded = writer.into_inner();
    preflight_json(&encoded).map_err(EncodeError::JsonPreflight)?;
    Ok(encoded)
}

/// Computes `BLAKE3(previous_hash || u64_be(length(e_i)) || e_i)`.
///
/// The previous hash is explicit and authoritative; chain fields already in
/// `envelope` never contribute to `e_i`.
pub fn compute_event_hash(
    previous_event_hash: Hash,
    envelope: &EventEnvelope,
) -> Result<Hash, EncodeError> {
    let material = encode_event_hash_material(envelope)?;
    let material_length =
        u64::try_from(material.len()).map_err(|_| EncodeError::HashMaterialLengthOutOfRange {
            actual: material.len(),
        })?;

    let mut hasher = blake3::Hasher::new();
    hasher.update(previous_event_hash.as_bytes());
    hasher.update(&material_length.to_be_bytes());
    hasher.update(&material);
    Ok(Hash::from_bytes(*hasher.finalize().as_bytes()))
}

pub fn decode_envelope(encoded: &[u8], policy: DecodePolicy) -> Result<DecodeOutcome, DecodeError> {
    if encoded.len() > MAX_ENVELOPE_BYTES {
        return Err(DecodeError::EnvelopeTooLarge {
            actual: encoded.len(),
            maximum: MAX_ENVELOPE_BYTES,
        });
    }
    preflight_json(encoded)?;

    let value: Value =
        serde_json::from_slice(encoded).map_err(|error| DecodeError::InvalidJson {
            message: error.to_string(),
            line: error.line(),
            column: error.column(),
        })?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid_envelope("root must be an object"))?;
    let format_version = required_u32(object, "format_version")?;
    let sequence = optional_u64(object.get("sequence"));
    let event_variant = optional_event_variant(object)?;

    if format_version != FORMAT_VERSION_V1 {
        return unsupported_outcome(
            policy,
            SkippedEvent {
                format_version,
                sequence,
                event_variant,
                encoded_len: encoded.len(),
                reason: SkipReason::UnsupportedVersion,
            },
            DecodeError::UnsupportedVersion {
                found: format_version,
                supported: FORMAT_VERSION_V1,
            },
        );
    }

    prevalidate_v1_envelope(object)?;
    let event_variant =
        event_variant.ok_or_else(|| invalid_envelope("event.type must be a string"))?;
    if event_variant == "paste_rejected" && encoded.len() > MAX_PASTE_REJECTION_BYTES {
        return Err(DecodeError::EnvelopeTooLarge {
            actual: encoded.len(),
            maximum: MAX_PASTE_REJECTION_BYTES,
        });
    }
    if !is_known_event_variant(&event_variant) {
        return unsupported_outcome(
            policy,
            SkippedEvent {
                format_version,
                sequence,
                event_variant: Some(event_variant.clone()),
                encoded_len: encoded.len(),
                reason: SkipReason::UnknownEventVariant,
            },
            DecodeError::UnknownEventVariant {
                variant: event_variant,
            },
        );
    }

    let wire: WireEventEnvelope =
        serde_json::from_value(value).map_err(|error| DecodeError::InvalidEnvelope {
            message: error.to_string(),
        })?;
    let envelope = EventEnvelope::from(wire);
    envelope.validate().map_err(DecodeError::Validation)?;
    Ok(DecodeOutcome::Decoded(envelope))
}

fn unsupported_outcome(
    policy: DecodePolicy,
    skipped: SkippedEvent,
    error: DecodeError,
) -> Result<DecodeOutcome, DecodeError> {
    match policy {
        DecodePolicy::RejectUnsupported => Err(error),
        DecodePolicy::SkipUnsupported => Ok(DecodeOutcome::Skipped(skipped)),
    }
}

fn prevalidate_v1_envelope(object: &serde_json::Map<String, Value>) -> Result<(), DecodeError> {
    let session_id = required_string(object, "session_id")?;
    SessionId::new(session_id).map_err(|source| DecodeError::InvalidIdentifier {
        field: "session_id",
        source,
    })?;

    let sequence = required_u64(object, "sequence")?;
    if sequence == 0 {
        return Err(DecodeError::Validation(
            ValidationError::SequenceMustStartAtOne,
        ));
    }

    let monotonic_millis = required_u64(object, "monotonic_millis")?;
    if monotonic_millis > MAX_MONOTONIC_MILLIS {
        return Err(DecodeError::Validation(
            ValidationError::MonotonicMillisOutOfRange {
                actual: monotonic_millis,
                maximum: MAX_MONOTONIC_MILLIS,
            },
        ));
    }

    match object.get("wall_clock_utc") {
        Some(Value::Null) => {}
        Some(Value::String(value)) => {
            validate_string("wall_clock_utc", value).map_err(DecodeError::Validation)?;
            DateTime::parse_from_rfc3339(value).map_err(|_| DecodeError::InvalidTimestamp {
                field: "wall_clock_utc",
                value: value.clone(),
            })?;
        }
        Some(_) => return Err(invalid_envelope("wall_clock_utc must be a string or null")),
        None => return Err(invalid_envelope("missing field wall_clock_utc")),
    }

    prevalidate_hash(object, "previous_event_hash")?;
    prevalidate_hash(object, "event_hash")?;
    Ok(())
}

fn prevalidate_hash(
    object: &serde_json::Map<String, Value>,
    field: &'static str,
) -> Result<(), DecodeError> {
    let value = required_string(object, field)?;
    Hash::from_str(value)
        .map(|_| ())
        .map_err(|source| DecodeError::InvalidHash { field, source })
}

fn required_u32(
    object: &serde_json::Map<String, Value>,
    field: &'static str,
) -> Result<u32, DecodeError> {
    let value = required_u64(object, field)?;
    u32::try_from(value).map_err(|_| invalid_envelope("format_version must fit in a u32"))
}

fn required_u64(
    object: &serde_json::Map<String, Value>,
    field: &'static str,
) -> Result<u64, DecodeError> {
    object.get(field).and_then(Value::as_u64).ok_or_else(|| {
        invalid_envelope(format!("missing or invalid unsigned integer field {field}"))
    })
}

fn required_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &'static str,
) -> Result<&'a str, DecodeError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_envelope(format!("missing or invalid string field {field}")))
}

fn optional_u64(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}

fn optional_event_variant(
    object: &serde_json::Map<String, Value>,
) -> Result<Option<String>, DecodeError> {
    let Some(event) = object.get("event").and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(variant) = event.get("type").and_then(Value::as_str) else {
        return Ok(None);
    };
    validate_string("event.type", variant).map_err(DecodeError::Validation)?;
    Ok(Some(variant.to_owned()))
}

fn is_known_event_variant(variant: &str) -> bool {
    matches!(
        variant,
        "session_started"
            | "controlled_command_started"
            | "controlled_command_output"
            | "controlled_command_finished"
            | "test_case_compared"
            | "session_resumed"
            | "session_ended"
            | "file_created"
            | "file_deleted"
            | "file_renamed"
            | "file_focused"
            | "file_edited"
            | "clipboard_copied"
            | "internal_paste"
            | "paste_rejected"
            | "selection_changed"
            | "viewport_changed"
            | "cargo_command_started"
            | "cargo_diagnostic"
            | "cargo_output"
            | "cargo_command_finished"
            | "lsp_completion_requested"
            | "lsp_completion_accepted"
            | "lsp_code_action_applied"
            | "workspace_checkpoint"
            | "external_file_change"
            | "external_observation"
            | "recovery_recorded"
            | "submission_finalized"
    )
}

/// Validates bounded JSON structure before callers perform typed decoding.
///
/// Callers remain responsible for enforcing their raw payload byte limit
/// before invoking this scanner.
pub fn preflight_json(encoded: &[u8]) -> Result<(), DecodeError> {
    let source = std::str::from_utf8(encoded).map_err(|error| {
        invalid_json_at(
            encoded,
            error.valid_up_to(),
            "input is not valid UTF-8".to_owned(),
        )
    })?;
    JsonPreflight::new(source).validate()
}

struct JsonPreflight<'a> {
    source: &'a str,
    bytes: &'a [u8],
    position: usize,
    value_count: usize,
}

impl<'a> JsonPreflight<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            position: 0,
            value_count: 0,
        }
    }

    fn validate(mut self) -> Result<(), DecodeError> {
        self.skip_whitespace();
        self.parse_value(0)?;
        self.skip_whitespace();
        if self.position != self.bytes.len() {
            return Err(self.syntax("trailing characters after JSON value"));
        }
        Ok(())
    }

    fn parse_value(&mut self, depth: usize) -> Result<(), DecodeError> {
        self.skip_whitespace();
        self.value_count += 1;
        if self.value_count > MAX_JSON_VALUES {
            return Err(DecodeError::JsonValueLimitExceeded {
                actual: self.value_count,
                maximum: MAX_JSON_VALUES,
            });
        }

        match self.current() {
            Some(b'{') => self.parse_object(depth + 1),
            Some(b'[') => self.parse_array(depth + 1),
            Some(b'"') => self.parse_string(JsonStringKind::Value, false).map(|_| ()),
            Some(b't') => self.parse_literal(b"true"),
            Some(b'f') => self.parse_literal(b"false"),
            Some(b'n') => self.parse_literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(_) => Err(self.syntax("expected a JSON value")),
            None => Err(self.syntax("unexpected end of input")),
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<(), DecodeError> {
        self.check_depth(depth)?;
        self.position += 1;
        self.skip_whitespace();
        if self.consume_if(b'}') {
            return Ok(());
        }

        let mut keys = HashSet::new();
        let mut members = 0usize;
        loop {
            members += 1;
            if members > MAX_VECTOR_ITEMS {
                return Err(DecodeError::JsonContainerTooLarge {
                    kind: JsonContainerKind::Object,
                    actual: members,
                    maximum: MAX_VECTOR_ITEMS,
                });
            }
            if self.current() != Some(b'"') {
                return Err(self.syntax("expected a JSON object key"));
            }
            let key = self.parse_string(JsonStringKind::Key, true)?;
            if keys.contains(&key) {
                return Err(DecodeError::DuplicateObjectKey { key });
            }
            keys.insert(key);

            self.skip_whitespace();
            self.expect_byte(b':', "expected ':' after JSON object key")?;
            self.parse_value(depth)?;
            self.skip_whitespace();
            if self.consume_if(b'}') {
                return Ok(());
            }
            self.expect_byte(b',', "expected ',' or '}' in JSON object")?;
            self.skip_whitespace();
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<(), DecodeError> {
        self.check_depth(depth)?;
        self.position += 1;
        self.skip_whitespace();
        if self.consume_if(b']') {
            return Ok(());
        }

        let mut items = 0usize;
        loop {
            items += 1;
            if items > MAX_VECTOR_ITEMS {
                return Err(DecodeError::JsonContainerTooLarge {
                    kind: JsonContainerKind::Array,
                    actual: items,
                    maximum: MAX_VECTOR_ITEMS,
                });
            }
            self.parse_value(depth)?;
            self.skip_whitespace();
            if self.consume_if(b']') {
                return Ok(());
            }
            self.expect_byte(b',', "expected ',' or ']' in JSON array")?;
            self.skip_whitespace();
        }
    }

    fn parse_string(&mut self, kind: JsonStringKind, capture: bool) -> Result<String, DecodeError> {
        self.position += 1;
        let raw_start = self.position;
        let mut decoded_length = 0usize;
        let mut decoded = String::new();

        loop {
            let Some(byte) = self.current() else {
                return Err(self.syntax("unterminated JSON string"));
            };
            let character = match byte {
                b'"' => {
                    self.check_string_lengths(kind, raw_start, decoded_length)?;
                    self.position += 1;
                    return Ok(decoded);
                }
                b'\\' => self.parse_escape()?,
                0x00..=0x1f => {
                    return Err(self.syntax("unescaped control character in JSON string"));
                }
                0x20..=0x7f => {
                    self.position += 1;
                    char::from(byte)
                }
                _ => {
                    let character = self.source[self.position..]
                        .chars()
                        .next()
                        .ok_or_else(|| self.syntax("invalid UTF-8 in JSON string"))?;
                    self.position += character.len_utf8();
                    character
                }
            };

            decoded_length += character.len_utf8();
            if capture {
                decoded.push(character);
            }
            self.check_string_lengths(kind, raw_start, decoded_length)?;
        }
    }

    fn parse_escape(&mut self) -> Result<char, DecodeError> {
        self.position += 1;
        let Some(escaped) = self.current() else {
            return Err(self.syntax("unterminated JSON escape"));
        };
        self.position += 1;

        match escaped {
            b'"' => Ok('"'),
            b'\\' => Ok('\\'),
            b'/' => Ok('/'),
            b'b' => Ok('\u{0008}'),
            b'f' => Ok('\u{000c}'),
            b'n' => Ok('\n'),
            b'r' => Ok('\r'),
            b't' => Ok('\t'),
            b'u' => {
                let high = self.parse_hex_quad()?;
                let scalar = if (0xd800..=0xdbff).contains(&high) {
                    if self.current() != Some(b'\\')
                        || self.bytes.get(self.position + 1) != Some(&b'u')
                    {
                        return Err(
                            self.syntax("high surrogate must be followed by a low surrogate")
                        );
                    }
                    self.position += 2;
                    let low = self.parse_hex_quad()?;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return Err(
                            self.syntax("high surrogate must be followed by a low surrogate")
                        );
                    }
                    0x1_0000 + ((high - 0xd800) << 10) + (low - 0xdc00)
                } else if (0xdc00..=0xdfff).contains(&high) {
                    return Err(self.syntax("low surrogate must follow a high surrogate"));
                } else {
                    high
                };
                char::from_u32(scalar)
                    .ok_or_else(|| self.syntax("invalid Unicode scalar in JSON escape"))
            }
            _ => Err(self.syntax("invalid JSON escape")),
        }
    }

    fn parse_hex_quad(&mut self) -> Result<u32, DecodeError> {
        let mut value = 0u32;
        for _ in 0..4 {
            let Some(byte) = self.current() else {
                return Err(self.syntax("incomplete Unicode escape"));
            };
            let digit = match byte {
                b'0'..=b'9' => u32::from(byte - b'0'),
                b'a'..=b'f' => u32::from(byte - b'a' + 10),
                b'A'..=b'F' => u32::from(byte - b'A' + 10),
                _ => return Err(self.syntax("invalid hexadecimal digit in Unicode escape")),
            };
            value = (value << 4) | digit;
            self.position += 1;
        }
        Ok(value)
    }

    fn parse_literal(&mut self, literal: &[u8]) -> Result<(), DecodeError> {
        let end = self.position + literal.len();
        if self.bytes.get(self.position..end) != Some(literal) {
            return Err(self.syntax("invalid JSON literal"));
        }
        self.position = end;
        Ok(())
    }

    fn parse_number(&mut self) -> Result<(), DecodeError> {
        self.consume_if(b'-');
        match self.current() {
            Some(b'0') => self.position += 1,
            Some(b'1'..=b'9') => {
                self.position += 1;
                self.consume_digits();
            }
            _ => return Err(self.syntax("invalid JSON number")),
        }

        if self.consume_if(b'.') && self.consume_digits() == 0 {
            return Err(self.syntax("JSON fraction requires at least one digit"));
        }
        if matches!(self.current(), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.current(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            if self.consume_digits() == 0 {
                return Err(self.syntax("JSON exponent requires at least one digit"));
            }
        }
        Ok(())
    }

    fn consume_digits(&mut self) -> usize {
        let start = self.position;
        while matches!(self.current(), Some(b'0'..=b'9')) {
            self.position += 1;
        }
        self.position - start
    }

    fn check_depth(&self, depth: usize) -> Result<(), DecodeError> {
        if depth > MAX_JSON_NESTING {
            Err(DecodeError::NestingTooDeep {
                actual: depth,
                maximum: MAX_JSON_NESTING,
            })
        } else {
            Ok(())
        }
    }

    fn check_string_lengths(
        &self,
        kind: JsonStringKind,
        raw_start: usize,
        decoded_length: usize,
    ) -> Result<(), DecodeError> {
        let raw_length = self.position - raw_start;
        let raw_maximum = match kind {
            JsonStringKind::Key => MAX_JSON_KEY_BYTES,
            JsonStringKind::Value => MAX_JSON_RAW_STRING_BYTES,
        };
        if raw_length > raw_maximum {
            return Err(DecodeError::JsonStringTooLong {
                kind,
                representation: JsonStringRepresentation::Raw,
                actual: raw_length,
                maximum: raw_maximum,
            });
        }

        let decoded_maximum = match kind {
            JsonStringKind::Key => MAX_JSON_KEY_BYTES,
            JsonStringKind::Value => MAX_JSON_STRING_BYTES,
        };
        if decoded_length > decoded_maximum {
            return Err(DecodeError::JsonStringTooLong {
                kind,
                representation: JsonStringRepresentation::Decoded,
                actual: decoded_length,
                maximum: decoded_maximum,
            });
        }
        Ok(())
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.current(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.position += 1;
        }
    }

    fn current(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn consume_if(&mut self, expected: u8) -> bool {
        if self.current() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect_byte(&mut self, expected: u8, message: &'static str) -> Result<(), DecodeError> {
        if self.consume_if(expected) {
            Ok(())
        } else {
            Err(self.syntax(message))
        }
    }

    fn syntax(&self, message: impl Into<String>) -> DecodeError {
        invalid_json_at(self.bytes, self.position, message.into())
    }
}

fn invalid_json_at(encoded: &[u8], position: usize, message: String) -> DecodeError {
    let position = position.min(encoded.len());
    let line = encoded[..position]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1;
    let line_start = encoded[..position]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    DecodeError::InvalidJson {
        message,
        line,
        column: position - line_start + 1,
    }
}

fn invalid_envelope(message: impl Into<String>) -> DecodeError {
    DecodeError::InvalidEnvelope {
        message: message.into(),
    }
}
