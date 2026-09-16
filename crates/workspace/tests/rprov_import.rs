use std::collections::BTreeMap;
use std::io::{self, Cursor, Read, Write};

use flate2::Compression;
use flate2::write::DeflateEncoder;
use rustrace_model::*;
use rustrace_workspace::rprov_import::{ImportedPackageKind, RprovImportError, import_rprov};

fn hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; Hash::LENGTH])
}

fn session(value: &str) -> SessionId {
    SessionId::new(value).unwrap()
}

fn producer() -> RprovProducer {
    RprovProducer {
        client_version: RprovKnown::Unknown,
        build_identity: RprovKnown::Unknown,
        os: RprovKnown::Unknown,
        architecture: RprovKnown::Unknown,
        rust_tools: vec![],
    }
}

fn push_event(events: &mut Vec<EventEnvelope>, session_id: &SessionId, event: Event) {
    let previous = events
        .last()
        .map_or_else(Hash::zero, |envelope| envelope.event_hash);
    let sequence = events.len() as u64 + 1;
    events.push(
        EventEnvelope {
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
        .unwrap(),
    );
}

fn event_reference(event: &EventEnvelope) -> RecordedEventRef {
    RecordedEventRef {
        session_id: event.session_id.clone(),
        sequence: event.sequence,
        event_hash: event.event_hash,
    }
}

fn jsonl(events: &[EventEnvelope]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for event in events {
        bytes.extend(encode_envelope(event).unwrap());
        bytes.push(b'\n');
    }
    bytes
}

fn segment_material(
    ordinal: u32,
    initial_tree: Hash,
    final_tree: Hash,
) -> (RprovSegment, Vec<(String, Vec<u8>)>) {
    let session_id = session(&format!("session-{ordinal}"));
    let mut events = Vec::new();
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: initial_tree,
            documents: vec![],
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: final_tree,
            documents: vec![],
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: final_tree,
            event_count: 3,
            clean: true,
            warnings: vec![],
        }),
    );

    let event_bytes = jsonl(&events);
    let initial_checkpoint = format!("RUSTCPK\0\0\0\0\x01initial-{ordinal}").into_bytes();
    let final_checkpoint = format!("RUSTCPK\0\0\0\0\x01final-{ordinal}").into_bytes();
    let prefix = format!("segments/{ordinal:04}");
    let event_path = format!("{prefix}/events.jsonl");
    let initial_path = format!("{prefix}/checkpoints/{:020}.rcpk", 1_u64);
    let final_path = format!("{prefix}/checkpoints/{:020}.rcpk", 2_u64);
    let segment = RprovSegment {
        ordinal,
        session_id: session_id.clone(),
        course_id: "ECE1724".to_owned(),
        assignment_id: "a3".to_owned(),
        assignment_version: "2026-09-01".to_owned(),
        assignment_manifest_blake3: hash(3),
        original_starter_tree_hash: hash(1),
        initial_tree_hash: initial_tree,
        producer: producer(),
        time: RprovSegmentTime {
            started_at_utc: RprovKnown::Unknown,
            ended_at_utc: RprovKnown::Unknown,
            inter_attempt_time: RprovInterAttemptTime::Unknown,
        },
        parent: None,
        events: RprovEventStreamRef {
            format_version: 1,
            entry: event_path.clone(),
            byte_length: event_bytes.len() as u64,
            blake3: rprov_raw_blake3(&event_bytes),
            completeness: RprovEventStreamCompleteness::Complete,
        },
        checkpoints: vec![
            RprovCheckpointRef {
                role: RprovCheckpointRole::Initial,
                format_version: 1,
                entry: initial_path.clone(),
                byte_length: initial_checkpoint.len() as u64,
                blake3: rprov_raw_blake3(&initial_checkpoint),
                owner: event_reference(&events[0]),
                workspace_hash: initial_tree,
            },
            RprovCheckpointRef {
                role: RprovCheckpointRole::Final,
                format_version: 1,
                entry: final_path.clone(),
                byte_length: final_checkpoint.len() as u64,
                blake3: rprov_raw_blake3(&final_checkpoint),
                owner: event_reference(&events[1]),
                workspace_hash: final_tree,
            },
        ],
        metadata: vec![],
        evidence: vec![],
        source_links: vec![],
        inclusive_event_count: events.len() as u64,
        last_event_hash: events.last().unwrap().event_hash,
        terminal_event_hash: RprovKnown::Known {
            value: events.last().unwrap().event_hash,
        },
        final_tree_hash: RprovKnown::Known { value: final_tree },
    };
    (
        segment,
        vec![
            (initial_path, initial_checkpoint),
            (final_path, final_checkpoint),
            (event_path, event_bytes),
        ],
    )
}

#[derive(Clone)]
struct Fixture {
    manifest: RprovManifest,
    payloads: BTreeMap<String, Vec<u8>>,
}

impl Fixture {
    fn one_segment() -> Self {
        let starter = b"fn main() {}\n".to_vec();
        let starter_digest = rprov_raw_blake3(&starter);
        let starter_path = format!("initial-workspace/blobs/{starter_digest}");
        let (segment, segment_payloads) = segment_material(1, hash(1), hash(4));
        let mut payloads = BTreeMap::from([(starter_path.clone(), starter)]);
        payloads.extend(segment_payloads);
        let inventory = payloads
            .iter()
            .map(|(path, bytes)| RprovInventoryEntry {
                path: path.clone(),
                byte_length: bytes.len() as u64,
                blake3: rprov_raw_blake3(bytes),
                kind: if path.starts_with("initial-workspace/") {
                    RprovEntryKind::InitialWorkspaceBlob
                } else if path.ends_with("events.jsonl") {
                    RprovEntryKind::Events
                } else {
                    RprovEntryKind::Checkpoint
                },
            })
            .collect();
        let manifest = RprovManifest {
            format_version: 1,
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
            aggregate_event_count: 3,
            producer: producer(),
            assignment_manifest: RprovAssignmentManifestIdentity {
                format_version: 1,
                byte_length: 123,
                blake3: hash(3),
            },
            initial_workspace: RprovInitialWorkspace {
                files: vec![RprovInitialWorkspaceFile {
                    path: WorkspacePath::new("src/main.rs").unwrap(),
                    entry: starter_path,
                }],
            },
            segments: vec![segment],
            inventory,
        };
        manifest.validate().unwrap();
        validate_rprov_event_stream(
            &manifest,
            &manifest.segments[0],
            payloads.get(&manifest.segments[0].events.entry).unwrap(),
        )
        .unwrap();
        Self { manifest, payloads }
    }

    fn two_segment_recovery() -> Self {
        let mut fixture = Self::one_segment();
        let parent = &fixture.manifest.segments[0];
        let parent_link = RprovParentLink {
            session_id: parent.session_id.clone(),
            terminal_event_hash: *parent.terminal_event_hash.known().unwrap(),
            final_tree_hash: *parent.final_tree_hash.known().unwrap(),
        };
        let (mut segment, payloads) = segment_material(2, hash(4), hash(14));
        segment.parent = Some(parent_link);
        fixture.payloads.extend(payloads);
        fixture.manifest.package_state = RprovPackageState::RecoveryIncomplete {
            unavailable_assurances: vec![RprovUnavailableAssurance::ReplayConsistency],
            gaps: vec![],
        };
        fixture.manifest.latest_session_id = session("session-2");
        fixture.manifest.final_tree_hash = RprovKnown::Known { value: hash(14) };
        fixture.manifest.aggregate_event_count = 6;
        fixture.manifest.segments.push(segment);
        fixture.manifest.inventory = fixture
            .payloads
            .iter()
            .map(|(path, bytes)| RprovInventoryEntry {
                path: path.clone(),
                byte_length: bytes.len() as u64,
                blake3: rprov_raw_blake3(bytes),
                kind: if path.starts_with("initial-workspace/") {
                    RprovEntryKind::InitialWorkspaceBlob
                } else if path.ends_with("events.jsonl") {
                    RprovEntryKind::Events
                } else {
                    RprovEntryKind::Checkpoint
                },
            })
            .collect();
        fixture.manifest.validate().unwrap();
        for segment in &fixture.manifest.segments {
            validate_rprov_event_stream(
                &fixture.manifest,
                segment,
                fixture.payloads.get(&segment.events.entry).unwrap(),
            )
            .unwrap();
        }
        fixture
    }

    fn add_metadata(&mut self, bytes: Vec<u8>) -> String {
        let digest = rprov_raw_blake3(&bytes);
        let path = format!("segments/0001/metadata/{digest}.json");
        let owner = self.manifest.segments[0].checkpoints[0].owner.clone();
        self.manifest.segments[0].metadata.push(RprovMetadataRef {
            format_version: 1,
            entry: path.clone(),
            byte_length: bytes.len() as u64,
            blake3: digest,
            owner,
        });
        self.payloads.insert(path.clone(), bytes);
        self.rebuild_inventory();
        path
    }

    fn replace_payload(&mut self, path: &str, bytes: Vec<u8>) {
        let digest = rprov_raw_blake3(&bytes);
        let length = bytes.len() as u64;
        self.payloads.insert(path.to_owned(), bytes);
        if let Some(segment) = self
            .manifest
            .segments
            .iter_mut()
            .find(|segment| segment.events.entry == path)
        {
            segment.events.byte_length = length;
            segment.events.blake3 = digest;
        }
        for segment in &mut self.manifest.segments {
            for checkpoint in &mut segment.checkpoints {
                if checkpoint.entry == path {
                    checkpoint.byte_length = length;
                    checkpoint.blake3 = digest;
                }
            }
            for metadata in &mut segment.metadata {
                if metadata.entry == path {
                    metadata.byte_length = length;
                    metadata.blake3 = digest;
                }
            }
        }
        self.rebuild_inventory();
    }

    fn rebuild_inventory(&mut self) {
        for entry in &mut self.manifest.inventory {
            let bytes = self.payloads.get(&entry.path).unwrap();
            entry.byte_length = bytes.len() as u64;
            entry.blake3 = rprov_raw_blake3(bytes);
        }
        for (path, bytes) in &self.payloads {
            if self
                .manifest
                .inventory
                .iter()
                .all(|entry| entry.path != *path)
            {
                self.manifest.inventory.push(RprovInventoryEntry {
                    path: path.clone(),
                    byte_length: bytes.len() as u64,
                    blake3: rprov_raw_blake3(bytes),
                    kind: if path.ends_with(".json") {
                        RprovEntryKind::RuntimeMetadata
                    } else {
                        unreachable!("test helper requires an explicit existing payload kind")
                    },
                });
            }
        }
        self.manifest
            .inventory
            .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    }

    fn records(&self) -> Vec<(String, Vec<u8>)> {
        let manifest = encode_rprov_manifest(&self.manifest).unwrap();
        std::iter::once(("manifest.json".to_owned(), manifest))
            .chain(self.manifest.inventory.iter().map(|entry| {
                (
                    entry.path.clone(),
                    self.payloads.get(&entry.path).unwrap().clone(),
                )
            }))
            .collect()
    }

    fn encode(&self) -> Vec<u8> {
        encode_records(self.records())
    }
}

fn encode_records(records: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let stored_records_bytes = records.iter().fold(0_u64, |total, (path, bytes)| {
        total + RPROV_RECORD_HEADER_BYTES as u64 + path.len() as u64 + bytes.len() as u64
    });
    let header = RprovContainerHeader {
        format_version: 1,
        entry_count: records.len() as u32,
        stored_records_bytes,
        expanded_records_bytes: stored_records_bytes,
    };
    let mut encoded = encode_rprov_container_header(&header).unwrap().to_vec();
    for (path, payload) in records {
        encoded.extend(
            encode_rprov_record_header(&RprovRecordHeader {
                path_bytes: path.len() as u16,
                entry_type: RprovRecordType::RegularFile,
                payload_bytes: payload.len() as u64,
            })
            .unwrap(),
        );
        encoded.extend(path.as_bytes());
        encoded.extend(payload);
    }
    encoded
}

#[derive(Clone, Copy)]
enum ZipMethod {
    Stored,
    Deflated,
    Unsupported(u16),
}

#[derive(Clone)]
struct ZipEntry {
    name: String,
    bytes: Vec<u8>,
    method: ZipMethod,
    made_by_system: u8,
    version_needed: u16,
    flags: u16,
    external_attributes: u32,
    extra: Vec<u8>,
    declared_compressed: Option<u32>,
    declared_expanded: Option<u32>,
    crc32: Option<u32>,
}

impl ZipEntry {
    fn file(name: &str, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            name: name.to_owned(),
            bytes: bytes.into(),
            method: ZipMethod::Stored,
            made_by_system: 3,
            version_needed: 20,
            flags: 0,
            external_attributes: 0o100644 << 16,
            extra: vec![],
            declared_compressed: None,
            declared_expanded: None,
            crc32: None,
        }
    }

    fn directory(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            bytes: vec![],
            method: ZipMethod::Stored,
            made_by_system: 3,
            version_needed: 20,
            flags: 0,
            external_attributes: (0o040755 << 16) | 0x10,
            extra: vec![],
            declared_compressed: None,
            declared_expanded: None,
            crc32: None,
        }
    }
}

fn utf8_file(name: &str, bytes: impl Into<Vec<u8>>) -> ZipEntry {
    let mut entry = ZipEntry::file(name, bytes);
    entry.flags = 1 << 11;
    entry
}

fn fixed_outer(rprov: Vec<u8>) -> Vec<u8> {
    let mut main = ZipEntry::file("src/main.rs", b"fn main() {}\n".to_vec());
    main.method = ZipMethod::Deflated;
    zip(&[
        ZipEntry::file(
            "Cargo.toml",
            b"[package]\nname = \"student\"\n[workspace]\n".to_vec(),
        ),
        ZipEntry::file("session.rprov", rprov),
        ZipEntry::directory("src/"),
        main,
    ])
}

fn zip(entries: &[ZipEntry]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut central = Vec::new();
    for entry in entries {
        let offset = output.len() as u32;
        let method = match entry.method {
            ZipMethod::Stored => 0,
            ZipMethod::Deflated => 8,
            ZipMethod::Unsupported(value) => value,
        };
        let stored = match entry.method {
            ZipMethod::Deflated => {
                let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
                encoder.write_all(&entry.bytes).unwrap();
                encoder.finish().unwrap()
            }
            ZipMethod::Stored | ZipMethod::Unsupported(_) => entry.bytes.clone(),
        };
        let crc = entry.crc32.unwrap_or_else(|| crc32fast::hash(&entry.bytes));
        let compressed = entry.declared_compressed.unwrap_or(stored.len() as u32);
        let expanded = entry.declared_expanded.unwrap_or(entry.bytes.len() as u32);
        let name = entry.name.as_bytes();

        push_u32(&mut output, 0x0403_4b50);
        push_u16(&mut output, entry.version_needed);
        push_u16(&mut output, entry.flags);
        push_u16(&mut output, method);
        push_u16(&mut output, 0);
        push_u16(&mut output, 0);
        push_u32(&mut output, crc);
        push_u32(&mut output, compressed);
        push_u32(&mut output, expanded);
        push_u16(&mut output, name.len() as u16);
        push_u16(&mut output, entry.extra.len() as u16);
        output.extend(name);
        output.extend(&entry.extra);
        output.extend(&stored);

        push_u32(&mut central, 0x0201_4b50);
        push_u16(&mut central, (u16::from(entry.made_by_system) << 8) | 20);
        push_u16(&mut central, entry.version_needed);
        push_u16(&mut central, entry.flags);
        push_u16(&mut central, method);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, crc);
        push_u32(&mut central, compressed);
        push_u32(&mut central, expanded);
        push_u16(&mut central, name.len() as u16);
        push_u16(&mut central, entry.extra.len() as u16);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, entry.external_attributes);
        push_u32(&mut central, offset);
        central.extend(name);
        central.extend(&entry.extra);
    }
    let central_offset = output.len() as u32;
    let central_size = central.len() as u32;
    output.extend(central);
    push_u32(&mut output, 0x0605_4b50);
    push_u16(&mut output, 0);
    push_u16(&mut output, 0);
    push_u16(&mut output, entries.len() as u16);
    push_u16(&mut output, entries.len() as u16);
    push_u32(&mut output, central_size);
    push_u32(&mut output, central_offset);
    push_u16(&mut output, 0);
    output
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend(value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend(value.to_le_bytes());
}

fn read_all(mut reader: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    bytes
}

struct ReadCeiling {
    inner: Cursor<Vec<u8>>,
    maximum: usize,
    read: usize,
}

impl ReadCeiling {
    fn new(bytes: Vec<u8>, maximum: usize) -> Self {
        Self {
            inner: Cursor::new(bytes),
            maximum,
            read: 0,
        }
    }
}

impl Read for ReadCeiling {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.read == self.maximum {
            return Err(io::Error::other("reader ceiling crossed"));
        }
        let allowed = (self.maximum - self.read).min(buffer.len());
        let read = self.inner.read(&mut buffer[..allowed])?;
        self.read += read;
        Ok(read)
    }
}

struct Chunked<R> {
    inner: R,
    chunk: usize,
}

impl<R: Read> Read for Chunked<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let length = self.chunk.min(buffer.len());
        self.inner.read(&mut buffer[..length])
    }
}

#[test]
fn red_imports_complete_one_and_two_segment_standalone_packages() {
    for fixture in [Fixture::one_segment(), Fixture::two_segment_recovery()] {
        let expected = fixture.manifest.clone();
        let imported = import_rprov(Chunked {
            inner: Cursor::new(fixture.encode()),
            chunk: 3,
        })
        .expect("valid standalone package");
        assert_eq!(imported.kind(), ImportedPackageKind::StandaloneRprov);
        assert_eq!(imported.manifest(), &expected);
        assert!(imported.outer_source_files().is_empty());
        for (path, expected_bytes) in &fixture.payloads {
            assert_eq!(
                read_all(imported.open_entry(path).expect("inventoried entry")),
                *expected_bytes
            );
        }
    }
}

#[test]
fn red_imports_fixed_outer_zip_and_exposes_exact_latest_source() {
    let fixture = Fixture::one_segment();
    let imported = import_rprov(Cursor::new(fixed_outer(fixture.encode())))
        .expect("valid fixed LMS ZIP wrapper");
    assert_eq!(imported.kind(), ImportedPackageKind::LmsZip);
    assert_eq!(imported.manifest(), &fixture.manifest);
    assert_eq!(
        imported
            .outer_source_files()
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        ["Cargo.toml", "src/main.rs"]
    );
    assert_eq!(
        read_all(
            imported
                .open_outer_source(&WorkspacePath::new("src/main.rs").unwrap())
                .unwrap()
        ),
        b"fn main() {}\n"
    );
}

#[test]
fn outer_accepts_an_empty_latest_source_tree_without_certifying_it() {
    let fixture = Fixture::one_segment();
    let imported = import_rprov(Cursor::new(zip(&[ZipEntry::file(
        "session.rprov",
        fixture.encode(),
    )])))
    .expect("an empty outer source tree is structurally representable");
    assert_eq!(imported.kind(), ImportedPackageKind::LmsZip);
    assert!(imported.outer_source_files().is_empty());
}

#[test]
fn red_rejects_unknown_version_without_reading_v1_fields() {
    let mut prefix = b"RUSTPROV".to_vec();
    prefix.extend(2_u32.to_le_bytes());
    let error = import_rprov(ReadCeiling::new(prefix, 12)).expect_err("unknown version");
    assert!(matches!(
        error,
        RprovImportError::Model(RprovError::UnsupportedFormatVersion {
            found: 2,
            supported: 1
        })
    ));
}

#[test]
fn red_rejects_oversized_record_claim_before_path_or_payload_read() {
    let payload_bytes = MAX_RPROV_RECORD_PAYLOAD_BYTES + 1;
    let records_bytes = 16 + 13 + payload_bytes;
    let header = encode_rprov_container_header(&RprovContainerHeader {
        format_version: 1,
        entry_count: 1,
        stored_records_bytes: records_bytes,
        expanded_records_bytes: records_bytes,
    })
    .unwrap();
    let mut bytes = header.to_vec();
    bytes.extend((13_u16).to_le_bytes());
    bytes.push(1);
    bytes.push(0);
    bytes.extend([0; 4]);
    bytes.extend(payload_bytes.to_le_bytes());
    let error = import_rprov(ReadCeiling::new(bytes, 56)).expect_err("oversized record");
    assert!(matches!(
        error,
        RprovImportError::Model(RprovError::LimitExceeded {
            field: "record payload bytes",
            ..
        })
    ));
}

#[test]
fn rejects_inner_truncation_reserved_fields_totals_and_trailing_bytes() {
    let valid = Fixture::one_segment().encode();
    for cut in [0, 7, 8, 11, 12, 39, 40, 55, 56, 68, valid.len() - 1] {
        assert!(
            import_rprov(Cursor::new(valid[..cut].to_vec())).is_err(),
            "accepted truncation at byte {cut}"
        );
    }

    let mut trailing = valid.clone();
    trailing.push(0);
    assert!(import_rprov(Cursor::new(trailing)).is_err());

    for offset in [14, 15, 20, 42, 43, 44] {
        let mut malformed = valid.clone();
        malformed[offset] = malformed[offset].wrapping_add(1);
        assert!(
            import_rprov(Cursor::new(malformed)).is_err(),
            "accepted nonzero/unsupported byte at {offset}"
        );
    }

    let mut bad_count = valid.clone();
    bad_count[16..20].copy_from_slice(&1_u32.to_le_bytes());
    assert!(import_rprov(Cursor::new(bad_count)).is_err());
    let mut bad_stored = valid.clone();
    let stored = u64::from_le_bytes(valid[24..32].try_into().unwrap());
    bad_stored[24..32].copy_from_slice(&(stored - 1).to_le_bytes());
    assert!(import_rprov(Cursor::new(bad_stored)).is_err());
    let mut bad_expanded = valid;
    bad_expanded[32..40].copy_from_slice(&(stored + 1).to_le_bytes());
    assert!(import_rprov(Cursor::new(bad_expanded)).is_err());
}

#[test]
fn rejects_missing_duplicate_reordered_uninventoried_and_bad_digest_records() {
    let fixture = Fixture::one_segment();
    let records = fixture.records();

    let mut missing = records.clone();
    missing.pop();
    assert!(import_rprov(Cursor::new(encode_records(missing))).is_err());

    let mut duplicate = records.clone();
    duplicate.push(records[1].clone());
    assert!(import_rprov(Cursor::new(encode_records(duplicate))).is_err());

    let mut reordered = records.clone();
    reordered.swap(1, 2);
    assert!(import_rprov(Cursor::new(encode_records(reordered))).is_err());

    let mut unlisted = records.clone();
    unlisted.push(("segments/0001/evidence/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.bin".to_owned(), vec![]));
    assert!(import_rprov(Cursor::new(encode_records(unlisted))).is_err());

    let mut corrupt = records;
    corrupt[1].1[0] ^= 1;
    assert!(import_rprov(Cursor::new(encode_records(corrupt))).is_err());
}

#[test]
fn rejects_event_framing_oversized_envelopes_and_noncanonical_json() {
    let fixture = Fixture::one_segment();
    let event_path = fixture.manifest.segments[0].events.entry.clone();
    let original = fixture.payloads.get(&event_path).unwrap();
    let cases = [
        original[..original.len() - 1].to_vec(),
        [original.as_slice(), b"\n"].concat(),
        vec![b'x'; MAX_ENVELOPE_BYTES + 2],
        format!("{}\n", "[".repeat(MAX_RPROV_JSON_NESTING + 1)).into_bytes(),
    ];
    for event_bytes in cases {
        let mut malformed = fixture.clone();
        malformed.replace_payload(&event_path, event_bytes);
        assert!(import_rprov(Cursor::new(malformed.encode())).is_err());
    }
}

#[test]
fn preflights_checkpoint_and_metadata_versions_without_semantic_certification() {
    let mut structurally_valid = Fixture::one_segment();
    structurally_valid.add_metadata(b"{\"version\":1}".to_vec());
    assert!(import_rprov(Cursor::new(structurally_valid.encode())).is_ok());

    let checkpoint = structurally_valid.manifest.segments[0].checkpoints[0]
        .entry
        .clone();
    let mut unknown_checkpoint = structurally_valid.clone();
    unknown_checkpoint.replace_payload(&checkpoint, b"RUSTCPK\0\0\0\0\x02future".to_vec());
    assert!(matches!(
        import_rprov(Cursor::new(unknown_checkpoint.encode())),
        Err(RprovImportError::Model(
            RprovError::UnsupportedInnerVersion {
                layer: "checkpoint",
                found: 2,
                supported: 1
            }
        ))
    ));

    let mut malformed_metadata = Fixture::one_segment();
    malformed_metadata.add_metadata(b"{\"version\":1} trailing".to_vec());
    assert!(import_rprov(Cursor::new(malformed_metadata.encode())).is_err());

    let mut unknown_metadata = Fixture::one_segment();
    unknown_metadata.add_metadata(b"{\"version\":2}".to_vec());
    assert!(matches!(
        import_rprov(Cursor::new(unknown_metadata.encode())),
        Err(RprovImportError::Model(
            RprovError::UnsupportedInnerVersion {
                layer: "runtime metadata",
                found: 2,
                supported: 1
            }
        ))
    ));
}

#[test]
fn outer_rejects_bad_paths_aliases_links_special_entries_and_nested_archives() {
    let rprov = Fixture::one_segment().encode();
    let cases = [
        vec![
            ZipEntry::file("session.rprov", rprov.clone()),
            ZipEntry::file("../escaped.rs", b"private".to_vec()),
        ],
        vec![
            ZipEntry::file("session.rprov", rprov.clone()),
            ZipEntry::file("/absolute.rs", vec![]),
        ],
        vec![
            ZipEntry::file("session.rprov", rprov.clone()),
            ZipEntry::file("src\\main.rs", vec![]),
        ],
        vec![
            ZipEntry::file("session.rprov", rprov.clone()),
            ZipEntry::file("Cargo.toml", vec![]),
            ZipEntry::file("cargo.toml", vec![]),
        ],
        vec![
            ZipEntry::file("session.rprov", rprov.clone()),
            ZipEntry::file("cafe\u{301}.rs", vec![]),
        ],
        {
            let mut link = ZipEntry::file("link", b"target".to_vec());
            link.external_attributes = 0o120777 << 16;
            vec![ZipEntry::file("session.rprov", rprov.clone()), link]
        },
        {
            let mut special = ZipEntry::file("device", vec![]);
            special.external_attributes = 0o020600 << 16;
            vec![ZipEntry::file("session.rprov", rprov.clone()), special]
        },
        vec![
            ZipEntry::file("session.rprov", rprov.clone()),
            ZipEntry::file("old.zip", vec![]),
        ],
        vec![
            ZipEntry::file("session.rprov", rprov.clone()),
            ZipEntry::file("nested.rprov", rprov.clone()),
        ],
        vec![ZipEntry::file("Cargo.toml", vec![])],
        vec![
            ZipEntry::file("session.rprov", rprov.clone()),
            ZipEntry::file("session.rprov", rprov),
        ],
    ];
    for entries in cases {
        assert!(import_rprov(Cursor::new(zip(&entries))).is_err());
    }
}

#[test]
fn correction1_rejects_unicode_aliases_including_file_prefixes() {
    let rprov = Fixture::one_segment().encode();
    let mut accepted = Vec::new();
    for (left, right) in [("σ.rs", "ς.rs"), ("ß.rs", "ss.rs"), ("ſ.rs", "s.rs")] {
        let entries = [
            ZipEntry::file("session.rprov", rprov.clone()),
            utf8_file(left, vec![]),
            utf8_file(right, vec![]),
        ];
        if import_rprov(Cursor::new(zip(&entries))).is_ok() {
            accepted.push(format!("{left:?} / {right:?}"));
        }
    }

    let prefix_alias = [
        ZipEntry::file("session.rprov", rprov.clone()),
        utf8_file("ß", vec![]),
        utf8_file("ss/main.rs", vec![]),
    ];
    if import_rprov(Cursor::new(zip(&prefix_alias))).is_ok() {
        accepted.push("file ß / prefix ss".to_owned());
    }
    assert!(
        accepted.is_empty(),
        "accepted portable Unicode aliases: {accepted:?}"
    );
}

#[test]
fn correction1_preserves_distinct_unicode_paths() {
    let rprov = Fixture::one_segment().encode();
    let distinct = [
        ZipEntry::file("session.rprov", rprov),
        utf8_file("東京.rs", b"tokyo".to_vec()),
        utf8_file("京都.rs", b"kyoto".to_vec()),
    ];
    let imported = import_rprov(Cursor::new(zip(&distinct)))
        .expect("distinct canonical Unicode source paths remain valid");
    assert_eq!(imported.outer_source_files().len(), 2);
    for expected in ["東京.rs", "京都.rs"] {
        assert!(
            imported
                .outer_source_files()
                .iter()
                .any(|entry| entry.path.as_str() == expected)
        );
    }
}

#[test]
fn correction1_rejects_dos_volume_labels_for_rprov_and_source_entries() {
    let rprov = Fixture::one_segment().encode();
    let mut labeled_rprov = ZipEntry::file("session.rprov", rprov.clone());
    labeled_rprov.made_by_system = 0;
    labeled_rprov.external_attributes = 0x08;
    let mut accepted = Vec::new();
    if import_rprov(Cursor::new(zip(&[labeled_rprov]))).is_ok() {
        accepted.push("session.rprov");
    }

    let mut labeled_source = ZipEntry::file("src/main.rs", b"fn main() {}\n".to_vec());
    labeled_source.made_by_system = 0;
    labeled_source.external_attributes = 0x08;
    if import_rprov(Cursor::new(zip(&[
        ZipEntry::file("session.rprov", rprov),
        labeled_source,
    ])))
    .is_ok()
    {
        accepted.push("outer source");
    }
    assert!(
        accepted.is_empty(),
        "accepted DOS volume-label records: {accepted:?}"
    );
}

#[test]
fn outer_rejects_unsupported_flags_compression_extras_crc_and_truncation() {
    let rprov = Fixture::one_segment().encode();
    let mut flag = ZipEntry::file("session.rprov", rprov.clone());
    flag.flags = 1 << 3;
    let mut method = ZipEntry::file("session.rprov", rprov.clone());
    method.method = ZipMethod::Unsupported(12);
    let mut version = ZipEntry::file("session.rprov", rprov.clone());
    version.version_needed = 0;
    let mut extra = ZipEntry::file("session.rprov", rprov.clone());
    extra.extra = vec![1, 2, 3, 4];
    let mut crc = ZipEntry::file("session.rprov", rprov);
    crc.crc32 = Some(0);
    for entries in [
        vec![flag],
        vec![method],
        vec![version],
        vec![extra],
        vec![crc],
    ] {
        assert!(import_rprov(Cursor::new(zip(&entries))).is_err());
    }

    let valid = fixed_outer(Fixture::one_segment().encode());
    for cut in [1, 21, valid.len() - 1, valid.len() - 22] {
        assert!(import_rprov(Cursor::new(valid[..cut].to_vec())).is_err());
    }
}

#[test]
fn outer_enforces_claimed_and_actual_aggregate_limits_before_decompression() {
    let rprov = Fixture::one_segment().encode();

    let mut oversized_rprov = ZipEntry::file("session.rprov", rprov.clone());
    oversized_rprov.declared_expanded = Some(MAX_RPROV_STORED_BYTES as u32 + 1);
    assert!(matches!(
        import_rprov(Cursor::new(zip(&[oversized_rprov]))),
        Err(RprovImportError::LimitExceeded { .. })
    ));

    let oversized_source = ZipEntry::file(
        "large.rs",
        vec![0; rustrace_workspace::hash::MAX_WORKSPACE_FILE_BYTES as usize + 1],
    );
    assert!(matches!(
        import_rprov(Cursor::new(zip(&[
            ZipEntry::file("session.rprov", rprov.clone()),
            oversized_source,
        ]))),
        Err(RprovImportError::LimitExceeded { .. })
    ));

    let mut too_many = vec![ZipEntry::file("session.rprov", rprov)];
    too_many.extend(
        (0..=rustrace_workspace::hash::MAX_WORKSPACE_FILES)
            .map(|index| ZipEntry::file(&format!("file-{index:03}.rs"), vec![])),
    );
    assert!(matches!(
        import_rprov(Cursor::new(zip(&too_many))),
        Err(RprovImportError::LimitExceeded { .. })
    ));

    let mut aggregate_source = vec![ZipEntry::file(
        "session.rprov",
        Fixture::one_segment().encode(),
    )];
    aggregate_source.extend((0..10).map(|index| {
        let mut entry = ZipEntry::file(&format!("source-{index}.rs"), vec![]);
        entry.declared_expanded =
            Some(u32::try_from(rustrace_workspace::hash::MAX_WORKSPACE_FILE_BYTES).unwrap());
        entry
    }));
    let mut over = ZipEntry::file("source-over.rs", vec![]);
    over.declared_expanded = Some(1);
    aggregate_source.push(over);
    assert!(matches!(
        import_rprov(Cursor::new(zip(&aggregate_source))),
        Err(RprovImportError::LimitExceeded {
            field: "outer source bytes",
            actual,
            maximum,
        }) if actual == maximum + 1
            && maximum == rustrace_workspace::hash::MAX_WORKSPACE_TOTAL_BYTES
    ));

    let mut bad_crc_rprov = ZipEntry::file("session.rprov", Fixture::one_segment().encode());
    bad_crc_rprov.crc32 = Some(0);
    let mut aggregate_entries = vec![
        bad_crc_rprov,
        ZipEntry::file("Cargo.toml", b"[package]".to_vec()),
    ];
    aggregate_entries.extend(
        (0..MAX_RPROV_ARCHIVE_ENTRIES - 2)
            .map(|index| ZipEntry::directory(&format!("directory-{index:04}/"))),
    );
    assert!(matches!(
        import_rprov(Cursor::new(zip(&aggregate_entries))),
        Err(RprovImportError::LimitExceeded {
            field: "aggregate outer and inner entries",
            actual,
            maximum,
        }) if actual == maximum + 1 && maximum == MAX_RPROV_ARCHIVE_ENTRIES as u64
    ));

    let inner_header = encode_rprov_container_header(&RprovContainerHeader {
        format_version: 1,
        entry_count: (MAX_RPROV_ARCHIVE_ENTRIES - 1) as u32,
        stored_records_bytes: 0,
        expanded_records_bytes: 0,
    })
    .unwrap();
    assert!(matches!(
        import_rprov(Cursor::new(zip(&[
            ZipEntry::file("session.rprov", inner_header.to_vec()),
            ZipEntry::file("Cargo.toml", b"[package]".to_vec()),
        ]))),
        Err(RprovImportError::LimitExceeded {
            field: "aggregate outer and inner entries",
            actual,
            maximum,
        }) if actual == maximum + 1 && maximum == MAX_RPROV_ARCHIVE_ENTRIES as u64
    ));
}

#[test]
fn outer_decompression_checks_exact_lengths_and_crc_without_unbounded_output() {
    let rprov = Fixture::one_segment().encode();
    let mut short = ZipEntry::file("session.rprov", rprov.clone());
    short.method = ZipMethod::Deflated;
    short.declared_expanded = Some(rprov.len() as u32 + 1);
    assert!(import_rprov(Cursor::new(zip(&[short]))).is_err());

    let mut overflow = ZipEntry::file("session.rprov", rprov.clone());
    overflow.method = ZipMethod::Deflated;
    overflow.declared_expanded = Some(rprov.len() as u32 - 1);
    assert!(import_rprov(Cursor::new(zip(&[overflow]))).is_err());

    let mut trailing_compressed = ZipEntry::file("session.rprov", rprov);
    trailing_compressed.method = ZipMethod::Deflated;
    trailing_compressed.declared_compressed = Some(u32::MAX);
    assert!(import_rprov(Cursor::new(zip(&[trailing_compressed]))).is_err());
}

#[test]
fn errors_do_not_echo_private_payload_or_path_bytes_and_no_path_is_extracted() {
    let private = "DO_NOT_ECHO_PRIVATE_PAYLOAD";
    let outer = zip(&[
        ZipEntry::file("session.rprov", Fixture::one_segment().encode()),
        ZipEntry::file(&format!("../{private}"), private.as_bytes().to_vec()),
    ]);
    let error = import_rprov(Cursor::new(outer)).expect_err("hostile path");
    let display = error.to_string();
    assert!(!display.contains(private));
    assert!(!format!("{error:?}").contains(private));
    assert!(!std::path::Path::new(private).exists());

    let fixture = Fixture::one_segment();
    let mut records = fixture.records();
    let insertion = records[0].1.len() - 2;
    records[0]
        .1
        .splice(insertion..insertion, format!(",\"{private}\":0").bytes());
    let error = import_rprov(Cursor::new(encode_records(records)))
        .expect_err("private unknown manifest field");
    assert!(!error.to_string().contains(private));
    assert!(!format!("{error:?}").contains(private));

    let mut fixture = Fixture::one_segment();
    let event_path = fixture.manifest.segments[0].events.entry.clone();
    let mut event_bytes = fixture.payloads.get(&event_path).unwrap().clone();
    let known = b"workspace_checkpoint";
    let offset = event_bytes
        .windows(known.len())
        .position(|window| window == known)
        .unwrap();
    event_bytes.splice(offset..offset + known.len(), private.bytes());
    fixture.replace_payload(&event_path, event_bytes);
    let error =
        import_rprov(Cursor::new(fixture.encode())).expect_err("private unknown event variant");
    assert!(!error.to_string().contains(private));
    assert!(!format!("{error:?}").contains(private));
}

#[test]
fn missing_archive_local_lookups_fail_without_normalizing_paths() {
    let imported = import_rprov(Cursor::new(Fixture::one_segment().encode())).unwrap();
    assert!(matches!(
        imported.open_entry("segments/0001/../events.jsonl"),
        Err(RprovImportError::MissingEntry)
    ));
    assert!(matches!(
        imported.open_outer_source(&WorkspacePath::new("missing.rs").unwrap()),
        Err(RprovImportError::MissingEntry)
    ));
}
