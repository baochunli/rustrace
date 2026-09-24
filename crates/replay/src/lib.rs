//! Deterministic, terminal-independent workspace replay.
//!
//! Replay reconstructs recorded workspace state and checks internal integrity.
//! It is a pure in-memory operation: it never reads a student's workspace,
//! executes recorded commands, compiles code, or invokes tools. Hash and chain
//! validation detect inconsistency, but are not authenticity or attestation;
//! a fully fabricated self-consistent record can still pass validation.

#![forbid(unsafe_code)]

mod command;

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use rustrace_editor::{ReplayDocument, TransactionError};
use rustrace_journal::{
    CheckpointError, CheckpointSnapshot, MAX_CHECKPOINT_DOCUMENTS, MAX_CHECKPOINT_TOTAL_FILE_BYTES,
    MAX_JOURNAL_SEQUENCE, StoredCheckpoint,
};
use rustrace_model::{
    ClipboardSource, CommandId, CompletionAccepted, CompletionRequested, DocumentHash, DocumentId,
    EditOrigin, EncodeError, Event, EventEnvelope, ExternalFileChange, FileCreated, FileDeleted,
    FileRenamed, Hash, InternalPaste, MAX_ENVELOPE_BYTES, MAX_VECTOR_ITEMS, RecordedEventRef,
    SelectionState, SessionId, TextEdit, WorkspaceCheckpoint, WorkspacePath, compute_event_hash,
    document_hash, encode_envelope,
};
use rustrace_workspace::hash::{WorkspaceHashError, hash_entries};

/// Maximum number of Cargo commands whose finish events may still be pending.
pub const MAX_ACTIVE_CARGO_COMMANDS: usize = MAX_VECTOR_ITEMS;

fn clipboard_error(detail: &str) -> ReplayError {
    ReplayError::NonMutatingEvent {
        event: "internal_clipboard",
        detail: detail.to_owned(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentState {
    path: WorkspacePath,
    document: ReplayDocument,
}

impl DocumentState {
    pub fn document_id(&self) -> &DocumentId {
        self.document.document_id()
    }

    pub const fn path(&self) -> &WorkspacePath {
        &self.path
    }

    pub fn text(&self) -> &str {
        self.document.text()
    }

    pub const fn version(&self) -> u64 {
        self.document.version()
    }

    pub const fn selection(&self) -> SelectionState {
        self.document.selection()
    }

    pub const fn content_hash(&self) -> Hash {
        self.document.hash()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceState {
    files: BTreeMap<WorkspacePath, Vec<u8>>,
    documents: BTreeMap<DocumentId, DocumentState>,
    active_document: Option<DocumentId>,
    workspace_hash: Hash,
}

impl WorkspaceState {
    pub fn files(&self) -> &BTreeMap<WorkspacePath, Vec<u8>> {
        &self.files
    }

    pub fn file(&self, path: &WorkspacePath) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }

    pub fn documents(&self) -> &BTreeMap<DocumentId, DocumentState> {
        &self.documents
    }

    pub fn document(&self, document_id: &DocumentId) -> Option<&DocumentState> {
        self.documents.get(document_id)
    }

    pub fn active_document(&self) -> Option<&DocumentId> {
        self.active_document.as_ref()
    }

    pub const fn workspace_hash(&self) -> Hash {
        self.workspace_hash
    }

    fn hash_after_change(
        &self,
        removed_path: Option<&WorkspacePath>,
        added_file: Option<(&WorkspacePath, &[u8])>,
    ) -> Result<Hash, ReplayError> {
        hash_entries(
            self.files
                .iter()
                .filter(|(path, _)| removed_path != Some(*path))
                .map(|(path, contents)| (path, contents.as_slice()))
                .chain(added_file),
        )
        .map_err(|source| ReplayError::WorkspaceLimit { source })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalState {
    SessionEnded,
    SubmissionFinalized,
}

/// An exact, process-local seek point certified by continuous replay.
///
/// Its fields and construction are intentionally private. Raw persisted
/// checkpoints are storage-valid but cannot establish complete editor or
/// derived lifecycle state for a later seek.
#[derive(Clone)]
pub struct CertifiedCheckpoint {
    replay: ReplayEngine,
}

impl CertifiedCheckpoint {
    pub fn cursor(&self) -> CertifiedCheckpointCursor {
        CertifiedCheckpointCursor::from_replay(&self.replay)
    }
}

/// Compact process-local proof that a persisted checkpoint matched one exact
/// continuously replayed prefix.
///
/// The workspace bytes stay in the validated package spool. Restoring requires
/// the same raw checkpoint again and revalidates its owner and snapshot before
/// combining it with this derived lifecycle state.
#[derive(Clone)]
pub struct CertifiedCheckpointCursor {
    session_id: SessionId,
    next_sequence: u64,
    last_event_hash: Hash,
    last_event_previous_hash: Hash,
    workspace_hash: Hash,
    terminal: Option<TerminalState>,
    active_cargo_commands: BTreeSet<CommandId>,
    controlled: command::CommandReplay,
    clipboard: Option<CopiedSnapshot>,
}

impl CertifiedCheckpointCursor {
    fn from_replay(replay: &ReplayEngine) -> Self {
        Self {
            session_id: replay.session_id.clone(),
            next_sequence: replay.next_sequence,
            last_event_hash: replay.last_event_hash,
            last_event_previous_hash: replay.last_event_previous_hash,
            workspace_hash: replay.state.workspace_hash,
            terminal: replay.terminal,
            active_cargo_commands: replay.active_cargo_commands.clone(),
            controlled: replay.controlled.clone(),
            clipboard: replay.clipboard.clone(),
        }
    }

    /// Conservative retained-heap charge for bounded application caches.
    pub fn retained_bytes(&self) -> usize {
        let mut bytes = 1_024_usize
            .saturating_add(self.session_id.as_str().len())
            .saturating_add(
                self.active_cargo_commands
                    .iter()
                    .map(|command| command.as_str().len().saturating_add(128))
                    .sum::<usize>(),
            );
        if self.controlled.active.is_some() {
            // The complete started-command value came from one bounded event.
            bytes = bytes.saturating_add(MAX_ENVELOPE_BYTES);
            bytes = bytes.saturating_add(
                self.controlled
                    .active
                    .as_ref()
                    .map_or(0, |command| command.stdout.len()),
            );
        }
        if self.controlled.last_finished.is_some() {
            bytes = bytes.saturating_add(MAX_ENVELOPE_BYTES.saturating_mul(2));
            bytes = bytes.saturating_add(
                self.controlled
                    .last_finished
                    .as_ref()
                    .map_or(0, |command| command.stdout.len()),
            );
        }
        if let Some(clipboard) = &self.clipboard {
            bytes = bytes
                .saturating_add(clipboard.text.len())
                .saturating_add(clipboard.reference.session_id.as_str().len())
                .saturating_add(128);
        }
        bytes
    }
}

#[derive(Clone)]
pub struct ReplayEngine {
    session_id: SessionId,
    next_sequence: u64,
    last_event_hash: Hash,
    last_event_previous_hash: Hash,
    state: WorkspaceState,
    terminal: Option<TerminalState>,
    active_cargo_commands: BTreeSet<CommandId>,
    controlled: command::CommandReplay,
    clipboard: Option<CopiedSnapshot>,
}

/// Derived historical evidence, not live permission to paste. One bounded slot
/// is enough because every explicit copy replaces the previous one.
#[derive(Clone)]
struct CopiedSnapshot {
    reference: RecordedEventRef,
    text: String,
}

impl ReplayEngine {
    /// Interrupted controlled commands remain incomplete, never a clean exit.
    /// Production resume finishes one only after proving its processes exited.
    pub fn controlled_command_pending(&self) -> bool {
        self.controlled.active.is_some()
    }

    /// The pending command's start, start time, and journaled
    /// `[stdout, stderr]` byte counts.
    pub fn pending_controlled_command(
        &self,
    ) -> Option<(&rustrace_model::ControlledCommandStarted, u64, [u64; 2])> {
        self.controlled
            .active
            .as_ref()
            .map(|active| (&active.started, active.millis, active.bytes))
    }

    /// Bounded captured stdout from the immediately preceding command finish.
    /// A following comparison or any intervening event consumes this route.
    pub fn just_finished_command_stdout(&self) -> Option<&[u8]> {
        self.controlled
            .last_finished
            .as_ref()
            .map(|command| command.stdout.as_slice())
    }

    pub fn command_output_bytes(&self) -> u64 {
        self.controlled.output_bytes
    }

    pub fn command_tree_link(&self) -> Option<&rustrace_model::CommandTreeLink> {
        self.controlled.checkpoint.as_ref()
    }

    pub fn workspace_version(&self) -> u64 {
        self.controlled.workspace_version
    }
    /// Restores the sequence-one checkpoint that anchors replay for a session.
    pub fn from_initial_checkpoint(checkpoint: StoredCheckpoint) -> Result<Self, ReplayError> {
        if checkpoint.owning_event.sequence != 1 {
            return Err(ReplayError::InitialCheckpointNotGenesis {
                sequence: checkpoint.owning_event.sequence,
            });
        }
        if checkpoint.owning_event.previous_event_hash != Hash::zero() {
            return Err(ReplayError::PreviousEventHashMismatch {
                sequence: 1,
                expected: Hash::zero(),
                actual: checkpoint.owning_event.previous_event_hash,
            });
        }
        let state = validate_stored_checkpoint(&checkpoint)?;
        let controlled = command::CommandReplay::new(
            1,
            checkpoint.owning_event.event_hash,
            state.workspace_hash,
            checkpoint.owning_event.monotonic_millis,
        );
        Ok(Self {
            session_id: checkpoint.owning_event.session_id,
            next_sequence: 2,
            last_event_hash: checkpoint.owning_event.event_hash,
            last_event_previous_hash: Hash::zero(),
            state,
            terminal: None,
            active_cargo_commands: BTreeSet::new(),
            controlled,
            clipboard: None,
        })
    }

    /// Restores a later seek point previously certified by continuous replay.
    pub fn from_checkpoint(checkpoint: CertifiedCheckpoint) -> Self {
        checkpoint.replay
    }

    /// Restores a compact certified cursor using the exact persisted
    /// checkpoint that was certified. Package ownership and event/snapshot
    /// hashes are checked again before the derived lifecycle is accepted.
    pub fn from_checkpoint_cursor(
        cursor: CertifiedCheckpointCursor,
        checkpoint: StoredCheckpoint,
    ) -> Result<Self, ReplayError> {
        let state = validate_stored_checkpoint(&checkpoint)?;
        let expected_sequence = cursor.next_sequence.checked_sub(1).ok_or_else(|| {
            ReplayError::CheckpointCertification {
                detail: "certified cursor has no owner sequence".to_owned(),
            }
        })?;
        if checkpoint.owning_event.session_id != cursor.session_id
            || checkpoint.owning_event.sequence != expected_sequence
            || checkpoint.owning_event.event_hash != cursor.last_event_hash
            || checkpoint.owning_event.previous_event_hash != cursor.last_event_previous_hash
            || state.workspace_hash != cursor.workspace_hash
        {
            return Err(ReplayError::CheckpointCertification {
                detail: "persisted checkpoint does not match the certified cursor".to_owned(),
            });
        }
        Ok(Self {
            session_id: cursor.session_id,
            next_sequence: cursor.next_sequence,
            last_event_hash: cursor.last_event_hash,
            last_event_previous_hash: cursor.last_event_previous_hash,
            state,
            terminal: cursor.terminal,
            active_cargo_commands: cursor.active_cargo_commands,
            controlled: cursor.controlled,
            clipboard: cursor.clipboard,
        })
    }

    /// Certifies a raw checkpoint against the exact owner already replayed.
    ///
    /// Besides storage integrity, this compares raw files, document paths and
    /// contents, versions, selections, and the active document. Derived
    /// lifecycle state is captured opaquely for correct suffix validation.
    pub fn certify_checkpoint(
        &self,
        checkpoint: StoredCheckpoint,
    ) -> Result<CertifiedCheckpoint, ReplayError> {
        self.validate_checkpoint(&checkpoint)?;
        Ok(CertifiedCheckpoint {
            replay: Self {
                session_id: self.session_id.clone(),
                next_sequence: self.next_sequence,
                last_event_hash: self.last_event_hash,
                last_event_previous_hash: self.last_event_previous_hash,
                state: self.state.clone(),
                terminal: self.terminal,
                active_cargo_commands: self.active_cargo_commands.clone(),
                controlled: self.controlled.clone(),
                clipboard: self.clipboard.clone(),
            },
        })
    }

    /// Certifies a checkpoint while retaining only derived lifecycle needed
    /// to resume it. Workspace bytes remain in the caller-owned validated
    /// package rather than being duplicated in the certificate cache.
    pub fn certify_checkpoint_cursor(
        &self,
        checkpoint: &StoredCheckpoint,
    ) -> Result<CertifiedCheckpointCursor, ReplayError> {
        self.validate_checkpoint(checkpoint)?;
        Ok(CertifiedCheckpointCursor::from_replay(self))
    }

    /// Validates a raw checkpoint against the current replay state without
    /// cloning that state into a seek certificate.
    pub fn validate_checkpoint(&self, checkpoint: &StoredCheckpoint) -> Result<(), ReplayError> {
        if self.terminal.is_some() {
            return Err(ReplayError::CheckpointCertification {
                detail: "cannot certify after a terminal event".to_owned(),
            });
        }
        let restored = validate_stored_checkpoint(checkpoint)?;
        let expected_sequence = self.next_sequence.checked_sub(1).ok_or_else(|| {
            ReplayError::CheckpointCertification {
                detail: "replay cursor has no preceding owner sequence".to_owned(),
            }
        })?;
        if checkpoint.owning_event.session_id != self.session_id {
            return Err(ReplayError::WrongSession {
                expected: self.session_id.clone(),
                actual: checkpoint.owning_event.session_id.clone(),
            });
        }
        if checkpoint.owning_event.sequence != expected_sequence {
            return Err(ReplayError::CheckpointCertification {
                detail: format!(
                    "owner sequence is {}; replay most recently applied sequence {expected_sequence}",
                    checkpoint.owning_event.sequence
                ),
            });
        }
        if checkpoint.owning_event.event_hash != self.last_event_hash {
            return Err(ReplayError::CheckpointCertification {
                detail: format!(
                    "owner hash is {}; replay chain tip is {}",
                    checkpoint.owning_event.event_hash, self.last_event_hash
                ),
            });
        }
        if checkpoint.owning_event.previous_event_hash != self.last_event_previous_hash {
            return Err(ReplayError::CheckpointCertification {
                detail: format!(
                    "owner previous hash is {}; replay recorded {}",
                    checkpoint.owning_event.previous_event_hash, self.last_event_previous_hash
                ),
            });
        }
        if restored != self.state {
            return Err(ReplayError::CheckpointCertification {
                detail: "restored checkpoint state differs from the reached replay state"
                    .to_owned(),
            });
        }
        Ok(())
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    pub const fn last_event_hash(&self) -> Hash {
        self.last_event_hash
    }

    pub const fn workspace_state(&self) -> &WorkspaceState {
        &self.state
    }

    /// Consumes a transient replay cursor and transfers its reconstructed file
    /// bytes to a read-only presentation without cloning the workspace.
    pub fn into_workspace_projection(
        self,
    ) -> (BTreeMap<WorkspacePath, Vec<u8>>, Option<WorkspacePath>) {
        let active_path = self
            .state
            .active_document
            .as_ref()
            .and_then(|document_id| self.state.documents.get(document_id))
            .map(|document| document.path.clone());
        (self.state.files, active_path)
    }

    pub const fn current_workspace_hash(&self) -> Hash {
        self.state.workspace_hash
    }

    pub const fn is_finalized(&self) -> bool {
        matches!(self.terminal, Some(TerminalState::SubmissionFinalized))
    }

    pub const fn is_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    pub fn verify_final_workspace_hash(&self, expected: Hash) -> Result<(), ReplayError> {
        if self.state.workspace_hash == expected {
            Ok(())
        } else {
            Err(ReplayError::FinalWorkspaceHashMismatch {
                expected,
                actual: self.state.workspace_hash,
            })
        }
    }

    /// Validates and atomically applies one contiguous journal event.
    pub fn apply(&mut self, envelope: &EventEnvelope) -> Result<(), ReplayError> {
        self.validate_envelope(envelope)?;
        if self.terminal.is_some() {
            return Err(ReplayError::EventAfterTerminal {
                sequence: envelope.sequence,
            });
        }

        self.controlled.validate(
            envelope,
            !self.active_cargo_commands.is_empty(),
            &self.state,
        )?;
        self.apply_event(envelope)?;
        self.controlled.applied(envelope, self.state.workspace_hash);
        self.last_event_previous_hash = envelope.previous_event_hash;
        self.last_event_hash = envelope.event_hash;
        self.next_sequence =
            envelope
                .sequence
                .checked_add(1)
                .ok_or(ReplayError::SequenceOutOfRange {
                    actual: envelope.sequence,
                    maximum: MAX_JOURNAL_SEQUENCE,
                })?;
        Ok(())
    }

    fn validate_envelope(&self, envelope: &EventEnvelope) -> Result<(), ReplayError> {
        if envelope.sequence > MAX_JOURNAL_SEQUENCE || self.next_sequence > MAX_JOURNAL_SEQUENCE {
            return Err(ReplayError::SequenceOutOfRange {
                actual: envelope.sequence,
                maximum: MAX_JOURNAL_SEQUENCE,
            });
        }
        if envelope.session_id != self.session_id {
            return Err(ReplayError::WrongSession {
                expected: self.session_id.clone(),
                actual: envelope.session_id.clone(),
            });
        }
        if envelope.sequence != self.next_sequence {
            return Err(ReplayError::UnexpectedSequence {
                expected: self.next_sequence,
                actual: envelope.sequence,
            });
        }
        if envelope.previous_event_hash != self.last_event_hash {
            return Err(ReplayError::PreviousEventHashMismatch {
                sequence: envelope.sequence,
                expected: self.last_event_hash,
                actual: envelope.previous_event_hash,
            });
        }
        encode_envelope(envelope).map_err(ReplayError::EventEncoding)?;
        let expected_hash = compute_event_hash(self.last_event_hash, envelope)
            .map_err(ReplayError::EventEncoding)?;
        if envelope.event_hash != expected_hash {
            return Err(ReplayError::EventHashMismatch {
                sequence: envelope.sequence,
                expected: expected_hash,
                actual: envelope.event_hash,
            });
        }
        Ok(())
    }

    fn apply_event(&mut self, envelope: &EventEnvelope) -> Result<(), ReplayError> {
        match &envelope.event {
            Event::ControlledCommandStarted(_)
            | Event::ControlledCommandOutput(_)
            | Event::ControlledCommandFinished(_)
            | Event::TestCaseCompared(_) => Ok(()),
            Event::SessionStarted(_) => Err(ReplayError::SessionLifecycle {
                event: "session_started",
                detail: "session start cannot follow the replay checkpoint".to_owned(),
            }),
            Event::SessionResumed(event) => {
                let expected = envelope.sequence - 1;
                if event.last_sequence == expected {
                    self.clipboard = None;
                    Ok(())
                } else {
                    Err(ReplayError::NonMutatingEvent {
                        event: "session_resumed",
                        detail: format!(
                            "last_sequence is {}; expected {expected}",
                            event.last_sequence
                        ),
                    })
                }
            }
            Event::SessionEnded(event) => {
                self.verify_final_workspace_hash(event.final_workspace_hash)?;
                self.validate_no_active_cargo_commands("session_ended")?;
                self.terminal = Some(TerminalState::SessionEnded);
                self.clipboard = None;
                Ok(())
            }
            Event::FileCreated(event) => self.apply_file_created(event),
            Event::FileDeleted(event) => self.apply_file_deleted(event),
            Event::FileRenamed(event) => self.apply_file_renamed(event),
            Event::FileFocused(event) => {
                if !self.state.documents.contains_key(&event.document_id) {
                    return Err(ReplayError::FileLifecycle {
                        operation: "focus",
                        detail: format!("document {} is not open", event.document_id),
                    });
                }
                self.state.active_document = Some(event.document_id.clone());
                Ok(())
            }
            Event::FileEdited(transaction) => self.apply_file_edited(transaction),
            Event::ClipboardCopied(source) => {
                let text = self.validate_clipboard_source(source)?.to_owned();
                self.clipboard = Some(CopiedSnapshot {
                    reference: RecordedEventRef {
                        session_id: envelope.session_id.clone(),
                        sequence: envelope.sequence,
                        event_hash: envelope.event_hash,
                    },
                    text,
                });
                Ok(())
            }
            Event::InternalPaste(paste) => {
                self.validate_internal_paste(paste)?;
                self.apply_file_edited(&paste.transaction)
            }
            Event::PasteRejected(_) => Ok(()),
            Event::SelectionChanged(event) => {
                let document = self
                    .state
                    .documents
                    .get_mut(&event.document_id)
                    .ok_or_else(|| ReplayError::FileLifecycle {
                        operation: "select",
                        detail: format!("document {} is not open", event.document_id),
                    })?;
                document
                    .document
                    .set_selection(SelectionState::new(event.anchor_byte, event.active_byte))
                    .map_err(|source| ReplayError::Transaction {
                        document_id: event.document_id.clone(),
                        source,
                    })?;
                Ok(())
            }
            Event::ViewportChanged(event) => {
                self.require_document(&event.document_id, "viewport_changed")?;
                Ok(())
            }
            Event::CargoCommandStarted(event) => {
                if self.active_cargo_commands.contains(&event.command_id) {
                    return Err(ReplayError::CargoCommandAlreadyActive {
                        command_id: event.command_id.clone(),
                    });
                }
                if self.active_cargo_commands.len() >= MAX_ACTIVE_CARGO_COMMANDS {
                    return Err(ReplayError::ActiveCargoCommandLimit {
                        attempted: self.active_cargo_commands.len().saturating_add(1),
                        maximum: MAX_ACTIVE_CARGO_COMMANDS,
                    });
                }
                self.active_cargo_commands.insert(event.command_id.clone());
                Ok(())
            }
            Event::CargoDiagnostic(event) => {
                self.require_active_cargo_command("cargo_diagnostic", &event.command_id)
            }
            Event::CargoOutput(event) => {
                self.require_active_cargo_command("cargo_output", &event.command_id)
            }
            Event::CargoCommandFinished(event) => {
                self.require_active_cargo_command("cargo_command_finished", &event.command_id)?;
                self.active_cargo_commands.remove(&event.command_id);
                Ok(())
            }
            Event::LspCompletionRequested(event) => self.validate_completion_request(event),
            Event::LspCompletionAccepted(event) => self.validate_completion_acceptance(event),
            Event::LspCodeActionApplied(event) => {
                for document_id in &event.document_ids {
                    self.require_document(document_id, "lsp_code_action_applied")?;
                }
                Ok(())
            }
            Event::WorkspaceCheckpoint(event) => self.verify_checkpoint_event(event),
            Event::ExternalFileChange(event) => self.apply_external_file_change(event),
            Event::ExternalObservation(event) => {
                let logical = self
                    .state
                    .file(&event.path)
                    .map(rustrace_model::observation_hash);
                if logical != event.logical_hash {
                    return Err(ReplayError::NonMutatingEvent {
                        event: "external_observation",
                        detail: "logical view hash mismatch".to_owned(),
                    });
                }
                Ok(())
            }
            Event::RecoveryRecorded(_) => Ok(()),
            Event::SubmissionFinalized(event) => {
                self.verify_final_workspace_hash(event.final_workspace_hash)?;
                if event.event_count != envelope.sequence {
                    return Err(ReplayError::NonMutatingEvent {
                        event: "submission_finalized",
                        detail: format!(
                            "event_count is {}; expected {}",
                            event.event_count, envelope.sequence
                        ),
                    });
                }
                if event.clean {
                    self.validate_workspace_coherence("clean finalization")?;
                }
                self.validate_no_active_cargo_commands("submission_finalized")?;
                self.terminal = Some(TerminalState::SubmissionFinalized);
                self.clipboard = None;
                Ok(())
            }
        }
    }

    /// Validates exact selected bytes against the current durable source prefix.
    /// No pathname lookup, equal-text search, or later source version is used.
    pub fn validate_clipboard_source(&self, source: &ClipboardSource) -> Result<&str, ReplayError> {
        Event::ClipboardCopied(source.clone())
            .validate()
            .map_err(|error| ReplayError::EventEncoding(EncodeError::Validation(error)))?;
        if self.is_terminal()
            || source.prefix.session_id != self.session_id
            || source.prefix.sequence != self.next_sequence - 1
            || source.prefix.event_hash != self.last_event_hash
        {
            return Err(clipboard_error(
                "source prefix does not match the reached session",
            ));
        }
        let document = self
            .state
            .document(&source.document_id)
            .ok_or_else(|| clipboard_error("source document is missing"))?;
        self.validate_document_coherence("internal copy", document)?;
        let selection = document.selection();
        if document.path() != &source.path
            || document.version() != source.version
            || document.content_hash() != source.content_hash
            || selection.anchor_byte.min(selection.active_byte) != source.start_byte
            || selection.anchor_byte.max(selection.active_byte) != source.end_byte
        {
            return Err(clipboard_error(
                "source identity, version, hash or selection differs",
            ));
        }
        let start = usize::try_from(source.start_byte)
            .map_err(|_| clipboard_error("source range is out of bounds"))?;
        let end = usize::try_from(source.end_byte)
            .map_err(|_| clipboard_error("source range is out of bounds"))?;
        document
            .text()
            .get(start..end)
            .ok_or_else(|| clipboard_error("source range is not exact UTF-8"))
    }

    /// Returns selected historical bytes only for the exact latest copy event.
    /// Production must separately hold live authority; this accessor does not
    /// restore it after restart or grant it to a caller supplying matching text.
    pub fn clipboard_text(&self, reference: &RecordedEventRef) -> Result<&str, ReplayError> {
        self.clipboard
            .as_ref()
            .filter(|snapshot| &snapshot.reference == reference && !self.is_terminal())
            .map(|snapshot| snapshot.text.as_str())
            .ok_or_else(|| clipboard_error("required copy source is missing or mismatched"))
    }

    pub fn validate_internal_paste(&self, paste: &InternalPaste) -> Result<(), ReplayError> {
        if paste.transaction.origin != EditOrigin::Paste || paste.transaction.edits.len() != 1 {
            return Err(clipboard_error(
                "linked paste must contain one normal Paste edit",
            ));
        }
        if paste.transaction.edits[0].inserted_text != self.clipboard_text(&paste.source)? {
            return Err(clipboard_error(
                "paste insertion differs from the recorded source range",
            ));
        }
        Ok(())
    }

    fn apply_file_edited(
        &mut self,
        transaction: &rustrace_model::EditorTransaction,
    ) -> Result<(), ReplayError> {
        let document_id = transaction.document_id.clone();
        let current =
            self.state
                .documents
                .get(&document_id)
                .ok_or_else(|| ReplayError::FileLifecycle {
                    operation: "edit",
                    detail: format!("document {document_id} is not open"),
                })?;
        let reload_reconciles_external_change = if transaction.origin == EditOrigin::FileReload {
            let contents = self.state.files.get(current.path()).ok_or_else(|| {
                ReplayError::WorkspaceCoherence {
                    operation: "file reload",
                    detail: format!(
                        "open document {document_id} path {} is missing",
                        current.path()
                    ),
                }
            })?;
            contents.as_slice() != current.text().as_bytes()
        } else {
            false
        };
        if transaction.origin != EditOrigin::FileReload {
            self.validate_document_coherence("ordinary edit", current)?;
        }

        let mut document = current.clone();
        let committed = document
            .document
            .apply_transaction(transaction)
            .map_err(|source| ReplayError::Transaction {
                document_id: document_id.clone(),
                source,
            })?;
        if !committed {
            return Err(ReplayError::NoOpTransaction { document_id });
        }
        self.validate_document_limits_after(Some(&document_id), document.text().len())?;

        if reload_reconciles_external_change {
            let contents = self.state.files.get(document.path()).ok_or_else(|| {
                ReplayError::WorkspaceCoherence {
                    operation: "file reload",
                    detail: format!(
                        "open document {document_id} path {} is missing",
                        document.path()
                    ),
                }
            })?;
            if contents.as_slice() != document.text().as_bytes() {
                return Err(ReplayError::WorkspaceCoherence {
                    operation: "file reload",
                    detail: format!(
                        "document {document_id} result differs from recorded file {}",
                        document.path()
                    ),
                });
            }
            self.state.documents.insert(document_id, document);
            return Ok(());
        }

        let workspace_hash = self.state.hash_after_change(
            Some(document.path()),
            Some((document.path(), document.text().as_bytes())),
        )?;
        self.state
            .files
            .insert(document.path().clone(), document.text().as_bytes().to_vec());
        self.state.documents.insert(document_id, document);
        self.state.workspace_hash = workspace_hash;
        Ok(())
    }

    fn apply_file_created(&mut self, event: &FileCreated) -> Result<(), ReplayError> {
        if self.state.files.contains_key(&event.path) {
            return Err(ReplayError::FileLifecycle {
                operation: "create",
                detail: format!("path {} already exists", event.path),
            });
        }
        if self.state.documents.contains_key(&event.document_id) {
            return Err(ReplayError::FileLifecycle {
                operation: "create",
                detail: format!("document {} is already open", event.document_id),
            });
        }
        if let Some(document_id) = self.document_id_at_path(&event.path) {
            return Err(ReplayError::FileLifecycle {
                operation: "create",
                detail: format!("path {} belongs to open document {document_id}", event.path),
            });
        }
        let actual_hash = document_hash(&event.contents);
        if actual_hash != event.content_hash {
            return Err(ReplayError::FileLifecycle {
                operation: "create",
                detail: format!(
                    "content hash is {}; recomputed hash is {actual_hash}",
                    event.content_hash
                ),
            });
        }
        let workspace_hash = self
            .state
            .hash_after_change(None, Some((&event.path, event.contents.as_bytes())))?;
        self.validate_document_limits_after(None, event.contents.len())?;
        let document = ReplayDocument::new(
            event.document_id.clone(),
            event.contents.clone(),
            0,
            SelectionState::default(),
        )
        .map_err(|source| ReplayError::Transaction {
            document_id: event.document_id.clone(),
            source,
        })?;
        self.state
            .files
            .insert(event.path.clone(), event.contents.as_bytes().to_vec());
        self.state.documents.insert(
            event.document_id.clone(),
            DocumentState {
                path: event.path.clone(),
                document,
            },
        );
        self.state.workspace_hash = workspace_hash;
        Ok(())
    }

    fn apply_file_deleted(&mut self, event: &FileDeleted) -> Result<(), ReplayError> {
        let document = self
            .state
            .documents
            .get(&event.document_id)
            .ok_or_else(|| ReplayError::FileLifecycle {
                operation: "delete",
                detail: format!("document {} is not open", event.document_id),
            })?;
        if document.path != event.path {
            return Err(ReplayError::FileLifecycle {
                operation: "delete",
                detail: format!(
                    "document {} is at {}, not {}",
                    event.document_id, document.path, event.path
                ),
            });
        }
        let contents =
            self.state
                .files
                .get(&event.path)
                .ok_or_else(|| ReplayError::FileLifecycle {
                    operation: "delete",
                    detail: format!("path {} does not exist", event.path),
                })?;
        let text = std::str::from_utf8(contents).map_err(|_| ReplayError::FileLifecycle {
            operation: "delete",
            detail: format!("open document path {} is not UTF-8", event.path),
        })?;
        let actual_hash = document_hash(text);
        if actual_hash != event.previous_hash {
            return Err(ReplayError::FileLifecycle {
                operation: "delete",
                detail: format!(
                    "previous hash is {}; current file hash is {actual_hash}",
                    event.previous_hash
                ),
            });
        }
        let workspace_hash = self.state.hash_after_change(Some(&event.path), None)?;
        self.state.files.remove(&event.path);
        self.state.documents.remove(&event.document_id);
        if self.state.active_document.as_ref() == Some(&event.document_id) {
            self.state.active_document = None;
        }
        self.state.workspace_hash = workspace_hash;
        Ok(())
    }

    fn apply_file_renamed(&mut self, event: &FileRenamed) -> Result<(), ReplayError> {
        let document = self
            .state
            .documents
            .get(&event.document_id)
            .ok_or_else(|| ReplayError::FileLifecycle {
                operation: "rename",
                detail: format!("document {} is not open", event.document_id),
            })?;
        if document.path != event.old_path {
            return Err(ReplayError::FileLifecycle {
                operation: "rename",
                detail: format!(
                    "document {} is at {}, not {}",
                    event.document_id, document.path, event.old_path
                ),
            });
        }
        let contents =
            self.state
                .files
                .get(&event.old_path)
                .ok_or_else(|| ReplayError::FileLifecycle {
                    operation: "rename",
                    detail: format!("source path {} does not exist", event.old_path),
                })?;
        if self.state.files.contains_key(&event.new_path) {
            return Err(ReplayError::FileLifecycle {
                operation: "rename",
                detail: format!("destination path {} already exists", event.new_path),
            });
        }
        if let Some(document_id) = self.document_id_at_path(&event.new_path) {
            return Err(ReplayError::FileLifecycle {
                operation: "rename",
                detail: format!(
                    "destination path {} belongs to open document {document_id}",
                    event.new_path
                ),
            });
        }
        let workspace_hash = self
            .state
            .hash_after_change(Some(&event.old_path), Some((&event.new_path, contents)))?;
        let contents =
            self.state
                .files
                .remove(&event.old_path)
                .ok_or_else(|| ReplayError::FileLifecycle {
                    operation: "rename",
                    detail: format!("source path {} disappeared", event.old_path),
                })?;
        self.state.files.insert(event.new_path.clone(), contents);
        let document = self
            .state
            .documents
            .get_mut(&event.document_id)
            .ok_or_else(|| ReplayError::FileLifecycle {
                operation: "rename",
                detail: format!("document {} disappeared", event.document_id),
            })?;
        document.path = event.new_path.clone();
        self.state.workspace_hash = workspace_hash;
        Ok(())
    }

    fn apply_external_file_change(
        &mut self,
        event: &ExternalFileChange,
    ) -> Result<(), ReplayError> {
        validate_external_hash_pair(
            &event.path,
            "previous",
            event.previous_contents.as_deref(),
            event.previous_hash,
        )?;
        validate_external_hash_pair(
            &event.path,
            "new",
            event.new_contents.as_deref(),
            event.new_hash,
        )?;

        match event.previous_contents.as_deref() {
            Some(expected) => match self.state.files.get(&event.path) {
                Some(actual) if actual.as_slice() == expected.as_bytes() => {}
                Some(_) => {
                    return Err(ReplayError::ExternalFileChange {
                        path: event.path.clone(),
                        detail: "previous contents do not match the current file".to_owned(),
                    });
                }
                None => {
                    return Err(ReplayError::ExternalFileChange {
                        path: event.path.clone(),
                        detail: "previous contents were supplied for a missing file".to_owned(),
                    });
                }
            },
            None if self.state.files.contains_key(&event.path) => {
                return Err(ReplayError::ExternalFileChange {
                    path: event.path.clone(),
                    detail: "create transition targets an existing file".to_owned(),
                });
            }
            None => {}
        }

        if event.previous_contents == event.new_contents {
            return Err(ReplayError::ExternalFileChange {
                path: event.path.clone(),
                detail: "event does not change file contents or existence".to_owned(),
            });
        }

        let new_contents = event.new_contents.as_deref().map(str::as_bytes);
        let workspace_hash = self.state.hash_after_change(
            event.previous_contents.as_ref().map(|_| &event.path),
            new_contents.map(|contents| (&event.path, contents)),
        )?;
        match new_contents {
            Some(contents) => {
                self.state
                    .files
                    .insert(event.path.clone(), contents.to_vec());
            }
            None => {
                self.state.files.remove(&event.path);
            }
        }
        self.state.workspace_hash = workspace_hash;
        Ok(())
    }

    fn validate_completion_request(&self, event: &CompletionRequested) -> Result<(), ReplayError> {
        let document = self.require_document(&event.document_id, "lsp_completion_requested")?;
        validate_document_version("lsp_completion_requested", document, event.document_version)?;
        validate_offset(
            "lsp_completion_requested",
            document.text(),
            event.position_byte,
        )
    }

    fn validate_completion_acceptance(
        &self,
        event: &CompletionAccepted,
    ) -> Result<(), ReplayError> {
        let document = self.require_document(&event.document_id, "lsp_completion_accepted")?;
        validate_document_version("lsp_completion_accepted", document, event.document_version)?;
        validate_prospective_edit(
            "lsp_completion_accepted",
            document.text(),
            &event.primary_edit,
        )?;
        for edit in &event.additional_edits {
            validate_prospective_edit("lsp_completion_accepted", document.text(), edit)?;
        }
        Ok(())
    }

    fn verify_checkpoint_event(&self, event: &WorkspaceCheckpoint) -> Result<(), ReplayError> {
        self.verify_state_hash("workspace_hash", event.workspace_hash)?;
        for document in self.state.documents.values() {
            let Some(contents) = self.state.files.get(document.path()) else {
                return Err(ReplayError::CheckpointStateMismatch {
                    field: "documents.path",
                    detail: format!(
                        "open document {} path {} is missing",
                        document.document_id(),
                        document.path()
                    ),
                });
            };
            if contents.as_slice() != document.text().as_bytes() {
                return Err(ReplayError::CheckpointStateMismatch {
                    field: "documents.path",
                    detail: format!(
                        "open document {} differs from workspace path {}",
                        document.document_id(),
                        document.path()
                    ),
                });
            }
        }
        let documents: Vec<_> = self
            .state
            .documents
            .iter()
            .map(|(document_id, document)| DocumentHash {
                document_id: document_id.clone(),
                hash: document.content_hash(),
            })
            .collect();
        if event.documents != documents {
            return Err(ReplayError::CheckpointStateMismatch {
                field: "documents",
                detail: "document IDs or content hashes differ from replay state".to_owned(),
            });
        }
        Ok(())
    }

    fn verify_state_hash(&self, field: &'static str, expected: Hash) -> Result<(), ReplayError> {
        if expected == self.state.workspace_hash {
            Ok(())
        } else {
            Err(ReplayError::CheckpointStateMismatch {
                field,
                detail: format!(
                    "recorded hash is {expected}; replay workspace hash is {}",
                    self.state.workspace_hash
                ),
            })
        }
    }

    fn require_document(
        &self,
        document_id: &DocumentId,
        event: &'static str,
    ) -> Result<&DocumentState, ReplayError> {
        self.state
            .documents
            .get(document_id)
            .ok_or_else(|| ReplayError::NonMutatingEvent {
                event,
                detail: format!("document {document_id} is not open"),
            })
    }

    fn validate_document_coherence(
        &self,
        operation: &'static str,
        document: &DocumentState,
    ) -> Result<(), ReplayError> {
        let Some(contents) = self.state.files.get(document.path()) else {
            return Err(ReplayError::WorkspaceCoherence {
                operation,
                detail: format!(
                    "open document {} path {} is missing",
                    document.document_id(),
                    document.path()
                ),
            });
        };
        if contents.as_slice() != document.text().as_bytes() {
            return Err(ReplayError::WorkspaceCoherence {
                operation,
                detail: format!(
                    "open document {} differs from recorded file {}",
                    document.document_id(),
                    document.path()
                ),
            });
        }
        Ok(())
    }

    fn validate_workspace_coherence(&self, operation: &'static str) -> Result<(), ReplayError> {
        if let Some(active_document) = &self.state.active_document
            && !self.state.documents.contains_key(active_document)
        {
            return Err(ReplayError::WorkspaceCoherence {
                operation,
                detail: format!("active document {active_document} is not open"),
            });
        }
        for document in self.state.documents.values() {
            self.validate_document_coherence(operation, document)?;
        }
        Ok(())
    }

    fn require_active_cargo_command(
        &self,
        event: &'static str,
        command_id: &CommandId,
    ) -> Result<(), ReplayError> {
        if self.active_cargo_commands.contains(command_id) {
            Ok(())
        } else {
            Err(ReplayError::CargoCommandNotActive {
                event,
                command_id: command_id.clone(),
            })
        }
    }

    fn validate_no_active_cargo_commands(&self, event: &'static str) -> Result<(), ReplayError> {
        if self.active_cargo_commands.is_empty() {
            Ok(())
        } else {
            Err(ReplayError::ActiveCargoCommandsAtTerminal {
                event,
                count: self.active_cargo_commands.len(),
            })
        }
    }

    fn document_id_at_path(&self, path: &WorkspacePath) -> Option<&DocumentId> {
        self.state
            .documents
            .iter()
            .find_map(|(document_id, document)| (document.path() == path).then_some(document_id))
    }

    fn validate_document_limits_after(
        &self,
        replaced_document: Option<&DocumentId>,
        new_text_bytes: usize,
    ) -> Result<(), ReplayError> {
        let replacing_existing = replaced_document
            .is_some_and(|document_id| self.state.documents.contains_key(document_id));
        let attempted_count = self
            .state
            .documents
            .len()
            .saturating_add(usize::from(!replacing_existing));
        if attempted_count > MAX_CHECKPOINT_DOCUMENTS {
            return Err(ReplayError::OpenDocumentCountLimit {
                attempted: attempted_count,
                maximum: MAX_CHECKPOINT_DOCUMENTS,
            });
        }
        let retained_bytes = self
            .state
            .documents
            .iter()
            .filter(|(document_id, _)| replaced_document != Some(*document_id))
            .try_fold(0usize, |total, (_, document)| {
                total.checked_add(document.text().len())
            })
            .unwrap_or(usize::MAX);
        let attempted_bytes = retained_bytes.saturating_add(new_text_bytes);
        if attempted_bytes > MAX_CHECKPOINT_TOTAL_FILE_BYTES {
            return Err(ReplayError::OpenDocumentBytesLimit {
                attempted: attempted_bytes,
                maximum: MAX_CHECKPOINT_TOTAL_FILE_BYTES,
            });
        }
        Ok(())
    }
}

fn validate_stored_checkpoint(
    checkpoint: &StoredCheckpoint,
) -> Result<WorkspaceState, ReplayError> {
    checkpoint
        .snapshot
        .verify_owner(&checkpoint.owning_event)
        .map_err(ReplayError::Checkpoint)?;
    encode_envelope(&checkpoint.owning_event).map_err(ReplayError::EventEncoding)?;
    let expected_owner_hash = compute_event_hash(
        checkpoint.owning_event.previous_event_hash,
        &checkpoint.owning_event,
    )
    .map_err(ReplayError::EventEncoding)?;
    if checkpoint.owning_event.event_hash != expected_owner_hash {
        return Err(ReplayError::EventHashMismatch {
            sequence: checkpoint.owning_event.sequence,
            expected: expected_owner_hash,
            actual: checkpoint.owning_event.event_hash,
        });
    }
    restore_workspace(&checkpoint.snapshot)
}

fn restore_workspace(snapshot: &CheckpointSnapshot) -> Result<WorkspaceState, ReplayError> {
    let files: BTreeMap<_, _> = snapshot
        .files()
        .iter()
        .map(|file| (file.path.clone(), file.contents.clone()))
        .collect();
    let mut documents = BTreeMap::new();
    for checkpoint_document in snapshot.documents() {
        let contents = files.get(&checkpoint_document.path).ok_or_else(|| {
            ReplayError::CheckpointStateMismatch {
                field: "documents.path",
                detail: format!(
                    "document {} references missing path {}",
                    checkpoint_document.document_id, checkpoint_document.path
                ),
            }
        })?;
        let text =
            std::str::from_utf8(contents).map_err(|_| ReplayError::CheckpointStateMismatch {
                field: "documents.path",
                detail: format!(
                    "document {} path {} is not UTF-8",
                    checkpoint_document.document_id, checkpoint_document.path
                ),
            })?;
        let document = ReplayDocument::new(
            checkpoint_document.document_id.clone(),
            text.to_owned(),
            checkpoint_document.version,
            checkpoint_document.selection,
        )
        .map_err(|source| ReplayError::Transaction {
            document_id: checkpoint_document.document_id.clone(),
            source,
        })?;
        documents.insert(
            checkpoint_document.document_id.clone(),
            DocumentState {
                path: checkpoint_document.path.clone(),
                document,
            },
        );
    }
    Ok(WorkspaceState {
        files,
        documents,
        active_document: snapshot.active_document().cloned(),
        workspace_hash: snapshot.workspace_hash(),
    })
}

fn validate_external_hash_pair(
    path: &WorkspacePath,
    side: &'static str,
    contents: Option<&str>,
    hash: Option<Hash>,
) -> Result<(), ReplayError> {
    match (contents, hash) {
        (None, None) => Ok(()),
        (Some(contents), Some(hash)) => {
            let actual = document_hash(contents);
            if actual == hash {
                Ok(())
            } else {
                Err(ReplayError::ExternalFileChange {
                    path: path.clone(),
                    detail: format!("{side} hash is {hash}; recomputed hash is {actual}"),
                })
            }
        }
        _ => Err(ReplayError::ExternalFileChange {
            path: path.clone(),
            detail: format!("{side} contents and hash must either both exist or both be absent"),
        }),
    }
}

fn validate_document_version(
    event: &'static str,
    document: &DocumentState,
    actual: u64,
) -> Result<(), ReplayError> {
    if actual == document.version() {
        Ok(())
    } else {
        Err(ReplayError::NonMutatingEvent {
            event,
            detail: format!(
                "document {} version is {actual}; current version is {}",
                document.document_id(),
                document.version()
            ),
        })
    }
}

fn validate_prospective_edit(
    event: &'static str,
    text: &str,
    edit: &TextEdit,
) -> Result<(), ReplayError> {
    validate_offset(event, text, edit.start_byte)?;
    validate_offset(event, text, edit.end_byte)
}

fn validate_offset(event: &'static str, text: &str, offset: u64) -> Result<(), ReplayError> {
    let offset = usize::try_from(offset).map_err(|_| ReplayError::NonMutatingEvent {
        event,
        detail: "byte offset does not fit this platform".to_owned(),
    })?;
    if offset > text.len() {
        return Err(ReplayError::NonMutatingEvent {
            event,
            detail: format!(
                "byte offset {offset} exceeds document length {}",
                text.len()
            ),
        });
    }
    if !text.is_char_boundary(offset) {
        return Err(ReplayError::NonMutatingEvent {
            event,
            detail: format!("byte offset {offset} splits a UTF-8 code point"),
        });
    }
    Ok(())
}

#[derive(Debug)]
pub enum ReplayError {
    Checkpoint(CheckpointError),
    EventEncoding(EncodeError),
    InitialCheckpointNotGenesis {
        sequence: u64,
    },
    CheckpointCertification {
        detail: String,
    },
    WrongSession {
        expected: SessionId,
        actual: SessionId,
    },
    UnexpectedSequence {
        expected: u64,
        actual: u64,
    },
    SequenceOutOfRange {
        actual: u64,
        maximum: u64,
    },
    PreviousEventHashMismatch {
        sequence: u64,
        expected: Hash,
        actual: Hash,
    },
    EventHashMismatch {
        sequence: u64,
        expected: Hash,
        actual: Hash,
    },
    EventAfterTerminal {
        sequence: u64,
    },
    SessionLifecycle {
        event: &'static str,
        detail: String,
    },
    FileLifecycle {
        operation: &'static str,
        detail: String,
    },
    Transaction {
        document_id: DocumentId,
        source: TransactionError,
    },
    NoOpTransaction {
        document_id: DocumentId,
    },
    CheckpointStateMismatch {
        field: &'static str,
        detail: String,
    },
    FinalWorkspaceHashMismatch {
        expected: Hash,
        actual: Hash,
    },
    WorkspaceLimit {
        source: WorkspaceHashError,
    },
    OpenDocumentCountLimit {
        attempted: usize,
        maximum: usize,
    },
    OpenDocumentBytesLimit {
        attempted: usize,
        maximum: usize,
    },
    ExternalFileChange {
        path: WorkspacePath,
        detail: String,
    },
    WorkspaceCoherence {
        operation: &'static str,
        detail: String,
    },
    CargoCommandAlreadyActive {
        command_id: CommandId,
    },
    CargoCommandNotActive {
        event: &'static str,
        command_id: CommandId,
    },
    ActiveCargoCommandLimit {
        attempted: usize,
        maximum: usize,
    },
    ActiveCargoCommandsAtTerminal {
        event: &'static str,
        count: usize,
    },
    NonMutatingEvent {
        event: &'static str,
        detail: String,
    },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Checkpoint(source) => write!(formatter, "invalid replay checkpoint: {source}"),
            Self::EventEncoding(source) => write!(formatter, "invalid replay event: {source}"),
            Self::InitialCheckpointNotGenesis { sequence } => write!(
                formatter,
                "initial replay checkpoint owns sequence {sequence}; expected genesis sequence 1"
            ),
            Self::CheckpointCertification { detail } => {
                write!(formatter, "checkpoint cannot be certified: {detail}")
            }
            Self::WrongSession { expected, actual } => {
                write!(formatter, "event session is {actual}; expected {expected}")
            }
            Self::UnexpectedSequence { expected, actual } => {
                write!(formatter, "event sequence is {actual}; expected {expected}")
            }
            Self::SequenceOutOfRange { actual, maximum } => {
                write!(
                    formatter,
                    "event sequence is {actual}; maximum is {maximum}"
                )
            }
            Self::PreviousEventHashMismatch {
                sequence,
                expected,
                actual,
            } => write!(
                formatter,
                "event {sequence} previous hash is {actual}; expected {expected}"
            ),
            Self::EventHashMismatch {
                sequence,
                expected,
                actual,
            } => write!(
                formatter,
                "event {sequence} hash is {actual}; recomputed hash is {expected}"
            ),
            Self::EventAfterTerminal { sequence } => {
                write!(formatter, "event {sequence} follows a terminal event")
            }
            Self::SessionLifecycle { event, detail } => {
                write!(formatter, "invalid {event} lifecycle event: {detail}")
            }
            Self::FileLifecycle { operation, detail } => {
                write!(formatter, "cannot {operation} replay file: {detail}")
            }
            Self::Transaction {
                document_id,
                source,
            } => write!(
                formatter,
                "document {document_id} transaction failed replay: {source}"
            ),
            Self::NoOpTransaction { document_id } => {
                write!(formatter, "document {document_id} transaction is a no-op")
            }
            Self::CheckpointStateMismatch { field, detail } => {
                write!(formatter, "checkpoint {field} mismatch: {detail}")
            }
            Self::FinalWorkspaceHashMismatch { expected, actual } => write!(
                formatter,
                "final workspace hash is {actual}; expected {expected}"
            ),
            Self::WorkspaceLimit { source } => {
                write!(
                    formatter,
                    "replay workspace exceeds a fixed limit: {source}"
                )
            }
            Self::OpenDocumentCountLimit { attempted, maximum } => write!(
                formatter,
                "replay would retain {attempted} open documents; maximum is {maximum}"
            ),
            Self::OpenDocumentBytesLimit { attempted, maximum } => write!(
                formatter,
                "replay open documents would retain {attempted} bytes; maximum is {maximum} bytes"
            ),
            Self::ExternalFileChange { path, detail } => {
                write!(formatter, "invalid external change for {path}: {detail}")
            }
            Self::WorkspaceCoherence { operation, detail } => {
                write!(formatter, "cannot accept {operation}: {detail}")
            }
            Self::CargoCommandAlreadyActive { command_id } => {
                write!(formatter, "Cargo command {command_id} is already active")
            }
            Self::CargoCommandNotActive { event, command_id } => {
                write!(
                    formatter,
                    "{event} references inactive Cargo command {command_id}"
                )
            }
            Self::ActiveCargoCommandLimit { attempted, maximum } => write!(
                formatter,
                "replay would retain {attempted} active Cargo commands; maximum is {maximum}"
            ),
            Self::ActiveCargoCommandsAtTerminal { event, count } => write!(
                formatter,
                "{event} leaves {count} active Cargo commands without finish events"
            ),
            Self::NonMutatingEvent { event, detail } => {
                write!(formatter, "invalid {event} event: {detail}")
            }
        }
    }
}

impl Error for ReplayError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Checkpoint(source) => Some(source),
            Self::EventEncoding(source) => Some(source),
            Self::Transaction { source, .. } => Some(source),
            Self::WorkspaceLimit { source } => Some(source),
            Self::InitialCheckpointNotGenesis { .. }
            | Self::CheckpointCertification { .. }
            | Self::WrongSession { .. }
            | Self::UnexpectedSequence { .. }
            | Self::SequenceOutOfRange { .. }
            | Self::PreviousEventHashMismatch { .. }
            | Self::EventHashMismatch { .. }
            | Self::EventAfterTerminal { .. }
            | Self::SessionLifecycle { .. }
            | Self::FileLifecycle { .. }
            | Self::NoOpTransaction { .. }
            | Self::CheckpointStateMismatch { .. }
            | Self::FinalWorkspaceHashMismatch { .. }
            | Self::OpenDocumentCountLimit { .. }
            | Self::OpenDocumentBytesLimit { .. }
            | Self::ExternalFileChange { .. }
            | Self::WorkspaceCoherence { .. }
            | Self::CargoCommandAlreadyActive { .. }
            | Self::CargoCommandNotActive { .. }
            | Self::ActiveCargoCommandLimit { .. }
            | Self::ActiveCargoCommandsAtTerminal { .. }
            | Self::NonMutatingEvent { .. } => None,
        }
    }
}
