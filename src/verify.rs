//! Headless semantic verification of hostile `.rprov` and LMS ZIP inputs.

use crate::{
    console::{TestCase, TestCaseOutcome, compare_test_case_bytes},
    process_indicators::{AttemptIndicatorAccumulator, ReviewIndicators, merge_factual},
    review_flags::{
        AdvisoryFlag, EVIDENCE_LIMITATION_FIRST, EVIDENCE_LIMITATION_SECOND,
        TypingShapeAccumulator, display_advisory, evidence_statement,
    },
    session::hash_imported_outer_source,
    toolchain::RuntimeToolchainMetadata,
};
use rustrace_journal::{CheckpointSnapshot, StoredCheckpoint, decode_checkpoint};
use rustrace_model::{
    DecodeOutcome, DecodePolicy, Event, EventEnvelope, Hash, MAX_ENVELOPE_BYTES,
    RprovCheckpointRef, RprovEntryKind, RprovEventIntegrityKind, RprovPackageState, RprovSegment,
    WorkspacePath,
    assignment::{AssignmentManifest, MAX_MANIFEST_BYTES},
    decode_envelope, rprov_raw_blake3,
};
use rustrace_replay::ReplayEngine;
use rustrace_workspace::{
    assignment_package::{ExtractionLimits, extract_assignment_package},
    hash::{hash_entries, hash_workspace},
    rprov_import::{
        ImportedPackageKind, ImportedRprov, ImportedRprovIssue, RprovImportError,
        import_rprov_for_review,
    },
};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationStatus {
    Ok,
    Failed,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmittedSourceStatus {
    Ok,
    SourceMismatch,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssignmentReferenceStatus {
    Ok,
    Mismatch,
    Unverified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestCaseEvidenceStatus {
    Recorded,
    ReferenceVerified,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestCaseFailure {
    pub case: String,
    pub line: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationIssueKind {
    Input,
    PackageStructure,
    EventChain,
    EventSequence,
    CheckpointHashes,
    Replay,
    SubmittedSource,
    UnprovenancedExternalChange,
    AssignmentReference,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerificationIssueLocation {
    Event(VerificationEventLocation),
    Artifact(String),
    Decoder(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationIssue {
    pub kind: VerificationIssueKind,
    pub location: VerificationIssueLocation,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerificationEventLocation {
    pub segment: u32,
    pub sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationReport {
    pub student_id: Option<String>,
    pub assignment_id: Option<String>,
    pub package_structure: VerificationStatus,
    pub event_chain: VerificationStatus,
    pub checkpoint_hashes: VerificationStatus,
    pub replay: VerificationStatus,
    pub submitted_source_match: SubmittedSourceStatus,
    pub assignment_reference: AssignmentReferenceStatus,
    pub test_case_runs: Option<u64>,
    pub test_case_passes: Option<u64>,
    pub test_case_mismatches: Option<u64>,
    pub test_case_errors: Option<u64>,
    pub test_case_evidence: Option<TestCaseEvidenceStatus>,
    pub first_failing_case: Option<TestCaseFailure>,
    pub external_changes: Option<u64>,
    pub unknown_edit_origins: Option<u64>,
    pub allowed_internal_pastes: Option<u64>,
    pub allowed_internal_paste_characters: Option<u64>,
    pub historical_origin_unverified_pastes: Option<u64>,
    pub historical_origin_unverified_paste_characters: Option<u64>,
    pub rejected_paste_attempts: Option<u64>,
    pub first_external_change: Option<VerificationEventLocation>,
    pub first_unknown_edit_origin: Option<VerificationEventLocation>,
    pub first_allowed_internal_paste: Option<VerificationEventLocation>,
    pub first_historical_origin_unverified_paste: Option<VerificationEventLocation>,
    pub first_rejected_paste_attempt: Option<VerificationEventLocation>,
    pub review_indicators: Option<ReviewIndicators>,
    pub advisories: Vec<AdvisoryFlag>,
    pub issues: Vec<VerificationIssue>,
}

impl VerificationReport {
    fn unavailable() -> Self {
        Self {
            student_id: None,
            assignment_id: None,
            package_structure: VerificationStatus::Unavailable,
            event_chain: VerificationStatus::Unavailable,
            checkpoint_hashes: VerificationStatus::Unavailable,
            replay: VerificationStatus::Unavailable,
            submitted_source_match: SubmittedSourceStatus::Unavailable,
            assignment_reference: AssignmentReferenceStatus::Unverified,
            test_case_runs: None,
            test_case_passes: None,
            test_case_mismatches: None,
            test_case_errors: None,
            test_case_evidence: None,
            first_failing_case: None,
            external_changes: None,
            unknown_edit_origins: None,
            allowed_internal_pastes: None,
            allowed_internal_paste_characters: None,
            historical_origin_unverified_pastes: None,
            historical_origin_unverified_paste_characters: None,
            rejected_paste_attempts: None,
            first_external_change: None,
            first_unknown_edit_origin: None,
            first_allowed_internal_paste: None,
            first_historical_origin_unverified_paste: None,
            first_rejected_paste_attempt: None,
            review_indicators: None,
            advisories: Vec::new(),
            issues: Vec::new(),
        }
    }

    pub(crate) fn input_failure(detail: impl Into<String>) -> Self {
        let mut report = Self::unavailable();
        report.record_unavailable(
            VerificationIssueKind::Input,
            VerificationIssueLocation::Decoder("package input".to_owned()),
            detail,
        );
        report
    }

    fn fail(
        &mut self,
        kind: VerificationIssueKind,
        location: VerificationIssueLocation,
        detail: impl Into<String>,
    ) {
        match kind {
            VerificationIssueKind::Input => {
                self.package_structure = VerificationStatus::Unavailable;
            }
            VerificationIssueKind::PackageStructure => {
                self.package_structure = VerificationStatus::Failed;
            }
            VerificationIssueKind::EventChain | VerificationIssueKind::EventSequence => {
                self.event_chain = VerificationStatus::Failed;
            }
            VerificationIssueKind::CheckpointHashes => {
                self.checkpoint_hashes = VerificationStatus::Failed;
            }
            VerificationIssueKind::Replay => self.replay = VerificationStatus::Failed,
            VerificationIssueKind::SubmittedSource => {
                self.submitted_source_match = SubmittedSourceStatus::SourceMismatch;
            }
            VerificationIssueKind::UnprovenancedExternalChange => {
                self.package_structure = VerificationStatus::Failed;
            }
            VerificationIssueKind::AssignmentReference => {
                self.assignment_reference = AssignmentReferenceStatus::Mismatch;
            }
        }
        self.issues.push(VerificationIssue {
            kind,
            location,
            detail: detail.into(),
        });
    }

    fn record_unavailable(
        &mut self,
        kind: VerificationIssueKind,
        location: VerificationIssueLocation,
        detail: impl Into<String>,
    ) {
        match kind {
            VerificationIssueKind::Input => {
                self.package_structure = VerificationStatus::Unavailable;
            }
            VerificationIssueKind::SubmittedSource => {
                self.submitted_source_match = SubmittedSourceStatus::Unavailable;
            }
            VerificationIssueKind::AssignmentReference => {
                self.assignment_reference = AssignmentReferenceStatus::Unverified;
            }
            VerificationIssueKind::PackageStructure
            | VerificationIssueKind::EventChain
            | VerificationIssueKind::EventSequence
            | VerificationIssueKind::CheckpointHashes
            | VerificationIssueKind::Replay
            | VerificationIssueKind::UnprovenancedExternalChange => {
                unreachable!("validation rows cannot be availability diagnostics")
            }
        }
        self.issues.push(VerificationIssue {
            kind,
            location,
            detail: detail.into(),
        });
    }

    pub fn is_clean(&self) -> bool {
        self.package_structure == VerificationStatus::Ok
            && self.event_chain == VerificationStatus::Ok
            && self.checkpoint_hashes == VerificationStatus::Ok
            && self.replay == VerificationStatus::Ok
            && self.submitted_source_match != SubmittedSourceStatus::SourceMismatch
            && self.assignment_reference != AssignmentReferenceStatus::Mismatch
            && self.issues.is_empty()
    }

    pub fn exit_code(&self) -> u8 {
        if self.issues.iter().any(|issue| {
            issue.kind == VerificationIssueKind::Input
                || (issue.kind == VerificationIssueKind::AssignmentReference
                    && self.assignment_reference == AssignmentReferenceStatus::Unverified)
        }) {
            2
        } else {
            u8::from(!self.is_clean())
        }
    }
}

pub fn verify_path(path: &Path, reference: Option<&Path>) -> VerificationReport {
    validate_replay_input(path, reference).0
}

/// Imports and semantically validates a replay input once, retaining the
/// importer-owned spool only when every replay prerequisite is clean.
pub(crate) fn validate_replay_input(
    path: &Path,
    reference: Option<&Path>,
) -> (VerificationReport, Option<ImportedRprov>) {
    let mut report = VerificationReport::unavailable();
    let input = match open_regular_read_only(path) {
        Ok(input) => input,
        Err(error) => {
            report.record_unavailable(
                VerificationIssueKind::Input,
                VerificationIssueLocation::Decoder("package input".to_owned()),
                error,
            );
            return (report, None);
        }
    };
    let package = match import_rprov_for_review(BufReader::new(input)) {
        Ok(package) => package,
        Err(error) => {
            classify_import_failure(&mut report, error);
            return (report, None);
        }
    };

    report.student_id = Some(package.manifest().student_id.clone());
    report.assignment_id = Some(package.manifest().assignment_id.clone());
    report.package_structure = VerificationStatus::Ok;
    report.event_chain = VerificationStatus::Ok;
    record_imported_issues(&package, &mut report);
    let assignment_reference = compare_reference(&package, reference, &mut report);

    if report.event_chain == VerificationStatus::Failed {
        return (report, None);
    }

    if !matches!(
        package.manifest().package_state,
        RprovPackageState::CleanFinalized
    ) {
        report.fail(
            VerificationIssueKind::PackageStructure,
            VerificationIssueLocation::Artifact("manifest.json".to_owned()),
            "recovery-incomplete packages cannot pass clean verification",
        );
        return (report, None);
    }

    if let Err(error) = validate_runtime_metadata(&package) {
        report.fail(
            VerificationIssueKind::PackageStructure,
            VerificationIssueLocation::Artifact("runtime metadata".to_owned()),
            error,
        );
        return (report, None);
    }
    let starter = match read_original_starter(&package) {
        Ok(starter) => starter,
        Err(error) => {
            report.fail(
                VerificationIssueKind::PackageStructure,
                VerificationIssueLocation::Artifact("initial workspace".to_owned()),
                error,
            );
            return (report, None);
        }
    };

    let authenticated_reference = assignment_reference
        .as_ref()
        .filter(|_| report.assignment_reference == AssignmentReferenceStatus::Ok);
    match replay_segments(&package, &starter, authenticated_reference) {
        Ok(facts) => {
            report.checkpoint_hashes = VerificationStatus::Ok;
            report.replay = VerificationStatus::Ok;
            report.test_case_runs = Some(facts.test_case_runs);
            report.test_case_passes = Some(facts.test_case_passes);
            report.test_case_mismatches = Some(facts.test_case_mismatches);
            report.test_case_errors = Some(facts.test_case_errors);
            report.test_case_evidence = Some(TestCaseEvidenceStatus::Recorded);
            report.first_failing_case = facts.first_failing_case.clone();
            authenticate_test_case_reference(assignment_reference.as_ref(), &facts, &mut report);
            let indicators = facts.review_indicators;
            report.external_changes = Some(indicators.factual.rejected_external_change.attempts);
            report.unknown_edit_origins = Some(indicators.factual.unknown_origin_edit.transactions);
            report.allowed_internal_pastes =
                Some(indicators.factual.allowed_internal_paste.transactions);
            report.allowed_internal_paste_characters =
                Some(indicators.factual.allowed_internal_paste.inserted_scalars);
            report.historical_origin_unverified_pastes = Some(
                indicators
                    .factual
                    .historical_origin_unverified_paste
                    .transactions,
            );
            report.historical_origin_unverified_paste_characters = Some(
                indicators
                    .factual
                    .historical_origin_unverified_paste
                    .inserted_scalars,
            );
            report.rejected_paste_attempts = Some(indicators.factual.rejected_paste.attempts);
            report.first_external_change =
                first_location(indicators.factual.rejected_external_change.links.first());
            report.first_unknown_edit_origin =
                first_location(indicators.factual.unknown_origin_edit.links.first());
            report.first_allowed_internal_paste =
                first_location(indicators.factual.allowed_internal_paste.links.first());
            report.first_historical_origin_unverified_paste = first_location(
                indicators
                    .factual
                    .historical_origin_unverified_paste
                    .links
                    .first(),
            );
            report.first_rejected_paste_attempt =
                first_location(indicators.factual.rejected_paste.links.first());
            report.review_indicators = Some(indicators);
            report.submitted_source_match =
                match package.kind() {
                    ImportedPackageKind::StandaloneRprov => SubmittedSourceStatus::Unavailable,
                    ImportedPackageKind::LmsZip => compare_submitted_source(
                        &mut report,
                        facts.final_tree_hash,
                        hash_imported_outer_source(&package).map_err(|error| error.to_string()),
                        package.manifest().segments.last().map(|segment| {
                            VerificationEventLocation {
                                segment: segment.ordinal,
                                sequence: segment.inclusive_event_count,
                            }
                        }),
                    ),
                };
            report.advisories = facts.advisories;
        }
        Err(error) => report.fail(error.kind, error.location, error.detail),
    }
    if report.event_chain == VerificationStatus::Ok
        && report.checkpoint_hashes == VerificationStatus::Ok
        && report.replay == VerificationStatus::Ok
        && !report.issues.iter().any(|issue| {
            matches!(
                issue.kind,
                VerificationIssueKind::Input | VerificationIssueKind::PackageStructure
            )
        })
    {
        (report, Some(package))
    } else {
        (report, None)
    }
}

pub fn validate_assignment_reference(path: &Path) -> Result<(), String> {
    read_assignment_reference(path).map(|_| ())
}

fn compare_submitted_source(
    report: &mut VerificationReport,
    expected: Hash,
    actual: Result<Hash, String>,
    final_event: Option<VerificationEventLocation>,
) -> SubmittedSourceStatus {
    match actual {
        Ok(actual) if actual == expected => SubmittedSourceStatus::Ok,
        Ok(_) => {
            report.fail(
                VerificationIssueKind::SubmittedSource,
                final_event.map_or_else(
                    || VerificationIssueLocation::Decoder("outer submitted source tree".to_owned()),
                    VerificationIssueLocation::Event,
                ),
                "outer submitted source differs from replayed final tree",
            );
            SubmittedSourceStatus::SourceMismatch
        }
        Err(error) => {
            report.record_unavailable(
                VerificationIssueKind::SubmittedSource,
                VerificationIssueLocation::Decoder("outer submitted source tree".to_owned()),
                error,
            );
            SubmittedSourceStatus::Unavailable
        }
    }
}

fn record_imported_issues(package: &ImportedRprov, report: &mut VerificationReport) {
    for issue in package.issues() {
        match issue {
            ImportedRprovIssue::MissingExternalEvidence {
                segment,
                sequence,
                detail,
            } => report.fail(
                VerificationIssueKind::UnprovenancedExternalChange,
                VerificationIssueLocation::Event(VerificationEventLocation {
                    segment: *segment,
                    sequence: *sequence,
                }),
                detail,
            ),
            ImportedRprovIssue::ExternalEvidencePayloadDigest { entry } => report.fail(
                VerificationIssueKind::UnprovenancedExternalChange,
                VerificationIssueLocation::Artifact(entry.clone()),
                format!("invalid .rprov payload digest for {entry}"),
            ),
            ImportedRprovIssue::EventStreamValidation { error } => {
                let (kind, location, detail) = import_failure_issue(error.clone().into());
                report.fail(kind, location, detail);
            }
        }
    }
}

fn classify_import_failure(report: &mut VerificationReport, error: RprovImportError) {
    let (kind, location, detail) = import_failure_issue(error);
    report.package_structure = if matches!(
        kind,
        VerificationIssueKind::EventChain | VerificationIssueKind::EventSequence
    ) {
        VerificationStatus::Ok
    } else {
        VerificationStatus::Failed
    };
    report.fail(kind, location, detail);
}

fn import_failure_issue(
    error: RprovImportError,
) -> (VerificationIssueKind, VerificationIssueLocation, String) {
    let detail = error.to_string();
    let (kind, location) = match error {
        RprovImportError::Model(rustrace_model::RprovError::EventIntegrity {
            kind,
            segment,
            sequence,
            ..
        }) => {
            let kind = match kind {
                RprovEventIntegrityKind::Chain | RprovEventIntegrityKind::Identity => {
                    VerificationIssueKind::EventChain
                }
                RprovEventIntegrityKind::Sequence => VerificationIssueKind::EventSequence,
                RprovEventIntegrityKind::FinalTreeBinding => VerificationIssueKind::Replay,
                RprovEventIntegrityKind::MissingExternalEvidence => {
                    VerificationIssueKind::UnprovenancedExternalChange
                }
            };
            (
                kind,
                VerificationIssueLocation::Event(VerificationEventLocation { segment, sequence }),
            )
        }
        RprovImportError::PayloadDigest { entry, kind } => {
            let issue = match kind {
                RprovEntryKind::Events => VerificationIssueKind::EventChain,
                RprovEntryKind::Checkpoint => VerificationIssueKind::CheckpointHashes,
                RprovEntryKind::ExternalRecoveryEvidence => {
                    VerificationIssueKind::UnprovenancedExternalChange
                }
                RprovEntryKind::InitialWorkspaceBlob | RprovEntryKind::RuntimeMetadata => {
                    VerificationIssueKind::PackageStructure
                }
            };
            (issue, VerificationIssueLocation::Artifact(entry))
        }
        RprovImportError::InvalidRprov { field } if field.starts_with("segments.evidence") => (
            VerificationIssueKind::UnprovenancedExternalChange,
            VerificationIssueLocation::Decoder(field.to_owned()),
        ),
        RprovImportError::InvalidRprov { field } if is_event_field(field) => (
            VerificationIssueKind::EventChain,
            VerificationIssueLocation::Decoder(field.to_owned()),
        ),
        RprovImportError::Model(rustrace_model::RprovError::UnsupportedInnerVersion {
            layer: "event",
            ..
        }) => (
            VerificationIssueKind::EventChain,
            VerificationIssueLocation::Decoder("event format version".to_owned()),
        ),
        RprovImportError::UnsafeOuterPath => (
            VerificationIssueKind::PackageStructure,
            VerificationIssueLocation::Decoder("outer LMS ZIP path".to_owned()),
        ),
        _ => (
            VerificationIssueKind::PackageStructure,
            VerificationIssueLocation::Decoder("package structure".to_owned()),
        ),
    };
    (kind, location, detail)
}

fn is_event_field(field: &str) -> bool {
    field.starts_with("events.jsonl")
        || field == "segments.events"
        || field == "segments.inclusive_event_count"
        || field == "segments.last_event_hash"
}

fn validate_runtime_metadata(package: &ImportedRprov) -> Result<(), String> {
    for segment in &package.manifest().segments {
        for declaration in &segment.metadata {
            let bytes = read_entry_exact(package, &declaration.entry, declaration.byte_length)?;
            let metadata: RuntimeToolchainMetadata = serde_json::from_slice(&bytes)
                .map_err(|_| "runtime metadata does not match its accepted schema".to_owned())?;
            if metadata.version != declaration.format_version
                || metadata.session_id != segment.session_id
                || metadata.manifest_hash != segment.assignment_manifest_blake3
                || metadata.sequence != declaration.owner.sequence
                || metadata.event_hash != declaration.owner.event_hash
            {
                return Err("runtime metadata owner or assignment identity mismatch".to_owned());
            }
        }
    }
    Ok(())
}

fn read_original_starter(
    package: &ImportedRprov,
) -> Result<BTreeMap<WorkspacePath, Vec<u8>>, String> {
    let mut files = BTreeMap::new();
    for file in &package.manifest().initial_workspace.files {
        let declaration = package
            .manifest()
            .inventory
            .iter()
            .find(|entry| entry.path == file.entry)
            .ok_or_else(|| "starter entry is absent from the validated inventory".to_owned())?;
        let bytes = read_entry_exact(package, &file.entry, declaration.byte_length)?;
        if files.insert(file.path.clone(), bytes).is_some() {
            return Err("starter contains a duplicate logical path".to_owned());
        }
    }
    let hash = hash_entries(files.iter().map(|(path, bytes)| (path, bytes.as_slice())))
        .map_err(|error| error.to_string())?;
    if hash != package.manifest().original_starter_tree_hash {
        return Err("original starter bytes differ from the manifest tree hash".to_owned());
    }
    Ok(files)
}

struct ReplayFacts {
    final_tree_hash: Hash,
    review_indicators: ReviewIndicators,
    advisories: Vec<AdvisoryFlag>,
    test_case_runs: u64,
    test_case_passes: u64,
    test_case_mismatches: u64,
    test_case_errors: u64,
    first_failing_case: Option<TestCaseFailure>,
    expected_hashes: Vec<(String, Hash)>,
}

impl ReplayFacts {
    fn new() -> Self {
        Self {
            final_tree_hash: Hash::zero(),
            review_indicators: ReviewIndicators::default(),
            advisories: Vec::new(),
            test_case_runs: 0,
            test_case_passes: 0,
            test_case_mismatches: 0,
            test_case_errors: 0,
            first_failing_case: None,
            expected_hashes: Vec::new(),
        }
    }
}

struct SemanticFailure {
    kind: VerificationIssueKind,
    location: VerificationIssueLocation,
    detail: String,
}

impl SemanticFailure {
    fn checkpoint(detail: impl Into<String>) -> Self {
        Self {
            kind: VerificationIssueKind::CheckpointHashes,
            location: VerificationIssueLocation::Decoder("checkpoint".to_owned()),
            detail: detail.into(),
        }
    }

    fn checkpoint_at(segment: u32, sequence: u64, detail: impl Into<String>) -> Self {
        Self {
            kind: VerificationIssueKind::CheckpointHashes,
            location: VerificationIssueLocation::Event(VerificationEventLocation {
                segment,
                sequence,
            }),
            detail: detail.into(),
        }
    }

    fn replay(detail: impl Into<String>) -> Self {
        Self {
            kind: VerificationIssueKind::Replay,
            location: VerificationIssueLocation::Decoder("replay".to_owned()),
            detail: detail.into(),
        }
    }

    fn replay_at(segment: u32, sequence: u64, detail: impl Into<String>) -> Self {
        Self {
            kind: VerificationIssueKind::Replay,
            location: VerificationIssueLocation::Event(VerificationEventLocation {
                segment,
                sequence,
            }),
            detail: detail.into(),
        }
    }

    fn event(detail: impl Into<String>) -> Self {
        Self {
            kind: VerificationIssueKind::EventChain,
            location: VerificationIssueLocation::Decoder("event stream".to_owned()),
            detail: detail.into(),
        }
    }
}

fn replay_segments(
    package: &ImportedRprov,
    starter: &BTreeMap<WorkspacePath, Vec<u8>>,
    reference: Option<&AssignmentReference>,
) -> Result<ReplayFacts, SemanticFailure> {
    let mut previous: Option<ReplayEngine> = None;
    let mut aggregate_count = 0_u64;
    let mut facts = ReplayFacts::new();

    for (index, segment) in package.manifest().segments.iter().enumerate() {
        let mut indicators =
            AttemptIndicatorAccumulator::new(segment.ordinal, segment.session_id.clone());
        let mut advisories = TypingShapeAccumulator::new(segment.ordinal);
        let initial_ref = segment
            .checkpoints
            .first()
            .ok_or_else(|| SemanticFailure::checkpoint("segment has no initial checkpoint"))?;
        let initial = decode_declared_checkpoint(package, segment, initial_ref)?;
        if initial_ref.owner.sequence != 1
            || initial.session_id() != &segment.session_id
            || initial.event_sequence() != initial_ref.owner.sequence
            || initial.workspace_hash() != initial_ref.workspace_hash
            || initial.workspace_hash() != segment.initial_tree_hash
        {
            return Err(SemanticFailure::checkpoint_at(
                segment.ordinal,
                initial_ref.owner.sequence,
                "initial checkpoint identity or workspace hash mismatch",
            ));
        }
        if index == 0 {
            if !snapshot_matches_map(&initial, starter) {
                return Err(SemanticFailure::replay_at(
                    segment.ordinal,
                    initial_ref.owner.sequence,
                    "root checkpoint bytes differ from the original starter",
                ));
            }
        } else if previous
            .as_ref()
            .is_none_or(|replay| !snapshot_matches_map(&initial, replay.workspace_state().files()))
        {
            return Err(SemanticFailure::replay_at(
                segment.ordinal,
                initial_ref.owner.sequence,
                "child initial checkpoint differs from its parent's replayed final workspace",
            ));
        }

        let mut reader = BufReader::with_capacity(
            8 * 1024,
            package
                .open_entry(&segment.events.entry)
                .map_err(|error| SemanticFailure::event(error.to_string()))?,
        );
        let mut line = Vec::new();
        let first = read_event(&mut reader, &mut line)?
            .ok_or_else(|| SemanticFailure::event("segment event stream is empty"))?;
        let mut replay = ReplayEngine::from_initial_checkpoint(StoredCheckpoint {
            owning_event: first.clone(),
            snapshot: initial,
        })
        .map_err(|error| {
            SemanticFailure::checkpoint_at(
                segment.ordinal,
                initial_ref.owner.sequence,
                error.to_string(),
            )
        })?;
        indicators
            .observe(&first, None)
            .map_err(SemanticFailure::event)?;
        advisories.observe(&first);
        let mut event_count = 1_u64;
        let mut seen_checkpoints = 1_usize;

        while let Some(envelope) = read_event(&mut reader, &mut line)? {
            let pre_edit_text = transaction(&envelope.event).and_then(|transaction| {
                replay
                    .workspace_state()
                    .document(&transaction.document_id)
                    .map(rustrace_replay::DocumentState::text)
            });
            indicators
                .observe(&envelope, pre_edit_text)
                .map_err(SemanticFailure::event)?;
            advisories.observe(&envelope);
            if let Event::TestCaseCompared(comparison) = &envelope.event
                && let Some(expected) = reference
                    .and_then(|reference| reference.test_case_expected.get(&comparison.case))
                && expected.blake3 == comparison.expected_blake3
                && let Some(actual) = replay.just_finished_command_stdout()
            {
                let computed = compare_test_case_bytes(
                    TestCase::new(comparison.case.clone()).expect("validated case name"),
                    &expected.bytes,
                    actual,
                );
                let matches = match (&comparison.outcome, computed.outcome) {
                    (rustrace_model::TestCaseComparisonOutcome::Pass, TestCaseOutcome::Pass)
                    | (rustrace_model::TestCaseComparisonOutcome::Error { .. }, _) => true,
                    (
                        rustrace_model::TestCaseComparisonOutcome::Mismatch {
                            line,
                            expected_len,
                            actual_len,
                        },
                        TestCaseOutcome::Fail(mismatch),
                    ) => {
                        *line == mismatch.line
                            && *expected_len == mismatch.expected_len as u64
                            && *actual_len == mismatch.actual_len as u64
                    }
                    _ => false,
                };
                if !matches {
                    return Err(SemanticFailure::replay_at(
                        segment.ordinal,
                        envelope.sequence,
                        "comparison differs from reference output",
                    ));
                }
            }
            replay.apply(&envelope).map_err(|error| {
                SemanticFailure::replay_at(segment.ordinal, envelope.sequence, error.to_string())
            })?;
            if let Event::TestCaseCompared(comparison) = &envelope.event {
                facts.test_case_runs = facts.test_case_runs.saturating_add(1);
                facts
                    .expected_hashes
                    .push((comparison.case.clone(), comparison.expected_blake3));
                match comparison.outcome {
                    rustrace_model::TestCaseComparisonOutcome::Pass => {
                        facts.test_case_passes = facts.test_case_passes.saturating_add(1);
                    }
                    rustrace_model::TestCaseComparisonOutcome::Mismatch { line, .. } => {
                        facts.test_case_mismatches = facts.test_case_mismatches.saturating_add(1);
                        facts.first_failing_case.get_or_insert(TestCaseFailure {
                            case: comparison.case.clone(),
                            line: Some(line),
                        });
                    }
                    rustrace_model::TestCaseComparisonOutcome::Error { .. } => {
                        facts.test_case_errors = facts.test_case_errors.saturating_add(1);
                        facts.first_failing_case.get_or_insert(TestCaseFailure {
                            case: comparison.case.clone(),
                            line: None,
                        });
                    }
                }
            }
            event_count = event_count
                .checked_add(1)
                .ok_or_else(|| SemanticFailure::event("event count overflow"))?;
            if matches!(envelope.event, Event::WorkspaceCheckpoint(_)) {
                let declaration = segment
                    .checkpoints
                    .binary_search_by_key(&envelope.sequence, |item| item.owner.sequence)
                    .ok()
                    .and_then(|position| segment.checkpoints.get(position))
                    .ok_or_else(|| {
                        SemanticFailure::checkpoint_at(
                            segment.ordinal,
                            envelope.sequence,
                            "replayed checkpoint event has no declared payload",
                        )
                    })?;
                let snapshot = decode_declared_checkpoint(package, segment, declaration)?;
                replay
                    .validate_checkpoint(&StoredCheckpoint {
                        owning_event: envelope,
                        snapshot,
                    })
                    .map_err(|error| {
                        SemanticFailure::checkpoint_at(
                            segment.ordinal,
                            declaration.owner.sequence,
                            error.to_string(),
                        )
                    })?;
                seen_checkpoints += 1;
            }
        }
        if event_count != segment.inclusive_event_count {
            return Err(SemanticFailure::event(
                "streamed event count differs from the segment inclusive count",
            ));
        }
        if seen_checkpoints != segment.checkpoints.len() {
            return Err(SemanticFailure::checkpoint_at(
                segment.ordinal,
                segment.inclusive_event_count,
                "not every declared checkpoint was replay-certified",
            ));
        }
        let expected = segment.final_tree_hash.known().copied().ok_or_else(|| {
            SemanticFailure::replay_at(
                segment.ordinal,
                segment.inclusive_event_count,
                "segment final tree hash is unavailable",
            )
        })?;
        if !replay.is_finalized() {
            return Err(SemanticFailure::replay_at(
                segment.ordinal,
                segment.inclusive_event_count,
                "segment replay did not reach SubmissionFinalized",
            ));
        }
        replay
            .verify_final_workspace_hash(expected)
            .map_err(|error| {
                SemanticFailure::replay_at(
                    segment.ordinal,
                    segment.inclusive_event_count,
                    error.to_string(),
                )
            })?;
        aggregate_count = aggregate_count
            .checked_add(event_count)
            .ok_or_else(|| SemanticFailure::event("aggregate event count overflow"))?;
        let (segment_factual, process_attempt) =
            indicators.finish().map_err(SemanticFailure::event)?;
        merge_factual(&mut facts.review_indicators.factual, segment_factual)
            .map_err(SemanticFailure::event)?;
        facts.review_indicators.attempts.push(process_attempt);
        facts.advisories.extend(advisories.finish());
        previous = Some(replay);
    }

    if aggregate_count != package.manifest().aggregate_event_count {
        return Err(SemanticFailure::event(
            "streamed aggregate event count differs from the manifest",
        ));
    }
    let replay = previous.ok_or_else(|| SemanticFailure::replay("package has no segments"))?;
    let expected = package
        .manifest()
        .final_tree_hash
        .known()
        .copied()
        .ok_or_else(|| SemanticFailure::replay("package final tree hash is unavailable"))?;
    replay
        .verify_final_workspace_hash(expected)
        .map_err(|error| {
            let segment = package
                .manifest()
                .segments
                .last()
                .expect("nonempty segments");
            SemanticFailure::replay_at(
                segment.ordinal,
                segment.inclusive_event_count,
                error.to_string(),
            )
        })?;
    facts.final_tree_hash = replay.current_workspace_hash();
    Ok(facts)
}

fn decode_declared_checkpoint(
    package: &ImportedRprov,
    segment: &RprovSegment,
    declaration: &RprovCheckpointRef,
) -> Result<CheckpointSnapshot, SemanticFailure> {
    let bytes = read_entry_exact(package, &declaration.entry, declaration.byte_length).map_err(
        |error| SemanticFailure::checkpoint_at(segment.ordinal, declaration.owner.sequence, error),
    )?;
    let snapshot = decode_checkpoint(&bytes).map_err(|error| {
        SemanticFailure::checkpoint_at(
            segment.ordinal,
            declaration.owner.sequence,
            error.to_string(),
        )
    })?;
    if snapshot.session_id() != &segment.session_id
        || snapshot.event_sequence() != declaration.owner.sequence
        || snapshot.workspace_hash() != declaration.workspace_hash
    {
        return Err(SemanticFailure::checkpoint_at(
            segment.ordinal,
            declaration.owner.sequence,
            "decoded checkpoint disagrees with its manifest declaration",
        ));
    }
    Ok(snapshot)
}

fn snapshot_matches_map(
    snapshot: &CheckpointSnapshot,
    expected: &BTreeMap<WorkspacePath, Vec<u8>>,
) -> bool {
    snapshot.files().len() == expected.len()
        && snapshot
            .files()
            .iter()
            .zip(expected)
            .all(|(file, expected)| {
                file.path == *expected.0 && file.contents.as_slice() == expected.1.as_slice()
            })
}

fn read_event<R: Read>(
    reader: &mut BufReader<R>,
    line: &mut Vec<u8>,
) -> Result<Option<EventEnvelope>, SemanticFailure> {
    line.clear();
    let maximum = (MAX_ENVELOPE_BYTES as u64).saturating_add(2);
    let read = reader
        .take(maximum)
        .read_until(b'\n', line)
        .map_err(|error| SemanticFailure::event(error.to_string()))?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        return Err(SemanticFailure::event(
            "event envelope exceeds its bound or lacks final LF",
        ));
    }
    line.pop();
    if line.len() > MAX_ENVELOPE_BYTES {
        return Err(SemanticFailure::event("event envelope exceeds its bound"));
    }
    match decode_envelope(line, DecodePolicy::RejectUnsupported)
        .map_err(|error| SemanticFailure::event(error.to_string()))?
    {
        DecodeOutcome::Decoded(envelope) => Ok(Some(envelope)),
        DecodeOutcome::Skipped(_) => {
            Err(SemanticFailure::event("unsupported event was not rejected"))
        }
    }
}

fn transaction(event: &Event) -> Option<&rustrace_model::EditorTransaction> {
    match event {
        Event::FileEdited(transaction) => Some(transaction),
        Event::InternalPaste(paste) => Some(&paste.transaction),
        _ => None,
    }
}

fn first_location(
    link: Option<&crate::process_indicators::IndicatorEventLink>,
) -> Option<VerificationEventLocation> {
    link.map(|link| VerificationEventLocation {
        segment: link.segment,
        sequence: link.sequence,
    })
}

fn read_entry_exact(
    package: &ImportedRprov,
    entry: &str,
    declared_length: u64,
) -> Result<Vec<u8>, String> {
    let expected = usize::try_from(declared_length)
        .map_err(|_| "package entry length does not fit this platform".to_owned())?;
    let mut bytes = Vec::with_capacity(expected.min(1024 * 1024));
    package
        .open_entry(entry)
        .map_err(|error| error.to_string())?
        .take(declared_length.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() != expected {
        return Err("package entry changed length while reading".to_owned());
    }
    Ok(bytes)
}

struct AssignmentReference {
    manifest: AssignmentManifest,
    manifest_bytes: Vec<u8>,
    starter_hash: Hash,
    test_case_suite_hash: Option<Hash>,
    test_case_expected: BTreeMap<String, ReferenceExpectedOutput>,
}

struct ReferenceExpectedOutput {
    blake3: Hash,
    bytes: Vec<u8>,
}

fn compare_reference(
    package: &ImportedRprov,
    reference: Option<&Path>,
    report: &mut VerificationReport,
) -> Option<AssignmentReference> {
    let Some(reference) = reference else {
        report.assignment_reference = AssignmentReferenceStatus::Unverified;
        return None;
    };
    let reference = match read_assignment_reference(reference) {
        Ok(reference) => reference,
        Err(error) => {
            report.record_unavailable(
                VerificationIssueKind::AssignmentReference,
                VerificationIssueLocation::Decoder("assignment reference".to_owned()),
                error,
            );
            return None;
        }
    };
    let manifest = package.manifest();
    let suite_mismatch = match (
        reference.test_case_suite_hash,
        manifest.test_case_suite_hash,
    ) {
        (None, Some(_)) => Some(
            "assignment package kind mismatch: reference has no packaged test-case suite but provenance requires one",
        ),
        (Some(_), None) => Some(
            "assignment package kind mismatch: reference has a packaged test-case suite but provenance records none",
        ),
        (Some(reference), Some(recorded)) if reference != recorded => {
            Some("assignment test-case suite identity mismatch")
        }
        _ => None,
    };
    if let Some(detail) = suite_mismatch {
        report.fail(
            VerificationIssueKind::AssignmentReference,
            VerificationIssueLocation::Decoder("assignment reference".to_owned()),
            detail,
        );
        return Some(reference);
    }
    let matches = reference.manifest.course_id == manifest.course_id
        && reference.manifest.assignment_id == manifest.assignment_id
        && reference.manifest.assignment_version == manifest.assignment_version
        && reference.manifest_bytes.len() as u64 == manifest.assignment_manifest.byte_length
        && rprov_raw_blake3(&reference.manifest_bytes) == manifest.assignment_manifest.blake3
        && reference.starter_hash == manifest.original_starter_tree_hash;
    if matches {
        report.assignment_reference = AssignmentReferenceStatus::Ok;
    } else {
        report.fail(
            VerificationIssueKind::AssignmentReference,
            VerificationIssueLocation::Decoder("assignment reference".to_owned()),
            "assignment manifest or starter identity mismatch",
        );
    }
    Some(reference)
}

fn authenticate_test_case_reference(
    reference: Option<&AssignmentReference>,
    facts: &ReplayFacts,
    report: &mut VerificationReport,
) {
    let Some(reference) = reference else {
        return;
    };
    if report.assignment_reference != AssignmentReferenceStatus::Ok
        || reference.manifest.format_version != 2
        || reference.test_case_suite_hash.is_none()
    {
        return;
    }
    for (case, recorded) in &facts.expected_hashes {
        if reference
            .test_case_expected
            .get(case)
            .map(|expected| expected.blake3)
            != Some(*recorded)
        {
            report.fail(
                VerificationIssueKind::AssignmentReference,
                VerificationIssueLocation::Decoder("assignment reference".to_owned()),
                format!("expected output hash mismatch for test case {case}"),
            );
            return;
        }
    }
    report.test_case_evidence = Some(TestCaseEvidenceStatus::ReferenceVerified);
}

fn read_assignment_reference(path: &Path) -> Result<AssignmentReference, String> {
    if path.extension().is_some_and(|extension| extension == "rta") {
        read_rta_reference(path)
    } else {
        read_manifest_reference(path)
    }
}

fn read_manifest_reference(path: &Path) -> Result<AssignmentReference, String> {
    let manifest_bytes = read_bounded_file(path, MAX_MANIFEST_BYTES)?;
    let manifest = AssignmentManifest::parse(&manifest_bytes).map_err(|error| error.to_string())?;
    if manifest.format_version == 2 {
        return Err(
            "format_version = 2 references must be an .rta archive so packaged test cases can be validated"
                .to_owned(),
        );
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let starter = parent.join("starter");
    let starter_hash = hash_workspace(&starter).map_err(|error| error.to_string())?;
    Ok(AssignmentReference {
        manifest,
        manifest_bytes,
        starter_hash,
        test_case_suite_hash: None,
        test_case_expected: BTreeMap::new(),
    })
}

fn read_rta_reference(path: &Path) -> Result<AssignmentReference, String> {
    let source = open_regular_read_only(path)?;
    let mut temporary = ReferenceDirectory::new()?;
    let extracted = extract_assignment_package(
        BufReader::new(source),
        &temporary.path,
        ExtractionLimits::default(),
    )
    .map_err(|error| error.to_string())?;
    temporary.owned = true;
    let starter_hash = hash_workspace(&temporary.path).map_err(|error| error.to_string())?;
    let (test_case_suite_hash, test_case_expected) = extracted.test_cases.map_or_else(
        || (None, BTreeMap::new()),
        |suite| {
            let expected = suite
                .cases
                .into_iter()
                .map(|case| {
                    (
                        case.name,
                        ReferenceExpectedOutput {
                            blake3: rprov_raw_blake3(&case.expected),
                            bytes: case.expected,
                        },
                    )
                })
                .collect();
            (Some(suite.hash), expected)
        },
    );
    Ok(AssignmentReference {
        manifest: extracted.manifest,
        manifest_bytes: extracted.manifest_bytes,
        starter_hash,
        test_case_suite_hash,
        test_case_expected,
    })
}

struct ReferenceDirectory {
    path: PathBuf,
    owned: bool,
}

impl ReferenceDirectory {
    fn new() -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let temporary_root = fs::canonicalize(std::env::temp_dir())
            .map_err(|error| format!("could not resolve the temporary directory: {error}"))?;
        for _ in 0..16 {
            let path = temporary_root.join(format!(
                "rustrace-verify-reference-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            if !path.exists() {
                return Ok(Self { path, owned: false });
            }
        }
        Err("could not reserve a unique reference extraction path".to_owned())
    }
}

impl Drop for ReferenceDirectory {
    fn drop(&mut self) {
        if self.owned
            && self.path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .starts_with("rustrace-verify-reference-")
            })
        {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn read_bounded_file(path: &Path, maximum: usize) -> Result<Vec<u8>, String> {
    let file = open_regular_read_only(path)?;
    let mut bytes = Vec::with_capacity(maximum.min(64 * 1024));
    file.take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > maximum {
        return Err(format!("reference file exceeds the {maximum}-byte limit"));
    }
    Ok(bytes)
}

fn open_regular_read_only(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|error| error.to_string())?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("input must be a regular file".to_owned());
    }
    Ok(file)
}

pub fn run_verify(args: &[String], output: &mut impl Write) -> Result<u8, String> {
    let (path, reference) = match args {
        [path] => (Path::new(path), None),
        [path, option, reference] if option == "--reference" => {
            (Path::new(path), Some(Path::new(reference)))
        }
        _ => return Err("Usage: rustrace verify PATH [--reference PATH]".to_owned()),
    };
    let report = verify_path(path, reference);
    if let Some(issue) = report
        .issues
        .iter()
        .find(|issue| issue.kind == VerificationIssueKind::Input)
    {
        write_safe_diagnostic(
            output,
            format_args!("could not open {}: {}", path.display(), issue.detail),
        )?;
        return Ok(report.exit_code());
    }
    write_row(
        output,
        "Package structure",
        status(report.package_structure),
    )?;
    if let Some(issue) = report.issues.iter().find(|issue| {
        issue.kind == VerificationIssueKind::PackageStructure
            && issue.detail.contains("recovery-incomplete")
    }) {
        write_safe_diagnostic(
            output,
            format_args!("INCOMPLETE RECOVERY: {}", issue.detail),
        )?;
    }
    write_row(output, "Event chain", status(report.event_chain))?;
    write_row(
        output,
        "Checkpoint hashes",
        status(report.checkpoint_hashes),
    )?;
    write_remainder(output, &report, reference)?;
    for advisory in &report.advisories {
        writeln!(output, "Advisory: {}", display_advisory(advisory))
            .map_err(|error| error.to_string())?;
    }
    write_evidence_limitations(output, &report)?;
    Ok(report.exit_code())
}

/// The shared Task 8.6 wording. The first sentence follows the package's own
/// verification outcome (`review_flags::evidence_statement`), so an unreadable
/// TA-side reference never reads as a package failure.
fn write_evidence_limitations(
    output: &mut impl Write,
    report: &VerificationReport,
) -> Result<(), String> {
    writeln!(
        output,
        "Evidence limitations: {} {EVIDENCE_LIMITATION_FIRST} {EVIDENCE_LIMITATION_SECOND}",
        evidence_statement(report)
    )
    .map_err(|error| error.to_string())
}

fn write_remainder(
    output: &mut impl Write,
    report: &VerificationReport,
    reference: Option<&Path>,
) -> Result<(), String> {
    write_row(output, "Replay", status(report.replay))?;
    write_row(
        output,
        "Submitted source match",
        match report.submitted_source_match {
            SubmittedSourceStatus::Ok => "OK",
            SubmittedSourceStatus::SourceMismatch => "SOURCE_MISMATCH",
            SubmittedSourceStatus::Unavailable => "unavailable",
        },
    )?;
    if let Some(issue) = report.issues.iter().find(|issue| {
        issue.kind == VerificationIssueKind::SubmittedSource
            && report.submitted_source_match == SubmittedSourceStatus::Unavailable
    }) {
        write_safe_diagnostic(
            output,
            format_args!("Submitted source unavailable: {}", issue.detail),
        )?;
    }
    write_row(
        output,
        "Assignment reference",
        match report.assignment_reference {
            AssignmentReferenceStatus::Ok => "OK",
            AssignmentReferenceStatus::Mismatch => "MISMATCH",
            AssignmentReferenceStatus::Unverified => "unverified",
        },
    )?;
    write_row(
        output,
        "Test-case runs",
        &count_or_unknown(report.test_case_runs),
    )?;
    write_row(
        output,
        "Test-case passes",
        &count_or_unknown(report.test_case_passes),
    )?;
    write_row(
        output,
        "Test-case mismatches",
        &count_or_unknown(report.test_case_mismatches),
    )?;
    write_row(
        output,
        "Test-case errors",
        &count_or_unknown(report.test_case_errors),
    )?;
    write_row(
        output,
        "Test-case evidence",
        match report.test_case_evidence {
            Some(TestCaseEvidenceStatus::Recorded) => "recorded (unverified)",
            Some(TestCaseEvidenceStatus::ReferenceVerified) => "reference-verified",
            None => "unavailable",
        },
    )?;
    let first_failure = match &report.first_failing_case {
        Some(failure) => failure.line.map_or_else(
            || failure.case.clone(),
            |line| format!("{}, line {line}", failure.case),
        ),
        None if report.test_case_runs.is_some() => "none".to_owned(),
        None => "unknown".to_owned(),
    };
    write_row(output, "First failing case", &first_failure)?;
    if let (Some(reference), Some(issue)) = (
        reference,
        report.issues.iter().find(|issue| {
            issue.kind == VerificationIssueKind::AssignmentReference
                && report.assignment_reference == AssignmentReferenceStatus::Unverified
        }),
    ) {
        write_safe_diagnostic(
            output,
            format_args!(
                "Assignment reference unavailable: could not read {}: {}",
                reference.display(),
                issue.detail
            ),
        )?;
    }
    write_row(
        output,
        "External changes",
        &report
            .external_changes
            .map_or_else(|| "unknown".to_owned(), |count| count.to_string()),
    )?;
    write_row(
        output,
        "Unknown edit origins",
        &report
            .unknown_edit_origins
            .map_or_else(|| "unknown".to_owned(), |count| count.to_string()),
    )
}

fn count_or_unknown(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |count| count.to_string())
}

fn status(status: VerificationStatus) -> &'static str {
    match status {
        VerificationStatus::Ok => "OK",
        VerificationStatus::Failed => "FAILED",
        VerificationStatus::Unavailable => "unavailable",
    }
}

fn write_row(output: &mut impl Write, label: &str, value: &str) -> Result<(), String> {
    writeln!(output, "{label:<25}{value}").map_err(|error| error.to_string())
}

fn write_safe_diagnostic(
    output: &mut impl Write,
    diagnostic: std::fmt::Arguments<'_>,
) -> Result<(), String> {
    writeln!(output, "{}", crate::display::label_fmt(diagnostic, 4096))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submitted_source_read_error_is_typed_as_unavailable() {
        let mut report = VerificationReport::unavailable();

        let status = compare_submitted_source(
            &mut report,
            Hash::zero(),
            Err("outer source became unreadable".to_owned()),
            None,
        );

        assert_eq!(status, SubmittedSourceStatus::Unavailable);
        assert_eq!(
            report.submitted_source_match,
            SubmittedSourceStatus::Unavailable
        );
        assert!(report.issues.iter().any(|issue| {
            issue.kind == VerificationIssueKind::SubmittedSource
                && issue.detail == "outer source became unreadable"
        }));
    }
}
