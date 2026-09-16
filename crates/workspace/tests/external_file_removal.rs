#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustrace_model::WorkspacePath;
use rustrace_workspace::hash::PinnedWorkspaceRoot;
use rustrace_workspace::{
    create_external_regular_file_in, remove_created_external_regular_file_in,
};

#[test]
fn created_identity_is_removed() {
    let temp = TempRoot::new();
    let root = PinnedWorkspaceRoot::open(temp.path()).unwrap();
    let path = WorkspacePath::new("created.in").unwrap();
    let created = create_external_regular_file_in(&root, &path).unwrap();
    let identity = created.identity();
    drop(created);

    assert!(remove_created_external_regular_file_in(&root, &path, identity).unwrap());
    assert!(!temp.path().join(path.as_str()).exists());
}

#[test]
fn replaced_identity_is_preserved() {
    let temp = TempRoot::new();
    let root = PinnedWorkspaceRoot::open(temp.path()).unwrap();
    let path = WorkspacePath::new("replaced.expected").unwrap();
    let created = create_external_regular_file_in(&root, &path).unwrap();
    let identity = created.identity();
    fs::remove_file(temp.path().join(path.as_str())).unwrap();
    fs::write(temp.path().join(path.as_str()), b"replacement").unwrap();

    assert!(!remove_created_external_regular_file_in(&root, &path, identity).unwrap());
    assert_eq!(
        fs::read(temp.path().join(path.as_str())).unwrap(),
        b"replacement"
    );
    drop(created);
}

#[test]
fn missing_path_is_unchanged() {
    let temp = TempRoot::new();
    let root = PinnedWorkspaceRoot::open(temp.path()).unwrap();
    let path = WorkspacePath::new("missing.in").unwrap();
    let created = create_external_regular_file_in(&root, &path).unwrap();
    let identity = created.identity();
    fs::remove_file(temp.path().join(path.as_str())).unwrap();

    assert!(!remove_created_external_regular_file_in(&root, &path, identity).unwrap());
    assert!(!temp.path().join(path.as_str()).exists());
    drop(created);
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rustrace-external-file-removal-{}-{id}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
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
