//! Student-facing inspection of the provenance a submission would contain.

use super::{
    FinalizationPayload, METADATA_LIMIT, ProductionSession, ReadOnlyFinalizationStatus, Result,
    SessionBudgets, SessionMetadata, digest,
};
use crate::{display, toolchain::RuntimeToolchainMetadata};
use rustrace_journal::{
    Journal, MAX_CHECKPOINTS_PER_READ, MAX_EVENTS_PER_READ, StoredCheckpoint, decode_checkpoint,
    encode_checkpoint,
};
use rustrace_model::{
    DecodeOutcome, DecodePolicy, EditorTransaction, Event, EventEnvelope, Hash,
    MAX_RPROV_CHECKPOINTS_PER_SEGMENT, MAX_RPROV_EVENTS, MAX_RPROV_MANIFEST_BYTES,
    MAX_RPROV_METADATA_ENTRY_BYTES, RprovEntryKind, RprovInitialWorkspace, RprovInventoryEntry,
    RprovKnown, RprovManifest, SessionId, decode_envelope, decode_rprov_manifest, encode_envelope,
    encode_rprov_manifest, rprov_raw_blake3,
};
use rustrace_replay::ReplayEngine;
use rustrace_workspace::hash::{
    PinnedStateDirectory, PinnedStateInspection, PinnedWorkspaceRoot, WorkspaceHashError,
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const EVENT_KIND_NAMES: [&str; 29] = [
    "controlled_command_started",
    "controlled_command_output",
    "controlled_command_finished",
    "test_case_compared",
    "session_started",
    "session_resumed",
    "session_ended",
    "file_created",
    "file_deleted",
    "file_renamed",
    "file_focused",
    "file_edited",
    "clipboard_copied",
    "internal_paste",
    "paste_rejected",
    "selection_changed",
    "viewport_changed",
    "cargo_command_started",
    "cargo_diagnostic",
    "cargo_output",
    "cargo_command_finished",
    "lsp_completion_requested",
    "lsp_completion_accepted",
    "lsp_code_action_applied",
    "workspace_checkpoint",
    "external_file_change",
    "external_observation",
    "recovery_recorded",
    "submission_finalized",
];

const POLICY_LINES: [&str; 3] = [
    "The data is used for grading only. There is no research use of the recorded data.",
    "Only the course's TAs and the instructor may review the data.",
    "All recorded data is permanently deleted after the term's grades are released.",
];

#[derive(Default)]
struct Totals {
    event_counts: [u64; EVENT_KIND_NAMES.len()],
    event_records: u64,
    event_stream_bytes: u64,
    edit_transactions: u64,
    inserted_scalars: u64,
    inserted_bytes: u64,
    deleted_scalars: u64,
    deleted_bytes: u64,
    command_records: u64,
    command_starts: u64,
    command_output_records: u64,
    command_output_bytes: u64,
    diagnostics: u64,
    diagnostic_bytes: u64,
    external_observations: u64,
    blocked_pastes: u64,
    blocked_paste_bytes: u64,
    checkpoints: u64,
    checkpoint_bytes: u64,
    metadata: u64,
    metadata_bytes: u64,
    evidence: u64,
    evidence_bytes: u64,
    initial_files: u64,
    initial_file_bytes: u64,
    initial_blobs: u64,
    initial_blob_bytes: u64,
}

struct AttemptSummary {
    session_id: SessionId,
    event_count: u64,
    first_millis: u64,
    last_millis: u64,
    started_at_utc: Option<String>,
    ended_at_utc: Option<String>,
}

struct PrivacySummary {
    state: &'static str,
    student_id: Option<String>,
    inherited_student_id: bool,
    course_id: String,
    assignment_id: String,
    assignment_version: String,
    latest_session_id: SessionId,
    original_starter_tree_hash: Hash,
    assignment_manifest_bytes: u64,
    attempts: Vec<AttemptSummary>,
    totals: Totals,
    preview: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisionLink {
    version: u32,
    kind: String,
    parent_root: PathBuf,
    parent_session_id: SessionId,
    parent_terminal_event_hash: Hash,
    parent_final_tree_hash: Hash,
    parent_manifest_blake3: Hash,
    #[serde(rename = "parent_sources_blake3")]
    _parent_sources_blake3: Hash,
    original_starter_tree_hash: Option<Hash>,
    #[serde(rename = "initial_workspace")]
    _initial_workspace: Option<RprovInitialWorkspace>,
    #[serde(rename = "initial_inventory")]
    _initial_inventory: Vec<RprovInventoryEntry>,
}

struct PreviewCapture {
    events: Vec<EventEnvelope>,
    framed_event_bytes: Vec<u64>,
    checkpoints: Vec<StoredCheckpoint>,
    metadata_entries: Vec<u64>,
    evidence_entries: Vec<u64>,
    manifest_bytes: Vec<u8>,
    parent_link: Option<RevisionLink>,
}

struct PublishedReceipt {
    manifest: RprovManifest,
    payloads: Vec<FinalizationPayload>,
}

impl PublishedReceipt {
    fn read_payload(&self, entry: &str) -> Result<Vec<u8>> {
        let payload = self
            .payloads
            .binary_search_by(|payload| payload.entry.as_str().cmp(entry))
            .ok()
            .and_then(|index| self.payloads.get(index))
            .ok_or_else(|| format!("published receipt has no payload for {entry}"))?;
        read_payload_source(payload)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedSources {
    version: u32,
    payloads: Vec<FinalizationPayload>,
}

struct ScratchDirectory(Option<PathBuf>);

impl ScratchDirectory {
    fn create() -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..1024 {
            let path = std::env::temp_dir().join(format!(
                "rustrace-privacy-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => match fs::canonicalize(&path) {
                    Ok(canonical) => return Ok(Self(Some(canonical))),
                    Err(error) => {
                        let _ = fs::remove_dir(&path);
                        return Err(error.into());
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err("could not allocate a private privacy-inspection directory".into())
    }

    fn path(&self) -> &Path {
        self.0.as_deref().expect("scratch directory is present")
    }

    fn remove(mut self) -> Result<()> {
        fs::remove_dir_all(self.path())?;
        self.0 = None;
        Ok(())
    }
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_dir_all(path);
        }
    }
}

pub fn run_privacy(args: &[String], output: &mut impl Write) -> Result<()> {
    let [workspace] = args else {
        return Err("Usage: rustrace privacy WORKSPACE".into());
    };
    let root = fs::canonicalize(Path::new(workspace))?;
    let metadata = ProductionSession::read_metadata(&root)?;
    let summary = match ProductionSession::inspect_finalization_read_only(&root)? {
        ReadOnlyFinalizationStatus::Finalized(_) => {
            summarize_receipt(&load_published_receipt(&root, &metadata)?)?
        }
        ReadOnlyFinalizationStatus::Unfinished => summarize_unfinished(&root, &metadata)?,
        ReadOnlyFinalizationStatus::Prepared { .. } => {
            return Err(
                "finalization is prepared but incomplete; preserve it and run `rustrace submit` or inspect recovery before privacy display"
                    .into(),
            );
        }
        ReadOnlyFinalizationStatus::Incomplete { .. } => {
            return Err(
                "finalization is incomplete; preserve and inspect recovery before privacy display"
                    .into(),
            );
        }
    };
    write_summary(&summary, output)
}

fn summarize_receipt(receipt: &PublishedReceipt) -> Result<PrivacySummary> {
    let manifest = &receipt.manifest;
    let mut totals = totals_from_inventory(manifest)?;
    let mut attempts = Vec::with_capacity(manifest.segments.len());
    for segment in &manifest.segments {
        let bytes = receipt.read_payload(&segment.events.entry)?;
        if bytes.len() as u64 != segment.events.byte_length {
            return Err("receipt event bytes differ from the manifest".into());
        }
        let (events, framed) = decode_event_stream(&bytes)?;
        let initial = segment
            .checkpoints
            .first()
            .ok_or("receipt segment has no initial checkpoint")?;
        let initial = decode_checkpoint(&receipt.read_payload(&initial.entry)?)?;
        let (first_millis, last_millis) = summarize_events(
            &events,
            &framed,
            StoredCheckpoint {
                owning_event: events
                    .first()
                    .ok_or("receipt segment has no events")?
                    .clone(),
                snapshot: initial,
            },
            None,
            &mut totals,
        )?;
        if events.len() as u64 != segment.inclusive_event_count {
            return Err("receipt event count differs from the manifest".into());
        }
        attempts.push(AttemptSummary {
            session_id: segment.session_id.clone(),
            event_count: segment.inclusive_event_count,
            first_millis,
            last_millis,
            started_at_utc: known_time(&segment.time.started_at_utc),
            ended_at_utc: known_time(&segment.time.ended_at_utc),
        });
    }
    if totals.event_records != manifest.aggregate_event_count {
        return Err("derived event count differs from the receipt aggregate".into());
    }
    Ok(PrivacySummary {
        state: "FINALIZED",
        student_id: Some(manifest.student_id.clone()),
        inherited_student_id: false,
        course_id: manifest.course_id.clone(),
        assignment_id: manifest.assignment_id.clone(),
        assignment_version: manifest.assignment_version.clone(),
        latest_session_id: manifest.latest_session_id.clone(),
        original_starter_tree_hash: manifest.original_starter_tree_hash,
        assignment_manifest_bytes: manifest.assignment_manifest.byte_length,
        attempts,
        totals,
        preview: false,
    })
}

fn summarize_unfinished(root: &Path, metadata: &SessionMetadata) -> Result<PrivacySummary> {
    let capture = capture_preview(root, metadata)?;
    if capture
        .checkpoints
        .first()
        .ok_or("unfinished journal has no initial checkpoint")?
        .snapshot
        .workspace_hash()
        != metadata.starter_hash
    {
        return Err("unfinished initial checkpoint differs from the starter identity".into());
    }
    let parent = match &capture.parent_link {
        Some(link) if link.kind == "finalized_revision" => {
            Some(load_preview_parent(link, metadata)?)
        }
        _ => None,
    };
    let mut summary = match parent {
        Some(receipt) => {
            let mut summary = summarize_receipt(&receipt)?;
            summary.state = "PREVIEW - UNFINISHED DURABLE PREFIX";
            summary.inherited_student_id = true;
            summary.latest_session_id = metadata.session_id.clone();
            summary.preview = true;
            summary
        }
        None => {
            let initial = capture
                .checkpoints
                .first()
                .ok_or("unfinished journal has no initial checkpoint")?;
            let mut totals = Totals::default();
            add_initial_workspace(&mut totals, initial)?;
            PrivacySummary {
                state: "PREVIEW - UNFINISHED DURABLE PREFIX",
                student_id: None,
                inherited_student_id: false,
                course_id: metadata.course_id.clone(),
                assignment_id: metadata.assignment_id.clone(),
                assignment_version: metadata.assignment_version.clone(),
                latest_session_id: metadata.session_id.clone(),
                original_starter_tree_hash: metadata.starter_hash,
                assignment_manifest_bytes: capture.manifest_bytes.len() as u64,
                attempts: Vec::new(),
                totals,
                preview: true,
            }
        }
    };

    let initial = capture
        .checkpoints
        .first()
        .ok_or("unfinished journal has no initial checkpoint")?
        .clone();
    let (first_millis, last_millis) = summarize_events(
        &capture.events,
        &capture.framed_event_bytes,
        initial,
        Some(&capture.checkpoints),
        &mut summary.totals,
    )?;
    add_pair(
        &mut summary.totals.checkpoints,
        &mut summary.totals.checkpoint_bytes,
        &capture
            .checkpoints
            .iter()
            .map(|checkpoint| {
                encode_checkpoint(&checkpoint.snapshot).map(|bytes| bytes.len() as u64)
            })
            .collect::<std::result::Result<Vec<_>, _>>()?,
        "checkpoint inventory",
    )?;
    add_pair(
        &mut summary.totals.metadata,
        &mut summary.totals.metadata_bytes,
        &capture.metadata_entries,
        "runtime metadata inventory",
    )?;
    add_pair(
        &mut summary.totals.evidence,
        &mut summary.totals.evidence_bytes,
        &capture.evidence_entries,
        "external evidence inventory",
    )?;
    summary.attempts.push(AttemptSummary {
        session_id: metadata.session_id.clone(),
        event_count: capture.events.len() as u64,
        first_millis,
        last_millis,
        started_at_utc: capture
            .events
            .iter()
            .find_map(|event| event.wall_clock_utc.as_ref().map(ToString::to_string)),
        ended_at_utc: capture
            .events
            .iter()
            .rev()
            .find_map(|event| event.wall_clock_utc.as_ref().map(ToString::to_string)),
    });
    Ok(summary)
}

fn lock_for_privacy_inspection(state: PinnedStateDirectory) -> Result<PinnedStateInspection> {
    match state.lock_for_inspection() {
        Ok(inspection) => Ok(inspection),
        Err(WorkspaceHashError::Filesystem {
            operation: "acquire exclusive workspace writer ownership",
            ..
        }) => {
            Err("a rustrace session is currently open on this workspace; close it and retry".into())
        }
        Err(error) => Err(error.into()),
    }
}

fn capture_preview(root: &Path, metadata: &SessionMetadata) -> Result<PreviewCapture> {
    let pinned = PinnedWorkspaceRoot::open(root)?;
    let state = pinned.open_existing_state_directory()?;
    let mut inspection = lock_for_privacy_inspection(state)?;
    let captured = (|| -> Result<PreviewCapture> {
        let inventory = inspection.inventory(SessionBudgets::default().storage_bytes, 1024)?;
        let manifest_bytes = inspection.read_artifact("manifest.toml", METADATA_LIMIT)?;
        if digest(&manifest_bytes) != metadata.manifest_hash {
            return Err("assignment manifest changed during privacy inspection".into());
        }
        let scratch = ScratchDirectory::create()?;
        let journal_name = format!("{}.sqlite", metadata.session_id);
        for name in [
            journal_name.clone(),
            format!("{journal_name}-wal"),
            format!("{journal_name}-shm"),
        ] {
            let Some((_, length, expected_digest)) = inventory
                .iter()
                .find(|(candidate, _, _)| candidate == &name)
            else {
                if name == journal_name {
                    return Err("unfinished session journal is missing".into());
                }
                continue;
            };
            let mut options = OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            }
            let source = options.open(root.join(".rustrace").join(&name))?;
            if source.metadata()?.len() != *length {
                return Err("journal artifact changed during privacy inspection".into());
            }
            let destination = scratch.path().join(&name);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?;
            let mut source = source;
            let mut hasher = blake3::Hasher::new();
            let mut copied = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            while copied < *length {
                let count = usize::try_from((*length - copied).min(buffer.len() as u64))?;
                source.read_exact(&mut buffer[..count])?;
                hasher.update(&buffer[..count]);
                file.write_all(&buffer[..count])?;
                copied += count as u64;
            }
            if source.read(&mut buffer[..1])? != 0
                || Hash::from_bytes(*hasher.finalize().as_bytes()) != *expected_digest
            {
                return Err("journal artifact changed during privacy inspection".into());
            }
        }
        let (events, checkpoints) = {
            let mut journal =
                Journal::open_read_only_no_follow(scratch.path().join(&journal_name))?;
            (
                read_all_events(&mut journal, &metadata.session_id)?,
                read_all_checkpoints(&mut journal, &metadata.session_id)?,
            )
        };
        scratch.remove()?;
        let framed_event_bytes = events
            .iter()
            .map(|event| {
                encode_envelope(event)
                    .map(|bytes| bytes.len() as u64 + 1)
                    .map_err(Into::into)
            })
            .collect::<Result<Vec<_>>>()?;
        let event_by_sequence = events
            .iter()
            .map(|event| (event.sequence, event.event_hash))
            .collect::<BTreeMap<_, _>>();

        let mut metadata_by_digest = BTreeMap::new();
        for (name, length, _) in &inventory {
            if !name.starts_with("toolchain-") || !name.ends_with(".json") {
                continue;
            }
            let bytes = inspection.read_artifact(name, MAX_RPROV_METADATA_ENTRY_BYTES as usize)?;
            let observation: RuntimeToolchainMetadata = serde_json::from_slice(&bytes)?;
            if observation.version != 1
                || observation.session_id != metadata.session_id
                || observation.manifest_hash != metadata.manifest_hash
                || event_by_sequence.get(&observation.sequence) != Some(&observation.event_hash)
                || bytes.len() as u64 != *length
            {
                return Err("runtime metadata differs from the durable prefix".into());
            }
            metadata_by_digest
                .entry(rprov_raw_blake3(&bytes))
                .or_insert(*length);
        }

        let mut evidence_names = BTreeSet::new();
        for envelope in &events {
            match &envelope.event {
                Event::ExternalObservation(observation) => {
                    evidence_names.insert(format!("evidence-{}.bin", observation.evidence_hash));
                }
                Event::RecoveryRecorded(recorded) => {
                    let name = if recorded.decision
                        == rustrace_model::RecoveryDecision::AbandonPreserved
                    {
                        "parent.json".to_owned()
                    } else {
                        format!("evidence-{}.bin", recorded.evidence_hash)
                    };
                    evidence_names.insert(name);
                }
                _ => {}
            }
        }
        let mut evidence_by_digest = BTreeMap::new();
        for name in evidence_names {
            let length = inventory
                .iter()
                .find_map(|(candidate, length, _)| (candidate == &name).then_some(*length))
                .ok_or("durable prefix references missing external evidence")?;
            let maximum = usize::try_from(length)
                .map_err(|_| "external evidence length does not fit this platform")?;
            let bytes = inspection.read_artifact(&name, maximum)?;
            if bytes.len() as u64 != length {
                return Err("external evidence changed during privacy inspection".into());
            }
            evidence_by_digest
                .entry(rprov_raw_blake3(&bytes))
                .or_insert(length);
        }

        let parent_link = if let Some(expected) = metadata.parent_evidence {
            let bytes = inspection.read_artifact("parent.json", METADATA_LIMIT)?;
            if digest(&bytes) != expected {
                return Err("linked-attempt evidence digest mismatch".into());
            }
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            if value.get("kind").and_then(serde_json::Value::as_str) == Some("finalized_revision") {
                Some(serde_json::from_value(value)?)
            } else {
                None
            }
        } else {
            None
        };
        inspection.verify()?;
        Ok(PreviewCapture {
            events,
            framed_event_bytes,
            checkpoints,
            metadata_entries: metadata_by_digest.into_values().collect(),
            evidence_entries: evidence_by_digest.into_values().collect(),
            manifest_bytes,
            parent_link,
        })
    })();
    inspection.release_ownership()?;
    captured
}

fn load_published_receipt(root: &Path, metadata: &SessionMetadata) -> Result<PublishedReceipt> {
    let pinned = PinnedWorkspaceRoot::open(root)?;
    let mut inspection = lock_for_privacy_inspection(pinned.open_existing_state_directory()?)?;
    let loaded = (|| -> Result<PublishedReceipt> {
        let summary = match ProductionSession::inspect_finalization_read_only(root)? {
            ReadOnlyFinalizationStatus::Finalized(summary) => summary,
            _ => return Err("published finalization changed during privacy inspection".into()),
        };
        let manifest_bytes = inspection.read_artifact(
            "finalization-candidate-manifest.json",
            MAX_RPROV_MANIFEST_BYTES,
        )?;
        let manifest = decode_rprov_manifest(&manifest_bytes)?;
        if encode_rprov_manifest(&manifest)? != manifest_bytes {
            return Err("published privacy manifest is not canonically encoded".into());
        }
        manifest.validate()?;

        let source_bytes =
            inspection.read_artifact("finalization-sources.json", MAX_RPROV_MANIFEST_BYTES)?;
        let mut sources: PublishedSources = serde_json::from_slice(&source_bytes)?;
        if sources.version != 1 {
            return Err("unsupported published payload-source version".into());
        }
        sources
            .payloads
            .sort_by(|left, right| left.entry.cmp(&right.entry));
        if sources.payloads.len() != manifest.inventory.len()
            || manifest
                .inventory
                .iter()
                .zip(&sources.payloads)
                .any(|(entry, source)| {
                    entry.path != source.entry
                        || entry.byte_length != source.byte_length
                        || entry.blake3 != source.blake3
                })
        {
            return Err("published payload sources differ from the privacy manifest".into());
        }
        for source in &sources.payloads {
            verify_payload_source(source)?;
        }

        let assignment = inspection.read_artifact("manifest.toml", METADATA_LIMIT)?;
        let tip = manifest
            .segments
            .last()
            .ok_or("published privacy manifest has no attempt")?;
        let ancestry = manifest
            .segments
            .iter()
            .map(|segment| segment.session_id.clone())
            .collect::<Vec<_>>();
        if manifest.student_id != summary.student_id
            || manifest.latest_session_id != summary.latest_session_id
            || manifest.aggregate_event_count != summary.aggregate_event_count
            || manifest.final_tree_hash.known() != Some(&summary.final_tree_hash)
            || tip.terminal_event_hash.known() != Some(&summary.terminal_chain_hash)
            || ancestry != summary.ancestry_session_ids
            || metadata.session_id != manifest.latest_session_id
            || metadata.course_id != manifest.course_id
            || metadata.assignment_id != manifest.assignment_id
            || metadata.assignment_version != manifest.assignment_version
            || metadata.test_case_suite_hash != manifest.test_case_suite_hash
            || manifest.assignment_manifest.byte_length != assignment.len() as u64
            || manifest.assignment_manifest.blake3 != rprov_raw_blake3(&assignment)
        {
            return Err("published receipt identity differs from the privacy manifest".into());
        }
        inspection.verify()?;
        Ok(PublishedReceipt {
            manifest,
            payloads: sources.payloads,
        })
    })();
    inspection.release_ownership()?;
    loaded
}

fn verify_payload_source(payload: &FinalizationPayload) -> Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(&payload.path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != payload.byte_length {
        return Err(format!("published payload {} changed size or type", payload.entry).into());
    }
    let mut hasher = blake3::Hasher::new();
    let mut remaining = payload.byte_length;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining != 0 {
        let count = usize::try_from(remaining.min(buffer.len() as u64))?;
        file.read_exact(&mut buffer[..count])?;
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
    }
    if file.read(&mut buffer[..1])? != 0
        || Hash::from_bytes(*hasher.finalize().as_bytes()) != payload.blake3
    {
        return Err(format!("published payload {} changed bytes", payload.entry).into());
    }
    Ok(())
}

fn read_payload_source(payload: &FinalizationPayload) -> Result<Vec<u8>> {
    let maximum = usize::try_from(payload.byte_length)
        .map_err(|_| "published payload length does not fit this platform")?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(&payload.path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != payload.byte_length {
        return Err(format!("published payload {} changed size or type", payload.entry).into());
    }
    let mut bytes = Vec::with_capacity(maximum.min(1024 * 1024));
    file.take(payload.byte_length.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() != maximum || rprov_raw_blake3(&bytes) != payload.blake3 {
        return Err(format!("published payload {} changed bytes", payload.entry).into());
    }
    Ok(bytes)
}

fn load_preview_parent(
    link: &RevisionLink,
    metadata: &SessionMetadata,
) -> Result<PublishedReceipt> {
    if link.version != 1 || link.kind != "finalized_revision" {
        return Err("linked attempt does not identify finalized revision ancestry".into());
    }
    let parent_metadata = ProductionSession::read_metadata(&link.parent_root).map_err(|error| {
        format!(
            "linked parent session {} at {} is unavailable; expected finalized provenance: {error}",
            link.parent_session_id,
            link.parent_root.display()
        )
    })?;
    let receipt = load_published_receipt(&link.parent_root, &parent_metadata).map_err(|error| {
        format!(
            "linked parent session {} at {} is unavailable; expected finalized provenance: {error}",
            link.parent_session_id,
            link.parent_root.display()
        )
    })?;
    let manifest = &receipt.manifest;
    let tip = manifest
        .segments
        .last()
        .ok_or("linked parent receipt has no attempt")?;
    if link.parent_session_id != tip.session_id
        || link.parent_terminal_event_hash != tip.last_event_hash
        || tip.final_tree_hash.known() != Some(&link.parent_final_tree_hash)
        || metadata.starter_hash != link.parent_final_tree_hash
        || metadata.course_id != manifest.course_id
        || metadata.assignment_id != manifest.assignment_id
        || metadata.assignment_version != manifest.assignment_version
        || metadata.manifest_hash != manifest.assignment_manifest.blake3
        || metadata.test_case_suite_hash != manifest.test_case_suite_hash
        || link.original_starter_tree_hash != Some(manifest.original_starter_tree_hash)
        || link.parent_manifest_blake3 != rprov_raw_blake3(&encode_rprov_manifest(manifest)?)
    {
        return Err("linked parent identity differs from the unfinished attempt".into());
    }
    Ok(receipt)
}

fn totals_from_inventory(manifest: &RprovManifest) -> Result<Totals> {
    let mut totals = Totals {
        initial_files: manifest.initial_workspace.files.len() as u64,
        ..Totals::default()
    };
    totals.initial_file_bytes =
        manifest
            .initial_workspace
            .files
            .iter()
            .try_fold(0_u64, |sum, file| {
                let length = manifest
                    .inventory
                    .iter()
                    .find(|entry| entry.path == file.entry)
                    .ok_or("initial workspace file has no inventory entry")?
                    .byte_length;
                sum.checked_add(length)
                    .ok_or("initial workspace byte count overflow")
            })?;
    let by_kind = |kind| {
        manifest
            .inventory
            .iter()
            .filter(|entry| entry.kind == kind)
            .map(|entry| entry.byte_length)
            .collect::<Vec<_>>()
    };
    add_pair(
        &mut totals.initial_blobs,
        &mut totals.initial_blob_bytes,
        &by_kind(RprovEntryKind::InitialWorkspaceBlob),
        "initial workspace inventory",
    )?;
    add_pair(
        &mut totals.checkpoints,
        &mut totals.checkpoint_bytes,
        &by_kind(RprovEntryKind::Checkpoint),
        "checkpoint inventory",
    )?;
    add_pair(
        &mut totals.metadata,
        &mut totals.metadata_bytes,
        &by_kind(RprovEntryKind::RuntimeMetadata),
        "runtime metadata inventory",
    )?;
    add_pair(
        &mut totals.evidence,
        &mut totals.evidence_bytes,
        &by_kind(RprovEntryKind::ExternalRecoveryEvidence),
        "external evidence inventory",
    )?;
    Ok(totals)
}

fn add_initial_workspace(totals: &mut Totals, initial: &StoredCheckpoint) -> Result<()> {
    totals.initial_files = initial.snapshot.files().len() as u64;
    let mut blobs = BTreeMap::new();
    for file in initial.snapshot.files() {
        totals.initial_file_bytes = checked_add(
            totals.initial_file_bytes,
            file.contents.len() as u64,
            "initial workspace byte count",
        )?;
        blobs
            .entry(rprov_raw_blake3(&file.contents))
            .or_insert(file.contents.len() as u64);
    }
    totals.initial_blobs = blobs.len() as u64;
    totals.initial_blob_bytes = blobs.values().try_fold(0_u64, |sum, length| {
        checked_add(sum, *length, "initial workspace blob byte count")
    })?;
    Ok(())
}

fn add_pair(count: &mut u64, bytes: &mut u64, entries: &[u64], label: &'static str) -> Result<()> {
    *count = checked_add(*count, entries.len() as u64, label)?;
    for length in entries {
        *bytes = checked_add(*bytes, *length, label)?;
    }
    Ok(())
}

fn summarize_events(
    events: &[EventEnvelope],
    framed_bytes: &[u64],
    initial: StoredCheckpoint,
    checkpoints: Option<&[StoredCheckpoint]>,
    totals: &mut Totals,
) -> Result<(u64, u64)> {
    if events.is_empty() || events.len() != framed_bytes.len() {
        return Err("event stream is empty or has inconsistent framing".into());
    }
    if initial.owning_event != events[0] {
        return Err("initial checkpoint owner differs from the event stream".into());
    }
    let mut replay = ReplayEngine::from_initial_checkpoint(initial)?;
    let checkpoint_by_sequence = checkpoints.map(|items| {
        items
            .iter()
            .map(|checkpoint| (checkpoint.owning_event.sequence, checkpoint))
            .collect::<BTreeMap<_, _>>()
    });
    for (index, (envelope, encoded_bytes)) in events.iter().zip(framed_bytes).enumerate() {
        tally_event(totals, envelope, *encoded_bytes)?;
        if index == 0 {
            continue;
        }
        if let Some(transaction) = event_transaction(&envelope.event) {
            tally_transaction(totals, &replay, transaction)?;
        }
        replay.apply(envelope)?;
        if matches!(envelope.event, Event::WorkspaceCheckpoint(_))
            && let Some(checkpoint) = checkpoint_by_sequence
                .as_ref()
                .and_then(|items| items.get(&envelope.sequence))
        {
            replay.validate_checkpoint(checkpoint)?;
        }
    }
    Ok((
        events.first().expect("nonempty").monotonic_millis,
        events.last().expect("nonempty").monotonic_millis,
    ))
}

fn tally_event(totals: &mut Totals, envelope: &EventEnvelope, encoded_bytes: u64) -> Result<()> {
    let index = event_kind_index(&envelope.event);
    totals.event_counts[index] = checked_add(totals.event_counts[index], 1, "event-kind count")?;
    totals.event_records = checked_add(totals.event_records, 1, "event count")?;
    totals.event_stream_bytes = checked_add(
        totals.event_stream_bytes,
        encoded_bytes,
        "event-stream byte count",
    )?;
    match &envelope.event {
        Event::ControlledCommandStarted(_) | Event::CargoCommandStarted(_) => {
            totals.command_starts = checked_add(totals.command_starts, 1, "command start count")?;
            totals.command_records =
                checked_add(totals.command_records, 1, "command record count")?;
        }
        Event::ControlledCommandOutput(output) => {
            totals.command_records =
                checked_add(totals.command_records, 1, "command record count")?;
            totals.command_output_records = checked_add(
                totals.command_output_records,
                1,
                "command output record count",
            )?;
            totals.command_output_bytes = checked_add(
                totals.command_output_bytes,
                (output.bytes_hex.len() / 2) as u64,
                "command output byte count",
            )?;
        }
        Event::CargoOutput(output) => {
            totals.command_records =
                checked_add(totals.command_records, 1, "command record count")?;
            totals.command_output_records = checked_add(
                totals.command_output_records,
                1,
                "command output record count",
            )?;
            totals.command_output_bytes = checked_add(
                totals.command_output_bytes,
                output.output.len() as u64,
                "command output byte count",
            )?;
        }
        Event::ControlledCommandFinished(_) | Event::CargoCommandFinished(_) => {
            totals.command_records =
                checked_add(totals.command_records, 1, "command record count")?;
        }
        Event::CargoDiagnostic(diagnostic) => {
            totals.diagnostics = checked_add(totals.diagnostics, 1, "diagnostic count")?;
            let bytes = diagnostic.message.len() + diagnostic.code.as_ref().map_or(0, String::len);
            totals.diagnostic_bytes = checked_add(
                totals.diagnostic_bytes,
                bytes as u64,
                "diagnostic byte count",
            )?;
        }
        Event::ExternalObservation(_) => {
            totals.external_observations = checked_add(
                totals.external_observations,
                1,
                "external observation count",
            )?;
        }
        Event::PasteRejected(_) => {
            totals.blocked_pastes = checked_add(totals.blocked_pastes, 1, "blocked-paste count")?;
            totals.blocked_paste_bytes = checked_add(
                totals.blocked_paste_bytes,
                encoded_bytes,
                "blocked-paste metadata byte count",
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn event_transaction(event: &Event) -> Option<&EditorTransaction> {
    match event {
        Event::FileEdited(transaction) => Some(transaction),
        Event::InternalPaste(paste) => Some(&paste.transaction),
        _ => None,
    }
}

fn tally_transaction(
    totals: &mut Totals,
    replay: &ReplayEngine,
    transaction: &EditorTransaction,
) -> Result<()> {
    let text = replay
        .workspace_state()
        .document(&transaction.document_id)
        .ok_or("edit transaction refers to a missing pre-edit document")?
        .text();
    totals.edit_transactions = checked_add(totals.edit_transactions, 1, "edit transaction count")?;
    for edit in &transaction.edits {
        totals.inserted_bytes = checked_add(
            totals.inserted_bytes,
            edit.inserted_text.len() as u64,
            "inserted byte count",
        )?;
        totals.inserted_scalars = checked_add(
            totals.inserted_scalars,
            edit.inserted_text.chars().count() as u64,
            "inserted scalar count",
        )?;
        let start = usize::try_from(edit.start_byte)
            .map_err(|_| "edit start does not fit this platform")?;
        let end =
            usize::try_from(edit.end_byte).map_err(|_| "edit end does not fit this platform")?;
        let removed = text
            .get(start..end)
            .ok_or("edit deletion range is not valid UTF-8 in the pre-edit document")?;
        totals.deleted_bytes = checked_add(
            totals.deleted_bytes,
            removed.len() as u64,
            "deleted byte count",
        )?;
        totals.deleted_scalars = checked_add(
            totals.deleted_scalars,
            removed.chars().count() as u64,
            "deleted scalar count",
        )?;
    }
    Ok(())
}

fn event_kind_index(event: &Event) -> usize {
    match event {
        Event::ControlledCommandStarted(_) => 0,
        Event::ControlledCommandOutput(_) => 1,
        Event::ControlledCommandFinished(_) => 2,
        Event::TestCaseCompared(_) => 3,
        Event::SessionStarted(_) => 4,
        Event::SessionResumed(_) => 5,
        Event::SessionEnded(_) => 6,
        Event::FileCreated(_) => 7,
        Event::FileDeleted(_) => 8,
        Event::FileRenamed(_) => 9,
        Event::FileFocused(_) => 10,
        Event::FileEdited(_) => 11,
        Event::ClipboardCopied(_) => 12,
        Event::InternalPaste(_) => 13,
        Event::PasteRejected(_) => 14,
        Event::SelectionChanged(_) => 15,
        Event::ViewportChanged(_) => 16,
        Event::CargoCommandStarted(_) => 17,
        Event::CargoDiagnostic(_) => 18,
        Event::CargoOutput(_) => 19,
        Event::CargoCommandFinished(_) => 20,
        Event::LspCompletionRequested(_) => 21,
        Event::LspCompletionAccepted(_) => 22,
        Event::LspCodeActionApplied(_) => 23,
        Event::WorkspaceCheckpoint(_) => 24,
        Event::ExternalFileChange(_) => 25,
        Event::ExternalObservation(_) => 26,
        Event::RecoveryRecorded(_) => 27,
        Event::SubmissionFinalized(_) => 28,
    }
}

fn decode_event_stream(bytes: &[u8]) -> Result<(Vec<EventEnvelope>, Vec<u64>)> {
    let framed = bytes
        .strip_suffix(b"\n")
        .ok_or("event stream is missing its final LF")?;
    if framed.is_empty() {
        return Err("event stream is empty".into());
    }
    let mut events = Vec::new();
    let mut lengths = Vec::new();
    for line in framed.split(|byte| *byte == b'\n') {
        let event = match decode_envelope(line, DecodePolicy::RejectUnsupported)? {
            DecodeOutcome::Decoded(event) => event,
            DecodeOutcome::Skipped(_) => return Err("unsupported event was not rejected".into()),
        };
        events.push(event);
        lengths.push(line.len() as u64 + 1);
    }
    Ok((events, lengths))
}

fn read_all_events(journal: &mut Journal, session_id: &SessionId) -> Result<Vec<EventEnvelope>> {
    let chain = journal.verify_session_chain(session_id)?;
    if chain.event_count == 0 || chain.event_count > MAX_RPROV_EVENTS {
        return Err("journal event count is outside privacy-inspection limits".into());
    }
    let mut events = Vec::with_capacity((chain.event_count as usize).min(MAX_EVENTS_PER_READ));
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
        events.extend(page);
    }
    if events.len() as u64 != chain.event_count {
        return Err("journal read differs from its verified event count".into());
    }
    Ok(events)
}

fn read_all_checkpoints(
    journal: &mut Journal,
    session_id: &SessionId,
) -> Result<Vec<StoredCheckpoint>> {
    let verified = journal.verify_session_checkpoints(session_id)?;
    if verified.checkpoint_count == 0
        || verified.checkpoint_count as usize > MAX_RPROV_CHECKPOINTS_PER_SEGMENT
    {
        return Err("checkpoint count is outside privacy-inspection limits".into());
    }
    let mut checkpoints = Vec::with_capacity(verified.checkpoint_count as usize);
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
        checkpoints.extend(page);
    }
    if checkpoints.len() as u64 != verified.checkpoint_count {
        return Err("checkpoint listing differs from its verified count".into());
    }
    Ok(checkpoints)
}

fn known_time<T: ToString>(value: &RprovKnown<T>) -> Option<String> {
    value.known().map(ToString::to_string)
}

fn checked_add(left: u64, right: u64, label: &'static str) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| format!("{label} overflow").into())
}

fn write_summary(summary: &PrivacySummary, output: &mut impl Write) -> Result<()> {
    let mut text = String::new();
    safe_line(&mut text, format_args!("Rustrace provenance manifest"));
    safe_line(&mut text, format_args!("State: {}", summary.state));
    match &summary.student_id {
        Some(student_id) => safe_line(&mut text, format_args!("Student ID: {student_id}")),
        None => safe_line(
            &mut text,
            format_args!("Student ID: not recorded until finalization"),
        ),
    }
    if summary.inherited_student_id {
        safe_line(
            &mut text,
            format_args!(
                "Student ID comes from finalized ancestry; this attempt must use the same value when finalized."
            ),
        );
    }
    safe_line(&mut text, format_args!("Course ID: {}", summary.course_id));
    safe_line(
        &mut text,
        format_args!("Assignment ID: {}", summary.assignment_id),
    );
    safe_line(
        &mut text,
        format_args!("Assignment version: {}", summary.assignment_version),
    );
    safe_line(
        &mut text,
        format_args!("Latest session ID: {}", summary.latest_session_id),
    );
    safe_line(
        &mut text,
        format_args!(
            "Original starter tree hash: {}",
            summary.original_starter_tree_hash
        ),
    );
    safe_line(
        &mut text,
        format_args!(
            "Assignment manifest: {} bytes",
            summary.assignment_manifest_bytes
        ),
    );
    safe_line(
        &mut text,
        format_args!("Attempts: {}", summary.attempts.len()),
    );
    safe_line(&mut text, format_args!("Ordered attempt ancestry:"));
    for (index, attempt) in summary.attempts.iter().enumerate() {
        safe_line(
            &mut text,
            format_args!("Attempt {} session {}", index + 1, attempt.session_id),
        );
        safe_line(
            &mut text,
            format_args!("  Event count: {}", attempt.event_count),
        );
        safe_line(
            &mut text,
            format_args!(
                "  Monotonic elapsed span: {}..{} ms ({} ms within this attempt)",
                attempt.first_millis,
                attempt.last_millis,
                attempt.last_millis.saturating_sub(attempt.first_millis)
            ),
        );
        let wall_clock = match (&attempt.started_at_utc, &attempt.ended_at_utc) {
            (Some(start), Some(end)) => format!("{start}..{end}"),
            _ => "unavailable".to_owned(),
        };
        safe_line(
            &mut text,
            format_args!("  Contextual wall-clock span: {wall_clock}"),
        );
    }
    safe_line(
        &mut text,
        format_args!(
            "Wall-clock values, when present, are contextual only and never trusted elapsed duration."
        ),
    );
    safe_line(
        &mut text,
        format_args!("Inter-attempt time is unknown; no continuous clock joins attempts."),
    );
    safe_line(
        &mut text,
        format_args!(
            "Revised bundles include all prior recorded attempts from the original starter."
        ),
    );

    let totals = &summary.totals;
    safe_line(&mut text, format_args!("Aggregate recorded categories:"));
    safe_line(
        &mut text,
        format_args!("Initial workspace logical files: {}", totals.initial_files),
    );
    safe_line(
        &mut text,
        format_args!(
            "Initial workspace logical bytes: {}",
            totals.initial_file_bytes
        ),
    );
    safe_line(
        &mut text,
        format_args!(
            "Initial workspace stored blobs: {} entries, {} bytes",
            totals.initial_blobs, totals.initial_blob_bytes
        ),
    );
    safe_line(
        &mut text,
        format_args!("Event records: {}", totals.event_records),
    );
    safe_line(
        &mut text,
        format_args!("Event stream bytes: {}", totals.event_stream_bytes),
    );
    safe_line(&mut text, format_args!("Events by kind:"));
    for (kind, count) in EVENT_KIND_NAMES.iter().zip(totals.event_counts) {
        safe_line(&mut text, format_args!("  {kind}: {count}"));
    }
    safe_line(
        &mut text,
        format_args!("Edit transactions: {}", totals.edit_transactions),
    );
    safe_line(
        &mut text,
        format_args!("Inserted Unicode scalars: {}", totals.inserted_scalars),
    );
    safe_line(
        &mut text,
        format_args!("Inserted text bytes: {}", totals.inserted_bytes),
    );
    safe_line(
        &mut text,
        format_args!("Deleted Unicode scalars: {}", totals.deleted_scalars),
    );
    safe_line(
        &mut text,
        format_args!("Deleted text bytes: {}", totals.deleted_bytes),
    );
    safe_line(
        &mut text,
        format_args!(
            "Command lifecycle/output records: {}",
            totals.command_records
        ),
    );
    safe_line(
        &mut text,
        format_args!(
            "Command argv/environment records: {}",
            totals.command_starts
        ),
    );
    safe_line(
        &mut text,
        format_args!("Command output records: {}", totals.command_output_records),
    );
    safe_line(
        &mut text,
        format_args!("Command output bytes: {}", totals.command_output_bytes),
    );
    safe_line(
        &mut text,
        format_args!("Diagnostic records: {}", totals.diagnostics),
    );
    safe_line(
        &mut text,
        format_args!("Diagnostic message/code bytes: {}", totals.diagnostic_bytes),
    );
    safe_line(
        &mut text,
        format_args!(
            "External-change observations: {}",
            totals.external_observations
        ),
    );
    safe_line(
        &mut text,
        format_args!(
            "External-change evidence: {} entries, {} bytes",
            totals.evidence, totals.evidence_bytes
        ),
    );
    safe_line(
        &mut text,
        format_args!("Blocked-paste metadata records: {}", totals.blocked_pastes),
    );
    safe_line(
        &mut text,
        format_args!(
            "Blocked-paste encoded metadata bytes: {}",
            totals.blocked_paste_bytes
        ),
    );
    safe_line(
        &mut text,
        format_args!(
            "Checkpoints: {} entries, {} bytes",
            totals.checkpoints, totals.checkpoint_bytes
        ),
    );
    safe_line(
        &mut text,
        format_args!(
            "Runtime metadata: {} entries, {} bytes",
            totals.metadata, totals.metadata_bytes
        ),
    );
    if summary.preview {
        safe_line(
            &mut text,
            format_args!(
                "Preview only: this session is unfinished; finalization will add a final checkpoint and submission-finalized event."
            ),
        );
    }

    safe_line(&mut text, format_args!("Course privacy policy:"));
    for line in POLICY_LINES {
        safe_line(&mut text, format_args!("{line}"));
    }
    safe_line(
        &mut text,
        format_args!("Any future research use would require a separate, new consent process."),
    );
    safe_line(
        &mut text,
        format_args!(
            "Ongoing linked attempts require ancestor journals, checkpoints, evidence, captures, and receipts for future clean self-contained exports."
        ),
    );
    safe_line(
        &mut text,
        format_args!("Normal `rustrace cleanup WORKSPACE --confirm` preserves provenance."),
    );
    safe_line(
        &mut text,
        format_args!(
            "`rustrace cleanup WORKSPACE --destroy-provenance --confirm` destroys that workspace's local provenance and can prevent future clean self-contained exports for linked attempts."
        ),
    );
    safe_line(
        &mut text,
        format_args!("Deleting workspace and bundle files removes those local copies."),
    );
    safe_line(
        &mut text,
        format_args!("Rustrace cannot verify deletion from the LMS, staff machines, or backups."),
    );
    output.write_all(text.as_bytes())?;
    Ok(())
}

fn safe_line(target: &mut String, value: std::fmt::Arguments<'_>) {
    let safe = display::label_fmt(value, 4096);
    writeln!(target, "{safe}").expect("writing to a String cannot fail");
}
