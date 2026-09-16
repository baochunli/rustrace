//! Immutable production-session finalization and linear revision receipts.
//!
//! The clean manifest and every payload are prepared and validated before the
//! terminal append. The receipt marker is published only after that exact
//! terminal is durable and the SQLite session is ended. Recovery can therefore
//! finish the prepared terminal without consulting the mutable workspace.

use super::{
    ARTIFACT_LIMIT, METADATA_LIMIT, ProductionSession, Result, SessionMetadata, digest,
    load_saved_receipt, process_probe, validate_prefix_for_finalization,
};
use crate::toolchain::{RuntimeToolchainMetadata, ToolchainReport};
use rustrace_journal::{
    CheckpointSnapshot, Journal, MAX_CHECKPOINTS_PER_READ, MAX_EVENTS_PER_READ, StoredCheckpoint,
    decode_checkpoint, encode_checkpoint,
};
use rustrace_model::{
    DecodeOutcome, DecodePolicy, EditOrigin, Event, EventEnvelope, Hash, MAX_IDENTIFIER_BYTES,
    MAX_RPROV_CHECKPOINTS_PER_SEGMENT, MAX_RPROV_EVENTS, MAX_RPROV_EVIDENCE_PER_SEGMENT,
    MAX_RPROV_EVIDENCE_USAGES, MAX_RPROV_EVIDENCE_USAGES_PER_ARTIFACT,
    MAX_RPROV_INITIAL_FILE_BYTES, MAX_RPROV_INITIAL_FILES, MAX_RPROV_INITIAL_WORKSPACE_BYTES,
    MAX_RPROV_MANIFEST_BYTES, MAX_RPROV_METADATA_ENTRY_BYTES, MAX_RPROV_METADATA_PER_SEGMENT,
    MAX_RPROV_SEGMENT_EVENTS_BYTES, MAX_RPROV_SEGMENTS, MAX_RPROV_SOURCE_LINKS_PER_SEGMENT,
    RPROV_FORMAT_VERSION_V1, RecordedEventRef, RprovAssignmentManifestIdentity, RprovCheckpointRef,
    RprovCheckpointRole, RprovEntryKind, RprovEventStreamCompleteness, RprovEventStreamRef,
    RprovEvidenceKind, RprovEvidenceRef, RprovInitialWorkspace, RprovInitialWorkspaceFile,
    RprovInterAttemptTime, RprovInventoryEntry, RprovKnown, RprovLegacyPasteVerification,
    RprovManifest, RprovMetadataRef, RprovPackageState, RprovParentLink, RprovProducer,
    RprovRecoveryGap, RprovSegment, RprovSegmentTime, RprovSourceLink,
    RprovSubmittedSourceComparison, RprovToolVersion, RprovUnavailableAssurance, SessionId,
    WorkspacePath, decode_envelope, decode_rprov_manifest, encode_envelope, encode_rprov_manifest,
    rprov_raw_blake3, validate_rprov_event_stream, validate_rprov_payload,
};
use rustrace_replay::ReplayEngine;
use rustrace_workspace::hash::{
    PinnedJournalFile, PinnedStateDirectory, PinnedWorkspaceRoot, hash_entries,
    read_pinned_workspace,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

const PREPARED_MANIFEST: &str = "finalization-candidate-manifest.json";
const PREPARED_SOURCES: &str = "finalization-sources.json";
const PREPARED_MARKER: &str = "finalization-prepared.json";
const RECEIPT_MARKER: &str = "finalization-receipt.json";
const INCOMPLETE_MARKER: &str = "finalization-incomplete.json";
const PREFIX_EVENTS: &str = "finalization-prefix.jsonl";
const COMPLETE_EVENTS: &str = "finalization-events.jsonl";
const RECOVERY_CAPTURE: &str = "finalization-recovery-capture.json";
const RECOVERY_EVENTS: &str = "finalization-recovery-events.jsonl";
const FINALIZATION_METADATA_LIMIT: usize = MAX_RPROV_MANIFEST_BYTES;
const INCOMPLETE_REASON_BYTES: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalizationPayload {
    pub entry: String,
    pub path: PathBuf,
    pub byte_length: u64,
    pub blake3: Hash,
}

#[derive(Clone, Debug)]
pub struct FinalizationReceipt {
    manifest: RprovManifest,
    payloads: Vec<FinalizationPayload>,
    final_workspace: BTreeMap<WorkspacePath, Vec<u8>>,
    binding: ReceiptBinding,
    workspace_root: PathBuf,
}

impl FinalizationReceipt {
    pub fn manifest(&self) -> &RprovManifest {
        &self.manifest
    }

    pub fn payloads(&self) -> &[FinalizationPayload] {
        &self.payloads
    }

    pub fn payload_path(&self, entry: &str) -> Option<&Path> {
        self.payloads
            .iter()
            .find(|payload| payload.entry == entry)
            .map(|payload| payload.path.as_path())
    }

    /// Reads one receipt-bound local payload through a no-follow bounded handle.
    /// T6.3 can map `entry` to its archive-local path without persisting this
    /// host locator in `.rprov`.
    pub fn read_payload(&self, entry: &str) -> Result<Vec<u8>> {
        let payload = self
            .payloads
            .iter()
            .find(|payload| payload.entry == entry)
            .ok_or_else(|| format!("receipt has no payload for archive entry {entry}"))?;
        read_payload_source(payload)
    }

    pub fn final_workspace(&self) -> &BTreeMap<WorkspacePath, Vec<u8>> {
        &self.final_workspace
    }

    pub(super) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }
}

#[derive(Clone, Debug)]
pub struct IncompleteFinalization {
    pub version: u32,
    pub label: String,
    pub reason: String,
    pub session_id: SessionId,
    pub capture_available: bool,
    capture: Option<Box<RecoveryCapture>>,
}

impl IncompleteFinalization {
    /// Returns an accepted recovery manifest when the retained facts can form
    /// one without inventing unavailable ancestry or finalization claims.
    pub fn manifest(&self) -> Option<&RprovManifest> {
        self.capture
            .as_ref()
            .and_then(|capture| capture.manifest.as_ref())
    }

    /// Exact immutable payload sources retained for downstream recovery export.
    pub fn payloads(&self) -> &[FinalizationPayload] {
        self.capture
            .as_ref()
            .map_or(&[], |capture| capture.payloads.as_slice())
    }

    pub fn read_payload(&self, entry: &str) -> Result<Vec<u8>> {
        let payload = self
            .payloads()
            .iter()
            .find(|payload| payload.entry == entry)
            .ok_or_else(|| {
                format!("incomplete capture has no payload for archive entry {entry}")
            })?;
        read_payload_source(payload)
    }

    /// The last replay-certified boundary checkpoint, even when the accepted
    /// manifest must keep its final-tree fact unknown.
    pub fn final_workspace(&self) -> Option<&BTreeMap<WorkspacePath, Vec<u8>>> {
        self.capture
            .as_ref()
            .map(|capture| &capture.final_workspace)
    }
}

#[derive(Clone, Debug)]
struct RecoveryCapture {
    manifest: Option<RprovManifest>,
    payloads: Vec<FinalizationPayload>,
    final_workspace: BTreeMap<WorkspacePath, Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedIncompleteFinalization {
    version: u32,
    label: String,
    reason: String,
    session_id: SessionId,
    capture_available: bool,
}

#[derive(Clone, Debug)]
pub enum FinalizationStatus {
    Finalized(Box<FinalizationReceipt>),
    Incomplete(IncompleteFinalization),
}

#[derive(Clone, Debug)]
pub(crate) struct ReadOnlyFinalizationReceipt {
    pub(crate) student_id: String,
    pub(crate) latest_session_id: SessionId,
    pub(crate) final_tree_hash: Hash,
    pub(crate) terminal_chain_hash: Hash,
    pub(crate) aggregate_event_count: u64,
    pub(crate) ancestry_session_ids: Vec<SessionId>,
}

#[derive(Clone, Debug)]
pub(crate) enum ReadOnlyFinalizationStatus {
    Unfinished,
    Prepared {
        session_id: SessionId,
    },
    Incomplete {
        reason: String,
        session_id: SessionId,
        capture_available: bool,
    },
    Finalized(ReadOnlyFinalizationReceipt),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedSources {
    version: u32,
    payloads: Vec<FinalizationPayload>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptBinding {
    version: u32,
    session_id: SessionId,
    prefix_event_count: u64,
    prefix_event_hash: Hash,
    prefix_byte_length: u64,
    prefix_blake3: Hash,
    terminal_event_count: u64,
    terminal_event_hash: Hash,
    final_tree_hash: Hash,
    manifest_blake3: Hash,
    sources_blake3: Hash,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedMarker {
    version: u32,
    label: String,
    binding: ReceiptBinding,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalReceiptMarker {
    version: u32,
    label: String,
    binding: ReceiptBinding,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisionLink {
    version: u32,
    kind: String,
    parent_root: PathBuf,
    parent_session_id: SessionId,
    parent_terminal_event_hash: Hash,
    parent_final_tree_hash: Hash,
    parent_manifest_blake3: Hash,
    parent_sources_blake3: Hash,
    #[serde(default)]
    original_starter_tree_hash: Option<Hash>,
    #[serde(default)]
    initial_workspace: Option<RprovInitialWorkspace>,
    #[serde(default)]
    initial_inventory: Vec<RprovInventoryEntry>,
}

pub(super) fn selected_assignment_matches(
    root: &Path,
    metadata: &SessionMetadata,
    manifest_bytes: &[u8],
    original_starter_tree_hash: Hash,
    test_case_suite_hash: Option<Hash>,
) -> Result<bool> {
    if metadata.manifest_hash != digest(manifest_bytes)
        || metadata.test_case_suite_hash != test_case_suite_hash
    {
        return Ok(false);
    }
    let Some(expected_link_hash) = metadata.parent_evidence else {
        return Ok(metadata.starter_hash == original_starter_tree_hash);
    };
    let state = PinnedWorkspaceRoot::open(root)?.open_existing_state_directory()?;
    let link_bytes = match state.read_artifact("parent.json", METADATA_LIMIT) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(false),
    };
    if digest(&link_bytes) != expected_link_hash {
        return Ok(false);
    }
    let value: serde_json::Value = match serde_json::from_slice(&link_bytes) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    if value.get("kind").and_then(serde_json::Value::as_str) != Some("finalized_revision") {
        return Ok(metadata.starter_hash == original_starter_tree_hash);
    }
    let link: RevisionLink = match serde_json::from_value(value) {
        Ok(link) => link,
        Err(_) => return Ok(false),
    };
    Ok(link.version == 1
        && link.kind == "finalized_revision"
        && link.original_starter_tree_hash == Some(original_starter_tree_hash)
        && metadata.starter_hash == link.parent_final_tree_hash)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FinishBoundary {
    Complete,
    #[cfg(test)]
    AfterCapture,
    #[cfg(test)]
    AfterTerminal,
    #[cfg(test)]
    BeforeReceipt,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(super) enum FinalizationInterruption {
    AfterCapture,
    AfterTerminal,
    BeforeReceipt,
}

#[derive(Clone)]
struct CapturedCurrent {
    segment: RprovSegment,
    inventory: Vec<RprovInventoryEntry>,
    payloads: Vec<FinalizationPayload>,
    publications: Vec<(String, Vec<u8>)>,
    prefix_bytes: Vec<u8>,
    initial_workspace: Option<RprovInitialWorkspace>,
    final_workspace: BTreeMap<WorkspacePath, Vec<u8>>,
    gaps: Vec<RprovRecoveryGap>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedCurrentCapture {
    version: u32,
    label: String,
    student_id: String,
    maximum_aggregate_event_count: u64,
    had_parent: bool,
    original_starter_tree_hash: Option<Hash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    test_case_suite_hash: Option<Hash>,
    assignment_manifest: RprovAssignmentManifestIdentity,
    initial_workspace: Option<RprovInitialWorkspace>,
    segment: RprovSegment,
    inventory: Vec<RprovInventoryEntry>,
    payloads: Vec<FinalizationPayload>,
    gaps: Vec<RprovRecoveryGap>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParentEvidenceKind {
    None,
    Abandonment,
    FinalizedRevision,
}

struct CaptureCurrentInput<'a> {
    metadata: &'a SessionMetadata,
    ordinal: u32,
    prefix_events: &'a [EventEnvelope],
    terminal: Option<&'a EventEnvelope>,
    checkpoints: &'a [StoredCheckpoint],
    parent_link: Option<RprovParentLink>,
    original_starter_tree_hash: Hash,
    include_initial_workspace: bool,
    event_artifact: &'a str,
    artifact_tag: &'a str,
}

struct RevisionSeed {
    original_starter_tree_hash: Hash,
    initial_workspace: RprovInitialWorkspace,
    inventory: Vec<RprovInventoryEntry>,
    payloads: Vec<FinalizationPayload>,
}

struct RecoveryAncestry {
    receipt: FinalizationReceipt,
    link: RprovParentLink,
    gaps: Vec<RprovRecoveryGap>,
}

struct CapturedRuntimeMetadata {
    refs: Vec<RprovMetadataRef>,
    inventory: Vec<RprovInventoryEntry>,
    payloads: Vec<FinalizationPayload>,
    producer: RprovProducer,
}

struct CapturedEvidence {
    refs: Vec<RprovEvidenceRef>,
    inventory: Vec<RprovInventoryEntry>,
    payloads: Vec<FinalizationPayload>,
    gaps: Vec<RprovRecoveryGap>,
}

impl ProductionSession {
    /// Consumes the active controller and returns a receipt only after its exact
    /// prepared terminal event and ended-session bit are durable.
    pub fn finalize(self, student_id: &str) -> Result<FinalizationReceipt> {
        self.finalize_with_limit(student_id, MAX_RPROV_EVENTS)
    }

    fn finalize_with_limit(
        mut self,
        student_id: &str,
        maximum_aggregate_event_count: u64,
    ) -> Result<FinalizationReceipt> {
        let result = self
            .prepare_finalization(student_id, maximum_aggregate_event_count)
            .and_then(|marker| {
                finish_prepared(
                    &mut self.effects.0.borrow_mut().owner,
                    &marker,
                    FinishBoundary::Complete,
                )
            });
        match result {
            Ok(receipt) => {
                self.effects.0.borrow_mut().owner.release_ownership()?;
                Ok(receipt)
            }
            Err(error) => {
                let capture_available =
                    artifact_exists(&self.effects.0.borrow().owner, RECOVERY_CAPTURE)
                        .unwrap_or(false);
                let _ = publish_incomplete_marker(
                    &self.effects.0.borrow().owner,
                    &self.metadata.session_id,
                    &error.to_string(),
                    capture_available,
                );
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub(super) fn finalize_with_aggregate_limit(
        self,
        student_id: &str,
        maximum: u64,
    ) -> Result<FinalizationReceipt> {
        self.finalize_with_limit(student_id, maximum)
    }

    /// Completes or loads a prepared finalization without reading the live
    /// source tree. Invalid assurance is returned as visibly incomplete.
    pub fn recover_finalization(root: &Path) -> Result<FinalizationStatus> {
        Self::recover_finalization_with_actual_limit(root, MAX_RPROV_EVENTS)
    }

    /// Returns `None` for an ordinary mutable session without publishing any
    /// recovery marker. Existing receipt/preparation/recovery state is loaded
    /// through the normal validating recovery path.
    pub(super) fn recover_finalization_if_started(
        root: &Path,
    ) -> Result<Option<FinalizationStatus>> {
        let metadata: SessionMetadata =
            serde_json::from_slice(&super::read_initial_metadata(root)?)?;
        let pinned = PinnedWorkspaceRoot::open(root)?;
        let mut owner = pinned
            .open_state_directory()?
            .open_journal_file(&metadata.session_id)?;
        let started = [
            RECEIPT_MARKER,
            PREPARED_MARKER,
            RECOVERY_CAPTURE,
            INCOMPLETE_MARKER,
        ]
        .into_iter()
        .try_fold(false, |found, name| {
            artifact_exists(&owner, name).map(|exists| found || exists)
        });
        owner.release_ownership()?;
        if started? {
            Self::recover_finalization(root).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Inspects only bounded, already-published artifacts. It never opens
    /// SQLite, completes a prepared finalization, or publishes recovery state.
    pub(crate) fn inspect_finalization_read_only(
        root: &Path,
    ) -> Result<ReadOnlyFinalizationStatus> {
        let metadata: SessionMetadata =
            serde_json::from_slice(&super::read_initial_metadata(root)?)?;
        let pinned = PinnedWorkspaceRoot::open(root)?;
        let state = pinned.open_existing_state_directory()?;
        if state.artifact_exists(RECEIPT_MARKER)? {
            return load_published_summary(&state, &metadata)
                .map(ReadOnlyFinalizationStatus::Finalized);
        }
        if state.artifact_exists(PREPARED_MARKER)? {
            let prepared = read_prepared_state(&state)?;
            if prepared.binding.session_id != metadata.session_id {
                return Err("prepared finalization has the wrong session identity".into());
            }
            return Ok(ReadOnlyFinalizationStatus::Prepared {
                session_id: prepared.binding.session_id,
            });
        }
        if state.artifact_exists(INCOMPLETE_MARKER)? {
            let incomplete = read_incomplete_state(&state)?;
            if incomplete.session_id != metadata.session_id {
                return Err("incomplete finalization has the wrong session identity".into());
            }
            return Ok(ReadOnlyFinalizationStatus::Incomplete {
                reason: incomplete.reason,
                session_id: incomplete.session_id,
                capture_available: incomplete.capture_available,
            });
        }
        if state.artifact_exists(RECOVERY_CAPTURE)? {
            return Ok(ReadOnlyFinalizationStatus::Incomplete {
                reason: "immutable recovery capture exists without a published finalization marker"
                    .to_owned(),
                session_id: metadata.session_id,
                capture_available: true,
            });
        }
        Ok(ReadOnlyFinalizationStatus::Unfinished)
    }

    fn recover_finalization_with_actual_limit(
        root: &Path,
        maximum_aggregate_event_count: u64,
    ) -> Result<FinalizationStatus> {
        let metadata: SessionMetadata =
            serde_json::from_slice(&super::read_initial_metadata(root)?)?;
        let pinned = PinnedWorkspaceRoot::open(root)?;
        let mut owner = pinned
            .open_state_directory()?
            .open_journal_file(&metadata.session_id)?;
        owner.secure_reserve(super::SessionBudgets::default().reserve_bytes, false)?;

        let recovered = (|| -> Result<FinalizationStatus> {
            if artifact_exists(&owner, RECEIPT_MARKER)? {
                let receipt = load_published_receipt(&owner)?;
                verify_local_terminal(&owner, &receipt)?;
                return Ok(FinalizationStatus::Finalized(Box::new(receipt)));
            }
            if !artifact_exists(&owner, PREPARED_MARKER)? {
                let reason = read_incomplete(&owner)
                    .map(|value| value.reason)
                    .unwrap_or_else(|_| {
                        if artifact_exists(&owner, RECOVERY_CAPTURE).unwrap_or(false) {
                            "clean finalization was not prepared after the immutable current-prefix capture"
                                .to_owned()
                        } else {
                            "no complete durable finalization capture is available".to_owned()
                        }
                    });
                let incomplete =
                    build_incomplete(&owner, &metadata, &reason, maximum_aggregate_event_count)?;
                return Ok(FinalizationStatus::Incomplete(incomplete));
            }
            if let Err(error) = super::command::require_inactive_marker(&owner, &metadata) {
                let incomplete = build_incomplete(
                    &owner,
                    &metadata,
                    &format!("mutator cleanup is not confirmed: {error}"),
                    maximum_aggregate_event_count,
                )?;
                return Ok(FinalizationStatus::Incomplete(incomplete));
            }
            let prepared = read_prepared(&owner)?;
            finish_prepared(&mut owner, &prepared, FinishBoundary::Complete)
                .map(Box::new)
                .map(FinalizationStatus::Finalized)
        })();

        let result = match recovered {
            Ok(status) => Ok(status),
            Err(error) => {
                let incomplete = build_incomplete(
                    &owner,
                    &metadata,
                    &format!("prepared finalization could not be certified: {error}"),
                    maximum_aggregate_event_count,
                )?;
                Ok(FinalizationStatus::Incomplete(incomplete))
            }
        };
        owner.release_ownership()?;
        result
    }

    #[cfg(test)]
    pub(super) fn recover_finalization_with_actual_aggregate_limit(
        root: &Path,
        maximum: u64,
    ) -> Result<FinalizationStatus> {
        Self::recover_finalization_with_actual_limit(root, maximum)
    }

    /// Starts a separate session from the exact captured parent final tree.
    pub fn start_revision(parent_root: &Path, root: &Path, manifest_bytes: &[u8]) -> Result<Self> {
        let parent_root = fs::canonicalize(parent_root)?;
        let parent = load_finalized_root(&parent_root)?;
        let parent_manifest = parent.manifest();
        if manifest_bytes.len() as u64 != parent_manifest.assignment_manifest.byte_length
            || rprov_raw_blake3(manifest_bytes) != parent_manifest.assignment_manifest.blake3
        {
            return Err("revision assignment manifest identity differs from its parent".into());
        }
        let pinned = PinnedWorkspaceRoot::open(root)?;
        let initial = read_pinned_workspace(&pinned)?;
        if initial != parent.final_workspace {
            return Err("revision initial workspace is not the exact parent final tree".into());
        }
        let tip = parent_manifest
            .segments
            .last()
            .ok_or("parent receipt has no segment")?;
        let terminal = tip
            .terminal_event_hash
            .known()
            .ok_or("parent receipt has no terminal event")?;
        let final_tree = tip
            .final_tree_hash
            .known()
            .ok_or("parent receipt has no final tree")?;
        let mut initial_inventory = BTreeMap::new();
        let mut seed_publications = BTreeMap::new();
        for file in &parent_manifest.initial_workspace.files {
            let declaration = parent_manifest
                .inventory
                .iter()
                .find(|entry| entry.path == file.entry)
                .ok_or("parent initial workspace has no inventory declaration")?;
            if declaration.kind != RprovEntryKind::InitialWorkspaceBlob {
                return Err("parent initial workspace declaration has the wrong kind".into());
            }
            initial_inventory
                .entry(declaration.path.clone())
                .or_insert_with(|| declaration.clone());
            seed_publications
                .entry(declaration.blake3)
                .or_insert(parent.read_payload(&file.entry)?);
        }
        let initial_inventory = initial_inventory.into_values().collect();
        let link = RevisionLink {
            version: 1,
            kind: "finalized_revision".to_owned(),
            parent_root,
            parent_session_id: tip.session_id.clone(),
            parent_terminal_event_hash: *terminal,
            parent_final_tree_hash: *final_tree,
            parent_manifest_blake3: parent.binding.manifest_blake3,
            parent_sources_blake3: parent.binding.sources_blake3,
            original_starter_tree_hash: Some(parent_manifest.original_starter_tree_hash),
            initial_workspace: Some(parent_manifest.initial_workspace.clone()),
            initial_inventory,
        };
        let link_bytes = serde_json::to_vec(&link)?;
        if link_bytes.len() > METADATA_LIMIT {
            return Err("revision recovery seed exceeds the bounded metadata limit".into());
        }
        let session = Self::start_linked(
            root,
            manifest_bytes,
            Some(link_bytes),
            parent_manifest.test_case_suite_hash,
        )?;
        if session.metadata.starter_hash != *final_tree {
            return Err("revision genesis does not bind the parent final tree".into());
        }
        let seed_bytes = seed_publications.values().try_fold(0_u64, |total, bytes| {
            total
                .checked_add(bytes.len() as u64)
                .ok_or("revision recovery-seed byte count overflow")
        })?;
        session.effects.0.borrow().headroom(seed_bytes)?;
        for (hash, bytes) in seed_publications {
            session.effects.0.borrow().owner.publish_artifact(
                &origin_artifact_name(hash),
                &bytes,
                false,
            )?;
        }
        Ok(session)
    }

    #[cfg(test)]
    pub(super) fn finalize_interrupted(
        mut self,
        student_id: &str,
        stage: FinalizationInterruption,
    ) -> Result<()> {
        let marker = self.prepare_finalization(student_id, MAX_RPROV_EVENTS)?;
        let boundary = match stage {
            FinalizationInterruption::AfterCapture => FinishBoundary::AfterCapture,
            FinalizationInterruption::AfterTerminal => FinishBoundary::AfterTerminal,
            FinalizationInterruption::BeforeReceipt => FinishBoundary::BeforeReceipt,
        };
        finish_prepared(&mut self.effects.0.borrow_mut().owner, &marker, boundary)?;
        Err(format!("intentional T6.2 interruption at {stage:?}").into())
    }

    fn prepare_finalization(
        &mut self,
        student_id: &str,
        maximum_aggregate_event_count: u64,
    ) -> Result<PreparedMarker> {
        validate_student_id(student_id)?;
        self.shutdown_command()?;
        if self.effects.0.borrow().command_active
            || !self.command_terminal_restoration_safe()
            || self.unpublished_command_capture().is_some()
        {
            return Err("command mutator cleanup is not confirmed before finalization".into());
        }
        self.clear_completion();
        let had_language_service = self.language_service.is_some();
        if !self
            .language_service
            .take()
            .is_none_or(|mut service| service.shutdown())
        {
            return Err(
                "language-service mutator cleanup is not confirmed before finalization".into(),
            );
        }
        if had_language_service {
            self.recheck_external()?;
        }
        if self.workspace.confirmation_pending() {
            return Err("pending workspace confirmation blocks finalization".into());
        }
        self.clear_clipboard();
        self.capture_boundary()?;
        self.drain()?;

        let writer = self
            .effects
            .0
            .borrow_mut()
            .writer
            .take()
            .ok_or("journal writer is already closed")?;
        writer.shutdown()?;

        let authority = self.effects.0.borrow();
        authority.owner.verify()?;
        if authority.command_active {
            return Err("a mutator still owns the workspace after shutdown".into());
        }
        let state_directory = authority
            .owner
            .display_path()
            .parent()
            .ok_or("session state directory is missing")?
            .to_path_buf();
        let mut journal = Journal::open_retained_no_follow(authority.owner.display_path())?;
        let baseline = load_saved_receipt(&authority.owner, &mut journal, &self.metadata)?;
        let prefix = validate_prefix_for_finalization(
            &mut journal,
            &self.metadata,
            &baseline,
            &authority.owner,
        )?;
        let session_state = journal.inspect_session(&self.metadata.session_id)?;
        if session_state.ended || prefix.replay.is_terminal() {
            return Err("session already has terminal finalization state".into());
        }
        if prefix.replay.controlled_command_pending() {
            return Err("controlled command evidence remains unfinished".into());
        }
        let disk = read_pinned_workspace(self.workspace.root_authority())?;
        let logical = self.workspace.logical_files()?;
        if disk != logical || prefix.replay.workspace_state().files() != &logical {
            return Err("finalization requires one verified D=S=L durable workspace".into());
        }

        let prefix_events = read_all_events(&mut journal, &self.metadata.session_id)?;
        if prefix_events.len() as u64 != prefix.sequence
            || prefix_events.last().map(|event| event.event_hash) != Some(prefix.hash)
        {
            return Err("fixed journal prefix disagrees with replay verification".into());
        }
        let parent_evidence_kind = classify_parent_evidence(&self.metadata, &prefix_events)?;
        let terminal_count = prefix
            .sequence
            .checked_add(1)
            .ok_or("terminal event count overflow")?;
        if terminal_count > MAX_RPROV_EVENTS {
            return Err("segment terminal event exceeds package event limit".into());
        }
        let terminal = EventEnvelope {
            format_version: 1,
            session_id: self.metadata.session_id.clone(),
            sequence: terminal_count,
            monotonic_millis: prefix.millis,
            wall_clock_utc: None,
            previous_event_hash: Hash::zero(),
            event_hash: Hash::zero(),
            event: Event::SubmissionFinalized(rustrace_model::SubmissionFinalized {
                final_workspace_hash: prefix.replay.current_workspace_hash(),
                event_count: terminal_count,
                clean: true,
                warnings: Vec::new(),
            }),
        }
        .seal(prefix.hash)?;
        let mut terminal_replay = prefix.replay.clone();
        terminal_replay.apply(&terminal)?;

        let checkpoints = read_all_checkpoints(&mut journal, &self.metadata.session_id)?;
        process_probe("finalization-before-capture");
        let had_parent = parent_evidence_kind == ParentEvidenceKind::FinalizedRevision;
        let original_starter_tree_hash = (!had_parent).then_some(self.metadata.starter_hash);
        let mut current = capture_current_segment(
            &authority.owner,
            &state_directory,
            CaptureCurrentInput {
                metadata: &self.metadata,
                ordinal: 1,
                prefix_events: &prefix_events,
                terminal: None,
                checkpoints: &checkpoints,
                parent_link: None,
                // The outer Option is authoritative for raw recovery. Zero is
                // an inert internal sentinel and is never emitted in a manifest.
                original_starter_tree_hash: original_starter_tree_hash.unwrap_or_else(Hash::zero),
                include_initial_workspace: !had_parent,
                event_artifact: RECOVERY_EVENTS,
                artifact_tag: "finalization-recovery",
            },
        )?;
        if current.final_workspace != logical {
            return Err("captured final checkpoint differs from the verified live boundary".into());
        }
        let manifest_bytes = authority
            .owner
            .read_artifact("manifest.toml", METADATA_LIMIT)?;
        let assignment_manifest = RprovAssignmentManifestIdentity {
            format_version: 1,
            byte_length: manifest_bytes.len() as u64,
            blake3: rprov_raw_blake3(&manifest_bytes),
        };
        let mut persisted_current = PersistedCurrentCapture {
            version: 1,
            label: "IMMUTABLE CURRENT PREFIX".to_owned(),
            student_id: student_id.to_owned(),
            maximum_aggregate_event_count,
            had_parent,
            original_starter_tree_hash,
            test_case_suite_hash: self.metadata.test_case_suite_hash,
            assignment_manifest: assignment_manifest.clone(),
            initial_workspace: current.initial_workspace.clone(),
            segment: current.segment.clone(),
            inventory: current.inventory.clone(),
            payloads: current.payloads.clone(),
            gaps: current.gaps.clone(),
        };
        publish_current_capture(&authority.owner, &authority, &current, &persisted_current)?;

        // The raw current boundary above is the recovery commit point. Only
        // after it exists may revision seed/link parsing fail. A certified
        // seed enriches the marker atomically without changing captured bytes;
        // interruption leaves either the truthful raw or enriched marker.
        let revision_seed = if had_parent {
            load_revision_seed(&authority.owner, &self.metadata)?
        } else {
            None
        };
        if let Some(seed) = &revision_seed {
            current.segment.original_starter_tree_hash = seed.original_starter_tree_hash;
            current.inventory.extend(seed.inventory.clone());
            current.payloads.extend(seed.payloads.clone());
            current
                .inventory
                .sort_by(|left, right| left.path.cmp(&right.path));
            current
                .payloads
                .sort_by(|left, right| left.entry.cmp(&right.entry));
            current.initial_workspace = Some(seed.initial_workspace.clone());
            persisted_current.original_starter_tree_hash = Some(seed.original_starter_tree_hash);
            persisted_current.initial_workspace = Some(seed.initial_workspace.clone());
            persisted_current.segment = current.segment.clone();
            persisted_current.inventory = current.inventory.clone();
            persisted_current.payloads = current.payloads.clone();
            validate_raw_current_capture(&authority.owner, &persisted_current)?;
            publish_enriched_current_capture(&authority.owner, &authority, &persisted_current)?;
        }
        if !current.gaps.is_empty() {
            return Err(
                "referenced evidence is unavailable from the immutable current prefix".into(),
            );
        }

        // External ancestry is consulted only after the complete current prefix
        // and its replay boundary are durably retained above.
        let (parent, parent_link) = if parent_evidence_kind == ParentEvidenceKind::FinalizedRevision
        {
            load_parent_ancestry(&authority.owner, &self.metadata)?
        } else {
            (None, None)
        };
        if let Some(parent) = &parent {
            if parent.manifest.student_id != student_id {
                return Err("revision student identifier differs from its parent receipt".into());
            }
            require_assignment_match(&parent.manifest, &self.metadata, &authority.owner)?;
            let child_initial = checkpoints
                .first()
                .ok_or("revision child is missing its genesis checkpoint")?;
            if parent.final_workspace != checkpoint_files(&child_initial.snapshot) {
                return Err("revision child start no longer equals the parent final tree".into());
            }
        }

        let ordinal = parent
            .as_ref()
            .map_or(1, |receipt| receipt.manifest.segments.len() + 1);
        if ordinal > MAX_RPROV_SEGMENTS {
            return Err("revision history exceeds the package segment limit".into());
        }
        let ordinal = u32::try_from(ordinal).map_err(|_| "segment ordinal overflow")?;
        let captured = complete_current_capture(
            &current,
            &state_directory,
            &terminal,
            ordinal,
            parent_link,
            parent
                .as_ref()
                .map_or(self.metadata.starter_hash, |receipt| {
                    receipt.manifest.original_starter_tree_hash
                }),
        )?;

        let mut inventory = parent
            .as_ref()
            .map_or_else(Vec::new, |receipt| receipt.manifest.inventory.clone());
        inventory.extend(
            captured
                .inventory
                .iter()
                .filter(|entry| {
                    parent.is_none() || entry.kind != RprovEntryKind::InitialWorkspaceBlob
                })
                .cloned(),
        );
        inventory.sort_by(|left, right| left.path.cmp(&right.path));
        let mut payloads = parent
            .as_ref()
            .map_or_else(Vec::new, |receipt| receipt.payloads.clone());
        payloads.extend(
            captured
                .payloads
                .iter()
                .filter(|payload| {
                    parent.is_none() || !payload.entry.starts_with("initial-workspace/blobs/")
                })
                .cloned(),
        );
        payloads.sort_by(|left, right| left.entry.cmp(&right.entry));
        let mut segments = parent
            .as_ref()
            .map_or_else(Vec::new, |receipt| receipt.manifest.segments.clone());
        segments.push(captured.segment);
        let aggregate_event_count = checked_aggregate_event_count(
            segments.iter().map(|segment| segment.inclusive_event_count),
        )?;
        if aggregate_event_count > maximum_aggregate_event_count {
            return Err("revision history exceeds the configured aggregate event limit".into());
        }
        let original_starter_tree_hash = parent
            .as_ref()
            .map_or(self.metadata.starter_hash, |receipt| {
                receipt.manifest.original_starter_tree_hash
            });
        let initial_workspace = match &parent {
            Some(receipt) => receipt.manifest.initial_workspace.clone(),
            None => captured
                .initial_workspace
                .clone()
                .ok_or("root capture is missing its initial workspace")?,
        };
        let producer = segments
            .last()
            .ok_or("candidate has no current segment")?
            .producer
            .clone();
        let manifest = RprovManifest {
            format_version: RPROV_FORMAT_VERSION_V1,
            package_state: RprovPackageState::CleanFinalized,
            submitted_source_comparison: RprovSubmittedSourceComparison::UnavailableStandalone,
            course_id: self.metadata.course_id.clone(),
            assignment_id: self.metadata.assignment_id.clone(),
            assignment_version: self.metadata.assignment_version.clone(),
            student_id: student_id.to_owned(),
            latest_session_id: self.metadata.session_id.clone(),
            original_starter_tree_hash,
            test_case_suite_hash: self.metadata.test_case_suite_hash,
            final_tree_hash: RprovKnown::Known {
                value: terminal_replay.current_workspace_hash(),
            },
            aggregate_event_count,
            producer,
            assignment_manifest,
            initial_workspace,
            segments,
            inventory,
        };
        let candidate_manifest = encode_rprov_manifest(&manifest)?;
        let sources = PersistedSources {
            version: 1,
            payloads,
        };
        let sources_bytes = serde_json::to_vec(&sources)?;
        if sources_bytes.len() > FINALIZATION_METADATA_LIMIT {
            return Err("finalization payload-source receipt exceeds its bounded limit".into());
        }

        let binding = ReceiptBinding {
            version: 1,
            session_id: self.metadata.session_id.clone(),
            prefix_event_count: prefix.sequence,
            prefix_event_hash: prefix.hash,
            prefix_byte_length: captured.prefix_bytes.len() as u64,
            prefix_blake3: rprov_raw_blake3(&captured.prefix_bytes),
            terminal_event_count: terminal.sequence,
            terminal_event_hash: terminal.event_hash,
            final_tree_hash: terminal_replay.current_workspace_hash(),
            manifest_blake3: rprov_raw_blake3(&candidate_manifest),
            sources_blake3: rprov_raw_blake3(&sources_bytes),
        };
        let marker = PreparedMarker {
            version: 1,
            label: "PREPARED CAPTURE - NOT FINALIZED".to_owned(),
            binding: binding.clone(),
        };
        let marker_bytes = serde_json::to_vec(&marker)?;
        let publication_bytes = captured
            .publications
            .iter()
            .try_fold(0_u64, |total, (_, bytes)| {
                total
                    .checked_add(bytes.len() as u64)
                    .ok_or("capture byte count overflow")
            })?
            .checked_add(captured.prefix_bytes.len() as u64)
            .and_then(|total| total.checked_add(candidate_manifest.len() as u64))
            .and_then(|total| total.checked_add(sources_bytes.len() as u64))
            .and_then(|total| total.checked_add(marker_bytes.len() as u64))
            .ok_or("capture byte count overflow")?;
        authority.headroom(publication_bytes)?;
        for (name, bytes) in &captured.publications {
            authority.owner.publish_artifact(name, bytes, false)?;
        }
        authority
            .owner
            .publish_artifact(PREFIX_EVENTS, &captured.prefix_bytes, false)?;
        let candidate = validate_receipt_parts(
            &authority.owner,
            manifest,
            sources.payloads.clone(),
            binding,
        )?;
        if candidate.final_workspace != captured.final_workspace {
            return Err("candidate replay does not equal its durable final capture".into());
        }
        authority
            .owner
            .publish_artifact(PREPARED_MANIFEST, &candidate_manifest, false)?;
        authority
            .owner
            .publish_artifact(PREPARED_SOURCES, &sources_bytes, false)?;
        authority
            .owner
            .publish_artifact(PREPARED_MARKER, &marker_bytes, false)?;
        process_probe("finalization-capture");
        Ok(marker)
    }
}

fn capture_current_segment(
    owner: &PinnedJournalFile,
    state_directory: &Path,
    input: CaptureCurrentInput<'_>,
) -> Result<CapturedCurrent> {
    let CaptureCurrentInput {
        metadata,
        ordinal,
        prefix_events,
        terminal,
        checkpoints,
        parent_link,
        original_starter_tree_hash,
        include_initial_workspace,
        event_artifact,
        artifact_tag,
    } = input;
    if checkpoints.len() < 2 || checkpoints.len() > MAX_RPROV_CHECKPOINTS_PER_SEGMENT {
        return Err(
            "finalization requires distinct initial/final full checkpoints within limits".into(),
        );
    }
    let prefix_bytes = encode_event_stream(prefix_events)?;
    let mut event_bytes = prefix_bytes.clone();
    if let Some(terminal) = terminal {
        event_bytes.extend_from_slice(&encode_envelope(terminal)?);
        event_bytes.push(b'\n');
    }
    if event_bytes.len() as u64 > MAX_RPROV_SEGMENT_EVENTS_BYTES {
        return Err("captured event stream exceeds the segment byte limit".into());
    }

    let mut publications = vec![(event_artifact.to_owned(), event_bytes.clone())];
    let event_entry = format!("segments/{ordinal:04}/events.jsonl");
    let event_digest = rprov_raw_blake3(&event_bytes);
    let mut inventory = vec![RprovInventoryEntry {
        path: event_entry.clone(),
        byte_length: event_bytes.len() as u64,
        blake3: event_digest,
        kind: RprovEntryKind::Events,
    }];
    let mut payloads = vec![FinalizationPayload {
        entry: event_entry.clone(),
        path: state_directory.join(event_artifact),
        byte_length: event_bytes.len() as u64,
        blake3: event_digest,
    }];
    let event_by_sequence = prefix_events
        .iter()
        .chain(terminal)
        .map(|event| (event.sequence, event))
        .collect::<BTreeMap<_, _>>();

    let mut checkpoint_refs = Vec::with_capacity(checkpoints.len());
    for (index, checkpoint) in checkpoints.iter().enumerate() {
        let sequence = checkpoint.owning_event.sequence;
        if event_by_sequence.get(&sequence).copied() != Some(&checkpoint.owning_event) {
            return Err("checkpoint owner differs from the fixed event prefix".into());
        }
        let bytes = encode_checkpoint(&checkpoint.snapshot)?;
        let digest = rprov_raw_blake3(&bytes);
        let local_name = format!("{artifact_tag}-checkpoint-{sequence:020}.rcpk");
        let entry = format!("segments/{ordinal:04}/checkpoints/{sequence:020}.rcpk");
        let role = if index == 0 {
            RprovCheckpointRole::Initial
        } else if terminal.is_some() && index + 1 == checkpoints.len() {
            RprovCheckpointRole::Final
        } else {
            RprovCheckpointRole::Accepted
        };
        checkpoint_refs.push(RprovCheckpointRef {
            role,
            format_version: 1,
            entry: entry.clone(),
            byte_length: bytes.len() as u64,
            blake3: digest,
            owner: event_reference(&checkpoint.owning_event),
            workspace_hash: checkpoint.snapshot.workspace_hash(),
        });
        inventory.push(RprovInventoryEntry {
            path: entry.clone(),
            byte_length: bytes.len() as u64,
            blake3: digest,
            kind: RprovEntryKind::Checkpoint,
        });
        payloads.push(FinalizationPayload {
            entry,
            path: state_directory.join(&local_name),
            byte_length: bytes.len() as u64,
            blake3: digest,
        });
        publications.push((local_name, bytes));
    }
    let initial_checkpoint = checkpoints.first().ok_or("missing initial checkpoint")?;
    let final_checkpoint = checkpoints.last().ok_or("missing final checkpoint")?;
    if initial_checkpoint.owning_event.sequence != 1
        || terminal
            .is_some_and(|terminal| final_checkpoint.owning_event.sequence + 1 != terminal.sequence)
        || terminal.is_none()
            && final_checkpoint.owning_event.sequence
                != prefix_events.last().map_or(0, |event| event.sequence)
    {
        return Err("final checkpoint is not the exact captured boundary event".into());
    }

    let runtime_metadata = capture_runtime_metadata(
        owner,
        state_directory,
        metadata,
        ordinal,
        &event_by_sequence,
    )?;
    inventory.extend(runtime_metadata.inventory);
    payloads.extend(runtime_metadata.payloads);
    let captured_evidence =
        capture_evidence(owner, state_directory, metadata, ordinal, prefix_events)?;
    inventory.extend(captured_evidence.inventory);
    payloads.extend(captured_evidence.payloads);
    let source_links = capture_source_links(prefix_events)?;

    let last_event = terminal
        .or_else(|| prefix_events.last())
        .ok_or("captured event stream is empty")?;
    let segment = RprovSegment {
        ordinal,
        session_id: metadata.session_id.clone(),
        course_id: metadata.course_id.clone(),
        assignment_id: metadata.assignment_id.clone(),
        assignment_version: metadata.assignment_version.clone(),
        assignment_manifest_blake3: metadata.manifest_hash,
        original_starter_tree_hash,
        initial_tree_hash: metadata.starter_hash,
        producer: runtime_metadata.producer,
        time: RprovSegmentTime {
            started_at_utc: RprovKnown::Unknown,
            ended_at_utc: RprovKnown::Unknown,
            inter_attempt_time: RprovInterAttemptTime::Unknown,
        },
        parent: parent_link,
        events: RprovEventStreamRef {
            format_version: 1,
            entry: event_entry,
            byte_length: event_bytes.len() as u64,
            blake3: event_digest,
            completeness: if terminal.is_some() {
                RprovEventStreamCompleteness::Complete
            } else {
                RprovEventStreamCompleteness::PrefixOnly
            },
        },
        checkpoints: checkpoint_refs,
        metadata: runtime_metadata.refs,
        evidence: captured_evidence.refs,
        source_links,
        inclusive_event_count: last_event.sequence,
        last_event_hash: last_event.event_hash,
        terminal_event_hash: terminal.map_or(RprovKnown::Unknown, |terminal| RprovKnown::Known {
            value: terminal.event_hash,
        }),
        final_tree_hash: terminal.map_or(RprovKnown::Unknown, |_| RprovKnown::Known {
            value: final_checkpoint.snapshot.workspace_hash(),
        }),
    };

    let initial_workspace = if include_initial_workspace {
        let mut files = Vec::with_capacity(initial_checkpoint.snapshot.files().len());
        if initial_checkpoint.snapshot.files().len() > MAX_RPROV_INITIAL_FILES {
            return Err("initial workspace exceeds the package file limit".into());
        }
        let mut total = 0_u64;
        let mut published = BTreeSet::new();
        for file in initial_checkpoint.snapshot.files() {
            let length = file.contents.len() as u64;
            total = total
                .checked_add(length)
                .ok_or("initial workspace byte count overflow")?;
            if length > MAX_RPROV_INITIAL_FILE_BYTES || total > MAX_RPROV_INITIAL_WORKSPACE_BYTES {
                return Err("initial workspace exceeds package byte limits".into());
            }
            let digest = rprov_raw_blake3(&file.contents);
            let entry = format!("initial-workspace/blobs/{digest}");
            files.push(RprovInitialWorkspaceFile {
                path: file.path.clone(),
                entry: entry.clone(),
            });
            if published.insert(entry.clone()) {
                let local_name = format!("{artifact_tag}-starter-{digest}.bin");
                inventory.push(RprovInventoryEntry {
                    path: entry.clone(),
                    byte_length: length,
                    blake3: digest,
                    kind: RprovEntryKind::InitialWorkspaceBlob,
                });
                payloads.push(FinalizationPayload {
                    entry,
                    path: state_directory.join(&local_name),
                    byte_length: length,
                    blake3: digest,
                });
                publications.push((local_name, file.contents.clone()));
            }
        }
        Some(RprovInitialWorkspace { files })
    } else {
        None
    };

    inventory.sort_by(|left, right| left.path.cmp(&right.path));
    payloads.sort_by(|left, right| left.entry.cmp(&right.entry));
    Ok(CapturedCurrent {
        segment,
        inventory,
        payloads,
        publications,
        prefix_bytes,
        initial_workspace,
        final_workspace: checkpoint_files(&final_checkpoint.snapshot),
        gaps: captured_evidence.gaps,
    })
}

fn publish_current_capture(
    owner: &PinnedJournalFile,
    authority: &super::Authority,
    current: &CapturedCurrent,
    persisted: &PersistedCurrentCapture,
) -> Result<()> {
    let persisted_bytes = serde_json::to_vec(persisted)?;
    if persisted_bytes.len() > FINALIZATION_METADATA_LIMIT {
        return Err("current recovery capture exceeds its bounded metadata limit".into());
    }
    let publication_bytes = current.publications.iter().try_fold(
        persisted_bytes.len() as u64,
        |total, (_, bytes)| {
            total
                .checked_add(bytes.len() as u64)
                .ok_or("recovery capture byte count overflow")
        },
    )?;
    authority.headroom(publication_bytes)?;
    let replacing_partial = artifact_exists(owner, RECOVERY_EVENTS)?;
    for (name, bytes) in &current.publications {
        // RECOVERY_CAPTURE below is the commit point. Components left before
        // that marker by an interruption are incomplete and may be replaced by
        // the next owned, fully validated boundary capture.
        owner.publish_artifact(name, bytes, replacing_partial)?;
    }
    owner.publish_artifact(RECOVERY_CAPTURE, &persisted_bytes, false)?;
    Ok(())
}

fn publish_enriched_current_capture(
    owner: &PinnedJournalFile,
    authority: &super::Authority,
    persisted: &PersistedCurrentCapture,
) -> Result<()> {
    let bytes = serde_json::to_vec(persisted)?;
    if bytes.len() > FINALIZATION_METADATA_LIMIT {
        return Err("enriched recovery capture exceeds its bounded metadata limit".into());
    }
    authority.headroom(bytes.len() as u64)?;
    owner.publish_artifact(RECOVERY_CAPTURE, &bytes, true)?;
    Ok(())
}

fn complete_current_capture(
    current: &CapturedCurrent,
    state_directory: &Path,
    terminal: &EventEnvelope,
    ordinal: u32,
    parent: Option<RprovParentLink>,
    original_starter_tree_hash: Hash,
) -> Result<CapturedCurrent> {
    if terminal.sequence
        != current
            .segment
            .inclusive_event_count
            .checked_add(1)
            .unwrap_or(0)
        || terminal.previous_event_hash != current.segment.last_event_hash
    {
        return Err("terminal does not immediately extend the immutable current prefix".into());
    }
    let mut result = current.clone();
    result.publications.clear();
    let mut complete_events = current.prefix_bytes.clone();
    complete_events.extend_from_slice(&encode_envelope(terminal)?);
    complete_events.push(b'\n');
    if complete_events.len() as u64 > MAX_RPROV_SEGMENT_EVENTS_BYTES {
        return Err("captured event stream exceeds the segment byte limit".into());
    }

    result.segment.ordinal = ordinal;
    result.segment.parent = parent;
    result.segment.original_starter_tree_hash = original_starter_tree_hash;
    result.segment.events.entry = rebase_segment_entry(&result.segment.events.entry, ordinal)?;
    for checkpoint in &mut result.segment.checkpoints {
        checkpoint.entry = rebase_segment_entry(&checkpoint.entry, ordinal)?;
    }
    for metadata in &mut result.segment.metadata {
        metadata.entry = rebase_segment_entry(&metadata.entry, ordinal)?;
    }
    for evidence in &mut result.segment.evidence {
        evidence.entry = rebase_segment_entry(&evidence.entry, ordinal)?;
    }
    for entry in &mut result.inventory {
        entry.path = rebase_segment_entry(&entry.path, ordinal)?;
    }
    for payload in &mut result.payloads {
        payload.entry = rebase_segment_entry(&payload.entry, ordinal)?;
    }

    let event_digest = rprov_raw_blake3(&complete_events);
    result.segment.events.byte_length = complete_events.len() as u64;
    result.segment.events.blake3 = event_digest;
    result.segment.events.completeness = RprovEventStreamCompleteness::Complete;
    result.segment.inclusive_event_count = terminal.sequence;
    result.segment.last_event_hash = terminal.event_hash;
    result.segment.terminal_event_hash = RprovKnown::Known {
        value: terminal.event_hash,
    };
    let Event::SubmissionFinalized(finalized) = &terminal.event else {
        return Err("candidate terminal is not SubmissionFinalized".into());
    };
    result.segment.final_tree_hash = RprovKnown::Known {
        value: finalized.final_workspace_hash,
    };
    result
        .segment
        .checkpoints
        .last_mut()
        .ok_or("current capture has no boundary checkpoint")?
        .role = RprovCheckpointRole::Final;
    let event_entry = result.segment.events.entry.clone();
    let inventory = result
        .inventory
        .iter_mut()
        .find(|entry| entry.path == event_entry)
        .ok_or("current event inventory entry is missing")?;
    inventory.byte_length = complete_events.len() as u64;
    inventory.blake3 = event_digest;
    let payload = result
        .payloads
        .iter_mut()
        .find(|payload| payload.entry == event_entry)
        .ok_or("current event payload source is missing")?;
    payload.path = state_directory.join(COMPLETE_EVENTS);
    payload.byte_length = complete_events.len() as u64;
    payload.blake3 = event_digest;
    result
        .publications
        .push((COMPLETE_EVENTS.to_owned(), complete_events));
    result
        .inventory
        .sort_by(|left, right| left.path.cmp(&right.path));
    result
        .payloads
        .sort_by(|left, right| left.entry.cmp(&right.entry));
    Ok(result)
}

fn rebase_segment_entry(entry: &str, ordinal: u32) -> Result<String> {
    let Some(suffix) = entry.strip_prefix("segments/0001/") else {
        if entry.starts_with("segments/") {
            return Err("current capture entry has an unexpected ordinal".into());
        }
        return Ok(entry.to_owned());
    };
    Ok(format!("segments/{ordinal:04}/{suffix}"))
}

fn origin_artifact_name(hash: Hash) -> String {
    format!("finalization-origin-{hash}.bin")
}

fn classify_parent_evidence(
    metadata: &SessionMetadata,
    events: &[EventEnvelope],
) -> Result<ParentEvidenceKind> {
    let Some(expected) = metadata.parent_evidence else {
        return Ok(ParentEvidenceKind::None);
    };
    let abandonment = events.iter().find_map(|envelope| {
        let Event::RecoveryRecorded(recorded) = &envelope.event else {
            return None;
        };
        (recorded.decision == rustrace_model::RecoveryDecision::AbandonPreserved)
            .then_some(recorded.evidence_hash)
    });
    match abandonment {
        Some(actual) if actual == expected => Ok(ParentEvidenceKind::Abandonment),
        Some(_) => Err("abandonment event differs from the linked recovery evidence".into()),
        None => Ok(ParentEvidenceKind::FinalizedRevision),
    }
}

fn load_revision_seed(
    owner: &PinnedJournalFile,
    metadata: &SessionMetadata,
) -> Result<Option<RevisionSeed>> {
    let Some(expected_link_hash) = metadata.parent_evidence else {
        return Ok(None);
    };
    let bytes = owner.read_artifact("parent.json", METADATA_LIMIT)?;
    if digest(&bytes) != expected_link_hash {
        return Err("durable revision link digest mismatch".into());
    }
    let link: RevisionLink = serde_json::from_slice(&bytes)
        .map_err(|_| "parent evidence is not a complete finalized-revision link")?;
    if link.version != 1 || link.kind != "finalized_revision" {
        return Err("parent evidence does not identify finalized revision ancestry".into());
    }
    let (Some(original_starter_tree_hash), Some(initial_workspace)) =
        (link.original_starter_tree_hash, link.initial_workspace)
    else {
        return Ok(None);
    };
    let state_directory = owner
        .display_path()
        .parent()
        .ok_or("session state directory is missing")?;
    let mut inventory = link.initial_inventory;
    inventory.sort_by(|left, right| left.path.cmp(&right.path));
    if inventory
        .iter()
        .any(|entry| entry.kind != RprovEntryKind::InitialWorkspaceBlob)
    {
        return Err("revision recovery seed contains a non-starter payload".into());
    }
    let payloads = inventory
        .iter()
        .map(|entry| FinalizationPayload {
            entry: entry.path.clone(),
            path: state_directory.join(origin_artifact_name(entry.blake3)),
            byte_length: entry.byte_length,
            blake3: entry.blake3,
        })
        .collect();
    Ok(Some(RevisionSeed {
        original_starter_tree_hash,
        initial_workspace,
        inventory,
        payloads,
    }))
}

fn capture_runtime_metadata(
    owner: &PinnedJournalFile,
    state_directory: &Path,
    metadata: &SessionMetadata,
    ordinal: u32,
    events: &BTreeMap<u64, &EventEnvelope>,
) -> Result<CapturedRuntimeMetadata> {
    owner.verify()?;
    let mut observations = Vec::new();
    for entry in fs::read_dir(state_directory)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "non-UTF-8 state artifact name")?;
        if !name.starts_with("toolchain-") || !name.ends_with(".json") {
            continue;
        }
        let bytes = owner.read_artifact(&name, MAX_RPROV_METADATA_ENTRY_BYTES as usize)?;
        let observation: RuntimeToolchainMetadata = serde_json::from_slice(&bytes)?;
        if observation.version != 1
            || observation.session_id != metadata.session_id
            || observation.manifest_hash != metadata.manifest_hash
            || events
                .get(&observation.sequence)
                .is_none_or(|event| event.event_hash != observation.event_hash)
        {
            return Err("runtime metadata owner/assignment identity mismatch".into());
        }
        if observations.len() == MAX_RPROV_METADATA_PER_SEGMENT {
            return Err("runtime metadata exceeds the segment item limit".into());
        }
        observations.push((observation, name, bytes));
    }
    owner.verify()?;
    observations.sort_by_key(|(observation, _, _)| observation.sequence);
    let selected_report = observations
        .last()
        .map(|(observation, _, _)| &observation.report);
    let producer = producer(metadata, selected_report);
    let mut by_entry = BTreeMap::new();
    for (observation, name, bytes) in observations {
        let digest = rprov_raw_blake3(&bytes);
        let archive_entry = format!("segments/{ordinal:04}/metadata/{digest}.json");
        by_entry
            .entry(archive_entry)
            .or_insert((observation, name, bytes.len() as u64, digest));
    }
    let mut refs = Vec::with_capacity(by_entry.len());
    let mut inventory = Vec::with_capacity(by_entry.len());
    let mut payloads = Vec::with_capacity(by_entry.len());
    for (entry, (observation, name, byte_length, digest)) in by_entry {
        refs.push(RprovMetadataRef {
            format_version: 1,
            entry: entry.clone(),
            byte_length,
            blake3: digest,
            owner: RecordedEventRef {
                session_id: metadata.session_id.clone(),
                sequence: observation.sequence,
                event_hash: observation.event_hash,
            },
        });
        inventory.push(RprovInventoryEntry {
            path: entry.clone(),
            byte_length,
            blake3: digest,
            kind: RprovEntryKind::RuntimeMetadata,
        });
        payloads.push(FinalizationPayload {
            entry,
            path: state_directory.join(name),
            byte_length,
            blake3: digest,
        });
    }
    Ok(CapturedRuntimeMetadata {
        refs,
        inventory,
        payloads,
        producer,
    })
}

fn capture_evidence(
    owner: &PinnedJournalFile,
    state_directory: &Path,
    metadata: &SessionMetadata,
    ordinal: u32,
    events: &[EventEnvelope],
) -> Result<CapturedEvidence> {
    let mut grouped: BTreeMap<Hash, (String, Vec<RecordedEventRef>)> = BTreeMap::new();
    let mut usage_count = 0_usize;
    for envelope in events {
        let evidence = match &envelope.event {
            Event::ExternalObservation(event) => Some((
                event.evidence_hash,
                format!("evidence-{}.bin", event.evidence_hash),
            )),
            Event::RecoveryRecorded(event) => Some((
                event.evidence_hash,
                if event.decision == rustrace_model::RecoveryDecision::AbandonPreserved {
                    "parent.json".to_owned()
                } else {
                    format!("evidence-{}.bin", event.evidence_hash)
                },
            )),
            _ => None,
        };
        if let Some((hash, name)) = evidence {
            usage_count = usage_count
                .checked_add(1)
                .ok_or("evidence usage count overflow")?;
            if usage_count > MAX_RPROV_EVIDENCE_USAGES {
                return Err("evidence usages exceed the package limit".into());
            }
            if !grouped.contains_key(&hash) && grouped.len() == MAX_RPROV_EVIDENCE_PER_SEGMENT {
                return Err("evidence artifacts exceed the segment item limit".into());
            }
            let group = grouped
                .entry(hash)
                .or_insert_with(|| (name.clone(), Vec::new()));
            if group.0 != name {
                return Err("one evidence digest resolves to conflicting durable artifacts".into());
            }
            if group.1.len() == MAX_RPROV_EVIDENCE_USAGES_PER_ARTIFACT {
                return Err("one evidence artifact exceeds its usage limit".into());
            }
            group.1.push(event_reference(envelope));
        }
    }
    let mut refs = Vec::with_capacity(grouped.len());
    let mut inventory = Vec::with_capacity(grouped.len());
    let mut payloads = Vec::with_capacity(grouped.len());
    let mut missing = Vec::new();
    for (expected_digest, (name, usages)) in grouped {
        let bytes = match owner.read_artifact(&name, ARTIFACT_LIMIT) {
            Ok(bytes) => bytes,
            Err(_) if !artifact_exists(owner, &name)? => {
                missing.extend(usages.into_iter().map(|usage| (usage, expected_digest)));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let actual = rprov_raw_blake3(&bytes);
        if actual != expected_digest {
            return Err("referenced evidence digest does not match its durable bytes".into());
        }
        let entry = format!("segments/{ordinal:04}/evidence/{actual}.bin");
        refs.push(RprovEvidenceRef {
            kind: RprovEvidenceKind::ExternalRecovery,
            entry: entry.clone(),
            byte_length: bytes.len() as u64,
            blake3: actual,
            usages,
        });
        inventory.push(RprovInventoryEntry {
            path: entry.clone(),
            byte_length: bytes.len() as u64,
            blake3: actual,
            kind: RprovEntryKind::ExternalRecoveryEvidence,
        });
        payloads.push(FinalizationPayload {
            entry,
            path: state_directory.join(name),
            byte_length: bytes.len() as u64,
            blake3: actual,
        });
    }
    if refs.iter().any(|item| {
        item.usages
            .iter()
            .any(|usage| usage.session_id != metadata.session_id)
    }) {
        return Err("evidence usage crosses session identity".into());
    }
    missing.sort_by(|(left_event, left_hash), (right_event, right_hash)| {
        left_event
            .session_id
            .cmp(&right_event.session_id)
            .then_with(|| left_event.sequence.cmp(&right_event.sequence))
            .then_with(|| left_event.event_hash.cmp(&right_event.event_hash))
            .then_with(|| left_hash.cmp(right_hash))
    });
    let gaps = missing
        .into_iter()
        .map(|(event, blake3)| RprovRecoveryGap::MissingEvidence { event, blake3 })
        .collect();
    Ok(CapturedEvidence {
        refs,
        inventory,
        payloads,
        gaps,
    })
}

fn capture_source_links(events: &[EventEnvelope]) -> Result<Vec<RprovSourceLink>> {
    let by_sequence = events
        .iter()
        .map(|event| (event.sequence, event))
        .collect::<BTreeMap<_, _>>();
    let mut links = Vec::new();
    for envelope in events {
        match &envelope.event {
            Event::InternalPaste(paste) => {
                let copied = by_sequence
                    .get(&paste.source.sequence)
                    .copied()
                    .ok_or("internal paste source event is absent")?;
                if copied.event_hash != paste.source.event_hash
                    || copied.session_id != paste.source.session_id
                {
                    return Err("internal paste source prefix identity mismatch".into());
                }
                let Event::ClipboardCopied(source) = &copied.event else {
                    return Err("internal paste source does not resolve to ClipboardCopied".into());
                };
                links.push(RprovSourceLink::InternalPaste {
                    paste_event: event_reference(envelope),
                    copied_event: event_reference(copied),
                    document_id: source.document_id.clone(),
                    path: source.path.clone(),
                    version: source.version,
                    content_hash: source.content_hash,
                    start_byte: source.start_byte,
                    end_byte: source.end_byte,
                });
            }
            Event::FileEdited(transaction) if transaction.origin == EditOrigin::Paste => {
                links.push(RprovSourceLink::LegacyPaste {
                    event: event_reference(envelope),
                    verification: RprovLegacyPasteVerification::OriginUnverified,
                });
            }
            _ => {}
        }
        if links.len() > MAX_RPROV_SOURCE_LINKS_PER_SEGMENT {
            return Err("paste source links exceed the segment item limit".into());
        }
    }
    Ok(links)
}

fn load_parent_ancestry(
    owner: &PinnedJournalFile,
    metadata: &SessionMetadata,
) -> Result<(Option<FinalizationReceipt>, Option<RprovParentLink>)> {
    let Some(expected_link_hash) = metadata.parent_evidence else {
        return Ok((None, None));
    };
    let bytes = owner.read_artifact("parent.json", METADATA_LIMIT)?;
    if digest(&bytes) != expected_link_hash {
        return Err("durable revision link digest mismatch".into());
    }
    let link: RevisionLink = serde_json::from_slice(&bytes)
        .map_err(|_| "parent evidence is not a complete finalized-revision link")?;
    if link.version != 1 || link.kind != "finalized_revision" {
        return Err("parent evidence does not identify finalized revision ancestry".into());
    }
    let receipt = load_finalized_root(&link.parent_root)
        .map_err(|error| format!("required ancestry is unavailable or invalid: {error}"))?;
    let tip = receipt
        .manifest
        .segments
        .last()
        .ok_or("required ancestry receipt has no tip")?;
    if link.parent_session_id != tip.session_id
        || tip.terminal_event_hash.known() != Some(&link.parent_terminal_event_hash)
        || tip.final_tree_hash.known() != Some(&link.parent_final_tree_hash)
        || link.parent_manifest_blake3 != receipt.binding.manifest_blake3
        || link.parent_sources_blake3 != receipt.binding.sources_blake3
        || metadata.starter_hash != link.parent_final_tree_hash
    {
        return Err("required ancestry link does not match the exact parent receipt".into());
    }
    let rprov_link = RprovParentLink {
        session_id: link.parent_session_id,
        terminal_event_hash: link.parent_terminal_event_hash,
        final_tree_hash: link.parent_final_tree_hash,
    };
    Ok((Some(receipt), Some(rprov_link)))
}

fn load_parent_ancestry_for_recovery(
    owner: &PinnedJournalFile,
    metadata: &SessionMetadata,
) -> Result<RecoveryAncestry> {
    let expected_link_hash = metadata
        .parent_evidence
        .ok_or("revision recovery has no parent-link identity")?;
    let bytes = owner.read_artifact("parent.json", METADATA_LIMIT)?;
    if digest(&bytes) != expected_link_hash {
        return Err("durable revision link digest mismatch".into());
    }
    let link: RevisionLink = serde_json::from_slice(&bytes)
        .map_err(|_| "parent evidence is not a complete finalized-revision link")?;
    if link.version != 1 || link.kind != "finalized_revision" {
        return Err("parent evidence does not identify finalized revision ancestry".into());
    }
    let (receipt, gaps) = load_finalized_root_for_recovery(&link.parent_root).map_err(|error| {
        format!("required recovery ancestry is unavailable or invalid: {error}")
    })?;
    let tip = receipt
        .manifest
        .segments
        .last()
        .ok_or("required recovery ancestry receipt has no tip")?;
    if link.parent_session_id != tip.session_id
        || tip.terminal_event_hash.known() != Some(&link.parent_terminal_event_hash)
        || tip.final_tree_hash.known() != Some(&link.parent_final_tree_hash)
        || link.parent_manifest_blake3 != receipt.binding.manifest_blake3
        || link.parent_sources_blake3 != receipt.binding.sources_blake3
        || metadata.starter_hash != link.parent_final_tree_hash
    {
        return Err(
            "required recovery ancestry link does not match the exact parent receipt".into(),
        );
    }
    require_assignment_match(&receipt.manifest, metadata, owner)?;
    Ok(RecoveryAncestry {
        receipt,
        link: RprovParentLink {
            session_id: link.parent_session_id,
            terminal_event_hash: link.parent_terminal_event_hash,
            final_tree_hash: link.parent_final_tree_hash,
        },
        gaps,
    })
}

fn require_assignment_match(
    parent: &RprovManifest,
    metadata: &SessionMetadata,
    owner: &PinnedJournalFile,
) -> Result<()> {
    let manifest_bytes = owner.read_artifact("manifest.toml", METADATA_LIMIT)?;
    if parent.course_id != metadata.course_id
        || parent.assignment_id != metadata.assignment_id
        || parent.assignment_version != metadata.assignment_version
        || parent.test_case_suite_hash != metadata.test_case_suite_hash
        || parent.assignment_manifest.blake3 != metadata.manifest_hash
        || parent.assignment_manifest.blake3 != rprov_raw_blake3(&manifest_bytes)
        || parent.assignment_manifest.byte_length != manifest_bytes.len() as u64
    {
        return Err("revision assignment identity differs from its complete ancestry".into());
    }
    Ok(())
}

fn finish_prepared(
    owner: &mut PinnedJournalFile,
    prepared: &PreparedMarker,
    boundary: FinishBoundary,
) -> Result<FinalizationReceipt> {
    #[cfg(not(test))]
    let _ = boundary;
    #[cfg(test)]
    if boundary == FinishBoundary::AfterCapture {
        return Err("intentional interruption after durable finalization capture".into());
    }
    validate_prepared_marker(prepared)?;
    let receipt = load_candidate(owner, &prepared.binding)?;
    let expected_terminal = receipt_terminal(&receipt)?;
    if expected_terminal.sequence != prepared.binding.terminal_event_count
        || expected_terminal.event_hash != prepared.binding.terminal_event_hash
        || expected_terminal.previous_event_hash != prepared.binding.prefix_event_hash
    {
        return Err("prepared terminal binding differs from candidate event bytes".into());
    }

    let mut journal = Journal::open_retained_no_follow(owner.display_path())?;
    owner.pin_live_sidecars()?;
    let state = journal.inspect_session(&prepared.binding.session_id)?;
    let chain = journal.verify_session_chain(&prepared.binding.session_id)?;
    if chain.event_count == prepared.binding.prefix_event_count {
        if state.ended || chain.final_hash != prepared.binding.prefix_event_hash {
            return Err("journal prefix differs from the prepared immutable capture".into());
        }
        journal.append_event(&prepared.binding.session_id, &expected_terminal)?;
    } else if chain.event_count == prepared.binding.terminal_event_count {
        if chain.final_hash != prepared.binding.terminal_event_hash {
            return Err("journal terminal hash differs from the prepared terminal".into());
        }
        let actual = journal
            .read_events(
                &prepared.binding.session_id,
                prepared.binding.terminal_event_count,
                1,
            )?
            .into_iter()
            .next()
            .ok_or("prepared terminal row is missing")?;
        if actual != expected_terminal {
            return Err("journal terminal bytes differ from the prepared terminal".into());
        }
    } else {
        return Err(
            "journal count is neither the captured prefix nor its one terminal successor".into(),
        );
    }
    process_probe("finalization-terminal");
    #[cfg(test)]
    if boundary == FinishBoundary::AfterTerminal {
        return Err("intentional interruption after terminal append".into());
    }
    let final_chain = journal.verify_session_chain(&prepared.binding.session_id)?;
    if final_chain.event_count != prepared.binding.terminal_event_count
        || final_chain.final_hash != prepared.binding.terminal_event_hash
    {
        return Err("durable terminal chain does not equal the prepared candidate".into());
    }
    journal.verify_session_checkpoints(&prepared.binding.session_id)?;
    let ended = journal.end_session(&prepared.binding.session_id)?;
    if !ended.ended {
        return Err("session end marker was not durable".into());
    }
    drop(journal);
    process_probe("finalization-before-receipt");
    #[cfg(test)]
    if boundary == FinishBoundary::BeforeReceipt {
        return Err("intentional interruption before receipt publication".into());
    }
    let marker = FinalReceiptMarker {
        version: 1,
        label: "FINALIZATION RECEIPT".to_owned(),
        binding: prepared.binding.clone(),
    };
    let bytes = serde_json::to_vec(&marker)?;
    match owner.publish_artifact(RECEIPT_MARKER, &bytes, false) {
        Ok(()) => {}
        Err(_) => {
            let existing = owner.read_artifact(RECEIPT_MARKER, METADATA_LIMIT)?;
            if existing != bytes {
                return Err("a conflicting finalization receipt already exists".into());
            }
        }
    }
    owner.verify()?;
    Ok(receipt)
}

fn load_published_receipt(owner: &PinnedJournalFile) -> Result<FinalizationReceipt> {
    let marker: FinalReceiptMarker =
        serde_json::from_slice(&owner.read_artifact(RECEIPT_MARKER, METADATA_LIMIT)?)?;
    if marker.version != 1 || marker.label != "FINALIZATION RECEIPT" {
        return Err("unsupported or invalid finalization receipt marker".into());
    }
    let prepared = read_prepared(owner)?;
    if marker.binding != prepared.binding {
        return Err("published receipt binding differs from the prepared capture".into());
    }
    load_candidate(owner, &marker.binding)
}

fn load_published_receipt_for_recovery(
    owner: &PinnedJournalFile,
) -> Result<(FinalizationReceipt, Vec<RprovRecoveryGap>)> {
    let marker: FinalReceiptMarker =
        serde_json::from_slice(&owner.read_artifact(RECEIPT_MARKER, METADATA_LIMIT)?)?;
    if marker.version != 1 || marker.label != "FINALIZATION RECEIPT" {
        return Err("unsupported or invalid finalization receipt marker".into());
    }
    let prepared = read_prepared(owner)?;
    if marker.binding != prepared.binding {
        return Err("published receipt binding differs from the prepared capture".into());
    }
    load_candidate_for_recovery(owner, &marker.binding)
}

fn load_candidate(
    owner: &PinnedJournalFile,
    binding: &ReceiptBinding,
) -> Result<FinalizationReceipt> {
    validate_binding(binding)?;
    let manifest_bytes = owner.read_artifact(PREPARED_MANIFEST, MAX_RPROV_MANIFEST_BYTES)?;
    let sources_bytes = owner.read_artifact(PREPARED_SOURCES, FINALIZATION_METADATA_LIMIT)?;
    if rprov_raw_blake3(&manifest_bytes) != binding.manifest_blake3
        || rprov_raw_blake3(&sources_bytes) != binding.sources_blake3
    {
        return Err("candidate manifest/source binding digest mismatch".into());
    }
    let manifest = decode_rprov_manifest(&manifest_bytes)?;
    if encode_rprov_manifest(&manifest)? != manifest_bytes {
        return Err("candidate manifest is not its exact canonical encoding".into());
    }
    let sources: PersistedSources = serde_json::from_slice(&sources_bytes)?;
    if sources.version != 1 {
        return Err("unsupported finalization source receipt version".into());
    }
    let receipt = validate_receipt_parts(owner, manifest, sources.payloads, binding.clone())?;
    Ok(receipt)
}

fn load_candidate_for_recovery(
    owner: &PinnedJournalFile,
    binding: &ReceiptBinding,
) -> Result<(FinalizationReceipt, Vec<RprovRecoveryGap>)> {
    validate_binding(binding)?;
    let manifest_bytes = owner.read_artifact(PREPARED_MANIFEST, MAX_RPROV_MANIFEST_BYTES)?;
    let sources_bytes = owner.read_artifact(PREPARED_SOURCES, FINALIZATION_METADATA_LIMIT)?;
    if rprov_raw_blake3(&manifest_bytes) != binding.manifest_blake3
        || rprov_raw_blake3(&sources_bytes) != binding.sources_blake3
    {
        return Err("candidate manifest/source binding digest mismatch".into());
    }
    let manifest = decode_rprov_manifest(&manifest_bytes)?;
    if encode_rprov_manifest(&manifest)? != manifest_bytes {
        return Err("candidate manifest is not its exact canonical encoding".into());
    }
    let sources: PersistedSources = serde_json::from_slice(&sources_bytes)?;
    if sources.version != 1 {
        return Err("unsupported finalization source receipt version".into());
    }
    let result =
        validate_receipt_parts_for_recovery(owner, manifest, sources.payloads, binding.clone())?;
    Ok(result)
}

fn validate_receipt_parts(
    owner: &PinnedJournalFile,
    manifest: RprovManifest,
    payloads: Vec<FinalizationPayload>,
    binding: ReceiptBinding,
) -> Result<FinalizationReceipt> {
    let (receipt, missing) =
        validate_receipt_parts_inner(owner, manifest, payloads, binding, false)?;
    if !missing.is_empty() {
        return Err("clean receipt unexpectedly omitted evidence".into());
    }
    Ok(receipt)
}

fn validate_receipt_parts_for_recovery(
    owner: &PinnedJournalFile,
    manifest: RprovManifest,
    payloads: Vec<FinalizationPayload>,
    binding: ReceiptBinding,
) -> Result<(FinalizationReceipt, Vec<RprovRecoveryGap>)> {
    validate_receipt_parts_inner(owner, manifest, payloads, binding, true)
}

fn validate_receipt_parts_inner(
    owner: &PinnedJournalFile,
    mut manifest: RprovManifest,
    mut payloads: Vec<FinalizationPayload>,
    binding: ReceiptBinding,
    allow_missing_evidence: bool,
) -> Result<(FinalizationReceipt, Vec<RprovRecoveryGap>)> {
    manifest.validate()?;
    validate_binding(&binding)?;
    if manifest.latest_session_id != binding.session_id
        || manifest.aggregate_event_count < binding.terminal_event_count
        || manifest.final_tree_hash.known() != Some(&binding.final_tree_hash)
    {
        return Err("receipt binding disagrees with the candidate manifest".into());
    }
    payloads.sort_by(|left, right| left.entry.cmp(&right.entry));
    if payloads.len() != manifest.inventory.len() {
        return Err("payload-source receipt does not cover the complete inventory".into());
    }
    let mut missing_evidence = BTreeSet::new();
    for (declaration, source) in manifest.inventory.iter().zip(&payloads) {
        if declaration.path != source.entry
            || declaration.byte_length != source.byte_length
            || declaration.blake3 != source.blake3
        {
            return Err("payload-source receipt disagrees with manifest inventory".into());
        }
        match read_payload_source(source) {
            Ok(bytes) => validate_rprov_payload(declaration, &bytes)?,
            Err(_)
                if allow_missing_evidence
                    && declaration.kind == RprovEntryKind::ExternalRecoveryEvidence
                    && payload_source_is_absent(source)? =>
            {
                missing_evidence.insert(declaration.path.clone());
            }
            Err(error) => return Err(error),
        }
    }
    for segment in &manifest.segments {
        let events = read_declared_payload(&payloads, &segment.events.entry)?;
        validate_rprov_event_stream(&manifest, segment, &events)?;
    }
    let tip = manifest
        .segments
        .last()
        .ok_or("receipt has no terminal segment")?;
    let complete_bytes = read_declared_payload(&payloads, &tip.events.entry)?;
    let complete_events = decode_event_stream(&complete_bytes)?;
    let (terminal, prefix_events) = complete_events
        .split_last()
        .ok_or("receipt event stream is empty")?;
    let prefix_bytes = encode_event_stream(prefix_events)?;
    let Event::SubmissionFinalized(finalized) = &terminal.event else {
        return Err("receipt event stream has no SubmissionFinalized terminal".into());
    };
    if prefix_events.len() as u64 != binding.prefix_event_count
        || prefix_events.last().map(|event| event.event_hash) != Some(binding.prefix_event_hash)
        || prefix_bytes.len() as u64 != binding.prefix_byte_length
        || rprov_raw_blake3(&prefix_bytes) != binding.prefix_blake3
        || terminal.sequence != binding.terminal_event_count
        || terminal.event_hash != binding.terminal_event_hash
        || terminal.previous_event_hash != binding.prefix_event_hash
        || finalized.event_count != binding.terminal_event_count
        || finalized.final_workspace_hash != binding.final_tree_hash
        || tip.inclusive_event_count != binding.terminal_event_count
        || tip.last_event_hash != binding.terminal_event_hash
        || tip.terminal_event_hash.known() != Some(&binding.terminal_event_hash)
        || tip.final_tree_hash.known() != Some(&binding.final_tree_hash)
    {
        return Err("receipt binding disagrees with the actual complete event stream".into());
    }
    let durable_prefix = owner.read_artifact(
        PREFIX_EVENTS,
        usize::try_from(binding.prefix_byte_length)
            .unwrap_or(usize::MAX)
            .min(MAX_RPROV_SEGMENT_EVENTS_BYTES as usize),
    )?;
    if durable_prefix != prefix_bytes {
        return Err("durable prefix bytes differ from the complete event stream".into());
    }
    let initial_files = read_initial_workspace(&manifest, &payloads)?;
    let mut latest_final = None;
    for (index, segment) in manifest.segments.iter().enumerate() {
        let initial = checkpoint_files(&segment_initial_checkpoint(segment, &payloads)?);
        let final_snapshot = replay_segment(segment, &payloads)?;
        if index == 0 && initial != initial_files {
            return Err("original initial-workspace blobs differ from root genesis".into());
        }
        if let Some(previous) = &latest_final
            && initial != *previous
        {
            return Err(
                "child genesis bytes differ from its immediate parent final checkpoint".into(),
            );
        }
        latest_final = Some(checkpoint_files(&final_snapshot));
    }
    let final_workspace = latest_final.ok_or("receipt has no replayed final workspace")?;
    let mut gaps = Vec::new();
    if !missing_evidence.is_empty() {
        let mut matched = BTreeSet::new();
        for segment in &mut manifest.segments {
            segment.evidence.retain(|evidence| {
                if missing_evidence.contains(&evidence.entry) {
                    matched.insert(evidence.entry.clone());
                    gaps.extend(evidence.usages.iter().cloned().map(|event| {
                        RprovRecoveryGap::MissingEvidence {
                            event,
                            blake3: evidence.blake3,
                        }
                    }));
                    false
                } else {
                    true
                }
            });
        }
        if matched != missing_evidence {
            return Err("missing evidence payload has no exact segment reference".into());
        }
        manifest
            .inventory
            .retain(|entry| !missing_evidence.contains(&entry.path));
        payloads.retain(|payload| !missing_evidence.contains(&payload.entry));
        sort_recovery_gaps(&mut gaps);
    }
    Ok((
        FinalizationReceipt {
            manifest,
            payloads,
            final_workspace,
            binding,
            workspace_root: owner
                .display_path()
                .parent()
                .and_then(Path::parent)
                .ok_or("finalization receipt has no workspace root")?
                .to_path_buf(),
        },
        gaps,
    ))
}

fn payload_source_is_absent(source: &FinalizationPayload) -> Result<bool> {
    match fs::symlink_metadata(&source.path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Ok(_) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn replay_segment(
    segment: &RprovSegment,
    payloads: &[FinalizationPayload],
) -> Result<CheckpointSnapshot> {
    let event_bytes = read_declared_payload(payloads, &segment.events.entry)?;
    let events = decode_event_stream(&event_bytes)?;
    let initial_ref = segment
        .checkpoints
        .first()
        .ok_or("segment has no initial checkpoint")?;
    let initial = decode_checkpoint(&read_declared_payload(payloads, &initial_ref.entry)?)?;
    let first = events.first().ok_or("segment event stream is empty")?;
    let mut replay = ReplayEngine::from_initial_checkpoint(StoredCheckpoint {
        owning_event: first.clone(),
        snapshot: initial,
    })?;
    let checkpoints = segment
        .checkpoints
        .iter()
        .map(|checkpoint| (checkpoint.owner.sequence, checkpoint))
        .collect::<BTreeMap<_, _>>();
    let mut final_snapshot = None;
    for envelope in events.iter().skip(1) {
        replay.apply(envelope)?;
        if matches!(envelope.event, Event::WorkspaceCheckpoint(_)) {
            let declaration = checkpoints
                .get(&envelope.sequence)
                .ok_or("replayed checkpoint has no declared payload")?;
            let snapshot =
                decode_checkpoint(&read_declared_payload(payloads, &declaration.entry)?)?;
            let stored = StoredCheckpoint {
                owning_event: envelope.clone(),
                snapshot: snapshot.clone(),
            };
            replay.validate_checkpoint(&stored)?;
            final_snapshot = Some(snapshot);
        }
    }
    if !replay.is_finalized()
        || replay.current_workspace_hash()
            != segment
                .final_tree_hash
                .known()
                .copied()
                .ok_or("segment final tree is unknown")?
    {
        return Err("semantic replay did not reach the declared finalized tree".into());
    }
    final_snapshot.ok_or_else(|| "segment has no replay-certified final checkpoint".into())
}

fn segment_initial_checkpoint(
    segment: &RprovSegment,
    payloads: &[FinalizationPayload],
) -> Result<CheckpointSnapshot> {
    let declaration = segment
        .checkpoints
        .first()
        .ok_or("segment has no initial checkpoint")?;
    Ok(decode_checkpoint(&read_declared_payload(
        payloads,
        &declaration.entry,
    )?)?)
}

fn read_initial_workspace(
    manifest: &RprovManifest,
    payloads: &[FinalizationPayload],
) -> Result<BTreeMap<WorkspacePath, Vec<u8>>> {
    manifest
        .initial_workspace
        .files
        .iter()
        .map(|file| {
            Ok((
                file.path.clone(),
                read_declared_payload(payloads, &file.entry)?,
            ))
        })
        .collect()
}

fn verify_local_terminal(owner: &PinnedJournalFile, receipt: &FinalizationReceipt) -> Result<()> {
    let mut journal = Journal::open_read_only_no_follow(owner.display_path())?;
    let tip = receipt
        .manifest
        .segments
        .last()
        .ok_or("receipt has no terminal segment")?;
    let chain = journal.verify_session_chain(&tip.session_id)?;
    let state = journal.inspect_session(&tip.session_id)?;
    if !state.ended
        || chain.event_count != tip.inclusive_event_count
        || chain.final_hash != tip.last_event_hash
    {
        return Err("local terminal journal differs from its published receipt".into());
    }
    journal.verify_session_checkpoints(&tip.session_id)?;
    Ok(())
}

fn load_finalized_root(root: &Path) -> Result<FinalizationReceipt> {
    let metadata: SessionMetadata = serde_json::from_slice(&super::read_initial_metadata(root)?)?;
    let pinned = PinnedWorkspaceRoot::open(root)?;
    let mut owner = pinned
        .open_state_directory()?
        .open_journal_file(&metadata.session_id)?;
    let result = (|| {
        let receipt = load_published_receipt(&owner)?;
        verify_local_terminal(&owner, &receipt)?;
        let manifest_bytes = owner.read_artifact("manifest.toml", METADATA_LIMIT)?;
        if receipt.manifest.assignment_manifest.byte_length != manifest_bytes.len() as u64
            || receipt.manifest.assignment_manifest.blake3 != rprov_raw_blake3(&manifest_bytes)
            || receipt.manifest.course_id != metadata.course_id
            || receipt.manifest.assignment_id != metadata.assignment_id
            || receipt.manifest.assignment_version != metadata.assignment_version
            || receipt.manifest.test_case_suite_hash != metadata.test_case_suite_hash
        {
            return Err("finalized receipt assignment identity differs from local session".into());
        }
        Ok(receipt)
    })();
    owner.release_ownership()?;
    result
}

fn load_finalized_root_for_recovery(
    root: &Path,
) -> Result<(FinalizationReceipt, Vec<RprovRecoveryGap>)> {
    let metadata: SessionMetadata = serde_json::from_slice(&super::read_initial_metadata(root)?)?;
    let pinned = PinnedWorkspaceRoot::open(root)?;
    let mut owner = pinned
        .open_state_directory()?
        .open_journal_file(&metadata.session_id)?;
    let result = (|| {
        let (receipt, gaps) = load_published_receipt_for_recovery(&owner)?;
        verify_local_terminal(&owner, &receipt)?;
        let manifest_bytes = owner.read_artifact("manifest.toml", METADATA_LIMIT)?;
        if receipt.manifest.assignment_manifest.byte_length != manifest_bytes.len() as u64
            || receipt.manifest.assignment_manifest.blake3 != rprov_raw_blake3(&manifest_bytes)
            || receipt.manifest.course_id != metadata.course_id
            || receipt.manifest.assignment_id != metadata.assignment_id
            || receipt.manifest.assignment_version != metadata.assignment_version
            || receipt.manifest.test_case_suite_hash != metadata.test_case_suite_hash
        {
            return Err(
                "finalized recovery ancestry assignment identity differs from local session".into(),
            );
        }
        Ok((receipt, gaps))
    })();
    owner.release_ownership()?;
    result
}

fn load_published_summary(
    state: &PinnedStateDirectory,
    metadata: &SessionMetadata,
) -> Result<ReadOnlyFinalizationReceipt> {
    let marker: FinalReceiptMarker =
        serde_json::from_slice(&state.read_artifact(RECEIPT_MARKER, METADATA_LIMIT)?)?;
    if marker.version != 1 || marker.label != "FINALIZATION RECEIPT" {
        return Err("unsupported or invalid finalization receipt marker".into());
    }
    let prepared = read_prepared_state(state)?;
    if marker.binding != prepared.binding {
        return Err("published receipt binding differs from the prepared capture".into());
    }
    let binding = marker.binding;
    validate_binding(&binding)?;
    let manifest_bytes = state.read_artifact(PREPARED_MANIFEST, MAX_RPROV_MANIFEST_BYTES)?;
    let sources_bytes = state.read_artifact(PREPARED_SOURCES, FINALIZATION_METADATA_LIMIT)?;
    if rprov_raw_blake3(&manifest_bytes) != binding.manifest_blake3
        || rprov_raw_blake3(&sources_bytes) != binding.sources_blake3
    {
        return Err("candidate manifest/source binding digest mismatch".into());
    }
    let manifest = decode_rprov_manifest(&manifest_bytes)?;
    if encode_rprov_manifest(&manifest)? != manifest_bytes {
        return Err("candidate manifest is not its exact canonical encoding".into());
    }
    manifest.validate()?;
    let mut sources: PersistedSources = serde_json::from_slice(&sources_bytes)?;
    if sources.version != 1 {
        return Err("unsupported finalization source receipt version".into());
    }
    sources
        .payloads
        .sort_by(|left, right| left.entry.cmp(&right.entry));
    if sources.payloads.len() != manifest.inventory.len()
        || manifest
            .inventory
            .iter()
            .zip(&sources.payloads)
            .any(|(declaration, source)| {
                declaration.path != source.entry
                    || declaration.byte_length != source.byte_length
                    || declaration.blake3 != source.blake3
            })
    {
        return Err("payload-source receipt disagrees with manifest inventory".into());
    }
    let tip = manifest
        .segments
        .last()
        .ok_or("receipt has no terminal segment")?;
    if manifest.latest_session_id != binding.session_id
        || metadata.session_id != binding.session_id
        || manifest.aggregate_event_count < binding.terminal_event_count
        || manifest.final_tree_hash.known() != Some(&binding.final_tree_hash)
        || tip.session_id != binding.session_id
        || tip.inclusive_event_count != binding.terminal_event_count
        || tip.last_event_hash != binding.terminal_event_hash
        || tip.terminal_event_hash.known() != Some(&binding.terminal_event_hash)
        || tip.final_tree_hash.known() != Some(&binding.final_tree_hash)
    {
        return Err("receipt binding disagrees with the candidate manifest".into());
    }
    let event_source = sources
        .payloads
        .iter()
        .find(|source| source.entry == tip.events.entry)
        .ok_or("receipt has no source for its terminal event stream")?;
    let event_maximum = usize::try_from(event_source.byte_length)
        .map_err(|_| "receipt event-stream length does not fit this platform")?;
    let complete_bytes = state.read_artifact(COMPLETE_EVENTS, event_maximum)?;
    if complete_bytes.len() as u64 != event_source.byte_length
        || rprov_raw_blake3(&complete_bytes) != event_source.blake3
    {
        return Err("terminal event-stream artifact differs from its source receipt".into());
    }
    validate_rprov_event_stream(&manifest, tip, &complete_bytes)?;
    let complete_events = decode_event_stream(&complete_bytes)?;
    let (terminal, prefix_events) = complete_events
        .split_last()
        .ok_or("receipt event stream is empty")?;
    let prefix_bytes = encode_event_stream(prefix_events)?;
    let Event::SubmissionFinalized(finalized) = &terminal.event else {
        return Err("receipt event stream has no SubmissionFinalized terminal".into());
    };
    if prefix_events.len() as u64 != binding.prefix_event_count
        || prefix_events.last().map(|event| event.event_hash) != Some(binding.prefix_event_hash)
        || prefix_bytes.len() as u64 != binding.prefix_byte_length
        || rprov_raw_blake3(&prefix_bytes) != binding.prefix_blake3
        || terminal.sequence != binding.terminal_event_count
        || terminal.event_hash != binding.terminal_event_hash
        || terminal.previous_event_hash != binding.prefix_event_hash
        || finalized.event_count != binding.terminal_event_count
        || finalized.final_workspace_hash != binding.final_tree_hash
    {
        return Err("receipt binding disagrees with the actual complete event stream".into());
    }
    let durable_prefix = state.read_artifact(
        PREFIX_EVENTS,
        usize::try_from(binding.prefix_byte_length)
            .unwrap_or(usize::MAX)
            .min(MAX_RPROV_SEGMENT_EVENTS_BYTES as usize),
    )?;
    if durable_prefix != prefix_bytes {
        return Err("durable prefix bytes differ from the complete event stream".into());
    }
    let assignment_bytes = state.read_artifact("manifest.toml", METADATA_LIMIT)?;
    if manifest.assignment_manifest.byte_length != assignment_bytes.len() as u64
        || manifest.assignment_manifest.blake3 != rprov_raw_blake3(&assignment_bytes)
        || manifest.course_id != metadata.course_id
        || manifest.assignment_id != metadata.assignment_id
        || manifest.assignment_version != metadata.assignment_version
        || manifest.test_case_suite_hash != metadata.test_case_suite_hash
    {
        return Err("finalized receipt assignment identity differs from local session".into());
    }
    let ancestry_session_ids = manifest
        .segments
        .iter()
        .map(|segment| segment.session_id.clone())
        .collect();
    Ok(ReadOnlyFinalizationReceipt {
        student_id: manifest.student_id,
        latest_session_id: manifest.latest_session_id,
        final_tree_hash: binding.final_tree_hash,
        terminal_chain_hash: binding.terminal_event_hash,
        aggregate_event_count: manifest.aggregate_event_count,
        ancestry_session_ids,
    })
}

fn read_prepared_state(state: &PinnedStateDirectory) -> Result<PreparedMarker> {
    let marker: PreparedMarker =
        serde_json::from_slice(&state.read_artifact(PREPARED_MARKER, METADATA_LIMIT)?)?;
    validate_prepared_marker(&marker)?;
    let maximum = usize::try_from(marker.binding.prefix_byte_length)
        .unwrap_or(usize::MAX)
        .min(MAX_RPROV_SEGMENT_EVENTS_BYTES as usize);
    let prefix = state.read_artifact(PREFIX_EVENTS, maximum)?;
    if prefix.len() as u64 != marker.binding.prefix_byte_length
        || rprov_raw_blake3(&prefix) != marker.binding.prefix_blake3
    {
        return Err("durable prefix bytes differ from the prepared binding".into());
    }
    let events = decode_event_stream(&prefix)?;
    if events.len() as u64 != marker.binding.prefix_event_count
        || events.last().map(|event| event.event_hash) != Some(marker.binding.prefix_event_hash)
    {
        return Err("durable prefix count/hash differs from the prepared binding".into());
    }
    Ok(marker)
}

fn read_incomplete_state(state: &PinnedStateDirectory) -> Result<PersistedIncompleteFinalization> {
    let value: PersistedIncompleteFinalization =
        serde_json::from_slice(&state.read_artifact(INCOMPLETE_MARKER, METADATA_LIMIT)?)?;
    if value.version != 1
        || value.label != "INCOMPLETE RECOVERY"
        || value.reason.len() > INCOMPLETE_REASON_BYTES
    {
        return Err("invalid incomplete-recovery marker".into());
    }
    Ok(value)
}

fn read_prepared(owner: &PinnedJournalFile) -> Result<PreparedMarker> {
    let marker: PreparedMarker =
        serde_json::from_slice(&owner.read_artifact(PREPARED_MARKER, METADATA_LIMIT)?)?;
    validate_prepared_marker(&marker)?;
    let maximum = usize::try_from(marker.binding.prefix_byte_length)
        .unwrap_or(usize::MAX)
        .min(MAX_RPROV_SEGMENT_EVENTS_BYTES as usize);
    let prefix = owner.read_artifact(PREFIX_EVENTS, maximum)?;
    if prefix.len() as u64 != marker.binding.prefix_byte_length
        || rprov_raw_blake3(&prefix) != marker.binding.prefix_blake3
    {
        return Err("durable prefix bytes differ from the prepared binding".into());
    }
    let events = decode_event_stream(&prefix)?;
    if events.len() as u64 != marker.binding.prefix_event_count
        || events.last().map(|event| event.event_hash) != Some(marker.binding.prefix_event_hash)
    {
        return Err("durable prefix count/hash differs from the prepared binding".into());
    }
    Ok(marker)
}

fn validate_prepared_marker(marker: &PreparedMarker) -> Result<()> {
    if marker.version != 1 || marker.label != "PREPARED CAPTURE - NOT FINALIZED" {
        return Err("unsupported or invalid prepared-finalization marker".into());
    }
    validate_binding(&marker.binding)
}

fn validate_binding(binding: &ReceiptBinding) -> Result<()> {
    if binding.version != 1
        || binding.prefix_event_count == 0
        || binding.terminal_event_count != binding.prefix_event_count.checked_add(1).unwrap_or(0)
        || binding.terminal_event_count > MAX_RPROV_EVENTS
        || binding.prefix_byte_length > MAX_RPROV_SEGMENT_EVENTS_BYTES
    {
        return Err("invalid finalization receipt binding".into());
    }
    Ok(())
}

fn receipt_terminal(receipt: &FinalizationReceipt) -> Result<EventEnvelope> {
    let tip = receipt
        .manifest
        .segments
        .last()
        .ok_or("receipt has no tip segment")?;
    decode_event_stream(&receipt.read_payload(&tip.events.entry)?)?
        .into_iter()
        .last()
        .ok_or_else(|| "receipt has no terminal envelope".into())
}

fn build_incomplete(
    owner: &PinnedJournalFile,
    metadata: &SessionMetadata,
    reason: &str,
    maximum_aggregate_event_count: u64,
) -> Result<IncompleteFinalization> {
    let (capture, reason) = if artifact_exists(owner, RECOVERY_CAPTURE)? {
        match load_recovery_capture(owner, metadata, maximum_aggregate_event_count) {
            Ok(capture) => (Some(capture), reason.to_owned()),
            Err(error) => (
                None,
                format!("{reason}; immutable recovery capture could not be certified: {error}"),
            ),
        }
    } else {
        (None, reason.to_owned())
    };
    let capture_available = capture.is_some();
    let value = publish_incomplete_marker(owner, &metadata.session_id, &reason, capture_available)?;
    Ok(IncompleteFinalization {
        version: value.version,
        label: value.label,
        reason: value.reason,
        session_id: value.session_id,
        capture_available,
        capture: capture.map(Box::new),
    })
}

fn publish_incomplete_marker(
    owner: &PinnedJournalFile,
    session_id: &SessionId,
    reason: &str,
    capture_available: bool,
) -> Result<PersistedIncompleteFinalization> {
    let mut end = reason.len().min(INCOMPLETE_REASON_BYTES);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let value = PersistedIncompleteFinalization {
        version: 1,
        label: "INCOMPLETE RECOVERY".to_owned(),
        reason: reason[..end].to_owned(),
        session_id: session_id.clone(),
        capture_available,
    };
    owner.publish_artifact(INCOMPLETE_MARKER, &serde_json::to_vec(&value)?, true)?;
    Ok(value)
}

fn read_incomplete(owner: &PinnedJournalFile) -> Result<PersistedIncompleteFinalization> {
    let value: PersistedIncompleteFinalization =
        serde_json::from_slice(&owner.read_artifact(INCOMPLETE_MARKER, METADATA_LIMIT)?)?;
    if value.version != 1 || value.label != "INCOMPLETE RECOVERY" {
        return Err("invalid incomplete-recovery marker".into());
    }
    Ok(value)
}

fn load_recovery_capture(
    owner: &PinnedJournalFile,
    metadata: &SessionMetadata,
    maximum_aggregate_event_count: u64,
) -> Result<RecoveryCapture> {
    let persisted: PersistedCurrentCapture = serde_json::from_slice(
        &owner.read_artifact(RECOVERY_CAPTURE, FINALIZATION_METADATA_LIMIT)?,
    )?;
    if persisted.version != 1
        || persisted.label != "IMMUTABLE CURRENT PREFIX"
        || persisted.segment.session_id != metadata.session_id
        || persisted.segment.course_id != metadata.course_id
        || persisted.segment.assignment_id != metadata.assignment_id
        || persisted.segment.assignment_version != metadata.assignment_version
        || persisted.segment.assignment_manifest_blake3 != metadata.manifest_hash
        || persisted.segment.initial_tree_hash != metadata.starter_hash
        || persisted.test_case_suite_hash != metadata.test_case_suite_hash
        || persisted.segment.ordinal != 1
        || persisted.segment.parent.is_some()
        || persisted.segment.events.completeness != RprovEventStreamCompleteness::PrefixOnly
        || persisted.segment.terminal_event_hash.known().is_some()
        || persisted.segment.final_tree_hash.known().is_some()
        || persisted.assignment_manifest.blake3 != metadata.manifest_hash
    {
        return Err("invalid immutable current-prefix capture".into());
    }
    let manifest_bytes = owner.read_artifact("manifest.toml", METADATA_LIMIT)?;
    if persisted.assignment_manifest.byte_length != manifest_bytes.len() as u64
        || persisted.assignment_manifest.blake3 != rprov_raw_blake3(&manifest_bytes)
    {
        return Err("immutable recovery capture differs from its assignment manifest".into());
    }
    let state_directory = owner
        .display_path()
        .parent()
        .ok_or("session state directory is missing")?;
    if persisted
        .payloads
        .iter()
        .any(|payload| payload.path.parent() != Some(state_directory))
    {
        return Err("immutable recovery capture has a non-local payload source".into());
    }
    if !persisted.had_parent && persisted.original_starter_tree_hash != Some(metadata.starter_hash)
    {
        return Err("root recovery capture differs from its starter identity".into());
    }
    match persisted.original_starter_tree_hash {
        Some(original) if persisted.segment.original_starter_tree_hash != original => {
            return Err("immutable recovery capture has conflicting starter identities".into());
        }
        None if !persisted.had_parent
            || persisted.segment.original_starter_tree_hash != Hash::zero() =>
        {
            return Err("immutable raw recovery capture has invalid seed state".into());
        }
        _ => {}
    }

    let final_workspace = validate_raw_current_capture(owner, &persisted)?;
    let raw_capture = || RecoveryCapture {
        manifest: None,
        payloads: persisted.payloads.clone(),
        final_workspace: final_workspace.clone(),
    };

    // The current capture is certified entirely from its copied local seed and
    // payloads. parent.json is consulted only below to add optional ancestry.
    let (candidate_missing_manifest, missing_payloads) =
        match build_recovery_manifest(&persisted, None, maximum_aggregate_event_count) {
            Ok(candidate) => candidate,
            Err(_) => return Ok(raw_capture()),
        };
    let (missing_manifest, final_workspace) =
        match validate_recovery_parts(&candidate_missing_manifest, missing_payloads) {
            Ok(workspace) => (Some(candidate_missing_manifest), workspace),
            Err(_)
                if persisted.had_parent
                    && persisted.original_starter_tree_hash
                        == Some(persisted.segment.initial_tree_hash) =>
            {
                // The accepted model rejects a missing-ancestry gap when the
                // retained child happens to equal the original starter. Use a
                // standalone shape only to validate the bytes, then expose the
                // capture without inventing a complete-ancestry claim.
                let mut standalone = persisted.clone();
                standalone.had_parent = false;
                let (validation_manifest, validation_payloads) =
                    build_recovery_manifest(&standalone, None, maximum_aggregate_event_count)?;
                (
                    None,
                    validate_recovery_parts(&validation_manifest, validation_payloads)?,
                )
            }
            Err(_) => return Ok(raw_capture()),
        };
    if !persisted.had_parent {
        return Ok(RecoveryCapture {
            manifest: missing_manifest,
            payloads: persisted.payloads,
            final_workspace,
        });
    }
    let parent = match load_parent_ancestry_for_recovery(owner, metadata) {
        Ok(parent) if recovery_ancestry_matches(&persisted, &parent) => parent,
        Ok(_) | Err(_) => {
            return Ok(RecoveryCapture {
                manifest: missing_manifest,
                payloads: persisted.payloads,
                final_workspace,
            });
        }
    };
    let (manifest, payloads) =
        match build_recovery_manifest(&persisted, Some(parent), maximum_aggregate_event_count) {
            Ok(candidate) => candidate,
            Err(_) => {
                return Ok(RecoveryCapture {
                    manifest: None,
                    payloads: persisted.payloads,
                    final_workspace,
                });
            }
        };
    let validated_workspace = match validate_recovery_parts(&manifest, payloads.clone()) {
        Ok(workspace) => workspace,
        Err(_) => {
            return Ok(RecoveryCapture {
                manifest: None,
                payloads: persisted.payloads,
                final_workspace,
            });
        }
    };
    if validated_workspace != final_workspace {
        return Err("ancestry recovery changed the immutable current boundary".into());
    }
    if manifest.aggregate_event_count > persisted.maximum_aggregate_event_count {
        return Ok(RecoveryCapture {
            manifest: None,
            payloads: persisted.payloads,
            final_workspace,
        });
    }
    Ok(RecoveryCapture {
        manifest: Some(manifest),
        payloads,
        final_workspace,
    })
}

fn recovery_ancestry_matches(
    persisted: &PersistedCurrentCapture,
    ancestry: &RecoveryAncestry,
) -> bool {
    let receipt = &ancestry.receipt;
    receipt.manifest.student_id == persisted.student_id
        && Some(receipt.manifest.original_starter_tree_hash) == persisted.original_starter_tree_hash
        && receipt.manifest.test_case_suite_hash == persisted.test_case_suite_hash
        && Some(&receipt.manifest.initial_workspace) == persisted.initial_workspace.as_ref()
        && receipt.manifest.assignment_manifest == persisted.assignment_manifest
}

fn build_recovery_manifest(
    persisted: &PersistedCurrentCapture,
    ancestry: Option<RecoveryAncestry>,
    maximum_aggregate_event_count: u64,
) -> Result<(RprovManifest, Vec<FinalizationPayload>)> {
    let missing_ancestry = ancestry.is_none() && persisted.had_parent;
    let original_starter_tree_hash = persisted
        .original_starter_tree_hash
        .ok_or("recovery capture lacks the original starter-tree identity")?;
    let initial_workspace = persisted
        .initial_workspace
        .clone()
        .ok_or("recovery capture lacks the original starter workspace")?;

    let (mut segments, mut inventory, mut payloads, parent_link, ordinal, mut gaps) =
        if let Some(ancestry) = ancestry {
            let receipt = ancestry.receipt;
            if receipt.manifest.student_id != persisted.student_id {
                return Err("revision student identifier differs from its parent receipt".into());
            }
            if receipt.manifest.assignment_manifest != persisted.assignment_manifest
                || receipt.manifest.original_starter_tree_hash != original_starter_tree_hash
                || receipt.manifest.test_case_suite_hash != persisted.test_case_suite_hash
                || receipt.manifest.initial_workspace != initial_workspace
            {
                return Err("recovery ancestry differs from the retained assignment seed".into());
            }
            let ordinal = receipt
                .manifest
                .segments
                .len()
                .checked_add(1)
                .ok_or("recovery segment ordinal overflow")?;
            (
                receipt.manifest.segments,
                receipt.manifest.inventory,
                receipt.payloads,
                Some(ancestry.link),
                u32::try_from(ordinal).map_err(|_| "recovery segment ordinal overflow")?,
                ancestry.gaps,
            )
        } else {
            (Vec::new(), Vec::new(), Vec::new(), None, 1, Vec::new())
        };
    let current =
        rebase_prefix_capture(persisted, ordinal, parent_link, original_starter_tree_hash)?;
    let has_parent = !segments.is_empty();
    inventory.extend(
        current
            .inventory
            .iter()
            .filter(|entry| !has_parent || entry.kind != RprovEntryKind::InitialWorkspaceBlob)
            .cloned(),
    );
    payloads.extend(
        current
            .payloads
            .iter()
            .filter(|payload| !has_parent || !payload.entry.starts_with("initial-workspace/blobs/"))
            .cloned(),
    );
    segments.push(current.segment);
    inventory.sort_by(|left, right| left.path.cmp(&right.path));
    payloads.sort_by(|left, right| left.entry.cmp(&right.entry));
    let aggregate_event_count = checked_aggregate_with_limit(
        segments.iter().map(|segment| segment.inclusive_event_count),
        maximum_aggregate_event_count,
    )?;
    let mut unavailable_assurances = Vec::new();
    if missing_ancestry {
        unavailable_assurances.push(RprovUnavailableAssurance::CompleteAncestry);
        gaps.push(RprovRecoveryGap::MissingAncestry {
            before_session_id: persisted.segment.session_id.clone(),
        });
    }
    unavailable_assurances.extend([
        RprovUnavailableAssurance::CompleteEventStream,
        RprovUnavailableAssurance::FinalCheckpoint,
    ]);
    gaps.extend(persisted.gaps.clone());
    sort_recovery_gaps(&mut gaps);
    if gaps
        .iter()
        .any(|gap| matches!(gap, RprovRecoveryGap::MissingEvidence { .. }))
    {
        unavailable_assurances.push(RprovUnavailableAssurance::ReferencedEvidence);
    }
    unavailable_assurances.extend([
        RprovUnavailableAssurance::FinalTree,
        RprovUnavailableAssurance::CleanFinalization,
    ]);
    let manifest = RprovManifest {
        format_version: RPROV_FORMAT_VERSION_V1,
        package_state: RprovPackageState::RecoveryIncomplete {
            unavailable_assurances,
            gaps,
        },
        submitted_source_comparison: RprovSubmittedSourceComparison::UnavailableStandalone,
        course_id: persisted.segment.course_id.clone(),
        assignment_id: persisted.segment.assignment_id.clone(),
        assignment_version: persisted.segment.assignment_version.clone(),
        student_id: persisted.student_id.clone(),
        latest_session_id: persisted.segment.session_id.clone(),
        original_starter_tree_hash,
        test_case_suite_hash: persisted.test_case_suite_hash,
        final_tree_hash: RprovKnown::Unknown,
        aggregate_event_count,
        producer: segments
            .last()
            .ok_or("recovery capture has no current segment")?
            .producer
            .clone(),
        assignment_manifest: persisted.assignment_manifest.clone(),
        initial_workspace,
        segments,
        inventory,
    };
    Ok((manifest, payloads))
}

fn rebase_prefix_capture(
    persisted: &PersistedCurrentCapture,
    ordinal: u32,
    parent: Option<RprovParentLink>,
    original_starter_tree_hash: Hash,
) -> Result<CapturedCurrent> {
    let mut segment = persisted.segment.clone();
    segment.ordinal = ordinal;
    segment.parent = parent;
    segment.original_starter_tree_hash = original_starter_tree_hash;
    segment.events.entry = rebase_segment_entry(&segment.events.entry, ordinal)?;
    for checkpoint in &mut segment.checkpoints {
        checkpoint.entry = rebase_segment_entry(&checkpoint.entry, ordinal)?;
    }
    for metadata in &mut segment.metadata {
        metadata.entry = rebase_segment_entry(&metadata.entry, ordinal)?;
    }
    for evidence in &mut segment.evidence {
        evidence.entry = rebase_segment_entry(&evidence.entry, ordinal)?;
    }
    let mut inventory = persisted.inventory.clone();
    for entry in &mut inventory {
        entry.path = rebase_segment_entry(&entry.path, ordinal)?;
    }
    let mut payloads = persisted.payloads.clone();
    for payload in &mut payloads {
        payload.entry = rebase_segment_entry(&payload.entry, ordinal)?;
    }
    inventory.sort_by(|left, right| left.path.cmp(&right.path));
    payloads.sort_by(|left, right| left.entry.cmp(&right.entry));
    Ok(CapturedCurrent {
        segment,
        inventory,
        payloads,
        publications: Vec::new(),
        prefix_bytes: read_declared_payload(&persisted.payloads, &persisted.segment.events.entry)?,
        initial_workspace: persisted.initial_workspace.clone(),
        final_workspace: BTreeMap::new(),
        gaps: persisted.gaps.clone(),
    })
}

fn validate_recovery_parts(
    manifest: &RprovManifest,
    mut payloads: Vec<FinalizationPayload>,
) -> Result<BTreeMap<WorkspacePath, Vec<u8>>> {
    manifest.validate()?;
    payloads.sort_by(|left, right| left.entry.cmp(&right.entry));
    if payloads.len() != manifest.inventory.len() {
        return Err("recovery payload sources do not cover the complete inventory".into());
    }
    for (declaration, source) in manifest.inventory.iter().zip(&payloads) {
        if declaration.path != source.entry
            || declaration.byte_length != source.byte_length
            || declaration.blake3 != source.blake3
        {
            return Err("recovery payload sources disagree with manifest inventory".into());
        }
        let bytes = read_payload_source(source)?;
        validate_rprov_payload(declaration, &bytes)?;
    }
    for segment in &manifest.segments {
        let events = read_declared_payload(&payloads, &segment.events.entry)?;
        validate_rprov_event_stream(manifest, segment, &events)?;
    }
    let initial_files = read_initial_workspace(manifest, &payloads)?;
    if hash_entries(
        initial_files
            .iter()
            .map(|(path, bytes)| (path, bytes.as_slice())),
    )? != manifest.original_starter_tree_hash
    {
        return Err("recovery starter blobs differ from the original starter tree".into());
    }
    let mut latest = None;
    for (index, segment) in manifest.segments.iter().enumerate() {
        let initial = checkpoint_files(&segment_initial_checkpoint(segment, &payloads)?);
        if index == 0
            && segment.initial_tree_hash == manifest.original_starter_tree_hash
            && initial != initial_files
        {
            return Err("original initial-workspace blobs differ from root genesis".into());
        }
        if let Some(previous) = &latest
            && initial != *previous
        {
            return Err(
                "child genesis bytes differ from its immediate parent final checkpoint".into(),
            );
        }
        let snapshot = if segment.events.completeness == RprovEventStreamCompleteness::Complete {
            replay_segment(segment, &payloads)?
        } else {
            replay_prefix_segment(segment, &payloads)?
        };
        latest = Some(checkpoint_files(&snapshot));
    }
    latest.ok_or_else(|| "recovery capture has no replayed workspace".into())
}

fn replay_prefix_segment(
    segment: &RprovSegment,
    payloads: &[FinalizationPayload],
) -> Result<CheckpointSnapshot> {
    let events = decode_event_stream(&read_declared_payload(payloads, &segment.events.entry)?)?;
    let first = events.first().ok_or("segment event stream is empty")?;
    let initial_ref = segment
        .checkpoints
        .first()
        .ok_or("segment has no initial checkpoint")?;
    let initial = decode_checkpoint(&read_declared_payload(payloads, &initial_ref.entry)?)?;
    let mut replay = ReplayEngine::from_initial_checkpoint(StoredCheckpoint {
        owning_event: first.clone(),
        snapshot: initial,
    })?;
    let checkpoints = segment
        .checkpoints
        .iter()
        .map(|checkpoint| (checkpoint.owner.sequence, checkpoint))
        .collect::<BTreeMap<_, _>>();
    let mut boundary = None;
    for envelope in events.iter().skip(1) {
        replay.apply(envelope)?;
        if matches!(envelope.event, Event::WorkspaceCheckpoint(_)) {
            let declaration = checkpoints
                .get(&envelope.sequence)
                .ok_or("replayed checkpoint has no declared payload")?;
            let snapshot =
                decode_checkpoint(&read_declared_payload(payloads, &declaration.entry)?)?;
            replay.validate_checkpoint(&StoredCheckpoint {
                owning_event: envelope.clone(),
                snapshot: snapshot.clone(),
            })?;
            boundary = Some(snapshot);
        }
    }
    let boundary = boundary.ok_or("prefix has no replay-certified boundary checkpoint")?;
    if replay.is_finalized() || replay.current_workspace_hash() != boundary.workspace_hash() {
        return Err("semantic replay did not reach the retained prefix boundary".into());
    }
    Ok(boundary)
}

fn validate_raw_current_capture(
    owner: &PinnedJournalFile,
    persisted: &PersistedCurrentCapture,
) -> Result<BTreeMap<WorkspacePath, Vec<u8>>> {
    validate_current_journal(owner, persisted)?;
    let mut inventory = persisted.inventory.clone();
    inventory.sort_by(|left, right| left.path.cmp(&right.path));
    let mut payloads = persisted.payloads.clone();
    payloads.sort_by(|left, right| left.entry.cmp(&right.entry));
    if inventory != persisted.inventory
        || payloads != persisted.payloads
        || inventory.len() != payloads.len()
        || inventory
            .windows(2)
            .any(|pair| pair[0].path == pair[1].path)
    {
        return Err("immutable raw capture inventory is not canonical and complete".into());
    }
    for (declaration, source) in inventory.iter().zip(&payloads) {
        if declaration.path != source.entry
            || declaration.byte_length != source.byte_length
            || declaration.blake3 != source.blake3
        {
            return Err("immutable raw capture payload identity mismatch".into());
        }
        validate_rprov_payload(declaration, &read_payload_source(source)?)?;
    }

    let declarations = inventory
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut referenced = BTreeSet::new();
    let mut require =
        |entry: &str, kind: RprovEntryKind, byte_length: u64, blake3: Hash| -> Result<()> {
            let declaration = declarations
                .get(entry)
                .ok_or("immutable raw capture reference has no payload")?;
            if declaration.kind != kind
                || declaration.byte_length != byte_length
                || declaration.blake3 != blake3
            {
                return Err("immutable raw capture reference identity mismatch".into());
            }
            referenced.insert(entry.to_owned());
            Ok(())
        };
    let segment = &persisted.segment;
    if segment.checkpoints.len() < 2
        || segment.checkpoints.len() > MAX_RPROV_CHECKPOINTS_PER_SEGMENT
        || segment.metadata.len() > MAX_RPROV_METADATA_PER_SEGMENT
        || segment.evidence.len() > MAX_RPROV_EVIDENCE_PER_SEGMENT
        || persisted
            .initial_workspace
            .as_ref()
            .is_some_and(|initial| initial.files.len() > MAX_RPROV_INITIAL_FILES)
    {
        return Err("immutable raw capture exceeds its structural item limits".into());
    }
    require(
        &segment.events.entry,
        RprovEntryKind::Events,
        segment.events.byte_length,
        segment.events.blake3,
    )?;
    for checkpoint in &segment.checkpoints {
        require(
            &checkpoint.entry,
            RprovEntryKind::Checkpoint,
            checkpoint.byte_length,
            checkpoint.blake3,
        )?;
    }
    for metadata in &segment.metadata {
        require(
            &metadata.entry,
            RprovEntryKind::RuntimeMetadata,
            metadata.byte_length,
            metadata.blake3,
        )?;
    }
    for evidence in &segment.evidence {
        require(
            &evidence.entry,
            RprovEntryKind::ExternalRecoveryEvidence,
            evidence.byte_length,
            evidence.blake3,
        )?;
    }

    let events = decode_event_stream(&read_declared_payload(&payloads, &segment.events.entry)?)?;
    if events.len() as u64 != segment.inclusive_event_count
        || events.last().map(|event| event.event_hash) != Some(segment.last_event_hash)
    {
        return Err("immutable raw capture event count/hash mismatch".into());
    }
    let events = events
        .iter()
        .map(|event| (event.sequence, event))
        .collect::<BTreeMap<_, _>>();
    for metadata in &segment.metadata {
        if events
            .get(&metadata.owner.sequence)
            .is_none_or(|event| event_reference(event) != metadata.owner)
        {
            return Err("immutable raw capture metadata owner mismatch".into());
        }
    }
    let mut evidence_usages = BTreeSet::new();
    for evidence in &segment.evidence {
        if evidence.usages.is_empty()
            || evidence.usages.len() > MAX_RPROV_EVIDENCE_USAGES_PER_ARTIFACT
        {
            return Err("immutable raw capture has an invalid evidence usage count".into());
        }
        for usage in &evidence.usages {
            if !evidence_usages.insert(usage.sequence)
                || !evidence_usage_matches(&events, usage, evidence.blake3)
            {
                return Err("immutable raw capture evidence usage mismatch".into());
            }
        }
    }
    for gap in &persisted.gaps {
        let RprovRecoveryGap::MissingEvidence { event, blake3 } = gap else {
            return Err("immutable raw capture contains a non-evidence gap".into());
        };
        if !evidence_usages.insert(event.sequence)
            || !evidence_usage_matches(&events, event, *blake3)
        {
            return Err("immutable raw capture evidence gap mismatch".into());
        }
    }
    if evidence_usages.len() > MAX_RPROV_EVIDENCE_USAGES {
        return Err("immutable raw capture exceeds the evidence usage limit".into());
    }
    if events.values().any(|event| {
        event_evidence_hash(&event.event).is_some() && !evidence_usages.contains(&event.sequence)
    }) {
        return Err("immutable raw capture omits an exact evidence usage".into());
    }

    match (
        persisted.original_starter_tree_hash,
        persisted.initial_workspace.as_ref(),
    ) {
        (Some(original), Some(initial)) => {
            let mut files = BTreeMap::new();
            for file in &initial.files {
                let declaration = declarations
                    .get(file.entry.as_str())
                    .ok_or("immutable raw capture starter file has no payload")?;
                if declaration.kind != RprovEntryKind::InitialWorkspaceBlob {
                    return Err("immutable raw capture starter payload has the wrong kind".into());
                }
                referenced.insert(file.entry.clone());
                if files
                    .insert(
                        file.path.clone(),
                        read_declared_payload(&payloads, &file.entry)?,
                    )
                    .is_some()
                {
                    return Err("immutable raw capture has duplicate starter paths".into());
                }
            }
            if hash_entries(files.iter().map(|(path, bytes)| (path, bytes.as_slice())))? != original
            {
                return Err("immutable raw capture starter bytes mismatch".into());
            }
        }
        (None, None) if persisted.had_parent => {}
        _ => return Err("immutable raw capture has incomplete starter seed state".into()),
    }
    if referenced.len() != inventory.len() {
        return Err("immutable raw capture has unreferenced payloads".into());
    }
    Ok(checkpoint_files(&replay_prefix_segment(
        segment, &payloads,
    )?))
}

fn evidence_usage_matches(
    events: &BTreeMap<u64, &EventEnvelope>,
    usage: &RecordedEventRef,
    blake3: Hash,
) -> bool {
    events.get(&usage.sequence).is_some_and(|event| {
        event_reference(event) == *usage && event_evidence_hash(&event.event) == Some(blake3)
    })
}

fn event_evidence_hash(event: &Event) -> Option<Hash> {
    match event {
        Event::ExternalObservation(event) => Some(event.evidence_hash),
        Event::RecoveryRecorded(event) => Some(event.evidence_hash),
        _ => None,
    }
}

fn sort_recovery_gaps(gaps: &mut [RprovRecoveryGap]) {
    gaps.sort_by(|left, right| match (left, right) {
        (
            RprovRecoveryGap::MissingAncestry {
                before_session_id: left,
            },
            RprovRecoveryGap::MissingAncestry {
                before_session_id: right,
            },
        ) => left.cmp(right),
        (RprovRecoveryGap::MissingAncestry { .. }, _) => std::cmp::Ordering::Less,
        (_, RprovRecoveryGap::MissingAncestry { .. }) => std::cmp::Ordering::Greater,
        (
            RprovRecoveryGap::MissingEvidence {
                event: left_event,
                blake3: left_blake3,
            },
            RprovRecoveryGap::MissingEvidence {
                event: right_event,
                blake3: right_blake3,
            },
        ) => left_event
            .session_id
            .cmp(&right_event.session_id)
            .then_with(|| left_event.sequence.cmp(&right_event.sequence))
            .then_with(|| left_event.event_hash.cmp(&right_event.event_hash))
            .then_with(|| left_blake3.cmp(right_blake3)),
        (RprovRecoveryGap::MissingEvidence { .. }, _) => std::cmp::Ordering::Less,
        (_, RprovRecoveryGap::MissingEvidence { .. }) => std::cmp::Ordering::Greater,
        (
            RprovRecoveryGap::MissingSourceLink { event: left },
            RprovRecoveryGap::MissingSourceLink { event: right },
        ) => left
            .session_id
            .cmp(&right.session_id)
            .then_with(|| left.sequence.cmp(&right.sequence))
            .then_with(|| left.event_hash.cmp(&right.event_hash)),
    });
}

fn validate_current_journal(
    owner: &PinnedJournalFile,
    persisted: &PersistedCurrentCapture,
) -> Result<()> {
    let expected_bytes =
        read_declared_payload(&persisted.payloads, &persisted.segment.events.entry)?;
    let mut journal = Journal::open_read_only_no_follow(owner.display_path())?;
    let chain = journal.verify_session_chain(&persisted.segment.session_id)?;
    if chain.event_count != persisted.segment.inclusive_event_count
        && chain.event_count
            != persisted
                .segment
                .inclusive_event_count
                .checked_add(1)
                .unwrap_or(0)
    {
        return Err("local journal is not the retained prefix or its terminal successor".into());
    }
    let events = read_all_events(&mut journal, &persisted.segment.session_id)?;
    let prefix_count = usize::try_from(persisted.segment.inclusive_event_count)
        .map_err(|_| "retained prefix event count does not fit this platform")?;
    if encode_event_stream(&events[..prefix_count])? != expected_bytes {
        return Err("local journal prefix differs from the immutable recovery capture".into());
    }
    let checkpoints = read_all_checkpoints(&mut journal, &persisted.segment.session_id)?;
    if checkpoints.len() != persisted.segment.checkpoints.len() {
        return Err("local checkpoint count differs from the immutable recovery capture".into());
    }
    for (stored, declaration) in checkpoints.iter().zip(&persisted.segment.checkpoints) {
        let bytes = read_declared_payload(&persisted.payloads, &declaration.entry)?;
        if declaration.owner != event_reference(&stored.owning_event)
            || encode_checkpoint(&stored.snapshot)? != bytes
        {
            return Err("local checkpoint differs from the immutable recovery capture".into());
        }
    }
    Ok(())
}

fn artifact_exists(owner: &PinnedJournalFile, name: &str) -> Result<bool> {
    owner.verify()?;
    let path = owner
        .display_path()
        .parent()
        .ok_or("state directory is missing")?
        .join(name);
    let exists = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(
                    format!("controlled state artifact {name} is not a regular file").into(),
                );
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    owner.verify()?;
    Ok(exists)
}

fn read_payload_source(payload: &FinalizationPayload) -> Result<Vec<u8>> {
    let maximum = usize::try_from(payload.byte_length)
        .map_err(|_| "receipt payload length does not fit this platform")?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(&payload.path)?;
    let before = file.metadata()?;
    if !before.is_file() || before.len() != payload.byte_length {
        return Err(format!("receipt payload {} has changed size/type", payload.entry).into());
    }
    let mut bytes = Vec::with_capacity(maximum.min(1024 * 1024));
    file.take(payload.byte_length.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() != maximum || rprov_raw_blake3(&bytes) != payload.blake3 {
        return Err(format!("receipt payload {} digest/length mismatch", payload.entry).into());
    }
    Ok(bytes)
}

fn read_declared_payload(payloads: &[FinalizationPayload], entry: &str) -> Result<Vec<u8>> {
    let source = payloads
        .binary_search_by(|payload| payload.entry.as_str().cmp(entry))
        .ok()
        .and_then(|index| payloads.get(index))
        .ok_or_else(|| format!("missing receipt payload source for {entry}"))?;
    read_payload_source(source)
}

fn read_all_events(journal: &mut Journal, session_id: &SessionId) -> Result<Vec<EventEnvelope>> {
    let chain = journal.verify_session_chain(session_id)?;
    if chain.event_count == 0 || chain.event_count > MAX_RPROV_EVENTS {
        return Err("journal event count is outside finalization limits".into());
    }
    let capacity =
        usize::try_from(chain.event_count).map_err(|_| "event count does not fit usize")?;
    let mut result = Vec::with_capacity(capacity.min(MAX_EVENTS_PER_READ));
    let mut next = 1_u64;
    while next <= chain.event_count {
        let page = journal.read_events(session_id, next, MAX_EVENTS_PER_READ)?;
        if page.is_empty() {
            return Err("journal event stream ended before its verified count".into());
        }
        next = page
            .last()
            .and_then(|event| event.sequence.checked_add(1))
            .ok_or("journal event sequence overflow")?;
        result.extend(page);
    }
    Ok(result)
}

fn read_all_checkpoints(
    journal: &mut Journal,
    session_id: &SessionId,
) -> Result<Vec<StoredCheckpoint>> {
    let verified = journal.verify_session_checkpoints(session_id)?;
    if verified.checkpoint_count == 0
        || verified.checkpoint_count as usize > MAX_RPROV_CHECKPOINTS_PER_SEGMENT
    {
        return Err("checkpoint count is outside finalization limits".into());
    }
    let mut result = Vec::with_capacity(verified.checkpoint_count as usize);
    let mut next = 1_u64;
    loop {
        let page = journal.list_checkpoints(session_id, next, MAX_CHECKPOINTS_PER_READ)?;
        if page.is_empty() {
            break;
        }
        next = page
            .last()
            .and_then(|checkpoint| checkpoint.owning_event.sequence.checked_add(1))
            .ok_or("checkpoint sequence overflow")?;
        result.extend(page);
    }
    if result.len() as u64 != verified.checkpoint_count {
        return Err("checkpoint listing differs from its verified count".into());
    }
    Ok(result)
}

fn encode_event_stream(events: &[EventEnvelope]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for event in events {
        let encoded = encode_envelope(event)?;
        let new_length = bytes
            .len()
            .checked_add(encoded.len())
            .and_then(|length| length.checked_add(1))
            .ok_or("event stream byte count overflow")?;
        if new_length as u64 > MAX_RPROV_SEGMENT_EVENTS_BYTES {
            return Err("event stream exceeds its bounded segment limit".into());
        }
        bytes.extend_from_slice(&encoded);
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn decode_event_stream(bytes: &[u8]) -> Result<Vec<EventEnvelope>> {
    let framed = bytes
        .strip_suffix(b"\n")
        .ok_or("event stream is missing its final LF")?;
    if framed.is_empty() {
        return Err("event stream is empty".into());
    }
    framed
        .split(|byte| *byte == b'\n')
        .map(
            |line| match decode_envelope(line, DecodePolicy::RejectUnsupported)? {
                DecodeOutcome::Decoded(event) => Ok(event),
                DecodeOutcome::Skipped(_) => Err("unsupported event was not rejected".into()),
            },
        )
        .collect()
}

fn event_reference(event: &EventEnvelope) -> RecordedEventRef {
    RecordedEventRef {
        session_id: event.session_id.clone(),
        sequence: event.sequence,
        event_hash: event.event_hash,
    }
}

fn checkpoint_files(snapshot: &CheckpointSnapshot) -> BTreeMap<WorkspacePath, Vec<u8>> {
    snapshot
        .files()
        .iter()
        .map(|file| (file.path.clone(), file.contents.clone()))
        .collect()
}

fn producer(metadata: &SessionMetadata, report: Option<&ToolchainReport>) -> RprovProducer {
    let mut tools = BTreeMap::new();
    if let Some(report) = report {
        for component in ["cargo", "rustc", "rustdoc", "rustup"] {
            if let Some(version) = report.version(component) {
                let value = version.trim();
                if !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control) {
                    tools.insert(component.to_owned(), value.to_owned());
                }
            }
        }
    }
    RprovProducer {
        client_version: RprovKnown::Known {
            value: metadata.client_version.clone(),
        },
        build_identity: RprovKnown::Known {
            value: metadata.build_identity.clone(),
        },
        os: RprovKnown::Known {
            value: std::env::consts::OS.to_owned(),
        },
        architecture: RprovKnown::Known {
            value: std::env::consts::ARCH.to_owned(),
        },
        rust_tools: tools
            .into_iter()
            .map(|(tool, version)| RprovToolVersion {
                tool,
                version: RprovKnown::Known { value: version },
            })
            .collect(),
    }
}

fn validate_student_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(
            "student identifier must use 1..=128 ASCII letters, digits, '-', '_', or '.'".into(),
        );
    }
    Ok(())
}

fn checked_aggregate(counts: impl IntoIterator<Item = u64>) -> Result<u64> {
    checked_aggregate_with_limit(counts, MAX_RPROV_EVENTS)
}

fn checked_aggregate_with_limit(
    counts: impl IntoIterator<Item = u64>,
    maximum: u64,
) -> Result<u64> {
    let total = counts.into_iter().try_fold(0_u64, |total, count| {
        total
            .checked_add(count)
            .ok_or("aggregate event count overflow")
    })?;
    if total == 0 || total > maximum.min(MAX_RPROV_EVENTS) {
        return Err("aggregate event count exceeds the package-wide limit".into());
    }
    Ok(total)
}

#[cfg(not(test))]
fn checked_aggregate_event_count(counts: impl IntoIterator<Item = u64>) -> Result<u64> {
    checked_aggregate(counts)
}

#[cfg(test)]
pub(super) fn checked_aggregate_event_count(counts: impl IntoIterator<Item = u64>) -> Result<u64> {
    checked_aggregate(counts)
}
