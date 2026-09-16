use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustrace_model::MAX_WORKSPACE_PATH_DEPTH;
use rustrace_model::assignment::MAX_MANIFEST_BYTES;
use rustrace_workspace::assignment_package::{
    AssignmentPackageError, ExtractionLimitError, ExtractionLimits, HARD_MAX_ENTRIES,
    HARD_MAX_EXPANDED_BYTES, MAX_TEST_CASE_TOTAL_BYTES, extract_assignment_package,
};
use rustrace_workspace::hash::{
    MAX_WORKSPACE_FILE_BYTES, MAX_WORKSPACE_FILES, MAX_WORKSPACE_TOTAL_BYTES,
};

const BLOCK_SIZE: usize = 512;
const MANIFEST: &str = r#"format_version = 1
course_id = "ECE1724"
assignment_id = "a3"
assignment_version = "2026-09-01"
title = "Ownership and Graph Traversal"
toolchain = "1.92.0"
edition = "2024"
allowed_paths = ["Cargo.toml", "src/**/*.rs"]

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
        if matches!(entry.kind, Kind::Symlink) {
            header[157..167].copy_from_slice(b"target.txt");
        }
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
    if path.len() <= 100 {
        header[..path.len()].copy_from_slice(path.as_bytes());
        return;
    }

    let split = path
        .rmatch_indices('/')
        .map(|(index, _)| index)
        .find(|index| *index <= 155 && path.len() - index - 1 <= 100)
        .expect("test path fits the ustar prefix and name fields");
    let (prefix, name) = path.split_at(split);
    let name = &name[1..];
    header[..name.len()].copy_from_slice(name.as_bytes());
    header[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
}

fn manifest_with_allowed_paths(patterns: &str) -> String {
    MANIFEST.replace(
        "allowed_paths = [\"Cargo.toml\", \"src/**/*.rs\"]",
        &format!("allowed_paths = {patterns}"),
    )
}

fn entry_header_offset(entries: &[Entry<'_>], index: usize) -> usize {
    entries[..index]
        .iter()
        .map(|entry| BLOCK_SIZE + entry.contents.len().next_multiple_of(BLOCK_SIZE))
        .sum()
}

fn write_octal(field: &mut [u8], value: u64) {
    let encoded = format!("{:0width$o}\0", value, width = field.len() - 1);
    assert_eq!(encoded.len(), field.len());
    field.copy_from_slice(encoded.as_bytes());
}

fn valid_entries<'a>() -> Vec<Entry<'a>> {
    vec![
        Entry::file("assignment.toml", MANIFEST.as_bytes()),
        Entry::directory("starter/"),
        Entry::file(
            "starter/Cargo.toml",
            b"[package]\nname = \"starter\"\n[workspace]\n",
        ),
        Entry::directory("starter/src/"),
        Entry::file("starter/src/main.rs", b"fn main() {}\n"),
    ]
}

#[test]
fn starter_manifest_requires_a_self_contained_package_in_both_versions() {
    for version in [1, 2] {
        let manifest =
            MANIFEST.replace("format_version = 1", &format!("format_version = {version}"));
        for (cargo, offending) in [
            (
                "[package]\n",
                "add an empty [workspace] table to starter/Cargo.toml",
            ),
            ("[workspace]\n", "[package] table"),
            (
                "[package]\n[workspace]\nmembers = []\n",
                "workspace.members",
            ),
            (
                "[package]\nworkspace = 'parent'\n[workspace]\n",
                "package.workspace",
            ),
            (
                "[package]\n[workspace]\nexclude = []\n",
                "workspace.exclude",
            ),
            (
                "[package]\n[workspace]\ndefault-members = []\n",
                "workspace.default-members",
            ),
            ("[package]\n[workspace.package]\n", "workspace.package"),
            (
                "[package]\n[workspace.dependencies]\n",
                "workspace.dependencies",
            ),
            (
                "[package]\n[workspace]\nresolver = '3'\n",
                "workspace.resolver",
            ),
            (
                "[package]\nworkspace = false\n[workspace]\n",
                "package.workspace",
            ),
            ("package = 'wrong type'\n[workspace]\n", "[package] table"),
            ("workspace = []\n[package]\n", "empty [workspace] table"),
            ("not valid TOML", "valid TOML"),
        ] {
            let temp = TempRoot::new();
            let destination = temp.path().join("workspace");
            let mut entries = vec![
                Entry::file("assignment.toml", manifest.as_bytes()),
                Entry::file("starter/Cargo.toml", cargo.as_bytes()),
            ];
            if version == 2 {
                entries.extend([
                    Entry::file("test-cases/sample.in", b""),
                    Entry::file("test-cases/sample.expected", b""),
                ]);
            }
            let error = extract_assignment_package(
                Cursor::new(package(&entries)),
                &destination,
                ExtractionLimits::default(),
            )
            .expect_err("non-compliant starter");
            assert!(
                error
                    .to_string()
                    .contains("assignment starter must be a self-contained package:"),
                "{error}"
            );
            assert!(error.to_string().contains(offending), "{error}");
            assert!(!destination.exists());
            assert_no_staging(temp.path());
        }
        for cargo in [
            "[package]\n[workspace]\n",
            "[package]\n[workspace]\n# comments are allowed\n",
        ] {
            let temp = TempRoot::new();
            let destination = temp.path().join("workspace");
            let mut entries = vec![
                Entry::file("assignment.toml", manifest.as_bytes()),
                Entry::file("starter/Cargo.toml", cargo.as_bytes()),
            ];
            if version == 2 {
                entries.extend([
                    Entry::file("test-cases/sample.in", b""),
                    Entry::file("test-cases/sample.expected", b""),
                ]);
            }
            extract_assignment_package(
                Cursor::new(package(&entries)),
                &destination,
                ExtractionLimits::default(),
            )
            .expect("self-contained package");
            assert_eq!(
                fs::read(destination.join("Cargo.toml")).unwrap(),
                cargo.as_bytes()
            );
        }
        let temp = TempRoot::new();
        let destination = temp.path().join("missing-manifest");
        let mut entries = vec![
            Entry::file("assignment.toml", manifest.as_bytes()),
            Entry::file("starter/src/main.rs", b""),
        ];
        if version == 2 {
            entries.extend([
                Entry::file("test-cases/sample.in", b""),
                Entry::file("test-cases/sample.expected", b""),
            ]);
        }
        let error = extract_assignment_package(
            Cursor::new(package(&entries)),
            &destination,
            ExtractionLimits::default(),
        )
        .expect_err("missing starter manifest");
        assert!(error.to_string().contains("starter/Cargo.toml"), "{error}");
        assert!(!destination.exists());
        assert_no_staging(temp.path());
    }
}

#[test]
fn extracts_a_valid_starter_tree_and_returns_its_manifest() {
    let temp = TempRoot::new();
    let destination = temp.path().join("workspace");

    let report = extract_assignment_package(
        Cursor::new(package(&valid_entries())),
        &destination,
        ExtractionLimits::default(),
    )
    .expect("safe package");

    assert_eq!(report.manifest.assignment_id, "a3");
    assert_eq!(report.starter_files, 2);
    assert_eq!(
        fs::read_to_string(destination.join("Cargo.toml")).unwrap(),
        "[package]\nname = \"starter\"\n[workspace]\n"
    );
    assert_eq!(
        fs::read_to_string(destination.join("src/main.rs")).unwrap(),
        "fn main() {}\n"
    );
    assert!(!destination.join("assignment.toml").exists());
}

#[test]
fn rejects_traversal_and_absolute_paths_without_writing_outside_destination() {
    for unsafe_path in [
        "starter/../../escaped.rs",
        "/absolute.rs",
        "starter\\escape.rs",
    ] {
        let temp = TempRoot::new();
        let destination = temp.path().join("workspace");
        let entries = [
            Entry::file("assignment.toml", MANIFEST.as_bytes()),
            Entry::file(unsafe_path, b"escaped"),
        ];

        let error = extract_assignment_package(
            Cursor::new(package(&entries)),
            &destination,
            ExtractionLimits::default(),
        )
        .expect_err("unsafe path");

        assert!(matches!(error, AssignmentPackageError::UnsafePath { .. }));
        assert!(!temp.path().join("escaped.rs").exists());
    }
}

#[test]
fn rejects_windows_prefixes_after_stripping_the_starter_root() {
    for unsafe_path in [
        "starter/C:/escaped.rs",
        "starter/C:escaped.rs",
        "starter//server/share.rs",
        "starter/\\\\server\\share.rs",
    ] {
        let temp = TempRoot::new();
        let destination = temp.path().join("workspace");
        let entries = [
            Entry::file("assignment.toml", MANIFEST.as_bytes()),
            Entry::file(unsafe_path, b"escaped"),
        ];

        let error = extract_assignment_package(
            Cursor::new(package(&entries)),
            &destination,
            ExtractionLimits::default(),
        )
        .expect_err("nested Windows prefix");

        assert!(matches!(error, AssignmentPackageError::UnsafePath { .. }));
        assert!(!destination.exists());
        assert_no_staging(temp.path());
    }
}

#[test]
fn rejects_symlinks_and_duplicate_targets() {
    let temp = TempRoot::new();
    let entries = [
        Entry::file("assignment.toml", MANIFEST.as_bytes()),
        Entry::symlink("starter/link"),
    ];
    let error = extract_assignment_package(
        Cursor::new(package(&entries)),
        &temp.path().join("symlink-workspace"),
        ExtractionLimits::default(),
    )
    .expect_err("symlink");
    assert!(matches!(
        error,
        AssignmentPackageError::UnsupportedEntryType { .. }
    ));

    let entries = [
        Entry::file("assignment.toml", MANIFEST.as_bytes()),
        Entry::file("starter/src/lib.rs", b"first"),
        Entry::file("starter/src/lib.rs", b"second"),
    ];
    let error = extract_assignment_package(
        Cursor::new(package(&entries)),
        &temp.path().join("duplicate-workspace"),
        ExtractionLimits::default(),
    )
    .expect_err("duplicate");
    assert!(matches!(
        error,
        AssignmentPackageError::DuplicatePath { .. }
    ));
}

#[test]
fn refuses_a_preexisting_destination_instead_of_overwriting_it() {
    let temp = TempRoot::new();
    let destination = temp.path().join("workspace");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("sentinel"), "keep me").unwrap();

    let error = extract_assignment_package(
        Cursor::new(package(&valid_entries())),
        &destination,
        ExtractionLimits::default(),
    )
    .expect_err("destination exists");

    assert!(matches!(
        error,
        AssignmentPackageError::DestinationExists { .. }
    ));
    assert_eq!(
        fs::read_to_string(destination.join("sentinel")).unwrap(),
        "keep me"
    );
}

#[test]
fn enforces_entry_and_expanded_size_limits_from_headers() {
    let temp = TempRoot::new();
    let entries = [
        Entry::file("assignment.toml", MANIFEST.as_bytes()),
        Entry::file("starter/src/lib.rs", b"content is never accepted"),
    ];

    let error = extract_assignment_package(
        Cursor::new(package(&entries)),
        &temp.path().join("entry-limited"),
        ExtractionLimits::new(1, HARD_MAX_EXPANDED_BYTES).unwrap(),
    )
    .expect_err("entry limit");
    assert!(matches!(
        error,
        AssignmentPackageError::EntryLimitExceeded { limit: 1 }
    ));

    let limit = MANIFEST.len() as u64;
    let error = extract_assignment_package(
        Cursor::new(package(&entries)),
        &temp.path().join("size-limited"),
        ExtractionLimits::new(10, limit).unwrap(),
    )
    .expect_err("expanded size limit");
    assert!(matches!(
        error,
        AssignmentPackageError::ExpandedSizeLimitExceeded {
            limit: actual,
            ..
        } if actual == limit
    ));
}

#[test]
fn refuses_caller_limits_above_the_unconditional_hard_maxima() {
    assert!(matches!(
        ExtractionLimits::new(HARD_MAX_ENTRIES + 1, HARD_MAX_EXPANDED_BYTES),
        Err(ExtractionLimitError::TooManyEntries { .. })
    ));
    assert!(matches!(
        ExtractionLimits::new(HARD_MAX_ENTRIES, HARD_MAX_EXPANDED_BYTES + 1),
        Err(ExtractionLimitError::TooManyExpandedBytes { .. })
    ));
}

#[test]
fn rejects_noncanonical_starter_paths_before_publication() {
    let over_depth = format!(
        "starter/{}",
        std::iter::repeat_n("d", MAX_WORKSPACE_PATH_DEPTH + 1)
            .collect::<Vec<_>>()
            .join("/")
    );
    let paths = [
        ("starter/src/cafe\u{301}.rs".to_owned(), false),
        ("starter/src/cafe\u{301}/".to_owned(), true),
        (over_depth, false),
        ("starter/src\\main.rs".to_owned(), false),
        ("starter/src//main.rs".to_owned(), false),
        ("starter/src/./main.rs".to_owned(), false),
    ];

    for (path, is_directory) in &paths {
        let temp = TempRoot::new();
        let destination = temp.path().join("workspace");
        let starter = if *is_directory {
            Entry::directory(path)
        } else {
            Entry::file(path, b"fn main() {}\n")
        };
        let entries = [Entry::file("assignment.toml", MANIFEST.as_bytes()), starter];

        let error = extract_assignment_package(
            Cursor::new(package(&entries)),
            &destination,
            ExtractionLimits::default(),
        )
        .expect_err(path);

        assert!(matches!(
            error,
            AssignmentPackageError::InvalidStarterPath { .. }
                | AssignmentPackageError::UnsafePath { .. }
        ));
        assert!(!destination.exists());
        assert_no_staging(temp.path());
    }
}

#[test]
fn rejects_invalid_or_violated_allowed_path_policy_before_publication() {
    for (patterns, starter_path, expected) in [
        ("[\"src/**bad.rs\"]", "starter/src/main.rs", "invalid"),
        ("[\"src/**/*.rs\"]", "starter/README.md", "not allowed"),
        (
            concat!("[\"src/cafe", "\u{301}", "*.rs\"]"),
            "starter/src/main.rs",
            "non-NFC",
        ),
    ] {
        let temp = TempRoot::new();
        let destination = temp.path().join("workspace");
        let manifest = manifest_with_allowed_paths(patterns);
        let entries = [
            Entry::file("assignment.toml", manifest.as_bytes()),
            Entry::file(starter_path, b"content"),
        ];

        let error = extract_assignment_package(
            Cursor::new(package(&entries)),
            &destination,
            ExtractionLimits::default(),
        )
        .expect_err(expected);

        assert!(matches!(error, AssignmentPackageError::PathPolicy { .. }));
        assert!(!destination.exists());
        assert_no_staging(temp.path());
    }
}

#[test]
fn accepts_exact_workspace_file_size_and_rejects_one_byte_more() {
    let maximum = vec![b'x'; MAX_WORKSPACE_FILE_BYTES as usize];
    let temp = TempRoot::new();
    let destination = temp.path().join("exact-file-size");
    let entries = [
        Entry::file("assignment.toml", MANIFEST.as_bytes()),
        Entry::file("starter/Cargo.toml", b"[package]\n[workspace]\n"),
        Entry::file("starter/src/maximum.rs", &maximum),
    ];

    let report = extract_assignment_package(
        Cursor::new(package(&entries)),
        &destination,
        ExtractionLimits::default(),
    )
    .expect("exact file-size limit");
    assert_eq!(report.starter_files, 2);
    assert_eq!(
        fs::metadata(destination.join("src/maximum.rs"))
            .unwrap()
            .len(),
        MAX_WORKSPACE_FILE_BYTES
    );

    let oversized = vec![b'x'; MAX_WORKSPACE_FILE_BYTES as usize + 1];
    let rejected = temp.path().join("oversized-file");
    let entries = [
        Entry::file("assignment.toml", MANIFEST.as_bytes()),
        Entry::file("starter/src/oversized.rs", &oversized),
    ];
    let error = extract_assignment_package(
        Cursor::new(package(&entries)),
        &rejected,
        ExtractionLimits::default(),
    )
    .expect_err("one byte over file-size limit");
    assert!(matches!(
        error,
        AssignmentPackageError::StarterFileSizeLimitExceeded { .. }
    ));
    assert!(!rejected.exists());
    assert_no_staging(temp.path());
}

#[test]
fn accepts_exact_workspace_file_count_and_rejects_one_more() {
    let exact_paths = (0..MAX_WORKSPACE_FILES - 1)
        .map(|index| format!("starter/src/file-{index}.rs"))
        .collect::<Vec<_>>();
    let mut exact_entries = vec![
        Entry::file("assignment.toml", MANIFEST.as_bytes()),
        Entry::file("starter/Cargo.toml", b"[package]\n[workspace]\n"),
    ];
    exact_entries.extend(exact_paths.iter().map(|path| Entry::file(path, b"")));
    let temp = TempRoot::new();
    let destination = temp.path().join("exact-file-count");

    let report = extract_assignment_package(
        Cursor::new(package(&exact_entries)),
        &destination,
        ExtractionLimits::default(),
    )
    .expect("exact file-count limit");
    assert_eq!(report.starter_files, MAX_WORKSPACE_FILES);

    let extra_paths = (0..MAX_WORKSPACE_FILES)
        .map(|index| format!("starter/src/extra-{index}.rs"))
        .collect::<Vec<_>>();
    let mut extra_entries = vec![
        Entry::file("assignment.toml", MANIFEST.as_bytes()),
        Entry::file("starter/Cargo.toml", b"[package]\n[workspace]\n"),
    ];
    extra_entries.extend(extra_paths.iter().map(|path| Entry::file(path, b"")));
    let rejected = temp.path().join("too-many-files");
    let error = extract_assignment_package(
        Cursor::new(package(&extra_entries)),
        &rejected,
        ExtractionLimits::default(),
    )
    .expect_err("one file over count limit");
    assert!(matches!(
        error,
        AssignmentPackageError::StarterFileCountLimitExceeded { .. }
    ));
    assert!(!rejected.exists());
    assert_no_staging(temp.path());
}

#[test]
fn accepts_exact_workspace_total_and_rejects_one_byte_more() {
    assert_eq!(
        HARD_MAX_EXPANDED_BYTES,
        MAX_WORKSPACE_TOTAL_BYTES + MAX_TEST_CASE_TOTAL_BYTES + MAX_MANIFEST_BYTES as u64
    );
    let chunk = vec![b'x'; MAX_WORKSPACE_FILE_BYTES as usize];
    let chunk_count = MAX_WORKSPACE_TOTAL_BYTES / MAX_WORKSPACE_FILE_BYTES;
    let paths = (0..chunk_count)
        .map(|index| format!("starter/src/chunk-{index}.rs"))
        .collect::<Vec<_>>();
    let mut entries = vec![Entry::file("assignment.toml", MANIFEST.as_bytes())];
    let cargo = b"[package]\n[workspace]\n";
    entries.push(Entry::file("starter/Cargo.toml", cargo));
    entries.extend(paths.iter().enumerate().map(|(index, path)| {
        let contents = if index == 0 {
            &chunk[..chunk.len() - cargo.len()]
        } else {
            &chunk
        };
        Entry::file(path, contents)
    }));
    let temp = TempRoot::new();
    let destination = temp.path().join("exact-total-size");

    let report = extract_assignment_package(
        Cursor::new(package(&entries)),
        &destination,
        ExtractionLimits::default(),
    )
    .expect("exact starter total-size limit");
    assert_eq!(
        report.expanded_bytes,
        MANIFEST.len() as u64 + MAX_WORKSPACE_TOTAL_BYTES
    );

    entries.push(Entry::file("starter/src/one-more.rs", b"x"));
    let rejected = temp.path().join("oversized-total");
    let error = extract_assignment_package(
        Cursor::new(package(&entries)),
        &rejected,
        ExtractionLimits::default(),
    )
    .expect_err("one byte over total-size limit");
    assert!(matches!(
        error,
        AssignmentPackageError::StarterTotalSizeLimitExceeded { .. }
    ));
    assert!(!rejected.exists());
    assert_no_staging(temp.path());
}

#[test]
fn rejects_bad_checksums_and_truncated_headers_bodies_padding_and_end_blocks() {
    let entries = valid_entries();
    let mut bad_checksum = package(&entries);
    bad_checksum[0] ^= 1;

    let mut truncated_header = package(&entries);
    truncated_header.truncate(entry_header_offset(&entries, 1) + 100);

    let body_entries = [
        Entry::file("assignment.toml", MANIFEST.as_bytes()),
        Entry::file("starter/src/body.rs", b"abcdef"),
    ];
    let body_offset = entry_header_offset(&body_entries, 1) + BLOCK_SIZE;
    let mut truncated_body = package(&body_entries);
    truncated_body.truncate(body_offset + 3);

    let padding_entries = [
        Entry::file("assignment.toml", MANIFEST.as_bytes()),
        Entry::file("starter/src/padding.rs", b"abc"),
    ];
    let padding_offset = entry_header_offset(&padding_entries, 1) + BLOCK_SIZE + 3;
    let mut truncated_padding = package(&padding_entries);
    truncated_padding.truncate(padding_offset);

    let mut truncated_end = package(&entries);
    truncated_end.truncate(truncated_end.len() - BLOCK_SIZE);

    for (name, archive) in [
        ("checksum", bad_checksum),
        ("header", truncated_header),
        ("body", truncated_body),
        ("padding", truncated_padding),
        ("end", truncated_end),
    ] {
        let temp = TempRoot::new();
        let destination = temp.path().join("workspace");
        let error = extract_assignment_package(
            Cursor::new(archive),
            &destination,
            ExtractionLimits::default(),
        )
        .expect_err(name);

        assert!(matches!(
            error,
            AssignmentPackageError::InvalidHeader { .. }
                | AssignmentPackageError::TruncatedArchive { .. }
        ));
        assert!(!destination.exists());
        assert_no_staging(temp.path());
    }
}

#[test]
fn rejects_trailing_data_and_removes_a_fully_written_staging_tree() {
    let temp = TempRoot::new();
    let destination = temp.path().join("workspace");
    let mut archive = package(&valid_entries());
    archive.push(1);

    let error = extract_assignment_package(
        Cursor::new(archive),
        &destination,
        ExtractionLimits::default(),
    )
    .expect_err("trailing data");

    assert!(matches!(error, AssignmentPackageError::TrailingData));
    assert!(!destination.exists());
    assert_no_staging(temp.path());
}

#[cfg(unix)]
#[test]
fn rejects_a_symlink_in_the_destination_ancestor_chain() {
    use std::os::unix::fs::symlink;

    let temp = TempRoot::new();
    let real_parent = temp.path().join("real-parent");
    let linked_parent = temp.path().join("linked-parent");
    fs::create_dir(&real_parent).unwrap();
    symlink(&real_parent, &linked_parent).unwrap();
    let destination = linked_parent.join("workspace");

    let error = extract_assignment_package(
        Cursor::new(package(&valid_entries())),
        &destination,
        ExtractionLimits::default(),
    )
    .expect_err("destination ancestor symlink");

    assert!(matches!(
        error,
        AssignmentPackageError::UnsafeDestinationAncestor { .. }
    ));
    assert!(!real_parent.join("workspace").exists());
    assert_no_staging(&real_parent);
}

#[test]
fn atomic_publication_does_not_replace_a_late_destination() {
    let temp = TempRoot::new();
    let destination = temp.path().join("workspace");
    let reader = DestinationCreatingReader {
        inner: Cursor::new(package(&valid_entries())),
        destination: destination.clone(),
        created: false,
    };

    let error = extract_assignment_package(reader, &destination, ExtractionLimits::default())
        .expect_err("late destination");

    assert!(matches!(
        error,
        AssignmentPackageError::DestinationExists { .. }
    ));
    assert_eq!(
        fs::read_to_string(destination.join("sentinel")).unwrap(),
        "keep me"
    );
    assert_no_staging(temp.path());
}

#[test]
fn rejects_filesystem_equivalent_directory_spellings_where_supported() {
    let temp = TempRoot::new();
    if !filesystem_is_case_insensitive(temp.path()) {
        return;
    }

    let destination = temp.path().join("workspace");
    let manifest = manifest_with_allowed_paths("[\"Source/**/*.rs\", \"source/**/*.rs\"]");
    let entries = [
        Entry::file("assignment.toml", manifest.as_bytes()),
        Entry::file("starter/Source/one.rs", b"one"),
        Entry::file("starter/source/two.rs", b"two"),
    ];
    let error = extract_assignment_package(
        Cursor::new(package(&entries)),
        &destination,
        ExtractionLimits::default(),
    )
    .expect_err("case-equivalent directory");

    assert!(matches!(
        error,
        AssignmentPackageError::FilesystemPathCollision { .. }
    ));
    assert!(!destination.exists());
    assert_no_staging(temp.path());
}

#[cfg(unix)]
#[test]
fn extracts_many_directories_with_a_256_descriptor_limit() {
    const CHILD_ENV: &str = "RUSTRACE_LOW_FD_EXTRACTION_CHILD";

    if std::env::var_os(CHILD_ENV).is_some() {
        let paths = (0..600)
            .map(|index| format!("starter/wide/directory-{index}/"))
            .collect::<Vec<_>>();
        let mut entries = vec![Entry::file("assignment.toml", MANIFEST.as_bytes())];
        entries.push(Entry::file(
            "starter/Cargo.toml",
            b"[package]\n[workspace]\n",
        ));
        entries.extend(paths.iter().map(|path| Entry::directory(path)));
        entries.push(Entry::file(
            "starter/src/main.rs",
            b"fn main() { println!(\"bounded fds\"); }\n",
        ));
        let temp = TempRoot::new();
        let destination = temp.path().join("workspace");

        let report = extract_assignment_package(
            Cursor::new(package(&entries)),
            &destination,
            ExtractionLimits::default(),
        )
        .expect("wide valid package must not exhaust descriptors");

        assert_eq!(report.starter_files, 2);
        assert_eq!(report.manifest.assignment_id, "a3");
        assert_eq!(fs::read_dir(destination.join("wide")).unwrap().count(), 600);
        assert_eq!(
            fs::read_to_string(destination.join("src/main.rs")).unwrap(),
            "fn main() { println!(\"bounded fds\"); }\n"
        );
        return;
    }

    let executable = std::env::current_exe().expect("resolve integration test executable");
    let output = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "ulimit -n 256 && exec \"$1\" --exact extracts_many_directories_with_a_256_descriptor_limit --nocapture",
            "rustrace-low-fd-test",
        ])
        .arg(executable)
        .env(CHILD_ENV, "1")
        .output()
        .expect("run isolated low-descriptor child");

    assert!(
        output.status.success(),
        "low-descriptor child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn filesystem_is_case_insensitive(root: &Path) -> bool {
    let lower = root.join("case-probe");
    fs::write(&lower, "probe").unwrap();
    let insensitive = root.join("CASE-PROBE").exists();
    fs::remove_file(lower).unwrap();
    insensitive
}

fn assert_no_staging(parent: &Path) {
    let staging = fs::read_dir(parent)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .find(|name| name.to_string_lossy().starts_with(".rustrace-staging-"));
    assert!(
        staging.is_none(),
        "private staging directory was not cleaned"
    );
}

struct DestinationCreatingReader {
    inner: Cursor<Vec<u8>>,
    destination: PathBuf,
    created: bool,
}

impl Read for DestinationCreatingReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if !self.created && self.inner.position() == self.inner.get_ref().len() as u64 {
            if !self.destination.exists() {
                fs::create_dir(&self.destination)?;
            }
            fs::write(self.destination.join("sentinel"), "keep me")?;
            self.created = true;
        }
        self.inner.read(buffer)
    }
}

#[test]
fn requires_exactly_one_manifest_and_at_least_one_starter_file() {
    let temp = TempRoot::new();
    let missing = [Entry::file("starter/src/lib.rs", b"")];
    let error = extract_assignment_package(
        Cursor::new(package(&missing)),
        &temp.path().join("missing-manifest"),
        ExtractionLimits::default(),
    )
    .expect_err("missing manifest");
    assert!(matches!(error, AssignmentPackageError::MissingManifest));

    let duplicate = [
        Entry::file("assignment.toml", MANIFEST.as_bytes()),
        Entry::file("assignment.toml", MANIFEST.as_bytes()),
        Entry::file("starter/src/lib.rs", b""),
    ];
    let error = extract_assignment_package(
        Cursor::new(package(&duplicate)),
        &temp.path().join("duplicate-manifest"),
        ExtractionLimits::default(),
    )
    .expect_err("duplicate manifest");
    assert!(matches!(
        error,
        AssignmentPackageError::DuplicatePath { .. }
    ));

    let no_starter = [Entry::file("assignment.toml", MANIFEST.as_bytes())];
    let error = extract_assignment_package(
        Cursor::new(package(&no_starter)),
        &temp.path().join("missing-starter"),
        ExtractionLimits::default(),
    )
    .expect_err("missing starter files");
    assert!(matches!(error, AssignmentPackageError::MissingStarterFiles));
}

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rustrace-assignment-package-{}-{sequence}",
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

#[test]
fn extraction_returns_the_exact_validated_manifest_bytes_for_session_identity() {
    let root = TempRoot::new();
    let destination = root.path().join("exact-manifest");
    let bytes = package(&[
        Entry::file("assignment.toml", MANIFEST.as_bytes()),
        Entry::file("starter/Cargo.toml", b"[package]\n[workspace]\n"),
    ]);
    let extracted = extract_assignment_package(
        Cursor::new(bytes),
        &destination,
        ExtractionLimits::default(),
    )
    .unwrap();
    assert_eq!(extracted.manifest_bytes, MANIFEST.as_bytes());
}
