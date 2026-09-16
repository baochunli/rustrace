#[path = "support/test_home.rs"]
mod test_home;
use rustrace::{
    cargo_policy::CargoAction,
    process_indicators::{INDICATOR_EXPLANATION, display_process_attempt_details},
    replay_tui::ReplayController,
    review_flags::{AdvisoryFlagKind, EVIDENCE_LIMITATIONS_BATCH},
    session::{ProductionSession, create_bundle},
    tui::EditorCommand,
};
use rustrace_editor::Movement;
use rustrace_workspace::assignment_package::{ExtractionLimits, extract_assignment_package};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Scan"
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
title = "Scan"
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
    root: PathBuf,
    workspace: PathBuf,
    submissions: PathBuf,
}

impl Fixture {
    fn start_session(&self) -> ProductionSession {
        let mut session = ProductionSession::start(&self.workspace, MANIFEST).unwrap();
        session.execute(EditorCommand::NextBuffer).unwrap();
        session
    }

    fn new(prefix: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rustrace-{prefix}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = root.join("workspace");
        let submissions = root.join("submissions");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir(&submissions).unwrap();
        fs::write(workspace.join("main.rs"), "A").unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n",
        )
        .unwrap();
        Self {
            root: fs::canonicalize(root).unwrap(),
            workspace: fs::canonicalize(workspace).unwrap(),
            submissions: fs::canonicalize(submissions).unwrap(),
        }
    }

    fn clean_bundle(&self, name: &str) -> PathBuf {
        let receipt = self.start_session().finalize("student-1").unwrap();
        create_bundle(&receipt, &self.submissions.join(name))
            .unwrap()
            .path
    }

    fn advisory_bundle(&self, name: &str) -> PathBuf {
        fs::write(
            self.workspace.join("main.rs"),
            (0..80).map(|_| "x\n").collect::<String>(),
        )
        .unwrap();
        let mut session = self.start_session();
        session.execute(EditorCommand::SelectAll).unwrap();
        session.execute(EditorCommand::ToggleComment).unwrap();
        let receipt = session.finalize("student-1").unwrap();
        create_bundle(&receipt, &self.submissions.join(name))
            .unwrap()
            .path
    }

    fn bundle_with_identity(&self, name: &str, student_id: &str, assignment_id: &str) -> PathBuf {
        let workspace = self.root.join(format!("workspace-{name}"));
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("main.rs"), "A").unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n",
        )
        .unwrap();
        let manifest = manifest_with_assignment(assignment_id);
        let receipt = ProductionSession::start(&workspace, &manifest)
            .unwrap()
            .finalize(student_id)
            .unwrap();
        create_bundle(&receipt, &self.submissions.join(name))
            .unwrap()
            .path
    }

    fn reference(&self) -> PathBuf {
        let path = self.root.join("assignment.rta");
        fs::write(&path, assignment_package(MANIFEST, b"A")).unwrap();
        path
    }

    fn clean_bundle_with_suite(&self, name: &str, cases: &[(&str, &[u8], &[u8])]) -> PathBuf {
        let package = self.root.join("session-assignment.rta");
        let extraction = self.root.join("session-assignment-extraction");
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
        create_bundle(&receipt, &self.submissions.join(name))
            .unwrap()
            .path
    }
}

fn manifest_with_assignment(assignment_id: &str) -> Vec<u8> {
    String::from_utf8(MANIFEST.to_vec())
        .unwrap()
        .replace(
            "assignment_id = \"assignment\"",
            &format!("assignment_id = \"{assignment_id}\""),
        )
        .into_bytes()
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
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

fn run_scan(directory: &Path, reference: Option<&Path>, csv: &Path) -> std::process::Output {
    let test_home = test_home::TestHome::new(false);
    let mut command = test_home.command(env!("CARGO_BIN_EXE_rustrace"));
    command.arg("scan").arg(directory).arg("--output").arg(csv);
    if let Some(reference) = reference {
        command.arg("--reference").arg(reference);
    }
    command.output().unwrap()
}

fn row<'a>(output: &'a str, file: &str) -> &'a str {
    output
        .lines()
        .find(|line| line.contains(file))
        .unwrap_or_else(|| panic!("missing row for {file:?} in {output:?}"))
}

fn indicators_after_row<'a>(output: &'a str, file: &str) -> &'a str {
    let mut lines = output.lines();
    lines
        .find(|line| line.contains(file))
        .unwrap_or_else(|| panic!("missing row for {file:?} in {output:?}"));
    let test_cases = lines
        .next()
        .unwrap_or_else(|| panic!("missing test-case continuation for {file:?} in {output:?}"));
    assert!(
        test_cases.starts_with("  Test cases: "),
        "expected test-case continuation after {file:?}, got {test_cases:?}"
    );
    let indicators = lines
        .next()
        .unwrap_or_else(|| panic!("missing indicator continuation for {file:?} in {output:?}"));
    assert!(
        indicators.starts_with("  Indicators: "),
        "expected indicator continuation after {file:?}, got {indicators:?}"
    );
    indicators
}

fn run_review_attempt(session: &mut ProductionSession) {
    for _ in 0..40 {
        session.execute(EditorCommand::Insert(' ')).unwrap();
    }
    session
        .execute(EditorCommand::Move {
            movement: Movement::Left,
            selecting: true,
        })
        .unwrap();
    session.execute(EditorCommand::Copy).unwrap();
    session
        .execute(EditorCommand::Move {
            movement: Movement::DocumentEnd,
            selecting: false,
        })
        .unwrap();
    session.execute(EditorCommand::Paste).unwrap();
    for blocked in ["first blocked paste", "second blocked paste"] {
        assert!(
            session
                .execute(EditorCommand::PasteExternal(blocked.to_owned()))
                .is_err()
        );
    }
    session.save_all().unwrap();
    for _ in 0..2 {
        session.start_command(CargoAction::Check).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while session.command_active() && Instant::now() < deadline {
            session.poll_command().unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!session.command_active(), "Cargo Check fixture timed out");
        assert!(session.command_outcome().is_some());
    }
}

fn copy_final_workspace(receipt: &rustrace::session::FinalizationReceipt, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for (path, bytes) in receipt.final_workspace() {
        let target = destination.join(path.as_str());
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(target, bytes).unwrap();
    }
}

#[test]
fn non_self_contained_assignment_references_report_the_structure_error_once() {
    let fixture = Fixture::new("scan-starter-structure");
    fixture.clean_bundle("clean.zip");
    let reference = fixture.root.join("invalid-starter.rta");
    let csv = fixture.root.join("structure.csv");
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
            let output = run_scan(&fixture.submissions, Some(&reference), &csv);
            assert!(output.status.success());
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert_eq!(
                stdout.matches("Assignment reference unavailable:").count(),
                1,
                "{stdout}"
            );
            assert!(
                stdout.contains("assignment starter must be a self-contained package:"),
                "{stdout}"
            );
            assert!(row(&stdout, "clean.zip").contains("unverified"), "{stdout}");
        }
    }
}

#[test]
fn scan_processes_each_candidate_isolates_failures_and_writes_matching_csv() {
    let fixture = Fixture::new("scan-cli-batch");
    let clean = fixture.clean_bundle("clean,submission.zip");
    let clean_bytes = fs::read(&clean).unwrap();
    fs::write(
        fixture.submissions.join("source-modified.zip"),
        tamper_stored_zip_entry(clean_bytes.clone(), "main.rs", b"Z"),
    )
    .unwrap();
    fs::write(fixture.submissions.join("corrupted.rprov"), b"RUST").unwrap();
    fs::write(
        fixture.submissions.join("hostile-traversal.zip"),
        rename_stored_zip_entry(clean_bytes, "main.rs", "../x.rs"),
    )
    .unwrap();
    fs::write(
        fixture.submissions.join("not-a-package.zip"),
        b"ordinary notes",
    )
    .unwrap();
    fs::write(fixture.submissions.join("ignored.txt"), b"not a candidate").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&clean, fixture.submissions.join("linked\nbundle.zip")).unwrap();
    let csv = fixture.root.join("review.csv");

    let result = run_scan(&fixture.submissions, Some(&fixture.reference()), &csv);
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let stdout = String::from_utf8(result.stdout).unwrap();
    let header = stdout
        .lines()
        .find(|line| line.starts_with("File "))
        .unwrap();
    for label in [
        "Runs",
        "Passes",
        "Mismatches",
        "Errors",
        "Case evidence",
        "First case",
        "Line",
    ] {
        assert!(
            header.contains(label),
            "missing terminal column {label}: {header}"
        );
    }
    assert_eq!(stdout.matches("Priority rule:").count(), 1, "{stdout}");
    assert!(stdout.contains("factual counts and sizes do not change priority"));
    assert!(stdout.contains(EVIDENCE_LIMITATIONS_BATCH), "{stdout}");
    assert!(row(&stdout, "clean,submission.zip").contains("student-1"));
    assert!(row(&stdout, "clean,submission.zip").contains("assignment"));
    assert!(row(&stdout, "clean,submission.zip").contains("OK"));
    assert!(row(&stdout, "clean,submission.zip").contains("Match"));
    assert!(row(&stdout, "clean,submission.zip").contains("Normal"));
    assert!(stdout.contains(
        "Test cases: runs=0 passes=0 mismatches=0 errors=0 evidence=recorded (unverified) first=none"
    ));
    assert!(row(&stdout, "source-modified.zip").contains("No"));
    assert!(row(&stdout, "source-modified.zip").contains("High"));
    assert!(row(&stdout, "source-modified.zip").contains("SOURCE_MISMATCH"));
    assert!(
        row(&stdout, "source-modified.zip").contains("SOURCE_MISMATCH [segment:1 seq:4]"),
        "{stdout}"
    );
    for name in [
        "corrupted.rprov",
        "hostile-traversal.zip",
        "not-a-package.zip",
    ] {
        assert!(row(&stdout, name).contains("Fail"), "{name}: {stdout}");
        assert!(row(&stdout, name).contains("High"), "{name}: {stdout}");
        assert!(
            row(&stdout, name).contains("PACKAGE_INVALID"),
            "{name}: {stdout}"
        );
    }
    #[cfg(unix)]
    {
        assert!(stdout.contains("linked\\nbundle.zip"), "{stdout}");
        assert!(!stdout.contains("linked\nbundle.zip"), "{stdout}");
        assert!(row(&stdout, "linked\\nbundle.zip").contains("not followed"));
    }
    assert!(!stdout.contains("ignored.txt"));
    for prohibited in ["cheating", "misconduct detected", "combined process"] {
        assert!(
            !stdout.to_ascii_lowercase().contains(prohibited),
            "{stdout}"
        );
    }

    let csv_text = fs::read_to_string(csv).unwrap();
    assert!(csv_text.starts_with("# Priority rule:"), "{csv_text}");
    assert!(csv_text.contains(
        "file_name,student_id,assignment_id,verify_result,submitted_source_result,reference_result,priority,test_case_runs,test_case_passes,test_case_mismatches,test_case_errors,test_case_evidence,first_failing_case,first_failing_line,review_flags,indicators,process_rule,explanation,evidence,advisories\r\n"
    ));
    for name in [
        "clean,submission.zip",
        "source-modified.zip",
        "corrupted.rprov",
        "hostile-traversal.zip",
        "not-a-package.zip",
    ] {
        assert_eq!(csv_text.matches(name).count(), 1, "{name}: {csv_text}");
    }
    assert!(csv_text.contains("\"clean,submission.zip\""), "{csv_text}");
    #[cfg(unix)]
    assert_eq!(
        csv_text.matches("linked\\nbundle.zip").count(),
        1,
        "{csv_text}"
    );
    assert!(!csv_text.contains("ignored.txt"));
}

#[test]
fn scan_advisory_golden_appends_csv_column_without_changing_priority_or_hard_flags() {
    let fixture = Fixture::new("scan-advisory");
    let bundle = fixture.advisory_bundle("advisory.zip");
    let report = rustrace::verify::verify_path(&bundle, None);
    assert_eq!(report.advisories.len(), 1, "{:#?}", report.advisories);
    assert_eq!(
        report.advisories[0].kind,
        AdvisoryFlagKind::LargeSingleInsertion
    );
    assert!(report.is_clean(), "{report:#?}");

    let csv = fixture.root.join("advisory.csv");
    let output = run_scan(&fixture.submissions, None, &csv);
    assert!(output.status.success());
    let terminal = String::from_utf8(output.stdout).unwrap();
    assert!(row(&terminal, "advisory.zip").contains("Normal"));
    assert!(row(&terminal, "advisory.zip").contains("No review flags"));
    assert!(
        terminal.contains("  Advisories: LARGE_SINGLE_INSERTION: 1"),
        "{terminal}"
    );

    let csv = fs::read_to_string(csv).unwrap();
    assert!(csv.contains(
        "file_name,student_id,assignment_id,verify_result,submitted_source_result,reference_result,priority,test_case_runs,test_case_passes,test_case_mismatches,test_case_errors,test_case_evidence,first_failing_case,first_failing_line,review_flags,indicators,process_rule,explanation,evidence,advisories\r\n"
    ));
    let row = csv
        .lines()
        .find(|line| line.starts_with("advisory.zip,"))
        .unwrap();
    assert!(row.ends_with(",LARGE_SINGLE_INSERTION: 1"), "{row}");
}

#[test]
fn large_factual_counts_stay_normal_and_unavailable_counts_are_unknown() {
    let fixture = Fixture::new("scan-cli-counts");
    let mut session = fixture.start_session();
    for index in 0..32 {
        fs::write(
            fixture.workspace.join("main.rs"),
            format!("external-{index}"),
        )
        .unwrap();
        session.save_all().unwrap();
    }
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Copy).unwrap();
    session
        .execute(EditorCommand::Move {
            movement: rustrace_editor::Movement::DocumentEnd,
            selecting: false,
        })
        .unwrap();
    session.execute(EditorCommand::Paste).unwrap();
    assert!(
        session
            .execute(EditorCommand::PasteExternal("not retained".to_owned()))
            .is_err()
    );
    let receipt = session.finalize("student-1").unwrap();
    create_bundle(&receipt, &fixture.submissions.join("facts.zip")).unwrap();
    fs::write(fixture.submissions.join("unknown.rprov"), b"RUST").unwrap();
    let csv = fixture.root.join("facts.csv");

    let result = run_scan(&fixture.submissions, None, &csv);
    assert!(result.status.success());
    let stdout = String::from_utf8(result.stdout).unwrap();
    let facts = row(&stdout, "facts.zip");
    assert!(facts.contains("Normal"), "{facts}");
    assert!(facts.contains("see Indicators below"), "{facts}");
    assert!(facts.len() < 1_024, "{facts}");
    let indicators = indicators_after_row(&stdout, "facts.zip");
    assert!(
        indicators.contains("external-change observations: 32"),
        "{indicators}"
    );
    assert!(
        indicators.contains("allowed internal paste: 1 / 1 chars"),
        "{indicators}"
    );
    assert!(
        indicators.contains("rejected paste attempts: 1"),
        "{indicators}"
    );
    assert!(indicators.contains("segment:1 seq:"), "{indicators}");
    assert!(
        stdout.contains("process-review-v1 attempt 1: not eligible:"),
        "{stdout}"
    );
    assert_eq!(stdout.matches(INDICATOR_EXPLANATION).count(), 1, "{stdout}");
    let unavailable = row(&stdout, "unknown.rprov");
    assert!(unavailable.contains("unknown"), "{unavailable}");
    assert!(unavailable.contains("High"), "{unavailable}");
    let csv_text = fs::read_to_string(csv).unwrap();
    assert!(
        csv_text.contains("external-change observations: 32"),
        "{csv_text}"
    );
    assert!(
        csv_text.contains("allowed internal paste: 1 / 1 chars"),
        "{csv_text}"
    );
    assert!(
        csv_text.contains("rejected paste attempts: 1"),
        "{csv_text}"
    );
    assert!(
        csv_text.contains("repeated rejected external-change attempts: 32"),
        "{csv_text}"
    );
    assert!(
        csv_text.contains("process-review-v1 attempt 1: not eligible:"),
        "{csv_text}"
    );
    assert!(
        csv_text
            .contains("Unknown means a missing origin explanation, not established external input"),
        "{csv_text}"
    );
    assert!(
        csv_text.contains("Internal paste is allowed and not inherently suspicious"),
        "{csv_text}"
    );
    assert!(
        csv_text.contains("segment:1 session:session-"),
        "{csv_text}"
    );
    assert!(csv_text.contains(" seq:"), "{csv_text}");
    assert!(
        csv_text.contains("process-review-v1 attempt 1: not eligible:"),
        "{csv_text}"
    );
    assert!(
        csv_text.contains("review indicators unavailable: factual counts unknown"),
        "{csv_text}"
    );
}

#[test]
fn three_attempt_production_scan_retains_every_process_outcome_and_explanation() {
    let fixture = Fixture::new("scan-cli-three-attempt-review");
    fs::remove_file(fixture.workspace.join("main.rs")).unwrap();
    fs::write(
        fixture.workspace.join("0.rs"),
        "pub fn recorded_value() -> usize { 1 }\n",
    )
    .unwrap();
    fs::write(
        fixture.workspace.join("Cargo.toml"),
        "[package]\nname = \"scan-review-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[lib]\npath = \"0.rs\"\n[workspace]\n",
    )
    .unwrap();
    fs::write(
        fixture.workspace.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"scan-review-fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    let mut first = fixture.start_session();
    run_review_attempt(&mut first);
    let first_receipt = first.finalize("student-1").unwrap();

    let second_root = fixture.root.join("revision-2");
    copy_final_workspace(&first_receipt, &second_root);
    let mut second =
        ProductionSession::start_revision(&fixture.workspace, &second_root, MANIFEST).unwrap();
    run_review_attempt(&mut second);
    let second_receipt = second.finalize("student-1").unwrap();

    let third_root = fixture.root.join("revision-3");
    copy_final_workspace(&second_receipt, &third_root);
    let mut third = ProductionSession::start_revision(&second_root, &third_root, MANIFEST).unwrap();
    run_review_attempt(&mut third);
    let third_receipt = third.finalize("student-1").unwrap();
    let bundle = create_bundle(
        &third_receipt,
        &fixture.submissions.join("three-attempts.zip"),
    )
    .unwrap()
    .path;

    let replay = ReplayController::open(&bundle).unwrap();
    let attempts = &replay
        .verification_report()
        .review_indicators
        .as_ref()
        .expect("production indicators")
        .attempts;
    assert_eq!(attempts.len(), 3);
    assert!(
        attempts
            .iter()
            .all(|attempt| attempt.values.complete_check_test_commands == 2)
    );

    let csv = fixture.root.join("three-attempts.csv");
    let result = run_scan(&fixture.submissions, None, &csv);
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let stdout = String::from_utf8(result.stdout).unwrap();
    let terminal_row = row(&stdout, "three-attempts.zip");
    assert!(terminal_row.len() < 1_024, "terminal row is not compact");
    assert!(!terminal_row.contains("session="), "{terminal_row}");
    for attempt in attempts {
        assert!(
            stdout.contains(&format!("attempt {}: not eligible:", attempt.attempt)),
            "{stdout}"
        );
    }
    assert_eq!(stdout.matches(INDICATOR_EXPLANATION).count(), 1, "{stdout}");

    let csv_text = fs::read_to_string(csv).unwrap();
    assert!(csv_text.contains(
        "priority,test_case_runs,test_case_passes,test_case_mismatches,test_case_errors,test_case_evidence,first_failing_case,first_failing_line,review_flags,indicators,process_rule,explanation,evidence,advisories\r\n"
    ));
    assert_eq!(
        csv_text.matches("process-review-v1 attempt ").count(),
        3,
        "{csv_text}"
    );
    for attempt in attempts {
        let replay_details = display_process_attempt_details(attempt);
        assert!(
            csv_text.contains(&replay_details),
            "scan CSV disagrees with replay attempt {}: {csv_text}",
            attempt.attempt
        );
    }
    assert!(csv_text.contains(INDICATOR_EXPLANATION), "{csv_text}");
    assert!(!csv_text.contains("[display truncated]"), "{csv_text}");
}

#[test]
fn scan_reports_operator_errors_but_submission_failures_do_not_change_exit_status() {
    let fixture = Fixture::new("scan-cli-errors");
    fs::write(fixture.submissions.join("bad.zip"), b"not a package").unwrap();
    let csv = fixture.root.join("review.csv");
    assert!(run_scan(&fixture.submissions, None, &csv).status.success());

    let missing = fixture.root.join("missing");
    let output = run_scan(&missing, None, &fixture.root.join("missing.csv"));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("scan stopped:")
    );

    let output = run_scan(
        &fixture.submissions,
        None,
        &fixture.root.join("missing-parent/review.csv"),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("scan stopped:")
    );
}

#[test]
fn terminal_rows_preserve_distinct_long_manifest_identities() {
    let fixture = Fixture::new("scan-cli-long-identities");
    let assignment_id = "assignment-12345678901234";
    let student_a = "student-a-12345678901234567";
    let student_b = "student-b-12345678901234567";
    assert_eq!(assignment_id.len(), 25);
    assert_eq!(student_a.len(), 27);
    assert_eq!(student_b.len(), 27);
    fixture.bundle_with_identity("alpha.zip", student_a, assignment_id);
    fixture.bundle_with_identity("bravo.zip", student_b, assignment_id);
    let csv = fixture.root.join("identities.csv");

    let result = run_scan(&fixture.submissions, None, &csv);
    assert!(result.status.success());
    let stdout = String::from_utf8(result.stdout).unwrap();
    let alpha = row(&stdout, "alpha.zip");
    let bravo = row(&stdout, "bravo.zip");
    assert!(alpha.contains(student_a), "{alpha}");
    assert!(bravo.contains(student_b), "{bravo}");
    assert!(alpha.contains(assignment_id), "{alpha}");
    assert!(bravo.contains(assignment_id), "{bravo}");
    assert_ne!(alpha, bravo);
    assert!(!alpha.contains("[display truncated]"), "{alpha}");
    assert!(!bravo.contains("[display truncated]"), "{bravo}");

    let csv_text = fs::read_to_string(csv).unwrap();
    assert!(csv_text.contains(student_a), "{csv_text}");
    assert!(csv_text.contains(student_b), "{csv_text}");
    assert_eq!(csv_text.matches(assignment_id).count(), 2, "{csv_text}");
}

#[test]
fn reference_only_mismatch_is_high_priority_context_but_not_a_hard_flag() {
    let fixture = Fixture::new("scan-cli-reference-mismatch");
    fixture.clean_bundle("clean.zip");
    let reference = fixture.root.join("other-assignment.rta");
    fs::write(
        &reference,
        assignment_package(&manifest_with_assignment("other-assignment"), b"A"),
    )
    .unwrap();
    let csv = fixture.root.join("mismatch.csv");

    let result = run_scan(&fixture.submissions, Some(&reference), &csv);
    assert!(result.status.success());
    let stdout = String::from_utf8(result.stdout).unwrap();
    let submission = row(&stdout, "clean.zip");
    assert!(submission.contains("OK"), "{submission}");
    assert!(submission.contains("Mismatch"), "{submission}");
    assert!(submission.contains("High"), "{submission}");
    assert!(
        submission.contains("assignment-reference mismatch"),
        "{submission}"
    );
    assert!(!submission.contains("REFERENCE_MISMATCH"), "{submission}");
    assert!(!submission.contains("PACKAGE_INVALID"), "{submission}");
    assert!(stdout.contains("assignment-reference mismatch"), "{stdout}");
}

#[test]
fn scan_reports_packaged_test_case_suite_mismatch() {
    let fixture = Fixture::new("scan-test-case-suite-mismatch");
    let cases = [("alpha", b"1\n".as_slice(), b"2\n".as_slice())];
    fixture.clean_bundle_with_suite("clean.zip", &cases);
    let reference = fixture.root.join("changed.rta");
    let changed_cases = [("alpha", b"1\n".as_slice(), b"3\n".as_slice())];
    fs::write(
        &reference,
        assignment_package_with_cases(MANIFEST_V2, b"A", &changed_cases),
    )
    .unwrap();
    let csv = fixture.root.join("mismatch.csv");

    let result = run_scan(&fixture.submissions, Some(&reference), &csv);
    assert!(result.status.success());
    let stdout = String::from_utf8(result.stdout).unwrap();
    assert!(row(&stdout, "clean.zip").contains("Mismatch"), "{stdout}");
    assert!(
        stdout.contains("test-case suite identity mismatch"),
        "{stdout}"
    );
}

#[test]
fn candidate_suffix_matching_is_ascii_case_insensitive() {
    let fixture = Fixture::new("scan-cli-uppercase");
    fixture.clean_bundle("UPPER.ZIP");
    let csv = fixture.root.join("uppercase.csv");

    let result = run_scan(&fixture.submissions, None, &csv);
    assert!(result.status.success());
    let stdout = String::from_utf8(result.stdout).unwrap();
    assert!(row(&stdout, "UPPER.ZIP").contains("student-1"));
    assert!(fs::read_to_string(csv).unwrap().contains("UPPER.ZIP"));
}

#[test]
fn unusable_reference_is_reported_once_before_unverified_rows() {
    let fixture = Fixture::new("scan-cli-unusable-reference");
    fixture.clean_bundle("clean.zip");
    let reference = fixture.root.join("bad\nreference.rta");
    fs::write(&reference, b"not an assignment package").unwrap();
    let csv = fixture.root.join("unusable-reference.csv");

    let result = run_scan(&fixture.submissions, Some(&reference), &csv);
    assert!(result.status.success());
    let stdout = String::from_utf8(result.stdout).unwrap();
    let diagnostic = "Assignment reference unavailable:";
    assert_eq!(stdout.matches(diagnostic).count(), 1, "{stdout}");
    assert!(stdout.contains("bad\\nreference.rta"), "{stdout}");
    assert!(
        stdout.find(diagnostic).unwrap() < stdout.find("File").unwrap(),
        "{stdout}"
    );
    assert!(row(&stdout, "clean.zip").contains("Unverified"));
}
