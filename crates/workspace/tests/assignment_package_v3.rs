//! Assignment package format 3: optional `NAME.in`, `NAME.args`, and the
//! `test-cases/files/` fixture tree.
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustrace_model::{TestCaseArgsError, WorkspacePath};
use rustrace_workspace::assignment_package::{
    AssignmentPackageError, ExtractedAssignment, ExtractionLimits, MAX_TEST_CASE_FILE_BYTES,
    MAX_TEST_CASE_TOTAL_BYTES, extract_assignment_package,
};
use rustrace_workspace::fixture_tree::{
    FixtureTreeError, MAX_FIXTURE_DIRECTORIES, MAX_FIXTURE_FILES,
};

const BLOCK_SIZE: usize = 512;
const MANIFEST_V1: &str = r#"format_version = 1
course_id = "ECE1724"
assignment_id = "lab2"
assignment_version = "2026-09-28"
title = "Search Utility"
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
    Other(u8),
}

struct Entry {
    path: String,
    kind: Kind,
    contents: Vec<u8>,
}

impl Entry {
    fn file(path: impl Into<String>, contents: impl AsRef<[u8]>) -> Self {
        Self {
            path: path.into(),
            kind: Kind::File,
            contents: contents.as_ref().to_vec(),
        }
    }

    fn directory(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: Kind::Directory,
            contents: Vec::new(),
        }
    }

    fn other(path: impl Into<String>, flag: u8) -> Self {
        Self {
            path: path.into(),
            kind: Kind::Other(flag),
            contents: Vec::new(),
        }
    }
}

fn manifest(version: u32) -> String {
    MANIFEST_V1.replacen(
        "format_version = 1",
        &format!("format_version = {version}"),
        1,
    )
}

fn base(version: u32) -> Vec<Entry> {
    vec![
        Entry::file("assignment.toml", manifest(version)),
        Entry::file("starter/Cargo.toml", b"[package]\n[workspace]\n"),
        Entry::file("starter/src/main.rs", b"fn main() {}\n"),
    ]
}

fn extract(version: u32, extra: Vec<Entry>) -> Result<ExtractedAssignment, AssignmentPackageError> {
    let mut entries = base(version);
    entries.extend(extra);
    let root = TempRoot::new();
    let destination = root.path().join("workspace");
    let result = extract_assignment_package(
        Cursor::new(package(&entries)),
        &destination,
        ExtractionLimits::default(),
    );
    if result.is_err() {
        assert!(
            !destination.exists(),
            "failed extraction published a workspace"
        );
    } else {
        assert!(
            !destination.join("test-cases").exists() && !destination.join("files").exists(),
            "cases and fixtures never enter the starter workspace"
        );
    }
    result
}

fn search_suite() -> Vec<Entry> {
    vec![
        Entry::directory("test-cases/"),
        Entry::file("test-cases/count.args", "-c\nfn\nsrc/lib.rs\n"),
        Entry::file("test-cases/count.expected", "2\n"),
        Entry::file("test-cases/stdin.in", "alpha\nbeta\n"),
        Entry::file("test-cases/stdin.args", "beta\n"),
        Entry::file("test-cases/stdin.expected", "beta\n"),
        Entry::file(
            "test-cases/usage.expected",
            "usage: grep PATTERN [FILE...]\n",
        ),
        Entry::directory("test-cases/files/"),
        Entry::directory("test-cases/files/empty/"),
        Entry::file("test-cases/files/src/lib.rs", "fn a() {}\nfn b() {}\n"),
        Entry::file("test-cases/files/notes with space.txt", "fn\n"),
    ]
}

#[test]
fn format_three_returns_optional_input_parsed_args_and_the_fixture_tree() {
    let extracted = extract(3, search_suite()).expect("valid format 3 package");
    assert_eq!(extracted.manifest.format_version, 3);
    let suite = extracted.test_cases.expect("format 3 suite");
    assert_eq!(suite.format_version, 3);
    let names = suite
        .cases
        .iter()
        .map(|case| case.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["count", "stdin", "usage"]);

    let count = &suite.cases[0];
    assert_eq!(count.input, None);
    assert_eq!(count.args, ["-c", "fn", "src/lib.rs"]);
    assert_eq!(
        count.args_file.as_deref(),
        Some(&b"-c\nfn\nsrc/lib.rs\n"[..])
    );
    assert_eq!(count.expected, b"2\n");
    let stdin = &suite.cases[1];
    assert_eq!(stdin.input.as_deref(), Some(&b"alpha\nbeta\n"[..]));
    assert_eq!(stdin.args, ["beta"]);
    let usage = &suite.cases[2];
    assert_eq!(
        (usage.input.as_ref(), usage.args_file.as_ref()),
        (None, None)
    );
    assert!(usage.args.is_empty());

    let fixtures = suite.fixtures.expect("fixture tree");
    assert_eq!(
        fixtures
            .directories()
            .map(WorkspacePath::as_str)
            .collect::<Vec<_>>(),
        ["empty", "src"],
        "explicit empty directories and implied file ancestors"
    );
    assert_eq!(
        fixtures
            .files()
            .map(|(path, bytes)| (path.as_str(), bytes))
            .collect::<Vec<_>>(),
        [
            ("notes with space.txt", &b"fn\n"[..]),
            ("src/lib.rs", &b"fn a() {}\nfn b() {}\n"[..]),
        ]
    );
    assert_eq!(fixtures.total_bytes(), 23);
    let case_bytes = 17 + 2 + 11 + 5 + 5 + 30;
    assert_eq!(suite.total_bytes, case_bytes + 23);
}

#[test]
fn format_three_suite_hash_is_order_independent_and_covers_every_role_and_fixture() {
    let hash = |entries: Vec<Entry>| extract(3, entries).unwrap().test_cases.unwrap().hash;
    let baseline = hash(search_suite());
    let mut reversed = search_suite();
    reversed.reverse();
    assert_eq!(baseline, hash(reversed));
    let without_explicit_parents = search_suite()
        .into_iter()
        .filter(|entry| entry.path != "test-cases/" && entry.path != "test-cases/files/")
        .collect::<Vec<_>>();
    assert_eq!(
        baseline,
        hash(without_explicit_parents),
        "implied directories hash like explicit ones"
    );

    let changed = |path: &str, contents: Option<&str>| {
        let mut entries = search_suite()
            .into_iter()
            .filter(|entry| entry.path != path)
            .collect::<Vec<_>>();
        if let Some(contents) = contents {
            entries.push(Entry::file(path, contents));
        }
        hash(entries)
    };
    let variants = [
        (
            "argument bytes",
            changed(
                "test-cases/count.args",
                Some("-c\nfn\nsrc/lib.rs\n\u{0020}x\n"),
            ),
        ),
        (
            "argument file removed",
            changed("test-cases/stdin.args", None),
        ),
        (
            "empty args file added",
            changed("test-cases/usage.args", Some("")),
        ),
        (
            "empty input added",
            changed("test-cases/usage.in", Some("")),
        ),
        ("input removed", changed("test-cases/stdin.in", None)),
        (
            "input bytes",
            changed("test-cases/stdin.in", Some("alpha\nBETA\n")),
        ),
        (
            "expected bytes",
            changed("test-cases/usage.expected", Some("usage\n")),
        ),
        (
            "fixture bytes",
            changed("test-cases/files/src/lib.rs", Some("fn a() {}\n")),
        ),
        (
            "fixture removed",
            changed("test-cases/files/notes with space.txt", None),
        ),
        (
            "fixture added",
            changed("test-cases/files/src/main.rs", Some("")),
        ),
    ];
    for (label, variant) in &variants {
        assert_ne!(baseline, *variant, "{label}");
    }
    let mut moved = search_suite()
        .into_iter()
        .filter(|entry| !entry.path.ends_with("lib.rs"))
        .collect::<Vec<_>>();
    moved.push(Entry::file(
        "test-cases/files/src/main.rs",
        "fn a() {}\nfn b() {}\n",
    ));
    assert_ne!(baseline, hash(moved), "fixture path");
    let without_empty = search_suite()
        .into_iter()
        .filter(|entry| entry.path != "test-cases/files/empty/")
        .collect::<Vec<_>>();
    assert_ne!(baseline, hash(without_empty), "empty fixture directory");

    let no_fixtures = || {
        search_suite()
            .into_iter()
            .filter(|entry| !entry.path.starts_with("test-cases/files"))
            .collect::<Vec<_>>()
    };
    let mut empty_tree = no_fixtures();
    empty_tree.push(Entry::directory("test-cases/files/"));
    let absent = extract(3, no_fixtures()).unwrap().test_cases.unwrap();
    let empty = extract(3, empty_tree).unwrap().test_cases.unwrap();
    assert_eq!(absent.fixtures, None);
    assert_eq!(
        empty.fixtures.as_ref().map(|tree| tree.file_count()),
        Some(0)
    );
    assert_ne!(
        absent.hash, empty.hash,
        "an empty files/ is still a fixture tree"
    );
}

#[test]
fn format_three_hashes_match_their_documented_encodings_and_goldens() {
    let suite = extract(3, search_suite()).unwrap().test_cases.unwrap();
    let fixtures = suite.fixtures.as_ref().unwrap();

    let mut tree = b"rustrace.test-case-fixtures.v1".to_vec();
    tree.extend_from_slice(&4_u32.to_be_bytes());
    for (path, bytes) in [
        ("empty", None),
        ("notes with space.txt", Some(&b"fn\n"[..])),
        ("src", None),
        ("src/lib.rs", Some(&b"fn a() {}\nfn b() {}\n"[..])),
    ] {
        tree.push(if bytes.is_some() { 2 } else { 1 });
        tree.extend_from_slice(&(path.len() as u32).to_be_bytes());
        tree.extend_from_slice(path.as_bytes());
        if let Some(bytes) = bytes {
            tree.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            tree.extend_from_slice(bytes);
        }
    }
    assert_eq!(
        fixtures.hash().to_string(),
        blake3::hash(&tree).to_hex().as_str()
    );

    let mut material = b"rustrace.test-case-suite.v2".to_vec();
    material.extend_from_slice(&3_u32.to_be_bytes());
    let role = |material: &mut Vec<u8>, bytes: Option<&[u8]>| match bytes {
        Some(bytes) => {
            material.push(1);
            material.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            material.extend_from_slice(bytes);
        }
        None => material.push(0),
    };
    for (name, args, input, expected) in [
        (
            "count",
            Some(&b"-c\nfn\nsrc/lib.rs\n"[..]),
            None,
            &b"2\n"[..],
        ),
        (
            "stdin",
            Some(b"beta\n"),
            Some(&b"alpha\nbeta\n"[..]),
            b"beta\n",
        ),
        ("usage", None, None, b"usage: grep PATTERN [FILE...]\n"),
    ] {
        material.extend_from_slice(&(name.len() as u32).to_be_bytes());
        material.extend_from_slice(name.as_bytes());
        role(&mut material, args);
        role(&mut material, input);
        role(&mut material, Some(expected));
    }
    material.push(1);
    material.extend_from_slice(fixtures.hash().as_bytes());
    assert_eq!(
        suite.hash.to_string(),
        blake3::hash(&material).to_hex().as_str()
    );

    assert_eq!(
        fixtures.hash().to_string(),
        "e131aab7abe246bfe053b4b91f281c6558fb9e2f382be737b8be7d2916663caa"
    );
    assert_eq!(
        suite.hash.to_string(),
        "8632c8c6965748de4326fd3d641ebe59ea01685ed3b6895eaa4c1b84e59a1dca"
    );
}

#[test]
fn format_three_starters_cannot_carry_cargo_configuration() {
    // Even a policy that allows every path cannot ship `.cargo`: fixture runs
    // would not read it, so they would build differently from F7.
    let open_policy = |version: u32| {
        manifest(version).replace(
            r#"allowed_paths = ["src/**/*.rs", "Cargo.toml"]"#,
            r#"allowed_paths = ["**"]"#,
        )
    };
    for (label, entry) in [
        (
            "file",
            Entry::file("starter/.cargo/config.toml", "[build]\n"),
        ),
        ("directory", Entry::directory("starter/.cargo/")),
        (
            "upper case",
            Entry::file("starter/.Cargo/config", "[build]\n"),
        ),
        (
            "nested",
            Entry::file("starter/tools/.cargo/config.toml", ""),
        ),
    ] {
        let manifest = open_policy(3);
        let mut entries = vec![
            Entry::file("assignment.toml", &manifest),
            Entry::file("starter/Cargo.toml", b"[package]\n[workspace]\n"),
            Entry::file("starter/src/main.rs", b"fn main() {}\n"),
            Entry::file("test-cases/case.expected", ""),
        ];
        entries.push(entry);
        let root = TempRoot::new();
        let error = extract_assignment_package(
            Cursor::new(package(&entries)),
            &root.path().join("workspace"),
            ExtractionLimits::default(),
        )
        .expect_err(label);
        assert!(
            matches!(
                error,
                AssignmentPackageError::StarterCargoConfiguration { .. }
            ),
            "{label}: {error}"
        );
        assert!(!root.path().join("workspace").exists());
    }

    // Format 2 packages keep accepting what their policy allows.
    let manifest = open_policy(2);
    let entries = [
        Entry::file("assignment.toml", &manifest),
        Entry::file("starter/Cargo.toml", b"[package]\n[workspace]\n"),
        Entry::file("starter/src/main.rs", b"fn main() {}\n"),
        Entry::file("starter/.cargo/config.toml", "[build]\n"),
        Entry::file("test-cases/case.in", ""),
        Entry::file("test-cases/case.expected", ""),
    ];
    let root = TempRoot::new();
    extract_assignment_package(
        Cursor::new(package(&entries)),
        &root.path().join("workspace"),
        ExtractionLimits::default(),
    )
    .expect("format 2 is unchanged");
    assert!(root.path().join("workspace/.cargo/config.toml").is_file());
}

#[test]
fn format_three_rejects_case_names_that_differ_only_in_letter_case() {
    let error = extract(
        3,
        vec![
            Entry::file("test-cases/Search.expected", "a"),
            Entry::file("test-cases/search.expected", "b"),
        ],
    )
    .expect_err("case alias");
    assert!(
        matches!(
            &error,
            AssignmentPackageError::TestCaseNameConflict { name, other }
                if name == "search" && other == "Search"
        ),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        "assignment package test cases `Search` and `search` differ only in letter case"
    );
    let error = extract(
        3,
        vec![
            Entry::file("test-cases/A-1.args", "x\n"),
            Entry::file("test-cases/A-1.expected", ""),
            Entry::file("test-cases/a-1.in", ""),
        ],
    )
    .expect_err("an alias through another role still merges");
    assert!(matches!(
        error,
        AssignmentPackageError::TestCaseNameConflict { .. }
    ));

    // Format 2 keeps accepting such names, exactly as before.
    let suite = extract(
        2,
        vec![
            Entry::file("test-cases/Search.in", ""),
            Entry::file("test-cases/Search.expected", "a"),
            Entry::file("test-cases/search.in", ""),
            Entry::file("test-cases/search.expected", "b"),
        ],
    )
    .expect("format 2 is unchanged")
    .test_cases
    .unwrap();
    assert_eq!(suite.format_version, 2);
    assert_eq!(suite.cases.len(), 2);
}

#[test]
fn format_three_requires_expected_output_for_every_case_and_one_case() {
    for (label, entries, missing) in [
        (
            "args only",
            vec![Entry::file("test-cases/only.args", "x\n")],
            "missing `.expected`",
        ),
        (
            "input only",
            vec![Entry::file("test-cases/only.in", "x\n")],
            "missing `.expected`",
        ),
        (
            "input and args",
            vec![
                Entry::file("test-cases/only.in", "x\n"),
                Entry::file("test-cases/only.args", "x\n"),
                Entry::file("test-cases/other.expected", ""),
            ],
            "test case `only` is missing `.expected`",
        ),
    ] {
        let error = extract(3, entries).expect_err(label);
        assert!(
            matches!(error, AssignmentPackageError::IncompleteTestCase { .. }),
            "{label}: {error}"
        );
        assert!(error.to_string().contains(missing), "{label}: {error}");
    }
    for entries in [
        vec![Entry::directory("test-cases/")],
        vec![Entry::file("test-cases/files/data.txt", "fixture\n")],
        vec![],
    ] {
        let error = extract(3, entries).expect_err("no cases");
        assert!(matches!(
            error,
            AssignmentPackageError::MissingTestCases { format_version: 3 }
        ));
        assert_eq!(
            error.to_string(),
            "assignment package format_version 3 has no test cases; a nonempty test-cases/ suite is required"
        );
    }
}

#[test]
fn format_three_rejects_every_invalid_args_encoding_and_bound() {
    let over_count = "a\n".repeat(65);
    let over_argument = format!("{}\n", "x".repeat(1025));
    for (label, bytes, expected) in [
        (
            "no final LF",
            b"-n".to_vec(),
            TestCaseArgsError::MissingFinalNewline,
        ),
        (
            "blank line",
            b"-n\n\nx\n".to_vec(),
            TestCaseArgsError::EmptyArgument { line: 2 },
        ),
        (
            "lone LF",
            b"\n".to_vec(),
            TestCaseArgsError::EmptyArgument { line: 1 },
        ),
        (
            "CRLF",
            b"-n\r\n".to_vec(),
            TestCaseArgsError::ControlCharacter { line: 1 },
        ),
        (
            "NUL",
            b"a\0b\n".to_vec(),
            TestCaseArgsError::ControlCharacter { line: 1 },
        ),
        (
            "tab",
            b"a\tb\n".to_vec(),
            TestCaseArgsError::ControlCharacter { line: 1 },
        ),
        (
            "escape",
            b"\x1b[0m\n".to_vec(),
            TestCaseArgsError::ControlCharacter { line: 1 },
        ),
        (
            "invalid UTF-8",
            b"\xc3\x28\n".to_vec(),
            TestCaseArgsError::InvalidUtf8,
        ),
        (
            "65 arguments",
            over_count.into_bytes(),
            TestCaseArgsError::TooManyArguments,
        ),
        (
            "long argument",
            over_argument.into_bytes(),
            TestCaseArgsError::ArgumentTooLong { line: 1 },
        ),
    ] {
        let error = extract(
            3,
            vec![
                Entry::file("test-cases/case.args", bytes),
                Entry::file("test-cases/case.expected", ""),
            ],
        )
        .expect_err(label);
        match &error {
            AssignmentPackageError::InvalidTestCaseArgs { name, source } => {
                assert_eq!((name.as_str(), *source), ("case", expected), "{label}");
            }
            other => panic!("{label}: {other}"),
        }
        assert!(
            error.to_string().contains("test-cases/case.args"),
            "{error}"
        );
    }

    let at_limit = format!("{}\n", "x".repeat(1023)).repeat(8);
    assert_eq!(at_limit.len(), 8192);
    let extracted = extract(
        3,
        vec![
            Entry::file("test-cases/case.args", &at_limit),
            Entry::file("test-cases/case.expected", ""),
        ],
    )
    .expect("8 KiB args file");
    assert_eq!(extracted.test_cases.unwrap().cases[0].args.len(), 8);
    let error = extract(
        3,
        vec![
            Entry::file("test-cases/case.args", format!("{at_limit}x\n")),
            Entry::file("test-cases/case.expected", ""),
        ],
    )
    .expect_err("args file over 8 KiB");
    assert!(matches!(
        error,
        AssignmentPackageError::TestCaseFileSizeLimitExceeded { limit: 8192, .. }
    ));
}

#[test]
fn format_three_fixtures_accept_only_canonical_regular_files_and_directories() {
    let case = || Entry::file("test-cases/case.expected", "");
    for (label, entry) in [
        ("symlink", Entry::other("test-cases/files/link", b'2')),
        ("hard link", Entry::other("test-cases/files/hard", b'1')),
        (
            "character device",
            Entry::other("test-cases/files/tty", b'3'),
        ),
        ("block device", Entry::other("test-cases/files/disk", b'4')),
        ("FIFO", Entry::other("test-cases/files/pipe", b'6')),
        ("symlinked root", Entry::other("test-cases/files", b'2')),
    ] {
        let error = extract(3, vec![case(), entry]).expect_err(label);
        assert!(
            matches!(error, AssignmentPackageError::UnsupportedEntryType { .. }),
            "{label}: {error}"
        );
    }
    for (label, entry) in [
        ("files as a file", Entry::file("test-cases/files", "x")),
        (
            "non-NFC name",
            Entry::file("test-cases/files/cafe\u{301}.txt", "x"),
        ),
        (
            "control character",
            Entry::file("test-cases/files/a\u{7}.txt", "x"),
        ),
        ("newline", Entry::file("test-cases/files/a\nb", "x")),
        (
            "Cargo configuration",
            Entry::file("test-cases/files/.cargo/config.toml", "x"),
        ),
        (
            "nested Cargo configuration",
            Entry::file("test-cases/files/src/.cargo/config", "x"),
        ),
        (
            "Cargo directory",
            Entry::directory("test-cases/files/.cargo/"),
        ),
        (
            "Cargo directory in another letter case",
            Entry::directory("test-cases/files/.CARGO/"),
        ),
        (
            "Finder metadata",
            Entry::file("test-cases/files/.DS_Store", "x"),
        ),
        (
            "nested Finder metadata",
            Entry::file("test-cases/files/src/.DS_Store", "x"),
        ),
        (
            "Windows thumbnails",
            Entry::file("test-cases/files/Thumbs.db", "x"),
        ),
        (
            "Windows folder settings",
            Entry::file("test-cases/files/desktop.ini", "x"),
        ),
        (
            "editor temporary",
            Entry::file(
                "test-cases/files/.rustrace-editor-0123456789abcdef0123456789abcdef.tmp",
                "x",
            ),
        ),
        ("backslash", Entry::file("test-cases/files/a\\b", "x")),
        ("other directory", Entry::directory("test-cases/data/")),
        (
            "nested case",
            Entry::file("test-cases/data/case.expected", "x"),
        ),
        ("unknown case file", Entry::file("test-cases/case.txt", "x")),
        (
            "case folder marker",
            Entry::file("test-cases/.rustrace-cases.json", "{}"),
        ),
    ] {
        let error = extract(3, vec![case(), entry]).expect_err(label);
        assert!(
            matches!(
                error,
                AssignmentPackageError::InvalidTestCasePath { .. }
                    | AssignmentPackageError::UnsafePath { .. }
            ),
            "{label}: {error}"
        );
    }
    let error = extract(
        3,
        vec![case(), Entry::file("test-cases/files/../escape", "x")],
    )
    .expect_err("traversal");
    assert!(matches!(error, AssignmentPackageError::UnsafePath { .. }));

    for (label, entries) in [
        (
            "file below a file",
            vec![
                Entry::file("test-cases/files/a", "x"),
                Entry::file("test-cases/files/a/b", "x"),
            ],
        ),
        (
            "file at an implied directory",
            vec![
                Entry::file("test-cases/files/a/b", "x"),
                Entry::file("test-cases/files/a", "x"),
            ],
        ),
        (
            "directory below a file",
            vec![
                Entry::file("test-cases/files/a", "x"),
                Entry::directory("test-cases/files/a/b/"),
            ],
        ),
        (
            "case alias",
            vec![
                Entry::file("test-cases/files/Data.txt", "x"),
                Entry::file("test-cases/files/data.txt", "x"),
            ],
        ),
        (
            "directory case alias",
            vec![
                Entry::file("test-cases/files/Src/a", "x"),
                Entry::file("test-cases/files/src/b", "x"),
            ],
        ),
    ] {
        let mut entries = entries;
        entries.push(case());
        let error = extract(3, entries).expect_err(label);
        assert!(
            matches!(
                error,
                AssignmentPackageError::Fixtures {
                    source: FixtureTreeError::PathConflict { .. }
                }
            ),
            "{label}: {error}"
        );
    }
    let error = extract(
        3,
        vec![
            case(),
            Entry::directory("test-cases/files/a/"),
            Entry::file("test-cases/files/a", "x"),
        ],
    )
    .expect_err("file and directory with one path");
    assert!(matches!(
        error,
        AssignmentPackageError::DuplicatePath { .. }
    ));
}

#[test]
fn format_three_fixtures_share_the_case_limits_and_have_count_limits() {
    let case = || Entry::file("test-cases/case.expected", "");
    let maximum = vec![b'x'; MAX_TEST_CASE_FILE_BYTES as usize];
    extract(
        3,
        vec![case(), Entry::file("test-cases/files/max", &maximum)],
    )
    .expect("maximum fixture file");
    let error = extract(
        3,
        vec![
            case(),
            Entry::file("test-cases/files/over", vec![b'x'; maximum.len() + 1]),
        ],
    )
    .expect_err("oversized fixture");
    assert!(matches!(
        error,
        AssignmentPackageError::TestCaseFileSizeLimitExceeded { .. }
    ));

    let chunks = (MAX_TEST_CASE_TOTAL_BYTES / MAX_TEST_CASE_FILE_BYTES) as usize;
    let mut exact = vec![case()];
    for index in 0..chunks {
        exact.push(Entry::file(format!("test-cases/files/{index}"), &maximum));
    }
    let suite = extract(3, exact)
        .expect("exact combined total")
        .test_cases
        .unwrap();
    assert_eq!(suite.total_bytes, MAX_TEST_CASE_TOTAL_BYTES);
    let mut over = vec![Entry::file("test-cases/case.expected", "x")];
    for index in 0..chunks {
        over.push(Entry::file(format!("test-cases/files/{index}"), &maximum));
    }
    let error = extract(3, over).expect_err("fixtures plus cases over the combined total");
    assert!(matches!(
        error,
        AssignmentPackageError::TestCaseTotalSizeLimitExceeded { .. }
    ));

    let mut files = vec![case()];
    for index in 0..MAX_FIXTURE_FILES {
        files.push(Entry::file(format!("test-cases/files/{index}"), ""));
    }
    extract(3, files).expect("maximum fixture file count");
    let mut files = vec![case()];
    for index in 0..=MAX_FIXTURE_FILES {
        files.push(Entry::file(format!("test-cases/files/{index}"), ""));
    }
    let error = extract(3, files).expect_err("fixture file count");
    assert!(matches!(
        error,
        AssignmentPackageError::Fixtures {
            source: FixtureTreeError::LimitExceeded { kind: "file", .. }
        }
    ));

    let mut directories = vec![case()];
    for index in 0..MAX_FIXTURE_DIRECTORIES {
        directories.push(Entry::directory(format!("test-cases/files/{index}/")));
    }
    extract(3, directories).expect("maximum fixture directory count");
    let mut directories = vec![case()];
    for index in 0..MAX_FIXTURE_DIRECTORIES {
        directories.push(Entry::directory(format!("test-cases/files/{index}/")));
    }
    directories.push(Entry::file("test-cases/files/extra/file", ""));
    let error = extract(3, directories).expect_err("implied directory over the count");
    assert!(matches!(
        error,
        AssignmentPackageError::Fixtures {
            source: FixtureTreeError::LimitExceeded {
                kind: "directory",
                ..
            }
        }
    ));
}

#[test]
fn format_two_still_rejects_args_files_and_fixtures_exactly_as_before() {
    for (entry, reason) in [
        (
            Entry::file("test-cases/case.args", "x\n"),
            "expected NAME.in or NAME.expected",
        ),
        (
            Entry::directory("test-cases/files/"),
            "directories below test-cases/ are not allowed",
        ),
        (
            Entry::file("test-cases/files/data.txt", "x"),
            "expected NAME.in or NAME.expected",
        ),
        (
            Entry::file("test-cases/files", "x"),
            "expected NAME.in or NAME.expected",
        ),
    ] {
        let path = entry.path.clone();
        let error = extract(
            2,
            vec![
                Entry::file("test-cases/case.in", ""),
                Entry::file("test-cases/case.expected", ""),
                entry,
            ],
        )
        .expect_err(&path);
        assert!(
            matches!(
                &error,
                AssignmentPackageError::InvalidTestCasePath { reason: actual, .. } if *actual == reason
            ),
            "{path}: {error}"
        );
    }
    let error = extract(2, vec![Entry::file("test-cases/case.expected", "")])
        .expect_err("format 2 input is still required");
    assert_eq!(
        error.to_string(),
        "assignment package test case `case` is missing `.in`"
    );
    let error = extract(2, vec![]).expect_err("format 2 suite is still required");
    assert_eq!(
        error.to_string(),
        "assignment package format_version 2 has no test cases; a nonempty test-cases/ suite is required"
    );
    for entry in [
        Entry::file("test-cases/case.args", "x\n"),
        Entry::file("test-cases/files/data.txt", "x"),
    ] {
        let error = extract(1, vec![entry]).expect_err("format 1");
        assert!(matches!(
            error,
            AssignmentPackageError::UnexpectedEntry { .. }
        ));
    }
}

#[test]
fn format_four_is_refused_before_any_entry_is_used() {
    let error = extract(4, search_suite()).expect_err("format 4");
    assert!(
        error
            .to_string()
            .contains("format_version 4 is unsupported"),
        "{error}"
    );
}

fn package(entries: &[Entry]) -> Vec<u8> {
    let mut archive = Vec::new();
    for entry in entries {
        let mut header = [0_u8; BLOCK_SIZE];
        write_ustar_path(&mut header, &entry.path);
        write_octal(&mut header[100..108], 0o644);
        write_octal(&mut header[108..116], 0);
        write_octal(&mut header[116..124], 0);
        write_octal(&mut header[124..136], entry.contents.len() as u64);
        write_octal(&mut header[136..148], 0);
        header[148..156].fill(b' ');
        header[156] = match entry.kind {
            Kind::File => b'0',
            Kind::Directory => b'5',
            Kind::Other(flag) => flag,
        };
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
        let checksum = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
        archive.extend_from_slice(&header);
        archive.extend_from_slice(&entry.contents);
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
            "rustrace-assignment-package-v3-{}-{sequence}",
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
