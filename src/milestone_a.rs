//! Temporary one-file editor-to-journal-to-replay vertical slice.
//!
//! This module deliberately keeps a single `main.rs` open. Content mutations
//! go through [`EditorBuffer`], its production effect callback captures the
//! committed transaction, and the exact transaction is acknowledged by the
//! production journal writer before an editing method reports success.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use rustrace_editor::{
    EditorBuffer, EditorTransaction, Movement, ReplayDocument, TransactionError,
};
use rustrace_journal::{
    CheckpointFile, CheckpointInput, CheckpointSnapshot, CheckpointSubmission, EventSubmission,
    Journal, JournalError, JournalWriteCompletion, JournalWriteJob, JournalWriteKind,
    JournalWriter, JournalWriterError, JournalWriterStartError, MAX_CHECKPOINTS_PER_READ,
    OpenDocument, StoredCheckpoint,
};
use rustrace_model::{
    DocumentHash, DocumentId, EditOrigin, Event, EventEnvelope, FORMAT_VERSION_V1, Hash,
    SelectionChanged, SelectionState, SessionId, SubmissionFinalized, WorkspaceCheckpoint,
    WorkspacePath, document_hash, encode_envelope,
};
use rustrace_replay::{DocumentState, ReplayEngine, ReplayError, WorkspaceState};
use rustrace_workspace::hash::{MAX_WORKSPACE_FILE_BYTES, WorkspaceHashError, hash_entries};

pub const SOURCE_FILE_NAME: &str = "main.rs";
pub const JOURNAL_FILE_NAME: &str = "session.sqlite3";
const DOCUMENT_ID: &str = "main.rs";
const WRITER_CAPACITY: usize = 1;
const RESERVED_TERMINAL_EVENTS: u64 = 2;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

/// Maximum number of states/events retained by the temporary milestone slice.
pub const MAX_MILESTONE_EVENTS: u64 = 256;

/// Maximum conservative allocation charge for all retained workspace states.
pub const MAX_REPLAY_RETAINED_STATE_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum conservative allocation charge for decoded envelopes and the event
/// copies retained in replay steps.
pub const MAX_REPLAY_RETAINED_EVENT_BYTES: u64 = 32 * 1024 * 1024;

pub type OneFileEditor = EditorBuffer<Box<dyn FnMut(&EditorTransaction)>>;
#[cfg(test)]
type AfterMutationHook = Box<dyn FnMut(&Path)>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct RecordingFailure {
    operation: &'static str,
    detail: String,
}

#[derive(Clone, Copy)]
struct TerminalEventBudget {
    checkpoint: u64,
    finalization: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl RecordingFailure {
    fn into_error(self) -> MilestoneError {
        MilestoneError::RecordingFailed {
            operation: self.operation,
            detail: self.detail,
        }
    }
}

struct StartupArtifacts {
    files: Vec<(PathBuf, FileIdentity)>,
    armed: bool,
}

impl StartupArtifacts {
    fn new() -> Self {
        Self {
            files: Vec::with_capacity(2),
            armed: true,
        }
    }

    fn track(&mut self, path: PathBuf, identity: FileIdentity) {
        self.files.push((path, identity));
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StartupArtifacts {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for (path, expected) in self.files.iter().rev() {
            let Ok(metadata) = fs::symlink_metadata(path) else {
                continue;
            };
            if metadata.file_type().is_file()
                && metadata_identity(&metadata).is_ok_and(|actual| actual == *expected)
            {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// An active, durable one-file editing session.
///
/// Mutable access to the editor is intentionally not exposed. That keeps every
/// content mutation inside the transaction capture and journal acknowledgement
/// boundary.
pub struct OneFileSession {
    directory: PathBuf,
    directory_identity: FileIdentity,
    source_path: PathBuf,
    source_identity: FileIdentity,
    journal_path: PathBuf,
    journal_identity: FileIdentity,
    workspace_path: WorkspacePath,
    document_id: DocumentId,
    session_id: SessionId,
    editor: OneFileEditor,
    captured_transactions: Rc<RefCell<VecDeque<EditorTransaction>>>,
    writer: Option<JournalWriter>,
    persisted_event_count: u64,
    retained_state_bytes: u64,
    retained_event_bytes: u64,
    next_monotonic_millis: u64,
    failure: Option<RecordingFailure>,
    #[cfg(test)]
    after_mutation_hook: Option<AfterMutationHook>,
}

impl OneFileSession {
    /// Creates a fresh one-file workspace and persists its genesis checkpoint.
    pub fn start(
        directory: impl AsRef<Path>,
        session_id: SessionId,
        starter_text: &str,
    ) -> Result<Self, MilestoneError> {
        ensure_supported_platform()?;
        require_limit(
            "source bytes",
            usize_to_u64(starter_text.len(), "starter source length")?,
            MAX_WORKSPACE_FILE_BYTES,
        )?;
        let retained_state_bytes = one_file_state_retained_charge(starter_text.len())?;

        let directory = resolve_input_directory_with(directory.as_ref(), std::env::current_dir)?;
        fs::create_dir_all(&directory).map_err(|source| MilestoneError::Io {
            operation: "create milestone directory",
            source,
        })?;
        let directory = fs::canonicalize(&directory).map_err(|source| MilestoneError::Io {
            operation: "anchor milestone directory",
            source,
        })?;
        let directory_identity = require_directory_identity(&directory, "milestone directory")?;
        let source_path = directory.join(SOURCE_FILE_NAME);
        let journal_path = directory.join(JOURNAL_FILE_NAME);
        let source_identity = write_new_synced(&source_path, starter_text.as_bytes())?;
        let mut created = StartupArtifacts::new();
        created.track(source_path.clone(), source_identity);

        let workspace_path = WorkspacePath::new(SOURCE_FILE_NAME)
            .expect("the fixed one-file workspace path is valid");
        let document_id =
            DocumentId::new(DOCUMENT_ID).expect("the fixed one-file document ID is valid");
        let captured_transactions = Rc::new(RefCell::new(VecDeque::new()));
        let captured_by_effect = Rc::clone(&captured_transactions);
        let effects: Box<dyn FnMut(&EditorTransaction)> = Box::new(move |transaction| {
            captured_by_effect
                .borrow_mut()
                .push_back(transaction.clone());
        });
        let editor = EditorBuffer::new(document_id.clone(), starter_text, effects);

        let mut journal = create_new_journal_no_follow(&journal_path)?;
        let journal_identity = require_regular_identity(&journal_path, "journal")?;
        created.track(journal_path.clone(), journal_identity);
        journal
            .create_or_resume_session(&session_id)
            .map_err(MilestoneError::Journal)?;
        let writer =
            JournalWriter::spawn(WRITER_CAPACITY, journal).map_err(MilestoneError::WriterStart)?;
        let genesis = CheckpointInput {
            session_id: session_id.clone(),
            files: vec![CheckpointFile {
                path: workspace_path.clone(),
                contents: starter_text.as_bytes().to_vec(),
            }],
            active_document: Some(document_id.clone()),
            documents: vec![OpenDocument {
                document_id: document_id.clone(),
                path: workspace_path.clone(),
                selection: SelectionState::caret(0),
                version: 0,
            }],
        };
        let retained_event_bytes = match CheckpointSnapshot::from_input(genesis.clone(), 1)
            .map_err(|error| MilestoneError::Invariant {
                detail: format!("could not construct genesis budget event: {error}"),
            })
            .and_then(|snapshot| {
                event_retained_charge_for_submission(
                    &session_id,
                    1,
                    0,
                    &Event::WorkspaceCheckpoint(snapshot.event_payload()),
                )
            })
            .and_then(|charge| {
                let terminal = terminal_event_retained_budget(
                    &session_id,
                    &document_id,
                    1,
                    1,
                    Hash::zero(),
                    Hash::zero(),
                )?;
                let attempted = charge
                    .checked_add(terminal.checkpoint)
                    .and_then(|value| value.checked_add(terminal.finalization))
                    .ok_or_else(event_budget_overflow)?;
                require_limit(
                    "replay retained event bytes",
                    attempted,
                    MAX_REPLAY_RETAINED_EVENT_BYTES,
                )?;
                Ok(charge)
            }) {
            Ok(charge) => charge,
            Err(error) => {
                let _ = writer.shutdown();
                return Err(error);
            }
        };
        let submission = CheckpointSubmission {
            monotonic_millis: 0,
            wall_clock_utc: None,
            input: genesis,
        };
        if let Err(failure) = wait_for_persistence(
            &writer,
            JournalWriteJob::Checkpoint(submission),
            JournalWriteKind::Checkpoint,
            1,
            "persist genesis checkpoint",
        ) {
            let _ = writer.shutdown();
            return Err(failure.into_error());
        }
        if let Err(error) = verify_workspace_identities(
            &directory,
            directory_identity,
            &source_path,
            source_identity,
            &journal_path,
            journal_identity,
        ) {
            let _ = writer.shutdown();
            return Err(error);
        }

        created.disarm();

        Ok(Self {
            directory,
            directory_identity,
            source_path,
            source_identity,
            journal_path,
            journal_identity,
            workspace_path,
            document_id,
            session_id,
            editor,
            captured_transactions,
            writer: Some(writer),
            persisted_event_count: 1,
            retained_state_bytes,
            retained_event_bytes,
            next_monotonic_millis: 1,
            failure: None,
            #[cfg(test)]
            after_mutation_hook: None,
        })
    }

    pub fn editor(&self) -> &OneFileEditor {
        &self.editor
    }

    pub fn text(&self) -> String {
        self.editor.text()
    }

    pub fn version(&self) -> u64 {
        self.editor.version()
    }

    pub fn selection(&self) -> SelectionState {
        self.editor.selection_state()
    }

    pub fn document_hash(&self) -> Hash {
        self.editor.hash()
    }

    pub fn persisted_event_count(&self) -> u64 {
        self.persisted_event_count
    }

    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    /// Applies one production editor movement and records the changed
    /// selection as one replayable event.
    pub fn move_cursor(
        &mut self,
        movement: Movement,
        selecting: bool,
    ) -> Result<bool, MilestoneError> {
        self.ensure_healthy()?;
        let before = self.editor.selection_state();
        let after = self.editor.selection_after_movement(movement, selecting);
        if before == after {
            return Ok(false);
        }
        let event = self.selection_event(after);
        self.preflight_recorded_state(self.editor.text().len(), &event)?;
        self.editor.move_cursor(movement, selecting);
        self.after_mutation_boundary();
        let result = if self.editor.selection_state() == after {
            self.persist_selection_change(before, "persist cursor movement")
        } else {
            Err(MilestoneError::Invariant {
                detail: "movement preview differed from the committed selection".to_owned(),
            })
        };
        self.finish_mutated_operation("persist cursor movement", result)
    }

    /// Selects the complete document as one logical selection action.
    pub fn select_all(&mut self) -> Result<bool, MilestoneError> {
        self.ensure_healthy()?;
        let before = self.editor.selection_state();
        let end =
            u64::try_from(self.editor.text().len()).map_err(|_| MilestoneError::Invariant {
                detail: "one-file text length does not fit a persisted byte offset".to_owned(),
            })?;
        if before == SelectionState::new(0, end) {
            return Ok(false);
        }
        let after = SelectionState::new(0, end);
        let event = self.selection_event(after);
        self.preflight_recorded_state(
            usize::try_from(end).map_err(|_| MilestoneError::Invariant {
                detail: "one-file text length does not fit usize".to_owned(),
            })?,
            &event,
        )?;
        self.editor
            .set_selection(after)
            .map_err(MilestoneError::Editor)?;
        self.after_mutation_boundary();
        let result = self.persist_selection_change(before, "persist select-all");
        self.finish_mutated_operation("persist select-all", result)
    }

    pub fn insert_char(&mut self, character: char) -> Result<bool, MilestoneError> {
        self.ensure_healthy()?;
        let transaction = self
            .editor
            .preview_insert_char(character)
            .map_err(MilestoneError::Editor)?;
        self.apply_editor_edit("persist typed character", transaction)
    }

    pub fn paste(&mut self, text: &str) -> Result<bool, MilestoneError> {
        self.ensure_healthy()?;
        let transaction = self
            .editor
            .preview_paste(text)
            .map_err(MilestoneError::Editor)?;
        self.apply_editor_edit("persist paste", transaction)
    }

    pub fn delete_backward(&mut self) -> Result<bool, MilestoneError> {
        self.ensure_healthy()?;
        let transaction = self
            .editor
            .preview_delete_backward()
            .map_err(MilestoneError::Editor)?;
        self.apply_editor_edit("persist backward delete", transaction)
    }

    pub fn delete_forward(&mut self) -> Result<bool, MilestoneError> {
        self.ensure_healthy()?;
        let transaction = self
            .editor
            .preview_delete_forward()
            .map_err(MilestoneError::Editor)?;
        self.apply_editor_edit("persist forward delete", transaction)
    }

    pub fn undo(&mut self) -> Result<bool, MilestoneError> {
        self.ensure_healthy()?;
        let transaction = self.editor.preview_undo().map_err(MilestoneError::Editor)?;
        self.apply_editor_edit("persist undo", transaction)
    }

    pub fn redo(&mut self) -> Result<bool, MilestoneError> {
        self.ensure_healthy()?;
        let transaction = self.editor.preview_redo().map_err(MilestoneError::Editor)?;
        self.apply_editor_edit("persist redo", transaction)
    }

    /// Persists final source bytes, a complete final checkpoint, and the clean
    /// terminal event, then drains the writer and ends the SQLite session.
    pub fn finish(mut self) -> Result<FinalizedSession, MilestoneError> {
        let result = self.try_finish();
        if result.is_err() {
            self.shutdown_ignoring_error();
        }
        result
    }

    fn try_finish(&mut self) -> Result<FinalizedSession, MilestoneError> {
        if let Some(failure) = self.failure.clone() {
            return Err(failure.into_error());
        }
        self.require_no_captured_transactions()?;

        let final_text = self.editor.text();
        let final_document_hash = self.editor.hash();
        require_equal(
            "final editor document hash",
            final_document_hash,
            document_hash(&final_text),
        )?;
        let final_workspace_hash = hash_entries([(&self.workspace_path, final_text.as_bytes())])
            .map_err(MilestoneError::WorkspaceHash)?;
        let terminal_budget = self.preflight_finalization(
            final_text.len(),
            final_document_hash,
            final_workspace_hash,
        )?;
        let final_retained_event_bytes = self
            .retained_event_bytes
            .checked_add(terminal_budget.checkpoint)
            .and_then(|value| value.checked_add(terminal_budget.finalization))
            .ok_or_else(event_budget_overflow)?;
        self.source_identity = atomic_replace_synced(
            &self.directory,
            self.directory_identity,
            &self.source_path,
            self.source_identity,
            final_text.as_bytes(),
        )?;
        let final_checkpoint = CheckpointInput {
            session_id: self.session_id.clone(),
            files: vec![CheckpointFile {
                path: self.workspace_path.clone(),
                contents: final_text.as_bytes().to_vec(),
            }],
            active_document: Some(self.document_id.clone()),
            documents: vec![OpenDocument {
                document_id: self.document_id.clone(),
                path: self.workspace_path.clone(),
                selection: self.editor.selection_state(),
                version: self.editor.version(),
            }],
        };
        let checkpoint_submission = CheckpointSubmission {
            monotonic_millis: self.take_monotonic_millis(),
            wall_clock_utc: None,
            input: final_checkpoint,
        };
        self.persist_job(
            JournalWriteJob::Checkpoint(checkpoint_submission),
            JournalWriteKind::Checkpoint,
            "persist final checkpoint",
        )?;
        self.retained_event_bytes = checked_add_limit(
            "replay retained event bytes",
            self.retained_event_bytes,
            terminal_budget.checkpoint,
            MAX_REPLAY_RETAINED_EVENT_BYTES,
        )?;
        self.retained_state_bytes = checked_add_limit(
            "replay retained state bytes",
            self.retained_state_bytes,
            one_file_state_retained_charge(final_text.len())?,
            MAX_REPLAY_RETAINED_STATE_BYTES,
        )?;

        let final_sequence =
            self.persisted_event_count
                .checked_add(1)
                .ok_or_else(|| MilestoneError::Invariant {
                    detail: "final event sequence overflowed u64".to_owned(),
                })?;
        let finalization = Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash,
            event_count: final_sequence,
            clean: true,
            warnings: Vec::new(),
        });
        let final_event_hash = self.persist_event(finalization, "persist finalization")?;
        require_equal(
            "preflighted terminal event bytes",
            self.retained_event_bytes,
            final_retained_event_bytes,
        )?;
        self.retained_state_bytes = checked_add_limit(
            "replay retained state bytes",
            self.retained_state_bytes,
            one_file_state_retained_charge(final_text.len())?,
            MAX_REPLAY_RETAINED_STATE_BYTES,
        )?;
        if self.persisted_event_count != final_sequence {
            return Err(MilestoneError::Invariant {
                detail: "finalization acknowledgement used an unexpected sequence".to_owned(),
            });
        }

        let writer = self
            .writer
            .take()
            .ok_or_else(|| MilestoneError::Invariant {
                detail: "journal writer is unavailable at orderly shutdown".to_owned(),
            })?;
        writer.shutdown().map_err(MilestoneError::Writer)?;

        // This is deliberately a fresh file-backed connection after the
        // writer has drained and closed its connection.
        verify_directory_identity(
            &self.directory,
            self.directory_identity,
            "milestone directory",
        )?;
        verify_regular_identity(&self.journal_path, self.journal_identity, "journal")?;
        let mut journal =
            Journal::open_no_follow(&self.journal_path).map_err(MilestoneError::Journal)?;
        verify_regular_identity(&self.journal_path, self.journal_identity, "journal")?;
        let ended = journal
            .end_session(&self.session_id)
            .map_err(MilestoneError::Journal)?;
        if !ended.ended {
            return Err(MilestoneError::Invariant {
                detail: "journal did not mark the finalized session ended".to_owned(),
            });
        }
        verify_workspace_identities(
            &self.directory,
            self.directory_identity,
            &self.source_path,
            self.source_identity,
            &self.journal_path,
            self.journal_identity,
        )?;

        Ok(FinalizedSession {
            directory: self.directory.clone(),
            directory_identity: self.directory_identity,
            source_path: self.source_path.clone(),
            source_identity: self.source_identity,
            journal_path: self.journal_path.clone(),
            journal_identity: self.journal_identity,
            session_id: self.session_id.clone(),
            event_count: self.persisted_event_count,
            final_event_hash,
            final_document_hash,
            final_workspace_hash,
        })
    }

    fn apply_editor_edit(
        &mut self,
        operation: &'static str,
        transaction: Option<EditorTransaction>,
    ) -> Result<bool, MilestoneError> {
        self.ensure_healthy()?;
        self.require_no_captured_transactions()?;
        let Some(transaction) = transaction else {
            return Ok(false);
        };
        let before_len = self.editor.text().len();
        let prospective_len = transaction_after_len(before_len, &transaction)?;
        self.preflight_recorded_state(prospective_len, &Event::FileEdited(transaction.clone()))?;
        let changed = match self.editor.apply_transaction(transaction) {
            Ok(changed) => changed,
            Err(error) => {
                self.require_no_captured_transactions()?;
                return Err(MilestoneError::Editor(error));
            }
        };
        self.after_mutation_boundary();
        let result = self.persist_applied_edit(operation, changed, prospective_len);
        self.finish_mutated_operation(operation, result)
    }

    fn persist_applied_edit(
        &mut self,
        operation: &'static str,
        changed: bool,
        prospective_len: usize,
    ) -> Result<bool, MilestoneError> {
        let captured_count = self.captured_transactions.borrow().len();
        if !changed {
            if captured_count != 0 {
                return Err(MilestoneError::Invariant {
                    detail: format!(
                        "no-op editor action captured {captured_count} transactions during {operation}"
                    ),
                });
            }
            return Ok(false);
        }
        if captured_count != 1 {
            return Err(MilestoneError::Invariant {
                detail: format!(
                    "successful editor action captured {captured_count} transactions during {operation}"
                ),
            });
        }
        let transaction = self
            .captured_transactions
            .borrow_mut()
            .pop_front()
            .expect("the exact captured transaction count was checked");
        self.persist_event(Event::FileEdited(transaction), operation)?;
        let after_len = self.editor.text().len();
        self.retained_state_bytes = checked_add_limit(
            "replay retained state bytes",
            self.retained_state_bytes,
            one_file_state_retained_charge(after_len)?,
            MAX_REPLAY_RETAINED_STATE_BYTES,
        )?;
        require_equal("previewed editor byte length", after_len, prospective_len)?;
        Ok(true)
    }

    fn persist_selection_change(
        &mut self,
        before: SelectionState,
        operation: &'static str,
    ) -> Result<bool, MilestoneError> {
        let after = self.editor.selection_state();
        if before == after {
            return Ok(false);
        }
        let event = Event::SelectionChanged(SelectionChanged {
            document_id: self.document_id.clone(),
            anchor_byte: after.anchor_byte,
            active_byte: after.active_byte,
        });
        self.persist_event(event, operation)?;
        self.retained_state_bytes = checked_add_limit(
            "replay retained state bytes",
            self.retained_state_bytes,
            one_file_state_retained_charge(self.editor.text().len())?,
            MAX_REPLAY_RETAINED_STATE_BYTES,
        )?;
        Ok(true)
    }

    fn persist_event(
        &mut self,
        event: Event,
        operation: &'static str,
    ) -> Result<Hash, MilestoneError> {
        let sequence =
            self.persisted_event_count
                .checked_add(1)
                .ok_or_else(|| MilestoneError::Invariant {
                    detail: "event budget sequence overflowed".to_owned(),
                })?;
        let monotonic_millis = self.next_monotonic_millis;
        let charge = event_retained_charge_for_submission(
            &self.session_id,
            sequence,
            monotonic_millis,
            &event,
        )?;
        let retained_event_bytes = checked_add_limit(
            "replay retained event bytes",
            self.retained_event_bytes,
            charge,
            MAX_REPLAY_RETAINED_EVENT_BYTES,
        )?;
        let submission = EventSubmission {
            session_id: self.session_id.clone(),
            monotonic_millis: self.take_monotonic_millis(),
            wall_clock_utc: None,
            event,
        };
        let hash = self.persist_job(
            JournalWriteJob::Event(submission),
            JournalWriteKind::Event,
            operation,
        )?;
        self.retained_event_bytes = retained_event_bytes;
        Ok(hash)
    }

    fn persist_job(
        &mut self,
        job: JournalWriteJob,
        kind: JournalWriteKind,
        operation: &'static str,
    ) -> Result<Hash, MilestoneError> {
        let result = (|| {
            self.ensure_healthy()?;
            verify_directory_identity(
                &self.directory,
                self.directory_identity,
                "milestone directory",
            )?;
            verify_regular_identity(&self.journal_path, self.journal_identity, "journal")?;
            let expected_sequence = self.persisted_event_count.checked_add(1).ok_or_else(|| {
                MilestoneError::Invariant {
                    detail: "journal event sequence overflowed u64".to_owned(),
                }
            })?;
            let writer = self
                .writer
                .as_ref()
                .ok_or_else(|| MilestoneError::Invariant {
                    detail: "journal writer is unavailable".to_owned(),
                })?;
            let hash = wait_for_persistence(writer, job, kind, expected_sequence, operation)
                .map_err(RecordingFailure::into_error)?;
            self.persisted_event_count = expected_sequence;
            Ok(hash)
        })();
        if let Err(error) = &result {
            self.record_failure(operation, error);
            self.shutdown_ignoring_error();
        }
        result
    }

    fn preflight_recorded_state(
        &self,
        prospective_len: usize,
        event: &Event,
    ) -> Result<(), MilestoneError> {
        verify_directory_identity(
            &self.directory,
            self.directory_identity,
            "milestone directory",
        )?;
        verify_regular_identity(&self.journal_path, self.journal_identity, "journal")?;
        require_limit(
            "source bytes",
            usize_to_u64(prospective_len, "prospective source length")?,
            MAX_WORKSPACE_FILE_BYTES,
        )?;
        let attempted_events = self
            .persisted_event_count
            .checked_add(1 + RESERVED_TERMINAL_EVENTS)
            .ok_or_else(|| MilestoneError::LimitExceeded {
                resource: "replay events",
                attempted: u64::MAX,
                maximum: MAX_MILESTONE_EVENTS,
            })?;
        require_limit("replay events", attempted_events, MAX_MILESTONE_EVENTS)?;
        let charge = one_file_state_retained_charge(prospective_len)?;
        let reserved_terminal = charge
            .checked_mul(RESERVED_TERMINAL_EVENTS)
            .ok_or_else(|| MilestoneError::LimitExceeded {
                resource: "replay retained state bytes",
                attempted: u64::MAX,
                maximum: MAX_REPLAY_RETAINED_STATE_BYTES,
            })?;
        let attempted = self
            .retained_state_bytes
            .checked_add(charge)
            .and_then(|total| total.checked_add(reserved_terminal))
            .ok_or_else(|| MilestoneError::LimitExceeded {
                resource: "replay retained state bytes",
                attempted: u64::MAX,
                maximum: MAX_REPLAY_RETAINED_STATE_BYTES,
            })?;
        require_limit(
            "replay retained state bytes",
            attempted,
            MAX_REPLAY_RETAINED_STATE_BYTES,
        )?;
        self.preflight_recorded_event(event)
    }

    fn preflight_recorded_event(&self, event: &Event) -> Result<(), MilestoneError> {
        let sequence =
            self.persisted_event_count
                .checked_add(1)
                .ok_or_else(|| MilestoneError::Invariant {
                    detail: "event budget sequence overflowed".to_owned(),
                })?;
        let charge = event_retained_charge_for_submission(
            &self.session_id,
            sequence,
            self.next_monotonic_millis,
            event,
        )?;
        let terminal = terminal_event_retained_budget(
            &self.session_id,
            &self.document_id,
            sequence,
            self.next_monotonic_millis.saturating_add(1),
            Hash::zero(),
            Hash::zero(),
        )?;
        let attempted = self
            .retained_event_bytes
            .checked_add(charge)
            .and_then(|value| value.checked_add(terminal.checkpoint))
            .and_then(|value| value.checked_add(terminal.finalization))
            .ok_or_else(event_budget_overflow)?;
        require_limit(
            "replay retained event bytes",
            attempted,
            MAX_REPLAY_RETAINED_EVENT_BYTES,
        )
    }

    fn selection_event(&self, selection: SelectionState) -> Event {
        Event::SelectionChanged(SelectionChanged {
            document_id: self.document_id.clone(),
            anchor_byte: selection.anchor_byte,
            active_byte: selection.active_byte,
        })
    }

    fn preflight_finalization(
        &self,
        final_len: usize,
        final_document_hash: Hash,
        final_workspace_hash: Hash,
    ) -> Result<TerminalEventBudget, MilestoneError> {
        verify_workspace_identities(
            &self.directory,
            self.directory_identity,
            &self.source_path,
            self.source_identity,
            &self.journal_path,
            self.journal_identity,
        )?;
        require_limit(
            "source bytes",
            usize_to_u64(final_len, "final source length")?,
            MAX_WORKSPACE_FILE_BYTES,
        )?;
        let attempted_events = self
            .persisted_event_count
            .checked_add(RESERVED_TERMINAL_EVENTS)
            .ok_or_else(|| MilestoneError::LimitExceeded {
                resource: "replay events",
                attempted: u64::MAX,
                maximum: MAX_MILESTONE_EVENTS,
            })?;
        require_limit("replay events", attempted_events, MAX_MILESTONE_EVENTS)?;
        let terminal_charge = one_file_state_retained_charge(final_len)?
            .checked_mul(RESERVED_TERMINAL_EVENTS)
            .ok_or_else(|| MilestoneError::LimitExceeded {
                resource: "replay retained state bytes",
                attempted: u64::MAX,
                maximum: MAX_REPLAY_RETAINED_STATE_BYTES,
            })?;
        checked_add_limit(
            "replay retained state bytes",
            self.retained_state_bytes,
            terminal_charge,
            MAX_REPLAY_RETAINED_STATE_BYTES,
        )?;

        let terminal = terminal_event_retained_budget(
            &self.session_id,
            &self.document_id,
            self.persisted_event_count,
            self.next_monotonic_millis,
            final_document_hash,
            final_workspace_hash,
        )?;
        let attempted = self
            .retained_event_bytes
            .checked_add(terminal.checkpoint)
            .and_then(|value| value.checked_add(terminal.finalization))
            .ok_or_else(event_budget_overflow)?;
        require_limit(
            "replay retained event bytes",
            attempted,
            MAX_REPLAY_RETAINED_EVENT_BYTES,
        )?;
        Ok(terminal)
    }

    fn ensure_healthy(&self) -> Result<(), MilestoneError> {
        match self.failure.clone() {
            Some(failure) => Err(failure.into_error()),
            None => Ok(()),
        }
    }

    fn require_no_captured_transactions(&self) -> Result<(), MilestoneError> {
        let count = self.captured_transactions.borrow().len();
        if count == 0 {
            Ok(())
        } else {
            Err(MilestoneError::Invariant {
                detail: format!("{count} editor transactions are awaiting persistence"),
            })
        }
    }

    fn finish_mutated_operation<T>(
        &mut self,
        operation: &'static str,
        result: Result<T, MilestoneError>,
    ) -> Result<T, MilestoneError> {
        if let Err(error) = &result {
            self.record_failure(operation, error);
            self.shutdown_ignoring_error();
        }
        result
    }

    fn record_failure(&mut self, operation: &'static str, error: &MilestoneError) {
        if self.failure.is_none() {
            self.failure = Some(RecordingFailure {
                operation,
                detail: error.to_string(),
            });
        }
    }

    #[cfg(test)]
    fn set_after_mutation_hook(&mut self, hook: impl FnMut(&Path) + 'static) {
        self.after_mutation_hook = Some(Box::new(hook));
    }

    fn after_mutation_boundary(&mut self) {
        #[cfg(test)]
        if let Some(hook) = &mut self.after_mutation_hook {
            hook(&self.journal_path);
        }
    }

    fn take_monotonic_millis(&mut self) -> u64 {
        let current = self.next_monotonic_millis;
        self.next_monotonic_millis = self.next_monotonic_millis.saturating_add(1);
        current
    }

    fn shutdown_ignoring_error(&mut self) {
        if let Some(writer) = self.writer.take() {
            let _ = writer.shutdown();
        }
    }
}

impl Drop for OneFileSession {
    fn drop(&mut self) {
        self.shutdown_ignoring_error();
    }
}

fn wait_for_persistence(
    writer: &JournalWriter,
    job: JournalWriteJob,
    expected_kind: JournalWriteKind,
    expected_sequence: u64,
    operation: &'static str,
) -> Result<Hash, RecordingFailure> {
    let receipt = writer.try_submit(job).map_err(|error| RecordingFailure {
        operation,
        detail: error.to_string(),
    })?;
    let completion = receipt.wait().map_err(|error| RecordingFailure {
        operation,
        detail: error.to_string(),
    })?;
    match completion {
        JournalWriteCompletion::Persisted {
            kind,
            sequence,
            event_hash,
            ..
        } if kind == expected_kind && sequence == expected_sequence => Ok(event_hash),
        JournalWriteCompletion::Persisted { kind, sequence, .. } => Err(RecordingFailure {
            operation,
            detail: format!(
                "journal acknowledged {kind:?} sequence {sequence}; expected {expected_kind:?} sequence {expected_sequence}"
            ),
        }),
        JournalWriteCompletion::Failed { detail, .. } => {
            Err(RecordingFailure { operation, detail })
        }
    }
}

fn resolve_input_directory_with(
    input: &Path,
    current_dir: impl FnOnce() -> std::io::Result<PathBuf>,
) -> Result<PathBuf, MilestoneError> {
    if input.is_absolute() {
        return Ok(input.to_path_buf());
    }
    current_dir()
        .map(|directory| directory.join(input))
        .map_err(|source| MilestoneError::Io {
            operation: "resolve milestone directory",
            source,
        })
}

fn create_new_journal_no_follow(path: &Path) -> Result<Journal, MilestoneError> {
    let initially_absent = match fs::symlink_metadata(path) {
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(source) => {
            return Err(MilestoneError::Io {
                operation: "inspect journal destination",
                source,
            });
        }
    };
    match Journal::create_no_follow(path) {
        Ok(journal) => Ok(journal),
        Err(error) => {
            // With settled namespace entries, create-new plus the absence
            // observation makes a resulting regular file ours. Concurrent
            // namespace replacement is outside this milestone's boundary; a
            // non-regular replacement is never removed.
            if initially_absent
                && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
            {
                let _ = fs::remove_file(path);
            }
            Err(MilestoneError::Journal(error))
        }
    }
}

fn write_new_synced(path: &Path, contents: &[u8]) -> Result<FileIdentity, MilestoneError> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|source| MilestoneError::Io {
            operation: "create starter source",
            source,
        })?;
    let identity = regular_file_handle_identity(&file, path, "starter source")?;
    let result = write_and_sync(&mut file, contents, "write starter source")
        .and_then(|()| verify_regular_identity(path, identity, "starter source"));
    drop(file);
    match result {
        Ok(()) => Ok(identity),
        Err(error) => {
            remove_created_file(path, identity);
            Err(error)
        }
    }
}

fn remove_created_file(path: &Path, expected: FileIdentity) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_file()
        && metadata_identity(&metadata).is_ok_and(|actual| actual == expected)
    {
        let _ = fs::remove_file(path);
    }
}

fn atomic_replace_synced(
    directory: &Path,
    directory_identity: FileIdentity,
    path: &Path,
    expected: FileIdentity,
    contents: &[u8],
) -> Result<FileIdentity, MilestoneError> {
    verify_directory_identity(directory, directory_identity, "milestone directory")?;
    let (temporary_path, mut temporary) = create_adjacent_temporary(directory)?;
    let prepared = (|| {
        write_and_sync(&mut temporary, contents, "write final source temporary")?;
        let identity =
            regular_file_handle_identity(&temporary, &temporary_path, "final source temporary")?;
        verify_directory_identity(directory, directory_identity, "milestone directory")?;
        verify_regular_identity(path, expected, "source")?;
        verify_regular_identity(&temporary_path, identity, "final source temporary")?;
        fs::rename(&temporary_path, path).map_err(|source| MilestoneError::Io {
            operation: "atomically replace final source",
            source,
        })?;
        verify_regular_identity(path, identity, "final source")?;
        verify_directory_identity(directory, directory_identity, "milestone directory")?;
        File::open(directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| MilestoneError::Io {
                operation: "sync milestone directory",
                source,
            })?;
        Ok(identity)
    })();
    if prepared.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    prepared
}

fn create_adjacent_temporary(directory: &Path) -> Result<(PathBuf, File), MilestoneError> {
    for _ in 0..128 {
        let serial = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            ".{SOURCE_FILE_NAME}.rustrace-{}-{serial}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(MilestoneError::Io {
                    operation: "create final source temporary",
                    source,
                });
            }
        }
    }
    Err(MilestoneError::Invariant {
        detail: "could not reserve a unique final source temporary".to_owned(),
    })
}

fn write_and_sync(
    file: &mut File,
    contents: &[u8],
    operation: &'static str,
) -> Result<(), MilestoneError> {
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|source| MilestoneError::Io { operation, source })
}

fn ensure_supported_platform() -> Result<(), MilestoneError> {
    if cfg!(unix) {
        Ok(())
    } else {
        Err(MilestoneError::UnsupportedPlatform {
            detail: "the milestone filesystem receipt requires Unix device/inode metadata"
                .to_owned(),
        })
    }
}

fn require_directory_identity(
    path: &Path,
    label: &'static str,
) -> Result<FileIdentity, MilestoneError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| MilestoneError::Io {
        operation: "inspect milestone directory",
        source,
    })?;
    if !metadata.file_type().is_dir() {
        return Err(MilestoneError::UnsafeFilesystemEntry {
            path: path.to_path_buf(),
            detail: format!("{label} is not a directory"),
        });
    }
    metadata_identity(&metadata)
}

fn require_regular_identity(
    path: &Path,
    label: &'static str,
) -> Result<FileIdentity, MilestoneError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| MilestoneError::Io {
        operation: "inspect milestone file",
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(MilestoneError::UnsafeFilesystemEntry {
            path: path.to_path_buf(),
            detail: format!("{label} is not a regular file"),
        });
    }
    metadata_identity(&metadata)
}

fn verify_directory_identity(
    path: &Path,
    expected: FileIdentity,
    label: &'static str,
) -> Result<(), MilestoneError> {
    let actual =
        fs::symlink_metadata(path).map_err(|error| MilestoneError::FilesystemIdentityMismatch {
            path: path.to_path_buf(),
            detail: format!("cannot inspect {label}: {error}"),
        })?;
    if !actual.file_type().is_dir() {
        return Err(MilestoneError::UnsafeFilesystemEntry {
            path: path.to_path_buf(),
            detail: format!("{label} is not a directory"),
        });
    }
    require_identity(path, label, expected, metadata_identity(&actual)?)
}

fn verify_regular_identity(
    path: &Path,
    expected: FileIdentity,
    label: &'static str,
) -> Result<(), MilestoneError> {
    let actual =
        fs::symlink_metadata(path).map_err(|error| MilestoneError::FilesystemIdentityMismatch {
            path: path.to_path_buf(),
            detail: format!("cannot inspect {label}: {error}"),
        })?;
    if !actual.file_type().is_file() {
        return Err(MilestoneError::UnsafeFilesystemEntry {
            path: path.to_path_buf(),
            detail: format!("{label} is not a regular file"),
        });
    }
    require_identity(path, label, expected, metadata_identity(&actual)?)
}

fn require_identity(
    path: &Path,
    label: &'static str,
    expected: FileIdentity,
    actual: FileIdentity,
) -> Result<(), MilestoneError> {
    if actual == expected {
        Ok(())
    } else {
        Err(MilestoneError::FilesystemIdentityMismatch {
            path: path.to_path_buf(),
            detail: format!("{label} identity changed from {expected:?} to {actual:?}"),
        })
    }
}

fn verify_workspace_identities(
    directory: &Path,
    directory_identity: FileIdentity,
    source_path: &Path,
    source_identity: FileIdentity,
    journal_path: &Path,
    journal_identity: FileIdentity,
) -> Result<(), MilestoneError> {
    verify_directory_identity(directory, directory_identity, "milestone directory")?;
    verify_regular_identity(source_path, source_identity, "source")?;
    verify_regular_identity(journal_path, journal_identity, "journal")
}

fn regular_file_handle_identity(
    file: &File,
    path: &Path,
    label: &'static str,
) -> Result<FileIdentity, MilestoneError> {
    let metadata = file.metadata().map_err(|source| MilestoneError::Io {
        operation: "inspect open milestone file",
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(MilestoneError::UnsafeFilesystemEntry {
            path: path.to_path_buf(),
            detail: format!("open {label} is not a regular file"),
        });
    }
    metadata_identity(&metadata)
}

fn metadata_identity(metadata: &fs::Metadata) -> Result<FileIdentity, MilestoneError> {
    #[cfg(unix)]
    {
        Ok(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(MilestoneError::UnsupportedPlatform {
            detail: "the milestone filesystem receipt requires Unix device/inode metadata"
                .to_owned(),
        })
    }
}

fn read_regular_file_bounded(
    path: &Path,
    expected: FileIdentity,
) -> Result<Vec<u8>, MilestoneError> {
    verify_regular_identity(path, expected, "source")?;
    #[cfg(unix)]
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|source| MilestoneError::Io {
            operation: "open final source without following links",
            source,
        })?;
    #[cfg(not(unix))]
    let file = return Err(MilestoneError::UnsupportedPlatform {
        detail: "bounded no-follow source reads require Unix open flags".to_owned(),
    });

    let handle_identity = regular_file_handle_identity(&file, path, "source")?;
    require_identity(path, "source", expected, handle_identity)?;
    let length = file
        .metadata()
        .map_err(|source| MilestoneError::Io {
            operation: "inspect final source length",
            source,
        })?
        .len();
    require_limit("source bytes", length, MAX_WORKSPACE_FILE_BYTES)?;
    let capacity = usize::try_from(length).map_err(|_| MilestoneError::LimitExceeded {
        resource: "source bytes",
        attempted: length,
        maximum: MAX_WORKSPACE_FILE_BYTES,
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_WORKSPACE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| MilestoneError::Io {
            operation: "read final source",
            source,
        })?;
    require_limit(
        "source bytes",
        usize_to_u64(bytes.len(), "read source length")?,
        MAX_WORKSPACE_FILE_BYTES,
    )?;
    verify_regular_identity(path, expected, "source")?;
    Ok(bytes)
}

fn one_file_state_retained_charge(length: usize) -> Result<u64, MilestoneError> {
    // Supported milestone states contain exactly one file and one open
    // document. The fixed allowance includes the step/state/document structs,
    // two generously sized one-entry BTree nodes, allocator headers, IDs,
    // paths, active-document storage, and hashes. Each cloned Vec/String
    // allocation is charged at twice its logical length plus a header, which
    // conservatively covers the exact-length Clone allocations used here and
    // ordinary allocator size-class rounding.
    const BTREE_NODE_ALLOWANCE: u64 = 1_024;
    const ALLOCATOR_HEADER_ALLOWANCE: u64 = 64;
    let fixed = usize_to_u64(
        std::mem::size_of::<ReplayStep>()
            + std::mem::size_of::<WorkspaceState>()
            + std::mem::size_of::<DocumentState>()
            + std::mem::size_of::<ReplayDocument>(),
        "one-file state fixed layout",
    )?
    .checked_add(2 * BTREE_NODE_ALLOWANCE)
    .and_then(|value| value.checked_add(8 * ALLOCATOR_HEADER_ALLOWANCE))
    .ok_or_else(state_budget_overflow)?;
    let names = allocation_charge(SOURCE_FILE_NAME.len())?
        .checked_mul(5)
        .ok_or_else(state_budget_overflow)?;
    let content = allocation_charge(length)?;
    fixed
        .checked_add(names)
        .and_then(|value| value.checked_add(content))
        .and_then(|value| value.checked_add(content))
        .ok_or_else(state_budget_overflow)
}

fn allocation_charge(length: usize) -> Result<u64, MilestoneError> {
    usize_to_u64(length, "retained allocation length")?
        .checked_mul(2)
        .and_then(|value| value.checked_add(64))
        .ok_or_else(state_budget_overflow)
}

fn state_budget_overflow() -> MilestoneError {
    MilestoneError::LimitExceeded {
        resource: "replay retained state bytes",
        attempted: u64::MAX,
        maximum: MAX_REPLAY_RETAINED_STATE_BYTES,
    }
}

fn transaction_after_len(
    before_len: usize,
    transaction: &EditorTransaction,
) -> Result<usize, MilestoneError> {
    let mut removed = 0usize;
    let mut inserted = 0usize;
    for edit in &transaction.edits {
        let start = usize::try_from(edit.start_byte).map_err(|_| MilestoneError::Invariant {
            detail: "edit start does not fit usize".to_owned(),
        })?;
        let end = usize::try_from(edit.end_byte).map_err(|_| MilestoneError::Invariant {
            detail: "edit end does not fit usize".to_owned(),
        })?;
        if start > end || end > before_len {
            return Err(MilestoneError::Invariant {
                detail: "previewed edit range is outside the current text".to_owned(),
            });
        }
        removed = removed
            .checked_add(end - start)
            .ok_or_else(state_budget_overflow)?;
        inserted = inserted
            .checked_add(edit.inserted_text.len())
            .ok_or_else(state_budget_overflow)?;
    }
    before_len
        .checked_sub(removed)
        .and_then(|remaining| remaining.checked_add(inserted))
        .ok_or_else(state_budget_overflow)
}

fn checked_add_limit(
    resource: &'static str,
    current: u64,
    additional: u64,
    maximum: u64,
) -> Result<u64, MilestoneError> {
    let attempted = current
        .checked_add(additional)
        .ok_or(MilestoneError::LimitExceeded {
            resource,
            attempted: u64::MAX,
            maximum,
        })?;
    require_limit(resource, attempted, maximum)?;
    Ok(attempted)
}

fn require_limit(
    resource: &'static str,
    attempted: u64,
    maximum: u64,
) -> Result<(), MilestoneError> {
    if attempted <= maximum {
        Ok(())
    } else {
        Err(MilestoneError::LimitExceeded {
            resource,
            attempted,
            maximum,
        })
    }
}

fn usize_to_u64(value: usize, detail: &'static str) -> Result<u64, MilestoneError> {
    u64::try_from(value).map_err(|_| MilestoneError::Invariant {
        detail: format!("{detail} does not fit u64"),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalizedSession {
    directory: PathBuf,
    directory_identity: FileIdentity,
    source_path: PathBuf,
    source_identity: FileIdentity,
    journal_path: PathBuf,
    journal_identity: FileIdentity,
    session_id: SessionId,
    event_count: u64,
    final_event_hash: Hash,
    final_document_hash: Hash,
    final_workspace_hash: Hash,
}

impl FinalizedSession {
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub const fn event_count(&self) -> u64 {
        self.event_count
    }

    pub const fn final_event_hash(&self) -> Hash {
        self.final_event_hash
    }

    pub const fn final_document_hash(&self) -> Hash {
        self.final_document_hash
    }

    pub const fn final_workspace_hash(&self) -> Hash {
        self.final_workspace_hash
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayStep {
    pub sequence: u64,
    pub event: Event,
    pub state: WorkspaceState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayReport {
    session_id: SessionId,
    source_bytes: Vec<u8>,
    steps: Vec<ReplayStep>,
    event_count: u64,
    checkpoint_count: u64,
    final_event_hash: Hash,
    final_workspace_hash: Hash,
    finalized: bool,
    retained_state_bytes: u64,
    retained_event_bytes: u64,
}

impl ReplayReport {
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn source_bytes(&self) -> &[u8] {
        &self.source_bytes
    }

    pub fn steps(&self) -> &[ReplayStep] {
        &self.steps
    }

    pub const fn event_count(&self) -> u64 {
        self.event_count
    }

    pub const fn checkpoint_count(&self) -> u64 {
        self.checkpoint_count
    }

    pub const fn final_event_hash(&self) -> Hash {
        self.final_event_hash
    }

    pub const fn final_workspace_hash(&self) -> Hash {
        self.final_workspace_hash
    }

    pub const fn finalized(&self) -> bool {
        self.finalized
    }

    pub const fn retained_state_bytes(&self) -> u64 {
        self.retained_state_bytes
    }

    pub const fn retained_event_bytes(&self) -> u64 {
        self.retained_event_bytes
    }

    pub fn final_state(&self) -> Option<&WorkspaceState> {
        self.steps.last().map(|step| &step.state)
    }
}

/// Opens a fresh journal connection, verifies all persisted integrity
/// boundaries, then replays and exposes the state following every event.
pub fn verify_and_replay(finalized: &FinalizedSession) -> Result<ReplayReport, MilestoneError> {
    ensure_supported_platform()?;
    verify_replay_target(
        &finalized.directory,
        finalized.directory_identity,
        &finalized.source_path,
        finalized.source_identity,
        &finalized.journal_path,
        finalized.journal_identity,
        &finalized.session_id,
        Some(finalized),
    )
}

/// Reopens an ended one-file session using only reconstructible process-external
/// inputs. Filesystem identities are captured at this point-in-time boundary;
/// the stronger process-local receipt checks remain in [`verify_and_replay`].
pub fn verify_and_replay_from_directory(
    directory: impl AsRef<Path>,
    session_id: &SessionId,
) -> Result<ReplayReport, MilestoneError> {
    ensure_supported_platform()?;
    let directory = resolve_input_directory_with(directory.as_ref(), std::env::current_dir)?;
    let directory = fs::canonicalize(&directory).map_err(|source| MilestoneError::Io {
        operation: "anchor verification directory",
        source,
    })?;
    let directory_identity = require_directory_identity(&directory, "milestone directory")?;
    let source_path = directory.join(SOURCE_FILE_NAME);
    let source_identity = require_regular_identity(&source_path, "source")?;
    let journal_path = directory.join(JOURNAL_FILE_NAME);
    let journal_identity = require_regular_identity(&journal_path, "journal")?;
    verify_replay_target(
        &directory,
        directory_identity,
        &source_path,
        source_identity,
        &journal_path,
        journal_identity,
        session_id,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn verify_replay_target(
    directory: &Path,
    directory_identity: FileIdentity,
    source_path: &Path,
    source_identity: FileIdentity,
    journal_path: &Path,
    journal_identity: FileIdentity,
    session_id: &SessionId,
    receipt: Option<&FinalizedSession>,
) -> Result<ReplayReport, MilestoneError> {
    verify_workspace_identities(
        directory,
        directory_identity,
        source_path,
        source_identity,
        journal_path,
        journal_identity,
    )?;
    let source_bytes = read_regular_file_bounded(source_path, source_identity)?;
    let workspace_path =
        WorkspacePath::new(SOURCE_FILE_NAME).expect("the fixed one-file workspace path is valid");
    let document_id =
        DocumentId::new(DOCUMENT_ID).expect("the fixed one-file document ID is valid");

    verify_regular_identity(journal_path, journal_identity, "journal")?;
    let mut journal =
        Journal::open_read_only_no_follow(journal_path).map_err(MilestoneError::Integrity)?;
    verify_regular_identity(journal_path, journal_identity, "journal")?;
    let session = journal
        .inspect_session(session_id)
        .map_err(MilestoneError::Integrity)?;
    let declared_event_count =
        session
            .next_sequence
            .checked_sub(1)
            .ok_or_else(|| MilestoneError::Mismatch {
                field: "session.next_sequence",
                detail: "next sequence cannot describe a nonnegative event count".to_owned(),
            })?;
    require_limit("replay events", declared_event_count, MAX_MILESTONE_EVENTS)?;
    if !session.ended {
        return Err(MilestoneError::IncompleteSession {
            detail: "session is not marked ended".to_owned(),
        });
    }
    let chain = journal
        .verify_session_chain(session_id)
        .map_err(MilestoneError::Integrity)?;
    require_equal(
        "declared event count",
        chain.event_count,
        declared_event_count,
    )?;
    let verified_checkpoints = journal
        .verify_session_checkpoints(session_id)
        .map_err(MilestoneError::Integrity)?;
    let expected_next_sequence =
        chain
            .event_count
            .checked_add(1)
            .ok_or_else(|| MilestoneError::Mismatch {
                field: "session.next_sequence",
                detail: "verified event count overflowed u64".to_owned(),
            })?;
    require_equal(
        "session.next_sequence",
        session.next_sequence,
        expected_next_sequence,
    )?;
    require_equal("checkpoint_count", verified_checkpoints.checkpoint_count, 2)?;
    if chain.event_count < 3 {
        return Err(MilestoneError::IncompleteSession {
            detail: "session lacks genesis, final checkpoint, or finalization".to_owned(),
        });
    }
    let final_checkpoint_sequence = chain.event_count - 1;
    require_equal(
        "checkpoints.latest_sequence",
        verified_checkpoints.latest_sequence,
        Some(final_checkpoint_sequence),
    )?;

    let (events, retained_event_bytes) =
        read_all_events(&mut journal, session_id, chain.event_count)?;
    let mut checkpoints = read_all_checkpoints(&mut journal, session_id)?;
    let retained_state_bytes =
        preflight_milestone_stream(&events, &checkpoints, &workspace_path, &document_id)?;
    let initial = checkpoints
        .remove(&1)
        .ok_or_else(|| MilestoneError::IncompleteSession {
            detail: "sequence 1 is not a stored genesis checkpoint".to_owned(),
        })?;
    let initial_event = events
        .first()
        .ok_or_else(|| MilestoneError::IncompleteSession {
            detail: "verified session contains no events".to_owned(),
        })?;
    require_equal(
        "genesis checkpoint owner",
        &initial.owning_event,
        initial_event,
    )?;
    let final_envelope = events
        .last()
        .expect("the verified genesis event makes the stream non-empty");
    let Event::SubmissionFinalized(finalization) = &final_envelope.event else {
        return Err(MilestoneError::IncompleteSession {
            detail: "last event is not submission_finalized".to_owned(),
        });
    };
    require_equal(
        "finalization.event_count",
        finalization.event_count,
        chain.event_count,
    )?;
    if !finalization.clean || !finalization.warnings.is_empty() {
        return Err(MilestoneError::IncompleteSession {
            detail: "terminal finalization is not clean and warning-free".to_owned(),
        });
    }

    let mut replay =
        ReplayEngine::from_initial_checkpoint(initial).map_err(MilestoneError::Replay)?;
    let step_capacity =
        usize::try_from(chain.event_count).map_err(|_| MilestoneError::LimitExceeded {
            resource: "replay events",
            attempted: chain.event_count,
            maximum: MAX_MILESTONE_EVENTS,
        })?;
    let mut steps = Vec::with_capacity(step_capacity);
    steps.push(ReplayStep {
        sequence: initial_event.sequence,
        event: initial_event.event.clone(),
        state: replay.workspace_state().clone(),
    });
    for envelope in events.iter().skip(1) {
        replay.apply(envelope).map_err(MilestoneError::Replay)?;
        if matches!(envelope.event, Event::WorkspaceCheckpoint(_)) {
            let checkpoint =
                checkpoints
                    .remove(&envelope.sequence)
                    .ok_or_else(|| MilestoneError::Mismatch {
                        field: "checkpoint owner",
                        detail: format!(
                            "event {} has no verified checkpoint payload",
                            envelope.sequence
                        ),
                    })?;
            require_equal(
                "checkpoint owner envelope",
                &checkpoint.owning_event,
                envelope,
            )?;
            replay
                .validate_checkpoint(&checkpoint)
                .map_err(MilestoneError::Replay)?;
        }
        steps.push(ReplayStep {
            sequence: envelope.sequence,
            event: envelope.event.clone(),
            state: replay.workspace_state().clone(),
        });
    }
    if !checkpoints.is_empty() {
        return Err(MilestoneError::Mismatch {
            field: "checkpoint events",
            detail: "verified checkpoint payload was not reached during replay".to_owned(),
        });
    }

    if !replay.is_terminal() || !replay.is_finalized() {
        return Err(MilestoneError::IncompleteSession {
            detail: "replay did not reach terminal finalized state".to_owned(),
        });
    }
    require_equal(
        "replay.next_sequence",
        replay.next_sequence(),
        expected_next_sequence,
    )?;
    require_equal(
        "event chain tip",
        replay.last_event_hash(),
        chain.final_hash,
    )?;

    let state = replay.workspace_state();
    let replay_file = state
        .file(&workspace_path)
        .ok_or_else(|| MilestoneError::Mismatch {
            field: "final file",
            detail: format!("replay has no {SOURCE_FILE_NAME}"),
        })?;
    require_equal("final raw file bytes", replay_file, source_bytes.as_slice())?;
    require_equal("final workspace file count", state.files().len(), 1usize)?;
    require_equal(
        "active document",
        state.active_document(),
        Some(&document_id),
    )?;
    let document = state
        .document(&document_id)
        .ok_or_else(|| MilestoneError::Mismatch {
            field: "final document",
            detail: format!("replay has no open document {document_id}"),
        })?;
    require_equal("final document path", document.path(), &workspace_path)?;
    require_equal(
        "final document bytes",
        document.text().as_bytes(),
        source_bytes.as_slice(),
    )?;
    require_equal(
        "final document hash",
        document.content_hash(),
        document_hash(document.text()),
    )?;
    let disk_workspace_hash = hash_entries([(&workspace_path, source_bytes.as_slice())])
        .map_err(MilestoneError::WorkspaceHash)?;
    require_equal(
        "final workspace hash",
        state.workspace_hash(),
        disk_workspace_hash,
    )?;
    require_equal(
        "finalization workspace hash",
        finalization.final_workspace_hash,
        disk_workspace_hash,
    )?;
    replay
        .verify_final_workspace_hash(disk_workspace_hash)
        .map_err(MilestoneError::Replay)?;
    if let Some(receipt) = receipt {
        require_equal(
            "finalized receipt event count",
            chain.event_count,
            receipt.event_count,
        )?;
        require_equal(
            "finalized receipt event hash",
            chain.final_hash,
            receipt.final_event_hash,
        )?;
        require_equal(
            "finalized receipt document hash",
            document.content_hash(),
            receipt.final_document_hash,
        )?;
        require_equal(
            "finalized receipt workspace hash",
            disk_workspace_hash,
            receipt.final_workspace_hash,
        )?;
    }
    verify_workspace_identities(
        directory,
        directory_identity,
        source_path,
        source_identity,
        journal_path,
        journal_identity,
    )?;

    Ok(ReplayReport {
        session_id: session_id.clone(),
        source_bytes,
        steps,
        event_count: chain.event_count,
        checkpoint_count: verified_checkpoints.checkpoint_count,
        final_event_hash: chain.final_hash,
        final_workspace_hash: disk_workspace_hash,
        finalized: replay.is_finalized(),
        retained_state_bytes,
        retained_event_bytes,
    })
}

#[derive(Clone, Copy)]
struct ProjectedOneFileState {
    length: usize,
    version: u64,
    selection: SelectionState,
}

fn preflight_milestone_stream(
    events: &[EventEnvelope],
    checkpoints: &BTreeMap<u64, StoredCheckpoint>,
    workspace_path: &WorkspacePath,
    document_id: &DocumentId,
) -> Result<u64, MilestoneError> {
    let event_count = events.len();
    if event_count < 3 {
        return Err(MilestoneError::IncompleteSession {
            detail: "session lacks genesis, final checkpoint, or finalization".to_owned(),
        });
    }
    let final_checkpoint_sequence =
        u64::try_from(event_count - 1).map_err(|_| MilestoneError::Invariant {
            detail: "final checkpoint index does not fit u64".to_owned(),
        })?;
    let initial = checkpoints
        .get(&1)
        .ok_or_else(|| MilestoneError::IncompleteSession {
            detail: "sequence 1 is not a stored genesis checkpoint".to_owned(),
        })?;
    require_equal(
        "genesis checkpoint owner",
        &initial.owning_event,
        &events[0],
    )?;
    if !matches!(events[0].event, Event::WorkspaceCheckpoint(_)) {
        return Err(MilestoneError::UnsupportedMilestoneEvent {
            sequence: 1,
            event: event_name(&events[0].event),
        });
    }
    let mut projected = validate_one_file_checkpoint(initial, workspace_path, document_id)?;
    let mut retained = checked_add_limit(
        "replay retained state bytes",
        0,
        one_file_state_retained_charge(projected.length)?,
        MAX_REPLAY_RETAINED_STATE_BYTES,
    )?;

    for (index, envelope) in events.iter().enumerate().skip(1) {
        let is_final_checkpoint = envelope.sequence == final_checkpoint_sequence;
        let is_finalization = index + 1 == event_count;
        match &envelope.event {
            Event::FileEdited(transaction) if !is_final_checkpoint && !is_finalization => {
                if !matches!(
                    transaction.origin,
                    EditOrigin::Keyboard | EditOrigin::Paste | EditOrigin::Undo | EditOrigin::Redo
                ) || transaction.edits.len() != 1
                {
                    return Err(MilestoneError::UnsupportedMilestoneEvent {
                        sequence: envelope.sequence,
                        event: "file_edited shape",
                    });
                }
                require_equal("edited document", &transaction.document_id, document_id)?;
                require_equal(
                    "edit version before",
                    transaction.version_before,
                    projected.version,
                )?;
                require_equal(
                    "edit version after",
                    transaction.version_after,
                    projected
                        .version
                        .checked_add(1)
                        .ok_or_else(|| MilestoneError::Invariant {
                            detail: "projected document version overflowed".to_owned(),
                        })?,
                )?;
                require_equal(
                    "edit selection before",
                    transaction.selection_before,
                    projected.selection,
                )?;
                projected.length = transaction_after_len(projected.length, transaction)?;
                require_limit(
                    "source bytes",
                    usize_to_u64(projected.length, "projected source length")?,
                    MAX_WORKSPACE_FILE_BYTES,
                )?;
                validate_projected_selection(transaction.selection_after, projected.length)?;
                projected.version = transaction.version_after;
                projected.selection = transaction.selection_after;
            }
            Event::SelectionChanged(selection) if !is_final_checkpoint && !is_finalization => {
                require_equal("selection document", &selection.document_id, document_id)?;
                let next = SelectionState::new(selection.anchor_byte, selection.active_byte);
                validate_projected_selection(next, projected.length)?;
                projected.selection = next;
            }
            Event::WorkspaceCheckpoint(_) if is_final_checkpoint => {
                let checkpoint = checkpoints.get(&envelope.sequence).ok_or_else(|| {
                    MilestoneError::Mismatch {
                        field: "checkpoint owner",
                        detail: format!(
                            "event {} has no verified checkpoint payload",
                            envelope.sequence
                        ),
                    }
                })?;
                require_equal(
                    "checkpoint owner envelope",
                    &checkpoint.owning_event,
                    envelope,
                )?;
                let checkpoint_state =
                    validate_one_file_checkpoint(checkpoint, workspace_path, document_id)?;
                require_equal(
                    "final checkpoint byte length",
                    checkpoint_state.length,
                    projected.length,
                )?;
                require_equal(
                    "final checkpoint document version",
                    checkpoint_state.version,
                    projected.version,
                )?;
                require_equal(
                    "final checkpoint selection",
                    checkpoint_state.selection,
                    projected.selection,
                )?;
            }
            Event::SubmissionFinalized(_) if is_finalization => {}
            event => {
                return Err(MilestoneError::UnsupportedMilestoneEvent {
                    sequence: envelope.sequence,
                    event: event_name(event),
                });
            }
        }
        retained = checked_add_limit(
            "replay retained state bytes",
            retained,
            one_file_state_retained_charge(projected.length)?,
            MAX_REPLAY_RETAINED_STATE_BYTES,
        )?;
    }
    Ok(retained)
}

fn validate_one_file_checkpoint(
    checkpoint: &StoredCheckpoint,
    workspace_path: &WorkspacePath,
    document_id: &DocumentId,
) -> Result<ProjectedOneFileState, MilestoneError> {
    let snapshot = &checkpoint.snapshot;
    require_equal("checkpoint file count", snapshot.files().len(), 1usize)?;
    require_equal(
        "checkpoint document count",
        snapshot.documents().len(),
        1usize,
    )?;
    require_equal(
        "checkpoint active document",
        snapshot.active_document(),
        Some(document_id),
    )?;
    let file = &snapshot.files()[0];
    require_equal("checkpoint file path", &file.path, workspace_path)?;
    let document = &snapshot.documents()[0];
    require_equal("checkpoint document id", &document.document_id, document_id)?;
    require_equal("checkpoint document path", &document.path, workspace_path)?;
    validate_projected_selection(document.selection, file.contents.len())?;
    Ok(ProjectedOneFileState {
        length: file.contents.len(),
        version: document.version,
        selection: document.selection,
    })
}

fn validate_projected_selection(
    selection: SelectionState,
    text_len: usize,
) -> Result<(), MilestoneError> {
    let maximum = usize_to_u64(text_len, "projected selection limit")?;
    if selection.anchor_byte <= maximum && selection.active_byte <= maximum {
        Ok(())
    } else {
        Err(MilestoneError::Mismatch {
            field: "projected selection",
            detail: format!("{selection:?} exceeds {maximum} bytes"),
        })
    }
}

const fn event_name(event: &Event) -> &'static str {
    match event {
        Event::SessionStarted(_) => "session_started",
        Event::SessionResumed(_) => "session_resumed",
        Event::SessionEnded(_) => "session_ended",
        Event::FileCreated(_) => "file_created",
        Event::FileDeleted(_) => "file_deleted",
        Event::FileRenamed(_) => "file_renamed",
        Event::FileFocused(_) => "file_focused",
        Event::FileEdited(_) => "file_edited",
        Event::ClipboardCopied(_) => "clipboard_copied",
        Event::InternalPaste(_) => "internal_paste",
        Event::PasteRejected(_) => "paste_rejected",
        Event::SelectionChanged(_) => "selection_changed",
        Event::ViewportChanged(_) => "viewport_changed",
        Event::CargoCommandStarted(_) => "cargo_command_started",
        Event::CargoDiagnostic(_) => "cargo_diagnostic",
        Event::CargoOutput(_) => "cargo_output",
        Event::CargoCommandFinished(_) => "cargo_command_finished",
        Event::ControlledCommandStarted(_) => "controlled_command_started",
        Event::ControlledCommandOutput(_) => "controlled_command_output",
        Event::ControlledCommandFinished(_) => "controlled_command_finished",
        Event::TestCaseCompared(_) => "test_case_compared",
        Event::LspCompletionRequested(_) => "lsp_completion_requested",
        Event::LspCompletionAccepted(_) => "lsp_completion_accepted",
        Event::LspCodeActionApplied(_) => "lsp_code_action_applied",
        Event::WorkspaceCheckpoint(_) => "workspace_checkpoint",
        Event::ExternalFileChange(_) => "external_file_change",
        Event::ExternalObservation(_) => "external_observation",
        Event::RecoveryRecorded(_) => "recovery_recorded",
        Event::SubmissionFinalized(_) => "submission_finalized",
    }
}

fn event_retained_charge(envelope: &EventEnvelope) -> Result<u64, MilestoneError> {
    let encoded = encode_envelope(envelope).map_err(|error| MilestoneError::Invariant {
        detail: format!("verified envelope could not be re-encoded: {error}"),
    })?;
    let dynamic = usize_to_u64(encoded.len(), "encoded retained event length")?
        .checked_mul(2)
        .ok_or_else(event_budget_overflow)?;
    usize_to_u64(
        std::mem::size_of::<EventEnvelope>() + std::mem::size_of::<Event>(),
        "retained event fixed layout",
    )?
    .checked_add(dynamic)
    .and_then(|value| value.checked_add(128))
    .ok_or_else(event_budget_overflow)
}

fn event_retained_charge_for_submission(
    session_id: &SessionId,
    sequence: u64,
    monotonic_millis: u64,
    event: &Event,
) -> Result<u64, MilestoneError> {
    event_retained_charge(&EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: session_id.clone(),
        sequence,
        monotonic_millis,
        wall_clock_utc: None,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event: event.clone(),
    })
}

fn terminal_event_retained_budget(
    session_id: &SessionId,
    document_id: &DocumentId,
    persisted_event_count: u64,
    next_monotonic_millis: u64,
    final_document_hash: Hash,
    final_workspace_hash: Hash,
) -> Result<TerminalEventBudget, MilestoneError> {
    let checkpoint_sequence =
        persisted_event_count
            .checked_add(1)
            .ok_or_else(|| MilestoneError::Invariant {
                detail: "final checkpoint sequence overflowed".to_owned(),
            })?;
    let finalization_sequence =
        checkpoint_sequence
            .checked_add(1)
            .ok_or_else(|| MilestoneError::Invariant {
                detail: "finalization sequence overflowed".to_owned(),
            })?;
    let checkpoint_event = Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
        workspace_hash: final_workspace_hash,
        documents: vec![DocumentHash {
            document_id: document_id.clone(),
            hash: final_document_hash,
        }],
    });
    let finalization_event = Event::SubmissionFinalized(SubmissionFinalized {
        final_workspace_hash,
        event_count: finalization_sequence,
        clean: true,
        warnings: Vec::new(),
    });
    let checkpoint = event_retained_charge_for_submission(
        session_id,
        checkpoint_sequence,
        next_monotonic_millis,
        &checkpoint_event,
    )?;
    let finalization = event_retained_charge_for_submission(
        session_id,
        finalization_sequence,
        next_monotonic_millis.saturating_add(1),
        &finalization_event,
    )?;
    Ok(TerminalEventBudget {
        checkpoint,
        finalization,
    })
}

fn event_budget_overflow() -> MilestoneError {
    MilestoneError::LimitExceeded {
        resource: "replay retained event bytes",
        attempted: u64::MAX,
        maximum: MAX_REPLAY_RETAINED_EVENT_BYTES,
    }
}

fn read_all_events(
    journal: &mut Journal,
    session_id: &SessionId,
    event_count: u64,
) -> Result<(Vec<EventEnvelope>, u64), MilestoneError> {
    require_limit("replay events", event_count, MAX_MILESTONE_EVENTS)?;
    let capacity = usize::try_from(event_count).map_err(|_| MilestoneError::LimitExceeded {
        resource: "replay events",
        attempted: event_count,
        maximum: MAX_MILESTONE_EVENTS,
    })?;
    let mut events = Vec::with_capacity(capacity);
    let mut retained_event_bytes = 0u64;
    let mut next_sequence = 1u64;
    while next_sequence <= event_count {
        let page = journal
            .read_events(session_id, next_sequence, 1)
            .map_err(MilestoneError::Integrity)?;
        let Some(last) = page.last() else {
            return Err(MilestoneError::Mismatch {
                field: "event count",
                detail: format!("event stream ended before sequence {next_sequence}"),
            });
        };
        next_sequence = last
            .sequence
            .checked_add(1)
            .ok_or_else(|| MilestoneError::Mismatch {
                field: "event sequence",
                detail: "event sequence overflowed u64".to_owned(),
            })?;
        for envelope in page {
            retained_event_bytes = checked_add_limit(
                "replay retained event bytes",
                retained_event_bytes,
                event_retained_charge(&envelope)?,
                MAX_REPLAY_RETAINED_EVENT_BYTES,
            )?;
            events.push(envelope);
        }
    }
    require_equal("decoded event count", events.len() as u64, event_count)?;
    Ok((events, retained_event_bytes))
}

fn read_all_checkpoints(
    journal: &mut Journal,
    session_id: &SessionId,
) -> Result<BTreeMap<u64, StoredCheckpoint>, MilestoneError> {
    let mut checkpoints = BTreeMap::new();
    let mut next_sequence = 1u64;
    loop {
        let page = journal
            .list_checkpoints(session_id, next_sequence, MAX_CHECKPOINTS_PER_READ)
            .map_err(MilestoneError::Integrity)?;
        let Some(last) = page.last() else {
            break;
        };
        next_sequence =
            last.owning_event
                .sequence
                .checked_add(1)
                .ok_or_else(|| MilestoneError::Mismatch {
                    field: "checkpoint sequence",
                    detail: "checkpoint sequence overflowed u64".to_owned(),
                })?;
        for checkpoint in page {
            let sequence = checkpoint.owning_event.sequence;
            if checkpoints.insert(sequence, checkpoint).is_some() {
                return Err(MilestoneError::Mismatch {
                    field: "checkpoint sequence",
                    detail: format!("duplicate checkpoint sequence {sequence}"),
                });
            }
        }
    }
    Ok(checkpoints)
}

fn require_equal<T>(field: &'static str, actual: T, expected: T) -> Result<(), MilestoneError>
where
    T: fmt::Debug + PartialEq,
{
    if actual == expected {
        Ok(())
    } else {
        Err(MilestoneError::Mismatch {
            field,
            detail: format!("actual {actual:?}; expected {expected:?}"),
        })
    }
}

#[derive(Debug)]
pub enum MilestoneError {
    Io {
        operation: &'static str,
        source: std::io::Error,
    },
    Journal(JournalError),
    Integrity(JournalError),
    WriterStart(JournalWriterStartError),
    Writer(JournalWriterError),
    Editor(TransactionError),
    Replay(ReplayError),
    WorkspaceHash(WorkspaceHashError),
    RecordingFailed {
        operation: &'static str,
        detail: String,
    },
    LimitExceeded {
        resource: &'static str,
        attempted: u64,
        maximum: u64,
    },
    UnsupportedMilestoneEvent {
        sequence: u64,
        event: &'static str,
    },
    UnsafeFilesystemEntry {
        path: PathBuf,
        detail: String,
    },
    FilesystemIdentityMismatch {
        path: PathBuf,
        detail: String,
    },
    UnsupportedPlatform {
        detail: String,
    },
    IncompleteSession {
        detail: String,
    },
    Mismatch {
        field: &'static str,
        detail: String,
    },
    Invariant {
        detail: String,
    },
}

impl fmt::Display for MilestoneError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::Journal(source) => write!(formatter, "journal operation failed: {source}"),
            Self::Integrity(source) => write!(formatter, "journal integrity failed: {source}"),
            Self::WriterStart(source) => write!(formatter, "journal writer start failed: {source}"),
            Self::Writer(source) => write!(formatter, "journal writer shutdown failed: {source}"),
            Self::Editor(source) => write!(formatter, "editor action rejected: {source}"),
            Self::Replay(source) => write!(formatter, "headless replay failed: {source}"),
            Self::WorkspaceHash(source) => write!(formatter, "workspace hashing failed: {source}"),
            Self::RecordingFailed { operation, detail } => {
                write!(formatter, "{operation}: {detail}")
            }
            Self::LimitExceeded {
                resource,
                attempted,
                maximum,
            } => write!(
                formatter,
                "milestone {resource} limit exceeded: {attempted} > {maximum}"
            ),
            Self::UnsupportedMilestoneEvent { sequence, event } => write!(
                formatter,
                "event {sequence} ({event}) is outside the one-file milestone slice"
            ),
            Self::UnsafeFilesystemEntry { path, detail } => {
                write!(
                    formatter,
                    "unsafe filesystem entry {}: {detail}",
                    path.display()
                )
            }
            Self::FilesystemIdentityMismatch { path, detail } => write!(
                formatter,
                "filesystem identity mismatch at {}: {detail}",
                path.display()
            ),
            Self::UnsupportedPlatform { detail } => {
                write!(formatter, "unsupported milestone platform: {detail}")
            }
            Self::IncompleteSession { detail } => write!(formatter, "incomplete session: {detail}"),
            Self::Mismatch { field, detail } => {
                write!(formatter, "verified {field} mismatch: {detail}")
            }
            Self::Invariant { detail } => write!(formatter, "vertical-slice invariant: {detail}"),
        }
    }
}

impl Error for MilestoneError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Journal(source) | Self::Integrity(source) => Some(source),
            Self::WriterStart(source) => Some(source),
            Self::Writer(source) => Some(source),
            Self::Editor(source) => Some(source),
            Self::Replay(source) => Some(source),
            Self::WorkspaceHash(source) => Some(source),
            Self::RecordingFailed { .. }
            | Self::LimitExceeded { .. }
            | Self::UnsupportedMilestoneEvent { .. }
            | Self::UnsafeFilesystemEntry { .. }
            | Self::FilesystemIdentityMismatch { .. }
            | Self::UnsupportedPlatform { .. }
            | Self::IncompleteSession { .. }
            | Self::Mismatch { .. }
            | Self::Invariant { .. } => None,
        }
    }
}

#[cfg(test)]
mod review_two_regressions {
    use std::cell::Cell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use rustrace_journal::Journal;
    use rustrace_model::SessionId;

    use super::{
        MAX_MILESTONE_EVENTS, MAX_REPLAY_RETAINED_EVENT_BYTES, MAX_REPLAY_RETAINED_STATE_BYTES,
        MilestoneError, OneFileSession, RESERVED_TERMINAL_EVENTS, one_file_state_retained_charge,
        resolve_input_directory_with,
    };
    use crate::editor::Movement;

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new(label: &str) -> Self {
            let serial = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rustrace-milestone-a-review-two-{label}-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn session_id(label: &str) -> SessionId {
        SessionId::new(label).unwrap()
    }

    #[test]
    fn relative_input_is_resolved_from_exactly_one_current_directory_snapshot() {
        let calls = Cell::new(0);
        let resolved = resolve_input_directory_with(Path::new("relative/workspace"), || {
            calls.set(calls.get() + 1);
            Ok(PathBuf::from("/captured/current"))
        })
        .unwrap();
        assert_eq!(resolved, Path::new("/captured/current/relative/workspace"));
        assert_eq!(calls.get(), 1);

        let absolute = resolve_input_directory_with(Path::new("/already/absolute"), || {
            panic!("absolute paths must not inspect current_dir")
        })
        .unwrap();
        assert_eq!(absolute, Path::new("/already/absolute"));
    }

    #[test]
    fn one_file_empty_state_has_positive_conservative_retained_charge() {
        let charge = one_file_state_retained_charge(0).unwrap();
        assert!(charge > 0);
        assert!(charge >= u64::try_from(std::mem::size_of::<super::ReplayStep>()).unwrap());
    }

    #[test]
    fn capacity_bound_noop_actions_return_false_without_spending_budget() {
        let directory = TempDirectory::new("capacity-noops");
        let mut session =
            OneFileSession::start(&directory.0, session_id("capacity-noops"), "").unwrap();
        session.persisted_event_count = MAX_MILESTONE_EVENTS - RESERVED_TERMINAL_EVENTS;
        let before_state = session.retained_state_bytes;

        assert!(!session.move_cursor(Movement::DocumentStart, false).unwrap());
        assert!(!session.delete_backward().unwrap());
        assert!(!session.delete_forward().unwrap());
        assert!(!session.paste("").unwrap());
        assert_eq!(session.retained_state_bytes, before_state);
        assert_eq!(
            session.persisted_event_count,
            MAX_MILESTONE_EVENTS - RESERVED_TERMINAL_EVENTS
        );
    }

    #[test]
    fn exact_shrinking_delete_is_admitted_at_the_state_budget_boundary() {
        let directory = TempDirectory::new("exact-delete");
        let mut session =
            OneFileSession::start(&directory.0, session_id("exact-delete"), "🦀").unwrap();
        assert!(session.select_all().unwrap());
        let empty_charge = one_file_state_retained_charge(0).unwrap();
        session.retained_state_bytes = MAX_REPLAY_RETAINED_STATE_BYTES - 3 * empty_charge;

        assert!(session.delete_backward().unwrap());
        assert_eq!(session.text(), "");
    }

    #[test]
    fn retained_event_budget_is_rejected_before_selection_mutation() {
        let directory = TempDirectory::new("recording-event-bytes");
        let mut session =
            OneFileSession::start(&directory.0, session_id("recording-event-bytes"), "a").unwrap();
        session.retained_event_bytes = MAX_REPLAY_RETAINED_EVENT_BYTES;
        let before = session.selection();

        assert!(matches!(
            session.move_cursor(Movement::DocumentEnd, false),
            Err(MilestoneError::LimitExceeded {
                resource: "replay retained event bytes",
                ..
            })
        ));
        assert_eq!(session.selection(), before);
        assert_eq!(session.persisted_event_count(), 1);
    }

    #[test]
    fn post_mutation_identity_failure_poisoning_survives_path_restoration() {
        let directory = TempDirectory::new("post-mutation-poison");
        let id = session_id("post-mutation-poison");
        let original = directory.0.join("original-session.sqlite3");
        let hook_original = original.clone();
        let mut session = OneFileSession::start(&directory.0, id.clone(), "a").unwrap();
        session.set_after_mutation_hook(move |journal_path| {
            fs::rename(journal_path, &hook_original).unwrap();
            fs::write(journal_path, b"replacement").unwrap();
        });

        assert!(matches!(
            session.insert_char('x'),
            Err(MilestoneError::FilesystemIdentityMismatch { .. })
        ));
        fs::remove_file(session.journal_path()).unwrap();
        fs::rename(&original, session.journal_path()).unwrap();
        assert!(matches!(
            session.insert_char('y'),
            Err(MilestoneError::RecordingFailed { .. })
        ));
        assert!(matches!(
            session.finish(),
            Err(MilestoneError::RecordingFailed { .. })
        ));

        let mut journal = Journal::open(directory.0.join("session.sqlite3")).unwrap();
        assert!(!journal.inspect_session(&id).unwrap().ended);
        assert_eq!(journal.verify_session_chain(&id).unwrap().event_count, 1);
    }

    #[test]
    fn post_selection_identity_failure_also_permanently_poisons() {
        let directory = TempDirectory::new("post-selection-poison");
        let id = session_id("post-selection-poison");
        let original = directory.0.join("original-session.sqlite3");
        let hook_original = original.clone();
        let mut session = OneFileSession::start(&directory.0, id.clone(), "a").unwrap();
        session.set_after_mutation_hook(move |journal_path| {
            fs::rename(journal_path, &hook_original).unwrap();
            fs::write(journal_path, b"replacement").unwrap();
        });

        assert!(matches!(
            session.move_cursor(Movement::DocumentEnd, false),
            Err(MilestoneError::FilesystemIdentityMismatch { .. })
        ));
        fs::remove_file(session.journal_path()).unwrap();
        fs::rename(&original, session.journal_path()).unwrap();
        assert!(matches!(
            session.select_all(),
            Err(MilestoneError::RecordingFailed { .. })
        ));
        assert!(matches!(
            session.finish(),
            Err(MilestoneError::RecordingFailed { .. })
        ));

        let mut journal = Journal::open(directory.0.join("session.sqlite3")).unwrap();
        assert!(!journal.inspect_session(&id).unwrap().ended);
        assert_eq!(journal.verify_session_chain(&id).unwrap().event_count, 1);
    }
}
