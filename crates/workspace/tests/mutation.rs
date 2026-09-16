#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustrace_model::WorkspacePath;
use rustrace_workspace::hash::{hash_entries, hash_workspace, read_workspace};
use rustrace_workspace::{
    WorkspaceMutationError, create_workspace_file, remove_workspace_file, rename_workspace_file,
    write_workspace_file,
};

fn path(value: &str) -> WorkspacePath {
    WorkspacePath::new(value).unwrap()
}

#[test]
fn read_workspace_returns_the_exact_tree_used_by_hashing() {
    let temp = TempRoot::new();
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::create_dir(temp.path().join("target")).unwrap();
    fs::write(temp.path().join("Cargo.toml"), b"[workspace]\n").unwrap();
    fs::write(temp.path().join("src/lib.rs"), b"pub fn answer() {}\n").unwrap();
    fs::write(temp.path().join("target/ignored"), b"ignored").unwrap();

    let expected = BTreeMap::from([
        (path("Cargo.toml"), b"[workspace]\n".to_vec()),
        (path("src/lib.rs"), b"pub fn answer() {}\n".to_vec()),
    ]);
    let actual = read_workspace(temp.path()).unwrap();

    assert_eq!(actual, expected);
    assert_eq!(
        hash_workspace(temp.path()).unwrap(),
        hash_entries(
            actual
                .iter()
                .map(|(path, contents)| (path, contents.as_slice()))
        )
        .unwrap()
    );
}

#[test]
fn create_write_rename_and_remove_are_descriptor_relative() {
    let temp = TempRoot::new();
    fs::create_dir(temp.path().join("src")).unwrap();
    let created = path("src/new.rs");
    let renamed = path("src/renamed.rs");

    create_workspace_file(temp.path(), &created).unwrap();
    assert_eq!(fs::read(temp.path().join(created.as_str())).unwrap(), b"");

    write_workspace_file(temp.path(), &created, b"fn main() {}\n").unwrap();
    assert_eq!(
        fs::read(temp.path().join(created.as_str())).unwrap(),
        b"fn main() {}\n"
    );

    rename_workspace_file(temp.path(), &created, &renamed).unwrap();
    assert!(!temp.path().join(created.as_str()).exists());
    assert_eq!(
        fs::read(temp.path().join(renamed.as_str())).unwrap(),
        b"fn main() {}\n"
    );

    remove_workspace_file(temp.path(), &renamed).unwrap();
    assert!(!temp.path().join(renamed.as_str()).exists());
}

#[test]
fn collision_and_missing_errors_leave_the_namespace_unchanged() {
    let temp = TempRoot::new();
    fs::create_dir(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/one.rs"), b"one").unwrap();
    fs::write(temp.path().join("src/two.rs"), b"two").unwrap();
    let before = read_workspace(temp.path()).unwrap();

    assert!(matches!(
        create_workspace_file(temp.path(), &path("src/one.rs")),
        Err(WorkspaceMutationError::PathCollision { .. })
    ));
    assert_eq!(read_workspace(temp.path()).unwrap(), before);

    assert!(matches!(
        create_workspace_file(temp.path(), &path("missing/new.rs")),
        Err(WorkspaceMutationError::MissingParent { .. })
    ));
    assert_eq!(read_workspace(temp.path()).unwrap(), before);

    assert!(matches!(
        rename_workspace_file(temp.path(), &path("src/one.rs"), &path("src/two.rs")),
        Err(WorkspaceMutationError::PathCollision { .. })
    ));
    assert_eq!(read_workspace(temp.path()).unwrap(), before);

    for operation in [
        remove_workspace_file(temp.path(), &path("src/missing.rs")),
        write_workspace_file(temp.path(), &path("src/missing.rs"), b"new"),
        rename_workspace_file(temp.path(), &path("src/missing.rs"), &path("src/new.rs")),
    ] {
        assert!(matches!(
            operation,
            Err(WorkspaceMutationError::MissingTarget { .. })
        ));
        assert_eq!(read_workspace(temp.path()).unwrap(), before);
    }
}

#[test]
fn symlinks_and_non_regular_targets_are_rejected_without_touching_them() {
    use std::os::unix::fs::symlink;

    let temp = TempRoot::new();
    let outside = temp.path().with_extension("outside");
    fs::write(&outside, b"outside").unwrap();
    fs::create_dir(temp.path().join("src")).unwrap();
    symlink(&outside, temp.path().join("src/link.rs")).unwrap();
    fs::create_dir(temp.path().join("src/directory.rs")).unwrap();

    for operation in [
        create_workspace_file(temp.path(), &path("src/link.rs")),
        write_workspace_file(temp.path(), &path("src/link.rs"), b"changed"),
        remove_workspace_file(temp.path(), &path("src/link.rs")),
        rename_workspace_file(temp.path(), &path("src/link.rs"), &path("src/renamed.rs")),
    ] {
        assert!(matches!(
            operation,
            Err(WorkspaceMutationError::Symlink { .. })
        ));
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert!(temp.path().join("src/link.rs").symlink_metadata().is_ok());
    }

    for operation in [
        write_workspace_file(temp.path(), &path("src/directory.rs"), b"changed"),
        remove_workspace_file(temp.path(), &path("src/directory.rs")),
        rename_workspace_file(
            temp.path(),
            &path("src/directory.rs"),
            &path("src/renamed.rs"),
        ),
    ] {
        assert!(matches!(
            operation,
            Err(WorkspaceMutationError::NotRegularFile { .. })
        ));
        assert!(temp.path().join("src/directory.rs").is_dir());
    }

    let _ = fs::remove_file(outside);
}

#[test]
fn symlink_parent_is_rejected_without_writing_outside_the_workspace() {
    use std::os::unix::fs::symlink;

    let temp = TempRoot::new();
    let outside = temp.path().with_extension("outside-dir");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, temp.path().join("linked")).unwrap();

    assert!(matches!(
        create_workspace_file(temp.path(), &path("linked/new.rs")),
        Err(WorkspaceMutationError::Symlink { .. })
    ));
    assert!(!outside.join("new.rs").exists());

    fs::remove_dir(outside).unwrap();
}

#[test]
fn save_replaces_a_workspace_hardlink_without_mutating_the_outside_inode() {
    let temp = TempRoot::new();
    let outside = temp.path().with_extension("outside-hardlink");
    fs::write(&outside, b"outside baseline").unwrap();
    fs::create_dir(temp.path().join("src")).unwrap();
    fs::hard_link(&outside, temp.path().join("src/lib.rs")).unwrap();

    write_workspace_file(temp.path(), &path("src/lib.rs"), b"workspace edit").unwrap();

    assert_eq!(
        fs::read(temp.path().join("src/lib.rs")).unwrap(),
        b"workspace edit"
    );
    assert_eq!(fs::read(&outside).unwrap(), b"outside baseline");

    fs::remove_file(outside).unwrap();
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rustrace-t3-3-mutation-tests-{}-{id}",
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
