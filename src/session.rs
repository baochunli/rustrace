//! Production single-writer sessions. Receipt success, never queue acceptance, is durability.
use crate::tui::{
    EditorCommand, EditorOutcome, WorkspaceEffectError, WorkspaceEffects, WorkspaceSession,
};
use rustrace_editor::{EditorEffectError, EditorEffects, EditorTransaction};
use rustrace_journal::*;
use rustrace_model::{assignment::AssignmentManifest, *};
use rustrace_replay::ReplayEngine;
use rustrace_workspace::hash::{
    PinnedJournalFile, PinnedStateInspection, PinnedWorkspaceRoot, hash_entries,
    read_pinned_workspace,
};
use rustrace_workspace::{
    AllowedPathSet, assignment_package::ExtractedAssignment, create_workspace_file_in,
    remove_workspace_file_in, write_workspace_file_in,
};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    error::Error,
    fs::{self, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
type Files = BTreeMap<WorkspacePath, Vec<u8>>;
const ARTIFACT_LIMIT: usize = 34 * 1024 * 1024;
const METADATA_LIMIT: usize = 1024 * 1024;

mod bundle;
mod command;
mod diagnostics;
mod finalization;
mod privacy;
mod retention;

pub use crate::console::{TestCase, TestCaseComparison, TestCaseMismatch, TestCaseOutcome};
pub use bundle::{
    BundleOutput, create_bundle, hash_imported_outer_source, run_submit, submit_finalized_workspace,
};
pub use finalization::{
    FinalizationPayload, FinalizationReceipt, FinalizationStatus, IncompleteFinalization,
};
pub(crate) use finalization::{ReadOnlyFinalizationReceipt, ReadOnlyFinalizationStatus};
pub use privacy::run_privacy;
pub use retention::{run_cleanup, run_revise, run_status};

#[cfg(test)]
#[path = "session_clipboard_tests.rs"]
mod clipboard_tests;

#[cfg(test)]
#[path = "session/completion_tests.rs"]
mod completion_tests;

#[cfg(test)]
#[path = "session/diagnostics_tests.rs"]
mod diagnostics_tests;

#[cfg(test)]
#[path = "session/finalization_tests.rs"]
mod finalization_tests;

#[cfg(test)]
#[path = "session/bundle_tests.rs"]
mod bundle_tests;

pub const PASTE_BLOCKED_WARNING: &str = "Paste blocked: only text copied or cut inside this recorded workspace is allowed. Use Ctrl-C, Ctrl-X and Ctrl-V. ⌘C, ⌘X and ⌘V work only when the terminal delivers them; macOS terminals capture ⌘C and ⌘V for the system clipboard.";
pub const SAVE_CHECK_BUSY_WARNING: &str =
    "Warning: save-triggered Check skipped because another command owns the runner.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeChoice {
    Resume,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConsoleStart {
    Started,
    OverwriteConfirmation { path: WorkspacePath },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExplicitSaveOutcome {
    CheckStarted,
    RunnerBusy,
    CheckFailedToStart(String),
}

pub(crate) enum TerminalQuit {
    Ready(Result<()>),
    CleanupPending,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMetadata {
    pub version: u32,
    pub session_id: SessionId,
    pub course_id: String,
    pub assignment_id: String,
    pub assignment_version: String,
    pub manifest_hash: Hash,
    pub starter_hash: Hash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_case_suite_hash: Option<Hash>,
    pub client_version: String,
    pub build_identity: String,
    pub elapsed_time: String,
    pub parent_evidence: Option<Hash>,
}

/// Fixed pilot budgets; commands consume the output policy in T4.
#[derive(Clone, Copy, Debug)]
pub struct SessionBudgets {
    pub storage_bytes: u64,
    pub events: u64,
    pub undo_bytes: usize,
    pub output_per_command: u64,
    pub output_per_session: u64,
    pub reserve_bytes: u64,
}
impl Default for SessionBudgets {
    fn default() -> Self {
        Self {
            storage_bytes: 2 * 1024 * 1024 * 1024,
            events: 1_000_000,
            undo_bytes: 64 * 1024 * 1024,
            output_per_command: 8 * 1024 * 1024,
            output_per_session: 64 * 1024 * 1024,
            reserve_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedReceipt {
    version: u32,
    session_id: SessionId,
    sequence: u64,
    workspace_hash: Hash,
    event_hash: Hash,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestorationOutcome {
    version: u32,
    evidence_hash: Hash,
    sequence: u64,
    event_hash: Hash,
    workspace_hash: Hash,
    outcome: String,
}

struct ValidatedPrefix {
    replay: ReplayEngine,
    sequence: u64,
    millis: u64,
    hash: Hash,
    saved: Files,
    checkpoint_millis: u64,
    uncaptured_edits: u64,
    changed: bool,
    pending_external: Option<Hash>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum EvidenceRequirement {
    Complete,
    AllowMissing,
}

#[derive(Clone)]
pub struct SessionEffects(Rc<RefCell<Authority>>);
struct PendingCheckpoint {
    receipt: JournalWriteReceipt,
    snapshot: CheckpointSnapshot,
    millis: u64,
}
struct Authority {
    writer: Option<JournalWriter>,
    owner: PinnedJournalFile,
    metadata: SessionMetadata,
    started: Instant,
    offset: u64,
    persisted_millis: u64,
    sequence: u64,
    hash: Hash,
    replay: Option<ReplayEngine>,
    poison: Option<String>,
    schedule: CheckpointScheduleState,
    changed: bool,
    pending_checkpoint: Option<PendingCheckpoint>,
    budgets: SessionBudgets,
    // Includes preparation and post-boundary persistence, not only child runtime.
    command_active: bool,
    // Armed only around one preflighted formatter transaction publication.
    formatter_permit: bool,
    // Armed only around one preflighted dependency transaction publication.
    dependency_tool_permit: bool,
    // Armed only around one preflighted completion transaction publication.
    pending_completion: Option<CompletionAccepted>,
    // Replay evidence is not live authority. Neither reference is restored.
    live_clipboard: Option<RecordedEventRef>,
    pending_paste: Option<RecordedEventRef>,
}
impl Drop for Authority {
    fn drop(&mut self) {
        // Retain the ownership lock until every accepted worker job finishes.
        // Explicit quit reports shutdown errors; implicit cleanup preserves the
        // journal for validation on the next open.
        if let Some(writer) = self.writer.take() {
            let _ = writer.shutdown();
        }
    }
}
impl Authority {
    fn healthy(&self) -> Result<()> {
        if let Some(reason) = &self.poison {
            return Err(format!("recovery required: {reason}").into());
        }
        self.owner.verify()?;
        if self.replay.as_ref().is_some_and(ReplayEngine::is_terminal) {
            return Err("session is terminal; preserve and start a linked session".into());
        }
        Ok(())
    }
    fn now(&self) -> u64 {
        self.offset
            .saturating_add(
                self.started
                    .elapsed()
                    .as_millis()
                    .min(u128::from(MAX_MONOTONIC_MILLIS)) as u64,
            )
            .min(MAX_MONOTONIC_MILLIS)
    }
    fn fail<T>(&mut self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result
            && self.poison.is_none()
        {
            self.poison = Some(error.to_string());
        }
        result
    }
    fn storage_bytes(&self) -> Result<u64> {
        self.owner.verify()?;
        let mut total = 0_u64;
        for entry in fs::read_dir(
            self.owner
                .display_path()
                .parent()
                .ok_or("missing state directory")?,
        )? {
            let entry = entry?;
            let stat = entry.metadata()?;
            if !stat.is_file() || entry.file_type()?.is_symlink() {
                return Err("unexpected entry in controlled state directory".into());
            }
            total = total
                .checked_add(stat.len())
                .ok_or("storage count overflow")?;
        }
        Ok(total)
    }
    fn headroom(&self, next_bytes: u64) -> Result<()> {
        self.headroom_for_events(next_bytes, 1)
    }
    fn headroom_for_events(&self, next_bytes: u64, event_count: u64) -> Result<()> {
        if self.sequence.saturating_add(event_count).saturating_add(1) >= self.budgets.events {
            return Err("event budget exhausted; preserve session for inspection".into());
        }
        if self
            .storage_bytes()?
            .saturating_add(next_bytes)
            .saturating_add(self.budgets.reserve_bytes)
            > self.budgets.storage_bytes
        {
            return Err(
                "journal/evidence storage budget exhausted; preserve session for inspection".into(),
            );
        }
        if self.owner.available_storage_bytes()?
            < next_bytes.saturating_add(self.budgets.reserve_bytes)
        {
            return Err(
                "insufficient storage headroom for checkpoint/recovery/export reserve".into(),
            );
        }
        Ok(())
    }
    fn append(&mut self, event: Event) -> Result<()> {
        let result = (|| {
            self.complete_checkpoint(true)?;
            self.healthy()?;
            match &event {
                Event::ClipboardCopied(source) => {
                    self.replay
                        .as_ref()
                        .ok_or("missing genesis")?
                        .validate_clipboard_source(source)?;
                }
                Event::InternalPaste(paste) => {
                    self.replay
                        .as_ref()
                        .ok_or("missing genesis")?
                        .validate_internal_paste(paste)?;
                }
                _ => {}
            }
            self.headroom(MAX_ENVELOPE_BYTES as u64 * 3)?;
            let millis = self.now();
            let prevalidated_finish = if matches!(&event, Event::ControlledCommandFinished(_)) {
                let envelope = EventEnvelope {
                    format_version: 1,
                    session_id: self.metadata.session_id.clone(),
                    sequence: self.sequence + 1,
                    monotonic_millis: millis,
                    wall_clock_utc: None,
                    previous_event_hash: Hash::zero(),
                    event_hash: Hash::zero(),
                    event: event.clone(),
                }
                .seal(self.hash)?;
                let mut replay = self.replay.as_ref().ok_or("missing genesis")?.clone();
                replay.apply(&envelope)?;
                Some((envelope, replay))
            } else {
                None
            };
            let receipt = self
                .writer
                .as_ref()
                .ok_or("writer closed")?
                .try_submit_event(EventSubmission {
                    session_id: self.metadata.session_id.clone(),
                    monotonic_millis: millis,
                    wall_clock_utc: None,
                    event: event.clone(),
                })?;
            let completion = receipt.wait()?;
            self.owner.verify()?;
            let (sequence, event_hash) = persisted(completion)?;
            if matches!(
                event,
                Event::FileEdited(_)
                    | Event::InternalPaste(_)
                    | Event::FileCreated(_)
                    | Event::FileDeleted(_)
                    | Event::FileRenamed(_)
            ) {
                process_probe("intent");
            }
            if let Some((envelope, replay)) = prevalidated_finish {
                if sequence != envelope.sequence || event_hash != envelope.event_hash {
                    return Err("persisted finish identity differs from prevalidation".into());
                }
                self.replay = Some(replay);
            } else {
                let envelope = EventEnvelope {
                    format_version: 1,
                    session_id: self.metadata.session_id.clone(),
                    sequence,
                    monotonic_millis: millis,
                    wall_clock_utc: None,
                    previous_event_hash: self.hash,
                    event_hash,
                    event: event.clone(),
                };
                self.replay
                    .as_mut()
                    .ok_or("missing genesis")?
                    .apply(&envelope)?;
            }
            self.sequence = sequence;
            self.hash = event_hash;
            self.persisted_millis = millis;
            if matches!(event, Event::FileEdited(_) | Event::InternalPaste(_)) {
                self.schedule.record_edit_events(1);
            }
            if matches!(
                event,
                Event::FileEdited(_)
                    | Event::InternalPaste(_)
                    | Event::FileCreated(_)
                    | Event::FileDeleted(_)
                    | Event::FileRenamed(_)
                    | Event::FileFocused(_)
                    | Event::SelectionChanged(_)
                    | Event::ExternalFileChange(_)
            ) {
                self.changed = true;
            }
            if matches!(
                event,
                Event::SessionResumed(_) | Event::SessionEnded(_) | Event::SubmissionFinalized(_)
            ) {
                self.live_clipboard = None;
                self.pending_paste = None;
            }
            Ok(())
        })();
        self.fail(result)
    }
    /// Prevalidate and persist a compared finish as one bounded durable step.
    /// Live replay and its cursor remain at the prefix until the pair is acked.
    fn append_compared_finish(
        &mut self,
        finish: ControlledCommandFinished,
        comparison: TestCaseCompared,
    ) -> Result<()> {
        let result = (|| {
            self.complete_checkpoint(true)?;
            self.healthy()?;
            self.headroom_for_events(MAX_ENVELOPE_BYTES as u64 * 6, 2)?;
            let millis = self.now();
            let first = EventEnvelope {
                format_version: FORMAT_VERSION_V1,
                session_id: self.metadata.session_id.clone(),
                sequence: self
                    .sequence
                    .checked_add(1)
                    .ok_or("finish sequence overflow")?,
                monotonic_millis: millis,
                wall_clock_utc: None,
                previous_event_hash: Hash::zero(),
                event_hash: Hash::zero(),
                event: Event::ControlledCommandFinished(finish),
            }
            .seal(self.hash)?;
            let second = EventEnvelope {
                format_version: FORMAT_VERSION_V1,
                session_id: self.metadata.session_id.clone(),
                sequence: first
                    .sequence
                    .checked_add(1)
                    .ok_or("comparison sequence overflow")?,
                monotonic_millis: millis,
                wall_clock_utc: None,
                previous_event_hash: Hash::zero(),
                event_hash: Hash::zero(),
                event: Event::TestCaseCompared(comparison),
            }
            .seal(first.event_hash)?;
            let mut replay = self.replay.as_ref().ok_or("missing genesis")?.clone();
            replay.apply(&first)?;
            replay.apply(&second)?;
            let expected_sequence = second.sequence;
            let expected_hash = second.event_hash;
            let submissions = [first, second].map(|envelope| EventSubmission {
                session_id: envelope.session_id,
                monotonic_millis: envelope.monotonic_millis,
                wall_clock_utc: envelope.wall_clock_utc,
                event: envelope.event,
            });
            let receipt = self
                .writer
                .as_ref()
                .ok_or("writer closed")?
                .try_submit(JournalWriteJob::EventPair(submissions))?;
            let attempt = receipt.attempt_id();
            let completion = receipt.wait()?;
            self.owner.verify()?;
            if completion.attempt_id() != attempt || completion.kind() != JournalWriteKind::Event {
                return Err(
                    "compared finish acknowledgement identity differs from submission".into(),
                );
            }
            let (sequence, event_hash) = persisted(completion)?;
            if sequence != expected_sequence || event_hash != expected_hash {
                return Err("persisted compared finish identity differs from prevalidation".into());
            }
            // Only the committed, identity-checked pair becomes live authority.
            self.replay = Some(replay);
            self.sequence = sequence;
            self.hash = event_hash;
            self.persisted_millis = millis;
            Ok(())
        })();
        self.fail(result)
    }
    fn reject_paste(
        &mut self,
        channel: PasteInputChannel,
        reason: PasteRejectionReason,
    ) -> Result<()> {
        if self.command_active {
            return Err("command owns workspace; clipboard input was discarded".into());
        }
        self.append(Event::PasteRejected(PasteRejected { reason, channel }))
            .map_err(|error| {
                format!("{PASTE_BLOCKED_WARNING} Recording stopped; preserve session: {error}")
                    .into()
            })
    }
    fn checkpoint(&mut self, input: CheckpointInput, boundary: bool) -> Result<bool> {
        let result = (|| {
            self.complete_checkpoint(boundary)?;
            self.healthy()?;
            let millis = self.now();
            let now = Duration::from_millis(millis);
            if self.sequence != 0
                && !boundary
                && (!self.changed
                    || !self
                        .schedule
                        .should_submit(now, CheckpointTrigger::Activity))
            {
                return Ok(false);
            }
            self.headroom(34 * 1024 * 1024)?;
            let expected = CheckpointSnapshot::from_input(input.clone(), self.sequence + 1)?;
            let receipt = self
                .writer
                .as_ref()
                .ok_or("writer closed")?
                .try_submit_checkpoint(CheckpointSubmission {
                    monotonic_millis: millis,
                    wall_clock_utc: None,
                    input,
                })?;
            self.schedule.checkpoint_queued(now, receipt.attempt_id())?;
            self.pending_checkpoint = Some(PendingCheckpoint {
                receipt,
                snapshot: expected,
                millis,
            });
            if boundary {
                self.complete_checkpoint(true)?;
            }
            Ok(true)
        })();
        self.fail(result)
    }

    fn complete_checkpoint(&mut self, wait: bool) -> Result<()> {
        let result = (|| {
            let Some(pending) = self.pending_checkpoint.take() else {
                return Ok(());
            };
            let result = if wait {
                pending.receipt.wait()
            } else {
                match pending.receipt.try_recv() {
                    Ok(Some(completion)) => Ok(completion),
                    Ok(None) => {
                        self.pending_checkpoint = Some(pending);
                        return Ok(());
                    }
                    Err(error) => Err(error),
                }
            };
            self.schedule.apply_receipt_result(result.clone())?;
            let (sequence, event_hash) = persisted(result?)?;
            if sequence > 1 {
                process_probe("checkpoint");
            }
            self.owner.verify()?;
            let envelope = EventEnvelope {
                format_version: 1,
                session_id: self.metadata.session_id.clone(),
                sequence,
                monotonic_millis: pending.millis,
                wall_clock_utc: None,
                previous_event_hash: self.hash,
                event_hash,
                event: Event::WorkspaceCheckpoint(pending.snapshot.event_payload()),
            };
            if let Some(replay) = &mut self.replay {
                replay.apply(&envelope)?;
            } else {
                self.replay = Some(ReplayEngine::from_initial_checkpoint(StoredCheckpoint {
                    owning_event: envelope,
                    snapshot: pending.snapshot,
                })?);
            }
            self.sequence = sequence;
            self.hash = event_hash;
            self.persisted_millis = pending.millis;
            self.changed = false;
            Ok(())
        })();
        self.fail(result)
    }
}
fn persisted(completion: JournalWriteCompletion) -> Result<(u64, Hash)> {
    match completion {
        JournalWriteCompletion::Persisted {
            sequence,
            event_hash,
            ..
        } => Ok((sequence, event_hash)),
        JournalWriteCompletion::Failed { detail, .. } => Err(detail.into()),
    }
}
impl EditorEffects for SessionEffects {
    fn check_input_origin(
        &mut self,
        origin: EditOrigin,
    ) -> std::result::Result<(), EditorEffectError> {
        let mut authority = self.0.borrow_mut();
        if origin == EditOrigin::Completion {
            return (!authority.command_active && authority.pending_completion.is_some())
                .then_some(())
                .ok_or_else(|| {
                    EditorEffectError::new(
                        "completion edit lacks a live preflighted completion permit",
                    )
                });
        }
        if origin == EditOrigin::Formatter {
            return (authority.command_active && authority.formatter_permit)
                .then_some(())
                .ok_or_else(|| {
                    EditorEffectError::new("formatter edit lacks controlled Format ownership")
                });
        }
        if origin == EditOrigin::DependencyTool {
            return (authority.command_active && authority.dependency_tool_permit)
                .then_some(())
                .ok_or_else(|| {
                    EditorEffectError::new(
                        "dependency edit lacks controlled dependency command ownership",
                    )
                });
        }
        if !matches!(origin, EditOrigin::Paste | EditOrigin::Unknown)
            || (origin == EditOrigin::Paste && authority.pending_paste.is_some())
        {
            return Ok(());
        }
        authority
            .reject_paste(
                PasteInputChannel::Programmatic,
                PasteRejectionReason::UnverifiableInput,
            )
            .map_err(|error| EditorEffectError::new(error.to_string()))?;
        Err(EditorEffectError::new(PASTE_BLOCKED_WARNING))
    }

    fn record_provenance(
        &mut self,
        transaction: &EditorTransaction,
    ) -> std::result::Result<(), EditorEffectError> {
        let controlled_formatter =
            transaction.origin == EditOrigin::Formatter && self.0.borrow().formatter_permit;
        let controlled_dependency = transaction.origin == EditOrigin::DependencyTool
            && self.0.borrow().dependency_tool_permit;
        let controlled_completion = transaction.origin == EditOrigin::Completion
            && self.0.borrow().pending_completion.is_some();
        if self.0.borrow().command_active && !controlled_formatter && !controlled_dependency {
            return Err(EditorEffectError::new("command owns workspace"));
        }
        if transaction.origin == EditOrigin::Formatter
            && (!self.0.borrow().command_active || !controlled_formatter)
        {
            return Err(EditorEffectError::new(
                "formatter edit lacks controlled Format ownership",
            ));
        }
        if transaction.origin == EditOrigin::DependencyTool
            && (!self.0.borrow().command_active || !controlled_dependency)
        {
            return Err(EditorEffectError::new(
                "dependency edit lacks controlled dependency command ownership",
            ));
        }
        if transaction.origin == EditOrigin::Completion && !controlled_completion {
            return Err(EditorEffectError::new(
                "completion edit lacks a live preflighted completion permit",
            ));
        }
        let mut authority = self.0.borrow_mut();
        let result = (|| {
            let event = match transaction.origin {
                EditOrigin::Paste => {
                    let Some(source) = authority.pending_paste.take() else {
                        authority.reject_paste(
                            PasteInputChannel::Programmatic,
                            PasteRejectionReason::UnverifiableInput,
                        )?;
                        return Err(PASTE_BLOCKED_WARNING.into());
                    };
                    Event::InternalPaste(InternalPaste {
                        source,
                        transaction: transaction.clone(),
                    })
                }
                EditOrigin::Unknown => {
                    authority.reject_paste(
                        PasteInputChannel::Programmatic,
                        PasteRejectionReason::UnverifiableInput,
                    )?;
                    return Err(PASTE_BLOCKED_WARNING.into());
                }
                EditOrigin::Completion => {
                    let accepted = authority
                        .pending_completion
                        .take()
                        .ok_or("completion edit lacks accepted metadata")?;
                    if accepted.document_id != transaction.document_id
                        || accepted.document_version != transaction.version_before
                        || !accepted.additional_edits.is_empty()
                        || transaction.edits.as_slice()
                            != std::slice::from_ref(&accepted.primary_edit)
                    {
                        return Err("completion metadata/transaction mismatch".into());
                    }
                    authority.append(Event::LspCompletionAccepted(accepted))?;
                    Event::FileEdited(transaction.clone())
                }
                _ => Event::FileEdited(transaction.clone()),
            };
            authority.append(event)
        })();
        result.map_err(|e: Box<dyn Error>| EditorEffectError::new(e.to_string()))
    }
}
impl WorkspaceEffects for SessionEffects {
    fn command_active(&self) -> bool {
        self.0.borrow().command_active
    }

    fn check_editor_command(
        &mut self,
        command: &EditorCommand,
    ) -> std::result::Result<(), WorkspaceEffectError> {
        let mut authority = self.0.borrow_mut();
        // Only the synchronous ProductionSession paste operation can arm this
        // single-use permit. A caller-supplied Paste/Unknown origin cannot.
        let reason = match command {
            EditorCommand::PasteExternal(_) if authority.pending_paste.is_some() => return Ok(()),
            EditorCommand::PasteExternal(_) => PasteRejectionReason::ExternalInput,
            EditorCommand::Paste | EditorCommand::Copy | EditorCommand::Cut => {
                PasteRejectionReason::UnverifiableInput
            }
            _ => return Ok(()),
        };
        authority
            .reject_paste(PasteInputChannel::ProductionCommand, reason)
            .map_err(|error| WorkspaceEffectError::new(error.to_string()))?;
        Err(WorkspaceEffectError::new(PASTE_BLOCKED_WARNING))
    }
    fn recovery_reason(&self) -> Option<String> {
        let a = self.0.borrow();
        a.poison
            .clone()
            .or_else(|| a.owner.verify().err().map(|e| e.to_string()))
    }
    fn record_workspace_event(
        &mut self,
        event: Event,
    ) -> std::result::Result<(), WorkspaceEffectError> {
        if self.0.borrow().command_active {
            return Err(WorkspaceEffectError::new("command owns workspace"));
        }
        self.0
            .borrow_mut()
            .append(event)
            .map_err(|e| WorkspaceEffectError::new(e.to_string()))
    }
    fn record_lifecycle(&mut self, event: Event) -> std::result::Result<(), WorkspaceEffectError> {
        self.record_workspace_event(event)
    }
}

#[derive(Clone, Debug)]
pub struct SessionHealth {
    pub events: u64,
    pub checkpoint_pending: bool,
    pub storage_bytes: u64,
    pub storage_headroom: u64,
    pub warning: bool,
    pub undo_bytes: usize,
    pub undo_limited: bool,
    pub recovery_reason: Option<String>,
}

pub struct ProductionSession {
    workspace: WorkspaceSession<SessionEffects>,
    effects: SessionEffects,
    metadata: SessionMetadata,
    evidence_paths: Vec<PathBuf>,
    last_save: Instant,
    undo_limited: bool,
    runtime_toolchain: Option<crate::toolchain::ToolchainReport>,
    external: Option<RecoveryEvidence>,
    external_blocker: Option<String>,
    last_recheck: Instant,
    external_warning: bool,
    pending_evidence: Option<Hash>,
    command: command::CommandState,
    language_service: Option<crate::language_service::LanguageService>,
    pending_completion_request: Option<PendingCompletionRequest>,
    completion: Option<CompletionState>,
    automatic_completion_requests: AutomaticCompletionRequests,
    live_diagnostic_generation: Option<u64>,
    live_diagnostics: BTreeMap<DocumentId, LiveDocumentDiagnostics>,
    selected_live_diagnostic: Option<(DocumentId, usize)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LiveDocumentDiagnostics {
    generation: u64,
    document_id: DocumentId,
    path: WorkspacePath,
    version: u64,
    diagnostics: Vec<crate::editor::LiveDiagnosticSpan>,
}

#[derive(Clone, Debug)]
struct PendingCompletionRequest {
    session_id: SessionId,
    workspace_root: PathBuf,
    document_hash: Hash,
    selection: SelectionState,
    request: crate::language_service::CompletionRequest,
    requires_service: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompletionCandidate {
    pub label: String,
    pub kind: Option<u8>,
    pub(crate) edit: TextEdit,
}

#[derive(Clone, Debug)]
struct CompletionState {
    context: PendingCompletionRequest,
    items: Vec<CompletionCandidate>,
    selected: usize,
}

#[derive(Clone, Debug)]
struct AutomaticCompletionRequest {
    request_sequence: u64,
    deadline: Instant,
    deferred: bool,
}

#[derive(Clone, Debug, Default)]
struct AutomaticCompletionRequests {
    in_flight: BTreeMap<DocumentId, AutomaticCompletionRequest>,
}

impl AutomaticCompletionRequests {
    fn request(&mut self, document_id: &DocumentId) -> bool {
        let Some(request) = self.in_flight.get_mut(document_id) else {
            return true;
        };
        request.deferred = true;
        false
    }

    fn started(&mut self, document_id: DocumentId, request_sequence: u64, deadline: Instant) {
        self.in_flight.insert(
            document_id,
            AutomaticCompletionRequest {
                request_sequence,
                deadline,
                deferred: false,
            },
        );
    }

    fn cancel_deferred(&mut self) {
        for request in self.in_flight.values_mut() {
            request.deferred = false;
        }
    }

    fn completed(&mut self, document_id: &DocumentId, request_sequence: u64) -> bool {
        let matches = self
            .in_flight
            .get(document_id)
            .is_some_and(|request| request.request_sequence == request_sequence);
        if !matches {
            return false;
        }
        self.in_flight
            .remove(document_id)
            .is_some_and(|request| request.deferred)
    }

    fn expire(&mut self, now: Instant) -> Vec<DocumentId> {
        let expired = self
            .in_flight
            .iter()
            .filter(|(_, request)| now >= request.deadline)
            .map(|(document_id, _)| document_id.clone())
            .collect::<Vec<_>>();
        expired
            .into_iter()
            .filter(|document_id| {
                self.in_flight
                    .remove(document_id)
                    .is_some_and(|request| request.deferred)
            })
            .collect()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.in_flight.len()
    }
}
impl ProductionSession {
    pub fn read_metadata(root: &Path) -> Result<SessionMetadata> {
        let metadata: SessionMetadata = serde_json::from_slice(&read_initial_metadata(root)?)?;
        if metadata.version != 1 {
            return Err("unsupported session metadata version".into());
        }
        Ok(metadata)
    }

    pub(crate) fn selected_assignment_matches(
        root: &Path,
        metadata: &SessionMetadata,
        manifest_bytes: &[u8],
        original_starter_tree_hash: Hash,
        test_case_suite_hash: Option<Hash>,
    ) -> Result<bool> {
        finalization::selected_assignment_matches(
            root,
            metadata,
            manifest_bytes,
            original_starter_tree_hash,
            test_case_suite_hash,
        )
    }

    pub fn start(root: &Path, manifest_bytes: &[u8]) -> Result<Self> {
        Self::start_linked(root, manifest_bytes, None, None)
    }

    pub fn start_from_assignment(root: &Path, assignment: &ExtractedAssignment) -> Result<Self> {
        Self::start_linked(
            root,
            &assignment.manifest_bytes,
            None,
            assignment.test_cases.as_ref().map(|suite| suite.hash),
        )
    }

    fn start_linked(
        root: &Path,
        manifest_bytes: &[u8],
        link: Option<Vec<u8>>,
        test_case_suite_hash: Option<Hash>,
    ) -> Result<Self> {
        let manifest = AssignmentManifest::parse(manifest_bytes)?;
        require_test_case_suite_identity(&manifest, test_case_suite_hash)?;
        let pinned = PinnedWorkspaceRoot::open(root)?;
        if root.join(".rustrace").try_exists()? {
            return Err(
                "existing session or incomplete startup is preserved: use --inspect, or start a new workspace with --workspace; never restart initialization in this directory"
                    .into(),
            );
        }
        let id = SessionId::new(format!(
            "session-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ))?;
        let mut owner = pinned.open_state_directory()?.create_journal_file(&id)?;
        process_probe("startup-journal");
        owner.secure_reserve(SessionBudgets::default().reserve_bytes, true)?;
        process_probe("startup-reserve");
        let starter = read_pinned_workspace(&pinned)?;
        let starter_hash = hash_entries(starter.iter().map(|(p, b)| (p, b.as_slice())))?;
        let version = crate::version::version_metadata();
        let metadata = SessionMetadata {
            version: 1,
            session_id: id.clone(),
            course_id: manifest.course_id.clone(),
            assignment_id: manifest.assignment_id.clone(),
            assignment_version: manifest.assignment_version.clone(),
            manifest_hash: digest(manifest_bytes),
            starter_hash,
            test_case_suite_hash,
            client_version: version.client_version.into(),
            build_identity: version.build_identity.into(),
            elapsed_time: "process-monotonic; offline intervals excluded/unknown".into(),
            parent_evidence: link.as_deref().map(digest),
        };
        if let Some(bytes) = &link {
            owner.publish_artifact("parent.json", bytes, false)?;
        }
        owner.publish_artifact("manifest.toml", manifest_bytes, false)?;
        process_probe("startup-manifest");
        owner.publish_artifact("session.json", &serde_json::to_vec(&metadata)?, false)?;
        process_probe("startup-metadata");
        let mut journal = Journal::open_retained_no_follow(owner.display_path())?;
        journal.create_or_resume_session(&id)?;
        owner.pin_live_sidecars()?;
        let effects = make_effects(owner, journal, metadata.clone(), None, 0, 0, Hash::zero())?;
        let workspace = WorkspaceSession::open(root, &manifest, None, effects.clone())?;
        let genesis = workspace.checkpoint_input(id.clone())?;
        // Initial saved bytes are durable before genesis; interruption at its
        // receipt can therefore recover even before the first compact receipt.
        let baseline = files_snapshot(&id, 1, &starter)?;
        effects.0.borrow().owner.publish_artifact(
            "baseline.bin",
            &encode_checkpoint(&baseline)?,
            false,
        )?;
        process_probe("startup-baseline");
        effects.0.borrow_mut().checkpoint(genesis, true)?;
        process_probe("startup-genesis");
        let mut session = Self {
            workspace,
            effects,
            metadata,
            evidence_paths: Vec::new(),
            last_save: Instant::now(),
            undo_limited: false,
            runtime_toolchain: None,
            external: None,
            external_blocker: None,
            last_recheck: Instant::now(),
            external_warning: false,
            pending_evidence: None,
            command: command::CommandState::default(),
            language_service: None,
            pending_completion_request: None,
            completion: None,
            automatic_completion_requests: AutomaticCompletionRequests::default(),
            live_diagnostic_generation: None,
            live_diagnostics: BTreeMap::new(),
            selected_live_diagnostic: None,
        };
        session.persist_baseline()?;
        Ok(session)
    }
    pub fn resume(root: &Path, manifest_bytes: &[u8], choice: ResumeChoice) -> Result<Self> {
        Self::resume_inner(root, manifest_bytes, None, choice)
    }

    pub(crate) fn resume_selected_assignment(
        root: &Path,
        manifest_bytes: &[u8],
        test_case_suite_hash: Option<Hash>,
        choice: ResumeChoice,
    ) -> Result<Self> {
        Self::resume_inner(root, manifest_bytes, Some(test_case_suite_hash), choice)
    }

    fn resume_inner(
        root: &Path,
        manifest_bytes: &[u8],
        selected_test_case_suite_hash: Option<Option<Hash>>,
        choice: ResumeChoice,
    ) -> Result<Self> {
        let manifest = AssignmentManifest::parse(manifest_bytes)?;
        let metadata: SessionMetadata = serde_json::from_slice(&read_initial_metadata(root)?)?;
        require_test_case_suite_identity(&manifest, metadata.test_case_suite_hash)?;
        if metadata.version != 1
            || metadata.manifest_hash != digest(manifest_bytes)
            || selected_test_case_suite_hash
                .is_some_and(|selected| metadata.test_case_suite_hash != selected)
        {
            return Err("assignment manifest identity mismatch; inspect preserved session".into());
        }
        let pinned = PinnedWorkspaceRoot::open(root)?;
        let mut owner = pinned
            .open_state_directory()?
            .open_journal_file(&metadata.session_id)?;
        let state_directory = owner
            .display_path()
            .parent()
            .ok_or("state directory missing")?;
        for name in [
            "finalization-events.jsonl",
            "finalization-prefix.jsonl",
            "finalization-prepared.json",
            "finalization-recovery-capture.json",
        ] {
            match fs::symlink_metadata(state_directory.join(name)) {
                Ok(_) => {
                    return Err(
                        "immutable finalization capture has started; preserve it and start a linked attempt with `rustrace revise PARENT_WORKSPACE NEW_WORKSPACE assignment.rta`"
                            .into(),
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let stale_command = command::stale_command_marker(&owner, &metadata)?;
        owner.secure_reserve(SessionBudgets::default().reserve_bytes, false)?;
        if owner.read_artifact("manifest.toml", METADATA_LIMIT)? != manifest_bytes {
            return Err("persisted assignment manifest mismatch".into());
        }
        let abandoned = owner
            .display_path()
            .parent()
            .ok_or("state directory missing")?
            .join("abandoned.json")
            .try_exists()?;
        // An abandoned original that cannot be validated stays closed; point
        // to the recovery workspace the abandonment created.
        let note = |error: Box<dyn Error>| -> Box<dyn Error> {
            if abandoned {
                format!("{error}; this workspace was abandoned: continue in the recovery workspace named in .rustrace/abandoned.json").into()
            } else {
                error
            }
        };
        if let Some(parent_hash) = metadata.parent_evidence
            && digest(&owner.read_artifact("parent.json", METADATA_LIMIT)?) != parent_hash
        {
            return Err("linked recovery evidence mismatch".into());
        }
        // Validate preserved bytes read-only before SQLite can initialize an invalid original.
        let mut journal =
            Journal::open_read_only_no_follow(owner.display_path()).map_err(|e| note(e.into()))?;
        let receipt = load_saved_receipt(&owner, &mut journal, &metadata).map_err(note)?;
        let ValidatedPrefix {
            replay,
            sequence,
            millis,
            hash,
            saved,
            checkpoint_millis,
            uncaptured_edits,
            changed,
            pending_external,
        } = validate_prefix(&mut journal, &metadata, &receipt, &owner).map_err(note)?;
        if replay.is_terminal() || journal.inspect_session(&metadata.session_id)?.ended {
            return Err(
                "terminal session is preserved; start a linked attempt with `rustrace revise PARENT_WORKSPACE NEW_WORKSPACE assignment.rta`".into(),
            );
        }
        // A stale activity marker proves, through the inherited writer lock we
        // now hold, that every process of the interrupted command has exited.
        if replay.controlled_command_pending() && !stale_command {
            return Err("unfinished command evidence; inspect and use linked recovery; child death is not established by restart".into());
        }
        let interrupted_command = replay.controlled_command_pending();
        let reclaim = if abandoned {
            Some(prepare_reclaim(&owner, &metadata)?)
        } else {
            None
        };
        let logical = replay.workspace_state().files().clone();
        let disk = read_pinned_workspace(&pinned)?;
        let policy = AllowedPathSet::from_manifest(&manifest)?;
        for path in logical.keys() {
            policy.validate(path)?;
        }
        let diverged = disk != saved || disk.keys().ne(logical.keys());
        // The intent authorizes the exact durable logical target. An interrupted
        // multi-file save can leave a checked mixture of saved A and logical B;
        // no other bytes or presence changes are attributable to this writer.
        let owned_publication = disk.keys().eq(saved.keys())
            && disk.keys().eq(logical.keys())
            && disk.iter().all(|(path, bytes)| {
                saved.get(path) == Some(bytes) || logical.get(path) == Some(bytes)
            });
        let own_save_pending = if pending_external.is_none() && owned_publication {
            let name = "save-intent.json";
            match fs::symlink_metadata(
                owner
                    .display_path()
                    .parent()
                    .ok_or("state directory")?
                    .join(name),
            ) {
                Ok(_) => {
                    let intent: SavedReceipt =
                        serde_json::from_slice(&owner.read_artifact(name, METADATA_LIMIT)?)?;
                    intent.version == 1
                        && intent.session_id == metadata.session_id
                        && intent.sequence == sequence
                        && intent.event_hash == hash
                        && intent.workspace_hash == replay.workspace_state().workspace_hash()
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => return Err(e.into()),
            }
        } else {
            false
        };
        let _ = choice; // Resume is automatic; the choice is kept for callers.
        drop(journal);
        owner.verify()?;
        let journal = Journal::open_retained_no_follow(owner.display_path())?;
        owner.pin_live_sidecars()?;
        let effects = make_effects(
            owner,
            journal,
            metadata.clone(),
            Some(replay),
            sequence,
            millis,
            hash,
        )?;
        {
            let mut authority = effects.0.borrow_mut();
            authority.schedule = CheckpointScheduleState::new(
                CheckpointPolicy::new(Duration::from_secs(30), 100)?,
                Duration::from_millis(checkpoint_millis),
            );
            authority.schedule.record_edit_events(uncaptured_edits);
            authority.changed = changed;
        }
        let recovered = snapshot_from_replay(
            &metadata.session_id,
            sequence,
            effects.0.borrow().replay.as_ref().ok_or("missing replay")?,
        )?;
        let workspace_baseline = &saved;
        let workspace = WorkspaceSession::from_recovered(
            root,
            &manifest,
            &recovered,
            workspace_baseline,
            effects.clone(),
        )?;
        let mut session = Self {
            workspace,
            effects,
            metadata,
            evidence_paths: Vec::new(),
            last_save: Instant::now(),
            undo_limited: false,
            runtime_toolchain: None,
            external: None,
            external_blocker: None,
            last_recheck: Instant::now(),
            external_warning: false,
            pending_evidence: None,
            command: command::CommandState::default(),
            language_service: None,
            pending_completion_request: None,
            completion: None,
            automatic_completion_requests: AutomaticCompletionRequests::default(),
            live_diagnostic_generation: None,
            live_diagnostics: BTreeMap::new(),
            selected_live_diagnostic: None,
        };
        if own_save_pending {
            session.effects.0.borrow().headroom(24 * 1024 * 1024)?;
            // The existing durable intent remains valid until publication and
            // baseline complete. Recheck before each write, without appending a
            // resume event that would invalidate that intent on interruption.
            restore_disk(session.workspace.root_authority(), &logical, &disk)?;
            session.workspace.adopt_verified_baseline(&logical)?;
            session.persist_baseline()?;
        } else if let Some(evidence_hash) = pending_external {
            let name = format!("evidence-{evidence_hash}.bin");
            let a = session.effects.0.borrow();
            session.external = Some(read_recovery_evidence(
                &a.owner.read_artifact(&name, ARTIFACT_LIMIT)?,
            )?);
            session.evidence_paths.push(
                a.owner
                    .display_path()
                    .parent()
                    .ok_or("state directory")?
                    .join(name),
            );
            session.pending_evidence = Some(evidence_hash);
        } else if diverged {
            session.external = Some(RecoveryEvidence {
                saved,
                logical,
                disk,
            });
        }
        if let Some(reclaim) = reclaim {
            finish_reclaim(&session.effects.0.borrow().owner, reclaim)?;
        }
        if interrupted_command {
            session.finish_interrupted_command()?;
        } else if stale_command {
            // Killed while preparing tools: no command started.
            session.record_command_activity(false)?;
        }
        let last_sequence = session.effects.0.borrow().sequence;
        session
            .effects
            .0
            .borrow_mut()
            .append(Event::SessionResumed(SessionResumed { last_sequence }))?;
        process_probe("resume");
        if session.external.is_some() {
            session.recheck_external()?;
        }
        Ok(session)
    }
    /// Validate and expose exact bounded views without enabling source mutation.
    pub fn inspect(root: &Path) -> Result<RecoveryEvidence> {
        let metadata: SessionMetadata = serde_json::from_slice(&read_initial_metadata(root)?)?;
        let pinned = PinnedWorkspaceRoot::open(root)?;
        let mut owner = pinned
            .open_state_directory()?
            .open_journal_file(&metadata.session_id)?;
        let mut journal = Journal::open_read_only_no_follow(owner.display_path())?;
        let receipt = load_saved_receipt(&owner, &mut journal, &metadata)?;
        let prefix = validate_prefix(&mut journal, &metadata, &receipt, &owner)?;
        let evidence = RecoveryEvidence {
            saved: prefix.saved,
            logical: prefix.replay.workspace_state().files().clone(),
            disk: read_pinned_workspace(&pinned)?,
        };
        drop(journal);
        owner.verify()?;
        owner.release_ownership()?;
        Ok(evidence)
    }

    /// Preserve an unusable original in place and start explicitly linked work.
    /// The caller prepares the new bounded starter directory by validated extraction.
    /// The CLI no longer offers this (`--abandon` was removed); it remains to
    /// build workspaces abandoned by older versions, which resume reclaims.
    pub fn abandon_into(root: &Path, fresh_root: &Path, manifest_bytes: &[u8]) -> Result<Self> {
        Self::abandon_with(root, manifest_bytes, None, || Ok(fresh_root.to_path_buf()))
    }

    /// Check and own the original before invoking validated fresh extraction.
    /// Preflight rejection never invokes the callback or creates a recovery sibling.
    pub(crate) fn abandon_with(
        root: &Path,
        manifest_bytes: &[u8],
        test_case_suite_hash: Option<Hash>,
        extract_fresh: impl FnOnce() -> Result<std::path::PathBuf>,
    ) -> Result<Self> {
        let pinned = PinnedWorkspaceRoot::open(root)?;
        let mut owner = pinned.open_state_directory()?.lock_for_inspection()?;
        let mut evidence = preserved_evidence(&pinned, &owner)?;
        if evidence["state_files"]
            .as_array()
            .is_some_and(|files| files.iter().any(|file| file["name"] == "abandoned.json"))
        {
            return Err("original already preserved and linked; use --inspect and abandoned.json to locate the recovery workspace".into());
        }
        verify_available_manifest(&evidence, manifest_bytes)?;
        let declared = Self::read_metadata(root).ok();
        if declared
            .as_ref()
            .is_some_and(|m| m.manifest_hash != digest(manifest_bytes))
        {
            return Err("assignment manifest identity mismatch; original preserved".into());
        }
        let fresh_root = extract_fresh()?;
        let fresh = PinnedWorkspaceRoot::open(&fresh_root)?;
        let starter = read_pinned_workspace(&fresh)?;
        let starter_hash = hash_entries(starter.iter().map(|(p, b)| (p, b.as_slice())))?;
        if let Some(metadata) = &declared
            && !Self::selected_assignment_matches(
                root,
                metadata,
                manifest_bytes,
                starter_hash,
                test_case_suite_hash,
            )?
        {
            return Err("assignment starter identity mismatch; original preserved".into());
        }
        evidence["decision"] = if declared.is_some() {
            "abandon_and_preserve"
        } else {
            "abandon_incomplete_startup_and_preserve"
        }
        .into();
        evidence["new_directory"] = serde_json::to_value(fresh.path())?;
        evidence["selected_manifest_hash"] = serde_json::to_value(digest(manifest_bytes))?;
        evidence["selected_starter_hash"] = serde_json::to_value(starter_hash)?;
        let link = serde_json::to_vec(&evidence)?;
        if link.len() > METADATA_LIMIT {
            return Err("startup evidence exceeds metadata budget; original preserved".into());
        }
        let session = Self::start_linked(
            &fresh_root,
            manifest_bytes,
            Some(link.clone()),
            test_case_suite_hash,
        )?;
        owner.verify()?;
        // Preserve the existing complete-metadata abandonment marker contract.
        // The declared ID is evidence only; never open its selected SQLite file.
        if declared.is_some() {
            owner.publish_artifact("abandoned.json", &link, false)?;
        }
        session
            .effects
            .0
            .borrow_mut()
            .append(Event::RecoveryRecorded(RecoveryRecorded {
                evidence_hash: digest(&link),
                decision: RecoveryDecision::AbandonPreserved,
            }))?;
        process_probe("abandon");
        owner.release_ownership()?;
        Ok(session)
    }

    /// Inspect bounded available evidence without inventing a replay or session identity.
    pub fn inspect_preserved(root: &Path) -> Result<serde_json::Value> {
        let pinned = PinnedWorkspaceRoot::open(root)?;
        let mut owner = pinned.open_state_directory()?.lock_for_inspection()?;
        let evidence = preserved_evidence(&pinned, &owner)?;
        owner.release_ownership()?;
        Ok(evidence)
    }

    /// Lower pilot budgets for deployments/tests; never exceed the supported caps.
    pub fn set_budgets(&mut self, budgets: SessionBudgets) -> Result<()> {
        self.require_command_idle()?;
        let max = SessionBudgets::default();
        if budgets.storage_bytes > max.storage_bytes
            || budgets.events > max.events
            || budgets.undo_bytes > max.undo_bytes
            || budgets.output_per_command > max.output_per_command
            || budgets.output_per_session > max.output_per_session
            || budgets.reserve_bytes < max.reserve_bytes
        {
            return Err("budget policy exceeds pilot limits or reduces recovery reserve".into());
        }
        self.effects.0.borrow_mut().budgets = budgets;
        self.undo_limited |= self.workspace.trim_undo_to(budgets.undo_bytes);
        Ok(())
    }

    pub fn health(&self) -> Result<SessionHealth> {
        let a = self.effects.0.borrow();
        let used = a.storage_bytes()?;
        Ok(SessionHealth {
            events: a.sequence,
            checkpoint_pending: a.pending_checkpoint.is_some(),
            storage_bytes: used,
            storage_headroom: a
                .budgets
                .storage_bytes
                .saturating_sub(used)
                .saturating_sub(a.budgets.reserve_bytes),
            warning: used >= a.budgets.storage_bytes.saturating_mul(80) / 100
                || a.sequence >= a.budgets.events.saturating_mul(80) / 100,
            undo_bytes: self.workspace.retained_undo_bytes(),
            undo_limited: self.undo_limited,
            recovery_reason: self.recovery_reason(),
        })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.metadata.session_id
    }
    pub fn metadata(&self) -> &SessionMetadata {
        &self.metadata
    }

    /// Observe tools once in this controller, after validated startup/resume.
    /// Keep original metadata unchanged; the artifact describes this opening
    /// after a specific durable prefix, not tools used by historical events.
    pub fn discover_toolchain(&mut self) -> Result<&crate::toolchain::ToolchainReport> {
        self.require_command_idle()?;
        if self.runtime_toolchain.is_some() {
            return Err(
                "toolchain already observed in this controller; reopen to probe changes".into(),
            );
        }
        let result = (|| {
            self.workspace.root_authority().verify_binding()?;
            let manifest = {
                let mut authority = self.effects.0.borrow_mut();
                authority.complete_checkpoint(true)?;
                authority.healthy()?;
                authority.headroom(METADATA_LIMIT as u64)?;
                let bytes = authority
                    .owner
                    .read_artifact("manifest.toml", METADATA_LIMIT)?;
                if digest(&bytes) != self.metadata.manifest_hash {
                    return Err(
                        "immutable assignment manifest changed; preserve and inspect session"
                            .into(),
                    );
                }
                AssignmentManifest::parse(&bytes)?
            };
            let original = self
                .effects
                .0
                .borrow()
                .owner
                .read_artifact("session.json", METADATA_LIMIT)?;
            if serde_json::from_slice::<SessionMetadata>(&original)? != self.metadata {
                return Err(
                    "original session metadata changed; preserve and inspect session".into(),
                );
            }
            let report =
                crate::toolchain::discover(self.workspace.root(), Some(&manifest.toolchain));
            self.workspace.root_authority().verify_binding()?;
            let authority = self.effects.0.borrow();
            authority.healthy()?;
            if authority
                .owner
                .read_artifact("session.json", METADATA_LIMIT)?
                != original
                || digest(
                    &authority
                        .owner
                        .read_artifact("manifest.toml", METADATA_LIMIT)?,
                ) != self.metadata.manifest_hash
            {
                return Err(
                    "session/assignment metadata changed during discovery; preserve and inspect"
                        .into(),
                );
            }
            let observation = crate::toolchain::RuntimeToolchainMetadata {
                version: 1,
                session_id: self.metadata.session_id.clone(),
                manifest_hash: self.metadata.manifest_hash,
                sequence: authority.sequence,
                event_hash: authority.hash,
                report: report.clone(),
            };
            let bytes = serde_json::to_vec(&observation)?;
            if bytes.len() > METADATA_LIMIT {
                return Err(
                    "toolchain metadata exceeds bounded artifact limit; preserve and inspect"
                        .into(),
                );
            }
            authority.headroom(bytes.len() as u64)?;
            authority.owner.publish_artifact(
                &format!("toolchain-{:020}.json", authority.sequence),
                &bytes,
                false,
            )?;
            Ok(report)
        })();
        self.runtime_toolchain = Some(self.effects.0.borrow_mut().fail(result)?);
        Ok(self
            .runtime_toolchain
            .as_ref()
            .expect("published toolchain observation"))
    }

    pub fn runtime_toolchain(&self) -> Option<&crate::toolchain::ToolchainReport> {
        self.runtime_toolchain.as_ref()
    }

    /// Begin optional installed-only resolution after normal startup discovery.
    pub fn start_language_service(&mut self) -> Result<()> {
        self.require_command_idle()?;
        if self.language_service.is_some() {
            return Ok(());
        }
        if self.runtime_toolchain.is_none() {
            return Err("observe required tools before starting the language service".into());
        }
        self.workspace.root_authority().verify_binding()?;
        let bytes = self
            .effects
            .0
            .borrow()
            .owner
            .read_artifact("manifest.toml", METADATA_LIMIT)?;
        if digest(&bytes) != self.metadata.manifest_hash {
            return Err("assignment identity changed before language-service start".into());
        }
        let manifest = AssignmentManifest::parse(&bytes)?;
        self.language_service = Some(crate::language_service::LanguageService::start(
            self.workspace.root().to_path_buf(),
            manifest.toolchain,
        ));
        Ok(())
    }

    pub fn language_service_status(&self) -> &'static str {
        self.language_service
            .as_ref()
            .map(crate::language_service::LanguageService::status)
            .unwrap_or("LSP off")
    }

    pub fn trigger_completion(&mut self) -> Result<bool> {
        self.trigger_completion_at(Instant::now(), false)
    }

    pub(crate) fn trigger_automatic_completion(&mut self) -> Result<bool> {
        self.trigger_completion_at(Instant::now(), true)
    }

    fn trigger_automatic_completion_at(&mut self, now: Instant) -> Result<bool> {
        self.trigger_completion_at(now, true)
    }

    fn trigger_completion_at(&mut self, now: Instant, automatic: bool) -> Result<bool> {
        self.clear_completion();
        let document = self.active_language_document();
        if automatic
            && !self
                .automatic_completion_requests
                .request(&document.document_id)
        {
            return Ok(false);
        }
        let Some(generation) = self
            .language_service
            .as_ref()
            .and_then(|service| service.completion_generation(&document))
        else {
            return Ok(false);
        };
        let Some(request) = self.prepare_completion_request_inner(generation, true)? else {
            return Ok(false);
        };
        let requested = self.language_service.as_mut().is_some_and(|service| {
            service.request_completion(
                &document,
                request.position_byte,
                request.position,
                request.request_sequence,
                now,
            )
        });
        if requested && automatic {
            self.automatic_completion_requests.started(
                document.document_id,
                request.request_sequence,
                now + crate::language_service::COMPLETION_DEADLINE,
            );
        } else if !requested {
            self.pending_completion_request = None;
        }
        Ok(requested)
    }

    fn active_language_document(&self) -> crate::language_service::DocumentState {
        let buffer = self.workspace.active_buffer();
        crate::language_service::DocumentState {
            document_id: self.workspace.active_document_id().clone(),
            path: self.workspace.active_path().clone(),
            version: buffer.version(),
            text: buffer.text(),
        }
    }

    #[cfg(test)]
    fn prepare_completion_request(
        &mut self,
        generation: u64,
    ) -> Result<Option<crate::language_service::CompletionRequest>> {
        self.prepare_completion_request_inner(generation, false)
    }

    fn prepare_completion_request_inner(
        &mut self,
        generation: u64,
        requires_service: bool,
    ) -> Result<Option<crate::language_service::CompletionRequest>> {
        self.clear_completion();
        self.workspace.ensure_active_user_editable()?;
        if self.command_active()
            || self.command.modal
            || self.workspace.confirmation_pending()
            || self.external_pending()
            || self.recovery_reason().is_some()
        {
            return Ok(None);
        }
        self.recheck_external()?;
        self.workspace.root_authority().verify_binding()?;
        let document = self.active_language_document();
        let selection = self.workspace.active_buffer().selection_state();
        let position_byte = selection.active_byte;
        let position = self
            .workspace
            .active_buffer()
            .positions(document.version)?
            .byte_to_utf16(rustrace_editor::position::ByteOffset(position_byte))?;
        let request_sequence = {
            let mut authority = self.effects.0.borrow_mut();
            authority.append(Event::LspCompletionRequested(CompletionRequested {
                document_id: document.document_id.clone(),
                document_version: document.version,
                position_byte,
            }))?;
            authority.sequence
        };
        let request = crate::language_service::CompletionRequest {
            generation,
            request_sequence,
            document_id: document.document_id.clone(),
            path: document.path,
            version: document.version,
            position_byte,
            position,
        };
        self.pending_completion_request = Some(PendingCompletionRequest {
            session_id: self.metadata.session_id.clone(),
            workspace_root: self.workspace.root().to_path_buf(),
            document_hash: self.workspace.active_buffer().hash(),
            selection,
            request: request.clone(),
            requires_service,
        });
        Ok(Some(request))
    }

    fn receive_completion(
        &mut self,
        response: crate::language_service::CompletionResponse,
    ) -> bool {
        let Some(context) = self.pending_completion_request.as_ref() else {
            return false;
        };
        if response.request != context.request {
            return false;
        }
        let context = self
            .pending_completion_request
            .take()
            .expect("matching completion request remains pending");
        self.completion = None;
        if !self.completion_context_is_current(&context) {
            return false;
        }
        let mut converted = Vec::new();
        {
            let buffer = self.workspace.active_buffer();
            let Ok(positions) = buffer.positions(context.request.version) else {
                return false;
            };
            for item in response
                .items
                .into_iter()
                .take(crate::language_service::MAX_COMPLETION_ITEMS)
            {
                if item.label.len() > MAX_STRING_BYTES
                    || item.kind.is_some_and(|kind| !(1..=25).contains(&kind))
                {
                    continue;
                }
                let edit = match item.edit {
                    crate::language_service::CompletionEdit::Insert(inserted_text) => TextEdit {
                        start_byte: context.request.position_byte,
                        end_byte: context.request.position_byte,
                        inserted_text,
                    },
                    crate::language_service::CompletionEdit::Replace {
                        start,
                        end,
                        new_text,
                    } => {
                        let (Ok(start), Ok(end)) =
                            (positions.utf16_to_byte(start), positions.utf16_to_byte(end))
                        else {
                            continue;
                        };
                        if start.0 > end.0 {
                            continue;
                        }
                        TextEdit {
                            start_byte: start.0,
                            end_byte: end.0,
                            inserted_text: new_text,
                        }
                    }
                };
                converted.push((item.label, item.kind, edit));
            }
        }
        let mut items = Vec::with_capacity(converted.len());
        for (label, kind, edit) in converted {
            if self
                .workspace
                .preview_completion(edit.clone())
                .is_ok_and(|tx| tx.is_some())
            {
                items.push(CompletionCandidate { label, kind, edit });
            }
        }
        if items.is_empty() {
            return true;
        }
        self.completion = Some(CompletionState {
            context,
            items,
            selected: 0,
        });
        true
    }

    fn completion_context_is_current(&self, context: &PendingCompletionRequest) -> bool {
        let buffer = self.workspace.active_buffer();
        !self.command_active()
            && !self.command.modal
            && !self.workspace.confirmation_pending()
            && !self.external_pending()
            && self.recovery_reason().is_none()
            && context.session_id == self.metadata.session_id
            && context.workspace_root == self.workspace.root()
            && context.request.document_id == *self.workspace.active_document_id()
            && context.request.path == *self.workspace.active_path()
            && context.request.version == buffer.version()
            && context.document_hash == buffer.hash()
            && context.selection == buffer.selection_state()
            && (!context.requires_service
                || self.language_service.as_ref().is_some_and(|service| {
                    service.completion_generation_is_ready(context.request.generation)
                }))
    }

    pub(crate) fn completion_items(&self) -> &[CompletionCandidate] {
        self.completion
            .as_ref()
            .map(|completion| completion.items.as_slice())
            .unwrap_or(&[])
    }

    pub(crate) fn selected_completion(&self) -> Option<usize> {
        self.completion
            .as_ref()
            .map(|completion| completion.selected)
    }

    pub(crate) fn move_completion(&mut self, delta: i32) -> bool {
        let Some(completion) = &mut self.completion else {
            return false;
        };
        let length = completion.items.len();
        completion.selected = if delta < 0 {
            (completion.selected + length - 1) % length
        } else {
            (completion.selected + 1) % length
        };
        true
    }

    pub(crate) fn select_completion(&mut self, index: usize) -> bool {
        let Some(completion) = &mut self.completion else {
            return false;
        };
        if index >= completion.items.len() {
            return false;
        }
        completion.selected = index;
        true
    }

    pub(crate) fn scroll_completion(&mut self, delta: isize) -> bool {
        let Some(completion) = &mut self.completion else {
            return false;
        };
        let selected = completion
            .selected
            .saturating_add_signed(delta)
            .min(completion.items.len().saturating_sub(1));
        let changed = selected != completion.selected;
        completion.selected = selected;
        changed
    }

    pub fn accept_completion(&mut self) -> Result<bool> {
        let Some(completion) = self.completion.take() else {
            return Ok(false);
        };
        if !self.completion_context_is_current(&completion.context) {
            return Ok(false);
        }
        let candidate = completion
            .items
            .get(completion.selected)
            .ok_or("completion selection is outside the bounded list")?
            .clone();
        let Some(transaction) = self.workspace.preview_completion(candidate.edit.clone())? else {
            return Ok(false);
        };
        self.effects.0.borrow_mut().pending_completion = Some(CompletionAccepted {
            document_id: completion.context.request.document_id,
            document_version: completion.context.request.version,
            label: candidate.label,
            primary_edit: candidate.edit,
            additional_edits: Vec::new(),
        });
        let applied = self.workspace.apply_completion(transaction);
        self.effects.0.borrow_mut().pending_completion = None;
        let applied = applied?;
        self.undo_limited |= self
            .workspace
            .trim_undo_to(self.effects.0.borrow().budgets.undo_bytes);
        Ok(applied)
    }

    pub(crate) fn clear_completion(&mut self) {
        self.pending_completion_request = None;
        self.completion = None;
        self.effects.0.borrow_mut().pending_completion = None;
    }

    pub(crate) fn cancel_deferred_automatic_completion(&mut self) {
        self.automatic_completion_requests.cancel_deferred();
    }

    fn retire_stale_completion(&mut self) -> bool {
        let stale_request = self
            .pending_completion_request
            .as_ref()
            .is_some_and(|context| !self.completion_context_is_current(context));
        let stale_list = self
            .completion
            .as_ref()
            .is_some_and(|completion| !self.completion_context_is_current(&completion.context));
        if stale_request || stale_list {
            self.clear_completion();
            true
        } else {
            false
        }
    }

    /// Explicit F8 operation. The controller itself enforces all authority gates.
    pub fn reload_language_service(&mut self) -> Result<bool> {
        self.clear_completion();
        self.clear_live_diagnostics();
        self.require_command_idle()?;
        if self.command.modal
            || self.workspace.confirmation_pending()
            || self.external_pending()
            || self.recovery_reason().is_some()
        {
            return Ok(false);
        }
        self.recheck_external()?;
        self.workspace.root_authority().verify_binding()?;
        if self.language_service.is_none() {
            self.start_language_service()?;
            return Ok(true);
        }
        Ok(self
            .language_service
            .as_mut()
            .is_some_and(|service| service.reload(Instant::now())))
    }

    fn publish_language_resolution(
        &mut self,
        record: &crate::language_service::ResolutionRecord,
    ) -> Result<()> {
        let result = (|| {
            self.workspace.root_authority().verify_binding()?;
            let mut authority = self.effects.0.borrow_mut();
            authority.complete_checkpoint(true)?;
            authority.healthy()?;
            let artifact = serde_json::json!({
                "version": 1,
                "session_id": self.metadata.session_id,
                "manifest_hash": self.metadata.manifest_hash,
                "sequence": authority.sequence,
                "event_hash": authority.hash,
                "resolution": record,
            });
            let bytes = serde_json::to_vec(&artifact)?;
            if bytes.len() > METADATA_LIMIT {
                return Err("language-service resolution artifact exceeds limit".into());
            }
            authority.headroom(bytes.len() as u64)?;
            authority.owner.publish_artifact(
                &format!(
                    "language-server-resolution-{:020}-{:020}.json",
                    authority.sequence, record.generation
                ),
                &bytes,
                false,
            )?;
            Ok(())
        })();
        self.effects.0.borrow_mut().fail(result)
    }

    fn poll_language_service(&mut self) -> Result<bool> {
        let writes_allowed = !self.command_active()
            && !self.command.modal
            && !self.workspace.confirmation_pending()
            && !self.external_pending()
            && self.recovery_reason().is_none();
        let documents = if writes_allowed {
            self.workspace.language_documents()?
        } else {
            Vec::new()
        };
        let Some(mut service) = self.language_service.take() else {
            return Ok(false);
        };
        let now = Instant::now();
        let poll = service.poll(&documents, writes_allowed, now);
        let mut changed = poll.changed;
        let completions = poll.completions;
        changed |= self.apply_live_diagnostic_poll(poll.diagnostic_generation, poll.diagnostics);
        if let Some(record) = poll.resolution {
            if !writes_allowed {
                self.language_service = Some(service);
                return Ok(changed);
            }
            if let Err(error) = self.recheck_external() {
                let cleanup_confirmed = service.stop_for_authority();
                self.language_service = Some(service);
                if !cleanup_confirmed {
                    return Err(
                        "language-service cleanup was not confirmed after launch reconciliation"
                            .into(),
                    );
                }
                return Err(error);
            }
            let published = self.publish_language_resolution(&record);
            service.authorize_resolution(published.is_ok(), Instant::now());
            self.language_service = Some(service);
            published?;
            changed = true;
        } else {
            self.language_service = Some(service);
        }
        let mut deferred_documents = Vec::new();
        for completion in completions {
            if self.automatic_completion_requests.completed(
                &completion.request.document_id,
                completion.request.request_sequence,
            ) {
                deferred_documents.push(completion.request.document_id.clone());
            }
            changed |= self.receive_completion(completion);
        }
        deferred_documents.extend(self.automatic_completion_requests.expire(now));
        deferred_documents.sort();
        deferred_documents.dedup();
        for document_id in deferred_documents {
            if document_id == *self.workspace.active_document_id() {
                changed |= self.trigger_automatic_completion_at(now)?;
            }
        }
        changed |= self.retire_stale_completion();
        Ok(changed)
    }

    fn stop_language_service_for_authority(&mut self) -> Result<()> {
        self.clear_live_diagnostics();
        if self
            .language_service
            .as_mut()
            .is_some_and(|service| !service.stop_for_authority())
        {
            return Err("language-service cleanup was not confirmed before reconciliation".into());
        }
        Ok(())
    }

    fn stop_language_service_for_command(&mut self) -> Result<()> {
        self.clear_live_diagnostics();
        if self
            .language_service
            .as_mut()
            .is_some_and(|service| !service.stop_for_command())
        {
            return Err(
                "language-service cleanup was not confirmed before command ownership".into(),
            );
        }
        Ok(())
    }

    fn resume_language_service_after_command(&mut self) {
        if self.command_quit_requested() {
            return;
        }
        if let Some(service) = self.language_service.as_mut() {
            service.resume_after_command();
        }
    }

    pub fn persisted_millis(&self) -> u64 {
        self.effects.0.borrow().persisted_millis
    }
    pub(crate) fn workspace_mut(&mut self) -> &mut WorkspaceSession<SessionEffects> {
        &mut self.workspace
    }

    pub fn workspace(&self) -> &WorkspaceSession<SessionEffects> {
        &self.workspace
    }
    pub fn evidence_paths(&self) -> &[PathBuf] {
        &self.evidence_paths
    }
    pub fn recovery_reason(&self) -> Option<String> {
        self.workspace
            .recovery_reason()
            .or_else(|| self.external_blocker.clone())
    }
    pub fn execute(&mut self, command: EditorCommand) -> Result<EditorOutcome> {
        self.clear_completion();
        if command == EditorCommand::RequestQuit {
            if self.command_active() {
                self.cancel_command_for_quit();
                return Ok(EditorOutcome::Quit);
            }
            let outcome = self.workspace.execute_editor(command)?;
            if outcome == EditorOutcome::Quit {
                self.cancel_command_for_quit();
            }
            return Ok(outcome);
        }
        self.require_command_idle()?;
        self.workspace.ensure_active_user_editable()?;
        if let Some(reason) = &self.external_blocker {
            return Err(reason.clone().into());
        }
        let outcome = match command {
            EditorCommand::Copy => self.copy_internal(false)?,
            EditorCommand::Cut => self.copy_internal(true)?,
            EditorCommand::Paste => self.paste_internal()?,
            command => self.workspace.execute_editor(command)?,
        };
        self.undo_limited |= self
            .workspace
            .trim_undo_to(self.effects.0.borrow().budgets.undo_bytes);
        Ok(outcome)
    }

    /// Drops only live authority. Recorded history remains replayable.
    pub fn clear_clipboard(&mut self) {
        let mut authority = self.effects.0.borrow_mut();
        authority.live_clipboard = None;
        authority.pending_paste = None;
    }

    pub(crate) fn has_internal_clipboard(&self) -> bool {
        self.effects.0.borrow().live_clipboard.is_some()
    }

    pub(crate) fn live_clipboard_text(&self) -> Result<Option<String>> {
        let authority = self.effects.0.borrow();
        let Some(source) = authority.live_clipboard.as_ref() else {
            return Ok(None);
        };
        Ok(Some(
            authority
                .replay
                .as_ref()
                .ok_or("missing genesis")?
                .clipboard_text(source)?
                .to_owned(),
        ))
    }

    pub(crate) fn terminal_paste_matches(&self, delivered: &str) -> Result<bool> {
        let Some(clipboard) = self.live_clipboard_text()? else {
            return Ok(false);
        };
        if delivered == clipboard {
            return Ok(true);
        }
        if !clipboard.contains('\n') || clipboard.contains('\r') || !delivered.contains('\r') {
            return Ok(false);
        }
        Ok(delivered.replace("\r\n", "\n").replace('\r', "\n") == clipboard)
    }

    pub(crate) fn paste_terminal(&mut self, delivered: &str) -> Result<EditorOutcome> {
        if self.terminal_paste_matches(delivered)? {
            return self.execute(EditorCommand::Paste);
        }
        self.reject_paste(
            PasteInputChannel::TerminalBracketed,
            PasteRejectionReason::ExternalInput,
        )?;
        Err(PASTE_BLOCKED_WARNING.into())
    }

    /// Metadata-only rejection boundary. The caller discards the input before
    /// invoking this method, so no payload can enter errors or evidence.
    pub fn reject_paste(
        &mut self,
        channel: PasteInputChannel,
        reason: PasteRejectionReason,
    ) -> Result<()> {
        self.clear_completion();
        self.effects.0.borrow_mut().reject_paste(channel, reason)
    }

    fn copy_internal(&mut self, cut: bool) -> Result<EditorOutcome> {
        if self.workspace.confirmation_pending() {
            return Err("Confirm or cancel the pending action before copying or cutting.".into());
        }
        if let Some(reason) = self.workspace.recovery_reason() {
            return Err(reason.into());
        }
        let selection = self.workspace.active_buffer().selection_state();
        if selection.is_caret() {
            return Ok(EditorOutcome::NoChange);
        }
        if cut {
            self.workspace.active_buffer().preview_delete_backward()?;
        }
        let mut authority = self.effects.0.borrow_mut();
        let result = authority
            .complete_checkpoint(true)
            .and_then(|()| authority.healthy());
        authority.fail(result)?;
        let buffer = self.workspace.active_buffer();
        let source = ClipboardSource {
            prefix: RecordedEventRef {
                session_id: self.metadata.session_id.clone(),
                sequence: authority.sequence,
                event_hash: authority.hash,
            },
            document_id: self.workspace.active_document_id().clone(),
            path: self.workspace.active_path().clone(),
            version: buffer.version(),
            content_hash: buffer.hash(),
            start_byte: selection.anchor_byte.min(selection.active_byte),
            end_byte: selection.anchor_byte.max(selection.active_byte),
        };
        // Bounds/source errors are normal preflight failures, not append poison.
        let selected = authority
            .replay
            .as_ref()
            .ok_or("missing genesis")?
            .validate_clipboard_source(&source)?;
        // Even copying an identical selection must satisfy normal edit/JSON
        // bounds before publishing proof or activating the slot. The preview
        // validates encoding before its no-op check and changes no state.
        buffer.preview_paste(selected)?;
        // Keep the old slot through all non-mutating preflight. Once durable
        // replacement starts, failure cannot leave an invalid usable reference.
        authority.live_clipboard = None;
        authority.pending_paste = None;
        authority.append(Event::ClipboardCopied(source))?;
        let reference = RecordedEventRef {
            session_id: self.metadata.session_id.clone(),
            sequence: authority.sequence,
            event_hash: authority.hash,
        };
        drop(authority);
        let outcome = if cut {
            self.workspace
                .execute_editor(EditorCommand::DeleteBackward)?
        } else {
            EditorOutcome::Copied
        };
        self.effects.0.borrow_mut().live_clipboard = Some(reference);
        Ok(outcome)
    }

    fn paste_internal(&mut self) -> Result<EditorOutcome> {
        if self.workspace.confirmation_pending() {
            self.reject_paste(
                PasteInputChannel::ProductionCommand,
                PasteRejectionReason::OutsideEditor,
            )?;
            return Err(PASTE_BLOCKED_WARNING.into());
        }
        let mut authority = self.effects.0.borrow_mut();
        let Some(source) = authority.live_clipboard.clone() else {
            authority.reject_paste(
                PasteInputChannel::InternalShortcut,
                PasteRejectionReason::MissingLiveSource,
            )?;
            return Err(PASTE_BLOCKED_WARNING.into());
        };
        authority.healthy()?;
        let text = authority
            .replay
            .as_ref()
            .ok_or("missing genesis")?
            .clipboard_text(&source)?
            .to_owned();
        // The permit is scoped to this one synchronous call, including no-op
        // and failed preflight paths. It is never reconstructed from text.
        authority.pending_paste = Some(source);
        drop(authority);
        let result = self
            .workspace
            .execute_editor(EditorCommand::PasteExternal(text));
        self.effects.0.borrow_mut().pending_paste = None;
        Ok(result?)
    }
    pub fn create_file(&mut self, path: &str) -> Result<crate::tui::WorkspaceOutcome> {
        self.require_command_idle()?;
        self.recheck_external()?;
        let outcome = self.workspace.create_file(path)?;
        self.finish_lifecycle(outcome)
    }
    pub fn rename_selected(&mut self, path: &str) -> Result<crate::tui::WorkspaceOutcome> {
        self.require_command_idle()?;
        self.recheck_external()?;
        let outcome = self.workspace.rename_selected(path)?;
        self.finish_lifecycle(outcome)
    }
    pub fn delete_selected(&mut self) -> Result<crate::tui::WorkspaceOutcome> {
        self.require_command_idle()?;
        self.recheck_external()?;
        let outcome = self.workspace.request_delete_selected()?;
        self.finish_lifecycle(outcome)
    }
    pub fn confirm_delete(&mut self) -> Result<crate::tui::WorkspaceOutcome> {
        self.require_command_idle()?;
        self.recheck_external()?;
        let outcome = self.workspace.confirm_delete()?;
        self.finish_lifecycle(outcome)
    }
    fn finish_lifecycle(
        &mut self,
        outcome: crate::tui::WorkspaceOutcome,
    ) -> Result<crate::tui::WorkspaceOutcome> {
        if matches!(
            outcome,
            crate::tui::WorkspaceOutcome::FileRenamed | crate::tui::WorkspaceOutcome::FileDeleted
        ) {
            self.clear_live_diagnostics();
        }
        if matches!(
            outcome,
            crate::tui::WorkspaceOutcome::FileCreated
                | crate::tui::WorkspaceOutcome::FileRenamed
                | crate::tui::WorkspaceOutcome::FileDeleted
        ) {
            self.save_all()?;
        }
        Ok(outcome)
    }

    /// One bounded scan, with at most two observations before stop/preserve.
    /// Timers are hints; save/resume/boundaries call this unconditionally.
    pub fn recheck_external(&mut self) -> Result<bool> {
        self.require_command_idle()?;
        self.recheck_with_hook(|_| Ok(()))
    }

    fn recheck_with_hook(&mut self, mut hook: impl FnMut(&str) -> Result<()>) -> Result<bool> {
        if let Some(reason) = &self.external_blocker {
            return Err(reason.clone().into());
        }
        self.last_recheck = Instant::now();
        self.drain()?;
        self.effects.0.borrow().healthy()?;
        let result = self.reconcile_canonical(&mut hook);
        if let Err(error) = &result {
            self.external_blocker = Some(format!(
                "external reconciliation stopped; preserve and inspect: {error}"
            ));
            self.effects.0.borrow_mut().poison = self.external_blocker.clone();
        }
        result
    }

    pub fn external_pending(&self) -> bool {
        self.external.is_some() || self.external_blocker.is_some()
    }

    pub fn external_notice(&self) -> Option<&str> {
        self.external_warning.then_some("External changes are not accepted.\nRustrace preserves recovery evidence.\nRestoring current contents, including unsaved edits.")
    }

    fn reconcile_canonical(&mut self, hook: &mut impl FnMut(&str) -> Result<()>) -> Result<bool> {
        let saved = self
            .external
            .as_ref()
            .map(|v| v.saved.clone())
            .unwrap_or_else(|| self.workspace.saved_files());
        let logical = self.workspace.buffered_files()?;
        let mut disk = read_pinned_workspace(self.workspace.root_authority())?;
        if disk == saved && self.external.is_none() {
            return Ok(false);
        }
        self.external_warning = true;
        // A background child can mutate without receiving another message.
        // End its process group before evidence capture/restoration and observe
        // disk again so the P2 boundary includes its final possible effects.
        self.stop_language_service_for_authority()?;
        disk = read_pinned_workspace(self.workspace.root_authority())?;
        if disk == logical
            && let Some(hash) = self.pending_evidence
        {
            let a = self.effects.0.borrow();
            let name = format!("restored-{hash}.json");
            // Existing outcomes were validated against evidence and checkpoint
            // before opening the writer. Finish only the interrupted baseline.
            if a.owner.read_artifact(&name, METADATA_LIMIT).is_ok() {
                drop(a);
                self.workspace.adopt_verified_baseline(&logical)?;
                self.persist_baseline()?;
                self.external = None;
                self.pending_evidence = None;
                return Ok(true);
            }
        }
        for attempt in 0..2 {
            hook("capture")?;
            let views = RecoveryEvidence {
                saved: saved.clone(),
                logical: logical.clone(),
                disk: disk.clone(),
            };
            let reusable = if let Some(hash) = self.pending_evidence.take() {
                let a = self.effects.0.borrow();
                let outcome_exists = a
                    .owner
                    .read_artifact(&format!("restored-{hash}.json"), METADATA_LIMIT)
                    .is_ok();
                // A completed restoration owns its immutable outcome forever.
                // Even identical C returning is a new observation, stamped with
                // the current durable sequence by preserve_observation.
                (!outcome_exists
                    && self.external.as_ref().is_some_and(|old| {
                        old.logical == logical && (old.disk == disk || disk == logical)
                    }))
                .then_some(hash)
            } else {
                None
            };
            let evidence_hash = match reusable {
                Some(hash) => hash,
                None => self.preserve_observation(&views)?,
            };
            self.external = Some(views);
            hook("observation")?;
            // Policy and text rejection happens after bounded capture but before
            // any source mutation. An unmanaged path is never silently swept.
            let manifest = self
                .effects
                .0
                .borrow()
                .owner
                .read_artifact("manifest.toml", METADATA_LIMIT)?;
            let policy = AllowedPathSet::from_manifest(&AssignmentManifest::parse(&manifest)?)?;
            for (path, bytes) in &disk {
                policy.validate(path)?;
                if path.as_str().chars().any(char::is_control)
                    || (saved.get(path) != Some(bytes)
                        && (std::str::from_utf8(bytes).is_err() || bytes.contains(&0)))
                {
                    return Err(format!(
                        "unsupported external text at {path}; exact bounded evidence retained"
                    )
                    .into());
                }
            }
            {
                let mut a = self.effects.0.borrow_mut();
                a.owner
                    .secure_reserve(SessionBudgets::default().reserve_bytes, false)?;
                a.headroom(40 * 1024 * 1024)?;
                a.append(Event::RecoveryRecorded(RecoveryRecorded {
                    evidence_hash,
                    decision: RecoveryDecision::RestoreLogical,
                }))?;
            }
            process_probe("external-intent");
            hook("intent")?;
            let current = read_pinned_workspace(self.workspace.root_authority())?;
            if current != disk {
                if attempt == 1 {
                    return Err(
                        "external target changed repeatedly; no restoration attempted".into(),
                    );
                }
                disk = current;
                continue;
            }
            restore_disk(self.workspace.root_authority(), &logical, &disk)?;
            process_probe("external-disk");
            hook("publication")?;
            self.workspace.adopt_verified_baseline(&logical)?;
            // The unchanged logical checkpoint is the durable successful
            // restoration boundary. Its linked outcome receipt retains evidence.
            let input = self
                .workspace
                .checkpoint_input(self.metadata.session_id.clone())?;
            self.effects.0.borrow_mut().checkpoint(input, true)?;
            process_probe("external-checkpoint");
            {
                let a = self.effects.0.borrow();
                let outcome = RestorationOutcome {
                    version: 1,
                    evidence_hash,
                    sequence: a.sequence,
                    event_hash: a.hash,
                    workspace_hash: a
                        .replay
                        .as_ref()
                        .ok_or("missing replay")?
                        .workspace_state()
                        .workspace_hash(),
                    outcome: "canonical_restored".into(),
                };
                a.owner.publish_artifact(
                    &format!("restored-{evidence_hash}.json"),
                    &serde_json::to_vec(&outcome)?,
                    false,
                )?;
            }
            process_probe("external-outcome");
            hook("outcome")?;
            self.persist_baseline()?;
            process_probe("external-baseline");
            hook("baseline")?;
            self.external = None;
            self.last_save = Instant::now();
            return Ok(true);
        }
        unreachable!("bounded reconciliation returns or stops")
    }

    fn preserve_observation(&mut self, views: &RecoveryEvidence) -> Result<Hash> {
        let a = self.effects.0.borrow();
        let saved = files_snapshot(&self.metadata.session_id, a.sequence, &views.saved)?;
        let logical = files_snapshot(&self.metadata.session_id, a.sequence, &views.logical)?;
        let bytes = evidence_bytes(&saved, &logical, &views.disk)?;
        drop(a);
        let evidence_hash = digest(&bytes);
        let name = format!("evidence-{evidence_hash}.bin");
        let mut a = self.effects.0.borrow_mut();
        a.headroom(bytes.len() as u64 + 40 * 1024 * 1024)?;
        match a.owner.read_artifact(&name, ARTIFACT_LIMIT) {
            Ok(existing) if existing == bytes => {}
            Ok(_) => return Err("external evidence collision".into()),
            Err(_) => a.owner.publish_artifact(&name, &bytes, false)?,
        }
        self.evidence_paths.clear();
        self.evidence_paths.push(
            a.owner
                .display_path()
                .parent()
                .ok_or("missing state directory")?
                .join(name),
        );
        process_probe("external-evidence");
        for path in views
            .saved
            .keys()
            .chain(views.logical.keys())
            .chain(views.disk.keys())
            .collect::<std::collections::BTreeSet<_>>()
        {
            if views.saved.get(path) != views.disk.get(path)
                || views.logical.contains_key(path) != views.disk.contains_key(path)
            {
                a.append(Event::ExternalObservation(ExternalObservation {
                    path: path.clone(),
                    saved_hash: view_hash(views.saved.get(path)),
                    logical_hash: view_hash(views.logical.get(path)),
                    observed_hash: view_hash(views.disk.get(path)),
                    evidence_hash,
                }))?;
            }
        }
        process_probe("external-observation");
        Ok(evidence_hash)
    }

    pub fn save_all(&mut self) -> Result<()> {
        self.require_command_idle()?;
        self.workspace.ensure_active_user_editable()?;
        self.save_all_with_hook(|_| {})
    }

    /// Save explicit user edits, then dispatch the same controlled Check used
    /// by the command menu. Autosave intentionally continues to call
    /// `save_all` directly and never reaches this path.
    pub fn save_all_and_check(&mut self) -> Result<ExplicitSaveOutcome> {
        if self.command_active() || self.command.pending_console.is_some() {
            return Ok(ExplicitSaveOutcome::RunnerBusy);
        }
        self.save_all()?;
        match self.start_command(crate::cargo_policy::CargoAction::Check) {
            Ok(()) => Ok(ExplicitSaveOutcome::CheckStarted),
            Err(error) => Ok(ExplicitSaveOutcome::CheckFailedToStart(
                crate::display::label_fmt(format_args!("{error}"), 1024),
            )),
        }
    }

    fn save_all_with_hook(&mut self, published: impl FnMut(&WorkspacePath)) -> Result<()> {
        self.recheck_with_hook(|_| Ok(()))?;
        let result = (|| {
            self.drain()?;
            self.effects.0.borrow().healthy()?;
            self.effects.0.borrow().headroom(24 * 1024 * 1024)?;
            let logical = self.workspace.logical_files()?;
            if self
                .effects
                .0
                .borrow()
                .replay
                .as_ref()
                .ok_or("missing replay")?
                .workspace_state()
                .files()
                != &logical
            {
                return Err("logical buffers differ from durable replay prefix".into());
            }
            {
                let a = self.effects.0.borrow();
                let intent = SavedReceipt {
                    version: 1,
                    session_id: self.metadata.session_id.clone(),
                    sequence: a.sequence,
                    event_hash: a.hash,
                    workspace_hash: hash_entries(logical.iter().map(|(p, b)| (p, b.as_slice())))?,
                };
                a.owner.publish_artifact(
                    "save-intent.json",
                    &serde_json::to_vec(&intent)?,
                    true,
                )?;
            }
            self.workspace.save_all_with_hook(published)?;
            process_probe("disk");
            self.persist_baseline()?;
            self.last_save = Instant::now();
            Ok(())
        })();
        self.effects.0.borrow_mut().fail(result)
    }
    fn persist_baseline(&mut self) -> Result<()> {
        let a = self.effects.0.borrow();
        a.healthy()?;
        let files = self.workspace.disk_files()?;
        if files != self.workspace.logical_files()? {
            return Err("baseline requires D=S=L".into());
        }
        let receipt = SavedReceipt {
            version: 1,
            session_id: self.metadata.session_id.clone(),
            sequence: a.sequence,
            workspace_hash: hash_entries(files.iter().map(|(p, b)| (p, b.as_slice())))?,
            event_hash: a.hash,
        };
        a.owner
            .publish_artifact("baseline.json", &serde_json::to_vec(&receipt)?, true)?;
        if a.sequence > 1 {
            process_probe("baseline");
        }
        Ok(())
    }
    pub fn drain(&mut self) -> Result<()> {
        self.effects.0.borrow_mut().complete_checkpoint(true)
    }

    /// Poll actual receipts, autosave when due, and queue changed periodic captures.
    pub fn tick(&mut self) -> Result<bool> {
        self.effects.0.borrow_mut().complete_checkpoint(false)?;
        let mut changed = self.poll_language_service()?;
        if self.effects.0.borrow().command_active {
            changed |= self.poll_command()?;
            return Ok(changed);
        }
        if self.last_recheck.elapsed() >= Duration::from_secs(2) {
            self.recheck_external()?;
        }
        if self.external.is_some() || self.external_blocker.is_some() {
            return Ok(changed);
        }
        if self.last_save.elapsed() >= Duration::from_secs(2)
            && self.workspace.file_tree().iter().any(|f| f.is_dirty())
        {
            self.save_all()?;
        }
        {
            let a = self.effects.0.borrow();
            a.healthy()?;
            if !a.changed
                || !a
                    .schedule
                    .should_submit(Duration::from_millis(a.now()), CheckpointTrigger::Activity)
            {
                return Ok(changed);
            }
        }
        self.recheck_external()?;
        let input = self
            .workspace
            .checkpoint_input(self.metadata.session_id.clone())?;
        changed |= self.effects.0.borrow_mut().checkpoint(input, false)?;
        Ok(changed)
    }
    pub fn capture_boundary(&mut self) -> Result<()> {
        self.require_command_idle()?;
        self.save_all()?;
        let input = self
            .workspace
            .checkpoint_input(self.metadata.session_id.clone())?;
        self.effects.0.borrow_mut().checkpoint(input, true)?;
        Ok(())
    }
    fn require_command_idle(&self) -> Result<()> {
        if self.effects.0.borrow().command_active || self.command.pending_console.is_some() {
            Err("command owns workspace; cancel or wait for its durable post-boundary".into())
        } else {
            Ok(())
        }
    }
    pub fn quit(mut self) -> Result<()> {
        let command_shutdown = self.shutdown_command();
        let had_language_service = self.language_service.is_some();
        let language_service_clean = self
            .language_service
            .take()
            .is_none_or(|mut service| service.shutdown());
        let post_service_reconciliation = if !language_service_clean {
            Err("language-service cleanup was not confirmed before final reconciliation".into())
        } else if had_language_service && self.external_blocker.is_none() {
            self.recheck_external().map(|_| ())
        } else {
            // With no service there are no shutdown side effects to reconcile. A
            // prior reconciliation failure already stopped any service and
            // retained its exact recovery evidence.
            Ok(())
        };
        let unpublished_capture = self.unpublished_command_capture().is_some();
        self.clear_clipboard();
        let drained = self.drain();
        let writer = self.effects.0.borrow_mut().writer.take();
        let shutdown = writer.map(JournalWriter::shutdown).transpose();
        self.effects.0.borrow().owner.verify()?;
        drained?;
        shutdown?;
        command_shutdown?;
        if unpublished_capture {
            return Err(
                "command capture could not be persisted; activity remains unfinished".into(),
            );
        }
        self.effects.0.borrow_mut().owner.release_ownership()?;
        post_service_reconciliation
    }

    pub(crate) fn prepare_terminal_quit(&mut self) -> TerminalQuit {
        let result = self.shutdown_command();
        if self.command_terminal_restoration_safe() {
            TerminalQuit::Ready(result)
        } else {
            TerminalQuit::CleanupPending
        }
    }
}

fn require_test_case_suite_identity(
    manifest: &AssignmentManifest,
    test_case_suite_hash: Option<Hash>,
) -> Result<()> {
    match (manifest.format_version, test_case_suite_hash) {
        (1, None) | (2, Some(_)) => Ok(()),
        (1, Some(_)) => {
            Err("format_version = 1 sessions cannot record a packaged test-case suite".into())
        }
        (2, None) => {
            Err("format_version = 2 sessions require a validated test-case suite hash".into())
        }
        _ => Err("unsupported assignment package identity".into()),
    }
}
impl Drop for ProductionSession {
    fn drop(&mut self) {
        self.cancel_command_for_quit();
        // An unfinished command may leave processes behind: never unlock
        // explicitly, so they keep the workspace locked until they exit.
        if let Ok(mut authority) = self.effects.0.try_borrow_mut()
            && authority.command_active
        {
            authority.owner.keep_writer_lock_for_children();
        }
    }
}
fn make_effects(
    owner: PinnedJournalFile,
    journal: Journal,
    metadata: SessionMetadata,
    replay: Option<ReplayEngine>,
    sequence: u64,
    millis: u64,
    hash: Hash,
) -> Result<SessionEffects> {
    Ok(SessionEffects(Rc::new(RefCell::new(Authority {
        writer: Some(JournalWriter::spawn(16, journal)?),
        owner,
        metadata,
        started: Instant::now(),
        offset: millis,
        persisted_millis: millis,
        sequence,
        hash,
        replay,
        poison: None,
        schedule: CheckpointScheduleState::new(
            CheckpointPolicy::new(Duration::from_secs(30), 100)?,
            Duration::from_millis(millis),
        ),
        changed: false,
        pending_checkpoint: None,
        budgets: SessionBudgets::default(),
        command_active: false,
        formatter_permit: false,
        dependency_tool_permit: false,
        pending_completion: None,
        live_clipboard: None,
        pending_paste: None,
    }))))
}
fn digest(bytes: &[u8]) -> Hash {
    Hash::from_bytes(*blake3::hash(bytes).as_bytes())
}

#[cfg(test)]
#[path = "session/command_authority_tests.rs"]
mod command_authority_tests;

fn preserved_evidence(
    root: &PinnedWorkspaceRoot,
    owner: &PinnedStateInspection,
) -> Result<serde_json::Value> {
    let inventory = owner.inventory(SessionBudgets::default().storage_bytes, 1024)?;
    let read = |name: &str| -> Result<Option<Vec<u8>>> {
        if inventory
            .iter()
            .any(|(n, len, _)| n == name && *len <= METADATA_LIMIT as u64)
        {
            Ok(Some(owner.read_artifact(name, METADATA_LIMIT)?))
        } else {
            Ok(None)
        }
    };
    let metadata_bytes = read("session.json")?;
    let metadata = metadata_bytes
        .as_deref()
        .and_then(|b| serde_json::from_slice::<SessionMetadata>(b).ok())
        .filter(|m| m.version == 1);
    let manifest = read("manifest.toml")?;
    let available_manifest_hash = manifest
        .as_deref()
        .filter(|b| AssignmentManifest::parse(b).is_ok())
        .map(digest);
    let source = read_pinned_workspace(root)?;
    let evidence = serde_json::json!({
        "version": 1,
        "original_directory": root.path(),
        "original_session": metadata.as_ref().map(|m| &m.session_id),
        "identity_status": if metadata.is_some() { "metadata-declared; journal not validated" } else { "unknown; metadata missing or invalid" },
        "available_manifest_hash": available_manifest_hash,
        "original_metadata_hash": inventory.iter().find(|(n, _, _)| n == "session.json").map(|(_, _, h)| h),
        "observed_source_hash": hash_entries(source.iter().map(|(p, b)| (p, b.as_slice())))?,
        "observed_source_files": source.len(),
        "state_files": inventory.iter().map(|(name, bytes, hash)| serde_json::json!({"name": name, "bytes": bytes, "blake3": hash})).collect::<Vec<_>>(),
        "meaning": "original source and state files retained in place; saved/logical views unavailable without validated journal; selected fresh package is not attributed to unknown original identity"
    });
    owner.verify()?;
    Ok(evidence)
}

pub(crate) fn verify_available_manifest(
    evidence: &serde_json::Value,
    manifest_bytes: &[u8],
) -> Result<()> {
    if !evidence["available_manifest_hash"].is_null()
        && evidence["available_manifest_hash"] != serde_json::to_value(digest(manifest_bytes))?
    {
        return Err("assignment manifest identity mismatch with preserved startup; select the original package; original left unchanged".into());
    }
    Ok(())
}
fn snapshot_files(snapshot: &CheckpointSnapshot) -> Files {
    snapshot
        .files()
        .iter()
        .map(|f| (f.path.clone(), f.contents.clone()))
        .collect()
}
fn files_snapshot(id: &SessionId, sequence: u64, files: &Files) -> Result<CheckpointSnapshot> {
    Ok(CheckpointSnapshot::new(
        id.clone(),
        sequence,
        files
            .iter()
            .map(|(path, contents)| CheckpointFile {
                path: path.clone(),
                contents: contents.clone(),
            })
            .collect(),
        None,
        vec![],
    )?)
}
fn snapshot_from_replay(
    id: &SessionId,
    sequence: u64,
    replay: &ReplayEngine,
) -> Result<CheckpointSnapshot> {
    let state = replay.workspace_state();
    Ok(CheckpointSnapshot::new(
        id.clone(),
        sequence,
        state
            .files()
            .iter()
            .map(|(path, contents)| CheckpointFile {
                path: path.clone(),
                contents: contents.clone(),
            })
            .collect(),
        state.active_document().cloned(),
        state
            .documents()
            .values()
            .map(|d| OpenDocument {
                document_id: d.document_id().clone(),
                path: d.path().clone(),
                version: d.version(),
                selection: d.selection(),
            })
            .collect(),
    )?)
}
fn load_saved_receipt(
    owner: &PinnedJournalFile,
    journal: &mut Journal,
    metadata: &SessionMetadata,
) -> Result<SavedReceipt> {
    let path = owner
        .display_path()
        .parent()
        .ok_or("state directory missing")?
        .join("baseline.json");
    match fs::symlink_metadata(&path) {
        Ok(_) => Ok(serde_json::from_slice(
            &owner.read_artifact("baseline.json", 1024)?,
        )?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Bootstrap snapshot also preserves sessions created before compact receipts.
            let baseline =
                decode_checkpoint(&owner.read_artifact("baseline.bin", ARTIFACT_LIMIT)?)?;
            let event = journal
                .read_events(&metadata.session_id, baseline.event_sequence(), 1)?
                .into_iter()
                .next()
                .ok_or("missing initial saved prefix")?;
            Ok(SavedReceipt {
                version: 1,
                session_id: baseline.session_id().clone(),
                sequence: baseline.event_sequence(),
                workspace_hash: baseline.workspace_hash(),
                event_hash: event.event_hash,
            })
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_prefix(
    journal: &mut Journal,
    metadata: &SessionMetadata,
    baseline: &SavedReceipt,
    owner: &PinnedJournalFile,
) -> Result<ValidatedPrefix> {
    validate_prefix_with_evidence(
        journal,
        metadata,
        baseline,
        owner,
        EvidenceRequirement::Complete,
    )
}

fn validate_prefix_for_finalization(
    journal: &mut Journal,
    metadata: &SessionMetadata,
    baseline: &SavedReceipt,
    owner: &PinnedJournalFile,
) -> Result<ValidatedPrefix> {
    validate_prefix_with_evidence(
        journal,
        metadata,
        baseline,
        owner,
        EvidenceRequirement::AllowMissing,
    )
}

fn validate_prefix_with_evidence(
    journal: &mut Journal,
    metadata: &SessionMetadata,
    baseline: &SavedReceipt,
    owner: &PinnedJournalFile,
    evidence_requirement: EvidenceRequirement,
) -> Result<ValidatedPrefix> {
    let id = &metadata.session_id;
    let chain = journal.verify_session_chain(id)?;
    if chain.event_count > SessionBudgets::default().events {
        return Err("event budget exceeded".into());
    }
    journal.verify_session_checkpoints(id)?;
    let genesis = journal
        .load_checkpoint(id, 1)?
        .ok_or("missing genesis checkpoint")?;
    if genesis.snapshot.workspace_hash() != metadata.starter_hash {
        return Err("starter identity mismatch".into());
    }
    if baseline.version != 1
        || &baseline.session_id != id
        || baseline.sequence == 0
        || baseline.sequence > chain.event_count
    {
        return Err("baseline does not belong to durable prefix".into());
    }
    let mut millis = genesis.owning_event.monotonic_millis;
    let mut checkpoint_millis = millis;
    let mut uncaptured_edits = 0;
    let mut changed = false;
    let mut pending_external = None;
    let genesis_hash = genesis.owning_event.event_hash;
    let mut replay = ReplayEngine::from_initial_checkpoint(genesis)?;
    let mut saved = if baseline.sequence == 1
        && baseline.workspace_hash == replay.workspace_state().workspace_hash()
        && baseline.event_hash == genesis_hash
    {
        Some(replay.workspace_state().files().clone())
    } else {
        None
    };
    let mut next = 2;
    loop {
        let events = journal.read_events(id, next, MAX_EVENTS_PER_READ)?;
        if events.is_empty() {
            break;
        }
        for event in events {
            match &event.event {
                Event::WorkspaceCheckpoint(_) => {
                    checkpoint_millis = event.monotonic_millis;
                    uncaptured_edits = 0;
                    changed = false;
                }
                Event::FileEdited(_) | Event::InternalPaste(_) => {
                    uncaptured_edits += 1;
                    changed = true;
                }
                Event::FileCreated(_)
                | Event::FileDeleted(_)
                | Event::FileRenamed(_)
                | Event::FileFocused(_)
                | Event::SelectionChanged(_)
                | Event::ExternalFileChange(_) => changed = true,
                _ => {}
            }
            match &event.event {
                Event::ExternalObservation(observation) => {
                    if event.sequence > baseline.sequence {
                        pending_external = Some(observation.evidence_hash);
                    }
                    if let Some(bytes) = read_prefix_evidence(
                        owner,
                        &format!("evidence-{}.bin", observation.evidence_hash),
                        ARTIFACT_LIMIT,
                        evidence_requirement,
                    )? {
                        if digest(&bytes) != observation.evidence_hash {
                            return Err("external observation evidence digest mismatch".into());
                        }
                        let evidence = read_recovery_evidence(&bytes)?;
                        if view_hash(evidence.saved.get(&observation.path))
                            != observation.saved_hash
                            || view_hash(evidence.logical.get(&observation.path))
                                != observation.logical_hash
                            || view_hash(evidence.disk.get(&observation.path))
                                != observation.observed_hash
                        {
                            return Err("external observation evidence view mismatch".into());
                        }
                    }
                }
                Event::RecoveryRecorded(decision) => {
                    let name = if decision.decision == RecoveryDecision::AbandonPreserved {
                        "parent.json".to_owned()
                    } else {
                        format!("evidence-{}.bin", decision.evidence_hash)
                    };
                    let evidence =
                        read_prefix_evidence(owner, &name, ARTIFACT_LIMIT, evidence_requirement)?;
                    if evidence
                        .as_ref()
                        .is_some_and(|bytes| digest(bytes) != decision.evidence_hash)
                    {
                        return Err("recovery decision evidence mismatch".into());
                    }
                    if decision.decision == RecoveryDecision::RestoreLogical
                        && let Some(evidence_bytes) = evidence
                    {
                        let name = format!("restored-{}.json", decision.evidence_hash);
                        let path = owner
                            .display_path()
                            .parent()
                            .ok_or("missing state directory")?
                            .join(&name);
                        match fs::symlink_metadata(path) {
                            Ok(_) => {
                                let outcome: RestorationOutcome = serde_json::from_slice(
                                    &owner.read_artifact(&name, METADATA_LIMIT)?,
                                )?;
                                let checkpoint = journal
                                    .load_checkpoint(id, outcome.sequence)?
                                    .ok_or("restoration outcome checkpoint missing")?;
                                let evidence = read_recovery_evidence(&evidence_bytes)?;
                                if outcome.version != 1
                                    || outcome.outcome != "canonical_restored"
                                    || outcome.evidence_hash != decision.evidence_hash
                                    || outcome.sequence <= event.sequence
                                    || outcome.event_hash != checkpoint.owning_event.event_hash
                                    || outcome.workspace_hash
                                        != checkpoint.snapshot.workspace_hash()
                                    || snapshot_files(&checkpoint.snapshot) != evidence.logical
                                {
                                    return Err("restoration outcome does not match canonical evidence/checkpoint".into());
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // Interrupted restoration or historical T3.6.
                            Err(e) => return Err(e.into()),
                        }
                    }
                }
                _ => {}
            }
            replay.apply(&event)?;
            if event.sequence == baseline.sequence
                && baseline.workspace_hash == replay.workspace_state().workspace_hash()
                && baseline.event_hash == event.event_hash
            {
                saved = Some(replay.workspace_state().files().clone());
            }
            millis = event.monotonic_millis;
            next = event.sequence + 1;
        }
    }
    if saved.is_none() || next - 1 != chain.event_count {
        return Err("saved baseline or durable prefix mismatch".into());
    }
    Ok(ValidatedPrefix {
        replay,
        sequence: chain.event_count,
        millis,
        hash: chain.final_hash,
        saved: saved.ok_or("missing saved prefix")?,
        checkpoint_millis,
        uncaptured_edits,
        changed,
        pending_external,
    })
}

fn read_prefix_evidence(
    owner: &PinnedJournalFile,
    name: &str,
    maximum: usize,
    requirement: EvidenceRequirement,
) -> Result<Option<Vec<u8>>> {
    match owner.read_artifact(name, maximum) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if requirement == EvidenceRequirement::AllowMissing => {
            let path = owner
                .display_path()
                .parent()
                .ok_or("state directory missing")?
                .join(name);
            match fs::symlink_metadata(path) {
                Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => Ok(None),
                _ => Err(error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}
/// An abandoned original whose linked recovery workspace recorded no work.
/// The recovery owner is held from the check until the original resumes.
struct AbandonedReclaim {
    recovery: Option<(PinnedJournalFile, PathBuf)>,
    // The recovery already holds this original's close marker.
    replace_marker: bool,
    root: PathBuf,
}

/// Abandonment only publishes `abandoned.json` in the original; it is never
/// journaled there. So the original can resume, without a new event, when its
/// recovery workspace is missing or recorded no work; that copy is then marked
/// abandoned instead, so only one line of history continues.
fn prepare_reclaim(
    owner: &PinnedJournalFile,
    metadata: &SessionMetadata,
) -> Result<AbandonedReclaim> {
    let bytes = owner.read_artifact("abandoned.json", METADATA_LIMIT)?;
    let record: serde_json::Value = serde_json::from_slice(&bytes)?;
    if record["decision"].as_str() == Some("reclaimed_by_original") {
        return Err(format!(
            "this unused recovery copy was closed when its original workspace {} resumed; continue there",
            record["original_directory"].as_str().unwrap_or("(unknown)")
        )
        .into());
    }
    if record["original_session"].as_str() != Some(metadata.session_id.as_str()) {
        return Err("abandonment record names another session; inspect before resuming".into());
    }
    let recorded = PathBuf::from(
        record["new_directory"]
            .as_str()
            .ok_or("abandonment record lacks its recovery workspace")?,
    );
    let root = fs::canonicalize(
        owner
            .display_path()
            .parent()
            .and_then(Path::parent)
            .ok_or("state directory missing")?,
    )?;
    let moved = record["original_directory"]
        .as_str()
        .and_then(|recorded| fs::canonicalize(recorded).ok())
        .is_none_or(|recorded| recorded != root);
    // Find the recovery bound to this record: beside a moved or copied
    // original first, then where it was created, then renamed beside it.
    let binding = digest(&bytes);
    let beside = root
        .parent()
        .zip(recorded.file_name())
        .map(|(parent, name)| parent.join(name));
    let mut candidates = Vec::new();
    if moved {
        candidates.extend(beside.clone());
    }
    candidates.push(recorded.clone());
    let directory = candidates
        .into_iter()
        .find(|candidate| candidate.join(".rustrace").exists())
        .or_else(|| find_bound_recovery(&root, binding))
        .unwrap_or_else(|| beside.unwrap_or(recorded));
    if !directory.join(".rustrace").try_exists()? {
        if moved {
            return Err(format!(
                "this workspace was abandoned and then moved; its recovery workspace {} was not found beside it; move it back next to this workspace and retry",
                directory.display()
            )
            .into());
        }
        // Deleted by the student: nothing else can continue this history.
        return Ok(AbandonedReclaim {
            recovery: None,
            replace_marker: false,
            root,
        });
    }
    let unusable = |error: &dyn std::fmt::Display| -> Box<dyn std::error::Error> {
        format!(
            "this workspace was abandoned; its recovery workspace {} could not be checked ({error}); close it if it is open and retry",
            directory.display()
        )
        .into()
    };
    let recovery_metadata: SessionMetadata =
        serde_json::from_slice(&read_initial_metadata(&directory).map_err(|e| unusable(&e))?)
            .map_err(|e| unusable(&e))?;
    // Only the recovery this record created is bound to it.
    if recovery_metadata.parent_evidence != Some(binding) {
        return Err(format!(
            "{} is not the recovery workspace linked to this abandoned workspace; inspect before resuming",
            directory.display()
        )
        .into());
    }
    let pinned = PinnedWorkspaceRoot::open(&directory).map_err(|e| unusable(&e))?;
    let recovery = pinned
        .open_state_directory()
        .and_then(|state| state.open_journal_file(&recovery_metadata.session_id))
        .map_err(|e| unusable(&e))?;
    let continue_there = || -> Box<dyn std::error::Error> {
        format!(
            "this workspace was abandoned and work was recorded in its recovery workspace {}; continue there with `rustrace work ASSIGNMENT.rta --workspace '{}'`",
            directory.display(),
            directory.display()
        )
        .into()
    };
    // Only this original's own close marker (from an interrupted reclaim) may
    // already be there; any other marker means the copy moved on.
    let closed = directory.join(".rustrace/abandoned.json").try_exists()?;
    if closed {
        let marker: serde_json::Value = serde_json::from_slice(
            &recovery
                .read_artifact("abandoned.json", METADATA_LIMIT)
                .map_err(|e| unusable(&e))?,
        )
        .map_err(|e| unusable(&e))?;
        // Our own marker names this original, or a path it has since left.
        let ours = marker["decision"].as_str() == Some("reclaimed_by_original")
            && marker["original_directory"]
                .as_str()
                .is_some_and(|recorded| {
                    fs::canonicalize(recorded).map_or(true, |recorded| recorded == root)
                });
        if !ours {
            return Err(format!(
                "this workspace was abandoned and its recovery workspace {} was itself closed or abandoned; run `rustrace work ASSIGNMENT.rta --workspace '{}' --inspect` to find the linked workspace to continue in",
                directory.display(),
                directory.display()
            )
            .into());
        }
    }
    let mut journal =
        Journal::open_read_only_no_follow(recovery.display_path()).map_err(|e| unusable(&e))?;
    let mut next = 1;
    loop {
        let events = journal
            .read_events(&recovery_metadata.session_id, next, 1024)
            .map_err(|e| unusable(&e))?;
        let Some(last) = events.last() else {
            break;
        };
        next = last.sequence + 1;
        if events.iter().any(|envelope| records_work(&envelope.event)) {
            return Err(continue_there());
        }
    }
    // Edits made outside Rustrace leave no events; the files must still be
    // the starter.
    let files = read_pinned_workspace(&pinned).map_err(|error| -> Box<dyn std::error::Error> {
        format!(
            "this workspace was abandoned and its recovery workspace {} has files Rustrace cannot check ({error}); continue there with `rustrace work ASSIGNMENT.rta --workspace '{}'`",
            directory.display(),
            directory.display()
        )
        .into()
    })?;
    if hash_entries(files.iter().map(|(path, bytes)| (path, bytes.as_slice())))?
        != recovery_metadata.starter_hash
    {
        return Err(continue_there());
    }
    Ok(AbandonedReclaim {
        recovery: Some((recovery, directory)),
        replace_marker: closed,
        root,
    })
}

/// A recovery renamed beside its original, found by its binding to the
/// abandonment record. The scan is bounded.
fn find_bound_recovery(root: &Path, binding: Hash) -> Option<PathBuf> {
    let parent = root.parent()?;
    fs::read_dir(parent)
        .ok()?
        .take(4096)
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path != root && path.join(".rustrace/session.json").is_file())
        .find(|path| {
            read_initial_metadata(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<SessionMetadata>(&bytes).ok())
                .is_some_and(|metadata| metadata.parent_evidence == Some(binding))
        })
}

/// Whether a recovery session's event changed files, ran commands or made a
/// decision; viewing, moving the caret and copying are not work.
fn records_work(event: &Event) -> bool {
    match event {
        Event::SessionStarted(_)
        | Event::SessionResumed(_)
        | Event::SessionEnded(_)
        | Event::FileFocused(_)
        | Event::SelectionChanged(_)
        | Event::ViewportChanged(_)
        | Event::ClipboardCopied(_)
        | Event::PasteRejected(_)
        | Event::LspCompletionRequested(_)
        | Event::WorkspaceCheckpoint(_) => false,
        Event::RecoveryRecorded(recorded) => {
            recorded.decision != RecoveryDecision::AbandonPreserved
        }
        _ => true,
    }
}

fn finish_reclaim(owner: &PinnedJournalFile, reclaim: AbandonedReclaim) -> Result<()> {
    let original = &reclaim.root;
    if let Some((recovery, _)) = &reclaim.recovery {
        let marker = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "decision": "reclaimed_by_original",
            "meaning": "the original workspace resumed; this unused recovery copy is closed",
            "original_directory": original.display().to_string(),
        }))?;
        recovery.publish_artifact("abandoned.json", &marker, reclaim.replace_marker)?;
    }
    // Keep the abandonment record as an unjournaled trace, like the original.
    let mut index = 1;
    while owner
        .display_path()
        .parent()
        .ok_or("state directory missing")?
        .join(format!("abandoned-reclaimed-{index}.json"))
        .try_exists()?
    {
        index += 1;
    }
    owner.rename_artifact(
        "abandoned.json",
        &format!("abandoned-reclaimed-{index}.json"),
    )?;
    Ok(())
}

fn read_initial_metadata(root: &Path) -> Result<Vec<u8>> {
    let pinned = PinnedWorkspaceRoot::open(root)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(root.join(".rustrace/session.json"))?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > METADATA_LIMIT as u64 {
        return Err("invalid bounded session metadata".into());
    }
    let mut bytes = Vec::new();
    file.take(METADATA_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > METADATA_LIMIT {
        return Err("session metadata grew beyond limit".into());
    }
    pinned.verify_binding()?;
    Ok(bytes)
}
fn view_hash(bytes: Option<&Vec<u8>>) -> Option<Hash> {
    bytes.map(|bytes| observation_hash(bytes))
}

fn evidence_bytes(
    saved: &CheckpointSnapshot,
    logical: &CheckpointSnapshot,
    disk: &Files,
) -> Result<Vec<u8>> {
    let observed = files_snapshot(logical.session_id(), logical.event_sequence(), disk)?;
    let mut bytes = b"RUSTEVD1".to_vec();
    for snapshot in [saved, logical, &observed] {
        let encoded = encode_checkpoint(snapshot)?;
        bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&encoded);
    }
    if bytes.len() > ARTIFACT_LIMIT {
        return Err("recovery evidence exceeds limit".into());
    }
    Ok(bytes)
}
fn restore_disk(root: &PinnedWorkspaceRoot, logical: &Files, disk: &Files) -> Result<()> {
    if &read_pinned_workspace(root)? != disk {
        return Err("observed disk changed during recovery; preserve and inspect".into());
    }
    let mut expected = disk.clone();
    for path in disk.keys().filter(|p| !logical.contains_key(*p)) {
        if read_pinned_workspace(root)? != expected {
            return Err("target changed before removal; preserved without retry".into());
        }
        remove_workspace_file_in(root, path)?;
        expected.remove(path);
        process_probe("external-file");
    }
    for (path, bytes) in logical {
        if disk.get(path) != Some(bytes) {
            if read_pinned_workspace(root)? != expected {
                return Err("target changed before restoration; preserved without retry".into());
            }
            if !disk.contains_key(path) {
                create_workspace_file_in(root, path)?;
                expected.insert(path.clone(), Vec::new());
                if read_pinned_workspace(root)? != expected {
                    return Err("newly created target changed; preserve without overwrite".into());
                }
            }
            write_workspace_file_in(root, path, bytes)?;
            expected.insert(path.clone(), bytes.clone());
            process_probe("external-file");
        }
    }
    if &read_pinned_workspace(root)? != logical {
        return Err("restoration publication uncertain; recovery required".into());
    }
    Ok(())
}

/// Exact saved A, logical B and observed C. Absence is represented by a missing path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryEvidence {
    pub saved: Files,
    pub logical: Files,
    pub disk: Files,
}

pub fn read_recovery_evidence(bytes: &[u8]) -> Result<RecoveryEvidence> {
    if bytes.len() > ARTIFACT_LIMIT || !bytes.starts_with(b"RUSTEVD1") {
        return Err("invalid bounded recovery evidence".into());
    }
    let mut remaining = &bytes[8..];
    let mut snapshots = Vec::with_capacity(3);
    for _ in 0..3 {
        let length = u64::from_be_bytes(
            remaining
                .get(..8)
                .ok_or("truncated evidence length")?
                .try_into()?,
        );
        let length = usize::try_from(length)?;
        remaining = &remaining[8..];
        snapshots.push(decode_checkpoint(
            remaining
                .get(..length)
                .ok_or("truncated evidence snapshot")?,
        )?);
        remaining = &remaining[length..];
    }
    if !remaining.is_empty()
        || snapshots
            .iter()
            .any(|s| s.session_id() != snapshots[0].session_id())
    {
        return Err("recovery evidence session/trailing bytes mismatch".into());
    }
    Ok(RecoveryEvidence {
        saved: snapshot_files(&snapshots[0]),
        logical: snapshot_files(&snapshots[1]),
        disk: snapshot_files(&snapshots[2]),
    })
}

// Dedicated probe builds only. The production binary has no environment-driven exits.
#[cfg(feature = "process-probes")]
pub(crate) fn process_probe(stage: &str) {
    if std::env::var("RUSTRACE_CRASH_STAGE").as_deref() == Ok(stage) {
        let marker = std::env::var_os("RUSTRACE_CRASH_MARKER")
            .expect("crash probe requires RUSTRACE_CRASH_MARKER");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .expect("crash probe marker must be created once");
        use std::io::Write;
        writeln!(file, "{stage} {}", std::process::id()).expect("crash probe marker write");
        file.sync_all().expect("crash probe marker sync");
        loop {
            std::thread::park_timeout(Duration::from_secs(60));
        }
    }
    if std::env::var("RUSTRACE_INTERRUPT_AT").as_deref() == Ok(stage) {
        std::process::exit(83);
    }
}
#[cfg(not(feature = "process-probes"))]
pub(crate) fn process_probe(_: &str) {}

#[cfg(test)]
mod external_tests {
    use super::*;
    const MANIFEST: &[u8] = br#"format_version = 1
course_id = "test"
assignment_id = "external"
assignment_version = "1"
title = "External"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> (Self, ProductionSession) {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "rustrace-p2-unit-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&root).unwrap();
            fs::write(root.join("main.rs"), "A").unwrap();
            let mut session = ProductionSession::start(&root, MANIFEST).unwrap();
            session.execute(EditorCommand::Insert('B')).unwrap();
            (Self(root), session)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn own_save_preserves_later_replacement_and_recovers_partial_publication() {
        for external in [false, true] {
            let (dir, session) = Fixture::new();
            session.quit().unwrap();
            // Start a fresh thirty-file assignment, retaining the fixture owner.
            let root = dir.0.join("many");
            fs::create_dir(&root).unwrap();
            for index in 0..30 {
                fs::write(root.join(format!("f{index:02}.rs")), "A").unwrap();
            }
            let mut session = ProductionSession::start(&root, MANIFEST).unwrap();
            for _ in 0..30 {
                session.execute(EditorCommand::Insert('B')).unwrap();
                session.execute(EditorCommand::NextBuffer).unwrap();
            }
            let mut publications = 0;
            let result = session.save_all_with_hook(|_| {
                publications += 1;
                if external && publications == 1 {
                    let replacement = dir.0.join("replacement");
                    fs::write(&replacement, "NEW EXTERNAL C").unwrap();
                    fs::rename(replacement, root.join("f29.rs")).unwrap();
                }
            });
            assert_eq!(result.is_err(), external, "later C must block publication");
            if external {
                assert_eq!(publications, 1);
                assert_eq!(fs::read(root.join("f29.rs")).unwrap(), b"NEW EXTERNAL C");
                assert!(session.capture_boundary().is_err());
                assert!(session.execute(EditorCommand::Insert('X')).is_err());
            }
            // A stopped authority may reject quit's drain; dropping still
            // releases ownership without rewriting its durable intent.
            drop(session);
            let session = ProductionSession::resume(&root, MANIFEST, ResumeChoice::Resume).unwrap();
            assert_eq!(session.external_notice().is_some(), external);
            if external {
                let evidence =
                    read_recovery_evidence(&fs::read(&session.evidence_paths()[0]).unwrap())
                        .unwrap();
                let path = WorkspacePath::new("f29.rs").unwrap();
                assert_eq!(evidence.saved[&path], b"A");
                assert_eq!(evidence.logical[&path], b"BA");
                assert_eq!(evidence.disk[&path], b"NEW EXTERNAL C");
            }
            session.quit().unwrap();
            let views = ProductionSession::inspect(&root).unwrap();
            assert_eq!(views.disk, views.logical);
            assert_eq!(views.saved, views.logical);
        }
    }

    #[test]
    fn reconciliation_preserves_selected_operation_and_pending_delete() {
        for operation in ["rename", "delete", "confirm", "cancel"] {
            let (dir, mut session) = Fixture::new();
            session.create_file("other.rs").unwrap();
            session.execute(EditorCommand::Insert('O')).unwrap();
            if matches!(operation, "rename" | "delete") {
                session.save_all().unwrap();
            }
            session.execute(EditorCommand::PreviousBuffer).unwrap();
            while session.workspace.selected_path().as_str() != "other.rs" {
                session.workspace.select_next();
            }
            if matches!(operation, "confirm" | "cancel") {
                session.delete_selected().unwrap();
                assert!(session.workspace.delete_confirmation_pending());
            }
            fs::write(dir.0.join("main.rs"), "C").unwrap();
            session.recheck_external().unwrap();
            assert_eq!(session.workspace.selected_path().as_str(), "other.rs");
            assert_eq!(session.workspace.active_path().as_str(), "main.rs");
            match operation {
                "rename" => {
                    session.rename_selected("renamed.rs").unwrap();
                }
                "delete" => {
                    session.delete_selected().unwrap();
                }
                "confirm" => {
                    assert!(session.workspace.delete_confirmation_pending());
                    session.confirm_delete().unwrap();
                }
                _ => {
                    session.workspace.cancel_delete();
                }
            }
            assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"BA");
            assert_eq!(dir.0.join("other.rs").exists(), operation == "cancel");
            if operation == "rename" {
                assert_eq!(fs::read(dir.0.join("renamed.rs")).unwrap(), b"O");
            }
        }
    }

    #[test]
    fn newer_observation_is_preserved_before_restore_and_retries_are_bounded() {
        for keep_changing in [false, true] {
            let (dir, mut session) = Fixture::new();
            fs::write(dir.0.join("main.rs"), "C").unwrap();
            let mut attempts = 0;
            let result = session.recheck_with_hook(|stage| {
                if stage == "intent" {
                    attempts += 1;
                    if attempts == 1 || keep_changing {
                        fs::write(dir.0.join("main.rs"), if attempts == 1 { "D" } else { "E" })?;
                    }
                }
                Ok(())
            });
            assert_eq!(attempts, 2);
            assert_eq!(result.is_err(), keep_changing);
            assert_eq!(
                fs::read(dir.0.join("main.rs")).unwrap(),
                if keep_changing {
                    b"E".as_slice()
                } else {
                    b"BA"
                }
            );
            let evidence =
                read_recovery_evidence(&fs::read(&session.evidence_paths()[0]).unwrap()).unwrap();
            assert_eq!(evidence.disk[&WorkspacePath::new("main.rs").unwrap()], b"D");
            let count = fs::read_dir(dir.0.join(".rustrace"))
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_str()
                        .unwrap()
                        .starts_with("evidence-")
                })
                .count();
            assert_eq!(count, 2);
            if keep_changing {
                assert!(session.save_all().is_err());
            }
        }
    }

    #[test]
    fn failures_at_each_reconciliation_boundary_stop_and_resume_safely() {
        for failed_stage in [
            "capture",
            "observation",
            "intent",
            "publication",
            "outcome",
            "baseline",
        ] {
            let (dir, mut session) = Fixture::new();
            fs::write(dir.0.join("main.rs"), "C").unwrap();
            assert!(
                session
                    .recheck_with_hook(|stage| if stage == failed_stage {
                        Err("injected boundary failure".into())
                    } else {
                        Ok(())
                    })
                    .is_err()
            );
            assert!(session.execute(EditorCommand::Insert('!')).is_err());
            assert!(session.capture_boundary().is_err());
            assert_eq!(session.workspace.active_buffer().text(), "BA");
            session.quit().unwrap();
            let session =
                ProductionSession::resume(&dir.0, MANIFEST, ResumeChoice::Resume).unwrap();
            assert_eq!(session.workspace.active_buffer().text(), "BA");
            assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"BA");
        }
    }

    #[test]
    fn polling_is_bounded_and_dropped_hints_are_rechecked() {
        let (dir, mut session) = Fixture::new();
        fs::write(dir.0.join("main.rs"), "C").unwrap();
        for _ in 0..20 {
            session.tick().unwrap();
        }
        assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"C");
        session.last_recheck = Instant::now() - Duration::from_secs(2);
        session.tick().unwrap();
        assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"BA");
        let events = session.health().unwrap().events;
        for _ in 0..20 {
            session.tick().unwrap();
        }
        assert_eq!(session.health().unwrap().events, events);
        fs::write(dir.0.join("main.rs"), "D").unwrap();
        session.capture_boundary().unwrap();
        assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"BA");
    }

    #[test]
    fn unsafe_oversize_and_reserve_failures_never_restore_or_sweep() {
        for mode in ["oversize", "symlink", "reserve"] {
            let (dir, mut session) = Fixture::new();
            fs::write(dir.0.join("main.rs"), "C").unwrap();
            match mode {
                "oversize" => {
                    fs::write(dir.0.join("large.rs"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
                }
                "symlink" => {
                    std::os::unix::fs::symlink(dir.0.join("main.rs"), dir.0.join("link.rs"))
                        .unwrap();
                }
                "reserve" => {
                    fs::OpenOptions::new()
                        .write(true)
                        .open(dir.0.join(".rustrace/reserve.bin"))
                        .unwrap()
                        .set_len(1)
                        .unwrap();
                }
                _ => unreachable!(),
            }
            assert!(session.save_all().is_err(), "{mode}");
            assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"C", "{mode}");
            assert_eq!(session.workspace.active_buffer().text(), "BA");
        }
    }
}
