use std::ffi::OsString;
use std::fs::{self, File, FileTimes, Permissions};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use rustrace_model::{WorkspacePath, WorkspacePathError};
use rustrace_workspace::hash::{
    MAX_WORKSPACE_FILE_BYTES, MAX_WORKSPACE_FILES, MAX_WORKSPACE_TOTAL_BYTES, WorkspaceHashError,
    hash_entries, hash_workspace,
};

fn path(value: &str) -> WorkspacePath {
    WorkspacePath::new(value).unwrap()
}

#[test]
fn empty_and_multi_entry_trees_match_v1_golden_hashes() {
    let empty_encoding = [b"rustrace.workspace-tree\0v1\0".as_slice(), &[0, 0, 0, 0]].concat();
    assert_eq!(
        hash_entries(std::iter::empty()).unwrap().to_string(),
        "062264cae8486d3a5e62fec871ed7b06d620121a236243ee8ad55e437a5f3ee1"
    );
    assert_eq!(
        hash_entries(std::iter::empty()).unwrap().as_bytes(),
        blake3::hash(&empty_encoding).as_bytes()
    );

    let cargo = path("Cargo.toml");
    let source = path("src/lib.rs");
    let entries = [
        (&source, b"pub fn answer() -> u8 { 42 }\n".as_slice()),
        (
            &cargo,
            b"[package]\nname=\"demo\"\n[workspace]\n".as_slice(),
        ),
    ];
    let multi_encoding = [
        b"rustrace.workspace-tree\0v1\0".as_slice(),
        &[0, 0, 0, 2],
        &[0, 0, 0, 10],
        b"Cargo.toml",
        &[0, 0, 0, 0, 0, 0, 0, 34],
        b"[package]\nname=\"demo\"\n[workspace]\n",
        &[0, 0, 0, 10],
        b"src/lib.rs",
        &[0, 0, 0, 0, 0, 0, 0, 29],
        b"pub fn answer() -> u8 { 42 }\n",
    ]
    .concat();
    assert_eq!(
        hash_entries(entries).unwrap().to_string(),
        "54c4cf82dbbe7f56e583ba98960e3f73d437a0cca26819c3d8922283c2faccc1"
    );
    assert_eq!(
        hash_entries(entries).unwrap().as_bytes(),
        blake3::hash(&multi_encoding).as_bytes()
    );
}

#[test]
fn sorting_is_canonical_and_independent_of_input_order() {
    let ascii = path("z.rs");
    let multibyte = path("é.rs");
    let nested = path("src/a.rs");
    let forward = [
        (&multibyte, b"three".as_slice()),
        (&nested, b"two".as_slice()),
        (&ascii, b"one".as_slice()),
    ];
    let reverse = [forward[2], forward[1], forward[0]];

    assert_eq!(
        hash_entries(forward).unwrap(),
        hash_entries(reverse).unwrap()
    );
}

#[test]
fn path_content_and_framing_changes_change_the_hash() {
    let a = path("a");
    let ab = path("ab");
    let renamed = path("b");

    let base = hash_entries([(&a, b"bc".as_slice())]).unwrap();
    assert_ne!(base, hash_entries([(&renamed, b"bc".as_slice())]).unwrap());
    assert_ne!(base, hash_entries([(&a, b"bd".as_slice())]).unwrap());
    assert_ne!(base, hash_entries([(&ab, b"c".as_slice())]).unwrap());
    assert_ne!(
        hash_entries([(&a, b"b".as_slice()), (&renamed, b"c".as_slice())]).unwrap(),
        hash_entries([(&a, b"bb".as_slice()), (&renamed, b"".as_slice())]).unwrap()
    );
}

#[test]
fn workspace_path_validation_precedes_hashing() {
    let slash = path("src/lib.rs");
    assert!(hash_entries([(&slash, b"same".as_slice())]).is_ok());
    assert_eq!(
        WorkspacePath::new("src\\lib.rs"),
        Err(WorkspacePathError::ContainsBackslash)
    );

    assert!(WorkspacePath::new("cafe\u{301}.rs").is_err());
}

#[test]
fn pure_hash_rejects_duplicate_paths_and_fixed_limit_overruns() {
    let duplicate = path("same.rs");
    assert!(matches!(
        hash_entries([
            (&duplicate, b"first".as_slice()),
            (&duplicate, b"second".as_slice())
        ]),
        Err(WorkspaceHashError::DuplicatePath { .. })
    ));

    let maximum = vec![0_u8; MAX_WORKSPACE_FILE_BYTES as usize];
    assert!(hash_entries([(&duplicate, maximum.as_slice())]).is_ok());
    let oversized = vec![0_u8; MAX_WORKSPACE_FILE_BYTES as usize + 1];
    assert!(matches!(
        hash_entries([(&duplicate, oversized.as_slice())]),
        Err(WorkspaceHashError::FileSizeLimitExceeded { .. })
    ));

    let paths: Vec<_> = (0..=MAX_WORKSPACE_FILES)
        .map(|index| path(&format!("{index:03}.rs")))
        .collect();
    assert!(matches!(
        hash_entries(paths.iter().map(|path| (path, b"".as_slice()))),
        Err(WorkspaceHashError::FileCountLimitExceeded { .. })
    ));

    let ten = vec![0_u8; MAX_WORKSPACE_FILE_BYTES as usize];
    let total_paths: Vec<_> = (0..10)
        .map(|index| path(&format!("total-{index}.bin")))
        .collect();
    assert_eq!(10 * MAX_WORKSPACE_FILE_BYTES, MAX_WORKSPACE_TOTAL_BYTES);
    assert!(hash_entries(total_paths.iter().map(|path| (path, ten.as_slice()))).is_ok());
    let extra = path("total-extra.bin");
    assert!(matches!(
        hash_entries(
            total_paths
                .iter()
                .map(|path| (path, ten.as_slice()))
                .chain([(&extra, b"x".as_slice())])
        ),
        Err(WorkspaceHashError::TotalSizeLimitExceeded { .. })
    ));
}

#[test]
fn documented_exclusions_are_exact() {
    let temp = TempRoot::new();
    fs::create_dir_all(temp.path().join("target/deep")).unwrap();
    fs::create_dir_all(temp.path().join(".rustrace")).unwrap();
    fs::create_dir_all(temp.path().join("nested")).unwrap();
    fs::create_dir_all(temp.path().join("vendor/target")).unwrap();
    fs::write(temp.path().join("target/deep/ignored.rs"), "ignored").unwrap();
    fs::write(temp.path().join(".rustrace/provenance.db"), "ignored").unwrap();
    fs::write(
        temp.path()
            .join("nested/.rustrace-editor-0123456789abcdef0123456789abcdef.tmp"),
        "ignored",
    )
    .unwrap();
    for name in [".DS_Store", "Thumbs.db", "desktop.ini"] {
        fs::write(temp.path().join("nested").join(name), "ignored").unwrap();
    }

    let included = [
        ("target.rs", b"root source".as_slice()),
        ("vendor/target/lib.rs", b"nested target".as_slice()),
        ("nested/.DS_Store.rs", b"similar OS name".as_slice()),
        (
            "nested/.rustrace-editor-not-a-nonce.tmp",
            b"similar temp name".as_slice(),
        ),
        (".rustrace-notes", b"similar provenance name".as_slice()),
    ];
    for (name, contents) in included {
        let absolute = temp.path().join(name);
        fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        fs::write(absolute, contents).unwrap();
    }

    let expected_paths: Vec<_> = included.iter().map(|(name, _)| path(name)).collect();
    let expected = hash_entries(
        expected_paths
            .iter()
            .zip(included)
            .map(|(path, (_, contents))| (path, contents)),
    )
    .unwrap();
    assert_eq!(hash_workspace(temp.path()).unwrap(), expected);
}

#[test]
fn filesystem_hash_is_order_independent_and_matches_pure_hash() {
    let first = TempRoot::new();
    let second = TempRoot::new();
    let entries = [
        ("src/lib.rs", b"pub fn library() {}\n".as_slice()),
        ("Cargo.toml", b"[workspace]\n".as_slice()),
        ("tests/check.rs", b"#[test] fn check() {}\n".as_slice()),
    ];
    write_entries(first.path(), entries);
    write_entries(second.path(), [entries[2], entries[0], entries[1]]);

    let paths: Vec<_> = entries.iter().map(|(name, _)| path(name)).collect();
    let pure = hash_entries(
        paths
            .iter()
            .zip(entries)
            .map(|(path, (_, contents))| (path, contents)),
    )
    .unwrap();
    assert_eq!(hash_workspace(first.path()).unwrap(), pure);
    assert_eq!(hash_workspace(second.path()).unwrap(), pure);
}

#[test]
fn filesystem_metadata_and_empty_directories_do_not_contribute() {
    let temp = TempRoot::new();
    let file_path = temp.path().join("src/lib.rs");
    fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    fs::write(&file_path, "same bytes").unwrap();
    let before = hash_workspace(temp.path()).unwrap();

    fs::set_permissions(&file_path, Permissions::from_mode(0o600)).unwrap();
    File::options()
        .write(true)
        .open(&file_path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1)))
        .unwrap();
    fs::create_dir_all(temp.path().join("empty/deep/tree")).unwrap();

    assert_eq!(hash_workspace(temp.path()).unwrap(), before);
}

#[test]
fn filesystem_rejects_symlink_files_directories_and_excluded_names() {
    use std::os::unix::fs::symlink;

    for (link, target_is_dir) in [
        ("link.rs", false),
        ("linked-dir", true),
        ("target", true),
        (".rustrace", true),
        (".DS_Store", false),
    ] {
        let temp = TempRoot::new();
        let target = temp.path().join("real");
        if target_is_dir {
            fs::create_dir(&target).unwrap();
        } else {
            fs::write(&target, "bytes").unwrap();
        }
        symlink(&target, temp.path().join(link)).unwrap();

        assert!(matches!(
            hash_workspace(temp.path()),
            Err(WorkspaceHashError::Symlink { .. })
        ));
    }
}

#[test]
fn filesystem_rejects_special_files() {
    let temp = TempRoot::new();
    let _listener = UnixListener::bind(temp.path().join("service.socket")).unwrap();
    assert!(matches!(
        hash_workspace(temp.path()),
        Err(WorkspaceHashError::UnsupportedFileType { .. })
    ));
}

#[test]
fn filesystem_rejects_non_nfc_non_utf8_and_separator_alias_names() {
    let invalid_names = [
        OsString::from("cafe\u{301}.rs"),
        OsString::from_vec(vec![b'b', b'a', b'd', 0x80]),
        OsString::from("nested\\file.rs"),
    ];
    for invalid_name in invalid_names {
        let temp = TempRoot::new();
        let invalid_path = temp.path().join(invalid_name);
        if let Err(error) = fs::write(&invalid_path, "bytes") {
            assert_eq!(error.raw_os_error(), Some(92));
            continue;
        }
        assert!(matches!(
            hash_workspace(temp.path()),
            Err(WorkspaceHashError::InvalidPath { .. })
                | Err(WorkspaceHashError::UnrepresentablePath { .. })
        ));
    }
}

#[test]
fn filesystem_enforces_file_count_boundary() {
    let temp = TempRoot::new();
    for index in 0..MAX_WORKSPACE_FILES {
        fs::write(temp.path().join(format!("{index:03}.rs")), "").unwrap();
    }
    assert!(hash_workspace(temp.path()).is_ok());

    fs::write(temp.path().join("overflow.rs"), "").unwrap();
    assert!(matches!(
        hash_workspace(temp.path()),
        Err(WorkspaceHashError::FileCountLimitExceeded { .. })
    ));
}

#[test]
fn filesystem_hashes_many_empty_directories_with_depth_bounded_descriptors() {
    let temp = TempRoot::new();
    for index in 0..1_100 {
        fs::create_dir(temp.path().join(format!("empty-{index:04}"))).unwrap();
    }

    assert_eq!(
        hash_workspace(temp.path()).unwrap(),
        hash_entries(std::iter::empty()).unwrap()
    );
}

#[test]
fn filesystem_enforces_per_file_boundary() {
    let temp = TempRoot::new();
    let file = temp.path().join("large.bin");
    File::create(&file)
        .unwrap()
        .set_len(MAX_WORKSPACE_FILE_BYTES)
        .unwrap();
    assert!(hash_workspace(temp.path()).is_ok());

    File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_len(MAX_WORKSPACE_FILE_BYTES + 1)
        .unwrap();
    assert!(matches!(
        hash_workspace(temp.path()),
        Err(WorkspaceHashError::FileSizeLimitExceeded { .. })
    ));
}

#[test]
fn filesystem_enforces_total_size_boundary() {
    let temp = TempRoot::new();
    for index in 0..10 {
        File::create(temp.path().join(format!("{index}.bin")))
            .unwrap()
            .set_len(MAX_WORKSPACE_FILE_BYTES)
            .unwrap();
    }
    assert!(hash_workspace(temp.path()).is_ok());

    fs::write(temp.path().join("overflow.bin"), "x").unwrap();
    assert!(matches!(
        hash_workspace(temp.path()),
        Err(WorkspaceHashError::TotalSizeLimitExceeded { .. })
    ));
}

#[test]
fn filesystem_reports_invalid_roots() {
    let temp = TempRoot::new();
    let file = temp.path().join("file-root");
    fs::write(&file, "bytes").unwrap();
    assert!(matches!(
        hash_workspace(&file),
        Err(WorkspaceHashError::RootNotDirectory { .. })
    ));
    assert!(matches!(
        hash_workspace(&temp.path().join("missing")),
        Err(WorkspaceHashError::InvalidRoot { .. })
    ));
}

fn write_entries<const N: usize>(root: &Path, entries: [(&str, &[u8]); N]) {
    for (name, contents) in entries {
        let absolute = root.join(name);
        fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        fs::write(absolute, contents).unwrap();
    }
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rustrace-t1-3-hash-tests-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
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
            panic!("failed to remove {}: {error}", self.0.display());
        }
    }
}
