#[path = "support/test_home.rs"]
mod test_home;
use rustrace::{
    replay_tui::{EventPosition, ReplayController},
    review_flags::{
        AdvisoryFlagKind, EVIDENCE_CONSISTENCY, EVIDENCE_LIMITATION_FIRST,
        EVIDENCE_LIMITATION_SECOND, ReviewFlagKind, ReviewFlagLink, display_advisory, display_flag,
        review_flags,
    },
    scan::run_scan,
    session::{ProductionSession, create_bundle},
    tui::EditorCommand,
    verify::{
        AssignmentReferenceStatus, SubmittedSourceStatus, VerificationIssueKind,
        VerificationStatus, verify_path,
    },
};
use rustrace_model::{
    DecodeOutcome, DecodePolicy, Event, Hash, RPROV_FORMAT_VERSION_V1, RPROV_RECORD_HEADER_BYTES,
    RprovContainerHeader, RprovEntryKind, RprovKnown, RprovManifest, RprovRecordHeader,
    RprovRecordType, SessionId, compute_event_hash, decode_envelope, encode_envelope,
    encode_rprov_container_header, encode_rprov_manifest, encode_rprov_record_header,
    rprov_raw_blake3,
};
use rustrace_workspace::{
    assignment_package::{ExtractionLimits, extract_assignment_package},
    rprov_import::import_rprov,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Verify"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["**"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

const MANIFEST_V2: &[u8] = br#"format_version = 2
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Verify"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["**"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

struct Fixture {
    base: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn start_session(&self) -> ProductionSession {
        let mut session = ProductionSession::start(&self.workspace, MANIFEST).unwrap();
        session.execute(EditorCommand::NextBuffer).unwrap();
        session
    }

    fn new(prefix: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "rustrace-{prefix}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = base.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("main.rs"), "A").unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n",
        )
        .unwrap();
        Self {
            base: fs::canonicalize(base).unwrap(),
            workspace: fs::canonicalize(workspace).unwrap(),
        }
    }

    fn bundle(&self, name: &str) -> PathBuf {
        let receipt = self.start_session().finalize("student-1").unwrap();
        create_bundle(&receipt, &self.base.join(name)).unwrap().path
    }

    fn bundle_with_test_case_suite(&self, name: &str, cases: &[(&str, &[u8], &[u8])]) -> PathBuf {
        let package = self.base.join("session-assignment.rta");
        let extraction = self.base.join("session-assignment-extraction");
        fs::write(
            &package,
            assignment_package_with_cases(MANIFEST_V2, b"A", cases),
        )
        .unwrap();
        let extracted = extract_assignment_package(
            fs::File::open(package).unwrap(),
            &extraction,
            ExtractionLimits::default(),
        )
        .unwrap();
        let receipt = ProductionSession::start_from_assignment(&self.workspace, &extracted)
            .unwrap()
            .finalize("student-1")
            .unwrap();
        fs::remove_dir_all(extraction).unwrap();
        create_bundle(&receipt, &self.base.join(name)).unwrap().path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn stored_zip_entry(archive: &[u8], wanted: &str) -> Vec<u8> {
    let mut offset = 0;
    while archive.get(offset..offset + 4) == Some(&0x0403_4b50_u32.to_le_bytes()) {
        let compressed = u32::from_le_bytes(archive[offset + 18..offset + 22].try_into().unwrap());
        let name_len =
            u16::from_le_bytes(archive[offset + 26..offset + 28].try_into().unwrap()) as usize;
        let extra_len =
            u16::from_le_bytes(archive[offset + 28..offset + 30].try_into().unwrap()) as usize;
        let name_start = offset + 30;
        let data_start = name_start + name_len + extra_len;
        let name = std::str::from_utf8(&archive[name_start..name_start + name_len]).unwrap();
        if name == wanted {
            return archive[data_start..data_start + compressed as usize].to_vec();
        }
        offset = data_start + compressed as usize;
    }
    panic!("missing stored ZIP entry {wanted}");
}

fn tamper_stored_zip_entry(mut archive: Vec<u8>, wanted: &str, replacement: &[u8]) -> Vec<u8> {
    let end = archive.len() - 22;
    let mut central = u32::from_le_bytes(archive[end + 16..end + 20].try_into().unwrap()) as usize;
    loop {
        let expanded =
            u32::from_le_bytes(archive[central + 24..central + 28].try_into().unwrap()) as usize;
        let name_len =
            u16::from_le_bytes(archive[central + 28..central + 30].try_into().unwrap()) as usize;
        let extra_len =
            u16::from_le_bytes(archive[central + 30..central + 32].try_into().unwrap()) as usize;
        let comment_len =
            u16::from_le_bytes(archive[central + 32..central + 34].try_into().unwrap()) as usize;
        let name = std::str::from_utf8(&archive[central + 46..central + 46 + name_len]).unwrap();
        if name == wanted {
            assert_eq!(replacement.len(), expanded);
            let local = u32::from_le_bytes(archive[central + 42..central + 46].try_into().unwrap())
                as usize;
            let local_name_len =
                u16::from_le_bytes(archive[local + 26..local + 28].try_into().unwrap()) as usize;
            let local_extra_len =
                u16::from_le_bytes(archive[local + 28..local + 30].try_into().unwrap()) as usize;
            let data = local + 30 + local_name_len + local_extra_len;
            archive[data..data + replacement.len()].copy_from_slice(replacement);
            let crc = crc32fast::hash(replacement).to_le_bytes();
            archive[local + 14..local + 18].copy_from_slice(&crc);
            archive[central + 16..central + 20].copy_from_slice(&crc);
            return archive;
        }
        central += 46 + name_len + extra_len + comment_len;
    }
}

fn collect_rprov(rprov: &[u8]) -> (RprovManifest, BTreeMap<String, Vec<u8>>) {
    let imported = import_rprov(Cursor::new(rprov)).unwrap();
    let manifest = imported.manifest().clone();
    let mut payloads = BTreeMap::new();
    for entry in &manifest.inventory {
        let mut bytes = Vec::new();
        imported
            .open_entry(&entry.path)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        payloads.insert(entry.path.clone(), bytes);
    }
    (manifest, payloads)
}

fn encode_rprov(manifest: &RprovManifest, payloads: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let manifest_bytes = encode_rprov_manifest(manifest).unwrap();
    encode_rprov_with_manifest(manifest, payloads, manifest_bytes)
}

fn encode_rprov_unchecked(
    manifest: &RprovManifest,
    payloads: &BTreeMap<String, Vec<u8>>,
) -> Vec<u8> {
    let mut manifest_bytes = serde_json::to_vec(manifest).unwrap();
    manifest_bytes.push(b'\n');
    encode_rprov_with_manifest(manifest, payloads, manifest_bytes)
}

fn encode_rprov_with_manifest(
    manifest: &RprovManifest,
    payloads: &BTreeMap<String, Vec<u8>>,
    manifest_bytes: Vec<u8>,
) -> Vec<u8> {
    let mut records_bytes = framed_record_bytes("manifest.json", manifest_bytes.len() as u64);
    for entry in &manifest.inventory {
        records_bytes += framed_record_bytes(&entry.path, entry.byte_length);
    }
    let header = RprovContainerHeader {
        format_version: RPROV_FORMAT_VERSION_V1,
        entry_count: (manifest.inventory.len() + 1) as u32,
        stored_records_bytes: records_bytes,
        expanded_records_bytes: records_bytes,
    };
    let mut output = encode_rprov_container_header(&header).unwrap().to_vec();
    write_record(&mut output, "manifest.json", &manifest_bytes);
    for entry in &manifest.inventory {
        write_record(&mut output, &entry.path, &payloads[&entry.path]);
    }
    output
}

fn framed_record_bytes(path: &str, payload: u64) -> u64 {
    RPROV_RECORD_HEADER_BYTES as u64 + path.len() as u64 + payload
}

fn write_record(output: &mut Vec<u8>, path: &str, payload: &[u8]) {
    let header = RprovRecordHeader {
        path_bytes: path.len() as u16,
        entry_type: RprovRecordType::RegularFile,
        payload_bytes: payload.len() as u64,
    };
    output.extend_from_slice(&encode_rprov_record_header(&header).unwrap());
    output.extend_from_slice(path.as_bytes());
    output.extend_from_slice(payload);
}

fn replace_payload(
    manifest: &mut RprovManifest,
    payloads: &mut BTreeMap<String, Vec<u8>>,
    path: &str,
    replacement: Vec<u8>,
) {
    let digest = rprov_raw_blake3(&replacement);
    let length = replacement.len() as u64;
    let inventory = manifest
        .inventory
        .iter_mut()
        .find(|entry| entry.path == path)
        .unwrap();
    inventory.blake3 = digest;
    inventory.byte_length = length;
    match inventory.kind {
        RprovEntryKind::Events => {
            let reference = manifest
                .segments
                .iter_mut()
                .find(|segment| segment.events.entry == path)
                .unwrap();
            reference.events.blake3 = digest;
            reference.events.byte_length = length;
        }
        RprovEntryKind::Checkpoint => {
            let reference = manifest
                .segments
                .iter_mut()
                .flat_map(|segment| &mut segment.checkpoints)
                .find(|checkpoint| checkpoint.entry == path)
                .unwrap();
            reference.blake3 = digest;
            reference.byte_length = length;
        }
        _ => panic!("replacement helper supports events and checkpoints"),
    }
    payloads.insert(path.to_owned(), replacement);
}

fn rebind_terminal_clean(
    manifest: &mut RprovManifest,
    payloads: &mut BTreeMap<String, Vec<u8>>,
    clean: bool,
) {
    let event_path = manifest.segments[0].events.entry.clone();
    let mut lines = payloads[&event_path]
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    let last = lines.last_mut().expect("terminal event");
    assert_eq!(last.pop(), Some(b'\n'));
    let DecodeOutcome::Decoded(mut envelope) =
        decode_envelope(last, DecodePolicy::RejectUnsupported).unwrap()
    else {
        panic!("production event must decode")
    };
    let Event::SubmissionFinalized(finalized) = &mut envelope.event else {
        panic!("production stream must end with finalization")
    };
    finalized.clean = clean;
    envelope.event_hash = compute_event_hash(envelope.previous_event_hash, &envelope).unwrap();
    *last = encode_envelope(&envelope).unwrap();
    last.push(b'\n');
    manifest.segments[0].last_event_hash = envelope.event_hash;
    manifest.segments[0].terminal_event_hash = RprovKnown::Known {
        value: envelope.event_hash,
    };
    replace_payload(manifest, payloads, &event_path, lines.concat());
}

fn write_mutated_rprov(
    base: &Path,
    name: &str,
    manifest: &RprovManifest,
    payloads: &BTreeMap<String, Vec<u8>>,
) -> PathBuf {
    let path = base.join(name);
    fs::write(&path, encode_rprov(manifest, payloads)).unwrap();
    path
}

fn assert_flag(report: &rustrace::verify::VerificationReport, expected: ReviewFlagKind) {
    let flags = review_flags(report);
    assert_eq!(
        flags.iter().map(|flag| flag.kind).collect::<Vec<_>>(),
        vec![expected],
        "unexpected flags for {report:?}"
    );
    let flag = &flags[0];
    assert!(!flag.detail.is_empty());
    match &flag.link {
        ReviewFlagLink::Event(location) => assert!(location.sequence > 0),
        ReviewFlagLink::Artifact(path) | ReviewFlagLink::Location(path) => {
            assert!(!path.is_empty())
        }
    }
}

fn write_reference(base: &Path, manifest: &[u8], starter: &[u8]) -> PathBuf {
    let root = base.join("reference");
    fs::create_dir_all(root.join("starter")).unwrap();
    fs::write(root.join("assignment.toml"), manifest).unwrap();
    fs::write(root.join("starter/main.rs"), starter).unwrap();
    fs::write(
        root.join("starter/Cargo.toml"),
        b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n",
    )
    .unwrap();
    root.join("assignment.toml")
}

fn assignment_package(manifest: &[u8], starter: &[u8]) -> Vec<u8> {
    assignment_package_with_cases(manifest, starter, &[])
}

fn assignment_package_with_cases(
    manifest: &[u8],
    starter: &[u8],
    cases: &[(&str, &[u8], &[u8])],
) -> Vec<u8> {
    assignment_package_with_cargo(
        manifest,
        starter,
        cases,
        b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n",
    )
}

fn assignment_package_with_cargo(
    manifest: &[u8],
    starter: &[u8],
    cases: &[(&str, &[u8], &[u8])],
    cargo: &[u8],
) -> Vec<u8> {
    let mut archive = Vec::new();
    let mut entries = vec![
        ("assignment.toml".to_owned(), manifest),
        ("starter/Cargo.toml".to_owned(), cargo),
        ("starter/main.rs".to_owned(), starter),
    ];
    for (name, input, expected) in cases {
        entries.push((format!("test-cases/{name}.in"), *input));
        entries.push((format!("test-cases/{name}.expected"), *expected));
    }
    for (path, contents) in entries {
        let mut header = [0_u8; 512];
        header[..path.len()].copy_from_slice(path.as_bytes());
        write_octal(&mut header[100..108], 0o644);
        write_octal(&mut header[108..116], 0);
        write_octal(&mut header[116..124], 0);
        write_octal(&mut header[124..136], contents.len() as u64);
        write_octal(&mut header[136..148], 0);
        header[148..156].fill(b' ');
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        archive.extend_from_slice(&header);
        archive.extend_from_slice(contents);
        archive.resize(archive.len().next_multiple_of(512), 0);
    }
    archive.resize(archive.len() + 1024, 0);
    archive
}

fn write_octal(field: &mut [u8], value: u64) {
    let encoded = format!("{:0width$o}\0", value, width = field.len() - 1);
    field.copy_from_slice(encoded.as_bytes());
}

fn rename_stored_zip_entry(mut archive: Vec<u8>, old: &str, new: &str) -> Vec<u8> {
    assert_eq!(old.len(), new.len());
    let end = archive.len() - 22;
    let mut central = u32::from_le_bytes(archive[end + 16..end + 20].try_into().unwrap()) as usize;
    loop {
        let name_len =
            u16::from_le_bytes(archive[central + 28..central + 30].try_into().unwrap()) as usize;
        let extra_len =
            u16::from_le_bytes(archive[central + 30..central + 32].try_into().unwrap()) as usize;
        let comment_len =
            u16::from_le_bytes(archive[central + 32..central + 34].try_into().unwrap()) as usize;
        let name = &archive[central + 46..central + 46 + name_len];
        if name == old.as_bytes() {
            let local = u32::from_le_bytes(archive[central + 42..central + 46].try_into().unwrap())
                as usize;
            archive[central + 46..central + 46 + name_len].copy_from_slice(new.as_bytes());
            archive[local + 30..local + 30 + name_len].copy_from_slice(new.as_bytes());
            return archive;
        }
        central += 46 + name_len + extra_len + comment_len;
    }
}

#[test]
fn non_self_contained_assignment_references_report_the_structure_error() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("verify-starter-structure");
    let bundle = fixture.bundle("submission.zip");
    let reference = fixture.base.join("invalid-starter.rta");
    for manifest in [MANIFEST, MANIFEST_V2] {
        for cargo in [
            b"[package]\n".as_slice(),
            b"[package]\n[workspace]\nmembers = []\n".as_slice(),
        ] {
            let cases = if manifest == MANIFEST_V2 {
                vec![("sample", b"".as_slice(), b"".as_slice())]
            } else {
                Vec::new()
            };
            fs::write(
                &reference,
                assignment_package_with_cargo(manifest, b"A", &cases, cargo),
            )
            .unwrap();
            let report = verify_path(&bundle, Some(&reference));
            assert_eq!(
                report.assignment_reference,
                AssignmentReferenceStatus::Unverified
            );
            assert_eq!(report.exit_code(), 2);
            assert!(
                report.issues.iter().any(|issue| issue.kind
                    == VerificationIssueKind::AssignmentReference
                    && issue
                        .detail
                        .contains("assignment starter must be a self-contained package:")),
                "{report:?}"
            );
            let output = test_home
                .command(env!("CARGO_BIN_EXE_rustrace"))
                .arg("verify")
                .arg(&bundle)
                .arg("--reference")
                .arg(&reference)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(2));
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert!(
                stdout.contains("assignment starter must be a self-contained package:"),
                "{stdout}"
            );
        }
    }
}

#[test]
fn verify_cli_accepts_production_zip_and_standalone_without_opening_the_tui() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("verify-cli-clean");
    let bundle = fixture.bundle("submission.zip");

    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("verify")
        .arg(&bundle)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "Package structure        OK",
        "Event chain              OK",
        "Checkpoint hashes        OK",
        "Replay                   OK",
        "Submitted source match   OK",
        "Assignment reference     unverified",
        "Test-case runs           0",
        "Test-case passes         0",
        "Test-case mismatches     0",
        "Test-case errors         0",
        "Test-case evidence       recorded (unverified)",
        "First failing case       none",
        "External changes         0",
        "Unknown edit origins     0",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in {stdout:?}"
        );
    }

    let standalone = fixture.base.join("session.rprov");
    fs::write(
        &standalone,
        stored_zip_entry(&fs::read(&bundle).unwrap(), "session.rprov"),
    )
    .unwrap();
    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("verify")
        .arg(&standalone)
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Submitted source match   unavailable"));
}

#[test]
fn verify_advisory_golden_keeps_consistent_exit_zero_and_prints_before_evidence() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("verify-advisory");
    fs::write(
        fixture.workspace.join("main.rs"),
        (0..80).map(|_| "x\n").collect::<String>(),
    )
    .unwrap();
    let mut session = fixture.start_session();
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::ToggleComment).unwrap();
    let receipt = session.finalize("student-1").unwrap();
    let bundle = create_bundle(&receipt, &fixture.base.join("advisory.zip"))
        .unwrap()
        .path;

    let report = verify_path(&bundle, None);
    assert!(report.is_clean(), "{report:#?}");
    assert_eq!(report.exit_code(), 0);
    assert_eq!(review_flags(&report), vec![]);
    assert_eq!(report.advisories.len(), 1, "{:#?}", report.advisories);
    assert_eq!(
        report.advisories[0].kind,
        AdvisoryFlagKind::LargeSingleInsertion
    );

    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("verify")
        .arg(&bundle)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines = stdout.lines().collect::<Vec<_>>();
    let advisory = format!("Advisory: {}", display_advisory(&report.advisories[0]));
    let advisory_index = lines
        .iter()
        .position(|line| *line == advisory)
        .unwrap_or_else(|| panic!("missing advisory golden line in {stdout:?}"));
    assert_eq!(lines[advisory_index - 1], "Unknown edit origins     0");
    assert_eq!(
        lines[advisory_index + 1],
        format!(
            "Evidence limitations: {EVIDENCE_CONSISTENCY} {EVIDENCE_LIMITATION_FIRST} {EVIDENCE_LIMITATION_SECOND}"
        )
    );
}

#[test]
fn verify_reports_source_and_reference_independently_of_replay() {
    let fixture = Fixture::new("verify-source-reference");
    let bundle = fixture.bundle("submission.zip");
    let reference = write_reference(&fixture.base, MANIFEST, b"A");

    let matching = verify_path(&bundle, Some(&reference));
    assert_eq!(matching.replay, VerificationStatus::Ok);
    assert_eq!(matching.submitted_source_match, SubmittedSourceStatus::Ok);
    assert_eq!(matching.assignment_reference, AssignmentReferenceStatus::Ok);
    assert!(matching.is_clean());

    let mut changed_manifest = MANIFEST.to_vec();
    let title = changed_manifest
        .windows(b"title = \"Verify\"".len())
        .position(|window| window == b"title = \"Verify\"")
        .unwrap();
    changed_manifest[title + 9] = b'X';
    fs::write(&reference, changed_manifest).unwrap();
    let mismatch = verify_path(&bundle, Some(&reference));
    assert_eq!(mismatch.replay, VerificationStatus::Ok);
    assert_eq!(
        mismatch.assignment_reference,
        AssignmentReferenceStatus::Mismatch
    );
    assert!(!mismatch.is_clean());

    let tampered = fixture.base.join("source-mismatch.zip");
    fs::write(
        &tampered,
        tamper_stored_zip_entry(fs::read(&bundle).unwrap(), "main.rs", b"Z"),
    )
    .unwrap();
    let mismatch = verify_path(&tampered, None);
    assert_eq!(mismatch.replay, VerificationStatus::Ok);
    assert_eq!(
        mismatch.submitted_source_match,
        SubmittedSourceStatus::SourceMismatch
    );
    assert!(!mismatch.is_clean());
    assert_flag(&mismatch, ReviewFlagKind::SourceMismatch);

    let mut replay = ReplayController::open(&tampered).unwrap();
    assert!(replay.timeline_available());
    let first = replay.positions().next().unwrap();
    let last = replay.positions().last().unwrap();
    assert_ne!(first, last);
    let flag = replay.review_flags().remove(0);
    assert_eq!(
        flag.link,
        ReviewFlagLink::Event(rustrace::verify::VerificationEventLocation {
            segment: (last.segment + 1) as u32,
            sequence: last.sequence,
        })
    );
    assert_eq!(replay.selected_event().unwrap().position, first);
    replay.follow_review_flag(&flag).unwrap();
    assert_eq!(replay.selected_event().unwrap().position, last);
}

#[test]
fn verify_compares_packaged_test_case_suite_identity_and_kind() {
    let cases = [("alpha", b"1\n".as_slice(), b"2\n".as_slice())];
    let fixture = Fixture::new("verify-test-case-suite");
    let bundle = fixture.bundle_with_test_case_suite("submission.zip", &cases);
    let matching = fixture.base.join("matching.rta");
    fs::write(
        &matching,
        assignment_package_with_cases(MANIFEST_V2, b"A", &cases),
    )
    .unwrap();
    let verified = verify_path(&bundle, Some(&matching));
    assert_eq!(verified.assignment_reference, AssignmentReferenceStatus::Ok);
    assert_eq!(
        verified.test_case_evidence,
        Some(rustrace::verify::TestCaseEvidenceStatus::ReferenceVerified)
    );

    let changed_cases = [("alpha", b"1\n".as_slice(), b"3\n".as_slice())];
    let changed = fixture.base.join("changed.rta");
    fs::write(
        &changed,
        assignment_package_with_cases(MANIFEST_V2, b"A", &changed_cases),
    )
    .unwrap();
    let mismatch = verify_path(&bundle, Some(&changed));
    assert_eq!(
        mismatch.assignment_reference,
        AssignmentReferenceStatus::Mismatch
    );
    assert!(mismatch.issues.iter().any(|issue| {
        issue.kind == VerificationIssueKind::AssignmentReference
            && issue.detail.contains("test-case suite identity mismatch")
    }));

    let legacy_reference = fixture.base.join("legacy.rta");
    fs::write(&legacy_reference, assignment_package(MANIFEST, b"A")).unwrap();
    let mismatch = verify_path(&bundle, Some(&legacy_reference));
    assert!(mismatch.issues.iter().any(|issue| {
        issue.kind == VerificationIssueKind::AssignmentReference
            && issue
                .detail
                .contains("reference has no packaged test-case suite")
    }));

    let legacy_fixture = Fixture::new("verify-v2-reference-against-v1");
    let legacy_bundle = legacy_fixture.bundle("submission.zip");
    let mismatch = verify_path(&legacy_bundle, Some(&matching));
    assert!(mismatch.issues.iter().any(|issue| {
        issue.kind == VerificationIssueKind::AssignmentReference
            && issue
                .detail
                .contains("reference has a packaged test-case suite")
    }));

    let plain = write_reference(&fixture.base, MANIFEST_V2, b"A");
    let unverified = verify_path(&bundle, Some(&plain));
    assert_eq!(
        unverified.assignment_reference,
        AssignmentReferenceStatus::Unverified
    );
    assert!(unverified.issues.iter().any(|issue| {
        issue.kind == VerificationIssueKind::AssignmentReference
            && issue.detail.contains("must be an .rta archive")
    }));
}

#[test]
fn missing_reference_is_unverified_with_a_safe_reason_and_usage_exit() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("verify-missing-reference");
    let bundle = fixture.bundle("submission.zip");
    let reference = fixture.base.join("missing\nreference.toml");

    let report = verify_path(&bundle, Some(&reference));
    assert_eq!(
        report.assignment_reference,
        AssignmentReferenceStatus::Unverified
    );
    assert_eq!(report.exit_code(), 2);
    assert!(report.issues.iter().any(|issue| {
        issue.kind == VerificationIssueKind::AssignmentReference && !issue.detail.is_empty()
    }));

    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("verify")
        .arg(&bundle)
        .arg("--reference")
        .arg(&reference)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Assignment reference     unverified"));
    assert!(stdout.contains("Assignment reference unavailable:"));
    assert!(stdout.contains("missing\\nreference.toml"));
    assert!(!stdout.contains("MISMATCH"));
    assert!(!stdout.contains("missing\nreference.toml"));
    assert!(
        stdout.len() <= 4_500,
        "unbounded diagnostic: {} bytes",
        stdout.len()
    );
}

#[test]
fn directory_reference_is_unverified_with_a_reason_and_usage_exit() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("verify-directory-reference");
    let bundle = fixture.bundle("submission.zip");

    let report = verify_path(&bundle, Some(&fixture.base));
    assert_eq!(
        report.assignment_reference,
        AssignmentReferenceStatus::Unverified
    );
    assert_eq!(report.exit_code(), 2);
    assert!(report.issues.iter().any(|issue| {
        issue.kind == VerificationIssueKind::AssignmentReference
            && issue.detail.contains("regular file")
    }));

    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("verify")
        .arg(&bundle)
        .arg("--reference")
        .arg(&fixture.base)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Assignment reference     unverified"));
    assert!(stdout.contains("Assignment reference unavailable:"));
    assert!(stdout.contains("regular file"));
    assert!(!stdout.contains("MISMATCH"));
}

#[test]
fn malformed_references_are_unverified_instead_of_mismatched() {
    let fixture = Fixture::new("verify-malformed-reference");
    let bundle = fixture.bundle("submission.zip");
    let invalid_manifest = fixture.base.join("invalid.toml");
    let malformed_package = fixture.base.join("invalid.rta");
    fs::write(&invalid_manifest, b"not = [valid").unwrap();
    fs::write(&malformed_package, b"not an assignment package").unwrap();

    for reference in [&invalid_manifest, &malformed_package] {
        let report = verify_path(&bundle, Some(reference));
        assert_eq!(
            report.assignment_reference,
            AssignmentReferenceStatus::Unverified
        );
        assert_eq!(report.exit_code(), 2);
        assert!(report.issues.iter().any(|issue| {
            issue.kind == VerificationIssueKind::AssignmentReference && !issue.detail.is_empty()
        }));
    }
}

#[test]
fn unopenable_input_reports_a_usage_error_without_package_verdicts() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("verify-unopenable-input");
    let input = fixture.base.join("missing\nsubmission.zip");

    let report = verify_path(&input, None);
    assert_eq!(report.package_structure, VerificationStatus::Unavailable);
    assert_eq!(report.exit_code(), 2);
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.kind == VerificationIssueKind::Input)
    );

    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("verify")
        .arg(&input)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("could not open "));
    assert!(stdout.contains("missing\\nsubmission.zip"));
    assert!(!stdout.contains("missing\nsubmission.zip"));
    assert!(!stdout.contains("Package structure"));
    assert!(!stdout.contains("FAILED"));
    assert!(
        stdout.len() <= 4_097,
        "unbounded diagnostic: {} bytes",
        stdout.len()
    );
}

#[test]
fn verify_streams_revised_attempts_and_counts_external_observations() {
    let fixture = Fixture::new("verify-revision");
    let mut parent = fixture.start_session();
    let parent_session_id = parent.session_id().clone();
    fs::write(fixture.workspace.join("main.rs"), "outside").unwrap();
    parent.save_all().unwrap();
    parent.execute(EditorCommand::Insert('B')).unwrap();
    let parent_receipt = parent.finalize("student-1").unwrap();

    let child_root = fixture.base.join("child");
    fs::create_dir(&child_root).unwrap();
    for (path, bytes) in parent_receipt.final_workspace() {
        fs::write(child_root.join(path.as_str()), bytes).unwrap();
    }
    let mut child =
        ProductionSession::start_revision(&fixture.workspace, &child_root, MANIFEST).unwrap();
    let child_session_id = child.session_id().clone();
    child.execute(EditorCommand::Insert('C')).unwrap();
    let receipt = child.finalize("student-1").unwrap();
    let bundle = create_bundle(&receipt, &fixture.base.join("revision.zip"))
        .unwrap()
        .path;

    let report = verify_path(&bundle, None);
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.external_changes, Some(1));
    assert_eq!(report.unknown_edit_origins, Some(0));
    let indicators = report
        .review_indicators
        .as_ref()
        .expect("clean replay has review indicators");
    assert_eq!(indicators.attempts.len(), 2);
    assert_eq!(indicators.attempts[0].attempt, 1);
    assert_eq!(indicators.attempts[1].attempt, 2);
    assert_eq!(indicators.attempts[0].session_id, parent_session_id);
    assert_eq!(indicators.attempts[1].session_id, child_session_id);
    assert_eq!(indicators.attempts[0].values.inserted_keyboard_scalars, 1);
    assert_eq!(indicators.attempts[1].values.inserted_keyboard_scalars, 1);
    assert_eq!(indicators.attempts[0].values.feedback_opportunities, 0);
    assert_eq!(indicators.attempts[1].values.feedback_opportunities, 0);
    assert_eq!(
        indicators.factual.rejected_external_change.links[0],
        rustrace::process_indicators::IndicatorEventLink {
            segment: 1,
            session_id: indicators.attempts[0].session_id.clone(),
            sequence: report.first_external_change.unwrap().sequence,
        }
    );
    assert!(
        review_flags(&report).is_empty(),
        "a fully retained rejected external change is context, not a hard flag"
    );
}

#[test]
fn verify_assigns_semantic_event_and_checkpoint_corruption_to_the_right_rows() {
    let fixture = Fixture::new("verify-corruption");
    let bundle = fixture.bundle("submission.zip");
    let clean_rprov = stored_zip_entry(&fs::read(bundle).unwrap(), "session.rprov");

    let (mut manifest, mut payloads) = collect_rprov(&clean_rprov);
    let event_path = manifest.segments[0].events.entry.clone();
    let mut events = payloads[&event_path].clone();
    let hash = events
        .windows(b"\"event_hash\":\"".len())
        .position(|window| window == b"\"event_hash\":\"")
        .unwrap()
        + b"\"event_hash\":\"".len();
    events[hash] = if events[hash] == b'0' { b'1' } else { b'0' };
    replace_payload(&mut manifest, &mut payloads, &event_path, events);
    let event_file = fixture.base.join("event-corrupt.rprov");
    fs::write(&event_file, encode_rprov(&manifest, &payloads)).unwrap();
    let event_report = verify_path(&event_file, None);
    assert_eq!(event_report.package_structure, VerificationStatus::Ok);
    assert_eq!(event_report.event_chain, VerificationStatus::Failed);
    assert!(!event_report.is_clean());
    assert_flag(&event_report, ReviewFlagKind::EventChainInvalid);
    assert!(matches!(
        review_flags(&event_report)[0].link,
        ReviewFlagLink::Event(_)
    ));

    let (mut manifest, mut payloads) = collect_rprov(&clean_rprov);
    let checkpoint_path = manifest.segments[0].checkpoints[0].entry.clone();
    let mut checkpoint = payloads[&checkpoint_path].clone();
    let offset = checkpoint.len() / 2;
    checkpoint[offset] ^= 1;
    replace_payload(&mut manifest, &mut payloads, &checkpoint_path, checkpoint);
    let checkpoint_file = fixture.base.join("checkpoint-corrupt.rprov");
    fs::write(&checkpoint_file, encode_rprov(&manifest, &payloads)).unwrap();
    let checkpoint_report = verify_path(&checkpoint_file, None);
    assert_eq!(
        checkpoint_report.checkpoint_hashes,
        VerificationStatus::Failed
    );
    assert!(!checkpoint_report.is_clean());
    assert_flag(&checkpoint_report, ReviewFlagKind::CheckpointMismatch);
    assert!(matches!(
        review_flags(&checkpoint_report)[0].link,
        ReviewFlagLink::Event(_)
    ));

    let (manifest, mut payloads) = collect_rprov(&clean_rprov);
    let checkpoint_path = manifest.segments[0].checkpoints[0].entry.clone();
    let checkpoint = payloads.get_mut(&checkpoint_path).unwrap();
    let checkpoint_offset = checkpoint.len() / 2;
    checkpoint[checkpoint_offset] ^= 1;
    let checkpoint_digest_file = fixture.base.join("checkpoint-digest-corrupt.rprov");
    fs::write(&checkpoint_digest_file, encode_rprov(&manifest, &payloads)).unwrap();
    let checkpoint_digest_report = verify_path(&checkpoint_digest_file, None);
    assert_eq!(
        checkpoint_digest_report.package_structure,
        VerificationStatus::Failed
    );
    assert_eq!(
        checkpoint_digest_report.checkpoint_hashes,
        VerificationStatus::Failed
    );
    assert_flag(
        &checkpoint_digest_report,
        ReviewFlagKind::CheckpointMismatch,
    );
}

#[test]
fn removed_referenced_external_evidence_is_unprovenanced() {
    let fixture = Fixture::new("verify-evidence-corruption");
    let mut session = fixture.start_session();
    fs::write(fixture.workspace.join("main.rs"), "outside").unwrap();
    session.save_all().unwrap();
    let receipt = session.finalize("student-1").unwrap();
    let bundle = create_bundle(&receipt, &fixture.base.join("submission.zip"))
        .unwrap()
        .path;
    let rprov = stored_zip_entry(&fs::read(bundle).unwrap(), "session.rprov");
    let (mut manifest, mut payloads) = collect_rprov(&rprov);
    let evidence = manifest.segments[0].evidence.remove(0);
    let expected_sequence = evidence
        .usages
        .iter()
        .map(|usage| usage.sequence)
        .min()
        .unwrap();
    manifest
        .inventory
        .retain(|entry| entry.path != evidence.entry);
    payloads.remove(&evidence.entry);
    let path = fixture.base.join("evidence-corrupt.rprov");
    fs::write(&path, encode_rprov(&manifest, &payloads)).unwrap();

    let report = verify_path(&path, None);
    assert_eq!(report.package_structure, VerificationStatus::Failed);
    assert!(!report.is_clean());
    assert_flag(&report, ReviewFlagKind::UnprovenancedExternalChange);
    assert!(matches!(
        review_flags(&report)[0].link,
        ReviewFlagLink::Event(_)
    ));

    let expected_location = rustrace::verify::VerificationEventLocation {
        segment: 1,
        sequence: expected_sequence,
    };
    for _ in 0..32 {
        assert_eq!(
            review_flags(&verify_path(&path, None))[0].link,
            ReviewFlagLink::Event(expected_location)
        );
    }
    let expected_display = display_flag(&review_flags(&report)[0]);
    let mut scan_output = Vec::new();
    run_scan(
        &[fixture.base.to_string_lossy().into_owned()],
        &mut scan_output,
    )
    .unwrap();
    assert!(
        String::from_utf8(scan_output)
            .unwrap()
            .contains(&expected_display)
    );

    let mut replay = ReplayController::open(&path).unwrap();
    assert!(replay.timeline_available());
    let flag = replay.review_flags().remove(0);
    assert_eq!(flag.link, ReviewFlagLink::Event(expected_location));
    replay.follow_review_flag(&flag).unwrap();
    assert_eq!(
        replay.selected_event().unwrap().position,
        EventPosition {
            segment: 0,
            sequence: expected_sequence,
        }
    );
}

#[test]
fn corrupted_external_evidence_payload_is_unprovenanced_and_links_the_artifact() {
    let fixture = Fixture::new("verify-evidence-payload-corruption");
    let mut session = fixture.start_session();
    fs::write(fixture.workspace.join("main.rs"), "outside").unwrap();
    session.save_all().unwrap();
    let receipt = session.finalize("student-1").unwrap();
    let bundle = create_bundle(&receipt, &fixture.base.join("submission.zip"))
        .unwrap()
        .path;
    let clean_rprov = stored_zip_entry(&fs::read(bundle).unwrap(), "session.rprov");
    let (manifest, mut payloads) = collect_rprov(&clean_rprov);
    let evidence_path = manifest.segments[0].evidence[0].entry.clone();
    payloads.get_mut(&evidence_path).unwrap()[0] ^= 1;
    let path = fixture.base.join("evidence-payload-corrupt.rprov");
    fs::write(&path, encode_rprov(&manifest, &payloads)).unwrap();

    let report = verify_path(&path, None);
    assert_flag(&report, ReviewFlagKind::UnprovenancedExternalChange);
    assert_eq!(
        review_flags(&report)[0].link,
        ReviewFlagLink::Artifact(evidence_path.clone())
    );

    let mut replay = ReplayController::open(&path).unwrap();
    assert!(replay.timeline_available());
    let flag = replay.review_flags().remove(0);
    replay.follow_review_flag(&flag).unwrap();
    let (preview_path, preview) = replay.artifact_preview().unwrap();
    assert_eq!(preview_path, evidence_path);
    assert!(!preview.is_empty());
    assert!(preview.len() <= 64 * 1024);
}

#[test]
fn missing_evidence_does_not_hide_a_later_terminal_chain_failure() {
    let fixture = Fixture::new("verify-evidence-plus-terminal");
    let mut session = fixture.start_session();
    fs::write(fixture.workspace.join("main.rs"), "outside").unwrap();
    session.save_all().unwrap();
    let receipt = session.finalize("student-1").unwrap();
    let bundle = create_bundle(&receipt, &fixture.base.join("submission.zip"))
        .unwrap()
        .path;
    let clean_rprov = stored_zip_entry(&fs::read(bundle).unwrap(), "session.rprov");

    let (mut manifest, mut payloads) = collect_rprov(&clean_rprov);
    rebind_terminal_clean(&mut manifest, &mut payloads, false);
    let terminal_path = fixture.base.join("terminal-only.rprov");
    fs::write(&terminal_path, encode_rprov_unchecked(&manifest, &payloads)).unwrap();
    let terminal_report = verify_path(&terminal_path, None);
    assert_eq!(terminal_report.event_chain, VerificationStatus::Failed);
    assert_eq!(
        review_flags(&terminal_report)
            .iter()
            .map(|flag| flag.kind)
            .collect::<Vec<_>>(),
        vec![ReviewFlagKind::EventChainInvalid]
    );

    let evidence = manifest.segments[0].evidence.remove(0);
    manifest
        .inventory
        .retain(|entry| entry.path != evidence.entry);
    payloads.remove(&evidence.entry);
    let combined_path = fixture.base.join("terminal-and-evidence.rprov");
    fs::write(&combined_path, encode_rprov_unchecked(&manifest, &payloads)).unwrap();
    let combined_report = verify_path(&combined_path, None);
    assert_eq!(
        combined_report.package_structure,
        VerificationStatus::Failed
    );
    assert_eq!(combined_report.event_chain, VerificationStatus::Failed);
    let kinds = review_flags(&combined_report)
        .iter()
        .map(|flag| flag.kind)
        .collect::<Vec<_>>();
    assert!(
        kinds.contains(&ReviewFlagKind::EventChainInvalid),
        "{combined_report:?}"
    );
    assert!(
        kinds.contains(&ReviewFlagKind::UnprovenancedExternalChange),
        "{combined_report:?}"
    );
}

#[test]
fn session_identity_mismatch_is_an_event_chain_failure_not_a_sequence_gap() {
    let fixture = Fixture::new("verify-session-identity");
    let bundle = fixture.bundle("submission.zip");
    let clean_rprov = stored_zip_entry(&fs::read(bundle).unwrap(), "session.rprov");
    let (mut manifest, mut payloads) = collect_rprov(&clean_rprov);
    let event_path = manifest.segments[0].events.entry.clone();
    let mut lines = payloads[&event_path]
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    let changed = lines.get_mut(1).expect("second production event");
    assert_eq!(changed.pop(), Some(b'\n'));
    let DecodeOutcome::Decoded(mut envelope) =
        decode_envelope(changed, DecodePolicy::RejectUnsupported).unwrap()
    else {
        panic!("production event must decode")
    };
    envelope.session_id = SessionId::new("other-session").unwrap();
    *changed = encode_envelope(&envelope).unwrap();
    changed.push(b'\n');
    replace_payload(&mut manifest, &mut payloads, &event_path, lines.concat());
    let path = fixture.base.join("session-identity.rprov");
    fs::write(&path, encode_rprov(&manifest, &payloads)).unwrap();

    let report = verify_path(&path, None);
    assert_eq!(report.event_chain, VerificationStatus::Failed);
    assert_eq!(
        review_flags(&report)
            .iter()
            .map(|flag| flag.kind)
            .collect::<Vec<_>>(),
        vec![ReviewFlagKind::EventChainInvalid]
    );
}

#[test]
fn two_real_flags_are_scanner_identical_and_cycle_through_tui_following() {
    let fixture = Fixture::new("verify-two-flags");
    let mut session = fixture.start_session();
    fs::write(fixture.workspace.join("main.rs"), "outside").unwrap();
    session.save_all().unwrap();
    let receipt = session.finalize("student-1").unwrap();
    let bundle = create_bundle(&receipt, &fixture.base.join("clean.zip"))
        .unwrap()
        .path;
    let clean_archive = fs::read(&bundle).unwrap();
    let clean_rprov = stored_zip_entry(&clean_archive, "session.rprov");
    let (manifest, mut payloads) = collect_rprov(&clean_rprov);
    let evidence_path = manifest.segments[0].evidence[0].entry.clone();
    payloads.get_mut(&evidence_path).unwrap()[0] ^= 1;
    let corrupt_rprov = encode_rprov(&manifest, &payloads);
    assert_eq!(corrupt_rprov.len(), clean_rprov.len());
    let archive = tamper_stored_zip_entry(clean_archive, "session.rprov", &corrupt_rprov);
    let mut source = stored_zip_entry(&archive, "main.rs");
    source[0] ^= 1;
    let archive = tamper_stored_zip_entry(archive, "main.rs", &source);
    let scan_dir = fixture.base.join("scan");
    fs::create_dir(&scan_dir).unwrap();
    let path = scan_dir.join("two-flags.zip");
    fs::write(&path, archive).unwrap();

    let report = verify_path(&path, None);
    let flags = review_flags(&report);
    assert_eq!(
        flags.iter().map(|flag| flag.kind).collect::<Vec<_>>(),
        vec![
            ReviewFlagKind::UnprovenancedExternalChange,
            ReviewFlagKind::SourceMismatch,
        ]
    );
    let mut scan_output = Vec::new();
    run_scan(&[scan_dir.to_string_lossy().into_owned()], &mut scan_output).unwrap();
    let scan_output = String::from_utf8(scan_output).unwrap();
    for flag in &flags {
        assert!(scan_output.contains(&display_flag(flag)), "{scan_output}");
    }

    let mut replay = ReplayController::open(&path).unwrap();
    let first = replay
        .follow_next_review_flag()
        .unwrap()
        .expect("first flag");
    assert_eq!(first.kind, ReviewFlagKind::UnprovenancedExternalChange);
    let (preview_path, _) = replay.artifact_preview().expect("evidence preview");
    assert_eq!(preview_path, evidence_path);

    let second = replay
        .follow_next_review_flag()
        .unwrap()
        .expect("second flag");
    assert_eq!(second.kind, ReviewFlagKind::SourceMismatch);
    assert_eq!(
        replay.selected_event().unwrap().position,
        replay.positions().last().unwrap()
    );
    assert_eq!(
        replay.follow_next_review_flag().unwrap().unwrap().kind,
        ReviewFlagKind::UnprovenancedExternalChange
    );
}

#[test]
fn removed_event_is_a_sequence_gap_and_tampered_final_binding_is_a_replay_mismatch() {
    let fixture = Fixture::new("verify-sequence-replay-flags");
    let mut session = fixture.start_session();
    session.execute(EditorCommand::Insert('B')).unwrap();
    let receipt = session.finalize("student-1").unwrap();
    let bundle = create_bundle(&receipt, &fixture.base.join("submission.zip"))
        .unwrap()
        .path;
    let clean_rprov = stored_zip_entry(&fs::read(bundle).unwrap(), "session.rprov");

    let (mut manifest, mut payloads) = collect_rprov(&clean_rprov);
    let event_path = manifest.segments[0].events.entry.clone();
    let mut lines = payloads[&event_path]
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    assert!(lines.len() > 2);
    lines.remove(1);
    replace_payload(&mut manifest, &mut payloads, &event_path, lines.concat());
    let sequence_path =
        write_mutated_rprov(&fixture.base, "sequence-gap.rprov", &manifest, &payloads);
    let sequence_report = verify_path(&sequence_path, None);
    assert_flag(&sequence_report, ReviewFlagKind::EventSequenceGap);
    assert!(matches!(
        review_flags(&sequence_report)[0].link,
        ReviewFlagLink::Event(_)
    ));

    let (mut manifest, mut payloads) = collect_rprov(&clean_rprov);
    let event_path = manifest.segments[0].events.entry.clone();
    let mut lines = payloads[&event_path]
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    let last = lines.last_mut().unwrap();
    last.pop();
    let DecodeOutcome::Decoded(mut envelope) =
        decode_envelope(last, DecodePolicy::RejectUnsupported).unwrap()
    else {
        panic!("production event must decode")
    };
    let changed_hash = Hash::from_bytes([0xa5; 32]);
    let Event::SubmissionFinalized(finalized) = &mut envelope.event else {
        panic!("production stream must end with finalization")
    };
    finalized.final_workspace_hash = changed_hash;
    envelope.event_hash = compute_event_hash(envelope.previous_event_hash, &envelope).unwrap();
    *last = encode_envelope(&envelope).unwrap();
    last.push(b'\n');
    manifest.segments[0].last_event_hash = envelope.event_hash;
    manifest.segments[0].terminal_event_hash = RprovKnown::Known {
        value: envelope.event_hash,
    };
    manifest.segments[0].final_tree_hash = RprovKnown::Known {
        value: changed_hash,
    };
    manifest.final_tree_hash = RprovKnown::Known {
        value: changed_hash,
    };
    replace_payload(&mut manifest, &mut payloads, &event_path, lines.concat());
    let replay_path = fixture.base.join("replay-mismatch.rprov");
    fs::write(&replay_path, encode_rprov_unchecked(&manifest, &payloads)).unwrap();
    let replay_report = verify_path(&replay_path, None);
    assert_eq!(replay_report.package_structure, VerificationStatus::Failed);
    assert_flag(&replay_report, ReviewFlagKind::ReplayMismatch);
    assert!(matches!(
        review_flags(&replay_report)[0].link,
        ReviewFlagLink::Event(_)
    ));
}

#[test]
fn verify_rejects_manifest_ancestry_and_aggregate_tampering() {
    let fixture = Fixture::new("verify-ancestry-corruption");
    let mut parent = fixture.start_session();
    parent.execute(EditorCommand::Insert('B')).unwrap();
    let parent_receipt = parent.finalize("student-1").unwrap();
    let child_root = fixture.base.join("child");
    fs::create_dir(&child_root).unwrap();
    for (path, bytes) in parent_receipt.final_workspace() {
        fs::write(child_root.join(path.as_str()), bytes).unwrap();
    }
    let child = ProductionSession::start_revision(&fixture.workspace, &child_root, MANIFEST)
        .unwrap()
        .finalize("student-1")
        .unwrap();
    let bundle = create_bundle(&child, &fixture.base.join("revision.zip"))
        .unwrap()
        .path;
    let mut rprov = stored_zip_entry(&fs::read(bundle).unwrap(), "session.rprov");
    let aggregate = child.manifest().aggregate_event_count.to_string();
    let needle = format!("\"aggregate_event_count\":{aggregate}");
    let at = rprov
        .windows(needle.len())
        .position(|window| window == needle.as_bytes())
        .unwrap()
        + needle.len()
        - 1;
    rprov[at] = if rprov[at] == b'9' {
        b'8'
    } else {
        rprov[at] + 1
    };
    let path = fixture.base.join("aggregate-corrupt.rprov");
    fs::write(&path, rprov).unwrap();

    let report = verify_path(&path, None);
    assert_eq!(report.package_structure, VerificationStatus::Failed);
    assert!(!report.is_clean());
}

#[test]
fn clean_report_for_library_has_expected_typed_facts() {
    let fixture = Fixture::new("verify-library-clean");
    let bundle = fixture.bundle("submission.zip");
    let report = verify_path(&bundle, None);
    assert_eq!(report.package_structure, VerificationStatus::Ok);
    assert_eq!(report.event_chain, VerificationStatus::Ok);
    assert_eq!(report.checkpoint_hashes, VerificationStatus::Ok);
    assert_eq!(report.replay, VerificationStatus::Ok);
    assert_eq!(report.submitted_source_match, SubmittedSourceStatus::Ok);
    assert_eq!(
        report.assignment_reference,
        AssignmentReferenceStatus::Unverified
    );
    assert_eq!(report.external_changes, Some(0));
    assert_eq!(report.unknown_edit_origins, Some(0));
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn verify_accepts_matching_rta_reference_and_routes_traversal_to_package_structure() {
    let fixture = Fixture::new("verify-rta-reference");
    let bundle = fixture.bundle("submission.zip");
    let reference = fixture.base.join("assignment.rta");
    fs::write(&reference, assignment_package(MANIFEST, b"A")).unwrap();

    let report = verify_path(&bundle, Some(&reference));
    assert_eq!(report.assignment_reference, AssignmentReferenceStatus::Ok);
    assert!(report.is_clean(), "{report:?}");

    let hostile = fixture.base.join("traversal.zip");
    fs::write(
        &hostile,
        rename_stored_zip_entry(fs::read(bundle).unwrap(), "main.rs", "../x.rs"),
    )
    .unwrap();
    let report = verify_path(&hostile, None);
    assert_eq!(report.package_structure, VerificationStatus::Failed);
    assert!(!report.is_clean());
    assert_flag(&report, ReviewFlagKind::PackageInvalid);
    assert!(matches!(
        review_flags(&report)[0].link,
        ReviewFlagLink::Location(_)
    ));

    let mut replay = ReplayController::open(&hostile).unwrap();
    assert!(!replay.timeline_available());
    let flag = replay.review_flags().remove(0);
    replay.follow_review_flag(&flag).unwrap();
    assert!(replay.flag_location_preview().is_some());
}

#[cfg(unix)]
#[test]
fn matching_rta_reference_works_below_a_symlinked_temporary_root() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("verify-symlinked-temporary-root");
    let bundle = fixture.bundle("submission.zip");
    let reference = fixture.base.join("assignment.rta");
    fs::write(&reference, assignment_package(MANIFEST, b"A")).unwrap();
    let physical_root = fixture.base.join("physical-temporary-root");
    let linked_root = fixture.base.join("linked-temporary-root");
    fs::create_dir(&physical_root).unwrap();
    std::os::unix::fs::symlink(&physical_root, &linked_root).unwrap();
    assert!(
        fs::symlink_metadata(&linked_root)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::canonicalize(&linked_root).unwrap(), physical_root);

    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("verify")
        .arg(&bundle)
        .arg("--reference")
        .arg(&reference)
        .env("TMPDIR", &linked_root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Assignment reference     OK"), "{stdout}");
    assert!(
        stdout.contains("Test-case evidence       recorded (unverified)"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("Assignment reference     unverified"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("Assignment reference unavailable"),
        "{stdout}"
    );
    assert!(!stdout.contains("ancestor is a symlink"), "{stdout}");
}
