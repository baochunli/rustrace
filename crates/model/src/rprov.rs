//! Pure version 1 `.rprov` container and manifest model.
//!
//! This module defines bytes and validates already-in-memory metadata. It does
//! not open paths, extract archives, finalize sessions, write packages, replay
//! workspaces, or compare submitted source.
//!
//! Version 1 event enums are additive. `DependencyTool` edit origins and
//! Doc/Add/Remove/Update controlled actions decode alongside historical v1
//! values; no released field, tag, default, or container version is rewritten.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet},
    error::Error,
    fmt,
    io::{self, BufRead, BufReader, Cursor, Read, Write},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::{
    DecodeError, DecodeOutcome, DecodePolicy, EditOrigin, Event, EventEnvelope, compute_event_hash,
    decode_envelope, encode_envelope,
};
use crate::{Hash, MAX_IDENTIFIER_BYTES, RecordedEventRef, SessionId, WorkspacePath};

pub const RPROV_FORMAT_VERSION_V1: u32 = 1;
pub const RPROV_CONTAINER_HEADER_BYTES: usize = 40;
pub const RPROV_RECORD_HEADER_BYTES: usize = 16;
pub const MAX_RPROV_STORED_BYTES: u64 = 2_147_483_648;
pub const MAX_RPROV_EXPANDED_BYTES: u64 = 4_294_967_296;
pub const MAX_RPROV_ARCHIVE_ENTRIES: usize = 4_096;
pub const MAX_RPROV_RECORD_PAYLOAD_BYTES: u64 = 1_073_741_824;
pub const MAX_RPROV_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RPROV_ARCHIVE_PATH_BYTES: usize = 192;
pub const MAX_RPROV_ARCHIVE_COMPONENT_BYTES: usize = 80;
pub const MAX_RPROV_ARCHIVE_PATH_DEPTH: usize = 6;
pub const MAX_RPROV_SEGMENTS: usize = 128;
pub const MAX_RPROV_EVENTS: u64 = 1_000_000;
pub const MAX_RPROV_SEGMENT_EVENTS_BYTES: u64 = 1_073_741_824;
pub const MAX_RPROV_EVENTS_BYTES: u64 = 1_610_612_736;
pub const MAX_RPROV_CHECKPOINTS_PER_SEGMENT: usize = 1_024;
pub const MAX_RPROV_CHECKPOINT_ENCODED_BYTES: u64 = 11_128_194;
pub const MAX_RPROV_CHECKPOINT_EXPANDED_BYTES: u64 = 11_062_597;
pub const MAX_RPROV_SEGMENT_CHECKPOINT_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_RPROV_CHECKPOINT_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_RPROV_EVIDENCE_PER_SEGMENT: usize = 256;
pub const MAX_RPROV_EVIDENCE_USAGES_PER_ARTIFACT: usize = 1_024;
pub const MAX_RPROV_EVIDENCE_USAGES: usize = 8_192;
pub const MAX_RPROV_EVIDENCE_ENTRY_BYTES: u64 = 35_651_584;
pub const MAX_RPROV_SEGMENT_EVIDENCE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_RPROV_EVIDENCE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_RPROV_METADATA_PER_SEGMENT: usize = 64;
pub const MAX_RPROV_METADATA_ENTRY_BYTES: u64 = 1024 * 1024;
pub const MAX_RPROV_SEGMENT_METADATA_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_RPROV_METADATA_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_RPROV_SOURCE_LINKS_PER_SEGMENT: usize = 4_096;
pub const MAX_RPROV_SOURCE_LINKS: usize = 8_192;
pub const MAX_RPROV_INITIAL_FILES: usize = 256;
pub const MAX_RPROV_INITIAL_FILE_BYTES: u64 = 1024 * 1024;
pub const MAX_RPROV_INITIAL_WORKSPACE_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_RPROV_ASSIGNMENT_MANIFEST_BYTES: u64 = 64 * 1024;
pub const MAX_RPROV_TOOLS: usize = 16;
pub const MAX_RPROV_TOOL_NAME_BYTES: usize = 64;
pub const MAX_RPROV_TOOL_VERSION_BYTES: usize = 256;
pub const MAX_RPROV_PRODUCER_VALUE_BYTES: usize = 256;
pub const MAX_RPROV_UNAVAILABLE_ASSURANCES: usize = 16;
pub const MAX_RPROV_RECOVERY_GAPS: usize = 8_192;
pub const MAX_RPROV_JSON_NESTING: usize = 16;
pub const MAX_RPROV_JSON_VALUES: usize = 131_072;
pub const MAX_RPROV_JSON_KEY_BYTES: usize = 64;
pub const MAX_RPROV_JSON_STRING_BYTES: usize = 4_096;
pub const MAX_RPROV_JSON_ARRAY_ITEMS: usize = 8_192;

const RPROV_MAGIC: &[u8; 8] = b"RUSTPROV";
const RPROV_COMPRESSION_NONE: u8 = 0;
const RPROV_RECORD_REGULAR_FILE: u8 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RprovContainerHeader {
    pub format_version: u32,
    pub entry_count: u32,
    pub stored_records_bytes: u64,
    pub expanded_records_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RprovRecordType {
    RegularFile,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RprovRecordHeader {
    pub path_bytes: u16,
    pub entry_type: RprovRecordType,
    pub payload_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RprovKnown<T> {
    Known { value: T },
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RprovEventIntegrityKind {
    Chain,
    Identity,
    Sequence,
    FinalTreeBinding,
    MissingExternalEvidence,
}

/// Review diagnostics produced only after the complete event stream and its
/// terminal invariants have been checked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RprovEventStreamReview {
    missing_external_evidence: Option<RprovError>,
    remaining_error: Option<RprovError>,
}

impl RprovEventStreamReview {
    pub fn into_errors(self) -> (Option<RprovError>, Option<RprovError>) {
        (self.missing_external_evidence, self.remaining_error)
    }
}

impl<T> RprovKnown<T> {
    pub fn known(&self) -> Option<&T> {
        match self {
            Self::Known { value } => Some(value),
            Self::Unknown => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RprovPackageState {
    CleanFinalized,
    RecoveryIncomplete {
        unavailable_assurances: Vec<RprovUnavailableAssurance>,
        gaps: Vec<RprovRecoveryGap>,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RprovUnavailableAssurance {
    CompleteAncestry,
    CompleteEventStream,
    FinalCheckpoint,
    ReferencedEvidence,
    SourceLinkIntegrity,
    ReplayConsistency,
    FinalTree,
    CleanFinalization,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RprovRecoveryGap {
    MissingAncestry {
        before_session_id: SessionId,
    },
    MissingEvidence {
        event: RecordedEventRef,
        blake3: Hash,
    },
    MissingSourceLink {
        event: RecordedEventRef,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RprovSubmittedSourceComparison {
    UnavailableStandalone,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RprovProducer {
    pub client_version: RprovKnown<String>,
    pub build_identity: RprovKnown<String>,
    pub os: RprovKnown<String>,
    pub architecture: RprovKnown<String>,
    pub rust_tools: Vec<RprovToolVersion>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RprovToolVersion {
    pub tool: String,
    pub version: RprovKnown<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RprovAssignmentManifestIdentity {
    pub format_version: u32,
    pub byte_length: u64,
    pub blake3: Hash,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RprovInitialWorkspace {
    pub files: Vec<RprovInitialWorkspaceFile>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RprovInitialWorkspaceFile {
    pub path: WorkspacePath,
    pub entry: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RprovSegmentTime {
    pub started_at_utc: RprovKnown<DateTime<Utc>>,
    pub ended_at_utc: RprovKnown<DateTime<Utc>>,
    pub inter_attempt_time: RprovInterAttemptTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RprovInterAttemptTime {
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RprovParentLink {
    pub session_id: SessionId,
    pub terminal_event_hash: Hash,
    pub final_tree_hash: Hash,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RprovEventStreamRef {
    pub format_version: u32,
    pub entry: String,
    pub byte_length: u64,
    pub blake3: Hash,
    pub completeness: RprovEventStreamCompleteness,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RprovEventStreamCompleteness {
    Complete,
    PrefixOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RprovCheckpointRef {
    pub role: RprovCheckpointRole,
    pub format_version: u32,
    pub entry: String,
    pub byte_length: u64,
    pub blake3: Hash,
    pub owner: RecordedEventRef,
    pub workspace_hash: Hash,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RprovCheckpointRole {
    Initial,
    Accepted,
    Final,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RprovMetadataRef {
    pub format_version: u32,
    pub entry: String,
    pub byte_length: u64,
    pub blake3: Hash,
    pub owner: RecordedEventRef,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RprovEvidenceRef {
    pub kind: RprovEvidenceKind,
    pub entry: String,
    pub byte_length: u64,
    pub blake3: Hash,
    pub usages: Vec<RecordedEventRef>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RprovEvidenceKind {
    ExternalRecovery,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RprovSourceLink {
    InternalPaste {
        paste_event: RecordedEventRef,
        copied_event: RecordedEventRef,
        document_id: crate::DocumentId,
        path: WorkspacePath,
        version: u64,
        content_hash: Hash,
        start_byte: u64,
        end_byte: u64,
    },
    LegacyPaste {
        event: RecordedEventRef,
        verification: RprovLegacyPasteVerification,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RprovLegacyPasteVerification {
    OriginUnverified,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RprovSegment {
    pub ordinal: u32,
    pub session_id: SessionId,
    pub course_id: String,
    pub assignment_id: String,
    pub assignment_version: String,
    pub assignment_manifest_blake3: Hash,
    pub original_starter_tree_hash: Hash,
    pub initial_tree_hash: Hash,
    pub producer: RprovProducer,
    pub time: RprovSegmentTime,
    pub parent: Option<RprovParentLink>,
    pub events: RprovEventStreamRef,
    pub checkpoints: Vec<RprovCheckpointRef>,
    pub metadata: Vec<RprovMetadataRef>,
    pub evidence: Vec<RprovEvidenceRef>,
    pub source_links: Vec<RprovSourceLink>,
    pub inclusive_event_count: u64,
    pub last_event_hash: Hash,
    pub terminal_event_hash: RprovKnown<Hash>,
    pub final_tree_hash: RprovKnown<Hash>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RprovEntryKind {
    InitialWorkspaceBlob,
    Events,
    Checkpoint,
    RuntimeMetadata,
    ExternalRecoveryEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RprovInventoryEntry {
    pub path: String,
    pub byte_length: u64,
    pub blake3: Hash,
    pub kind: RprovEntryKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RprovManifest {
    pub format_version: u32,
    pub package_state: RprovPackageState,
    pub submitted_source_comparison: RprovSubmittedSourceComparison,
    pub course_id: String,
    pub assignment_id: String,
    pub assignment_version: String,
    pub student_id: String,
    pub latest_session_id: SessionId,
    pub original_starter_tree_hash: Hash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_case_suite_hash: Option<Hash>,
    pub final_tree_hash: RprovKnown<Hash>,
    pub aggregate_event_count: u64,
    pub producer: RprovProducer,
    pub assignment_manifest: RprovAssignmentManifestIdentity,
    pub initial_workspace: RprovInitialWorkspace,
    pub segments: Vec<RprovSegment>,
    pub inventory: Vec<RprovInventoryEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRprovManifest {
    format_version: u32,
    package_state: RprovPackageState,
    submitted_source_comparison: RprovSubmittedSourceComparison,
    course_id: String,
    assignment_id: String,
    assignment_version: String,
    student_id: String,
    latest_session_id: SessionId,
    original_starter_tree_hash: Hash,
    #[serde(default)]
    test_case_suite_hash: Option<Hash>,
    final_tree_hash: RprovKnown<Hash>,
    aggregate_event_count: u64,
    producer: RprovProducer,
    assignment_manifest: RprovAssignmentManifestIdentity,
    initial_workspace: RprovInitialWorkspace,
    segments: Vec<RprovSegment>,
    inventory: Vec<RprovInventoryEntry>,
}

impl From<WireRprovManifest> for RprovManifest {
    fn from(wire: WireRprovManifest) -> Self {
        Self {
            format_version: wire.format_version,
            package_state: wire.package_state,
            submitted_source_comparison: wire.submitted_source_comparison,
            course_id: wire.course_id,
            assignment_id: wire.assignment_id,
            assignment_version: wire.assignment_version,
            student_id: wire.student_id,
            latest_session_id: wire.latest_session_id,
            original_starter_tree_hash: wire.original_starter_tree_hash,
            test_case_suite_hash: wire.test_case_suite_hash,
            final_tree_hash: wire.final_tree_hash,
            aggregate_event_count: wire.aggregate_event_count,
            producer: wire.producer,
            assignment_manifest: wire.assignment_manifest,
            initial_workspace: wire.initial_workspace,
            segments: wire.segments,
            inventory: wire.inventory,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RprovArchiveEntryType {
    RegularFile,
    Directory,
    Symlink,
    HardLink,
    Special,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RprovArchiveEntry {
    pub path: String,
    pub entry_type: RprovArchiveEntryType,
    pub byte_length: u64,
    pub blake3: Option<Hash>,
}

/// Raw BLAKE3-256 used by v1 inventory entries.
pub fn rprov_raw_blake3(bytes: &[u8]) -> Hash {
    Hash::from_bytes(*blake3::hash(bytes).as_bytes())
}

/// Validates one payload against its manifest inventory declaration.
///
/// Checkpoints and runtime metadata receive only their version preflight here;
/// their complete existing codecs and semantic consumers remain T6.4 work.
pub fn validate_rprov_payload(
    declaration: &RprovInventoryEntry,
    bytes: &[u8],
) -> Result<(), RprovError> {
    validate_inventory_shape(declaration)?;
    if declaration.byte_length != bytes.len() as u64 {
        return Err(RprovError::invalid(
            "payload byte_length",
            "does not equal the exact payload length",
        ));
    }
    if declaration.blake3 != rprov_raw_blake3(bytes) {
        return Err(RprovError::invalid(
            "payload blake3",
            "does not equal raw BLAKE3-256 of the exact payload",
        ));
    }
    match declaration.kind {
        RprovEntryKind::Checkpoint => validate_checkpoint_version(bytes),
        RprovEntryKind::RuntimeMetadata => validate_metadata_version(bytes),
        RprovEntryKind::InitialWorkspaceBlob
        | RprovEntryKind::Events
        | RprovEntryKind::ExternalRecoveryEvidence => Ok(()),
    }
}

fn validate_checkpoint_version(bytes: &[u8]) -> Result<(), RprovError> {
    const MAGIC: &[u8; 8] = b"RUSTCPK\0";
    if bytes.len() < 12 {
        return Err(RprovError::Truncated {
            layer: "checkpoint preamble",
        });
    }
    if &bytes[..8] != MAGIC {
        return Err(RprovError::invalid(
            "checkpoint magic",
            "does not match the accepted checkpoint codec",
        ));
    }
    let version = u32::from_be_bytes(bytes[8..12].try_into().unwrap());
    require_inner_version("checkpoint", version)
}

fn validate_metadata_version(bytes: &[u8]) -> Result<(), RprovError> {
    preflight_compact_json_object(bytes, "runtime metadata")?;
    let version = inner_json_version_prefix(bytes, "runtime metadata")?;
    require_inner_version("runtime metadata", version)
}

fn inner_json_version_prefix(bytes: &[u8], layer: &'static str) -> Result<u32, RprovError> {
    const PREFIX: &[u8] = b"{\"version\":";
    if !bytes.starts_with(PREFIX) {
        return Err(RprovError::invalid(
            layer,
            "version must be the first canonical member",
        ));
    }
    let remaining = &bytes[PREFIX.len()..];
    let digit_count = remaining
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digit_count == 0 || !matches!(remaining.get(digit_count), Some(b',') | Some(b'}')) {
        return Err(RprovError::invalid(
            layer,
            "version must be an unsigned decimal u32",
        ));
    }
    if digit_count > 1 && remaining[0] == b'0' {
        return Err(RprovError::invalid(
            layer,
            "version must use its canonical unsigned decimal lexeme",
        ));
    }
    std::str::from_utf8(&remaining[..digit_count])
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| RprovError::invalid(layer, "version must fit u32"))
}

fn require_inner_version(layer: &'static str, version: u32) -> Result<(), RprovError> {
    if version == RPROV_FORMAT_VERSION_V1 {
        Ok(())
    } else {
        Err(RprovError::UnsupportedInnerVersion {
            layer,
            found: version,
            supported: RPROV_FORMAT_VERSION_V1,
        })
    }
}

/// Validates one already-bounded `events.jsonl` payload against its validated
/// manifest and exact declared segment.
///
/// This preserves event bytes, sequences and hashes, checks package-level
/// structural references, and never applies events to a workspace.
pub fn validate_rprov_event_stream(
    manifest: &RprovManifest,
    segment: &RprovSegment,
    bytes: &[u8],
) -> Result<(), RprovError> {
    validate_rprov_event_stream_reader(manifest, segment, Cursor::new(bytes))
}

/// Streaming form of [`validate_rprov_event_stream`].
///
/// The reader is consumed through a fixed buffer and one bounded envelope
/// buffer. At most one byte beyond the declared stream length is inspected, so
/// hostile lengths and missing framing do not require retaining the complete
/// `events.jsonl` payload.
pub fn validate_rprov_event_stream_reader<R: Read>(
    manifest: &RprovManifest,
    segment: &RprovSegment,
    reader: R,
) -> Result<(), RprovError> {
    let review = review_rprov_event_stream_reader(manifest, segment, reader)?;
    let (missing_external_evidence, remaining_error) = review.into_errors();
    if let Some(error) = remaining_error {
        return Err(error);
    }
    if let Some(error) = missing_external_evidence {
        return Err(error);
    }
    Ok(())
}

/// Fully validates one event stream while separately retaining the first
/// missing-external-evidence issue for review.
pub fn review_rprov_event_stream_reader<R: Read>(
    manifest: &RprovManifest,
    segment: &RprovSegment,
    reader: R,
) -> Result<RprovEventStreamReview, RprovError> {
    manifest.validate()?;
    let segment_index = manifest
        .segments
        .iter()
        .position(|declared| declared == segment)
        .ok_or_else(|| {
            RprovError::invalid(
                "segments",
                "event stream segment must be an exact member of the validated manifest",
            )
        })?;
    let is_tip = segment_index + 1 == manifest.segments.len();
    let package_state = &manifest.package_state;
    require_inner_version("event", segment.events.format_version)?;
    require_max(
        "segment events bytes",
        segment.events.byte_length,
        MAX_RPROV_SEGMENT_EVENTS_BYTES,
    )?;
    let read_limit =
        segment
            .events
            .byte_length
            .checked_add(1)
            .ok_or(RprovError::ArithmeticOverflow {
                field: "event stream read limit",
            })?;
    let mut reader = BufReader::with_capacity(8 * 1024, reader.take(read_limit));
    let mut raw_hasher = blake3::Hasher::new();
    let mut raw_bytes = 0_u64;
    let mut line = Vec::new();

    let links = EventLinkIndex::new(segment)?;
    let recovery_gaps = EventRecoveryGapIndex::new(package_state, segment)?;
    let checkpoint_by_sequence = checkpoint_index(segment)?;
    let mut seen_checkpoints = HashSet::with_capacity(checkpoint_by_sequence.len());
    let mut seen_links = vec![false; segment.source_links.len()];
    let mut seen_copies = vec![false; segment.source_links.len()];
    let mut seen_source_gaps = HashSet::with_capacity(recovery_gaps.missing_source.len());
    let mut event_hashes = Vec::new();
    let mut previous_hash = Hash::zero();
    let mut last_event: Option<EventEnvelope> = None;
    let mut event_evidence = BTreeMap::new();

    while read_rprov_event_line(
        &mut reader,
        &mut line,
        &mut raw_hasher,
        &mut raw_bytes,
        segment.events.byte_length,
    )? {
        if line.is_empty() {
            return Err(RprovError::invalid(
                "events.jsonl framing",
                "empty records and repeated LF are forbidden",
            ));
        }
        let sequence = u64::try_from(event_hashes.len())
            .ok()
            .and_then(|count| count.checked_add(1))
            .ok_or(RprovError::ArithmeticOverflow {
                field: "event sequence",
            })?;
        if sequence > MAX_RPROV_EVENTS {
            return Err(RprovError::LimitExceeded {
                field: "segment event count",
                actual: sequence,
                maximum: MAX_RPROV_EVENTS,
            });
        }
        let envelope = decode_rprov_event(&line)?;
        if encode_envelope(&envelope)
            .map_err(|error| RprovError::invalid("events.jsonl envelope", error.to_string()))?
            != line.as_slice()
        {
            return Err(RprovError::invalid(
                "events.jsonl envelope",
                "is not the unchanged canonical event encoding",
            ));
        }
        if envelope.session_id != segment.session_id {
            return Err(RprovError::EventIntegrity {
                kind: RprovEventIntegrityKind::Identity,
                segment: segment.ordinal,
                sequence: envelope.sequence,
                detail: "session must match the owning segment".to_owned(),
            });
        }
        if envelope.sequence != sequence {
            return Err(RprovError::EventIntegrity {
                kind: RprovEventIntegrityKind::Sequence,
                segment: segment.ordinal,
                sequence: envelope.sequence,
                detail: "sequence must match the exact array order".to_owned(),
            });
        }
        if envelope.previous_event_hash != previous_hash
            || compute_event_hash(previous_hash, &envelope)
                .map_err(|error| RprovError::invalid("events.jsonl chain", error.to_string()))?
                != envelope.event_hash
        {
            return Err(RprovError::EventIntegrity {
                kind: RprovEventIntegrityKind::Chain,
                segment: segment.ordinal,
                sequence: envelope.sequence,
                detail: "previous or event hash is not the unchanged linear chain".to_owned(),
            });
        }
        validate_stream_event(
            segment,
            &envelope,
            &event_hashes,
            &links,
            &recovery_gaps,
            &mut seen_links,
            &mut seen_copies,
            &mut seen_source_gaps,
            &checkpoint_by_sequence,
            &mut seen_checkpoints,
            &mut event_evidence,
        )?;
        previous_hash = envelope.event_hash;
        event_hashes.push(envelope.event_hash);
        last_event = Some(envelope);
    }

    if raw_bytes != segment.events.byte_length
        || Hash::from_bytes(*raw_hasher.finalize().as_bytes()) != segment.events.blake3
    {
        return Err(RprovError::invalid(
            "segments.events",
            "byte length or raw digest disagrees with the payload",
        ));
    }
    if event_hashes.is_empty() {
        return Err(RprovError::invalid(
            "events.jsonl",
            "must contain at least one envelope",
        ));
    }

    let actual_count = event_hashes.len() as u64;
    if actual_count != segment.inclusive_event_count {
        return Err(RprovError::invalid(
            "segments.inclusive_event_count",
            "does not equal the exact framed envelope count",
        ));
    }
    let last = last_event.unwrap();
    if last.event_hash != segment.last_event_hash {
        return Err(RprovError::invalid(
            "segments.last_event_hash",
            "does not equal the final included envelope hash",
        ));
    }
    if seen_checkpoints.len() != segment.checkpoints.len() {
        return Err(RprovError::invalid(
            "segments.checkpoints",
            "a declared full checkpoint has no exact owning event",
        ));
    }
    if seen_links.iter().any(|seen| !seen) {
        return Err(RprovError::invalid(
            "segments.source_links",
            "a source-link declaration has no exact matching event",
        ));
    }
    for (index, link) in segment.source_links.iter().enumerate() {
        if matches!(link, RprovSourceLink::InternalPaste { .. }) && !seen_copies[index] {
            return Err(RprovError::invalid(
                "segments.source_links",
                "an internal-paste declaration has no exact ClipboardCopied source event",
            ));
        }
    }
    for reference in recovery_gaps.missing_source.values() {
        validate_resolved_reference(reference, segment, &event_hashes)?;
    }
    if seen_source_gaps.len() != recovery_gaps.missing_source.len() {
        return Err(RprovError::invalid(
            "package_state.gaps",
            "a missing-source-link gap has no exact paste event",
        ));
    }
    for reference in segment.metadata.iter().map(|item| &item.owner) {
        validate_resolved_reference(reference, segment, &event_hashes)?;
    }
    let mut declared_evidence = HashMap::new();
    for evidence in &segment.evidence {
        for usage in &evidence.usages {
            validate_resolved_reference(usage, segment, &event_hashes)?;
            if declared_evidence
                .insert(usage.sequence, evidence.blake3)
                .is_some()
            {
                return Err(RprovError::invalid(
                    "segments.evidence.usages",
                    "an event must have exactly one evidence usage",
                ));
            }
            if event_evidence.get(&usage.sequence) != Some(&evidence.blake3) {
                return Err(RprovError::invalid(
                    "segments.evidence.usages",
                    "must be exact events that reference this evidence digest",
                ));
            }
        }
    }
    for (sequence, (reference, digest)) in &recovery_gaps.missing_evidence {
        validate_resolved_reference(reference, segment, &event_hashes)?;
        if event_evidence.get(sequence) != Some(digest) {
            return Err(RprovError::invalid(
                "package_state.gaps",
                "a missing-evidence gap must match an exact evidence-bearing event and digest",
            ));
        }
        if declared_evidence.contains_key(sequence)
            || segment
                .evidence
                .iter()
                .any(|evidence| evidence.blake3 == *digest)
        {
            return Err(RprovError::invalid(
                "package_state.gaps",
                "a missing-evidence gap contradicts retained evidence",
            ));
        }
    }
    let mut missing_external_evidence = None;
    for (sequence, evidence_hash) in &event_evidence {
        let retained = declared_evidence.get(sequence) == Some(evidence_hash);
        let missing = recovery_gaps
            .missing_evidence
            .get(sequence)
            .is_some_and(|(_, digest)| digest == evidence_hash);
        if !retained && !missing {
            missing_external_evidence.get_or_insert_with(|| RprovError::EventIntegrity {
                kind: RprovEventIntegrityKind::MissingExternalEvidence,
                segment: segment.ordinal,
                sequence: *sequence,
                detail: "an evidence-bearing event has no retained evidence artifact".to_owned(),
            });
        }
        if retained && missing {
            return Err(RprovError::invalid(
                "segments.evidence",
                "retained evidence contradicts an explicit missing-evidence gap",
            ));
        }
    }

    let remaining_error =
        validate_event_stream_termination(segment, package_state, is_tip, &last, actual_count)
            .err();
    Ok(RprovEventStreamReview {
        missing_external_evidence,
        remaining_error,
    })
}

fn validate_event_stream_termination(
    segment: &RprovSegment,
    package_state: &RprovPackageState,
    is_tip: bool,
    last: &EventEnvelope,
    actual_count: u64,
) -> Result<(), RprovError> {
    match segment.events.completeness {
        RprovEventStreamCompleteness::Complete => {
            let Event::SubmissionFinalized(finalized) = &last.event else {
                return Err(RprovError::invalid(
                    "events.jsonl terminal",
                    "a complete segment must end in SubmissionFinalized",
                ));
            };
            if finalized.event_count != actual_count
                || segment.terminal_event_hash.known() != Some(&last.event_hash)
                || segment.final_tree_hash.known() != Some(&finalized.final_workspace_hash)
            {
                return Err(RprovError::invalid(
                    "events.jsonl terminal",
                    "inclusive count, terminal hash, or final tree disagrees",
                ));
            }
            if !is_tip && !finalized.clean {
                return Err(RprovError::invalid(
                    "events.jsonl terminal",
                    "a retained non-tip segment requires SubmissionFinalized.clean=true",
                ));
            }
            if is_tip {
                match package_state {
                    RprovPackageState::CleanFinalized if !finalized.clean => {
                        return Err(RprovError::invalid(
                            "events.jsonl terminal",
                            "a clean package requires SubmissionFinalized.clean=true",
                        ));
                    }
                    RprovPackageState::RecoveryIncomplete {
                        unavailable_assurances,
                        ..
                    } => {
                        let clean_finalization_unavailable = unavailable_assurances
                            .binary_search(&RprovUnavailableAssurance::CleanFinalization)
                            .is_ok();
                        if clean_finalization_unavailable == finalized.clean {
                            return Err(RprovError::invalid(
                                "unavailable_assurances",
                                "CleanFinalization must correspond exactly to SubmissionFinalized.clean=false",
                            ));
                        }
                    }
                    RprovPackageState::CleanFinalized => {}
                }
            }
        }
        RprovEventStreamCompleteness::PrefixOnly => {
            if matches!(last.event, Event::SubmissionFinalized(_)) {
                return Err(RprovError::invalid(
                    "events.jsonl terminal",
                    "a prefix-only stream cannot include SubmissionFinalized",
                ));
            }
            if segment.terminal_event_hash.known().is_some() {
                return Err(RprovError::invalid(
                    "segments.terminal_event_hash",
                    "a prefix-only stream cannot claim a terminal event",
                ));
            }
        }
    }
    Ok(())
}

fn read_rprov_event_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    hasher: &mut blake3::Hasher,
    raw_bytes: &mut u64,
    declared_bytes: u64,
) -> Result<bool, RprovError> {
    line.clear();
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(RprovError::invalid(
                    "events.jsonl input",
                    format!("read failed with {}", error.kind()),
                ));
            }
        };
        if available.is_empty() {
            if line.is_empty() {
                return Ok(false);
            }
            return Err(RprovError::invalid(
                "events.jsonl framing",
                "every envelope, including the last, requires exactly one LF",
            ));
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let content_bytes = newline.unwrap_or(available.len());
        let consumed = content_bytes + usize::from(newline.is_some());
        let attempted_line =
            line.len()
                .checked_add(content_bytes)
                .ok_or(RprovError::ArithmeticOverflow {
                    field: "event envelope bytes",
                })?;
        if attempted_line > crate::MAX_ENVELOPE_BYTES {
            return Err(RprovError::LimitExceeded {
                field: "event envelope bytes",
                actual: attempted_line as u64,
                maximum: crate::MAX_ENVELOPE_BYTES as u64,
            });
        }
        let attempted_raw =
            raw_bytes
                .checked_add(consumed as u64)
                .ok_or(RprovError::ArithmeticOverflow {
                    field: "event stream bytes",
                })?;
        if attempted_raw > declared_bytes {
            return Err(RprovError::invalid(
                "segments.events",
                "payload contains bytes beyond its declared byte_length",
            ));
        }

        hasher.update(&available[..consumed]);
        line.extend_from_slice(&available[..content_bytes]);
        *raw_bytes = attempted_raw;
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(true);
        }
    }
}

fn decode_rprov_event(line: &[u8]) -> Result<EventEnvelope, RprovError> {
    match decode_envelope(line, DecodePolicy::RejectUnsupported) {
        Ok(DecodeOutcome::Decoded(envelope)) => Ok(envelope),
        Ok(DecodeOutcome::Skipped(_)) => Err(RprovError::invalid(
            "events.jsonl envelope",
            "unsupported event was not rejected",
        )),
        Err(DecodeError::UnsupportedVersion { found, supported }) => {
            Err(RprovError::UnsupportedInnerVersion {
                layer: "event",
                found,
                supported,
            })
        }
        Err(error) => Err(RprovError::invalid(
            "events.jsonl envelope",
            error.to_string(),
        )),
    }
}

struct EventLinkIndex {
    internal_by_paste: HashMap<u64, usize>,
    internal_by_copy: HashMap<u64, Vec<usize>>,
    legacy_by_event: HashMap<u64, usize>,
}

struct EventRecoveryGapIndex<'a> {
    missing_evidence: HashMap<u64, (&'a RecordedEventRef, Hash)>,
    missing_source: HashMap<u64, &'a RecordedEventRef>,
}

impl<'a> EventRecoveryGapIndex<'a> {
    fn new(
        package_state: &'a RprovPackageState,
        segment: &RprovSegment,
    ) -> Result<Self, RprovError> {
        let mut result = Self {
            missing_evidence: HashMap::new(),
            missing_source: HashMap::new(),
        };
        let mut missing_evidence_usages = HashMap::new();
        let RprovPackageState::RecoveryIncomplete { gaps, .. } = package_state else {
            return Ok(result);
        };
        for gap in gaps {
            match gap {
                RprovRecoveryGap::MissingEvidence { event, blake3 }
                    if event.session_id == segment.session_id =>
                {
                    validate_owner(segment, event, "package_state.gaps.event")?;
                    let count = missing_evidence_usages.entry(*blake3).or_insert(0_usize);
                    *count = count.checked_add(1).ok_or(RprovError::ArithmeticOverflow {
                        field: "missing evidence usages per artifact",
                    })?;
                    if *count > MAX_RPROV_EVIDENCE_USAGES_PER_ARTIFACT {
                        return Err(RprovError::limit(
                            "missing evidence usages per artifact",
                            *count,
                            MAX_RPROV_EVIDENCE_USAGES_PER_ARTIFACT,
                        ));
                    }
                    if result
                        .missing_evidence
                        .insert(event.sequence, (event, *blake3))
                        .is_some()
                    {
                        return Err(RprovError::invalid(
                            "package_state.gaps",
                            "an event has duplicate missing-evidence gaps",
                        ));
                    }
                }
                RprovRecoveryGap::MissingSourceLink { event }
                    if event.session_id == segment.session_id =>
                {
                    validate_owner(segment, event, "package_state.gaps.event")?;
                    if result
                        .missing_source
                        .insert(event.sequence, event)
                        .is_some()
                    {
                        return Err(RprovError::invalid(
                            "package_state.gaps",
                            "an event has duplicate missing-source-link gaps",
                        ));
                    }
                }
                _ => {}
            }
        }
        Ok(result)
    }
}

impl EventLinkIndex {
    fn new(segment: &RprovSegment) -> Result<Self, RprovError> {
        validate_source_links(segment)?;
        let mut result = Self {
            internal_by_paste: HashMap::new(),
            internal_by_copy: HashMap::new(),
            legacy_by_event: HashMap::new(),
        };
        for (index, link) in segment.source_links.iter().enumerate() {
            validate_source_link_shape(segment, link)?;
            match link {
                RprovSourceLink::InternalPaste {
                    paste_event,
                    copied_event,
                    ..
                } => {
                    if result
                        .internal_by_paste
                        .insert(paste_event.sequence, index)
                        .is_some()
                    {
                        return Err(RprovError::invalid(
                            "segments.source_links",
                            "an internal paste event has duplicate declarations",
                        ));
                    }
                    result
                        .internal_by_copy
                        .entry(copied_event.sequence)
                        .or_default()
                        .push(index);
                }
                RprovSourceLink::LegacyPaste { event, .. } => {
                    if result
                        .legacy_by_event
                        .insert(event.sequence, index)
                        .is_some()
                    {
                        return Err(RprovError::invalid(
                            "segments.source_links",
                            "a legacy paste event has duplicate declarations",
                        ));
                    }
                }
            }
        }
        Ok(result)
    }
}

fn checkpoint_index(
    segment: &RprovSegment,
) -> Result<HashMap<u64, &RprovCheckpointRef>, RprovError> {
    let mut result = HashMap::with_capacity(segment.checkpoints.len());
    for checkpoint in &segment.checkpoints {
        if result
            .insert(checkpoint.owner.sequence, checkpoint)
            .is_some()
        {
            return Err(RprovError::invalid(
                "segments.checkpoints",
                "checkpoint owner sequences must be unique",
            ));
        }
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn validate_stream_event(
    segment: &RprovSegment,
    envelope: &EventEnvelope,
    prior_hashes: &[Hash],
    links: &EventLinkIndex,
    recovery_gaps: &EventRecoveryGapIndex<'_>,
    seen_links: &mut [bool],
    seen_copies: &mut [bool],
    seen_source_gaps: &mut HashSet<u64>,
    checkpoints: &HashMap<u64, &RprovCheckpointRef>,
    seen_checkpoints: &mut HashSet<u64>,
    event_evidence: &mut BTreeMap<u64, Hash>,
) -> Result<(), RprovError> {
    if envelope.sequence == 1 {
        let Event::WorkspaceCheckpoint(initial) = &envelope.event else {
            return Err(RprovError::invalid(
                "events.jsonl genesis",
                "sequence 1 must be the initial WorkspaceCheckpoint",
            ));
        };
        let declaration = checkpoints.get(&1).ok_or_else(|| {
            RprovError::invalid(
                "events.jsonl genesis",
                "sequence 1 requires its exact initial checkpoint reference",
            )
        })?;
        if declaration.role != RprovCheckpointRole::Initial
            || declaration.owner.event_hash != envelope.event_hash
            || initial.workspace_hash != segment.initial_tree_hash
            || declaration.workspace_hash != segment.initial_tree_hash
        {
            return Err(RprovError::invalid(
                "events.jsonl genesis",
                "initial checkpoint event, payload reference, and segment tree must agree",
            ));
        }
    }
    if matches!(envelope.event, Event::SubmissionFinalized(_))
        && envelope.sequence != segment.inclusive_event_count
    {
        return Err(RprovError::invalid(
            "events.jsonl terminal",
            "events after SubmissionFinalized are forbidden",
        ));
    }

    match &envelope.event {
        Event::WorkspaceCheckpoint(checkpoint) => {
            let Some(declaration) = checkpoints.get(&envelope.sequence) else {
                return Err(RprovError::invalid(
                    "segments.checkpoints",
                    "a workspace checkpoint event lacks a full checkpoint reference",
                ));
            };
            if declaration.owner.event_hash != envelope.event_hash
                || declaration.workspace_hash != checkpoint.workspace_hash
            {
                return Err(RprovError::invalid(
                    "segments.checkpoints.owner",
                    "event hash or workspace hash disagrees with the owning event",
                ));
            }
            seen_checkpoints.insert(envelope.sequence);
        }
        Event::ClipboardCopied(source) => {
            validate_prior_reference(&source.prefix, segment, prior_hashes)?;
            if let Some(indices) = links.internal_by_copy.get(&envelope.sequence) {
                for index in indices {
                    let RprovSourceLink::InternalPaste {
                        copied_event,
                        document_id,
                        path,
                        version,
                        content_hash,
                        start_byte,
                        end_byte,
                        ..
                    } = &segment.source_links[*index]
                    else {
                        unreachable!()
                    };
                    if copied_event.event_hash != envelope.event_hash
                        || *document_id != source.document_id
                        || *path != source.path
                        || *version != source.version
                        || *content_hash != source.content_hash
                        || *start_byte != source.start_byte
                        || *end_byte != source.end_byte
                    {
                        return Err(RprovError::invalid(
                            "segments.source_links",
                            "copied event or source document/version/range metadata disagrees",
                        ));
                    }
                    seen_copies[*index] = true;
                }
            }
        }
        Event::InternalPaste(paste) => {
            let declared = links.internal_by_paste.get(&envelope.sequence).copied();
            let missing = recovery_gaps.missing_source.get(&envelope.sequence);
            if declared.is_some() && missing.is_some() {
                return Err(RprovError::invalid(
                    "package_state.gaps",
                    "a missing-source-link gap contradicts a retained source link",
                ));
            }
            if let Some(reference) = missing {
                if reference.event_hash != envelope.event_hash {
                    return Err(RprovError::invalid(
                        "package_state.gaps",
                        "missing-source-link event hash disagrees",
                    ));
                }
                validate_prior_reference(&paste.source, segment, prior_hashes)?;
                seen_source_gaps.insert(envelope.sequence);
                return Ok(());
            }
            let Some(index) = declared else {
                return Err(RprovError::invalid(
                    "segments.source_links",
                    "an InternalPaste event lacks its required source declaration",
                ));
            };
            let RprovSourceLink::InternalPaste {
                paste_event,
                copied_event,
                ..
            } = &segment.source_links[index]
            else {
                unreachable!()
            };
            if paste_event.event_hash != envelope.event_hash || copied_event != &paste.source {
                return Err(RprovError::invalid(
                    "segments.source_links",
                    "paste event or required copied-event reference disagrees",
                ));
            }
            validate_prior_reference(copied_event, segment, prior_hashes)?;
            seen_links[index] = true;
        }
        Event::FileEdited(transaction) if transaction.origin == EditOrigin::Paste => {
            let declared = links.legacy_by_event.get(&envelope.sequence).copied();
            let missing = recovery_gaps.missing_source.get(&envelope.sequence);
            if declared.is_some() && missing.is_some() {
                return Err(RprovError::invalid(
                    "package_state.gaps",
                    "a missing-source-link gap contradicts a retained source link",
                ));
            }
            if let Some(reference) = missing {
                if reference.event_hash != envelope.event_hash {
                    return Err(RprovError::invalid(
                        "package_state.gaps",
                        "missing-source-link event hash disagrees",
                    ));
                }
                seen_source_gaps.insert(envelope.sequence);
                return Ok(());
            }
            let Some(index) = declared else {
                return Err(RprovError::invalid(
                    "segments.source_links",
                    "a legacy Paste event requires an origin-unverified marker",
                ));
            };
            let RprovSourceLink::LegacyPaste { event, .. } = &segment.source_links[index] else {
                unreachable!()
            };
            if event.event_hash != envelope.event_hash {
                return Err(RprovError::invalid(
                    "segments.source_links",
                    "legacy paste marker event hash disagrees",
                ));
            }
            seen_links[index] = true;
        }
        Event::ExternalObservation(observation) => {
            event_evidence.insert(envelope.sequence, observation.evidence_hash);
        }
        Event::RecoveryRecorded(recovery) => {
            event_evidence.insert(envelope.sequence, recovery.evidence_hash);
        }
        _ => {}
    }
    Ok(())
}

fn validate_prior_reference(
    reference: &RecordedEventRef,
    segment: &RprovSegment,
    prior_hashes: &[Hash],
) -> Result<(), RprovError> {
    if reference.session_id != segment.session_id || reference.sequence == 0 {
        return Err(RprovError::invalid(
            "recorded event reference",
            "must name the same segment and an earlier event",
        ));
    }
    let index = usize::try_from(reference.sequence - 1).map_err(|_| {
        RprovError::invalid("recorded event reference", "sequence does not fit usize")
    })?;
    if prior_hashes.get(index) != Some(&reference.event_hash) {
        return Err(RprovError::invalid(
            "recorded event reference",
            "does not resolve to the exact included prefix",
        ));
    }
    Ok(())
}

fn validate_resolved_reference(
    reference: &RecordedEventRef,
    segment: &RprovSegment,
    hashes: &[Hash],
) -> Result<(), RprovError> {
    if reference.session_id != segment.session_id || reference.sequence == 0 {
        return Err(RprovError::invalid(
            "recorded event reference",
            "does not belong to the segment",
        ));
    }
    let index = usize::try_from(reference.sequence - 1).map_err(|_| {
        RprovError::invalid("recorded event reference", "sequence does not fit usize")
    })?;
    if hashes.get(index) != Some(&reference.event_hash) {
        return Err(RprovError::invalid(
            "recorded event reference",
            "does not resolve to an exact included event",
        ));
    }
    Ok(())
}

/// Validates the deterministic container layout against canonical manifest
/// bytes and metadata for every already-enumerated record.
///
/// T8.1 remains responsible for safely producing this bounded in-memory view
/// from a hostile file. This function performs no I/O and follows no links.
pub fn validate_rprov_layout(
    header: &RprovContainerHeader,
    manifest_bytes: &[u8],
    manifest: &RprovManifest,
    entries: &[RprovArchiveEntry],
) -> Result<(), RprovError> {
    validate_container_header(header)?;
    manifest.validate()?;
    if encode_rprov_manifest(manifest)? != manifest_bytes {
        return Err(RprovError::NonCanonicalManifest);
    }
    if entries.len() > MAX_RPROV_ARCHIVE_ENTRIES {
        return Err(RprovError::limit(
            "archive entries",
            entries.len(),
            MAX_RPROV_ARCHIVE_ENTRIES,
        ));
    }
    if usize::try_from(header.entry_count).ok() != Some(entries.len()) {
        return Err(RprovError::invalid(
            "entry_count",
            "does not equal the enumerated record count",
        ));
    }
    let Some(first) = entries.first() else {
        return Err(RprovError::invalid(
            "entries",
            "manifest.json must be the first record",
        ));
    };
    if first.path != "manifest.json"
        || first.entry_type != RprovArchiveEntryType::RegularFile
        || first.byte_length != manifest_bytes.len() as u64
        || first.blake3.is_some()
    {
        return Err(RprovError::invalid(
            "manifest record",
            "must be first, regular, exact-length, and unhashed",
        ));
    }
    if entries.len() != manifest.inventory.len() + 1 {
        return Err(RprovError::invalid(
            "entries",
            "must contain exactly manifest.json and every inventoried payload",
        ));
    }

    let mut total = 0_u64;
    let mut previous: Option<&str> = None;
    for (index, entry) in entries.iter().enumerate() {
        validate_rprov_archive_path(&entry.path)?;
        if entry.entry_type != RprovArchiveEntryType::RegularFile {
            return Err(RprovError::invalid(
                "entry type",
                "version 1 permits only regular files",
            ));
        }
        require_max(
            "record payload bytes",
            entry.byte_length,
            MAX_RPROV_RECORD_PAYLOAD_BYTES,
        )?;
        if index > 0 {
            if previous.is_some_and(|path| path >= entry.path.as_str()) {
                return Err(RprovError::invalid(
                    "entry order",
                    "payload paths must be unique and strictly sorted",
                ));
            }
            previous = Some(&entry.path);
        }
        total = checked_add("record bytes", total, RPROV_RECORD_HEADER_BYTES as u64)?;
        total = checked_add("record bytes", total, entry.path.len() as u64)?;
        total = checked_add("record bytes", total, entry.byte_length)?;

        if index > 0 {
            let expected = &manifest.inventory[index - 1];
            if entry.path != expected.path
                || entry.byte_length != expected.byte_length
                || entry.blake3 != Some(expected.blake3)
            {
                return Err(RprovError::invalid(
                    "payload record",
                    "path, length, or digest disagrees with manifest inventory",
                ));
            }
        }
    }
    if header.stored_records_bytes != total || header.expanded_records_bytes != total {
        return Err(RprovError::invalid(
            "records bytes",
            "header lengths do not equal checked record framing and payload bytes",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RprovError {
    UnsupportedFormatVersion {
        found: u32,
        supported: u32,
    },
    UnsupportedInnerVersion {
        layer: &'static str,
        found: u32,
        supported: u32,
    },
    ManifestTooLarge {
        actual: usize,
        maximum: usize,
    },
    Truncated {
        layer: &'static str,
    },
    InvalidMagic,
    InvalidManifest {
        detail: String,
    },
    NonCanonicalManifest,
    InvalidField {
        field: &'static str,
        detail: String,
    },
    EventIntegrity {
        kind: RprovEventIntegrityKind,
        segment: u32,
        sequence: u64,
        detail: String,
    },
    LimitExceeded {
        field: &'static str,
        actual: u64,
        maximum: u64,
    },
    ArithmeticOverflow {
        field: &'static str,
    },
}

impl fmt::Display for RprovError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormatVersion { found, supported } => write!(
                formatter,
                ".rprov format version {found} is unsupported; supported version is {supported}"
            ),
            Self::UnsupportedInnerVersion {
                layer,
                found,
                supported,
            } => write!(
                formatter,
                ".rprov {layer} format version {found} is unsupported; supported version is {supported}"
            ),
            Self::ManifestTooLarge { actual, maximum } => write!(
                formatter,
                "manifest.json is {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::Truncated { layer } => write!(formatter, "truncated .rprov {layer}"),
            Self::InvalidMagic => formatter.write_str("invalid .rprov magic"),
            Self::InvalidManifest { detail } => {
                write!(formatter, "invalid manifest.json: {detail}")
            }
            Self::NonCanonicalManifest => {
                formatter.write_str("manifest.json is not the canonical version 1 encoding")
            }
            Self::InvalidField { field, detail } => {
                write!(formatter, "invalid .rprov field {field}: {detail}")
            }
            Self::EventIntegrity {
                kind,
                segment,
                sequence,
                detail,
            } => {
                let kind = match kind {
                    RprovEventIntegrityKind::Chain => "chain",
                    RprovEventIntegrityKind::Identity => "session identity",
                    RprovEventIntegrityKind::Sequence => "sequence",
                    RprovEventIntegrityKind::FinalTreeBinding => "final tree binding",
                    RprovEventIntegrityKind::MissingExternalEvidence => "missing external evidence",
                };
                write!(
                    formatter,
                    "invalid .rprov event {kind} at segment {segment} sequence {sequence}: {detail}"
                )
            }
            Self::LimitExceeded {
                field,
                actual,
                maximum,
            } => write!(
                formatter,
                ".rprov {field} is {actual}; maximum is {maximum}"
            ),
            Self::ArithmeticOverflow { field } => {
                write!(
                    formatter,
                    ".rprov arithmetic overflow while totaling {field}"
                )
            }
        }
    }
}

impl Error for RprovError {}

impl RprovError {
    fn invalid(field: &'static str, detail: impl Into<String>) -> Self {
        Self::InvalidField {
            field,
            detail: detail.into(),
        }
    }

    fn limit(field: &'static str, actual: usize, maximum: usize) -> Self {
        Self::LimitExceeded {
            field,
            actual: actual as u64,
            maximum: maximum as u64,
        }
    }
}

pub fn encode_rprov_container_header(
    header: &RprovContainerHeader,
) -> Result<[u8; RPROV_CONTAINER_HEADER_BYTES], RprovError> {
    validate_container_header(header)?;
    let mut bytes = [0_u8; RPROV_CONTAINER_HEADER_BYTES];
    bytes[..8].copy_from_slice(RPROV_MAGIC);
    bytes[8..12].copy_from_slice(&header.format_version.to_le_bytes());
    bytes[12..14].copy_from_slice(&(RPROV_CONTAINER_HEADER_BYTES as u16).to_le_bytes());
    bytes[14] = RPROV_COMPRESSION_NONE;
    bytes[16..20].copy_from_slice(&header.entry_count.to_le_bytes());
    bytes[24..32].copy_from_slice(&header.stored_records_bytes.to_le_bytes());
    bytes[32..40].copy_from_slice(&header.expanded_records_bytes.to_le_bytes());
    Ok(bytes)
}

pub fn decode_rprov_container_header(encoded: &[u8]) -> Result<RprovContainerHeader, RprovError> {
    if encoded.len() < 12 {
        return Err(RprovError::Truncated { layer: "header" });
    }
    if &encoded[..8] != RPROV_MAGIC {
        return Err(RprovError::InvalidMagic);
    }
    let format_version = u32::from_le_bytes(encoded[8..12].try_into().unwrap());
    require_version(format_version)?;
    if encoded.len() != RPROV_CONTAINER_HEADER_BYTES {
        return Err(RprovError::invalid(
            "container_header",
            format!(
                "must be exactly {RPROV_CONTAINER_HEADER_BYTES} bytes, found {}",
                encoded.len()
            ),
        ));
    }
    let header_bytes = u16::from_le_bytes(encoded[12..14].try_into().unwrap());
    if usize::from(header_bytes) != RPROV_CONTAINER_HEADER_BYTES {
        return Err(RprovError::invalid("header_bytes", "must equal 40"));
    }
    if encoded[14] != RPROV_COMPRESSION_NONE {
        return Err(RprovError::invalid(
            "compression",
            "version 1 requires none",
        ));
    }
    if encoded[15] != 0 || encoded[20..24] != [0; 4] {
        return Err(RprovError::invalid("reserved", "must be zero"));
    }
    let header = RprovContainerHeader {
        format_version,
        entry_count: u32::from_le_bytes(encoded[16..20].try_into().unwrap()),
        stored_records_bytes: u64::from_le_bytes(encoded[24..32].try_into().unwrap()),
        expanded_records_bytes: u64::from_le_bytes(encoded[32..40].try_into().unwrap()),
    };
    validate_container_header(&header)?;
    Ok(header)
}

fn validate_container_header(header: &RprovContainerHeader) -> Result<(), RprovError> {
    require_version(header.format_version)?;
    require_range(
        "archive entry count",
        u64::from(header.entry_count),
        1,
        MAX_RPROV_ARCHIVE_ENTRIES as u64,
    )?;
    let stored = header
        .stored_records_bytes
        .checked_add(RPROV_CONTAINER_HEADER_BYTES as u64)
        .ok_or(RprovError::ArithmeticOverflow {
            field: "stored container bytes",
        })?;
    if stored > MAX_RPROV_STORED_BYTES {
        return Err(RprovError::LimitExceeded {
            field: "stored container bytes",
            actual: stored,
            maximum: MAX_RPROV_STORED_BYTES,
        });
    }
    let expanded = header
        .expanded_records_bytes
        .checked_add(RPROV_CONTAINER_HEADER_BYTES as u64)
        .ok_or(RprovError::ArithmeticOverflow {
            field: "expanded container bytes",
        })?;
    if expanded > MAX_RPROV_EXPANDED_BYTES {
        return Err(RprovError::LimitExceeded {
            field: "expanded container bytes",
            actual: expanded,
            maximum: MAX_RPROV_EXPANDED_BYTES,
        });
    }
    if header.stored_records_bytes != header.expanded_records_bytes {
        return Err(RprovError::invalid(
            "expanded_records_bytes",
            "must equal stored_records_bytes for uncompressed version 1",
        ));
    }
    Ok(())
}

pub fn encode_rprov_record_header(
    header: &RprovRecordHeader,
) -> Result<[u8; RPROV_RECORD_HEADER_BYTES], RprovError> {
    validate_record_header(header)?;
    let mut bytes = [0_u8; RPROV_RECORD_HEADER_BYTES];
    bytes[..2].copy_from_slice(&header.path_bytes.to_le_bytes());
    bytes[2] = RPROV_RECORD_REGULAR_FILE;
    bytes[8..16].copy_from_slice(&header.payload_bytes.to_le_bytes());
    Ok(bytes)
}

pub fn decode_rprov_record_header(encoded: &[u8]) -> Result<RprovRecordHeader, RprovError> {
    if encoded.len() != RPROV_RECORD_HEADER_BYTES {
        return Err(RprovError::Truncated {
            layer: "record header",
        });
    }
    if encoded[2] != RPROV_RECORD_REGULAR_FILE {
        return Err(RprovError::invalid(
            "record_type",
            "version 1 permits only regular files",
        ));
    }
    if encoded[3] != 0 || encoded[4..8] != [0; 4] {
        return Err(RprovError::invalid("record reserved", "must be zero"));
    }
    let header = RprovRecordHeader {
        path_bytes: u16::from_le_bytes(encoded[..2].try_into().unwrap()),
        entry_type: RprovRecordType::RegularFile,
        payload_bytes: u64::from_le_bytes(encoded[8..16].try_into().unwrap()),
    };
    validate_record_header(&header)?;
    Ok(header)
}

fn validate_record_header(header: &RprovRecordHeader) -> Result<(), RprovError> {
    require_range(
        "record path bytes",
        u64::from(header.path_bytes),
        1,
        MAX_RPROV_ARCHIVE_PATH_BYTES as u64,
    )?;
    if header.payload_bytes > MAX_RPROV_RECORD_PAYLOAD_BYTES {
        return Err(RprovError::LimitExceeded {
            field: "record payload bytes",
            actual: header.payload_bytes,
            maximum: MAX_RPROV_RECORD_PAYLOAD_BYTES,
        });
    }
    Ok(())
}

pub fn validate_rprov_archive_path(path: &str) -> Result<(), RprovError> {
    if path.is_empty() {
        return Err(RprovError::invalid("archive path", "must not be empty"));
    }
    if path.len() > MAX_RPROV_ARCHIVE_PATH_BYTES {
        return Err(RprovError::limit(
            "archive path bytes",
            path.len(),
            MAX_RPROV_ARCHIVE_PATH_BYTES,
        ));
    }
    if !path.is_ascii() {
        return Err(RprovError::invalid(
            "archive path",
            "must use the version 1 ASCII grammar",
        ));
    }
    if path.starts_with('/') || path.contains('\\') || has_drive_prefix(path) {
        return Err(RprovError::invalid(
            "archive path",
            "must be slash-separated and archive-relative",
        ));
    }
    let mut depth = 0;
    for component in path.split('/') {
        if component.is_empty() || matches!(component, "." | "..") {
            return Err(RprovError::invalid(
                "archive path",
                "empty and dot components are forbidden",
            ));
        }
        if component.len() > MAX_RPROV_ARCHIVE_COMPONENT_BYTES {
            return Err(RprovError::limit(
                "archive path component bytes",
                component.len(),
                MAX_RPROV_ARCHIVE_COMPONENT_BYTES,
            ));
        }
        if !component.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        }) {
            return Err(RprovError::invalid(
                "archive path",
                "contains a character outside [a-z0-9._-]",
            ));
        }
        let lower = component.to_ascii_lowercase();
        if lower.ends_with(".rprov") || lower.ends_with(".zip") {
            return Err(RprovError::invalid(
                "archive path",
                "nested .rprov and ZIP names are forbidden",
            ));
        }
        depth += 1;
    }
    if depth > MAX_RPROV_ARCHIVE_PATH_DEPTH {
        return Err(RprovError::limit(
            "archive path depth",
            depth,
            MAX_RPROV_ARCHIVE_PATH_DEPTH,
        ));
    }
    if path == "final-workspace" || path.starts_with("final-workspace/") {
        return Err(RprovError::invalid(
            "archive path",
            "final-workspace is not part of version 1",
        ));
    }
    Ok(())
}

fn has_drive_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

pub fn encode_rprov_manifest(manifest: &RprovManifest) -> Result<Vec<u8>, RprovError> {
    manifest.validate()?;
    let encoded_length = serialize_manifest(CappedWriter::new(io::sink()), manifest)?.written + 1;
    if encoded_length > MAX_RPROV_MANIFEST_BYTES {
        return Err(RprovError::ManifestTooLarge {
            actual: encoded_length,
            maximum: MAX_RPROV_MANIFEST_BYTES,
        });
    }
    let mut writer = CappedWriter::new(Vec::with_capacity(encoded_length));
    serde_json::to_writer(&mut writer, manifest).map_err(|error| RprovError::InvalidManifest {
        detail: error.to_string(),
    })?;
    writer
        .write_all(b"\n")
        .map_err(|error| RprovError::InvalidManifest {
            detail: error.to_string(),
        })?;
    Ok(writer.into_inner())
}

fn serialize_manifest<W: Write>(
    mut writer: CappedWriter<W>,
    manifest: &RprovManifest,
) -> Result<CappedWriter<W>, RprovError> {
    serde_json::to_writer(&mut writer, manifest).map_err(|error| RprovError::InvalidManifest {
        detail: error.to_string(),
    })?;
    Ok(writer)
}

pub fn decode_rprov_manifest(encoded: &[u8]) -> Result<RprovManifest, RprovError> {
    if encoded.len() > MAX_RPROV_MANIFEST_BYTES {
        return Err(RprovError::ManifestTooLarge {
            actual: encoded.len(),
            maximum: MAX_RPROV_MANIFEST_BYTES,
        });
    }
    let version = manifest_version_prefix(encoded)?;
    require_version(version)?;
    preflight_manifest_json(encoded)?;
    let wire: WireRprovManifest =
        serde_json::from_slice(encoded).map_err(|error| RprovError::InvalidManifest {
            detail: error.to_string(),
        })?;
    let manifest = RprovManifest::from(wire);
    manifest.validate()?;
    if encode_rprov_manifest(&manifest)? != encoded {
        return Err(RprovError::NonCanonicalManifest);
    }
    Ok(manifest)
}

fn manifest_version_prefix(encoded: &[u8]) -> Result<u32, RprovError> {
    const PREFIX: &[u8] = b"{\"format_version\":";
    if !encoded.starts_with(PREFIX) {
        return Err(RprovError::InvalidManifest {
            detail: "format_version must be the first canonical member".to_owned(),
        });
    }
    let remaining = &encoded[PREFIX.len()..];
    let digit_count = remaining
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digit_count == 0 || remaining.get(digit_count) != Some(&b',') {
        return Err(RprovError::InvalidManifest {
            detail: "format_version must be an unsigned decimal u32".to_owned(),
        });
    }
    let digits = std::str::from_utf8(&remaining[..digit_count]).map_err(|error| {
        RprovError::InvalidManifest {
            detail: error.to_string(),
        }
    })?;
    digits
        .parse::<u32>()
        .map_err(|_| RprovError::InvalidManifest {
            detail: "format_version must fit u32".to_owned(),
        })
}

fn require_version(version: u32) -> Result<(), RprovError> {
    if version == RPROV_FORMAT_VERSION_V1 {
        Ok(())
    } else {
        Err(RprovError::UnsupportedFormatVersion {
            found: version,
            supported: RPROV_FORMAT_VERSION_V1,
        })
    }
}

impl RprovManifest {
    pub fn validate(&self) -> Result<(), RprovError> {
        require_version(self.format_version)?;
        validate_identifier("course_id", &self.course_id)?;
        validate_identifier("assignment_id", &self.assignment_id)?;
        validate_identifier("assignment_version", &self.assignment_version)?;
        validate_identifier("student_id", &self.student_id)?;
        validate_producer(&self.producer)?;
        validate_assignment_manifest(&self.assignment_manifest)?;
        validate_package_state(&self.package_state)?;

        if self.initial_workspace.files.len() > MAX_RPROV_INITIAL_FILES {
            return Err(RprovError::limit(
                "initial workspace files",
                self.initial_workspace.files.len(),
                MAX_RPROV_INITIAL_FILES,
            ));
        }
        if self.segments.is_empty() {
            return Err(RprovError::invalid(
                "segments",
                "must contain at least one segment",
            ));
        }
        if self.segments.len() > MAX_RPROV_SEGMENTS {
            return Err(RprovError::limit(
                "segments",
                self.segments.len(),
                MAX_RPROV_SEGMENTS,
            ));
        }
        if self.inventory.len() + 1 > MAX_RPROV_ARCHIVE_ENTRIES {
            return Err(RprovError::limit(
                "archive entries",
                self.inventory.len() + 1,
                MAX_RPROV_ARCHIVE_ENTRIES,
            ));
        }
        if self.latest_session_id != self.segments.last().unwrap().session_id {
            return Err(RprovError::invalid(
                "latest_session_id",
                "must name the final segment",
            ));
        }

        let inventory = validate_inventory(&self.inventory)?;
        let mut references: HashMap<&str, usize> = self
            .inventory
            .iter()
            .map(|entry| (entry.path.as_str(), 0))
            .collect();
        validate_initial_workspace(&self.initial_workspace, &inventory, &mut references)?;

        let mut sessions = HashSet::with_capacity(self.segments.len());
        let mut event_count = 0_u64;
        let mut event_bytes = 0_u64;
        let mut checkpoint_bytes = 0_u64;
        let mut metadata_bytes = 0_u64;
        let mut evidence_bytes = 0_u64;
        let mut evidence_usages = 0_usize;
        let mut source_links = 0_usize;
        for (index, segment) in self.segments.iter().enumerate() {
            validate_segment(self, segment, index, &inventory, &mut references)?;
            if !sessions.insert(segment.session_id.clone()) {
                return Err(RprovError::invalid(
                    "segments.session_id",
                    "session IDs must be unique",
                ));
            }
            event_count = checked_add("event count", event_count, segment.inclusive_event_count)?;
            event_bytes = checked_add("events bytes", event_bytes, segment.events.byte_length)?;
            checkpoint_bytes = checked_add(
                "checkpoint bytes",
                checkpoint_bytes,
                checked_sum(
                    "segment checkpoint bytes",
                    segment.checkpoints.iter().map(|item| item.byte_length),
                )?,
            )?;
            metadata_bytes = checked_add(
                "metadata bytes",
                metadata_bytes,
                checked_sum(
                    "segment metadata bytes",
                    segment.metadata.iter().map(|item| item.byte_length),
                )?,
            )?;
            evidence_bytes = checked_add(
                "evidence bytes",
                evidence_bytes,
                checked_sum(
                    "segment evidence bytes",
                    segment.evidence.iter().map(|item| item.byte_length),
                )?,
            )?;
            for evidence in &segment.evidence {
                evidence_usages = evidence_usages.checked_add(evidence.usages.len()).ok_or(
                    RprovError::ArithmeticOverflow {
                        field: "evidence usages",
                    },
                )?;
            }
            source_links = source_links.checked_add(segment.source_links.len()).ok_or(
                RprovError::ArithmeticOverflow {
                    field: "source links",
                },
            )?;
        }
        let gap_counts = validate_recovery_gaps(self)?;
        evidence_usages = evidence_usages
            .checked_add(gap_counts.missing_evidence)
            .ok_or(RprovError::ArithmeticOverflow {
                field: "evidence usages",
            })?;
        source_links = source_links
            .checked_add(gap_counts.missing_source_links)
            .ok_or(RprovError::ArithmeticOverflow {
                field: "source links",
            })?;
        require_max("aggregate event count", event_count, MAX_RPROV_EVENTS)?;
        if event_count != self.aggregate_event_count {
            return Err(RprovError::invalid(
                "aggregate_event_count",
                "must equal the checked sum of segment inclusive counts",
            ));
        }
        require_max(
            "aggregate events bytes",
            event_bytes,
            MAX_RPROV_EVENTS_BYTES,
        )?;
        require_max(
            "aggregate checkpoint bytes",
            checkpoint_bytes,
            MAX_RPROV_CHECKPOINT_BYTES,
        )?;
        require_max(
            "aggregate metadata bytes",
            metadata_bytes,
            MAX_RPROV_METADATA_BYTES,
        )?;
        require_max(
            "aggregate evidence bytes",
            evidence_bytes,
            MAX_RPROV_EVIDENCE_BYTES,
        )?;
        if evidence_usages > MAX_RPROV_EVIDENCE_USAGES {
            return Err(RprovError::limit(
                "aggregate evidence usages",
                evidence_usages,
                MAX_RPROV_EVIDENCE_USAGES,
            ));
        }
        if source_links > MAX_RPROV_SOURCE_LINKS {
            return Err(RprovError::limit(
                "aggregate source links",
                source_links,
                MAX_RPROV_SOURCE_LINKS,
            ));
        }
        if references.values().any(|count| *count == 0) {
            return Err(RprovError::invalid(
                "inventory",
                "contains an unreferenced payload entry",
            ));
        }

        if self.final_tree_hash != self.segments.last().unwrap().final_tree_hash {
            return Err(RprovError::invalid(
                "final_tree_hash",
                "must have the same known or unknown value as the final segment",
            ));
        }

        match &self.package_state {
            RprovPackageState::CleanFinalized => {
                self.final_tree_hash.known().ok_or_else(|| {
                    RprovError::invalid("final_tree_hash", "clean package requires a known hash")
                })?;
            }
            RprovPackageState::RecoveryIncomplete {
                unavailable_assurances,
                ..
            } => validate_recovery_assurances(self, unavailable_assurances)?,
        }
        Ok(())
    }
}

fn validate_recovery_assurances(
    manifest: &RprovManifest,
    unavailable: &[RprovUnavailableAssurance],
) -> Result<(), RprovError> {
    let tip = manifest.segments.last().unwrap();
    let manifest_visible = [
        (
            tip.events.completeness == RprovEventStreamCompleteness::PrefixOnly,
            RprovUnavailableAssurance::CompleteEventStream,
        ),
        (
            tip.checkpoints.last().map(|item| item.role) != Some(RprovCheckpointRole::Final),
            RprovUnavailableAssurance::FinalCheckpoint,
        ),
        (
            tip.final_tree_hash.known().is_none() || manifest.final_tree_hash.known().is_none(),
            RprovUnavailableAssurance::FinalTree,
        ),
    ];
    for (is_unavailable, assurance) in manifest_visible {
        if unavailable.binary_search(&assurance).is_ok() != is_unavailable {
            return Err(RprovError::invalid(
                "unavailable_assurances",
                format!("{assurance:?} must correspond exactly to the declared tip"),
            ));
        }
    }
    if tip.events.completeness == RprovEventStreamCompleteness::PrefixOnly
        && unavailable
            .binary_search(&RprovUnavailableAssurance::CleanFinalization)
            .is_err()
    {
        return Err(RprovError::invalid(
            "unavailable_assurances",
            "a prefix without SubmissionFinalized must identify unavailable CleanFinalization",
        ));
    }
    Ok(())
}

fn validate_package_state(state: &RprovPackageState) -> Result<(), RprovError> {
    if let RprovPackageState::RecoveryIncomplete {
        unavailable_assurances,
        gaps,
    } = state
    {
        if unavailable_assurances.is_empty() {
            return Err(RprovError::invalid(
                "unavailable_assurances",
                "recovery packages must identify at least one unavailable assurance",
            ));
        }
        if unavailable_assurances.len() > MAX_RPROV_UNAVAILABLE_ASSURANCES {
            return Err(RprovError::limit(
                "unavailable assurances",
                unavailable_assurances.len(),
                MAX_RPROV_UNAVAILABLE_ASSURANCES,
            ));
        }
        for pair in unavailable_assurances.windows(2) {
            if pair[0] >= pair[1] {
                return Err(RprovError::invalid(
                    "unavailable_assurances",
                    "must be unique and in canonical enum order",
                ));
            }
        }
        if gaps.len() > MAX_RPROV_RECOVERY_GAPS {
            return Err(RprovError::limit(
                "recovery gaps",
                gaps.len(),
                MAX_RPROV_RECOVERY_GAPS,
            ));
        }
        for pair in gaps.windows(2) {
            if compare_recovery_gaps(&pair[0], &pair[1]) != Ordering::Less {
                return Err(RprovError::invalid(
                    "package_state.gaps",
                    "must be unique and in canonical kind, session, sequence, hash order",
                ));
            }
        }
        let gap_assurances = [
            (
                RprovUnavailableAssurance::CompleteAncestry,
                gaps.iter()
                    .any(|gap| matches!(gap, RprovRecoveryGap::MissingAncestry { .. })),
            ),
            (
                RprovUnavailableAssurance::ReferencedEvidence,
                gaps.iter()
                    .any(|gap| matches!(gap, RprovRecoveryGap::MissingEvidence { .. })),
            ),
            (
                RprovUnavailableAssurance::SourceLinkIntegrity,
                gaps.iter()
                    .any(|gap| matches!(gap, RprovRecoveryGap::MissingSourceLink { .. })),
            ),
        ];
        for (assurance, has_gap) in gap_assurances {
            if unavailable_assurances.binary_search(&assurance).is_ok() != has_gap {
                return Err(RprovError::invalid(
                    "package_state",
                    format!("{assurance:?} must correspond exactly to recovery gaps"),
                ));
            }
        }
    }
    Ok(())
}

fn compare_recovery_gaps(left: &RprovRecoveryGap, right: &RprovRecoveryGap) -> Ordering {
    let rank = |gap: &RprovRecoveryGap| match gap {
        RprovRecoveryGap::MissingAncestry { .. } => 0_u8,
        RprovRecoveryGap::MissingEvidence { .. } => 1,
        RprovRecoveryGap::MissingSourceLink { .. } => 2,
    };
    rank(left)
        .cmp(&rank(right))
        .then_with(|| match (left, right) {
            (
                RprovRecoveryGap::MissingAncestry {
                    before_session_id: left,
                },
                RprovRecoveryGap::MissingAncestry {
                    before_session_id: right,
                },
            ) => left.cmp(right),
            (
                RprovRecoveryGap::MissingEvidence {
                    event: left_event,
                    blake3: left_digest,
                },
                RprovRecoveryGap::MissingEvidence {
                    event: right_event,
                    blake3: right_digest,
                },
            ) => compare_event_refs(left_event, right_event).then(left_digest.cmp(right_digest)),
            (
                RprovRecoveryGap::MissingSourceLink { event: left },
                RprovRecoveryGap::MissingSourceLink { event: right },
            ) => compare_event_refs(left, right),
            _ => Ordering::Equal,
        })
}

fn compare_event_refs(left: &RecordedEventRef, right: &RecordedEventRef) -> Ordering {
    left.session_id
        .cmp(&right.session_id)
        .then(left.sequence.cmp(&right.sequence))
        .then(left.event_hash.cmp(&right.event_hash))
}

#[derive(Default)]
struct RecoveryGapCounts {
    missing_evidence: usize,
    missing_source_links: usize,
}

fn validate_recovery_gaps(manifest: &RprovManifest) -> Result<RecoveryGapCounts, RprovError> {
    let RprovPackageState::RecoveryIncomplete { gaps, .. } = &manifest.package_state else {
        return Ok(RecoveryGapCounts::default());
    };
    let mut counts = RecoveryGapCounts::default();
    let mut missing_evidence_events = HashSet::new();
    let mut missing_evidence_usages = HashMap::new();
    let mut missing_source_events = HashSet::new();
    for gap in gaps {
        match gap {
            RprovRecoveryGap::MissingAncestry { before_session_id } => {
                let (index, segment) = find_gap_segment(manifest, before_session_id)?;
                if segment.parent.is_some()
                    || (index == 0
                        && segment.initial_tree_hash == manifest.original_starter_tree_hash)
                {
                    return Err(RprovError::invalid(
                        "package_state.gaps",
                        "a missing-ancestry gap contradicts complete retained ancestry",
                    ));
                }
            }
            RprovRecoveryGap::MissingEvidence { event, blake3 } => {
                let (_, segment) = find_gap_segment(manifest, &event.session_id)?;
                validate_owner(segment, event, "package_state.gaps.event")?;
                if !missing_evidence_events.insert((event.session_id.clone(), event.sequence)) {
                    return Err(RprovError::invalid(
                        "package_state.gaps",
                        "an event has more than one missing-evidence gap",
                    ));
                }
                let count = missing_evidence_usages
                    .entry((event.session_id.clone(), *blake3))
                    .or_insert(0_usize);
                *count = count.checked_add(1).ok_or(RprovError::ArithmeticOverflow {
                    field: "missing evidence usages per artifact",
                })?;
                if *count > MAX_RPROV_EVIDENCE_USAGES_PER_ARTIFACT {
                    return Err(RprovError::limit(
                        "missing evidence usages per artifact",
                        *count,
                        MAX_RPROV_EVIDENCE_USAGES_PER_ARTIFACT,
                    ));
                }
                if segment.evidence.iter().any(|evidence| {
                    evidence.blake3 == *blake3
                        || evidence
                            .usages
                            .iter()
                            .any(|usage| usage.sequence == event.sequence)
                }) {
                    return Err(RprovError::invalid(
                        "package_state.gaps",
                        "a missing-evidence gap contradicts a retained artifact",
                    ));
                }
                counts.missing_evidence = counts.missing_evidence.checked_add(1).ok_or(
                    RprovError::ArithmeticOverflow {
                        field: "missing evidence gaps",
                    },
                )?;
            }
            RprovRecoveryGap::MissingSourceLink { event } => {
                let (_, segment) = find_gap_segment(manifest, &event.session_id)?;
                validate_owner(segment, event, "package_state.gaps.event")?;
                if !missing_source_events.insert((event.session_id.clone(), event.sequence)) {
                    return Err(RprovError::invalid(
                        "package_state.gaps",
                        "an event has more than one missing-source-link gap",
                    ));
                }
                if segment
                    .source_links
                    .iter()
                    .any(|link| source_link_event(link).sequence == event.sequence)
                {
                    return Err(RprovError::invalid(
                        "package_state.gaps",
                        "a missing-source-link gap contradicts a retained source link",
                    ));
                }
                counts.missing_source_links = counts.missing_source_links.checked_add(1).ok_or(
                    RprovError::ArithmeticOverflow {
                        field: "missing source-link gaps",
                    },
                )?;
            }
        }
    }
    for segment in &manifest.segments {
        let missing = gaps
            .iter()
            .filter(|gap| {
                matches!(gap, RprovRecoveryGap::MissingSourceLink { event } if event.session_id == segment.session_id)
            })
            .count();
        let total = segment.source_links.len().checked_add(missing).ok_or(
            RprovError::ArithmeticOverflow {
                field: "segment source links",
            },
        )?;
        if total > MAX_RPROV_SOURCE_LINKS_PER_SEGMENT {
            return Err(RprovError::limit(
                "segment source links including gaps",
                total,
                MAX_RPROV_SOURCE_LINKS_PER_SEGMENT,
            ));
        }
    }
    Ok(counts)
}

fn find_gap_segment<'a>(
    manifest: &'a RprovManifest,
    session_id: &SessionId,
) -> Result<(usize, &'a RprovSegment), RprovError> {
    manifest
        .segments
        .iter()
        .enumerate()
        .find(|(_, segment)| &segment.session_id == session_id)
        .ok_or_else(|| {
            RprovError::invalid(
                "package_state.gaps",
                "must refer to an exact retained session",
            )
        })
}

fn has_missing_ancestry(state: &RprovPackageState, session_id: &SessionId) -> bool {
    matches!(
        state,
        RprovPackageState::RecoveryIncomplete { gaps, .. }
            if gaps.iter().any(|gap| matches!(
                gap,
                RprovRecoveryGap::MissingAncestry { before_session_id }
                    if before_session_id == session_id
            ))
    )
}

fn validate_assignment_manifest(value: &RprovAssignmentManifestIdentity) -> Result<(), RprovError> {
    require_inner_version("assignment manifest", value.format_version)?;
    require_range(
        "assignment manifest bytes",
        value.byte_length,
        1,
        MAX_RPROV_ASSIGNMENT_MANIFEST_BYTES,
    )
}

fn validate_initial_workspace<'a>(
    workspace: &'a RprovInitialWorkspace,
    inventory: &HashMap<&'a str, &'a RprovInventoryEntry>,
    references: &mut HashMap<&'a str, usize>,
) -> Result<(), RprovError> {
    let mut previous: Option<&WorkspacePath> = None;
    let mut total = 0_u64;
    for file in &workspace.files {
        if previous.is_some_and(|path| path >= &file.path) {
            return Err(RprovError::invalid(
                "initial_workspace.files",
                "paths must be unique and strictly sorted",
            ));
        }
        previous = Some(&file.path);
        let entry = require_inventory_ref(
            inventory,
            references,
            &file.entry,
            RprovEntryKind::InitialWorkspaceBlob,
            None,
            None,
        )?;
        let expected = initial_blob_path(entry.blake3);
        if file.entry != expected {
            return Err(RprovError::invalid(
                "initial_workspace.files.entry",
                "must be the content-addressed starter blob path",
            ));
        }
        require_max(
            "initial workspace file bytes",
            entry.byte_length,
            MAX_RPROV_INITIAL_FILE_BYTES,
        )?;
        total = checked_add("initial workspace bytes", total, entry.byte_length)?;
    }
    require_max(
        "initial workspace bytes",
        total,
        MAX_RPROV_INITIAL_WORKSPACE_BYTES,
    )
}

fn validate_inventory(
    entries: &[RprovInventoryEntry],
) -> Result<HashMap<&str, &RprovInventoryEntry>, RprovError> {
    let mut result = HashMap::with_capacity(entries.len());
    let mut previous: Option<&str> = None;
    for entry in entries {
        validate_rprov_archive_path(&entry.path)?;
        if entry.path == "manifest.json" {
            return Err(RprovError::invalid(
                "inventory.path",
                "manifest.json is the sole unlisted entry",
            ));
        }
        if previous.is_some_and(|path| path >= entry.path.as_str()) {
            return Err(RprovError::invalid(
                "inventory",
                "paths must be unique and strictly sorted",
            ));
        }
        previous = Some(&entry.path);
        require_max(
            "inventory payload bytes",
            entry.byte_length,
            MAX_RPROV_RECORD_PAYLOAD_BYTES,
        )?;
        validate_inventory_shape(entry)?;
        result.insert(entry.path.as_str(), entry);
    }
    Ok(result)
}

fn validate_inventory_shape(entry: &RprovInventoryEntry) -> Result<(), RprovError> {
    match entry.kind {
        RprovEntryKind::InitialWorkspaceBlob => {
            if entry.path != initial_blob_path(entry.blake3) {
                return Err(RprovError::invalid(
                    "inventory.path",
                    "starter blob path must contain its raw digest",
                ));
            }
            require_max(
                "initial workspace blob bytes",
                entry.byte_length,
                MAX_RPROV_INITIAL_FILE_BYTES,
            )
        }
        RprovEntryKind::Events => require_max(
            "events entry bytes",
            entry.byte_length,
            MAX_RPROV_SEGMENT_EVENTS_BYTES,
        ),
        RprovEntryKind::Checkpoint => require_max(
            "checkpoint entry bytes",
            entry.byte_length,
            MAX_RPROV_CHECKPOINT_ENCODED_BYTES,
        ),
        RprovEntryKind::RuntimeMetadata => require_max(
            "runtime metadata entry bytes",
            entry.byte_length,
            MAX_RPROV_METADATA_ENTRY_BYTES,
        ),
        RprovEntryKind::ExternalRecoveryEvidence => require_max(
            "external recovery evidence entry bytes",
            entry.byte_length,
            MAX_RPROV_EVIDENCE_ENTRY_BYTES,
        ),
    }
}

fn validate_segment<'a>(
    manifest: &RprovManifest,
    segment: &'a RprovSegment,
    index: usize,
    inventory: &HashMap<&'a str, &'a RprovInventoryEntry>,
    references: &mut HashMap<&'a str, usize>,
) -> Result<(), RprovError> {
    let expected_ordinal = u32::try_from(index + 1).unwrap();
    if segment.ordinal != expected_ordinal {
        return Err(RprovError::invalid(
            "segments.ordinal",
            "must be one-based, contiguous, and in array order",
        ));
    }
    validate_identifier("segments.course_id", &segment.course_id)?;
    validate_identifier("segments.assignment_id", &segment.assignment_id)?;
    validate_identifier("segments.assignment_version", &segment.assignment_version)?;
    if segment.course_id != manifest.course_id
        || segment.assignment_id != manifest.assignment_id
        || segment.assignment_version != manifest.assignment_version
        || segment.assignment_manifest_blake3 != manifest.assignment_manifest.blake3
        || segment.original_starter_tree_hash != manifest.original_starter_tree_hash
    {
        return Err(RprovError::invalid(
            "segments.assignment_identity",
            "must equal the package assignment and original starter identity",
        ));
    }
    validate_producer(&segment.producer)?;
    validate_segment_ancestry(manifest, segment, index)?;
    if segment.inclusive_event_count == 0 || segment.inclusive_event_count > MAX_RPROV_EVENTS {
        return Err(RprovError::LimitExceeded {
            field: "segment inclusive event count",
            actual: segment.inclusive_event_count,
            maximum: MAX_RPROV_EVENTS,
        });
    }
    require_inner_version("event", segment.events.format_version)?;
    require_max(
        "segment events bytes",
        segment.events.byte_length,
        MAX_RPROV_SEGMENT_EVENTS_BYTES,
    )?;
    let expected_events = segment_events_path(segment.ordinal);
    if segment.events.entry != expected_events {
        return Err(RprovError::invalid(
            "segments.events.entry",
            "does not match the segment ordinal",
        ));
    }
    require_inventory_ref(
        inventory,
        references,
        &segment.events.entry,
        RprovEntryKind::Events,
        Some(segment.events.byte_length),
        Some(segment.events.blake3),
    )?;

    validate_checkpoints(segment, inventory, references)?;
    validate_metadata(segment, inventory, references)?;
    validate_evidence(segment, inventory, references)?;
    validate_source_links(segment)?;

    if segment.checkpoints[0].workspace_hash != segment.initial_tree_hash {
        return Err(RprovError::invalid(
            "segments.checkpoints",
            "initial checkpoint must bind the segment initial tree",
        ));
    }

    match segment.events.completeness {
        RprovEventStreamCompleteness::Complete => {
            if segment.terminal_event_hash.known() != Some(&segment.last_event_hash) {
                return Err(RprovError::invalid(
                    "segments.terminal_event_hash",
                    "a complete stream requires the known last event hash",
                ));
            }
            let final_hash = segment.final_tree_hash.known().ok_or_else(|| {
                RprovError::invalid(
                    "segments.final_tree_hash",
                    "a complete stream requires a known final tree hash",
                )
            })?;
            if segment.checkpoints.last().map(|item| item.workspace_hash) != Some(*final_hash) {
                return Err(RprovError::EventIntegrity {
                    kind: RprovEventIntegrityKind::FinalTreeBinding,
                    segment: segment.ordinal,
                    sequence: segment.checkpoints.last().unwrap().owner.sequence,
                    detail: "final checkpoint must bind the segment final tree".to_owned(),
                });
            }
        }
        RprovEventStreamCompleteness::PrefixOnly => {
            if segment.terminal_event_hash.known().is_some()
                || segment.final_tree_hash.known().is_some()
            {
                return Err(RprovError::invalid(
                    "segments.events.completeness",
                    "a prefix-only stream cannot claim terminal or final-tree facts",
                ));
            }
        }
    }

    let is_clean = matches!(manifest.package_state, RprovPackageState::CleanFinalized);
    if is_clean {
        if segment.events.completeness != RprovEventStreamCompleteness::Complete {
            return Err(RprovError::invalid(
                "segments.events.completeness",
                "clean packages require complete streams",
            ));
        }
    } else if index + 1 != manifest.segments.len()
        && segment.events.completeness != RprovEventStreamCompleteness::Complete
    {
        return Err(RprovError::invalid(
            "segments",
            "only the recovery tip may be incomplete",
        ));
    }
    Ok(())
}

fn validate_segment_ancestry(
    manifest: &RprovManifest,
    segment: &RprovSegment,
    index: usize,
) -> Result<(), RprovError> {
    let missing_ancestry = has_missing_ancestry(&manifest.package_state, &segment.session_id);
    if index == 0 {
        if segment.parent.is_some() {
            return Err(RprovError::invalid(
                "segments.parent",
                "the root parent must be null",
            ));
        }
        if segment.initial_tree_hash != manifest.original_starter_tree_hash && !missing_ancestry {
            return Err(RprovError::invalid(
                "segments.initial_tree_hash",
                "the first retained segment must start at the original starter tree or have an exact recovery gap",
            ));
        }
        return Ok(());
    }

    let previous = &manifest.segments[index - 1];
    if missing_ancestry && segment.parent.is_none() {
        return Ok(());
    }
    let parent = segment.parent.as_ref().ok_or_else(|| {
        RprovError::invalid(
            "segments.parent",
            "every non-root segment requires its immediate parent",
        )
    })?;
    let previous_terminal = previous.terminal_event_hash.known().ok_or_else(|| {
        RprovError::invalid(
            "segments.parent",
            "an incomplete segment cannot have a child",
        )
    })?;
    let previous_final = previous.final_tree_hash.known().ok_or_else(|| {
        RprovError::invalid(
            "segments.parent",
            "a segment with unknown final tree cannot have a child",
        )
    })?;
    if parent.session_id != previous.session_id
        || parent.terminal_event_hash != *previous_terminal
        || parent.final_tree_hash != *previous_final
    {
        return Err(RprovError::invalid(
            "segments.parent",
            "must bind the immediately preceding session, terminal hash, and final tree",
        ));
    }
    if segment.initial_tree_hash != *previous_final {
        return Err(RprovError::invalid(
            "segments.initial_tree_hash",
            "must equal the immediate parent final tree",
        ));
    }
    Ok(())
}

fn validate_checkpoints<'a>(
    segment: &'a RprovSegment,
    inventory: &HashMap<&'a str, &'a RprovInventoryEntry>,
    references: &mut HashMap<&'a str, usize>,
) -> Result<(), RprovError> {
    if segment.checkpoints.is_empty() {
        return Err(RprovError::invalid(
            "segments.checkpoints",
            "must contain an initial checkpoint",
        ));
    }
    if segment.checkpoints.len() > MAX_RPROV_CHECKPOINTS_PER_SEGMENT {
        return Err(RprovError::limit(
            "segment checkpoints",
            segment.checkpoints.len(),
            MAX_RPROV_CHECKPOINTS_PER_SEGMENT,
        ));
    }
    if segment.checkpoints[0].role != RprovCheckpointRole::Initial {
        return Err(RprovError::invalid(
            "segments.checkpoints",
            "first checkpoint role must be initial",
        ));
    }
    if segment.checkpoints[0].owner.sequence != 1 {
        return Err(RprovError::invalid(
            "segments.checkpoints.owner.sequence",
            "the initial checkpoint must be the sequence-1 genesis",
        ));
    }
    let complete = segment.events.completeness == RprovEventStreamCompleteness::Complete;
    if complete
        && (segment.checkpoints.len() < 2
            || segment.checkpoints.last().unwrap().role != RprovCheckpointRole::Final)
    {
        return Err(RprovError::invalid(
            "segments.checkpoints",
            "a finalized segment requires distinct initial and final checkpoints",
        ));
    }
    let mut previous_sequence = 0_u64;
    let mut total = 0_u64;
    for (index, checkpoint) in segment.checkpoints.iter().enumerate() {
        require_inner_version("checkpoint", checkpoint.format_version)?;
        validate_owner(segment, &checkpoint.owner, "segments.checkpoints.owner")?;
        if checkpoint.owner.sequence <= previous_sequence {
            return Err(RprovError::invalid(
                "segments.checkpoints.owner.sequence",
                "must be strictly increasing",
            ));
        }
        previous_sequence = checkpoint.owner.sequence;
        if index > 0 {
            let expected_role = if complete && index + 1 == segment.checkpoints.len() {
                RprovCheckpointRole::Final
            } else {
                RprovCheckpointRole::Accepted
            };
            if checkpoint.role != expected_role {
                return Err(RprovError::invalid(
                    "segments.checkpoints.role",
                    "must be initial, zero or more accepted, then final only for a complete stream",
                ));
            }
        }
        let expected = checkpoint_path(segment.ordinal, checkpoint.owner.sequence);
        if checkpoint.entry != expected {
            return Err(RprovError::invalid(
                "segments.checkpoints.entry",
                "does not match its segment and owning sequence",
            ));
        }
        require_max(
            "checkpoint entry bytes",
            checkpoint.byte_length,
            MAX_RPROV_CHECKPOINT_ENCODED_BYTES,
        )?;
        require_inventory_ref(
            inventory,
            references,
            &checkpoint.entry,
            RprovEntryKind::Checkpoint,
            Some(checkpoint.byte_length),
            Some(checkpoint.blake3),
        )?;
        total = checked_add("segment checkpoint bytes", total, checkpoint.byte_length)?;
    }
    require_max(
        "segment checkpoint bytes",
        total,
        MAX_RPROV_SEGMENT_CHECKPOINT_BYTES,
    )
}

fn validate_metadata<'a>(
    segment: &'a RprovSegment,
    inventory: &HashMap<&'a str, &'a RprovInventoryEntry>,
    references: &mut HashMap<&'a str, usize>,
) -> Result<(), RprovError> {
    if segment.metadata.len() > MAX_RPROV_METADATA_PER_SEGMENT {
        return Err(RprovError::limit(
            "segment metadata entries",
            segment.metadata.len(),
            MAX_RPROV_METADATA_PER_SEGMENT,
        ));
    }
    let mut previous = None;
    let mut total = 0_u64;
    for metadata in &segment.metadata {
        require_inner_version("runtime metadata", metadata.format_version)?;
        validate_owner(segment, &metadata.owner, "segments.metadata.owner")?;
        let expected = metadata_path(segment.ordinal, metadata.blake3);
        if metadata.entry != expected
            || previous.is_some_and(|path| path >= metadata.entry.as_str())
        {
            return Err(RprovError::invalid(
                "segments.metadata",
                "entries must use digest paths and be strictly sorted",
            ));
        }
        previous = Some(metadata.entry.as_str());
        require_inventory_ref(
            inventory,
            references,
            &metadata.entry,
            RprovEntryKind::RuntimeMetadata,
            Some(metadata.byte_length),
            Some(metadata.blake3),
        )?;
        total = checked_add("segment metadata bytes", total, metadata.byte_length)?;
    }
    require_max(
        "segment metadata bytes",
        total,
        MAX_RPROV_SEGMENT_METADATA_BYTES,
    )
}

fn validate_evidence<'a>(
    segment: &'a RprovSegment,
    inventory: &HashMap<&'a str, &'a RprovInventoryEntry>,
    references: &mut HashMap<&'a str, usize>,
) -> Result<(), RprovError> {
    if segment.evidence.len() > MAX_RPROV_EVIDENCE_PER_SEGMENT {
        return Err(RprovError::limit(
            "segment evidence entries",
            segment.evidence.len(),
            MAX_RPROV_EVIDENCE_PER_SEGMENT,
        ));
    }
    let mut previous = None;
    let mut used_sequences = HashSet::new();
    let mut total = 0_u64;
    for evidence in &segment.evidence {
        if evidence.usages.is_empty() {
            return Err(RprovError::invalid(
                "segments.evidence.usages",
                "must contain at least one exact event usage",
            ));
        }
        if evidence.usages.len() > MAX_RPROV_EVIDENCE_USAGES_PER_ARTIFACT {
            return Err(RprovError::limit(
                "evidence usages per artifact",
                evidence.usages.len(),
                MAX_RPROV_EVIDENCE_USAGES_PER_ARTIFACT,
            ));
        }
        let mut previous_usage = 0_u64;
        for usage in &evidence.usages {
            validate_owner(segment, usage, "segments.evidence.usages")?;
            if usage.sequence <= previous_usage {
                return Err(RprovError::invalid(
                    "segments.evidence.usages",
                    "must be unique and strictly ordered by event sequence",
                ));
            }
            if !used_sequences.insert(usage.sequence) {
                return Err(RprovError::invalid(
                    "segments.evidence.usages",
                    "an event may be used by only one evidence artifact",
                ));
            }
            previous_usage = usage.sequence;
        }
        let expected = evidence_path(segment.ordinal, evidence.blake3);
        if evidence.entry != expected
            || previous.is_some_and(|path| path >= evidence.entry.as_str())
        {
            return Err(RprovError::invalid(
                "segments.evidence",
                "entries must use digest paths and be strictly sorted",
            ));
        }
        previous = Some(evidence.entry.as_str());
        require_inventory_ref(
            inventory,
            references,
            &evidence.entry,
            RprovEntryKind::ExternalRecoveryEvidence,
            Some(evidence.byte_length),
            Some(evidence.blake3),
        )?;
        total = checked_add("segment evidence bytes", total, evidence.byte_length)?;
    }
    require_max(
        "segment evidence bytes",
        total,
        MAX_RPROV_SEGMENT_EVIDENCE_BYTES,
    )
}

fn validate_source_link_shape(
    segment: &RprovSegment,
    source: &RprovSourceLink,
) -> Result<(), RprovError> {
    match source {
        RprovSourceLink::InternalPaste {
            paste_event,
            copied_event,
            start_byte,
            end_byte,
            ..
        } => {
            validate_owner(segment, paste_event, "source_links.paste_event")?;
            validate_owner(segment, copied_event, "source_links.copied_event")?;
            if copied_event.sequence >= paste_event.sequence || start_byte >= end_byte {
                return Err(RprovError::invalid(
                    "source_links.internal_paste",
                    "copy must precede paste and the source range must be nonempty",
                ));
            }
            if end_byte - start_byte > crate::MAX_INTERNAL_CLIPBOARD_BYTES as u64 {
                return Err(RprovError::invalid(
                    "source_links.internal_paste",
                    "source range exceeds the internal clipboard limit",
                ));
            }
        }
        RprovSourceLink::LegacyPaste { event, .. } => {
            validate_owner(segment, event, "source_links.event")?;
        }
    }
    Ok(())
}

fn validate_source_links(segment: &RprovSegment) -> Result<(), RprovError> {
    if segment.source_links.len() > MAX_RPROV_SOURCE_LINKS_PER_SEGMENT {
        return Err(RprovError::limit(
            "segment source links",
            segment.source_links.len(),
            MAX_RPROV_SOURCE_LINKS_PER_SEGMENT,
        ));
    }
    let mut previous_sequence = 0_u64;
    for source in &segment.source_links {
        validate_source_link_shape(segment, source)?;
        let sequence = source_link_event(source).sequence;
        if sequence <= previous_sequence {
            return Err(RprovError::invalid(
                "segments.source_links",
                "must be unique and strictly ordered by owning paste-event sequence",
            ));
        }
        previous_sequence = sequence;
    }
    Ok(())
}

fn source_link_event(source: &RprovSourceLink) -> &RecordedEventRef {
    match source {
        RprovSourceLink::InternalPaste { paste_event, .. } => paste_event,
        RprovSourceLink::LegacyPaste { event, .. } => event,
    }
}

fn validate_owner(
    segment: &RprovSegment,
    owner: &RecordedEventRef,
    field: &'static str,
) -> Result<(), RprovError> {
    if owner.session_id != segment.session_id
        || owner.sequence == 0
        || owner.sequence > segment.inclusive_event_count
    {
        return Err(RprovError::invalid(
            field,
            "must name an included event in the owning segment",
        ));
    }
    Ok(())
}

fn require_inventory_ref<'a>(
    inventory: &HashMap<&'a str, &'a RprovInventoryEntry>,
    references: &mut HashMap<&'a str, usize>,
    path: &'a str,
    kind: RprovEntryKind,
    length: Option<u64>,
    digest: Option<Hash>,
) -> Result<&'a RprovInventoryEntry, RprovError> {
    let entry = inventory
        .get(path)
        .copied()
        .ok_or_else(|| RprovError::invalid("entry reference", "is absent from inventory"))?;
    if entry.kind != kind
        || length.is_some_and(|value| value != entry.byte_length)
        || digest.is_some_and(|value| value != entry.blake3)
    {
        return Err(RprovError::invalid(
            "entry reference",
            "kind, length, or digest disagrees with inventory",
        ));
    }
    let count = references
        .get_mut(path)
        .ok_or_else(|| RprovError::invalid("entry reference", "is absent from inventory"))?;
    *count = count.checked_add(1).ok_or(RprovError::ArithmeticOverflow {
        field: "entry references",
    })?;
    if kind != RprovEntryKind::InitialWorkspaceBlob && *count != 1 {
        return Err(RprovError::invalid(
            "entry reference",
            "non-starter payload has more than one owner",
        ));
    }
    Ok(entry)
}

fn validate_producer(producer: &RprovProducer) -> Result<(), RprovError> {
    validate_known_string("producer.client_version", &producer.client_version)?;
    validate_known_string("producer.build_identity", &producer.build_identity)?;
    validate_known_string("producer.os", &producer.os)?;
    validate_known_string("producer.architecture", &producer.architecture)?;
    if producer.rust_tools.len() > MAX_RPROV_TOOLS {
        return Err(RprovError::limit(
            "producer rust tools",
            producer.rust_tools.len(),
            MAX_RPROV_TOOLS,
        ));
    }
    let mut previous: Option<&str> = None;
    for tool in &producer.rust_tools {
        validate_tool_name(&tool.tool)?;
        if previous.is_some_and(|name| name >= tool.tool.as_str()) {
            return Err(RprovError::invalid(
                "producer.rust_tools",
                "tool names must be unique and strictly sorted",
            ));
        }
        previous = Some(&tool.tool);
        if let RprovKnown::Known { value } = &tool.version {
            validate_nfc_text(
                "producer.rust_tools.version",
                value,
                MAX_RPROV_TOOL_VERSION_BYTES,
            )?;
        }
    }
    Ok(())
}

fn validate_known_string(
    field: &'static str,
    value: &RprovKnown<String>,
) -> Result<(), RprovError> {
    if let RprovKnown::Known { value } = value {
        validate_nfc_text(field, value, MAX_RPROV_PRODUCER_VALUE_BYTES)?;
    }
    Ok(())
}

fn validate_tool_name(value: &str) -> Result<(), RprovError> {
    validate_nfc_text("producer.rust_tools.tool", value, MAX_RPROV_TOOL_NAME_BYTES)?;
    if !value.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
    }) {
        return Err(RprovError::invalid(
            "producer.rust_tools.tool",
            "must use lowercase ASCII letters, digits, '-' or '_'",
        ));
    }
    Ok(())
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), RprovError> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES {
        return Err(RprovError::invalid(
            field,
            format!("must contain 1..={MAX_IDENTIFIER_BYTES} bytes"),
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(RprovError::invalid(
            field,
            "must use ASCII letters, digits, '-', '_' or '.'",
        ));
    }
    Ok(())
}

fn validate_nfc_text(field: &'static str, value: &str, maximum: usize) -> Result<(), RprovError> {
    if value.is_empty() || value.len() > maximum {
        return Err(RprovError::invalid(
            field,
            format!("must contain 1..={maximum} UTF-8 bytes"),
        ));
    }
    if value.nfc().ne(value.chars()) || value.chars().any(char::is_control) {
        return Err(RprovError::invalid(
            field,
            "must be NFC and contain no control characters",
        ));
    }
    Ok(())
}

fn initial_blob_path(digest: Hash) -> String {
    format!("initial-workspace/blobs/{digest}")
}

fn segment_events_path(ordinal: u32) -> String {
    format!("segments/{ordinal:04}/events.jsonl")
}

fn checkpoint_path(ordinal: u32, sequence: u64) -> String {
    format!("segments/{ordinal:04}/checkpoints/{sequence:020}.rcpk")
}

fn metadata_path(ordinal: u32, digest: Hash) -> String {
    format!("segments/{ordinal:04}/metadata/{digest}.json")
}

fn evidence_path(ordinal: u32, digest: Hash) -> String {
    format!("segments/{ordinal:04}/evidence/{digest}.bin")
}

fn require_range(
    field: &'static str,
    value: u64,
    minimum: u64,
    maximum: u64,
) -> Result<(), RprovError> {
    if value < minimum || value > maximum {
        Err(RprovError::LimitExceeded {
            field,
            actual: value,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn require_max(field: &'static str, value: u64, maximum: u64) -> Result<(), RprovError> {
    if value > maximum {
        Err(RprovError::LimitExceeded {
            field,
            actual: value,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn checked_add(field: &'static str, left: u64, right: u64) -> Result<u64, RprovError> {
    left.checked_add(right)
        .ok_or(RprovError::ArithmeticOverflow { field })
}

fn checked_sum(
    field: &'static str,
    mut values: impl Iterator<Item = u64>,
) -> Result<u64, RprovError> {
    values.try_fold(0, |total, value| checked_add(field, total, value))
}

struct CappedWriter<W> {
    inner: W,
    written: usize,
}

impl<W> CappedWriter<W> {
    const fn new(inner: W) -> Self {
        Self { inner, written: 0 }
    }

    fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for CappedWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.written.saturating_add(buffer.len()) > MAX_RPROV_MANIFEST_BYTES {
            return Err(io::Error::other("manifest byte limit exceeded"));
        }
        let count = self.inner.write(buffer)?;
        self.written += count;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn preflight_manifest_json(encoded: &[u8]) -> Result<(), RprovError> {
    let source = std::str::from_utf8(encoded).map_err(|error| RprovError::InvalidManifest {
        detail: format!("not UTF-8 at byte {}", error.valid_up_to()),
    })?;
    ManifestJsonPreflight::new(source).validate()
}

fn preflight_compact_json_object(encoded: &[u8], layer: &'static str) -> Result<(), RprovError> {
    let source = std::str::from_utf8(encoded).map_err(|error| {
        RprovError::invalid(layer, format!("not UTF-8 at byte {}", error.valid_up_to()))
    })?;
    if encoded.first() != Some(&b'{') {
        return Err(RprovError::invalid(layer, "must be one JSON object"));
    }
    ManifestJsonPreflight::new(source)
        .validate_compact()
        .map_err(|error| match error {
            RprovError::InvalidManifest { detail } => RprovError::invalid(layer, detail),
            other => other,
        })
}

struct ManifestJsonPreflight<'a> {
    source: &'a str,
    bytes: &'a [u8],
    position: usize,
    value_count: usize,
    first_whitespace: Option<usize>,
}

impl<'a> ManifestJsonPreflight<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            position: 0,
            value_count: 0,
            first_whitespace: None,
        }
    }

    fn validate(mut self) -> Result<(), RprovError> {
        self.skip_whitespace();
        self.parse_value(0)?;
        self.skip_whitespace();
        if self.position != self.bytes.len() {
            return Err(self.syntax("trailing characters after JSON value"));
        }
        Ok(())
    }

    fn validate_compact(mut self) -> Result<(), RprovError> {
        self.parse_value(0)?;
        if self.position != self.bytes.len() {
            return Err(self.syntax("trailing characters after JSON value"));
        }
        if let Some(position) = self.first_whitespace {
            return Err(RprovError::InvalidManifest {
                detail: format!("whitespace outside strings is forbidden at byte {position}"),
            });
        }
        Ok(())
    }

    fn parse_value(&mut self, depth: usize) -> Result<(), RprovError> {
        self.skip_whitespace();
        self.value_count += 1;
        if self.value_count > MAX_RPROV_JSON_VALUES {
            return Err(RprovError::LimitExceeded {
                field: "JSON values",
                actual: self.value_count as u64,
                maximum: MAX_RPROV_JSON_VALUES as u64,
            });
        }
        match self.current() {
            Some(b'{') => self.parse_object(depth + 1),
            Some(b'[') => self.parse_array(depth + 1),
            Some(b'"') => self.parse_string(false).map(|_| ()),
            Some(b't') => self.parse_literal(b"true"),
            Some(b'f') => self.parse_literal(b"false"),
            Some(b'n') => self.parse_literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(_) => Err(self.syntax("expected a JSON value")),
            None => Err(self.syntax("unexpected end of input")),
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<(), RprovError> {
        self.check_depth(depth)?;
        self.position += 1;
        self.skip_whitespace();
        if self.consume_if(b'}') {
            return Ok(());
        }
        let mut keys = HashSet::new();
        let mut members = 0_usize;
        loop {
            members += 1;
            if members > MAX_RPROV_JSON_ARRAY_ITEMS {
                return Err(RprovError::limit(
                    "JSON object members",
                    members,
                    MAX_RPROV_JSON_ARRAY_ITEMS,
                ));
            }
            if self.current() != Some(b'"') {
                return Err(self.syntax("expected a JSON object key"));
            }
            let key = self.parse_string(true)?;
            if !keys.insert(key) {
                return Err(self.syntax("duplicate JSON object key"));
            }
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

    fn parse_array(&mut self, depth: usize) -> Result<(), RprovError> {
        self.check_depth(depth)?;
        self.position += 1;
        self.skip_whitespace();
        if self.consume_if(b']') {
            return Ok(());
        }
        let mut items = 0_usize;
        loop {
            items += 1;
            if items > MAX_RPROV_JSON_ARRAY_ITEMS {
                return Err(RprovError::limit(
                    "JSON array items",
                    items,
                    MAX_RPROV_JSON_ARRAY_ITEMS,
                ));
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

    fn parse_string(&mut self, key: bool) -> Result<String, RprovError> {
        self.position += 1;
        let raw_start = self.position;
        let mut decoded_length = 0_usize;
        let mut decoded = String::new();
        loop {
            let Some(byte) = self.current() else {
                return Err(self.syntax("unterminated JSON string"));
            };
            let character = match byte {
                b'"' => {
                    self.check_string_lengths(key, raw_start, decoded_length)?;
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
            if key {
                decoded.push(character);
            }
            self.check_string_lengths(key, raw_start, decoded_length)?;
        }
    }

    fn parse_escape(&mut self) -> Result<char, RprovError> {
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
                        return Err(self.syntax("high surrogate requires a low surrogate"));
                    }
                    self.position += 2;
                    let low = self.parse_hex_quad()?;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return Err(self.syntax("high surrogate requires a low surrogate"));
                    }
                    0x1_0000 + ((high - 0xd800) << 10) + (low - 0xdc00)
                } else if (0xdc00..=0xdfff).contains(&high) {
                    return Err(self.syntax("low surrogate requires a high surrogate"));
                } else {
                    high
                };
                char::from_u32(scalar).ok_or_else(|| self.syntax("invalid Unicode escape"))
            }
            _ => Err(self.syntax("invalid JSON escape")),
        }
    }

    fn parse_hex_quad(&mut self) -> Result<u32, RprovError> {
        let mut value = 0_u32;
        for _ in 0..4 {
            let Some(byte) = self.current() else {
                return Err(self.syntax("incomplete Unicode escape"));
            };
            let digit = match byte {
                b'0'..=b'9' => u32::from(byte - b'0'),
                b'a'..=b'f' => u32::from(byte - b'a' + 10),
                b'A'..=b'F' => u32::from(byte - b'A' + 10),
                _ => return Err(self.syntax("invalid Unicode escape digit")),
            };
            value = (value << 4) | digit;
            self.position += 1;
        }
        Ok(value)
    }

    fn parse_literal(&mut self, literal: &[u8]) -> Result<(), RprovError> {
        let end = self.position + literal.len();
        if self.bytes.get(self.position..end) != Some(literal) {
            return Err(self.syntax("invalid JSON literal"));
        }
        self.position = end;
        Ok(())
    }

    fn parse_number(&mut self) -> Result<(), RprovError> {
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
            return Err(self.syntax("JSON fraction requires a digit"));
        }
        if matches!(self.current(), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.current(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            if self.consume_digits() == 0 {
                return Err(self.syntax("JSON exponent requires a digit"));
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

    fn check_depth(&self, depth: usize) -> Result<(), RprovError> {
        if depth > MAX_RPROV_JSON_NESTING {
            Err(RprovError::LimitExceeded {
                field: "JSON nesting",
                actual: depth as u64,
                maximum: MAX_RPROV_JSON_NESTING as u64,
            })
        } else {
            Ok(())
        }
    }

    fn check_string_lengths(
        &self,
        key: bool,
        raw_start: usize,
        decoded_length: usize,
    ) -> Result<(), RprovError> {
        let maximum = if key {
            MAX_RPROV_JSON_KEY_BYTES
        } else {
            MAX_RPROV_JSON_STRING_BYTES
        };
        let raw_length = self.position - raw_start;
        if raw_length > maximum || decoded_length > maximum {
            return Err(RprovError::LimitExceeded {
                field: if key {
                    "JSON key bytes"
                } else {
                    "JSON string bytes"
                },
                actual: raw_length.max(decoded_length) as u64,
                maximum: maximum as u64,
            });
        }
        Ok(())
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.current(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.first_whitespace.get_or_insert(self.position);
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

    fn expect_byte(&mut self, expected: u8, message: &'static str) -> Result<(), RprovError> {
        if self.consume_if(expected) {
            Ok(())
        } else {
            Err(self.syntax(message))
        }
    }

    fn syntax(&self, message: impl Into<String>) -> RprovError {
        RprovError::InvalidManifest {
            detail: format!("{} at byte {}", message.into(), self.position),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_integrity_display_uses_stable_human_labels() {
        let rendered = RprovError::EventIntegrity {
            kind: RprovEventIntegrityKind::MissingExternalEvidence,
            segment: 1,
            sequence: 2,
            detail: "required artifact is absent".to_owned(),
        }
        .to_string();

        assert!(rendered.contains("missing external evidence"), "{rendered}");
        assert!(!rendered.contains("MissingExternalEvidence"), "{rendered}");
    }

    #[test]
    fn manifest_json_preflight_limits_are_inclusive() {
        let nested_exact = format!(
            "{}0{}",
            "[".repeat(MAX_RPROV_JSON_NESTING),
            "]".repeat(MAX_RPROV_JSON_NESTING)
        );
        preflight_manifest_json(nested_exact.as_bytes()).unwrap();
        let nested_over = format!(
            "{}0{}",
            "[".repeat(MAX_RPROV_JSON_NESTING + 1),
            "]".repeat(MAX_RPROV_JSON_NESTING + 1)
        );
        assert!(preflight_manifest_json(nested_over.as_bytes()).is_err());

        let key_exact = format!("{{\"{}\":0}}", "k".repeat(MAX_RPROV_JSON_KEY_BYTES));
        preflight_manifest_json(key_exact.as_bytes()).unwrap();
        let key_over = format!("{{\"{}\":0}}", "k".repeat(MAX_RPROV_JSON_KEY_BYTES + 1));
        assert!(preflight_manifest_json(key_over.as_bytes()).is_err());

        let string_exact = format!("\"{}\"", "s".repeat(MAX_RPROV_JSON_STRING_BYTES));
        preflight_manifest_json(string_exact.as_bytes()).unwrap();
        let string_over = format!("\"{}\"", "s".repeat(MAX_RPROV_JSON_STRING_BYTES + 1));
        assert!(preflight_manifest_json(string_over.as_bytes()).is_err());

        let array_exact = format!(
            "[{}]",
            std::iter::repeat_n("0", MAX_RPROV_JSON_ARRAY_ITEMS)
                .collect::<Vec<_>>()
                .join(",")
        );
        preflight_manifest_json(array_exact.as_bytes()).unwrap();
        let array_over = format!(
            "[{}]",
            std::iter::repeat_n("0", MAX_RPROV_JSON_ARRAY_ITEMS + 1)
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(preflight_manifest_json(array_over.as_bytes()).is_err());

        let group_count = MAX_RPROV_JSON_ARRAY_ITEMS - 1;
        let base_group_items = 15;
        let first_group_items =
            MAX_RPROV_JSON_VALUES - 1 - group_count - base_group_items * (group_count - 1);
        let base_group = format!(
            "[{}]",
            std::iter::repeat_n("0", base_group_items)
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut groups = vec![base_group; group_count];
        groups[0] = format!(
            "[{}]",
            std::iter::repeat_n("0", first_group_items)
                .collect::<Vec<_>>()
                .join(",")
        );
        let values_exact = format!("[{}]", groups.join(","));
        preflight_manifest_json(values_exact.as_bytes()).unwrap();
        groups[0] = format!(
            "[{}]",
            std::iter::repeat_n("0", first_group_items + 1)
                .collect::<Vec<_>>()
                .join(",")
        );
        let values_over = format!("[{}]", groups.join(","));
        assert!(preflight_manifest_json(values_over.as_bytes()).is_err());
    }
}
