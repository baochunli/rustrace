//! T8.6: TA-facing surfaces show the shared evidence-limitations wording
//! verbatim, never overstate evidentiary value, and avoid verdict vocabulary.

#[path = "support/test_home.rs"]
mod test_home;
use rustrace::session::{ProductionSession, create_bundle};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Trust"
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

/// The exact shared sentences from Task 8.6, spelled out literally so this
/// test fails if any surface drifts from the accepted wording.
const CONSISTENT: &str = "This provenance is internally replayable and consistent.";
const NOT_VALIDATED: &str = "This package did not validate.";
const NOT_EVALUATED: &str = "Package checks passed; reference or submitted source not evaluated.";
const LIMITATION: &str = "It does not prove that the client was unmodified or that the recorded \
code originated from the student.";
const FORBIDDEN: [&str; 6] = [
    "misconduct",
    "cheating",
    "plagiarism",
    "ai probability",
    "authorship proof",
    "detected",
];

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    submissions: PathBuf,
}

impl Fixture {
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
        Self {
            root: fs::canonicalize(root).unwrap(),
            workspace: fs::canonicalize(workspace).unwrap(),
            submissions: fs::canonicalize(submissions).unwrap(),
        }
    }

    fn clean_bundle(&self, name: &str) -> PathBuf {
        let receipt = ProductionSession::start(&self.workspace, MANIFEST)
            .unwrap()
            .finalize("student-1")
            .unwrap();
        create_bundle(&receipt, &self.submissions.join(name))
            .unwrap()
            .path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
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

fn rustrace(args: &[&Path]) -> String {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .args(args)
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.stderr.is_empty(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

fn assert_neutral(surface: &str, text: &str) {
    let lower = text.to_ascii_lowercase();
    for forbidden in FORBIDDEN {
        assert!(
            !lower.contains(forbidden),
            "{surface} contains forbidden vocabulary {forbidden:?}:\n{text}"
        );
    }
}

#[test]
fn verify_scan_and_csv_show_exact_evidence_limitations_without_verdict_vocabulary() {
    let fixture = Fixture::new("trust-wording");
    let clean = fixture.clean_bundle("clean.zip");
    let modified = fixture.submissions.join("source-modified.zip");
    fs::write(
        &modified,
        tamper_stored_zip_entry(fs::read(&clean).unwrap(), "main.rs", b"Z"),
    )
    .unwrap();
    fs::write(fixture.submissions.join("not-a-package.zip"), b"notes").unwrap();

    // `verify` states the conditional sentence and the shared limitation.
    let verify_clean = rustrace(&[Path::new("verify"), &clean]);
    assert!(
        verify_clean.contains(&format!("Evidence limitations: {CONSISTENT} {LIMITATION}")),
        "{verify_clean}"
    );
    assert!(!verify_clean.contains(NOT_VALIDATED), "{verify_clean}");
    assert_neutral("verify (clean)", &verify_clean);

    let verify_modified = rustrace(&[Path::new("verify"), &modified]);
    assert!(
        verify_modified.contains(&format!(
            "Evidence limitations: {NOT_VALIDATED} {LIMITATION}"
        )),
        "{verify_modified}"
    );
    assert!(!verify_modified.contains(CONSISTENT), "{verify_modified}");
    assert_neutral("verify (source mismatch)", &verify_modified);

    // A clean package whose TA-side reference cannot be read is neither
    // consistent-and-verified nor failed: it gets the neutral third form.
    let missing_reference = fixture.root.join("missing-reference.rta");
    let verify_unreadable_reference = rustrace(&[
        Path::new("verify"),
        &clean,
        Path::new("--reference"),
        &missing_reference,
    ]);
    assert!(
        verify_unreadable_reference.contains(&format!(
            "Evidence limitations: {NOT_EVALUATED} {LIMITATION}"
        )),
        "{verify_unreadable_reference}"
    );
    assert!(
        verify_unreadable_reference.contains("Package structure        OK"),
        "{verify_unreadable_reference}"
    );
    assert!(
        !verify_unreadable_reference.contains(NOT_VALIDATED),
        "{verify_unreadable_reference}"
    );
    assert!(
        !verify_unreadable_reference.contains(CONSISTENT),
        "{verify_unreadable_reference}"
    );
    assert_neutral(
        "verify (unreadable reference)",
        &verify_unreadable_reference,
    );

    // The scanner header is per batch: it must quote all three row outcomes and
    // the shared limitation without asserting consistency above failed rows.
    let csv = fixture.root.join("review.csv");
    let scan = rustrace(&[
        Path::new("scan"),
        &fixture.submissions,
        Path::new("--output"),
        &csv,
    ]);
    let header = scan
        .lines()
        .find(|line| line.starts_with("Evidence limitations:"))
        .unwrap_or_else(|| panic!("missing scanner evidence-limitations header:\n{scan}"));
    assert!(header.contains(CONSISTENT), "{header}");
    assert!(header.contains(NOT_VALIDATED), "{header}");
    assert!(header.contains(NOT_EVALUATED), "{header}");
    assert!(header.contains(LIMITATION), "{header}");
    assert!(
        !header.starts_with(&format!("Evidence limitations: {CONSISTENT}")),
        "batch header asserts consistency for every row:\n{header}"
    );
    // Each row states its own package outcome with the same three-way rule.
    assert_eq!(
        evidence_after_row(&scan, "clean.zip"),
        format!("  Evidence: {CONSISTENT}"),
        "{scan}"
    );
    assert_eq!(
        evidence_after_row(&scan, "source-modified.zip"),
        format!("  Evidence: {NOT_VALIDATED}"),
        "{scan}"
    );
    assert_eq!(
        evidence_after_row(&scan, "not-a-package.zip"),
        format!("  Evidence: {NOT_VALIDATED}"),
        "{scan}"
    );
    assert_neutral("scan terminal", &scan);

    // An unreadable batch --reference must read the same way as in verify:
    // the reference was not evaluated, so each clean row is partial.
    let scan_unreadable_reference = rustrace(&[
        Path::new("scan"),
        &fixture.submissions,
        Path::new("--reference"),
        &missing_reference,
    ]);
    assert_eq!(
        scan_unreadable_reference
            .matches("Assignment reference unavailable:")
            .count(),
        1,
        "{scan_unreadable_reference}"
    );
    assert_eq!(
        evidence_after_row(&scan_unreadable_reference, "clean.zip"),
        format!("  Evidence: {NOT_EVALUATED}"),
        "{scan_unreadable_reference}"
    );
    assert_eq!(
        evidence_after_row(&scan_unreadable_reference, "source-modified.zip"),
        format!("  Evidence: {NOT_VALIDATED}"),
        "{scan_unreadable_reference}"
    );
    assert_neutral(
        "scan terminal (unreadable reference)",
        &scan_unreadable_reference,
    );

    let csv_text = fs::read_to_string(&csv).unwrap();
    let comment = csv_text.lines().next().unwrap();
    assert!(comment.starts_with("# Priority rule:"), "{comment}");
    assert!(comment.contains(CONSISTENT), "{comment}");
    assert!(comment.contains(NOT_VALIDATED), "{comment}");
    assert!(comment.contains(LIMITATION), "{comment}");
    assert!(
        !comment.contains(&format!("Evidence limitations: {CONSISTENT}")),
        "CSV comment asserts consistency for every row:\n{comment}"
    );
    let header_row = csv_text.lines().nth(1).unwrap();
    assert!(
        header_row.ends_with(",explanation,evidence,advisories"),
        "{header_row}"
    );
    let evidence_column = |file: &str| {
        let row = csv_text
            .lines()
            .find(|line| line.starts_with(file))
            .unwrap_or_else(|| panic!("missing CSV row for {file}: {csv_text}"));
        row.rsplit(',').nth(1).unwrap().to_owned()
    };
    assert_eq!(evidence_column("clean.zip"), CONSISTENT);
    assert_eq!(evidence_column("source-modified.zip"), NOT_VALIDATED);
    assert_eq!(evidence_column("not-a-package.zip"), NOT_VALIDATED);
    assert_neutral("scan CSV", &csv_text);
}

/// The `Evidence:` continuation line printed after a scanner row and its
/// optional `Indicators:` / `Process rule:` / `Advisories:` continuation lines.
fn evidence_after_row(output: &str, file: &str) -> String {
    let mut lines = output.lines();
    lines
        .find(|line| line.contains(file))
        .unwrap_or_else(|| panic!("missing row for {file:?} in {output:?}"));
    lines
        .take_while(|line| line.starts_with("  "))
        .find(|line| line.starts_with("  Evidence: "))
        .unwrap_or_else(|| panic!("missing Evidence line after {file:?} in {output:?}"))
        .to_owned()
}
