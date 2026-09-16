//! Local export history, session status, revision creation, and explicit cleanup.

use super::{
    BundleOutput, FinalizationReceipt, FinalizationStatus, IncompleteFinalization, METADATA_LIMIT,
    ProductionSession, ReadOnlyFinalizationReceipt, ReadOnlyFinalizationStatus, Result,
};
use crate::{display, work};
use rustrace_model::{
    Hash, MAX_RPROV_SEGMENTS, RprovInitialWorkspace, RprovInventoryEntry, SessionId, WorkspacePath,
};
use rustrace_workspace::hash::PinnedWorkspaceRoot;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const EXPORT_RECORDS: &str = "export-records.json";
const EXPORT_RECORDS_LIMIT: usize = 1024 * 1024;
const MAX_EXPORT_RECORDS: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportRecord {
    bundle_path: PathBuf,
    bundle_blake3: Hash,
    exported_at_unix_millis: u64,
    timestamp_context: String,
    #[serde(default)]
    incomplete: bool,
    final_tree_hash: Option<Hash>,
    terminal_chain_hash: Option<Hash>,
    ancestry_session_ids: Vec<SessionId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportRecordLog {
    version: u32,
    records: Vec<ExportRecord>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisionLinkStatus {
    version: u32,
    kind: String,
    parent_root: PathBuf,
    parent_session_id: SessionId,
    #[serde(rename = "parent_terminal_event_hash")]
    _parent_terminal_event_hash: Hash,
    #[serde(rename = "parent_final_tree_hash")]
    _parent_final_tree_hash: Hash,
    #[serde(rename = "parent_manifest_blake3")]
    _parent_manifest_blake3: Hash,
    #[serde(rename = "parent_sources_blake3")]
    _parent_sources_blake3: Hash,
    #[serde(default, rename = "original_starter_tree_hash")]
    _original_starter_tree_hash: Option<Hash>,
    #[serde(default, rename = "initial_workspace")]
    _initial_workspace: Option<RprovInitialWorkspace>,
    #[serde(default, rename = "initial_inventory")]
    _initial_inventory: Vec<RprovInventoryEntry>,
}

#[derive(Clone, Debug)]
struct LinkedAttempt {
    path: PathBuf,
    session_id: SessionId,
}

#[derive(Clone, Debug)]
enum CleanupKind {
    File,
    Directory,
}

#[derive(Clone, Debug)]
struct CleanupCandidate {
    path: PathBuf,
    kind: CleanupKind,
}

pub(super) fn record_successful_export(
    receipt: &FinalizationReceipt,
    bundle: &BundleOutput,
) -> Result<()> {
    if hash_regular_file(&bundle.path)? != bundle.blake3 {
        return Err("published bundle bytes differ from the bundle hash".into());
    }
    let (final_tree_hash, terminal_chain_hash, ancestry_session_ids) = receipt_facts(receipt)?;
    let metadata = ProductionSession::read_metadata(receipt.workspace_root())?;
    if metadata.session_id != receipt.manifest().latest_session_id {
        return Err("export workspace session differs from the finalization receipt".into());
    }
    let exported_at_unix_millis =
        u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
    let record = ExportRecord {
        bundle_path: bundle.path.clone(),
        bundle_blake3: bundle.blake3,
        exported_at_unix_millis,
        timestamp_context:
            "contextual wall clock for local artifact export; not trusted elapsed time or LMS hand-in"
                .to_owned(),
        incomplete: false,
        final_tree_hash: Some(final_tree_hash),
        terminal_chain_hash: Some(terminal_chain_hash),
        ancestry_session_ids,
    };

    let pinned = PinnedWorkspaceRoot::open(receipt.workspace_root())?;
    let mut owner = pinned
        .open_state_directory()?
        .open_journal_file(&metadata.session_id)?;
    let result = (|| -> Result<()> {
        let mut log = read_export_log(&owner, receipt)?;
        append_export_record(&owner, &mut log, record)
    })();
    owner.release_ownership()?;
    result
}

pub(super) fn record_incomplete_export(
    workspace: &Path,
    incomplete: &IncompleteFinalization,
    bundle: &BundleOutput,
) -> Result<()> {
    if hash_regular_file(&bundle.path)? != bundle.blake3 {
        return Err("published incomplete bundle bytes differ from the bundle hash".into());
    }
    let manifest = incomplete
        .manifest()
        .ok_or("incomplete export has no valid recovery manifest")?;
    manifest.validate()?;
    if !matches!(
        manifest.package_state,
        rustrace_model::RprovPackageState::RecoveryIncomplete { .. }
    ) || manifest.latest_session_id != incomplete.session_id
    {
        return Err("incomplete export manifest has conflicting recovery identity".into());
    }
    let metadata = ProductionSession::read_metadata(workspace)?;
    if metadata.session_id != incomplete.session_id {
        return Err("incomplete export workspace differs from its capture".into());
    }
    let ancestry_session_ids = manifest
        .segments
        .iter()
        .map(|segment| segment.session_id.clone())
        .collect::<Vec<_>>();
    let record = ExportRecord {
        bundle_path: bundle.path.clone(),
        bundle_blake3: bundle.blake3,
        exported_at_unix_millis: u64::try_from(
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
        )?,
        timestamp_context:
            "contextual wall clock for local incomplete recovery export; not trusted elapsed time or LMS hand-in"
                .to_owned(),
        incomplete: true,
        final_tree_hash: None,
        terminal_chain_hash: None,
        ancestry_session_ids: ancestry_session_ids.clone(),
    };

    let pinned = PinnedWorkspaceRoot::open(workspace)?;
    let mut owner = pinned
        .open_state_directory()?
        .open_journal_file(&metadata.session_id)?;
    let result = (|| -> Result<()> {
        let mut log = read_export_log_unvalidated(&owner)?;
        if log.records.iter().any(|record| {
            !record.incomplete
                || record.final_tree_hash.is_some()
                || record.terminal_chain_hash.is_some()
                || record.ancestry_session_ids != ancestry_session_ids
        }) {
            return Err(
                "invalid .rustrace/export-records.json: records differ from the incomplete recovery capture"
                    .into(),
            );
        }
        append_export_record(&owner, &mut log, record)
    })();
    owner.release_ownership()?;
    result
}

pub fn run_status(args: &[String], output: &mut impl Write) -> Result<()> {
    let [workspace] = args else {
        return Err("Usage: rustrace status WORKSPACE".into());
    };
    let root = canonical_workspace(Path::new(workspace))?;
    let safe_root = safe_path(&root);
    let metadata = match ProductionSession::read_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) => {
            writeln!(output, "INCOMPLETE RECOVERY at {safe_root}")?;
            writeln!(
                output,
                "Session identity unavailable; preserved state requires inspection: {}",
                display::label_fmt(format_args!("{error}"), 4096)
            )?;
            return Ok(());
        }
    };
    let revision = read_revision_link(&root, metadata.parent_evidence.as_ref())?;
    match ProductionSession::inspect_finalization_read_only(&root)? {
        ReadOnlyFinalizationStatus::Unfinished => {
            writeln!(
                output,
                "UNFINISHED session {} at {safe_root}",
                metadata.session_id
            )?;
            writeln!(
                output,
                "This attempt remains mutable; its journal, checkpoints, evidence, and captures are preserved."
            )?;
            write_revision_status(output, &metadata.session_id, revision.as_ref(), None)?;
        }
        ReadOnlyFinalizationStatus::Prepared { session_id } => {
            writeln!(
                output,
                "FINALIZATION PREPARED session {session_id} at {safe_root}"
            )?;
            writeln!(
                output,
                "finalization prepared, not completed; run `rustrace submit` to complete or inspect"
            )?;
            writeln!(output, "All captured provenance remains preserved.")?;
            write_revision_status(output, &metadata.session_id, revision.as_ref(), None)?;
        }
        ReadOnlyFinalizationStatus::Incomplete {
            reason,
            session_id,
            capture_available,
        } => {
            writeln!(
                output,
                "INCOMPLETE RECOVERY session {} at {safe_root}",
                session_id
            )?;
            writeln!(output, "Reason: {}", display::label(&reason, 4096))?;
            writeln!(output, "Immutable capture available: {capture_available}")?;
            writeln!(output, "All available provenance remains preserved.")?;
            write_revision_status(output, &metadata.session_id, revision.as_ref(), None)?;
        }
        ReadOnlyFinalizationStatus::Finalized(receipt) => {
            writeln!(
                output,
                "FINALIZED IMMUTABLE SNAPSHOT session {} at {safe_root}",
                receipt.latest_session_id
            )?;
            writeln!(output, "Student ID: {}", receipt.student_id)?;
            writeln!(output, "Final tree hash: {}", receipt.final_tree_hash)?;
            writeln!(
                output,
                "Terminal chain hash: {}",
                receipt.terminal_chain_hash
            )?;
            writeln!(
                output,
                "Aggregate recorded events: {}",
                receipt.aggregate_event_count
            )?;
            writeln!(
                output,
                "This snapshot cannot accept later work; use `rustrace revise` for a new linked attempt."
            )?;
            writeln!(
                output,
                "Finalized/exported describes a local artifact only and does not mean a successful LMS hand-in."
            )?;
            write_ancestry(output, &receipt.ancestry_session_ids)?;
            let records = load_export_records_read_only(&root, &receipt)?;
            writeln!(output, "Local export records: {}", records.len())?;
            for record in records {
                writeln!(
                    output,
                    "  {}  {}  {} (contextual wall clock; local artifact only)",
                    record.exported_at_unix_millis,
                    record.bundle_blake3,
                    safe_path(&record.bundle_path)
                )?;
            }
            write_revision_status(
                output,
                &metadata.session_id,
                revision.as_ref(),
                Some(&receipt.ancestry_session_ids),
            )?;
        }
    }
    Ok(())
}

pub fn run_revise(args: &[String], output: &mut impl Write) -> Result<()> {
    let [parent, new_workspace, package] = args else {
        return Err("Usage: rustrace revise PARENT_WORKSPACE NEW_WORKSPACE assignment.rta".into());
    };
    let parent = canonical_workspace(Path::new(parent))?;
    let new_workspace = absent_workspace_path(Path::new(new_workspace))?;
    let receipt = match ProductionSession::recover_finalization_if_started(&parent)? {
        Some(FinalizationStatus::Finalized(receipt)) => receipt,
        Some(FinalizationStatus::Incomplete(_)) => {
            return Err(
                "parent attempt is incomplete; preserve and inspect it before revision".into(),
            );
        }
        None => return Err("parent attempt is unfinished and cannot seed a revision".into()),
    };

    let validation = work::unique_sibling(&new_workspace, "revision-package")?;
    let extracted = work::extract_package(Path::new(package), &validation);
    let cleanup = if validation.try_exists()? {
        fs::remove_dir_all(&validation)
    } else {
        Ok(())
    };
    let extracted = extracted?;
    cleanup?;
    let assignment = &receipt.manifest().assignment_manifest;
    if extracted.manifest_bytes.len() as u64 != assignment.byte_length
        || Hash::from_bytes(*blake3::hash(&extracted.manifest_bytes).as_bytes())
            != assignment.blake3
        || extracted.test_cases.as_ref().map(|suite| suite.hash)
            != receipt.manifest().test_case_suite_hash
    {
        return Err("revision assignment package differs from the finalized parent".into());
    }

    materialize_revision(&new_workspace, receipt.final_workspace())?;
    let session =
        ProductionSession::start_revision(&parent, &new_workspace, &extracted.manifest_bytes)?;
    let child_id = session.session_id().clone();
    let parent_id = receipt.manifest().latest_session_id.clone();
    session.quit()?;
    writeln!(
        output,
        "Started linked revision {child_id} at {}; immutable parent {parent_id} remains unchanged at {}.",
        safe_path(&new_workspace),
        safe_path(&parent)
    )?;
    Ok(())
}

pub fn run_cleanup(args: &[String], output: &mut impl Write) -> Result<()> {
    let Some(workspace) = args.first() else {
        return Err("Usage: rustrace cleanup WORKSPACE [--confirm] [--destroy-provenance]".into());
    };
    let mut confirm = false;
    let mut destroy = false;
    for option in &args[1..] {
        match option.as_str() {
            "--confirm" if !confirm => confirm = true,
            "--destroy-provenance" if !destroy => destroy = true,
            _ => return Err("invalid or duplicate cleanup option".into()),
        }
    }
    let root = canonical_workspace(Path::new(workspace))?;
    require_state_directory(&root)?;
    let candidates = cleanup_candidates(&root)?;
    writeln!(
        output,
        "{} cleanup for {}",
        if confirm { "CONFIRMED" } else { "DRY RUN" },
        safe_path(&root)
    )?;
    if candidates.is_empty() {
        writeln!(output, "REMOVE: nothing from the normal cleanup allowlist")?;
    } else {
        for candidate in &candidates {
            writeln!(
                output,
                "{}REMOVE: {}",
                if candidate.path.starts_with(&root) {
                    ""
                } else {
                    "OUTSIDE ROOT "
                },
                safe_path(&candidate.path)
            )?;
        }
    }
    if destroy {
        writeln!(
            output,
            "WOULD REMOVE: all journals, checkpoints, evidence, prepared captures, finalization receipts, and export records under {}",
            safe_path(&root.join(".rustrace"))
        )?;
    } else {
        writeln!(
            output,
            "PRESERVE: managed source plus all journals, checkpoints, evidence, prepared captures, finalization receipts, and export records under {}",
            safe_path(&root.join(".rustrace"))
        )?;
    }
    writeln!(
        output,
        "PRESERVE: every path not explicitly listed for removal, including active or unrecognized temporary files."
    )?;

    let (affected, unknown, session_id) = if destroy {
        let session_id = ProductionSession::read_metadata(&root)?.session_id;
        let result = discover_linked_attempts(&root, &session_id)?;
        writeln!(
            output,
            "WARNING: destroying provenance for session {} can prevent future clean self-contained exports unless complete validated evidence remains available.",
            session_id
        )?;
        writeln!(
            output,
            "Linked-attempt search scope: sibling workspaces under {}; attempts stored elsewhere cannot be discovered automatically.",
            safe_path(root.parent().ok_or("workspace has no parent")?)
        )?;
        if result.0.is_empty() {
            writeln!(output, "Affected linked attempts: none discovered")?;
        } else {
            writeln!(output, "Affected linked attempts:")?;
            for attempt in &result.0 {
                writeln!(
                    output,
                    "  {} at {}",
                    attempt.session_id,
                    safe_path(&attempt.path)
                )?;
            }
        }
        for (path, reason) in &result.1 {
            writeln!(
                output,
                "UNKNOWN OR UNREACHABLE linked attempt at {}: {}",
                safe_path(path),
                display::label(reason, 4096)
            )?;
        }
        (result.0, result.1, Some(session_id))
    } else {
        (Vec::new(), Vec::new(), None)
    };

    if !confirm {
        if destroy {
            return Err(
                "destructive provenance cleanup requires --destroy-provenance --confirm".into(),
            );
        }
        return Ok(());
    }
    if destroy && !unknown.is_empty() {
        return Err(
            "provenance was preserved because not every linked attempt could be identified".into(),
        );
    }
    for candidate in candidates {
        match candidate.kind {
            CleanupKind::File => fs::remove_file(candidate.path)?,
            CleanupKind::Directory => fs::remove_dir_all(candidate.path)?,
        }
    }
    if destroy {
        let session_id = session_id.expect("destructive cleanup loaded the session identity");
        let state = root.join(".rustrace");
        let state_metadata = fs::symlink_metadata(&state)?;
        if !state_metadata.is_dir() || state_metadata.file_type().is_symlink() {
            return Err("provenance path is not the expected directory; preserved".into());
        }
        fs::remove_dir_all(&state)?;
        writeln!(
            output,
            "Destroyed provenance for session {}; {} affected linked attempt(s) were named above. Source files remain.",
            session_id,
            affected.len()
        )?;
    }
    Ok(())
}

fn receipt_facts(receipt: &FinalizationReceipt) -> Result<(Hash, Hash, Vec<SessionId>)> {
    let final_tree = *receipt
        .manifest()
        .final_tree_hash
        .known()
        .ok_or("finalized receipt has no known final tree hash")?;
    let terminal = receipt
        .manifest()
        .segments
        .last()
        .ok_or("finalized receipt has no terminal segment")?
        .last_event_hash;
    let ancestry = receipt
        .manifest()
        .segments
        .iter()
        .map(|segment| segment.session_id.clone())
        .collect::<Vec<_>>();
    Ok((final_tree, terminal, ancestry))
}

fn read_export_log(
    owner: &rustrace_workspace::hash::PinnedJournalFile,
    receipt: &FinalizationReceipt,
) -> Result<ExportRecordLog> {
    let log = read_export_log_unvalidated(owner)?;
    let (tree, terminal, ancestry) = receipt_facts(receipt)?;
    validate_export_log(log, tree, terminal, &ancestry)
}

fn read_export_log_unvalidated(
    owner: &rustrace_workspace::hash::PinnedJournalFile,
) -> Result<ExportRecordLog> {
    let state = owner
        .display_path()
        .parent()
        .ok_or("state directory missing")?;
    let log = match fs::symlink_metadata(state.join(EXPORT_RECORDS)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ExportRecordLog {
            version: 1,
            records: Vec::new(),
        },
        Ok(_) => {
            serde_json::from_slice(&owner.read_artifact(EXPORT_RECORDS, EXPORT_RECORDS_LIMIT)?)?
        }
        Err(error) => return Err(error.into()),
    };
    if log.version != 1 || log.records.len() > MAX_EXPORT_RECORDS {
        return Err(
            "invalid .rustrace/export-records.json: unsupported version or record limit".into(),
        );
    }
    Ok(log)
}

fn append_export_record(
    owner: &rustrace_workspace::hash::PinnedJournalFile,
    log: &mut ExportRecordLog,
    record: ExportRecord,
) -> Result<()> {
    if log.records.len() >= MAX_EXPORT_RECORDS {
        return Err("export record limit reached; provenance preserved".into());
    }
    log.records.push(record);
    let bytes = serde_json::to_vec(log)?;
    if bytes.len() > EXPORT_RECORDS_LIMIT {
        return Err("export records exceed their bounded local metadata limit".into());
    }
    owner.publish_artifact(EXPORT_RECORDS, &bytes, true)?;
    Ok(())
}

fn validate_export_log(
    log: ExportRecordLog,
    tree: Hash,
    terminal: Hash,
    ancestry: &[SessionId],
) -> Result<ExportRecordLog> {
    if log.version != 1 || log.records.len() > MAX_EXPORT_RECORDS {
        return Err(
            "invalid .rustrace/export-records.json: unsupported version or record limit".into(),
        );
    }
    if log.records.iter().any(|record| {
        record.incomplete
            || record.final_tree_hash != Some(tree)
            || record.terminal_chain_hash != Some(terminal)
            || record.ancestry_session_ids != ancestry
    }) {
        return Err(
            "invalid .rustrace/export-records.json: records differ from the immutable finalization receipt"
                .into(),
        );
    }
    Ok(log)
}

fn load_export_records_read_only(
    root: &Path,
    receipt: &ReadOnlyFinalizationReceipt,
) -> Result<Vec<ExportRecord>> {
    let pinned = PinnedWorkspaceRoot::open(root)?;
    let state = pinned.open_existing_state_directory()?;
    let log = if state.artifact_exists(EXPORT_RECORDS)? {
        let bytes = state
            .read_artifact(EXPORT_RECORDS, EXPORT_RECORDS_LIMIT)
            .map_err(|error| format!("invalid .rustrace/export-records.json: {error}"))?;
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid .rustrace/export-records.json: {error}"))?
    } else {
        ExportRecordLog {
            version: 1,
            records: Vec::new(),
        }
    };
    validate_export_log(
        log,
        receipt.final_tree_hash,
        receipt.terminal_chain_hash,
        &receipt.ancestry_session_ids,
    )
    .map(|log| log.records)
}

fn write_revision_status(
    output: &mut impl Write,
    current: &SessionId,
    revision: Option<&RevisionLinkStatus>,
    finalized_ancestry: Option<&[SessionId]>,
) -> Result<()> {
    let Some(revision) = revision.filter(|link| link.kind == "finalized_revision") else {
        return Ok(());
    };
    writeln!(
        output,
        "LINKED REVISION: immediate parent {} at {}",
        revision.parent_session_id,
        safe_path(&revision.parent_root)
    )?;
    if finalized_ancestry.is_some() {
        return Ok(());
    }
    match ProductionSession::inspect_finalization_read_only(&revision.parent_root) {
        Ok(ReadOnlyFinalizationStatus::Finalized(parent)) => {
            let mut ancestry = parent.ancestry_session_ids;
            ancestry.push(current.clone());
            write_ancestry(output, &ancestry)?;
        }
        Ok(_) => writeln!(
            output,
            "Ordered parent ancestry is unavailable because the linked parent is not finalized."
        )?,
        Err(error) => writeln!(
            output,
            "Ordered parent ancestry is unreachable: {}",
            display::label_fmt(format_args!("{error}"), 4096)
        )?,
    }
    Ok(())
}

fn write_ancestry(output: &mut impl Write, ancestry: &[SessionId]) -> Result<()> {
    writeln!(output, "Ordered attempt ancestry:")?;
    for session_id in ancestry {
        writeln!(output, "  {session_id}")?;
    }
    Ok(())
}

fn read_revision_link(
    root: &Path,
    expected_digest: Option<&Hash>,
) -> Result<Option<RevisionLinkStatus>> {
    let pinned = PinnedWorkspaceRoot::open(root)?;
    let state = pinned.open_existing_state_directory()?;
    if !state.artifact_exists("parent.json")? {
        return Ok(None);
    }
    let bytes = state.read_artifact("parent.json", METADATA_LIMIT)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if value.get("kind").and_then(serde_json::Value::as_str) != Some("finalized_revision") {
        return Ok(None);
    }
    let link: RevisionLinkStatus = serde_json::from_value(value)?;
    if link.version != 1 || link.kind != "finalized_revision" {
        return Err("invalid finalized revision link".into());
    }
    if expected_digest != Some(&super::digest(&bytes)) {
        return Err("finalized revision link differs from session metadata".into());
    }
    Ok(Some(link))
}

fn require_state_directory(root: &Path) -> Result<()> {
    let state = root.join(".rustrace");
    match fs::symlink_metadata(&state) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err("cleanup requires an owned .rustrace directory; nothing was removed".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("cleanup requires an owned .rustrace directory; nothing was removed".into())
        }
        Err(error) => Err(error.into()),
    }
}

fn cleanup_candidates(root: &Path) -> Result<Vec<CleanupCandidate>> {
    let mut candidates = Vec::new();
    let target = root.join("target");
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            candidates.push(CleanupCandidate {
                path: target,
                kind: CleanupKind::Directory,
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut directories = BTreeSet::new();
    directories.insert(root.to_path_buf());
    if let Some(parent) = root.parent() {
        directories.insert(parent.to_path_buf());
    }
    for directory in directories {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !is_stale_submit_temporary(name) {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_file() && !metadata.file_type().is_symlink() {
                candidates.push(CleanupCandidate {
                    path: entry.path(),
                    kind: CleanupKind::File,
                });
            }
        }
    }
    candidates.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(candidates)
}

pub(super) fn submit_temporary_process(name: &str) -> Option<u32> {
    let body = name
        .strip_prefix(".rustrace-submit-")
        .and_then(|name| name.strip_suffix(".tmp"))?;
    let mut fields = body.split('-');
    let process = fields.next()?;
    let counter = fields.next()?;
    let tag = fields.next();
    if process.is_empty()
        || counter.is_empty()
        || !process.bytes().all(|byte| byte.is_ascii_digit())
        || !counter.bytes().all(|byte| byte.is_ascii_digit())
        || fields.next().is_some()
        || tag
            .is_some_and(|tag| tag.len() != 64 || !tag.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return None;
    }
    process.parse().ok()
}

pub(super) fn is_stale_submit_temporary(name: &str) -> bool {
    let Some(process) = submit_temporary_process(name) else {
        return false;
    };
    if process == std::process::id() {
        return false;
    }
    #[cfg(unix)]
    {
        let Ok(process) = i32::try_from(process) else {
            return false;
        };
        // Signal zero does not deliver a signal; it checks whether the PID is
        // still reachable. EPERM is conservatively treated as live/unknown.
        (unsafe { libc::kill(process, 0) }) != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

type UnknownAttempt = (PathBuf, String);

fn discover_linked_attempts(
    root: &Path,
    target: &SessionId,
) -> Result<(Vec<LinkedAttempt>, Vec<UnknownAttempt>)> {
    let parent = root.parent().ok_or("workspace has no parent directory")?;
    let mut affected = Vec::new();
    let mut unknown = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let candidate = match fs::canonicalize(&path) {
            Ok(candidate) => candidate,
            Err(error) => {
                unknown.push((path, error.to_string()));
                continue;
            }
        };
        if candidate == root {
            continue;
        }
        match fs::symlink_metadata(candidate.join(".rustrace/parent.json")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Ok(_) => {}
            Err(error) => {
                unknown.push((candidate, error.to_string()));
                continue;
            }
        }
        let candidate_metadata = match ProductionSession::read_metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) => {
                unknown.push((candidate, error.to_string()));
                continue;
            }
        };
        let link = match read_revision_link(&candidate, candidate_metadata.parent_evidence.as_ref())
        {
            Ok(Some(link)) => link,
            Ok(None) => continue,
            Err(error) => {
                unknown.push((candidate, error.to_string()));
                continue;
            }
        };
        match link_reaches(&link, target) {
            Ok(true) => {
                affected.push(LinkedAttempt {
                    path: candidate,
                    session_id: candidate_metadata.session_id,
                });
            }
            Ok(false) => {}
            Err(error) => unknown.push((candidate, error.to_string())),
        }
    }
    affected.sort_by(|left, right| left.path.cmp(&right.path));
    unknown.sort_by(|left, right| left.0.cmp(&right.0));
    Ok((affected, unknown))
}

fn link_reaches(initial: &RevisionLinkStatus, target: &SessionId) -> Result<bool> {
    let mut link = initial.clone();
    let mut visited = BTreeSet::new();
    for _ in 0..MAX_RPROV_SEGMENTS {
        if &link.parent_session_id == target {
            return Ok(true);
        }
        let parent = fs::canonicalize(&link.parent_root)
            .map_err(|error| format!("linked parent is unreachable: {error}"))?;
        if !visited.insert(parent.clone()) {
            return Err("linked attempt ancestry contains a cycle".into());
        }
        let metadata = ProductionSession::read_metadata(&parent)
            .map_err(|error| format!("linked parent metadata is unreachable: {error}"))?;
        if metadata.session_id != link.parent_session_id {
            return Err("linked parent path/session identity mismatch".into());
        }
        let Some(parent_link) = read_revision_link(&parent, metadata.parent_evidence.as_ref())?
        else {
            return Ok(false);
        };
        link = parent_link;
    }
    Err("linked attempt ancestry exceeds the supported segment limit".into())
}

fn materialize_revision(root: &Path, files: &BTreeMap<WorkspacePath, Vec<u8>>) -> Result<()> {
    fs::create_dir(root)?;
    let result = (|| -> Result<()> {
        for (path, bytes) in files {
            let destination = root.join(path.as_str());
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?
                .write_all(bytes)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(root);
    }
    result
}

fn canonical_workspace(path: &Path) -> Result<PathBuf> {
    let input_metadata = fs::symlink_metadata(path)?;
    if input_metadata.file_type().is_symlink() {
        return Err("workspace path must not be a symlink".into());
    }
    let root = fs::canonicalize(path)?;
    let metadata = fs::symlink_metadata(&root)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("workspace must be an existing non-symlink directory".into());
    }
    Ok(root)
}

fn absent_workspace_path(path: &Path) -> Result<PathBuf> {
    if path.try_exists()? {
        return Err("new revision workspace already exists; original preserved".into());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent)?;
    let name = path
        .file_name()
        .ok_or("new revision workspace must name a directory")?;
    Ok(parent.join(name))
}

fn hash_regular_file(path: &Path) -> Result<Hash> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err("published bundle is not a regular file".into());
    }
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(Hash::from_bytes(*hasher.finalize().as_bytes()));
        }
        hasher.update(&buffer[..read]);
    }
}

fn safe_path(path: &Path) -> String {
    display::label_fmt(format_args!("{}", path.display()), 4096)
}
