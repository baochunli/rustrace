use std::cell::Cell;
use std::io::{self, Cursor, Read};
use std::rc::Rc;

use rustrace_model::*;

fn hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; Hash::LENGTH])
}

fn session(value: &str) -> SessionId {
    SessionId::new(value).unwrap()
}

fn unknown_string() -> RprovKnown<String> {
    RprovKnown::Unknown
}

fn unknown_time() -> RprovKnown<chrono::DateTime<chrono::Utc>> {
    RprovKnown::Unknown
}

fn producer() -> RprovProducer {
    RprovProducer {
        client_version: unknown_string(),
        build_identity: unknown_string(),
        os: unknown_string(),
        architecture: unknown_string(),
        rust_tools: vec![],
    }
}

fn event_ref(sequence: u64, event_hash: Hash) -> RecordedEventRef {
    RecordedEventRef {
        session_id: session("session-1"),
        sequence,
        event_hash,
    }
}

fn checkpoint(
    role: RprovCheckpointRole,
    sequence: u64,
    digest: Hash,
    workspace_hash: Hash,
) -> RprovCheckpointRef {
    RprovCheckpointRef {
        role,
        format_version: 1,
        entry: format!("segments/0001/checkpoints/{sequence:020}.rcpk"),
        byte_length: 10,
        blake3: digest,
        owner: event_ref(sequence, hash(sequence as u8 + 10)),
        workspace_hash,
    }
}

fn clean_manifest() -> RprovManifest {
    let initial = checkpoint(RprovCheckpointRole::Initial, 1, hash(6), hash(1));
    let final_checkpoint = checkpoint(RprovCheckpointRole::Final, 3, hash(7), hash(4));
    RprovManifest {
        format_version: RPROV_FORMAT_VERSION_V1,
        package_state: RprovPackageState::CleanFinalized,
        submitted_source_comparison: RprovSubmittedSourceComparison::UnavailableStandalone,
        course_id: "ECE1724".to_owned(),
        assignment_id: "a3".to_owned(),
        assignment_version: "2026-09-01".to_owned(),
        student_id: "student-1".to_owned(),
        latest_session_id: session("session-1"),
        original_starter_tree_hash: hash(1),
        test_case_suite_hash: None,
        final_tree_hash: RprovKnown::Known { value: hash(4) },
        aggregate_event_count: 4,
        producer: producer(),
        assignment_manifest: RprovAssignmentManifestIdentity {
            format_version: 1,
            byte_length: 123,
            blake3: hash(3),
        },
        initial_workspace: RprovInitialWorkspace {
            files: vec![RprovInitialWorkspaceFile {
                path: WorkspacePath::new("src/main.rs").unwrap(),
                entry: format!("initial-workspace/blobs/{}", hash(2)),
            }],
        },
        segments: vec![RprovSegment {
            ordinal: 1,
            session_id: session("session-1"),
            course_id: "ECE1724".to_owned(),
            assignment_id: "a3".to_owned(),
            assignment_version: "2026-09-01".to_owned(),
            assignment_manifest_blake3: hash(3),
            original_starter_tree_hash: hash(1),
            initial_tree_hash: hash(1),
            producer: producer(),
            time: RprovSegmentTime {
                started_at_utc: unknown_time(),
                ended_at_utc: unknown_time(),
                inter_attempt_time: RprovInterAttemptTime::Unknown,
            },
            parent: None,
            events: RprovEventStreamRef {
                format_version: 1,
                entry: "segments/0001/events.jsonl".to_owned(),
                byte_length: 3,
                blake3: hash(5),
                completeness: RprovEventStreamCompleteness::Complete,
            },
            checkpoints: vec![initial, final_checkpoint],
            metadata: vec![],
            evidence: vec![],
            source_links: vec![],
            inclusive_event_count: 4,
            last_event_hash: hash(9),
            terminal_event_hash: RprovKnown::Known { value: hash(9) },
            final_tree_hash: RprovKnown::Known { value: hash(4) },
        }],
        inventory: vec![
            RprovInventoryEntry {
                path: format!("initial-workspace/blobs/{}", hash(2)),
                byte_length: 12,
                blake3: hash(2),
                kind: RprovEntryKind::InitialWorkspaceBlob,
            },
            RprovInventoryEntry {
                path: "segments/0001/checkpoints/00000000000000000001.rcpk".to_owned(),
                byte_length: 10,
                blake3: hash(6),
                kind: RprovEntryKind::Checkpoint,
            },
            RprovInventoryEntry {
                path: "segments/0001/checkpoints/00000000000000000003.rcpk".to_owned(),
                byte_length: 10,
                blake3: hash(7),
                kind: RprovEntryKind::Checkpoint,
            },
            RprovInventoryEntry {
                path: "segments/0001/events.jsonl".to_owned(),
                byte_length: 3,
                blake3: hash(5),
                kind: RprovEntryKind::Events,
            },
        ],
    }
}

fn two_segment_manifest() -> RprovManifest {
    let mut manifest = clean_manifest();
    let mut child = manifest.segments[0].clone();
    child.ordinal = 2;
    child.session_id = session("session-2");
    child.initial_tree_hash = hash(4);
    child.parent = Some(RprovParentLink {
        session_id: session("session-1"),
        terminal_event_hash: hash(9),
        final_tree_hash: hash(4),
    });
    child.events = RprovEventStreamRef {
        format_version: 1,
        entry: "segments/0002/events.jsonl".to_owned(),
        byte_length: 5,
        blake3: hash(15),
        completeness: RprovEventStreamCompleteness::Complete,
    };
    child.checkpoints = vec![
        RprovCheckpointRef {
            role: RprovCheckpointRole::Initial,
            format_version: 1,
            entry: "segments/0002/checkpoints/00000000000000000001.rcpk".to_owned(),
            byte_length: 11,
            blake3: hash(16),
            owner: RecordedEventRef {
                session_id: session("session-2"),
                sequence: 1,
                event_hash: hash(21),
            },
            workspace_hash: hash(4),
        },
        RprovCheckpointRef {
            role: RprovCheckpointRole::Final,
            format_version: 1,
            entry: "segments/0002/checkpoints/00000000000000000003.rcpk".to_owned(),
            byte_length: 11,
            blake3: hash(17),
            owner: RecordedEventRef {
                session_id: session("session-2"),
                sequence: 3,
                event_hash: hash(23),
            },
            workspace_hash: hash(14),
        },
    ];
    child.last_event_hash = hash(19);
    child.terminal_event_hash = RprovKnown::Known { value: hash(19) };
    child.final_tree_hash = RprovKnown::Known { value: hash(14) };

    manifest.latest_session_id = session("session-2");
    manifest.final_tree_hash = RprovKnown::Known { value: hash(14) };
    manifest.aggregate_event_count = 8;
    manifest.segments.push(child);
    manifest.inventory.extend([
        RprovInventoryEntry {
            path: "segments/0002/checkpoints/00000000000000000001.rcpk".to_owned(),
            byte_length: 11,
            blake3: hash(16),
            kind: RprovEntryKind::Checkpoint,
        },
        RprovInventoryEntry {
            path: "segments/0002/checkpoints/00000000000000000003.rcpk".to_owned(),
            byte_length: 11,
            blake3: hash(17),
            kind: RprovEntryKind::Checkpoint,
        },
        RprovInventoryEntry {
            path: "segments/0002/events.jsonl".to_owned(),
            byte_length: 5,
            blake3: hash(15),
            kind: RprovEntryKind::Events,
        },
    ]);
    manifest
}

fn indexed_hash(index: u64) -> Hash {
    let mut bytes = [0_u8; Hash::LENGTH];
    bytes[..8].copy_from_slice(&index.to_be_bytes());
    Hash::from_bytes(bytes)
}

fn linear_manifest(segment_count: usize) -> RprovManifest {
    assert!(segment_count > 0);
    let mut manifest = clean_manifest();
    while manifest.segments.len() < segment_count {
        let ordinal = manifest.segments.len() as u32 + 1;
        let previous = manifest.segments.last().unwrap();
        let previous_session = previous.session_id.clone();
        let previous_terminal = *previous.terminal_event_hash.known().unwrap();
        let previous_final = *previous.final_tree_hash.known().unwrap();
        let session_id = session(&format!("session-{ordinal}"));
        let terminal = indexed_hash(20_000 + u64::from(ordinal));
        let final_tree = indexed_hash(30_000 + u64::from(ordinal));
        let event_digest = indexed_hash(40_000 + u64::from(ordinal));
        let initial_digest = indexed_hash(50_000 + u64::from(ordinal));
        let final_digest = indexed_hash(60_000 + u64::from(ordinal));
        let event_path = format!("segments/{ordinal:04}/events.jsonl");
        let initial_path = format!("segments/{ordinal:04}/checkpoints/{:020}.rcpk", 1_u64);
        let final_path = format!("segments/{ordinal:04}/checkpoints/{:020}.rcpk", 3_u64);

        manifest.segments.push(RprovSegment {
            ordinal,
            session_id: session_id.clone(),
            course_id: manifest.course_id.clone(),
            assignment_id: manifest.assignment_id.clone(),
            assignment_version: manifest.assignment_version.clone(),
            assignment_manifest_blake3: manifest.assignment_manifest.blake3,
            original_starter_tree_hash: manifest.original_starter_tree_hash,
            initial_tree_hash: previous_final,
            producer: producer(),
            time: RprovSegmentTime {
                started_at_utc: unknown_time(),
                ended_at_utc: unknown_time(),
                inter_attempt_time: RprovInterAttemptTime::Unknown,
            },
            parent: Some(RprovParentLink {
                session_id: previous_session,
                terminal_event_hash: previous_terminal,
                final_tree_hash: previous_final,
            }),
            events: RprovEventStreamRef {
                format_version: 1,
                entry: event_path.clone(),
                byte_length: 1,
                blake3: event_digest,
                completeness: RprovEventStreamCompleteness::Complete,
            },
            checkpoints: vec![
                RprovCheckpointRef {
                    role: RprovCheckpointRole::Initial,
                    format_version: 1,
                    entry: initial_path.clone(),
                    byte_length: 1,
                    blake3: initial_digest,
                    owner: RecordedEventRef {
                        session_id: session_id.clone(),
                        sequence: 1,
                        event_hash: indexed_hash(70_000 + u64::from(ordinal)),
                    },
                    workspace_hash: previous_final,
                },
                RprovCheckpointRef {
                    role: RprovCheckpointRole::Final,
                    format_version: 1,
                    entry: final_path.clone(),
                    byte_length: 1,
                    blake3: final_digest,
                    owner: RecordedEventRef {
                        session_id: session_id.clone(),
                        sequence: 3,
                        event_hash: indexed_hash(80_000 + u64::from(ordinal)),
                    },
                    workspace_hash: final_tree,
                },
            ],
            metadata: vec![],
            evidence: vec![],
            source_links: vec![],
            inclusive_event_count: 4,
            last_event_hash: terminal,
            terminal_event_hash: RprovKnown::Known { value: terminal },
            final_tree_hash: RprovKnown::Known { value: final_tree },
        });
        manifest.inventory.extend([
            RprovInventoryEntry {
                path: initial_path,
                byte_length: 1,
                blake3: initial_digest,
                kind: RprovEntryKind::Checkpoint,
            },
            RprovInventoryEntry {
                path: final_path,
                byte_length: 1,
                blake3: final_digest,
                kind: RprovEntryKind::Checkpoint,
            },
            RprovInventoryEntry {
                path: event_path,
                byte_length: 1,
                blake3: event_digest,
                kind: RprovEntryKind::Events,
            },
        ]);
    }
    manifest.latest_session_id = manifest.segments.last().unwrap().session_id.clone();
    manifest.final_tree_hash = manifest.segments.last().unwrap().final_tree_hash.clone();
    manifest.aggregate_event_count = manifest
        .segments
        .iter()
        .map(|segment| segment.inclusive_event_count)
        .sum();
    manifest
        .inventory
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    manifest
}

fn split_declared_bytes(mut total: u64, entry_maximum: u64) -> Vec<u64> {
    let mut result = Vec::new();
    while total > entry_maximum {
        result.push(entry_maximum);
        total -= entry_maximum;
    }
    result.push(total);
    result
}

fn set_checkpoint_lengths(manifest: &mut RprovManifest, index: usize, lengths: &[u64]) {
    assert!(!lengths.is_empty());
    let ordinal = manifest.segments[index].ordinal;
    let prefix = format!("segments/{ordinal:04}/checkpoints/");
    manifest
        .inventory
        .retain(|entry| !entry.path.starts_with(&prefix));

    let segment = &mut manifest.segments[index];
    segment.inclusive_event_count = lengths.len() as u64 + 1;
    segment.checkpoints.clear();
    for (offset, byte_length) in lengths.iter().copied().enumerate() {
        let sequence = offset as u64 + 1;
        let digest = indexed_hash(100_000 + u64::from(ordinal) * 10_000 + sequence);
        let path = format!("segments/{ordinal:04}/checkpoints/{sequence:020}.rcpk");
        let last = offset + 1 == lengths.len();
        segment.checkpoints.push(RprovCheckpointRef {
            role: if offset == 0 {
                RprovCheckpointRole::Initial
            } else if last {
                RprovCheckpointRole::Final
            } else {
                RprovCheckpointRole::Accepted
            },
            format_version: 1,
            entry: path.clone(),
            byte_length,
            blake3: digest,
            owner: RecordedEventRef {
                session_id: segment.session_id.clone(),
                sequence,
                event_hash: indexed_hash(200_000 + u64::from(ordinal) * 10_000 + sequence),
            },
            workspace_hash: if last {
                *segment.final_tree_hash.known().unwrap()
            } else {
                segment.initial_tree_hash
            },
        });
        manifest.inventory.push(RprovInventoryEntry {
            path,
            byte_length,
            blake3: digest,
            kind: RprovEntryKind::Checkpoint,
        });
    }
    manifest.aggregate_event_count = manifest
        .segments
        .iter()
        .map(|segment| segment.inclusive_event_count)
        .sum();
    manifest
        .inventory
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
}

fn set_metadata_lengths(manifest: &mut RprovManifest, index: usize, lengths: &[u64]) {
    let ordinal = manifest.segments[index].ordinal;
    let segment = &mut manifest.segments[index];
    for (offset, byte_length) in lengths.iter().copied().enumerate() {
        let digest = indexed_hash(300_000 + u64::from(ordinal) * 10_000 + offset as u64);
        let path = format!("segments/{ordinal:04}/metadata/{digest}.json");
        segment.metadata.push(RprovMetadataRef {
            format_version: 1,
            entry: path.clone(),
            byte_length,
            blake3: digest,
            owner: RecordedEventRef {
                session_id: segment.session_id.clone(),
                sequence: 2,
                event_hash: indexed_hash(400_000 + u64::from(ordinal)),
            },
        });
        manifest.inventory.push(RprovInventoryEntry {
            path,
            byte_length,
            blake3: digest,
            kind: RprovEntryKind::RuntimeMetadata,
        });
    }
    manifest
        .inventory
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
}

fn set_evidence_lengths(manifest: &mut RprovManifest, index: usize, lengths: &[u64]) {
    let ordinal = manifest.segments[index].ordinal;
    let segment = &mut manifest.segments[index];
    for (offset, byte_length) in lengths.iter().copied().enumerate() {
        let digest = indexed_hash(500_000 + u64::from(ordinal) * 10_000 + offset as u64);
        let path = format!("segments/{ordinal:04}/evidence/{digest}.bin");
        segment.evidence.push(RprovEvidenceRef {
            kind: RprovEvidenceKind::ExternalRecovery,
            entry: path.clone(),
            byte_length,
            blake3: digest,
            usages: vec![RecordedEventRef {
                session_id: segment.session_id.clone(),
                sequence: offset as u64 + 2,
                event_hash: indexed_hash(600_000 + u64::from(ordinal) * 10_000 + offset as u64),
            }],
        });
        manifest.inventory.push(RprovInventoryEntry {
            path,
            byte_length,
            blake3: digest,
            kind: RprovEntryKind::ExternalRecoveryEvidence,
        });
    }
    segment.inclusive_event_count = segment
        .inclusive_event_count
        .max(u64::try_from(lengths.len()).unwrap() + 2);
    manifest.aggregate_event_count = manifest
        .segments
        .iter()
        .map(|segment| segment.inclusive_event_count)
        .sum();
    manifest
        .inventory
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
}

fn evidence_usage_manifest(usage_count: usize) -> RprovManifest {
    assert!(usage_count > 0);
    let mut manifest = clean_manifest();
    let artifact_count = usage_count.div_ceil(MAX_RPROV_EVIDENCE_USAGES_PER_ARTIFACT);
    set_evidence_lengths(&mut manifest, 0, &vec![1; artifact_count]);
    let session_id = manifest.segments[0].session_id.clone();
    let mut sequence = 2_u64;
    for evidence in &mut manifest.segments[0].evidence {
        let remaining = usage_count - usize::try_from(sequence - 2).unwrap();
        let count = remaining.min(MAX_RPROV_EVIDENCE_USAGES_PER_ARTIFACT);
        evidence.usages = (0..count)
            .map(|_| {
                let usage = RecordedEventRef {
                    session_id: session_id.clone(),
                    sequence,
                    event_hash: indexed_hash(700_000 + sequence),
                };
                sequence += 1;
                usage
            })
            .collect();
    }
    let event_count = u64::try_from((usage_count + 2).max(4)).unwrap();
    manifest.segments[0].inclusive_event_count = event_count;
    manifest.aggregate_event_count = event_count;
    manifest
}

fn recovery_gap_manifest(gap_count: usize) -> RprovManifest {
    assert!(gap_count > 0);
    let mut manifest = clean_manifest();
    let event_count = u64::try_from((gap_count + 2).max(4)).unwrap();
    manifest.segments[0].inclusive_event_count = event_count;
    manifest.aggregate_event_count = event_count;
    manifest.package_state = RprovPackageState::RecoveryIncomplete {
        unavailable_assurances: vec![
            RprovUnavailableAssurance::ReferencedEvidence,
            RprovUnavailableAssurance::CleanFinalization,
        ],
        gaps: (2..=gap_count + 1)
            .map(|sequence| RprovRecoveryGap::MissingEvidence {
                event: event_ref(sequence as u64, indexed_hash(800_000 + sequence as u64)),
                blake3: indexed_hash(900_000 + sequence as u64),
            })
            .collect(),
    };
    manifest
}

fn set_event_length(manifest: &mut RprovManifest, index: usize, byte_length: u64) {
    let path = manifest.segments[index].events.entry.clone();
    manifest.segments[index].events.byte_length = byte_length;
    manifest
        .inventory
        .iter_mut()
        .find(|entry| entry.path == path)
        .unwrap()
        .byte_length = byte_length;
}

fn set_source_link_count(manifest: &mut RprovManifest, index: usize, count: usize) {
    let segment = &mut manifest.segments[index];
    segment.inclusive_event_count = (count as u64).max(4);
    segment.source_links = (0..count)
        .map(|offset| RprovSourceLink::LegacyPaste {
            event: RecordedEventRef {
                session_id: segment.session_id.clone(),
                sequence: offset as u64 + 1,
                event_hash: indexed_hash(700_000 + index as u64 * 10_000 + offset as u64),
            },
            verification: RprovLegacyPasteVerification::OriginUnverified,
        })
        .collect();
    manifest.aggregate_event_count = manifest
        .segments
        .iter()
        .map(|segment| segment.inclusive_event_count)
        .sum();
}

fn increment_declared_entry(manifest: &mut RprovManifest, path: &str) {
    manifest
        .inventory
        .iter_mut()
        .find(|entry| entry.path == path)
        .unwrap()
        .byte_length += 1;
}

fn archive_entries(manifest_bytes: &[u8], manifest: &RprovManifest) -> Vec<RprovArchiveEntry> {
    let mut entries = vec![RprovArchiveEntry {
        path: "manifest.json".to_owned(),
        entry_type: RprovArchiveEntryType::RegularFile,
        byte_length: manifest_bytes.len() as u64,
        blake3: None,
    }];
    entries.extend(manifest.inventory.iter().map(|entry| RprovArchiveEntry {
        path: entry.path.clone(),
        entry_type: RprovArchiveEntryType::RegularFile,
        byte_length: entry.byte_length,
        blake3: Some(entry.blake3),
    }));
    entries
}

fn records_bytes(entries: &[RprovArchiveEntry]) -> u64 {
    entries.iter().fold(0, |total, entry| {
        total + RPROV_RECORD_HEADER_BYTES as u64 + entry.path.len() as u64 + entry.byte_length
    })
}

#[test]
fn canonical_container_headers_have_fixed_golden_bytes() {
    let header = RprovContainerHeader {
        format_version: 1,
        entry_count: 5,
        stored_records_bytes: 1_234,
        expanded_records_bytes: 1_234,
    };
    let expected = decode_hex(include_str!("fixtures/rprov/header-v1.hex"));
    assert_eq!(
        encode_rprov_container_header(&header).unwrap().as_slice(),
        expected
    );
    assert_eq!(decode_rprov_container_header(&expected).unwrap(), header);

    let record = RprovRecordHeader {
        path_bytes: 13,
        entry_type: RprovRecordType::RegularFile,
        payload_bytes: 1_234,
    };
    let expected = decode_hex(include_str!("fixtures/rprov/record-header-v1.hex"));
    assert_eq!(
        encode_rprov_record_header(&record).unwrap().as_slice(),
        expected
    );
    assert_eq!(decode_rprov_record_header(&expected).unwrap(), record);
}

#[test]
fn canonical_one_segment_clean_manifest_matches_golden_bytes() {
    let manifest = clean_manifest();
    let encoded = encode_rprov_manifest(&manifest).unwrap();

    let golden = include_bytes!("fixtures/rprov/one-segment-clean-v1.manifest.json");
    let first_difference = encoded
        .iter()
        .zip(golden)
        .position(|(actual, expected)| actual != expected);
    assert_eq!(
        (encoded.len(), first_difference),
        (golden.len(), None),
        "actual={}\ngolden={}",
        String::from_utf8_lossy(&encoded),
        String::from_utf8_lossy(golden)
    );
    assert_eq!(decode_rprov_manifest(&encoded).unwrap(), manifest);
    assert_eq!(encode_rprov_manifest(&manifest).unwrap(), encoded);
}

#[test]
fn test_case_suite_hash_is_optional_and_canonical() {
    let mut manifest = clean_manifest();
    manifest.test_case_suite_hash = Some(hash(42));

    let encoded = encode_rprov_manifest(&manifest).unwrap();
    let decoded = decode_rprov_manifest(&encoded).unwrap();

    assert_eq!(decoded.test_case_suite_hash, Some(hash(42)));
    assert_eq!(encode_rprov_manifest(&decoded).unwrap(), encoded);
    assert!(String::from_utf8(encoded).unwrap().contains(
        "\"test_case_suite_hash\":\"2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a\""
    ));
}

#[test]
fn unknown_outer_versions_are_rejected_before_version_specific_decoding() {
    let mut header = encode_rprov_container_header(&RprovContainerHeader {
        format_version: 1,
        entry_count: 1,
        stored_records_bytes: 16 + 13,
        expanded_records_bytes: 16 + 13,
    })
    .unwrap();
    header[8..12].copy_from_slice(&2_u32.to_le_bytes());
    assert!(matches!(
        decode_rprov_container_header(&header),
        Err(RprovError::UnsupportedFormatVersion {
            found: 2,
            supported: 1
        })
    ));

    let encoded = encode_rprov_manifest(&clean_manifest()).unwrap();
    let unknown = String::from_utf8(encoded).unwrap().replacen(
        "\"format_version\":1",
        "\"format_version\":2",
        1,
    );
    assert!(matches!(
        decode_rprov_manifest(unknown.as_bytes()),
        Err(RprovError::UnsupportedFormatVersion {
            found: 2,
            supported: 1
        })
    ));
    assert!(matches!(
        decode_rprov_manifest(include_bytes!(
            "fixtures/rprov/malformed-unknown-version.json"
        )),
        Err(RprovError::UnsupportedFormatVersion {
            found: 2,
            supported: 1
        })
    ));
}

#[test]
fn strict_manifest_decode_rejects_noncanonical_and_invalid_schema() {
    let encoded = String::from_utf8(encode_rprov_manifest(&clean_manifest()).unwrap()).unwrap();
    let invalid = [
        format!(" {encoded}"),
        format!("{encoded}\n"),
        encoded.replacen(
            "{\"format_version\":1,\"package_state\"",
            "{\"package_state\":{\"state\":\"clean_finalized\"},\"format_version\":1,\"package_state_copy\"",
            1,
        ),
        encoded.replacen(
            "\"format_version\":1",
            "\"format_version\":1,\"format_version\":1",
            1,
        ),
        encoded.replacen("\"student_id\":\"student-1\",", "", 1),
        encoded.replacen("\"aggregate_event_count\":4", "\"aggregate_event_count\":\"4\"", 1),
        encoded.replacen("\"student_id\":\"student-1\"", "\"student_id\":\"student-1\",\"extra\":false", 1),
        encoded.replacen("\"aggregate_event_count\":4", "\"aggregate_event_count\":18446744073709551616", 1),
    ];
    for bytes in invalid {
        assert!(
            decode_rprov_manifest(bytes.as_bytes()).is_err(),
            "accepted invalid manifest: {bytes}"
        );
    }
    let mut invalid_utf8 = encode_rprov_manifest(&clean_manifest()).unwrap();
    invalid_utf8[0] = 0xff;
    assert!(decode_rprov_manifest(&invalid_utf8).is_err());
    assert!(
        decode_rprov_manifest(include_bytes!(
            "fixtures/rprov/malformed-duplicate-version.json"
        ))
        .is_err()
    );
}

#[test]
fn layout_inventory_is_exact_complete_ordered_and_regular_file_only() {
    let manifest = clean_manifest();
    let manifest_bytes = encode_rprov_manifest(&manifest).unwrap();
    let entries = archive_entries(&manifest_bytes, &manifest);
    let length = records_bytes(&entries);
    let header = RprovContainerHeader {
        format_version: 1,
        entry_count: entries.len() as u32,
        stored_records_bytes: length,
        expanded_records_bytes: length,
    };
    validate_rprov_layout(&header, &manifest_bytes, &manifest, &entries).unwrap();

    let mut missing = entries.clone();
    missing.pop();
    assert!(validate_rprov_layout(&header, &manifest_bytes, &manifest, &missing).is_err());

    let mut duplicate = entries.clone();
    duplicate.push(duplicate[1].clone());
    assert!(validate_rprov_layout(&header, &manifest_bytes, &manifest, &duplicate).is_err());

    let mut reordered = entries.clone();
    reordered.swap(1, 2);
    assert!(validate_rprov_layout(&header, &manifest_bytes, &manifest, &reordered).is_err());

    let mut wrong_length = entries.clone();
    wrong_length[1].byte_length += 1;
    assert!(validate_rprov_layout(&header, &manifest_bytes, &manifest, &wrong_length).is_err());

    let mut wrong_digest = entries.clone();
    wrong_digest[1].blake3 = Some(hash(99));
    assert!(validate_rprov_layout(&header, &manifest_bytes, &manifest, &wrong_digest).is_err());

    for entry_type in [
        RprovArchiveEntryType::Directory,
        RprovArchiveEntryType::Symlink,
        RprovArchiveEntryType::HardLink,
        RprovArchiveEntryType::Special,
    ] {
        let mut hostile = entries.clone();
        hostile[1].entry_type = entry_type;
        assert!(validate_rprov_layout(&header, &manifest_bytes, &manifest, &hostile).is_err());
    }
}

#[test]
fn archive_names_reject_aliases_traversal_and_forbidden_layouts() {
    let invalid = [
        "",
        "/manifest.json",
        "C:manifest.json",
        "../manifest.json",
        "segments/./0001/events.jsonl",
        "segments/0001/../0002/events.jsonl",
        "segments//0001/events.jsonl",
        "segments\\0001\\events.jsonl",
        "segments/0001/events.jsonl/",
        "final-workspace/src/main.rs",
        "segments/0001/prior.rprov",
        "segments/0001/prior.zip",
        "segments/cafe\u{301}/events.jsonl",
    ];
    for path in invalid {
        assert!(
            validate_rprov_archive_path(path).is_err(),
            "accepted {path:?}"
        );
    }
    assert!(validate_rprov_archive_path("segments/0001/events.jsonl").is_ok());

    let exact_component = "x".repeat(MAX_RPROV_ARCHIVE_COMPONENT_BYTES);
    assert!(validate_rprov_archive_path(&exact_component).is_ok());
    let exact_depth = std::iter::repeat_n("x", MAX_RPROV_ARCHIVE_PATH_DEPTH)
        .collect::<Vec<_>>()
        .join("/");
    assert!(validate_rprov_archive_path(&exact_depth).is_ok());
    let exact_path = [32_usize, 31, 31, 31, 31, 31]
        .into_iter()
        .map(|length| "x".repeat(length))
        .collect::<Vec<_>>()
        .join("/");
    assert_eq!(exact_path.len(), MAX_RPROV_ARCHIVE_PATH_BYTES);
    assert!(validate_rprov_archive_path(&exact_path).is_ok());

    let long_component = "x".repeat(MAX_RPROV_ARCHIVE_COMPONENT_BYTES + 1);
    assert!(validate_rprov_archive_path(&long_component).is_err());
    let too_deep = std::iter::repeat_n("x", MAX_RPROV_ARCHIVE_PATH_DEPTH + 1)
        .collect::<Vec<_>>()
        .join("/");
    assert!(validate_rprov_archive_path(&too_deep).is_err());
    let too_long = format!(
        "{}/{}",
        "a/".repeat(MAX_RPROV_ARCHIVE_PATH_DEPTH - 1),
        "x".repeat(MAX_RPROV_ARCHIVE_PATH_BYTES)
    );
    assert!(validate_rprov_archive_path(&too_long).is_err());
}

#[test]
fn clean_and_recovery_states_are_structurally_distinct() {
    let clean = clean_manifest();
    let mut recovery = clean.clone();
    recovery.package_state = RprovPackageState::RecoveryIncomplete {
        unavailable_assurances: vec![
            RprovUnavailableAssurance::CompleteEventStream,
            RprovUnavailableAssurance::FinalCheckpoint,
            RprovUnavailableAssurance::FinalTree,
            RprovUnavailableAssurance::CleanFinalization,
        ],
        gaps: vec![],
    };
    recovery.segments[0].events.completeness = RprovEventStreamCompleteness::PrefixOnly;
    recovery.segments[0].checkpoints.pop();
    recovery.inventory.remove(2);
    recovery.segments[0].terminal_event_hash = RprovKnown::Unknown;
    recovery.segments[0].final_tree_hash = RprovKnown::Unknown;
    recovery.final_tree_hash = RprovKnown::Unknown;

    let clean_bytes = encode_rprov_manifest(&clean).unwrap();
    let recovery_bytes = encode_rprov_manifest(&recovery).unwrap();
    assert_ne!(clean_bytes, recovery_bytes);
    assert_eq!(decode_rprov_manifest(&recovery_bytes).unwrap(), recovery);

    let mut mislabeled = recovery;
    mislabeled.package_state = RprovPackageState::CleanFinalized;
    assert!(encode_rprov_manifest(&mislabeled).is_err());

    let mut empty_recovery = clean;
    empty_recovery.package_state = RprovPackageState::RecoveryIncomplete {
        unavailable_assurances: vec![],
        gaps: vec![],
    };
    assert!(encode_rprov_manifest(&empty_recovery).is_err());
}

#[test]
fn recovery_state_cannot_make_declared_final_facts_contradictory() {
    let recovery_state = RprovPackageState::RecoveryIncomplete {
        unavailable_assurances: vec![RprovUnavailableAssurance::CleanFinalization],
        gaps: vec![],
    };

    let mut wrong_terminal = clean_manifest();
    wrong_terminal.package_state = recovery_state.clone();
    wrong_terminal.segments[0].terminal_event_hash = RprovKnown::Known { value: hash(88) };
    assert!(wrong_terminal.validate().is_err());

    let mut wrong_checkpoint = clean_manifest();
    wrong_checkpoint.package_state = recovery_state.clone();
    wrong_checkpoint.segments[0].checkpoints[1].workspace_hash = hash(88);
    assert!(wrong_checkpoint.validate().is_err());

    let mut wrong_package_tip = clean_manifest();
    wrong_package_tip.package_state = recovery_state;
    wrong_package_tip.final_tree_hash = RprovKnown::Known { value: hash(88) };
    assert!(wrong_package_tip.validate().is_err());

    let mut prefix_claiming_final = clean_manifest();
    prefix_claiming_final.package_state = RprovPackageState::RecoveryIncomplete {
        unavailable_assurances: vec![
            RprovUnavailableAssurance::CompleteEventStream,
            RprovUnavailableAssurance::CleanFinalization,
        ],
        gaps: vec![],
    };
    prefix_claiming_final.segments[0].events.completeness =
        RprovEventStreamCompleteness::PrefixOnly;
    assert!(prefix_claiming_final.validate().is_err());
}

#[test]
fn marked_missing_ancestry_retains_a_known_segment_only_in_recovery() {
    let mut recovery = clean_manifest();
    recovery.package_state = RprovPackageState::RecoveryIncomplete {
        unavailable_assurances: vec![
            RprovUnavailableAssurance::CompleteAncestry,
            RprovUnavailableAssurance::CleanFinalization,
        ],
        gaps: vec![RprovRecoveryGap::MissingAncestry {
            before_session_id: session("session-1"),
        }],
    };
    recovery.segments[0].initial_tree_hash = hash(77);
    recovery.segments[0].checkpoints[0].workspace_hash = hash(77);

    recovery.validate().unwrap();
    let encoded = encode_rprov_manifest(&recovery).unwrap();
    assert_eq!(decode_rprov_manifest(&encoded).unwrap(), recovery);

    let mut clean = recovery.clone();
    clean.package_state = RprovPackageState::CleanFinalized;
    assert!(clean.validate().is_err());

    let mut unmarked = recovery.clone();
    let RprovPackageState::RecoveryIncomplete { gaps, .. } = &mut unmarked.package_state else {
        unreachable!()
    };
    gaps.clear();
    assert!(unmarked.validate().is_err());

    let mut contradictory = recovery;
    contradictory.segments[0].parent = Some(RprovParentLink {
        session_id: session("missing-parent"),
        terminal_event_hash: hash(78),
        final_tree_hash: hash(79),
    });
    assert!(contradictory.validate().is_err());
}

#[test]
fn counts_and_aggregate_bounds_are_inclusive_and_checked() {
    let mut manifest = clean_manifest();
    manifest.segments[0].inclusive_event_count = MAX_RPROV_EVENTS;
    manifest.aggregate_event_count = MAX_RPROV_EVENTS;
    assert!(manifest.validate().is_ok());
    manifest.segments[0].inclusive_event_count += 1;
    manifest.aggregate_event_count += 1;
    assert!(manifest.validate().is_err());

    let mut manifest = clean_manifest();
    manifest.segments[0].events.byte_length = MAX_RPROV_SEGMENT_EVENTS_BYTES;
    manifest.inventory[3].byte_length = MAX_RPROV_SEGMENT_EVENTS_BYTES;
    assert!(manifest.validate().is_ok());
    manifest.segments[0].events.byte_length += 1;
    manifest.inventory[3].byte_length += 1;
    assert!(manifest.validate().is_err());

    let mut manifest = clean_manifest();
    let template = manifest.segments[0].clone();
    manifest.segments = (0..=MAX_RPROV_SEGMENTS)
        .map(|index| {
            let mut segment = template.clone();
            segment.ordinal = index as u32 + 1;
            segment.session_id = session(&format!("s-{index}"));
            segment
        })
        .collect();
    assert!(manifest.validate().is_err());
}

#[test]
fn evidence_usage_and_recovery_gap_limits_are_inclusive() {
    let mut per_artifact = evidence_usage_manifest(MAX_RPROV_EVIDENCE_USAGES_PER_ARTIFACT);
    assert!(per_artifact.validate().is_ok());
    let extra_usage = per_artifact.segments[0].evidence[0].usages[0].clone();
    per_artifact.segments[0].evidence[0]
        .usages
        .push(extra_usage);
    assert!(per_artifact.validate().is_err());

    let aggregate = evidence_usage_manifest(MAX_RPROV_EVIDENCE_USAGES);
    assert!(aggregate.validate().is_ok());
    let mut aggregate_plus_one = aggregate;
    let extra_usage = aggregate_plus_one.segments[0].evidence[0].usages[0].clone();
    aggregate_plus_one.segments[0].evidence[0]
        .usages
        .push(extra_usage);
    assert!(aggregate_plus_one.validate().is_err());

    let recovery = recovery_gap_manifest(MAX_RPROV_RECOVERY_GAPS);
    assert!(recovery.validate().is_ok());
    let mut recovery_plus_one = recovery;
    let extra_gap = match &mut recovery_plus_one.package_state {
        RprovPackageState::RecoveryIncomplete { gaps, .. } => gaps[0].clone(),
        RprovPackageState::CleanFinalized => unreachable!(),
    };
    let RprovPackageState::RecoveryIncomplete { gaps, .. } = &mut recovery_plus_one.package_state
    else {
        unreachable!()
    };
    gaps.push(extra_gap);
    assert!(recovery_plus_one.validate().is_err());
}

#[test]
fn aggregate_payload_byte_limits_are_inclusive_without_large_allocations() {
    let mut event_bytes = linear_manifest(2);
    set_event_length(&mut event_bytes, 0, MAX_RPROV_SEGMENT_EVENTS_BYTES);
    set_event_length(
        &mut event_bytes,
        1,
        MAX_RPROV_EVENTS_BYTES - MAX_RPROV_SEGMENT_EVENTS_BYTES,
    );
    assert!(event_bytes.validate().is_ok());
    set_event_length(
        &mut event_bytes,
        1,
        MAX_RPROV_EVENTS_BYTES - MAX_RPROV_SEGMENT_EVENTS_BYTES + 1,
    );
    assert!(event_bytes.validate().is_err());

    let mut checkpoint_bytes = linear_manifest(3);
    let checkpoint_share = MAX_RPROV_CHECKPOINT_BYTES / 3;
    for index in 0..3 {
        let total = if index == 2 {
            MAX_RPROV_CHECKPOINT_BYTES - 2 * checkpoint_share
        } else {
            checkpoint_share
        };
        set_checkpoint_lengths(
            &mut checkpoint_bytes,
            index,
            &split_declared_bytes(total, MAX_RPROV_CHECKPOINT_ENCODED_BYTES),
        );
    }
    assert!(checkpoint_bytes.validate().is_ok());
    let checkpoint = checkpoint_bytes.segments[2].checkpoints.last_mut().unwrap();
    checkpoint.byte_length += 1;
    let checkpoint_path = checkpoint.entry.clone();
    increment_declared_entry(&mut checkpoint_bytes, &checkpoint_path);
    assert!(checkpoint_bytes.validate().is_err());

    let mut evidence_bytes = linear_manifest(9);
    let evidence_share = MAX_RPROV_EVIDENCE_BYTES / 9;
    for index in 0..9 {
        let total = if index == 8 {
            MAX_RPROV_EVIDENCE_BYTES - 8 * evidence_share
        } else {
            evidence_share
        };
        set_evidence_lengths(
            &mut evidence_bytes,
            index,
            &split_declared_bytes(total, MAX_RPROV_EVIDENCE_ENTRY_BYTES),
        );
    }
    assert!(evidence_bytes.validate().is_ok());
    let evidence = evidence_bytes.segments[8].evidence.last_mut().unwrap();
    evidence.byte_length += 1;
    let evidence_path = evidence.entry.clone();
    increment_declared_entry(&mut evidence_bytes, &evidence_path);
    assert!(evidence_bytes.validate().is_err());

    let mut metadata_bytes = linear_manifest(9);
    let metadata_share = MAX_RPROV_METADATA_BYTES / 9;
    for index in 0..9 {
        let total = if index == 8 {
            MAX_RPROV_METADATA_BYTES - 8 * metadata_share
        } else {
            metadata_share
        };
        set_metadata_lengths(
            &mut metadata_bytes,
            index,
            &split_declared_bytes(total, MAX_RPROV_METADATA_ENTRY_BYTES),
        );
    }
    assert!(metadata_bytes.validate().is_ok());
    let metadata = metadata_bytes.segments[8].metadata.last_mut().unwrap();
    metadata.byte_length += 1;
    let metadata_path = metadata.entry.clone();
    increment_declared_entry(&mut metadata_bytes, &metadata_path);
    assert!(metadata_bytes.validate().is_err());
}

#[test]
fn per_segment_and_collection_limits_are_inclusive_without_payload_allocations() {
    let mut checkpoint_bytes = clean_manifest();
    set_checkpoint_lengths(
        &mut checkpoint_bytes,
        0,
        &split_declared_bytes(
            MAX_RPROV_SEGMENT_CHECKPOINT_BYTES,
            MAX_RPROV_CHECKPOINT_ENCODED_BYTES,
        ),
    );
    assert!(checkpoint_bytes.validate().is_ok());
    let checkpoint = checkpoint_bytes.segments[0].checkpoints.last_mut().unwrap();
    checkpoint.byte_length += 1;
    let checkpoint_path = checkpoint.entry.clone();
    increment_declared_entry(&mut checkpoint_bytes, &checkpoint_path);
    assert!(checkpoint_bytes.validate().is_err());

    let mut checkpoint_count = clean_manifest();
    set_checkpoint_lengths(
        &mut checkpoint_count,
        0,
        &vec![0; MAX_RPROV_CHECKPOINTS_PER_SEGMENT],
    );
    assert!(checkpoint_count.validate().is_ok());
    set_checkpoint_lengths(
        &mut checkpoint_count,
        0,
        &vec![0; MAX_RPROV_CHECKPOINTS_PER_SEGMENT + 1],
    );
    assert!(checkpoint_count.validate().is_err());

    let mut evidence_bytes = clean_manifest();
    set_evidence_lengths(
        &mut evidence_bytes,
        0,
        &split_declared_bytes(
            MAX_RPROV_SEGMENT_EVIDENCE_BYTES,
            MAX_RPROV_EVIDENCE_ENTRY_BYTES,
        ),
    );
    assert!(evidence_bytes.validate().is_ok());
    let evidence = evidence_bytes.segments[0].evidence.last_mut().unwrap();
    evidence.byte_length += 1;
    let evidence_path = evidence.entry.clone();
    increment_declared_entry(&mut evidence_bytes, &evidence_path);
    assert!(evidence_bytes.validate().is_err());

    let mut evidence_count = clean_manifest();
    set_evidence_lengths(
        &mut evidence_count,
        0,
        &vec![0; MAX_RPROV_EVIDENCE_PER_SEGMENT],
    );
    assert!(evidence_count.validate().is_ok());
    set_evidence_lengths(&mut evidence_count, 0, &[0]);
    assert!(evidence_count.validate().is_err());

    let mut metadata_bytes = clean_manifest();
    let mut exact_metadata = vec![MAX_RPROV_METADATA_ENTRY_BYTES; 8];
    exact_metadata.push(0);
    set_metadata_lengths(&mut metadata_bytes, 0, &exact_metadata);
    assert!(metadata_bytes.validate().is_ok());
    let metadata = metadata_bytes.segments[0].metadata.last_mut().unwrap();
    metadata.byte_length += 1;
    let metadata_path = metadata.entry.clone();
    increment_declared_entry(&mut metadata_bytes, &metadata_path);
    assert!(metadata_bytes.validate().is_err());

    let mut metadata_count = clean_manifest();
    set_metadata_lengths(
        &mut metadata_count,
        0,
        &vec![0; MAX_RPROV_METADATA_PER_SEGMENT],
    );
    assert!(metadata_count.validate().is_ok());
    set_metadata_lengths(&mut metadata_count, 0, &[0]);
    assert!(metadata_count.validate().is_err());

    let mut source_count = clean_manifest();
    set_source_link_count(&mut source_count, 0, MAX_RPROV_SOURCE_LINKS_PER_SEGMENT);
    assert!(source_count.validate().is_ok());
    set_source_link_count(&mut source_count, 0, MAX_RPROV_SOURCE_LINKS_PER_SEGMENT + 1);
    assert!(source_count.validate().is_err());

    let mut aggregate_sources = linear_manifest(3);
    set_source_link_count(&mut aggregate_sources, 0, 2_731);
    set_source_link_count(&mut aggregate_sources, 1, 2_731);
    set_source_link_count(&mut aggregate_sources, 2, 2_730);
    assert_eq!(
        aggregate_sources
            .segments
            .iter()
            .map(|segment| segment.source_links.len())
            .sum::<usize>(),
        MAX_RPROV_SOURCE_LINKS
    );
    assert!(aggregate_sources.validate().is_ok());
    set_source_link_count(&mut aggregate_sources, 2, 2_731);
    assert!(aggregate_sources.validate().is_err());

    let mut starter_files = clean_manifest();
    let starter_entry = starter_files.initial_workspace.files[0].entry.clone();
    starter_files.initial_workspace.files = (0..MAX_RPROV_INITIAL_FILES)
        .map(|index| RprovInitialWorkspaceFile {
            path: WorkspacePath::new(format!("file-{index:03}.rs")).unwrap(),
            entry: starter_entry.clone(),
        })
        .collect();
    assert!(starter_files.validate().is_ok());
    starter_files
        .initial_workspace
        .files
        .push(RprovInitialWorkspaceFile {
            path: WorkspacePath::new("file-999.rs").unwrap(),
            entry: starter_entry,
        });
    assert!(starter_files.validate().is_err());

    let mut starter_file_bytes = clean_manifest();
    starter_file_bytes.inventory[0].byte_length = MAX_RPROV_INITIAL_FILE_BYTES;
    assert!(starter_file_bytes.validate().is_ok());
    starter_file_bytes.inventory[0].byte_length += 1;
    assert!(starter_file_bytes.validate().is_err());

    let mut starter_total = clean_manifest();
    starter_total.initial_workspace.files.clear();
    starter_total
        .inventory
        .retain(|entry| entry.kind != RprovEntryKind::InitialWorkspaceBlob);
    for index in 0..11_u64 {
        let digest = indexed_hash(800_000 + index);
        let path = format!("initial-workspace/blobs/{digest}");
        let byte_length = if index < 10 {
            MAX_RPROV_INITIAL_FILE_BYTES
        } else {
            0
        };
        starter_total
            .initial_workspace
            .files
            .push(RprovInitialWorkspaceFile {
                path: WorkspacePath::new(format!("file-{index:03}.rs")).unwrap(),
                entry: path.clone(),
            });
        starter_total.inventory.push(RprovInventoryEntry {
            path,
            byte_length,
            blake3: digest,
            kind: RprovEntryKind::InitialWorkspaceBlob,
        });
    }
    starter_total
        .inventory
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    assert!(starter_total.validate().is_ok());
    let last_starter = starter_total
        .initial_workspace
        .files
        .last()
        .unwrap()
        .entry
        .clone();
    increment_declared_entry(&mut starter_total, &last_starter);
    assert!(starter_total.validate().is_err());
}

#[test]
fn empty_starter_is_representable_and_assignment_identities_must_match() {
    let mut empty = clean_manifest();
    empty.initial_workspace.files.clear();
    empty
        .inventory
        .retain(|entry| entry.kind != RprovEntryKind::InitialWorkspaceBlob);
    assert!(empty.validate().is_ok());

    let mut wrong_course = clean_manifest();
    wrong_course.segments[0].course_id = "other-course".to_owned();
    assert!(wrong_course.validate().is_err());

    let mut wrong_assignment_manifest = clean_manifest();
    wrong_assignment_manifest.segments[0].assignment_manifest_blake3 = hash(88);
    assert!(wrong_assignment_manifest.validate().is_err());

    let mut wrong_original_starter = clean_manifest();
    wrong_original_starter.segments[0].original_starter_tree_hash = hash(88);
    assert!(wrong_original_starter.validate().is_err());
}

#[test]
fn manifest_raw_and_json_structure_limits_reject_limit_plus_one() {
    let mut exact = b"{\"format_version\":1,\"x\":0}".to_vec();
    exact.resize(MAX_RPROV_MANIFEST_BYTES, b' ');
    assert!(!matches!(
        decode_rprov_manifest(&exact),
        Err(RprovError::ManifestTooLarge { .. })
    ));

    let oversized = vec![b' '; MAX_RPROV_MANIFEST_BYTES + 1];
    assert!(matches!(
        decode_rprov_manifest(&oversized),
        Err(RprovError::ManifestTooLarge { .. })
    ));

    let open = "[".repeat(MAX_RPROV_JSON_NESTING + 1);
    let close = "]".repeat(MAX_RPROV_JSON_NESTING + 1);
    let nested = format!("{{\"format_version\":1,\"x\":{open}0{close} }}");
    assert!(decode_rprov_manifest(nested.as_bytes()).is_err());
}

#[test]
fn canonical_two_segment_manifest_preserves_root_to_tip_identities() {
    let manifest = two_segment_manifest();
    let encoded = encode_rprov_manifest(&manifest).unwrap();
    assert_eq!(
        encoded,
        include_bytes!("fixtures/rprov/two-segment-linear-v1.manifest.json")
    );
    assert_eq!(decode_rprov_manifest(&encoded).unwrap(), manifest);

    let root = &manifest.segments[0];
    let child = &manifest.segments[1];
    let parent = child.parent.as_ref().unwrap();
    assert_eq!(root.ordinal, 1);
    assert_eq!(child.ordinal, 2);
    assert_eq!(parent.session_id, root.session_id);
    assert_eq!(parent.terminal_event_hash, root.last_event_hash);
    assert_eq!(
        parent.final_tree_hash,
        *root.final_tree_hash.known().unwrap()
    );
    assert_eq!(child.initial_tree_hash, parent.final_tree_hash);
    assert_eq!(manifest.aggregate_event_count, 8);
    assert_eq!(manifest.latest_session_id, child.session_id);
    assert_eq!(
        manifest.final_tree_hash.known(),
        child.final_tree_hash.known()
    );
}

#[test]
fn linear_ancestry_rejects_wrong_parent_cycle_and_starter_links() {
    let valid = two_segment_manifest();

    let mut wrong_session = valid.clone();
    wrong_session.segments[1]
        .parent
        .as_mut()
        .unwrap()
        .session_id = session("unrelated");
    assert!(wrong_session.validate().is_err());

    let mut wrong_terminal = valid.clone();
    wrong_terminal.segments[1]
        .parent
        .as_mut()
        .unwrap()
        .terminal_event_hash = hash(88);
    assert!(wrong_terminal.validate().is_err());

    let mut wrong_final = valid.clone();
    wrong_final.segments[1]
        .parent
        .as_mut()
        .unwrap()
        .final_tree_hash = hash(88);
    assert!(wrong_final.validate().is_err());

    let mut cycle = valid.clone();
    cycle.segments[1].parent.as_mut().unwrap().session_id = session("session-2");
    assert!(cycle.validate().is_err());

    let mut wrong_child_start = valid.clone();
    wrong_child_start.segments[1].initial_tree_hash = hash(88);
    assert!(wrong_child_start.validate().is_err());

    let mut wrong_child_checkpoint = valid.clone();
    wrong_child_checkpoint.segments[1].checkpoints[0].workspace_hash = hash(88);
    assert!(wrong_child_checkpoint.validate().is_err());

    let mut wrong_root = valid;
    wrong_root.segments[0].initial_tree_hash = hash(88);
    assert!(wrong_root.validate().is_err());
}

#[test]
fn segment_order_latest_tip_and_terminal_identity_are_not_aliasable() {
    let valid = two_segment_manifest();

    let mut missing_parent = valid.clone();
    missing_parent.segments[1].parent = None;
    assert!(missing_parent.validate().is_err());

    let mut duplicate_session = valid.clone();
    duplicate_session.segments[1].session_id = session("session-1");
    assert!(duplicate_session.validate().is_err());

    let mut reordered = valid.clone();
    reordered.segments.swap(0, 1);
    assert!(reordered.validate().is_err());

    let mut wrong_latest = valid.clone();
    wrong_latest.latest_session_id = session("session-1");
    assert!(wrong_latest.validate().is_err());

    let mut wrong_tip_hash = valid;
    wrong_tip_hash.final_tree_hash = RprovKnown::Known { value: hash(88) };
    assert!(wrong_tip_hash.validate().is_err());
}

fn push_event(events: &mut Vec<EventEnvelope>, session_id: &SessionId, event: Event) {
    let sequence = events.len() as u64 + 1;
    let previous = events
        .last()
        .map_or_else(Hash::zero, |envelope| envelope.event_hash);
    let envelope = EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: session_id.clone(),
        sequence,
        monotonic_millis: sequence * 10,
        wall_clock_utc: None,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event,
    }
    .seal(previous)
    .unwrap();
    events.push(envelope);
}

fn event_reference(event: &EventEnvelope) -> RecordedEventRef {
    RecordedEventRef {
        session_id: event.session_id.clone(),
        sequence: event.sequence,
        event_hash: event.event_hash,
    }
}

fn linked_event_stream() -> Vec<EventEnvelope> {
    let session_id = session("session-1");
    let mut events = Vec::new();
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: hash(1),
            documents: vec![],
        }),
    );
    let prefix = event_reference(&events[0]);
    push_event(
        &mut events,
        &session_id,
        Event::ClipboardCopied(ClipboardSource {
            prefix,
            document_id: DocumentId::new("source-doc").unwrap(),
            path: WorkspacePath::new("src/main.rs").unwrap(),
            version: 7,
            content_hash: document_hash("secret"),
            start_byte: 0,
            end_byte: 6,
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::FileDeleted(FileDeleted {
            document_id: DocumentId::new("source-doc").unwrap(),
            path: WorkspacePath::new("src/main.rs").unwrap(),
            previous_hash: document_hash("secret"),
        }),
    );
    let copied = event_reference(&events[1]);
    push_event(
        &mut events,
        &session_id,
        Event::InternalPaste(InternalPaste {
            source: copied,
            transaction: EditorTransaction {
                document_id: DocumentId::new("destination-doc").unwrap(),
                version_before: 1,
                version_after: 2,
                origin: EditOrigin::Paste,
                edits: vec![TextEdit {
                    start_byte: 0,
                    end_byte: 0,
                    inserted_text: "secret".to_owned(),
                }],
                selection_before: SelectionState::caret(0),
                selection_after: SelectionState::caret(6),
                hash_before: document_hash(""),
                hash_after: document_hash("secret"),
            },
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::FileEdited(FileEdited {
            document_id: DocumentId::new("destination-doc").unwrap(),
            version_before: 2,
            version_after: 3,
            origin: EditOrigin::Paste,
            edits: vec![TextEdit {
                start_byte: 6,
                end_byte: 6,
                inserted_text: "x".to_owned(),
            }],
            selection_before: SelectionState::caret(6),
            selection_after: SelectionState::caret(7),
            hash_before: document_hash("secret"),
            hash_after: document_hash("secretx"),
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::PasteRejected(PasteRejected {
            reason: PasteRejectionReason::ExternalInput,
            channel: PasteInputChannel::TerminalBracketed,
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: hash(4),
            documents: vec![],
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: hash(4),
            event_count: 8,
            clean: true,
            warnings: vec![],
        }),
    );
    events
}

fn jsonl(events: &[EventEnvelope]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for event in events {
        bytes.extend(encode_envelope(event).unwrap());
        bytes.push(b'\n');
    }
    bytes
}

fn payload_manifest() -> (RprovManifest, Vec<EventEnvelope>, Vec<u8>, Vec<Vec<u8>>) {
    let events = linked_event_stream();
    let event_bytes = jsonl(&events);
    let starter = b"fn source() {}\n".to_vec();
    let checkpoints = [
        b"RUSTCPK\0\0\0\0\x01initial".to_vec(),
        b"RUSTCPK\0\0\0\0\x01final".to_vec(),
    ];
    let starter_digest = rprov_raw_blake3(&starter);
    let event_digest = rprov_raw_blake3(&event_bytes);
    let initial_digest = rprov_raw_blake3(&checkpoints[0]);
    let final_digest = rprov_raw_blake3(&checkpoints[1]);
    let mut manifest = clean_manifest();
    manifest.aggregate_event_count = events.len() as u64;
    manifest.initial_workspace.files[0].entry = format!("initial-workspace/blobs/{starter_digest}");
    let segment = &mut manifest.segments[0];
    segment.events.byte_length = event_bytes.len() as u64;
    segment.events.blake3 = event_digest;
    segment.inclusive_event_count = events.len() as u64;
    segment.last_event_hash = events.last().unwrap().event_hash;
    segment.terminal_event_hash = RprovKnown::Known {
        value: events.last().unwrap().event_hash,
    };
    segment.checkpoints = vec![
        RprovCheckpointRef {
            role: RprovCheckpointRole::Initial,
            format_version: 1,
            entry: "segments/0001/checkpoints/00000000000000000001.rcpk".to_owned(),
            byte_length: checkpoints[0].len() as u64,
            blake3: initial_digest,
            owner: event_reference(&events[0]),
            workspace_hash: hash(1),
        },
        RprovCheckpointRef {
            role: RprovCheckpointRole::Final,
            format_version: 1,
            entry: "segments/0001/checkpoints/00000000000000000007.rcpk".to_owned(),
            byte_length: checkpoints[1].len() as u64,
            blake3: final_digest,
            owner: event_reference(&events[6]),
            workspace_hash: hash(4),
        },
    ];
    segment.source_links = vec![
        RprovSourceLink::InternalPaste {
            paste_event: event_reference(&events[3]),
            copied_event: event_reference(&events[1]),
            document_id: DocumentId::new("source-doc").unwrap(),
            path: WorkspacePath::new("src/main.rs").unwrap(),
            version: 7,
            content_hash: document_hash("secret"),
            start_byte: 0,
            end_byte: 6,
        },
        RprovSourceLink::LegacyPaste {
            event: event_reference(&events[4]),
            verification: RprovLegacyPasteVerification::OriginUnverified,
        },
    ];
    manifest.inventory = vec![
        RprovInventoryEntry {
            path: format!("initial-workspace/blobs/{starter_digest}"),
            byte_length: starter.len() as u64,
            blake3: starter_digest,
            kind: RprovEntryKind::InitialWorkspaceBlob,
        },
        RprovInventoryEntry {
            path: "segments/0001/checkpoints/00000000000000000001.rcpk".to_owned(),
            byte_length: checkpoints[0].len() as u64,
            blake3: initial_digest,
            kind: RprovEntryKind::Checkpoint,
        },
        RprovInventoryEntry {
            path: "segments/0001/checkpoints/00000000000000000007.rcpk".to_owned(),
            byte_length: checkpoints[1].len() as u64,
            blake3: final_digest,
            kind: RprovEntryKind::Checkpoint,
        },
        RprovInventoryEntry {
            path: "segments/0001/events.jsonl".to_owned(),
            byte_length: event_bytes.len() as u64,
            blake3: event_digest,
            kind: RprovEntryKind::Events,
        },
    ];
    (
        manifest,
        events,
        event_bytes,
        vec![starter, checkpoints[0].clone(), checkpoints[1].clone()],
    )
}

fn replace_event_payload(manifest: &mut RprovManifest, bytes: &[u8]) {
    replace_segment_event_payload(manifest, 0, bytes);
}

fn replace_segment_event_payload(manifest: &mut RprovManifest, index: usize, bytes: &[u8]) {
    let digest = rprov_raw_blake3(bytes);
    let path = manifest.segments[index].events.entry.clone();
    manifest.segments[index].events.byte_length = bytes.len() as u64;
    manifest.segments[index].events.blake3 = digest;
    let inventory = manifest
        .inventory
        .iter_mut()
        .find(|entry| entry.path == path)
        .unwrap();
    inventory.byte_length = bytes.len() as u64;
    inventory.blake3 = digest;
}

fn validate_first_event_stream(manifest: &RprovManifest, bytes: &[u8]) -> Result<(), RprovError> {
    validate_rprov_event_stream(manifest, &manifest.segments[0], bytes)
}

fn two_segment_payload_manifest(
    tip_clean: Option<bool>,
) -> (RprovManifest, Vec<EventEnvelope>, Vec<u8>, Vec<u8>) {
    let (mut manifest, ancestor_events, ancestor_bytes, _) = payload_manifest();
    let tip_session = session("session-2");
    let mut tip_events = Vec::new();
    push_event(
        &mut tip_events,
        &tip_session,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: hash(4),
            documents: vec![],
        }),
    );
    if let Some(clean) = tip_clean {
        push_event(
            &mut tip_events,
            &tip_session,
            Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
                workspace_hash: hash(14),
                documents: vec![],
            }),
        );
        push_event(
            &mut tip_events,
            &tip_session,
            Event::SubmissionFinalized(SubmissionFinalized {
                final_workspace_hash: hash(14),
                event_count: 3,
                clean,
                warnings: vec![],
            }),
        );
    }
    let tip_bytes = jsonl(&tip_events);
    let tip_event_digest = rprov_raw_blake3(&tip_bytes);
    let initial_checkpoint = b"RUSTCPK\0\0\0\0\x01revision-initial";
    let initial_digest = rprov_raw_blake3(initial_checkpoint);
    let final_checkpoint = b"RUSTCPK\0\0\0\0\x01revision-final";
    let final_digest = rprov_raw_blake3(final_checkpoint);
    let complete = tip_clean.is_some();
    let ancestor = &manifest.segments[0];
    let mut tip = ancestor.clone();
    tip.ordinal = 2;
    tip.session_id = tip_session.clone();
    tip.initial_tree_hash = hash(4);
    tip.parent = Some(RprovParentLink {
        session_id: ancestor.session_id.clone(),
        terminal_event_hash: *ancestor.terminal_event_hash.known().unwrap(),
        final_tree_hash: *ancestor.final_tree_hash.known().unwrap(),
    });
    tip.events = RprovEventStreamRef {
        format_version: 1,
        entry: "segments/0002/events.jsonl".to_owned(),
        byte_length: tip_bytes.len() as u64,
        blake3: tip_event_digest,
        completeness: if complete {
            RprovEventStreamCompleteness::Complete
        } else {
            RprovEventStreamCompleteness::PrefixOnly
        },
    };
    tip.checkpoints = vec![RprovCheckpointRef {
        role: RprovCheckpointRole::Initial,
        format_version: 1,
        entry: "segments/0002/checkpoints/00000000000000000001.rcpk".to_owned(),
        byte_length: initial_checkpoint.len() as u64,
        blake3: initial_digest,
        owner: event_reference(&tip_events[0]),
        workspace_hash: hash(4),
    }];
    if complete {
        tip.checkpoints.push(RprovCheckpointRef {
            role: RprovCheckpointRole::Final,
            format_version: 1,
            entry: "segments/0002/checkpoints/00000000000000000002.rcpk".to_owned(),
            byte_length: final_checkpoint.len() as u64,
            blake3: final_digest,
            owner: event_reference(&tip_events[1]),
            workspace_hash: hash(14),
        });
    }
    tip.metadata.clear();
    tip.evidence.clear();
    tip.source_links.clear();
    tip.inclusive_event_count = tip_events.len() as u64;
    tip.last_event_hash = tip_events.last().unwrap().event_hash;
    tip.terminal_event_hash = if complete {
        RprovKnown::Known {
            value: tip_events.last().unwrap().event_hash,
        }
    } else {
        RprovKnown::Unknown
    };
    tip.final_tree_hash = if complete {
        RprovKnown::Known { value: hash(14) }
    } else {
        RprovKnown::Unknown
    };

    manifest.package_state = match tip_clean {
        None => RprovPackageState::RecoveryIncomplete {
            unavailable_assurances: vec![
                RprovUnavailableAssurance::CompleteEventStream,
                RprovUnavailableAssurance::FinalCheckpoint,
                RprovUnavailableAssurance::FinalTree,
                RprovUnavailableAssurance::CleanFinalization,
            ],
            gaps: vec![],
        },
        Some(false) => RprovPackageState::RecoveryIncomplete {
            unavailable_assurances: vec![RprovUnavailableAssurance::CleanFinalization],
            gaps: vec![],
        },
        Some(true) => RprovPackageState::CleanFinalized,
    };
    manifest.latest_session_id = tip_session;
    manifest.final_tree_hash = tip.final_tree_hash.clone();
    manifest.aggregate_event_count += tip.inclusive_event_count;
    manifest.segments.push(tip);
    manifest.inventory.extend([
        RprovInventoryEntry {
            path: "segments/0002/checkpoints/00000000000000000001.rcpk".to_owned(),
            byte_length: initial_checkpoint.len() as u64,
            blake3: initial_digest,
            kind: RprovEntryKind::Checkpoint,
        },
        RprovInventoryEntry {
            path: "segments/0002/events.jsonl".to_owned(),
            byte_length: tip_bytes.len() as u64,
            blake3: tip_event_digest,
            kind: RprovEntryKind::Events,
        },
    ]);
    if complete {
        manifest.inventory.push(RprovInventoryEntry {
            path: "segments/0002/checkpoints/00000000000000000002.rcpk".to_owned(),
            byte_length: final_checkpoint.len() as u64,
            blake3: final_digest,
            kind: RprovEntryKind::Checkpoint,
        });
    }
    manifest
        .inventory
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    (manifest, ancestor_events, ancestor_bytes, tip_bytes)
}

fn missing_evidence_recovery(usage_count: usize) -> (RprovManifest, Vec<u8>) {
    assert!(usage_count > 0);
    let session_id = session("session-1");
    let evidence_digest = hash(44);
    let mut events = Vec::with_capacity(usage_count + 3);
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: hash(1),
            documents: vec![],
        }),
    );
    for _ in 0..usage_count {
        push_event(
            &mut events,
            &session_id,
            Event::ExternalObservation(ExternalObservation {
                path: WorkspacePath::new("src/main.rs").unwrap(),
                saved_hash: None,
                logical_hash: None,
                observed_hash: None,
                evidence_hash: evidence_digest,
            }),
        );
    }
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: hash(4),
            documents: vec![],
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: hash(4),
            event_count: (usage_count + 3) as u64,
            clean: false,
            warnings: vec!["recovery evidence unavailable".to_owned()],
        }),
    );

    let bytes = jsonl(&events);
    let mut manifest = clean_manifest();
    manifest.package_state = RprovPackageState::RecoveryIncomplete {
        unavailable_assurances: vec![
            RprovUnavailableAssurance::ReferencedEvidence,
            RprovUnavailableAssurance::CleanFinalization,
        ],
        gaps: events[1..=usage_count]
            .iter()
            .map(|event| RprovRecoveryGap::MissingEvidence {
                event: event_reference(event),
                blake3: evidence_digest,
            })
            .collect(),
    };
    manifest.aggregate_event_count = events.len() as u64;
    let old_final_checkpoint_entry = manifest.segments[0].checkpoints[1].entry.clone();
    let final_checkpoint_sequence = events[usage_count + 1].sequence;
    let final_checkpoint_entry =
        format!("segments/0001/checkpoints/{final_checkpoint_sequence:020}.rcpk");
    {
        let segment = &mut manifest.segments[0];
        segment.checkpoints[0].owner = event_reference(&events[0]);
        segment.checkpoints[1].owner = event_reference(&events[usage_count + 1]);
        segment.checkpoints[1].entry = final_checkpoint_entry.clone();
        segment.inclusive_event_count = events.len() as u64;
        segment.last_event_hash = events.last().unwrap().event_hash;
        segment.terminal_event_hash = RprovKnown::Known {
            value: events.last().unwrap().event_hash,
        };
    }
    manifest
        .inventory
        .iter_mut()
        .find(|entry| entry.path == old_final_checkpoint_entry)
        .unwrap()
        .path = final_checkpoint_entry;
    replace_event_payload(&mut manifest, &bytes);
    (manifest, bytes)
}

#[test]
fn canonical_event_stream_validates_chain_checkpoints_and_deleted_source_link() {
    let (manifest, _, event_bytes, payloads) = payload_manifest();
    manifest.validate().unwrap();
    validate_first_event_stream(&manifest, &event_bytes).unwrap();

    for (entry, bytes) in manifest
        .inventory
        .iter()
        .filter(|entry| entry.kind != RprovEntryKind::Events)
        .zip(payloads)
    {
        validate_rprov_payload(entry, &bytes).unwrap();
    }
}

struct OneByteReader<R> {
    inner: R,
}

impl<R: Read> Read for OneByteReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let length = buffer.len().min(1);
        self.inner.read(&mut buffer[..length])
    }
}

struct CountedReader<R> {
    inner: R,
    bytes_read: Rc<Cell<usize>>,
}

struct InterruptOnceReader<R> {
    inner: R,
    interrupted: bool,
}

impl<R: Read> Read for InterruptOnceReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if !self.interrupted {
            self.interrupted = true;
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        self.inner.read(buffer)
    }
}

impl<R: Read> Read for CountedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.bytes_read.set(self.bytes_read.get() + read);
        Ok(read)
    }
}

#[test]
fn streaming_event_validation_matches_slice_validation_with_one_byte_reads() {
    let (manifest, _, event_bytes, _) = payload_manifest();
    let segment = &manifest.segments[0];

    validate_rprov_event_stream(&manifest, segment, &event_bytes).unwrap();
    validate_rprov_event_stream_reader(
        &manifest,
        segment,
        OneByteReader {
            inner: Cursor::new(&event_bytes),
        },
    )
    .unwrap();

    let mut no_final_lf = event_bytes;
    no_final_lf.pop();
    let mut malformed_manifest = manifest;
    replace_event_payload(&mut malformed_manifest, &no_final_lf);
    let slice_error = validate_first_event_stream(&malformed_manifest, &no_final_lf).unwrap_err();
    let reader_error = validate_rprov_event_stream_reader(
        &malformed_manifest,
        &malformed_manifest.segments[0],
        OneByteReader {
            inner: Cursor::new(no_final_lf),
        },
    )
    .unwrap_err();
    assert_eq!(reader_error, slice_error);
}

#[test]
fn linked_v1_event_stream_round_trips_the_additive_comparison_event() {
    let (mut manifest, mut events, _, _) = payload_manifest();
    let terminal = events.pop().expect("terminal event");
    assert!(matches!(terminal.event, Event::SubmissionFinalized(_)));
    let session_id = events[0].session_id.clone();
    let digest = hash(31);
    push_event(
        &mut events,
        &session_id,
        Event::TestCaseCompared(TestCaseCompared {
            command_id: CommandId::new("command-7").unwrap(),
            case: "sample".to_owned(),
            expected_blake3: digest,
            actual_blake3: Some(digest),
            outcome: TestCaseComparisonOutcome::Pass,
        }),
    );
    let event_count = events.len() as u64 + 1;
    push_event(
        &mut events,
        &session_id,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: hash(4),
            event_count,
            clean: true,
            warnings: vec![],
        }),
    );
    let bytes = jsonl(&events);
    manifest.aggregate_event_count = event_count;
    let segment = &mut manifest.segments[0];
    segment.inclusive_event_count = event_count;
    segment.last_event_hash = events.last().unwrap().event_hash;
    segment.terminal_event_hash = RprovKnown::Known {
        value: events.last().unwrap().event_hash,
    };
    replace_event_payload(&mut manifest, &bytes);

    validate_first_event_stream(&manifest, &bytes).unwrap();
    let decoded = bytes
        .strip_suffix(b"\n")
        .unwrap()
        .split(|byte| *byte == b'\n')
        .map(
            |line| match decode_envelope(line, DecodePolicy::RejectUnsupported).unwrap() {
                DecodeOutcome::Decoded(envelope) => envelope,
                DecodeOutcome::Skipped(_) => panic!("comparison event was skipped"),
            },
        )
        .collect::<Vec<_>>();
    assert_eq!(decoded, events);
    assert!(matches!(
        &decoded[decoded.len() - 2].event,
        Event::TestCaseCompared(comparison)
            if matches!(comparison.outcome, TestCaseComparisonOutcome::Pass)
    ));
}

#[test]
fn correction1_streaming_event_validation_retries_one_interrupted_read() {
    let (manifest, _, event_bytes, _) = payload_manifest();
    validate_rprov_event_stream_reader(
        &manifest,
        &manifest.segments[0],
        InterruptOnceReader {
            inner: Cursor::new(event_bytes),
            interrupted: false,
        },
    )
    .unwrap();
}

#[test]
fn streaming_event_validation_reads_at_most_declared_length_plus_one() {
    let (manifest, _, event_bytes, _) = payload_manifest();
    let declared = event_bytes.len();
    let bytes_read = Rc::new(Cell::new(0));
    let mut input = event_bytes;
    input.extend([b'X'; 64]);

    let error = validate_rprov_event_stream_reader(
        &manifest,
        &manifest.segments[0],
        CountedReader {
            inner: Cursor::new(input),
            bytes_read: Rc::clone(&bytes_read),
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        RprovError::InvalidField {
            field: "segments.events",
            ..
        }
    ));
    assert_eq!(bytes_read.get(), declared + 1);
}

#[test]
fn streaming_event_validation_rejects_an_overlong_line_before_growth() {
    let mut bytes = vec![b'x'; MAX_ENVELOPE_BYTES + 1];
    bytes.push(b'\n');
    let (mut manifest, _, _, _) = payload_manifest();
    replace_event_payload(&mut manifest, &bytes);

    let error = validate_rprov_event_stream_reader(
        &manifest,
        &manifest.segments[0],
        OneByteReader {
            inner: Cursor::new(bytes),
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        RprovError::LimitExceeded {
            field: "event envelope bytes",
            actual,
            maximum,
        } if actual == MAX_ENVELOPE_BYTES as u64 + 1
            && maximum == MAX_ENVELOPE_BYTES as u64
    ));
}

#[test]
fn production_checkpoint_genesis_binds_the_exact_initial_checkpoint() {
    let session_id = session("session-1");
    let mut events = Vec::new();
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: hash(1),
            documents: vec![],
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: hash(4),
            documents: vec![],
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: hash(4),
            event_count: 3,
            clean: true,
            warnings: vec![],
        }),
    );

    let original_bytes = jsonl(&events);
    let mut manifest = clean_manifest();
    let old_final_checkpoint = manifest.segments[0].checkpoints[1].entry.clone();
    {
        let segment = &mut manifest.segments[0];
        segment.checkpoints[0].owner = event_reference(&events[0]);
        segment.checkpoints[1].owner = event_reference(&events[1]);
        segment.checkpoints[1].entry =
            "segments/0001/checkpoints/00000000000000000002.rcpk".to_owned();
        segment.inclusive_event_count = events.len() as u64;
        segment.last_event_hash = events[2].event_hash;
        segment.terminal_event_hash = RprovKnown::Known {
            value: events[2].event_hash,
        };
    }
    manifest.aggregate_event_count = events.len() as u64;
    manifest
        .inventory
        .iter_mut()
        .find(|entry| entry.path == old_final_checkpoint)
        .unwrap()
        .path = "segments/0001/checkpoints/00000000000000000002.rcpk".to_owned();
    replace_event_payload(&mut manifest, &original_bytes);
    manifest.validate().unwrap();

    validate_rprov_event_stream(&manifest, &manifest.segments[0], &original_bytes).unwrap();
    assert_eq!(jsonl(&events), original_bytes, "event bytes stay unchanged");
}

#[test]
fn recovery_finalization_and_prefix_terminal_match_the_package_state() {
    let (mut recovery, mut events, _, _) = payload_manifest();
    let previous_hash = events[events.len() - 2].event_hash;
    let mut terminal = events.pop().unwrap();
    let Event::SubmissionFinalized(finalized) = &mut terminal.event else {
        panic!("payload fixture must end in SubmissionFinalized")
    };
    finalized.clean = false;
    terminal = terminal.seal(previous_hash).unwrap();
    events.push(terminal);
    let event_bytes = jsonl(&events);

    recovery.package_state = RprovPackageState::RecoveryIncomplete {
        unavailable_assurances: vec![RprovUnavailableAssurance::CleanFinalization],
        gaps: vec![],
    };
    recovery.segments[0].last_event_hash = events.last().unwrap().event_hash;
    recovery.segments[0].terminal_event_hash = RprovKnown::Known {
        value: events.last().unwrap().event_hash,
    };
    replace_event_payload(&mut recovery, &event_bytes);
    recovery.validate().unwrap();
    validate_first_event_stream(&recovery, &event_bytes).unwrap();

    let mut mislabeled_clean = recovery.clone();
    mislabeled_clean.package_state = RprovPackageState::CleanFinalized;
    assert!(validate_first_event_stream(&mislabeled_clean, &event_bytes).is_err());

    let mut prefix = recovery;
    prefix.segments[0].events.completeness = RprovEventStreamCompleteness::PrefixOnly;
    prefix.segments[0].terminal_event_hash = RprovKnown::Unknown;
    prefix.segments[0].final_tree_hash = RprovKnown::Unknown;
    assert!(validate_first_event_stream(&prefix, &event_bytes).is_err());
}

#[test]
fn review2_clean_true_terminal_rejects_false_clean_finalization_assurance() {
    let (mut recovery, _, bytes, _) = payload_manifest();
    recovery.package_state = RprovPackageState::RecoveryIncomplete {
        unavailable_assurances: vec![RprovUnavailableAssurance::CleanFinalization],
        gaps: vec![],
    };
    recovery.validate().unwrap();

    let result = validate_first_event_stream(&recovery, &bytes);
    assert!(
        matches!(
            result,
            Err(RprovError::InvalidField {
                field: "unavailable_assurances",
                ..
            })
        ),
        "clean-true terminal accepted a false recovery assurance: {result:?}"
    );
}

#[test]
fn review2_clean_false_terminal_requires_clean_finalization_assurance() {
    let (mut recovery, mut events, _, _) = payload_manifest();
    let previous_hash = events[events.len() - 2].event_hash;
    let mut terminal = events.pop().unwrap();
    let Event::SubmissionFinalized(finalized) = &mut terminal.event else {
        panic!("payload fixture must end in SubmissionFinalized")
    };
    finalized.clean = false;
    terminal = terminal.seal(previous_hash).unwrap();
    events.push(terminal);
    let bytes = jsonl(&events);

    recovery.package_state = RprovPackageState::RecoveryIncomplete {
        unavailable_assurances: vec![RprovUnavailableAssurance::ReplayConsistency],
        gaps: vec![],
    };
    recovery.segments[0].last_event_hash = events.last().unwrap().event_hash;
    recovery.segments[0].terminal_event_hash = RprovKnown::Known {
        value: events.last().unwrap().event_hash,
    };
    replace_event_payload(&mut recovery, &bytes);

    let result = validate_first_event_stream(&recovery, &bytes);
    assert!(
        matches!(
            result,
            Err(RprovError::InvalidField {
                field: "unavailable_assurances",
                ..
            })
        ),
        "clean-false terminal accepted without its recovery assurance: {result:?}"
    );
    recovery.validate().unwrap();
}

#[test]
fn review2_manifest_visible_assurances_are_bidirectional() {
    for spurious in [
        RprovUnavailableAssurance::CompleteEventStream,
        RprovUnavailableAssurance::FinalCheckpoint,
        RprovUnavailableAssurance::FinalTree,
    ] {
        let mut recovery = clean_manifest();
        recovery.package_state = RprovPackageState::RecoveryIncomplete {
            unavailable_assurances: vec![spurious, RprovUnavailableAssurance::CleanFinalization],
            gaps: vec![],
        };
        let result = recovery.validate();
        assert!(
            matches!(
                result,
                Err(RprovError::InvalidField {
                    field: "unavailable_assurances",
                    ..
                })
            ),
            "manifest accepted spurious {spurious:?}: {result:?}"
        );
    }
}

#[test]
fn review2_prefix_recovery_preserves_actual_unknown_terminal_facts() {
    let (mut recovery, mut events, _, _) = payload_manifest();
    events.pop();
    let bytes = jsonl(&events);
    recovery.package_state = RprovPackageState::RecoveryIncomplete {
        unavailable_assurances: vec![
            RprovUnavailableAssurance::CompleteEventStream,
            RprovUnavailableAssurance::FinalCheckpoint,
            RprovUnavailableAssurance::FinalTree,
            RprovUnavailableAssurance::CleanFinalization,
        ],
        gaps: vec![],
    };
    recovery.aggregate_event_count = events.len() as u64;
    {
        let segment = &mut recovery.segments[0];
        segment.events.completeness = RprovEventStreamCompleteness::PrefixOnly;
        segment.checkpoints[1].role = RprovCheckpointRole::Accepted;
        segment.inclusive_event_count = events.len() as u64;
        segment.last_event_hash = events.last().unwrap().event_hash;
        segment.terminal_event_hash = RprovKnown::Unknown;
        segment.final_tree_hash = RprovKnown::Unknown;
    }
    recovery.final_tree_hash = RprovKnown::Unknown;
    replace_event_payload(&mut recovery, &bytes);

    recovery.validate().unwrap();
    validate_first_event_stream(&recovery, &bytes).unwrap();
}

#[test]
fn review3_retained_clean_ancestor_allows_prefix_only_recovery_tip() {
    let (manifest, _, ancestor_bytes, tip_bytes) = two_segment_payload_manifest(None);
    manifest.validate().unwrap();

    validate_rprov_event_stream(&manifest, &manifest.segments[1], &tip_bytes).unwrap();
    validate_rprov_event_stream(&manifest, &manifest.segments[0], &ancestor_bytes).unwrap();
}

#[test]
fn review3_retained_clean_ancestor_allows_unclean_complete_recovery_tip() {
    let (manifest, _, ancestor_bytes, tip_bytes) = two_segment_payload_manifest(Some(false));
    manifest.validate().unwrap();

    validate_rprov_event_stream(&manifest, &manifest.segments[1], &tip_bytes).unwrap();
    validate_rprov_event_stream(&manifest, &manifest.segments[0], &ancestor_bytes).unwrap();
}

#[test]
fn review3_clean_tip_allows_an_exercised_gap_on_its_retained_ancestor() {
    let (mut manifest, ancestor_events, ancestor_bytes, tip_bytes) =
        two_segment_payload_manifest(Some(true));
    let paste_event = ancestor_events
        .iter()
        .find(|event| matches!(event.event, Event::InternalPaste(_)))
        .unwrap();
    manifest.segments[0]
        .source_links
        .retain(|link| !matches!(link, RprovSourceLink::InternalPaste { .. }));
    manifest.package_state = RprovPackageState::RecoveryIncomplete {
        unavailable_assurances: vec![RprovUnavailableAssurance::SourceLinkIntegrity],
        gaps: vec![RprovRecoveryGap::MissingSourceLink {
            event: event_reference(paste_event),
        }],
    };
    manifest.validate().unwrap();

    validate_rprov_event_stream(&manifest, &manifest.segments[0], &ancestor_bytes).unwrap();
    validate_rprov_event_stream(&manifest, &manifest.segments[1], &tip_bytes).unwrap();
}

#[test]
fn review3_rejects_unclean_retained_non_tip_terminal() {
    let (mut manifest, mut ancestor_events, _, tip_bytes) =
        two_segment_payload_manifest(Some(false));
    let previous_hash = ancestor_events[ancestor_events.len() - 2].event_hash;
    let mut terminal = ancestor_events.pop().unwrap();
    let Event::SubmissionFinalized(finalized) = &mut terminal.event else {
        panic!("ancestor fixture must end in SubmissionFinalized")
    };
    finalized.clean = false;
    terminal = terminal.seal(previous_hash).unwrap();
    ancestor_events.push(terminal);
    let ancestor_bytes = jsonl(&ancestor_events);
    manifest.segments[0].last_event_hash = ancestor_events.last().unwrap().event_hash;
    manifest.segments[0].terminal_event_hash = RprovKnown::Known {
        value: ancestor_events.last().unwrap().event_hash,
    };
    manifest.segments[1]
        .parent
        .as_mut()
        .unwrap()
        .terminal_event_hash = ancestor_events.last().unwrap().event_hash;
    replace_segment_event_payload(&mut manifest, 0, &ancestor_bytes);
    manifest.validate().unwrap();
    validate_rprov_event_stream(&manifest, &manifest.segments[1], &tip_bytes).unwrap();

    let result = validate_rprov_event_stream(&manifest, &manifest.segments[0], &ancestor_bytes);
    assert!(
        matches!(
            result,
            Err(RprovError::InvalidField {
                field: "events.jsonl terminal",
                ..
            })
        ),
        "accepted a clean=false retained non-tip terminal: {result:?}"
    );
}

#[test]
fn review3_rejects_event_segment_outside_its_manifest_context() {
    let (manifest, _, _, tip_bytes) = two_segment_payload_manifest(None);
    manifest.validate().unwrap();
    let mut foreign_segment = manifest.segments[1].clone();
    foreign_segment.ordinal = 1;

    let result = validate_rprov_event_stream(&manifest, &foreign_segment, &tip_bytes);
    assert!(
        matches!(
            result,
            Err(RprovError::InvalidField {
                field: "segments",
                ..
            })
        ),
        "accepted a segment outside the manifest context: {result:?}"
    );
}

#[test]
fn event_stream_requires_exact_lf_framing_canonical_bytes_and_inner_version() {
    let (manifest, _, event_bytes, _) = payload_manifest();

    let mut no_final_lf = event_bytes.clone();
    no_final_lf.pop();
    let mut changed = manifest.clone();
    replace_event_payload(&mut changed, &no_final_lf);
    assert!(validate_first_event_stream(&changed, &no_final_lf).is_err());

    let mut empty_final_record = event_bytes.clone();
    empty_final_record.push(b'\n');
    let mut changed = manifest.clone();
    replace_event_payload(&mut changed, &empty_final_record);
    assert!(validate_first_event_stream(&changed, &empty_final_record).is_err());

    let noncanonical = String::from_utf8(event_bytes.clone())
        .unwrap()
        .replacen("{\"format_version\":1", "{ \"format_version\":1", 1)
        .into_bytes();
    let mut changed = manifest.clone();
    replace_event_payload(&mut changed, &noncanonical);
    assert!(validate_first_event_stream(&changed, &noncanonical).is_err());

    let unknown = String::from_utf8(event_bytes)
        .unwrap()
        .replacen("\"format_version\":1", "\"format_version\":2", 1)
        .into_bytes();
    let mut changed = manifest;
    replace_event_payload(&mut changed, &unknown);
    assert!(matches!(
        validate_first_event_stream(&changed, &unknown),
        Err(RprovError::UnsupportedInnerVersion {
            layer: "event",
            found: 2,
            supported: 1
        })
    ));
}

#[test]
fn source_links_reject_missing_forged_cross_session_and_wrong_source_metadata() {
    let (valid, _, bytes, _) = payload_manifest();

    let mut missing = valid.clone();
    missing.segments[0].source_links.remove(0);
    assert!(validate_first_event_stream(&missing, &bytes).is_err());

    let mut forged = valid.clone();
    let RprovSourceLink::InternalPaste { copied_event, .. } =
        &mut forged.segments[0].source_links[0]
    else {
        panic!("wrong source-link fixture")
    };
    copied_event.event_hash = hash(88);
    assert!(validate_first_event_stream(&forged, &bytes).is_err());

    let mut cross_session = valid.clone();
    let RprovSourceLink::InternalPaste { copied_event, .. } =
        &mut cross_session.segments[0].source_links[0]
    else {
        panic!("wrong source-link fixture")
    };
    copied_event.session_id = session("session-2");
    assert!(validate_first_event_stream(&cross_session, &bytes).is_err());

    let mut wrong_document = valid.clone();
    let RprovSourceLink::InternalPaste { document_id, .. } =
        &mut wrong_document.segments[0].source_links[0]
    else {
        panic!("wrong source-link fixture")
    };
    *document_id = DocumentId::new("forged-doc").unwrap();
    assert!(validate_first_event_stream(&wrong_document, &bytes).is_err());

    let mut wrong_version = valid.clone();
    let RprovSourceLink::InternalPaste { version, .. } =
        &mut wrong_version.segments[0].source_links[0]
    else {
        panic!("wrong source-link fixture")
    };
    *version += 1;
    assert!(validate_first_event_stream(&wrong_version, &bytes).is_err());

    let mut wrong_range = valid;
    let RprovSourceLink::InternalPaste { end_byte, .. } =
        &mut wrong_range.segments[0].source_links[0]
    else {
        panic!("wrong source-link fixture")
    };
    *end_byte -= 1;
    assert!(validate_first_event_stream(&wrong_range, &bytes).is_err());
}

#[test]
fn declared_inner_versions_and_source_link_order_are_closed() {
    let mut unknown_event = clean_manifest();
    unknown_event.segments[0].events.format_version = 2;
    assert!(matches!(
        unknown_event.validate(),
        Err(RprovError::UnsupportedInnerVersion {
            layer: "event",
            found: 2,
            supported: 1
        })
    ));

    let mut unknown_checkpoint = clean_manifest();
    unknown_checkpoint.segments[0].checkpoints[0].format_version = 2;
    assert!(matches!(
        unknown_checkpoint.validate(),
        Err(RprovError::UnsupportedInnerVersion {
            layer: "checkpoint",
            found: 2,
            supported: 1
        })
    ));

    let mut unknown_metadata = clean_manifest();
    unknown_metadata.segments[0]
        .metadata
        .push(RprovMetadataRef {
            format_version: 2,
            entry: "segments/0001/metadata/not-materialized.json".to_owned(),
            byte_length: 1,
            blake3: hash(30),
            owner: event_ref(1, hash(31)),
        });
    assert!(matches!(
        unknown_metadata.validate(),
        Err(RprovError::UnsupportedInnerVersion {
            layer: "runtime metadata",
            found: 2,
            supported: 1
        })
    ));

    let (mut reordered_links, _, event_bytes, _) = payload_manifest();
    reordered_links.segments[0].source_links.swap(0, 1);
    assert!(reordered_links.validate().is_err());
    assert!(validate_first_event_stream(&reordered_links, &event_bytes).is_err());
}

#[test]
fn legacy_paste_stays_origin_unverified_and_new_link_cannot_downgrade() {
    let (valid, _, bytes, _) = payload_manifest();

    let mut no_legacy_marker = valid.clone();
    no_legacy_marker.segments[0].source_links.pop();
    assert!(validate_first_event_stream(&no_legacy_marker, &bytes).is_err());

    let mut downgraded = valid;
    downgraded.segments[0].source_links[0] = RprovSourceLink::LegacyPaste {
        event: event_reference(&linked_event_stream()[4]),
        verification: RprovLegacyPasteVerification::OriginUnverified,
    };
    assert!(validate_first_event_stream(&downgraded, &bytes).is_err());
}

#[test]
fn checkpoint_payload_rejects_unknown_version_and_owner_mismatch() {
    let (manifest, _, bytes, payloads) = payload_manifest();
    let checkpoint_entry = &manifest.inventory[1];
    let mut unknown = payloads[1].clone();
    unknown[8..12].copy_from_slice(&2_u32.to_be_bytes());
    let mut unknown_entry = checkpoint_entry.clone();
    unknown_entry.blake3 = rprov_raw_blake3(&unknown);
    assert!(matches!(
        validate_rprov_payload(&unknown_entry, &unknown),
        Err(RprovError::UnsupportedInnerVersion {
            layer: "checkpoint",
            found: 2,
            supported: 1
        })
    ));

    let mut wrong_owner = manifest;
    wrong_owner.segments[0].checkpoints[0].owner.event_hash = hash(88);
    assert!(validate_first_event_stream(&wrong_owner, &bytes).is_err());
}

#[test]
fn rejection_content_leakage_is_rejected_without_echoing_the_sentinel() {
    let (mut manifest, _, bytes, _) = payload_manifest();
    let leaked = String::from_utf8(bytes)
        .unwrap()
        .replacen(
            "\"channel\":\"terminal_bracketed\"",
            "\"channel\":\"terminal_bracketed\",\"rejected_text\":\"PRIVATE_SENTINEL\"",
            1,
        )
        .into_bytes();
    replace_event_payload(&mut manifest, &leaked);
    let error = validate_first_event_stream(&manifest, &leaked).unwrap_err();
    assert!(!error.to_string().contains("PRIVATE_SENTINEL"));
}

#[test]
fn container_record_and_assignment_byte_limits_are_exact() {
    let largest = RprovContainerHeader {
        format_version: 1,
        entry_count: MAX_RPROV_ARCHIVE_ENTRIES as u32,
        stored_records_bytes: MAX_RPROV_STORED_BYTES - RPROV_CONTAINER_HEADER_BYTES as u64,
        expanded_records_bytes: MAX_RPROV_STORED_BYTES - RPROV_CONTAINER_HEADER_BYTES as u64,
    };
    assert!(encode_rprov_container_header(&largest).is_ok());
    let mut too_large = largest.clone();
    too_large.stored_records_bytes += 1;
    too_large.expanded_records_bytes += 1;
    assert!(encode_rprov_container_header(&too_large).is_err());

    let mut too_many_records = largest;
    too_many_records.entry_count += 1;
    assert!(encode_rprov_container_header(&too_many_records).is_err());

    let largest_record = RprovRecordHeader {
        path_bytes: MAX_RPROV_ARCHIVE_PATH_BYTES as u16,
        entry_type: RprovRecordType::RegularFile,
        payload_bytes: MAX_RPROV_RECORD_PAYLOAD_BYTES,
    };
    assert!(encode_rprov_record_header(&largest_record).is_ok());
    let mut too_large_record = largest_record.clone();
    too_large_record.payload_bytes += 1;
    assert!(encode_rprov_record_header(&too_large_record).is_err());
    let mut too_long_record_path = largest_record;
    too_long_record_path.path_bytes += 1;
    assert!(encode_rprov_record_header(&too_long_record_path).is_err());

    let mut manifest = clean_manifest();
    manifest.assignment_manifest.byte_length = MAX_RPROV_ASSIGNMENT_MANIFEST_BYTES;
    assert!(manifest.validate().is_ok());
    manifest.assignment_manifest.byte_length += 1;
    assert!(manifest.validate().is_err());
}

#[test]
fn identifier_producer_and_tool_limits_are_canonical() {
    let mut manifest = clean_manifest();
    manifest.student_id = "x".repeat(MAX_IDENTIFIER_BYTES);
    manifest.producer.client_version = RprovKnown::Known {
        value: "c".repeat(MAX_RPROV_PRODUCER_VALUE_BYTES),
    };
    manifest.producer.rust_tools = (0..MAX_RPROV_TOOLS)
        .map(|index| RprovToolVersion {
            tool: format!("tool-{index:02}"),
            version: RprovKnown::Known {
                value: "1.0.0".to_owned(),
            },
        })
        .collect();
    manifest.producer.rust_tools[0].tool = "a".repeat(MAX_RPROV_TOOL_NAME_BYTES);
    manifest.producer.rust_tools[0].version = RprovKnown::Known {
        value: "v".repeat(MAX_RPROV_TOOL_VERSION_BYTES),
    };
    assert!(manifest.validate().is_ok());

    let mut long_id = manifest.clone();
    long_id.student_id.push('x');
    assert!(long_id.validate().is_err());

    let mut too_many_tools = manifest.clone();
    too_many_tools.producer.rust_tools.push(RprovToolVersion {
        tool: "tool-99".to_owned(),
        version: RprovKnown::Unknown,
    });
    assert!(too_many_tools.validate().is_err());

    let mut long_producer_value = manifest.clone();
    long_producer_value.producer.client_version = RprovKnown::Known {
        value: "c".repeat(MAX_RPROV_PRODUCER_VALUE_BYTES + 1),
    };
    assert!(long_producer_value.validate().is_err());

    let mut long_tool_name = manifest.clone();
    long_tool_name.producer.rust_tools[0].tool = "a".repeat(MAX_RPROV_TOOL_NAME_BYTES + 1);
    assert!(long_tool_name.validate().is_err());

    let mut long_tool_version = manifest.clone();
    long_tool_version.producer.rust_tools[0].version = RprovKnown::Known {
        value: "v".repeat(MAX_RPROV_TOOL_VERSION_BYTES + 1),
    };
    assert!(long_tool_version.validate().is_err());

    let mut duplicate_tool = manifest.clone();
    duplicate_tool.producer.rust_tools[1].tool = duplicate_tool.producer.rust_tools[0].tool.clone();
    assert!(duplicate_tool.validate().is_err());

    let mut non_nfc = manifest;
    non_nfc.producer.os = RprovKnown::Known {
        value: "cafe\u{301}".to_owned(),
    };
    assert!(non_nfc.validate().is_err());
}

#[test]
fn collection_limits_reject_limit_plus_one_before_materializing_payloads() {
    let mut manifest = clean_manifest();
    manifest.initial_workspace.files =
        vec![manifest.initial_workspace.files[0].clone(); MAX_RPROV_INITIAL_FILES + 1];
    assert!(manifest.validate().is_err());

    let mut manifest = clean_manifest();
    manifest.inventory = vec![manifest.inventory[0].clone(); MAX_RPROV_ARCHIVE_ENTRIES];
    assert!(manifest.validate().is_err());

    let mut manifest = clean_manifest();
    manifest.segments[0].checkpoints =
        vec![manifest.segments[0].checkpoints[0].clone(); MAX_RPROV_CHECKPOINTS_PER_SEGMENT + 1];
    assert!(manifest.validate().is_err());

    let mut manifest = clean_manifest();
    let metadata = RprovMetadataRef {
        format_version: 1,
        entry: "not-materialized".to_owned(),
        byte_length: 0,
        blake3: hash(30),
        owner: event_ref(1, hash(31)),
    };
    manifest.segments[0].metadata = vec![metadata; MAX_RPROV_METADATA_PER_SEGMENT + 1];
    assert!(manifest.validate().is_err());

    let mut manifest = clean_manifest();
    let evidence = RprovEvidenceRef {
        kind: RprovEvidenceKind::ExternalRecovery,
        entry: "not-materialized".to_owned(),
        byte_length: 0,
        blake3: hash(32),
        usages: vec![event_ref(1, hash(33))],
    };
    manifest.segments[0].evidence = vec![evidence; MAX_RPROV_EVIDENCE_PER_SEGMENT + 1];
    assert!(manifest.validate().is_err());

    let mut manifest = clean_manifest();
    let source = RprovSourceLink::LegacyPaste {
        event: event_ref(1, hash(34)),
        verification: RprovLegacyPasteVerification::OriginUnverified,
    };
    manifest.segments[0].source_links = vec![source; MAX_RPROV_SOURCE_LINKS_PER_SEGMENT + 1];
    assert!(manifest.validate().is_err());

    let exact_segments = linear_manifest(MAX_RPROV_SEGMENTS);
    assert!(exact_segments.validate().is_ok());
    let too_many_segments = linear_manifest(MAX_RPROV_SEGMENTS + 1);
    assert!(too_many_segments.validate().is_err());
}

#[test]
fn payload_digest_length_and_runtime_metadata_version_are_strict() {
    let bytes = b"{\"version\":1}";
    let digest = rprov_raw_blake3(bytes);
    let declaration = RprovInventoryEntry {
        path: format!("segments/0001/metadata/{digest}.json"),
        byte_length: bytes.len() as u64,
        blake3: digest,
        kind: RprovEntryKind::RuntimeMetadata,
    };
    validate_rprov_payload(&declaration, bytes).unwrap();

    let mut wrong_length = declaration.clone();
    wrong_length.byte_length += 1;
    assert!(validate_rprov_payload(&wrong_length, bytes).is_err());

    let mut wrong_digest = declaration.clone();
    wrong_digest.blake3 = hash(90);
    assert!(validate_rprov_payload(&wrong_digest, bytes).is_err());

    let unknown = b"{\"version\":2}";
    let unknown_declaration = RprovInventoryEntry {
        path: format!("segments/0001/metadata/{}.json", rprov_raw_blake3(unknown)),
        byte_length: unknown.len() as u64,
        blake3: rprov_raw_blake3(unknown),
        kind: RprovEntryKind::RuntimeMetadata,
    };
    assert!(matches!(
        validate_rprov_payload(&unknown_declaration, unknown),
        Err(RprovError::UnsupportedInnerVersion {
            layer: "runtime metadata",
            found: 2,
            supported: 1
        })
    ));
}

#[test]
fn review2_runtime_metadata_requires_complete_canonical_json() {
    let valid = b"{\"version\":1,\"tools\":[{\"name\":\"cargo\",\"enabled\":true}],\"os\":null}";
    validate_rprov_payload(&metadata_declaration(valid), valid).unwrap();

    for invalid in [
        b"{\"version\":01}".as_slice(),
        b"{\"version\":1}garbage",
        b"{\"version\":1}\n",
        b"{\"version\": 1}",
        b"{\"version\":+1}",
        b"{\"version\":-1}",
        b"{\"version\":1.0}",
        b"{\"version\":1e0}",
        b"{\"version\":\"1\"}",
        b"{\"version\":1,}",
        b"{\"version\":1",
    ] {
        let result = validate_rprov_payload(&metadata_declaration(invalid), invalid);
        assert!(
            result.is_err(),
            "accepted malformed runtime metadata: {}",
            String::from_utf8_lossy(invalid)
        );
    }

    let unsupported = b"{\"version\":2}";
    assert!(matches!(
        validate_rprov_payload(&metadata_declaration(unsupported), unsupported),
        Err(RprovError::UnsupportedInnerVersion {
            layer: "runtime metadata",
            found: 2,
            supported: 1
        })
    ));
}

fn metadata_declaration(bytes: &[u8]) -> RprovInventoryEntry {
    let digest = rprov_raw_blake3(bytes);
    RprovInventoryEntry {
        path: format!("segments/0001/metadata/{digest}.json"),
        byte_length: bytes.len() as u64,
        blake3: digest,
        kind: RprovEntryKind::RuntimeMetadata,
    }
}

#[test]
fn referenced_recovery_evidence_cannot_be_omitted_or_owned_by_another_event() {
    let session_id = session("session-1");
    let mut events = Vec::new();
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: hash(1),
            documents: vec![],
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::ExternalObservation(ExternalObservation {
            path: WorkspacePath::new("src/main.rs").unwrap(),
            saved_hash: None,
            logical_hash: None,
            observed_hash: None,
            evidence_hash: hash(44),
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: hash(4),
            documents: vec![],
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: hash(4),
            event_count: 4,
            clean: true,
            warnings: vec![],
        }),
    );
    let bytes = jsonl(&events);
    let mut manifest = clean_manifest();
    {
        let segment = &mut manifest.segments[0];
        segment.inclusive_event_count = 4;
        segment.last_event_hash = events[3].event_hash;
        segment.terminal_event_hash = RprovKnown::Known {
            value: events[3].event_hash,
        };
        segment.checkpoints[0].owner = event_reference(&events[0]);
        segment.checkpoints[1].owner = event_reference(&events[2]);
    }
    replace_event_payload(&mut manifest, &bytes);

    assert!(validate_rprov_event_stream(&manifest, &manifest.segments[0], &bytes).is_err());
    manifest.segments[0].evidence.push(RprovEvidenceRef {
        kind: RprovEvidenceKind::ExternalRecovery,
        entry: format!("segments/0001/evidence/{}.bin", hash(44)),
        byte_length: 1,
        blake3: hash(44),
        usages: vec![event_reference(&events[1])],
    });
    manifest.inventory.push(RprovInventoryEntry {
        path: format!("segments/0001/evidence/{}.bin", hash(44)),
        byte_length: 1,
        blake3: hash(44),
        kind: RprovEntryKind::ExternalRecoveryEvidence,
    });
    manifest
        .inventory
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    validate_rprov_event_stream(&manifest, &manifest.segments[0], &bytes).unwrap();

    manifest.segments[0].evidence[0].usages[0] = event_reference(&events[0]);
    assert!(validate_rprov_event_stream(&manifest, &manifest.segments[0], &bytes).is_err());
}

#[test]
fn one_evidence_artifact_covers_all_exact_production_usages() {
    let session_id = session("session-1");
    let mut events = Vec::new();
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: hash(1),
            documents: vec![],
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::ExternalObservation(ExternalObservation {
            path: WorkspacePath::new("src/main.rs").unwrap(),
            saved_hash: None,
            logical_hash: None,
            observed_hash: None,
            evidence_hash: hash(44),
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::ExternalObservation(ExternalObservation {
            path: WorkspacePath::new("src/lib.rs").unwrap(),
            saved_hash: None,
            logical_hash: None,
            observed_hash: None,
            evidence_hash: hash(44),
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::RecoveryRecorded(RecoveryRecorded {
            evidence_hash: hash(44),
            decision: RecoveryDecision::RestoreLogical,
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: hash(4),
            documents: vec![],
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: hash(4),
            event_count: 6,
            clean: true,
            warnings: vec![],
        }),
    );

    let bytes = jsonl(&events);
    let mut manifest = clean_manifest();
    let old_final_checkpoint = manifest.segments[0].checkpoints[1].entry.clone();
    let evidence_path = format!("segments/0001/evidence/{}.bin", hash(44));
    {
        let segment = &mut manifest.segments[0];
        segment.inclusive_event_count = events.len() as u64;
        segment.last_event_hash = events[5].event_hash;
        segment.terminal_event_hash = RprovKnown::Known {
            value: events[5].event_hash,
        };
        segment.checkpoints[0].owner = event_reference(&events[0]);
        segment.checkpoints[1].owner = event_reference(&events[4]);
        segment.checkpoints[1].entry =
            "segments/0001/checkpoints/00000000000000000005.rcpk".to_owned();
        segment.evidence.push(RprovEvidenceRef {
            kind: RprovEvidenceKind::ExternalRecovery,
            entry: evidence_path.clone(),
            byte_length: 1,
            blake3: hash(44),
            usages: vec![
                event_reference(&events[1]),
                event_reference(&events[2]),
                event_reference(&events[3]),
            ],
        });
    }
    manifest.aggregate_event_count = events.len() as u64;
    manifest
        .inventory
        .iter_mut()
        .find(|entry| entry.path == old_final_checkpoint)
        .unwrap()
        .path = "segments/0001/checkpoints/00000000000000000005.rcpk".to_owned();
    manifest.inventory.push(RprovInventoryEntry {
        path: evidence_path,
        byte_length: 1,
        blake3: hash(44),
        kind: RprovEntryKind::ExternalRecoveryEvidence,
    });
    manifest
        .inventory
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    replace_event_payload(&mut manifest, &bytes);

    validate_rprov_event_stream(&manifest, &manifest.segments[0], &bytes).unwrap();

    let mut missing = manifest.clone();
    missing.segments[0].evidence[0].usages.pop();
    assert!(validate_rprov_event_stream(&missing, &missing.segments[0], &bytes).is_err());

    let mut duplicate = manifest.clone();
    duplicate.segments[0].evidence[0]
        .usages
        .push(event_reference(&events[3]));
    assert!(validate_rprov_event_stream(&duplicate, &duplicate.segments[0], &bytes).is_err());

    let mut spurious = manifest;
    spurious.segments[0].evidence[0].usages[2] = event_reference(&events[4]);
    assert!(validate_rprov_event_stream(&spurious, &spurious.segments[0], &bytes).is_err());
}

#[test]
fn marked_missing_evidence_preserves_events_without_claiming_the_artifact() {
    let session_id = session("session-1");
    let mut events = Vec::new();
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: hash(1),
            documents: vec![],
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::ExternalObservation(ExternalObservation {
            path: WorkspacePath::new("src/main.rs").unwrap(),
            saved_hash: None,
            logical_hash: None,
            observed_hash: None,
            evidence_hash: hash(44),
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: hash(4),
            documents: vec![],
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: hash(4),
            event_count: 4,
            clean: false,
            warnings: vec!["recovery evidence unavailable".to_owned()],
        }),
    );

    let bytes = jsonl(&events);
    let mut recovery = clean_manifest();
    recovery.package_state = RprovPackageState::RecoveryIncomplete {
        unavailable_assurances: vec![
            RprovUnavailableAssurance::ReferencedEvidence,
            RprovUnavailableAssurance::CleanFinalization,
        ],
        gaps: vec![RprovRecoveryGap::MissingEvidence {
            event: event_reference(&events[1]),
            blake3: hash(44),
        }],
    };
    recovery.aggregate_event_count = events.len() as u64;
    {
        let segment = &mut recovery.segments[0];
        segment.checkpoints[0].owner = event_reference(&events[0]);
        segment.checkpoints[0].entry =
            "segments/0001/checkpoints/00000000000000000001.rcpk".to_owned();
        segment.checkpoints[1].owner = event_reference(&events[2]);
        segment.checkpoints[1].entry =
            "segments/0001/checkpoints/00000000000000000003.rcpk".to_owned();
        segment.inclusive_event_count = events.len() as u64;
        segment.last_event_hash = events[3].event_hash;
        segment.terminal_event_hash = RprovKnown::Known {
            value: events[3].event_hash,
        };
    }
    recovery.inventory[1].path = "segments/0001/checkpoints/00000000000000000001.rcpk".to_owned();
    replace_event_payload(&mut recovery, &bytes);

    recovery.validate().unwrap();
    validate_first_event_stream(&recovery, &bytes).unwrap();

    let mut clean = recovery.clone();
    clean.package_state = RprovPackageState::CleanFinalized;
    assert!(validate_first_event_stream(&clean, &bytes).is_err());

    let mut contradictory = recovery.clone();
    let evidence_path = format!("segments/0001/evidence/{}.bin", hash(44));
    contradictory.segments[0].evidence.push(RprovEvidenceRef {
        kind: RprovEvidenceKind::ExternalRecovery,
        entry: evidence_path.clone(),
        byte_length: 1,
        blake3: hash(44),
        usages: vec![event_reference(&events[1])],
    });
    contradictory.inventory.push(RprovInventoryEntry {
        path: evidence_path,
        byte_length: 1,
        blake3: hash(44),
        kind: RprovEntryKind::ExternalRecoveryEvidence,
    });
    contradictory
        .inventory
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    assert!(contradictory.validate().is_err());

    let mut spurious = recovery;
    let RprovPackageState::RecoveryIncomplete { gaps, .. } = &mut spurious.package_state else {
        unreachable!()
    };
    gaps[0] = RprovRecoveryGap::MissingEvidence {
        event: event_reference(&events[0]),
        blake3: hash(44),
    };
    assert!(validate_first_event_stream(&spurious, &bytes).is_err());
}

#[test]
fn review2_clean_true_terminal_allows_exercised_source_gap() {
    let (mut recovery, events, bytes, _) = payload_manifest();
    let paste_event = events
        .iter()
        .find(|event| matches!(event.event, Event::InternalPaste(_)))
        .unwrap();
    let retained_link = recovery.segments[0].source_links[0].clone();
    recovery.segments[0]
        .source_links
        .retain(|link| !matches!(link, RprovSourceLink::InternalPaste { .. }));
    recovery.package_state = RprovPackageState::RecoveryIncomplete {
        unavailable_assurances: vec![RprovUnavailableAssurance::SourceLinkIntegrity],
        gaps: vec![RprovRecoveryGap::MissingSourceLink {
            event: event_reference(paste_event),
        }],
    };

    recovery.validate().unwrap();
    validate_first_event_stream(&recovery, &bytes).unwrap();

    let mut clean = recovery.clone();
    clean.package_state = RprovPackageState::CleanFinalized;
    assert!(validate_first_event_stream(&clean, &bytes).is_err());

    let mut contradictory = recovery.clone();
    contradictory.segments[0]
        .source_links
        .insert(0, retained_link);
    assert!(contradictory.validate().is_err());

    let mut spurious = recovery;
    let RprovPackageState::RecoveryIncomplete { gaps, .. } = &mut spurious.package_state else {
        unreachable!()
    };
    gaps[0] = RprovRecoveryGap::MissingSourceLink {
        event: event_reference(&events[0]),
    };
    assert!(validate_first_event_stream(&spurious, &bytes).is_err());
}

#[test]
fn review2_missing_evidence_per_artifact_limit_is_inclusive() {
    let (exact, exact_bytes) = missing_evidence_recovery(MAX_RPROV_EVIDENCE_USAGES_PER_ARTIFACT);
    exact.validate().unwrap();
    validate_first_event_stream(&exact, &exact_bytes).unwrap();

    let (over, over_bytes) = missing_evidence_recovery(MAX_RPROV_EVIDENCE_USAGES_PER_ARTIFACT + 1);
    let result = over.validate();
    assert!(
        matches!(
            result,
            Err(RprovError::LimitExceeded {
                field: "missing evidence usages per artifact",
                actual: 1_025,
                maximum: 1_024,
            })
        ),
        "1,025 distinct usages bypassed the per-artifact guard: {result:?}"
    );
    assert!(validate_first_event_stream(&over, &over_bytes).is_err());
}

#[test]
fn header_reserved_compression_and_record_type_bytes_are_closed() {
    let header = encode_rprov_container_header(&RprovContainerHeader {
        format_version: 1,
        entry_count: 1,
        stored_records_bytes: 29,
        expanded_records_bytes: 29,
    })
    .unwrap();
    for index in [14, 15, 20] {
        let mut malformed = header;
        malformed[index] = 1;
        assert!(decode_rprov_container_header(&malformed).is_err());
    }

    let record = encode_rprov_record_header(&RprovRecordHeader {
        path_bytes: 13,
        entry_type: RprovRecordType::RegularFile,
        payload_bytes: 0,
    })
    .unwrap();
    for index in [2, 3, 4] {
        let mut malformed = record;
        malformed[index] = 2;
        assert!(decode_rprov_record_header(&malformed).is_err());
    }
}

fn decode_hex(value: &str) -> Vec<u8> {
    let value = value.trim();
    assert_eq!(value.len() % 2, 0);
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let pair = std::str::from_utf8(pair).unwrap();
            u8::from_str_radix(pair, 16).unwrap()
        })
        .collect()
}
