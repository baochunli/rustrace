//! Deployed `test-cases/files/` trees are read back without following links
//! and hash exactly like the packaged tree they came from.
#![cfg(unix)]

use std::fs;
use std::io::Cursor;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustrace_workspace::assignment_package::{ExtractionLimits, extract_assignment_package};
use rustrace_workspace::fixture_tree::{
    FixtureTreeError, hash_deployed_fixture_tree, read_deployed_fixture_tree,
};
use rustrace_workspace::hash::PinnedWorkspaceRoot;

const MANIFEST: &str = r#"format_version = 3
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

fn deploy(root: &Path) -> PathBuf {
    let cases = root.join("test-cases");
    fs::create_dir_all(cases.join("files/src")).unwrap();
    fs::create_dir_all(cases.join("files/empty")).unwrap();
    fs::write(cases.join("files/src/lib.rs"), "fn a() {}\n").unwrap();
    fs::write(cases.join("files/notes.txt"), "fn\n").unwrap();
    fs::write(cases.join("case.expected"), "2\n").unwrap();
    fs::write(cases.join("case.args"), "fn\n").unwrap();
    cases
}

fn packaged_hash(root: &Path) -> rustrace_model::Hash {
    let entries: [(&str, &[u8]); 7] = [
        ("assignment.toml", MANIFEST.as_bytes()),
        ("starter/Cargo.toml", b"[package]\n[workspace]\n"),
        ("starter/src/main.rs", b"fn main() {}\n"),
        ("test-cases/case.expected", b"2\n"),
        ("test-cases/files/src/lib.rs", b"fn a() {}\n"),
        ("test-cases/files/notes.txt", b"fn\n"),
        ("test-cases/files/empty/", b""),
    ];
    let mut archive = Vec::new();
    for (path, contents) in entries {
        let mut header = [0_u8; 512];
        header[..path.len()].copy_from_slice(path.as_bytes());
        for (range, value) in [
            (100..108, 0o644),
            (108..116, 0),
            (116..124, 0),
            (124..136, contents.len() as u64),
            (136..148, 0),
        ] {
            let width = range.len() - 1;
            header[range].copy_from_slice(format!("{value:0width$o}\0").as_bytes());
        }
        header[148..156].fill(b' ');
        header[156] = if path.ends_with('/') { b'5' } else { b'0' };
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        archive.extend_from_slice(&header);
        archive.extend_from_slice(contents);
        archive.resize(archive.len().next_multiple_of(512), 0);
    }
    archive.resize(archive.len() + 1024, 0);
    extract_assignment_package(
        Cursor::new(archive),
        &root.join("extracted"),
        ExtractionLimits::default(),
    )
    .unwrap()
    .test_cases
    .unwrap()
    .fixtures
    .unwrap()
    .hash()
}

#[test]
fn deployed_tree_hashes_like_the_packaged_tree_and_detects_changes() {
    let temp = TempRoot::new();
    let cases = deploy(temp.path());
    let root = PinnedWorkspaceRoot::open(&cases).unwrap();
    let tree = read_deployed_fixture_tree(&root).unwrap().unwrap();
    assert_eq!(
        tree.directories()
            .map(|path| path.as_str())
            .collect::<Vec<_>>(),
        ["empty", "src"]
    );
    assert_eq!(
        tree.files()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>(),
        ["notes.txt", "src/lib.rs"]
    );
    let deployed = hash_deployed_fixture_tree(&root).unwrap().unwrap();
    assert_eq!(deployed, packaged_hash(temp.path()));

    fs::write(cases.join("files/notes.txt"), "changed\n").unwrap();
    assert_ne!(
        hash_deployed_fixture_tree(&root).unwrap().unwrap(),
        deployed
    );
    fs::write(cases.join("files/notes.txt"), "fn\n").unwrap();
    fs::write(cases.join("files/extra.txt"), "").unwrap();
    assert_ne!(
        hash_deployed_fixture_tree(&root).unwrap().unwrap(),
        deployed
    );
    fs::remove_file(cases.join("files/extra.txt")).unwrap();
    fs::remove_dir(cases.join("files/empty")).unwrap();
    assert_ne!(
        hash_deployed_fixture_tree(&root).unwrap().unwrap(),
        deployed
    );
    fs::create_dir(cases.join("files/empty")).unwrap();
    assert_eq!(
        hash_deployed_fixture_tree(&root).unwrap().unwrap(),
        deployed
    );
}

#[test]
fn absent_fixture_root_is_none_and_unsafe_entries_are_errors() {
    let temp = TempRoot::new();
    let cases = temp.path().join("test-cases");
    fs::create_dir(&cases).unwrap();
    fs::write(cases.join("case.expected"), "").unwrap();
    let root = PinnedWorkspaceRoot::open(&cases).unwrap();
    assert!(read_deployed_fixture_tree(&root).unwrap().is_none());

    fs::write(temp.path().join("outside.txt"), "outside\n").unwrap();
    symlink("../outside.txt", cases.join("files")).unwrap();
    let error = read_deployed_fixture_tree(&root).expect_err("symlinked root");
    assert!(
        matches!(error, FixtureTreeError::UnsupportedEntry { .. }),
        "{error}"
    );
    fs::remove_file(cases.join("files")).unwrap();
    fs::write(cases.join("files"), "not a directory").unwrap();
    assert!(read_deployed_fixture_tree(&root).is_err());
    fs::remove_file(cases.join("files")).unwrap();
    fs::create_dir(cases.join("files")).unwrap();

    type Setup = fn(&Path);
    let setups: [(&str, Setup); 5] = [
        ("symlink", |files| {
            symlink("../../outside.txt", files.join("link")).unwrap();
        }),
        ("directory symlink", |files| {
            symlink("..", files.join("parent")).unwrap();
        }),
        ("hard link", |files| {
            fs::write(files.join("a"), "a").unwrap();
            fs::hard_link(files.join("a"), files.join("b")).unwrap();
        }),
        ("FIFO", |files| {
            let status = std::process::Command::new("mkfifo")
                .arg(files.join("pipe"))
                .status()
                .unwrap();
            assert!(status.success());
        }),
        ("non-NFC name", |files| {
            fs::write(files.join("cafe\u{301}"), "x").unwrap();
        }),
    ];
    for (label, setup) in setups {
        let files = cases.join("files");
        fs::remove_dir_all(&files).unwrap();
        fs::create_dir(&files).unwrap();
        fs::create_dir(files.join("nested")).unwrap();
        setup(&files.join("nested"));
        let error = read_deployed_fixture_tree(&root).expect_err(label);
        if label == "non-NFC name" && !matches!(error, FixtureTreeError::UnsupportedEntry { .. }) {
            panic!("{label}: {error}");
        }
        assert!(
            error.to_string().contains("files/nested/"),
            "{label}: {error}"
        );
    }
    assert_eq!(
        fs::read(temp.path().join("outside.txt")).unwrap(),
        b"outside\n"
    );
}

#[test]
fn created_external_directories_are_removed_only_while_empty_and_unchanged() {
    use rustrace_model::WorkspacePath;
    use rustrace_workspace::{
        WorkspaceMutationError, create_external_directory_in, external_directory_exists_in,
        remove_created_external_directory_in,
    };

    let temp = TempRoot::new();
    let root = PinnedWorkspaceRoot::open(temp.path()).unwrap();
    let files = WorkspacePath::new("files").unwrap();
    let nested = WorkspacePath::new("files/nested").unwrap();
    assert!(matches!(
        create_external_directory_in(&root, &nested),
        Err(WorkspaceMutationError::MissingParent { .. })
    ));
    assert!(!external_directory_exists_in(&root, &files).unwrap());
    let identity = create_external_directory_in(&root, &files).unwrap();
    assert!(external_directory_exists_in(&root, &files).unwrap());
    assert!(matches!(
        create_external_directory_in(&root, &files),
        Err(WorkspaceMutationError::PathCollision { .. })
    ));

    fs::write(temp.path().join("files/kept.txt"), "kept").unwrap();
    assert!(!remove_created_external_directory_in(&root, &files, identity).unwrap());
    assert!(temp.path().join("files/kept.txt").exists());
    fs::remove_file(temp.path().join("files/kept.txt")).unwrap();

    // Keep the original alive so the replacement cannot reuse its inode.
    fs::rename(temp.path().join("files"), temp.path().join("files.old")).unwrap();
    fs::create_dir(temp.path().join("files")).unwrap();
    assert!(
        !remove_created_external_directory_in(&root, &files, identity).unwrap(),
        "a replacement directory is not the created one"
    );
    assert!(temp.path().join("files").is_dir());

    let replacement = WorkspacePath::new("replacement").unwrap();
    let identity = create_external_directory_in(&root, &replacement).unwrap();
    assert!(remove_created_external_directory_in(&root, &replacement, identity).unwrap());
    assert!(!temp.path().join("replacement").exists());

    symlink("files", temp.path().join("alias")).unwrap();
    assert!(matches!(
        external_directory_exists_in(&root, &WorkspacePath::new("alias").unwrap()),
        Err(WorkspaceMutationError::Symlink { .. })
    ));
    fs::write(temp.path().join("plain"), "").unwrap();
    assert!(matches!(
        external_directory_exists_in(&root, &WorkspacePath::new("plain").unwrap()),
        Err(WorkspaceMutationError::NotDirectory { .. })
    ));
}

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rustrace-fixture-tree-{}-{sequence}",
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
