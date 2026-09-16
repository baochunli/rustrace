use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustrace_workspace::assignment_package::{
    AssignmentPackageError, ExtractionLimits, MAX_TEST_CASE_FILE_BYTES, MAX_TEST_CASE_TOTAL_BYTES,
    MAX_TEST_CASES, extract_assignment_package,
};

const BLOCK_SIZE: usize = 512;
const MANIFEST_V1: &str = r#"format_version = 1
course_id = "ECE1724"
assignment_id = "a3"
assignment_version = "2026-09-01"
title = "Ownership and Graph Traversal"
toolchain = "1.92.0"
edition = "2024"
allowed_paths = ["src/**/*.rs", "Cargo.toml"]

[commands]
check = ["cargo", "check", "--locked"]
test = ["cargo", "test", "--locked"]
run = ["cargo", "run", "--locked"]
clippy = ["cargo", "clippy", "--locked", "--", "-D", "warnings"]
format = ["cargo", "fmt"]
"#;

#[derive(Clone, Copy)]
enum Kind {
    File,
    Directory,
    Symlink,
}

struct Entry<'a> {
    path: &'a str,
    kind: Kind,
    contents: &'a [u8],
}

impl<'a> Entry<'a> {
    fn file(path: &'a str, contents: &'a [u8]) -> Self {
        Self {
            path,
            kind: Kind::File,
            contents,
        }
    }

    fn directory(path: &'a str) -> Self {
        Self {
            path,
            kind: Kind::Directory,
            contents: &[],
        }
    }

    fn symlink(path: &'a str) -> Self {
        Self {
            path,
            kind: Kind::Symlink,
            contents: &[],
        }
    }
}

fn manifest_v2() -> String {
    MANIFEST_V1.replacen("format_version = 1", "format_version = 2", 1)
}

fn base_entries<'a>(manifest: &'a str) -> Vec<Entry<'a>> {
    vec![
        Entry::file("assignment.toml", manifest.as_bytes()),
        Entry::file("starter/Cargo.toml", b"[package]\n[workspace]\n"),
        Entry::file("starter/src/main.rs", b"fn main() {}\n"),
    ]
}

#[test]
fn v2_returns_sorted_complete_cases_without_extracting_them_into_the_workspace() {
    let manifest = manifest_v2();
    let mut entries = base_entries(&manifest);
    entries.extend([
        Entry::directory("test-cases/"),
        Entry::file("test-cases/z-last.expected", b"z-out\n"),
        Entry::file("test-cases/a_first.in", b"a-in\n"),
        Entry::file("test-cases/z-last.in", b"z-in\n"),
        Entry::file("test-cases/a_first.expected", b"a-out\n"),
    ]);
    let root = TempRoot::new();
    let destination = root.path().join("workspace");

    let extracted = extract_assignment_package(
        Cursor::new(package(&entries)),
        &destination,
        ExtractionLimits::default(),
    )
    .expect("valid v2 package");

    let suite = extracted.test_cases.expect("v2 suite");
    assert_eq!(suite.cases.len(), 2);
    assert_eq!(suite.cases[0].name, "a_first");
    assert_eq!(suite.cases[0].input, b"a-in\n");
    assert_eq!(suite.cases[0].expected, b"a-out\n");
    assert_eq!(suite.cases[1].name, "z-last");
    assert_eq!(suite.total_bytes, 22);
    assert!(!destination.join("test-cases").exists());
    assert_eq!(
        fs::read(destination.join("src/main.rs")).unwrap(),
        b"fn main() {}\n"
    );
}

#[test]
fn v1_still_rejects_every_test_case_entry() {
    for entry in [
        Entry::directory("test-cases/"),
        Entry::file("test-cases/sample.in", b"input"),
    ] {
        let mut entries = base_entries(MANIFEST_V1);
        entries.push(entry);
        let root = TempRoot::new();
        let destination = root.path().join("workspace");

        let error = extract_assignment_package(
            Cursor::new(package(&entries)),
            &destination,
            ExtractionLimits::default(),
        )
        .expect_err("v1 test-cases entry");

        assert!(matches!(
            error,
            AssignmentPackageError::UnexpectedEntry { .. }
        ));
        assert!(!destination.exists());
    }
}

#[test]
fn v2_requires_a_nonempty_complete_suite() {
    let manifest = manifest_v2();
    for (label, case_entries, expected) in [
        ("empty", vec![Entry::directory("test-cases/")], "required"),
        (
            "missing expected",
            vec![Entry::file("test-cases/only.in", b"input")],
            "missing `.expected`",
        ),
        (
            "missing input",
            vec![Entry::file("test-cases/only.expected", b"output")],
            "missing `.in`",
        ),
    ] {
        let mut entries = base_entries(&manifest);
        entries.extend(case_entries);
        let root = TempRoot::new();

        let error = extract_assignment_package(
            Cursor::new(package(&entries)),
            &root.path().join(label),
            ExtractionLimits::default(),
        )
        .expect_err(label);

        assert!(matches!(
            error,
            AssignmentPackageError::MissingTestCases
                | AssignmentPackageError::IncompleteTestCase { .. }
        ));
        assert!(error.to_string().contains(expected));
    }
}

#[test]
fn v2_rejects_noncanonical_nested_extra_duplicate_and_link_entries() {
    let manifest = manifest_v2();
    let long_name = "x".repeat(65);
    let invalid_paths = [
        ".in".to_owned(),
        "has.dot.in".to_owned(),
        "café.in".to_owned(),
        format!("{long_name}.in"),
        "nested/case.in".to_owned(),
        "README".to_owned(),
    ];
    for invalid in invalid_paths {
        let path = format!("test-cases/{invalid}");
        let mut entries = base_entries(&manifest);
        entries.push(Entry::file(&path, b"bytes"));
        let root = TempRoot::new();

        let error = extract_assignment_package(
            Cursor::new(package(&entries)),
            &root.path().join("workspace"),
            ExtractionLimits::default(),
        )
        .expect_err(&path);

        assert!(matches!(
            error,
            AssignmentPackageError::InvalidTestCasePath { .. }
        ));
    }

    let nested = [
        Entry::file("assignment.toml", manifest.as_bytes()),
        Entry::file("starter/Cargo.toml", b"[package]\n[workspace]\n"),
        Entry::file("starter/src/main.rs", b"fn main() {}\n"),
        Entry::directory("test-cases/nested/"),
    ];
    let root = TempRoot::new();
    let error = extract_assignment_package(
        Cursor::new(package(&nested)),
        &root.path().join("nested"),
        ExtractionLimits::default(),
    )
    .expect_err("nested case directory");
    assert!(matches!(
        error,
        AssignmentPackageError::InvalidTestCasePath { .. }
    ));

    let duplicate = [
        Entry::file("assignment.toml", manifest.as_bytes()),
        Entry::file("starter/Cargo.toml", b"[package]\n[workspace]\n"),
        Entry::file("starter/src/main.rs", b"fn main() {}\n"),
        Entry::file("test-cases/repeated.in", b"one"),
        Entry::file("test-cases/repeated.in", b"two"),
    ];
    let root = TempRoot::new();
    let error = extract_assignment_package(
        Cursor::new(package(&duplicate)),
        &root.path().join("duplicate"),
        ExtractionLimits::default(),
    )
    .expect_err("duplicate case base and suffix");
    assert!(matches!(
        error,
        AssignmentPackageError::DuplicatePath { .. }
    ));

    let linked = [
        Entry::file("assignment.toml", manifest.as_bytes()),
        Entry::file("starter/Cargo.toml", b"[package]\n[workspace]\n"),
        Entry::file("starter/src/main.rs", b"fn main() {}\n"),
        Entry::symlink("test-cases/link.in"),
    ];
    let root = TempRoot::new();
    let error = extract_assignment_package(
        Cursor::new(package(&linked)),
        &root.path().join("linked"),
        ExtractionLimits::default(),
    )
    .expect_err("case symlink");
    assert!(matches!(
        error,
        AssignmentPackageError::UnsupportedEntryType { .. }
    ));
}

#[test]
fn v2_rejects_case_path_traversal_before_publication() {
    let manifest = manifest_v2();
    let entries = [
        Entry::file("assignment.toml", manifest.as_bytes()),
        Entry::file("starter/Cargo.toml", b"[package]\n[workspace]\n"),
        Entry::file("starter/src/main.rs", b"fn main() {}\n"),
        Entry::file("test-cases/../escaped.in", b"escape"),
    ];
    let root = TempRoot::new();
    let destination = root.path().join("workspace");

    let error = extract_assignment_package(
        Cursor::new(package(&entries)),
        &destination,
        ExtractionLimits::default(),
    )
    .expect_err("case traversal");

    assert!(matches!(error, AssignmentPackageError::UnsafePath { .. }));
    assert!(!destination.exists());
    assert!(!root.path().join("escaped.in").exists());
}

#[test]
fn v2_enforces_the_case_file_size_limit_at_limit_plus_one() {
    let manifest = manifest_v2();
    let maximum = vec![b'x'; MAX_TEST_CASE_FILE_BYTES as usize];
    let mut exact_entries = base_entries(&manifest);
    exact_entries.extend([
        Entry::file("test-cases/maximum.in", &maximum),
        Entry::file("test-cases/maximum.expected", b""),
    ]);
    let root = TempRoot::new();
    let extracted = extract_assignment_package(
        Cursor::new(package(&exact_entries)),
        &root.path().join("exact-file"),
        ExtractionLimits::default(),
    )
    .expect("exact case file size");
    assert_eq!(
        extracted.test_cases.unwrap().total_bytes,
        MAX_TEST_CASE_FILE_BYTES
    );

    let oversized = vec![b'x'; MAX_TEST_CASE_FILE_BYTES as usize + 1];
    let mut oversized_entries = base_entries(&manifest);
    oversized_entries.extend([
        Entry::file("test-cases/oversized.in", &oversized),
        Entry::file("test-cases/oversized.expected", b""),
    ]);
    let error = extract_assignment_package(
        Cursor::new(package(&oversized_entries)),
        &root.path().join("oversized-file"),
        ExtractionLimits::default(),
    )
    .expect_err("case file size limit plus one");
    assert!(matches!(
        error,
        AssignmentPackageError::TestCaseFileSizeLimitExceeded { .. }
    ));
}

#[test]
fn v2_enforces_the_case_count_limit_at_limit_plus_one() {
    let manifest = manifest_v2();
    let exact_paths = (0..MAX_TEST_CASES)
        .map(|index| {
            (
                format!("test-cases/case-{index}.in"),
                format!("test-cases/case-{index}.expected"),
            )
        })
        .collect::<Vec<_>>();
    let mut exact_entries = base_entries(&manifest);
    for (input, expected) in &exact_paths {
        exact_entries.push(Entry::file(input, b""));
        exact_entries.push(Entry::file(expected, b""));
    }
    let root = TempRoot::new();
    let extracted = extract_assignment_package(
        Cursor::new(package(&exact_entries)),
        &root.path().join("exact-count"),
        ExtractionLimits::default(),
    )
    .expect("exact case count");
    assert_eq!(extracted.test_cases.unwrap().cases.len(), MAX_TEST_CASES);

    let extra_input = format!("test-cases/case-{MAX_TEST_CASES}.in");
    let extra_expected = format!("test-cases/case-{MAX_TEST_CASES}.expected");
    exact_entries.push(Entry::file(&extra_input, b""));
    exact_entries.push(Entry::file(&extra_expected, b""));
    let error = extract_assignment_package(
        Cursor::new(package(&exact_entries)),
        &root.path().join("too-many-cases"),
        ExtractionLimits::default(),
    )
    .expect_err("case count limit plus one");
    assert!(matches!(
        error,
        AssignmentPackageError::TestCaseCountLimitExceeded { .. }
    ));
}

#[test]
fn v2_enforces_the_case_total_limit_at_limit_plus_one() {
    let manifest = manifest_v2();
    let chunk = vec![b'x'; MAX_TEST_CASE_FILE_BYTES as usize];
    let chunk_count = MAX_TEST_CASE_TOTAL_BYTES / MAX_TEST_CASE_FILE_BYTES;
    let paths = (0..chunk_count)
        .map(|index| format!("test-cases/total-{index}.in"))
        .collect::<Vec<_>>();
    let expected_paths = (0..chunk_count)
        .map(|index| format!("test-cases/total-{index}.expected"))
        .collect::<Vec<_>>();
    let mut exact_entries = base_entries(&manifest);
    for (input, expected) in paths.iter().zip(&expected_paths) {
        exact_entries.push(Entry::file(input, &chunk));
        exact_entries.push(Entry::file(expected, b""));
    }
    let root = TempRoot::new();
    let extracted = extract_assignment_package(
        Cursor::new(package(&exact_entries)),
        &root.path().join("exact-total"),
        ExtractionLimits::default(),
    )
    .expect("exact case total");
    assert_eq!(
        extracted.test_cases.unwrap().total_bytes,
        MAX_TEST_CASE_TOTAL_BYTES
    );

    exact_entries.push(Entry::file("test-cases/one-more.in", b"x"));
    exact_entries.push(Entry::file("test-cases/one-more.expected", b""));
    let error = extract_assignment_package(
        Cursor::new(package(&exact_entries)),
        &root.path().join("oversized-total"),
        ExtractionLimits::default(),
    )
    .expect_err("case total limit plus one");
    assert!(matches!(
        error,
        AssignmentPackageError::TestCaseTotalSizeLimitExceeded { .. }
    ));
}

#[test]
fn suite_hash_is_order_independent_and_changes_for_input_or_expected_bytes() {
    let manifest = manifest_v2();
    let archive = |input: &'static [u8], expected: &'static [u8], reverse: bool| {
        let mut entries = base_entries(&manifest);
        let mut cases = vec![
            Entry::file("test-cases/a.in", input),
            Entry::file("test-cases/a.expected", expected),
            Entry::file("test-cases/z.in", b"z-in"),
            Entry::file("test-cases/z.expected", b"z-out"),
        ];
        if reverse {
            cases.reverse();
        }
        entries.extend(cases);
        package(&entries)
    };
    let root = TempRoot::new();
    let hash = |bytes, name: &str| {
        extract_assignment_package(
            Cursor::new(bytes),
            &root.path().join(name),
            ExtractionLimits::default(),
        )
        .unwrap()
        .test_cases
        .unwrap()
        .hash
    };

    let baseline = hash(archive(b"input", b"expected", false), "baseline");
    assert_eq!(
        baseline,
        hash(archive(b"input", b"expected", true), "reordered")
    );
    assert_ne!(
        baseline,
        hash(archive(b"inpuT", b"expected", false), "input-change")
    );
    assert_ne!(
        baseline,
        hash(archive(b"input", b"expecteD", false), "expected-change")
    );
}

fn package(entries: &[Entry<'_>]) -> Vec<u8> {
    let mut archive = Vec::new();
    for entry in entries {
        let mut header = [0_u8; BLOCK_SIZE];
        write_ustar_path(&mut header, entry.path);
        write_octal(&mut header[100..108], 0o644);
        write_octal(&mut header[108..116], 0);
        write_octal(&mut header[116..124], 0);
        write_octal(&mut header[124..136], entry.contents.len() as u64);
        write_octal(&mut header[136..148], 0);
        header[148..156].fill(b' ');
        header[156] = match entry.kind {
            Kind::File => b'0',
            Kind::Directory => b'5',
            Kind::Symlink => b'2',
        };
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
        let checksum = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
        archive.extend_from_slice(&header);
        archive.extend_from_slice(entry.contents);
        archive.resize(archive.len().next_multiple_of(BLOCK_SIZE), 0);
    }
    archive.resize(archive.len() + 2 * BLOCK_SIZE, 0);
    archive
}

fn write_ustar_path(header: &mut [u8; BLOCK_SIZE], path: &str) {
    assert!(path.len() <= 100, "test path must fit ustar name field");
    header[..path.len()].copy_from_slice(path.as_bytes());
}

fn write_octal(field: &mut [u8], value: u64) {
    let encoded = format!("{:0width$o}\0", value, width = field.len() - 1);
    assert_eq!(encoded.len(), field.len());
    field.copy_from_slice(encoded.as_bytes());
}

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rustrace-assignment-package-v2-{}-{sequence}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        fs::create_dir(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            panic!("failed to remove test directory: {error}");
        }
    }
}
