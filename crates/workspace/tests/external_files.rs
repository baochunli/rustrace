#![cfg(unix)]

use rustrace_model::WorkspacePath;
use rustrace_workspace::hash::PinnedWorkspaceRoot;
use rustrace_workspace::hash::{MAX_WORKSPACE_DIRECTORIES, MAX_WORKSPACE_FILES};
use rustrace_workspace::{
    WorkspaceMutationError, create_external_regular_file_in, list_external_regular_files_in,
    open_external_regular_file_read_in, open_external_regular_file_write_in,
};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::symlink,
};

fn fixture(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "rustrace-external-files-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir(&path).unwrap();
    path
}

#[test]
fn descriptor_relative_external_files_are_bounded_regular_and_content_free() {
    let root = fixture("regular");
    fs::create_dir(root.join("nested")).unwrap();
    fs::write(root.join("input.txt"), b"input").unwrap();
    fs::write(root.join("nested/output.txt"), b"preserve").unwrap();
    let pinned = PinnedWorkspaceRoot::open(&root).unwrap();

    let paths = list_external_regular_files_in(&pinned).unwrap();
    assert_eq!(
        paths.iter().map(WorkspacePath::as_str).collect::<Vec<_>>(),
        ["input.txt", "nested/output.txt"]
    );

    let mut input =
        open_external_regular_file_read_in(&pinned, &WorkspacePath::new("input.txt").unwrap())
            .unwrap();
    let mut bytes = Vec::new();
    input.file_mut().read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"input");

    let mut output = open_external_regular_file_write_in(
        &pinned,
        &WorkspacePath::new("nested/output.txt").unwrap(),
    )
    .unwrap();
    assert_eq!(
        fs::read(root.join("nested/output.txt")).unwrap(),
        b"preserve"
    );
    output.file_mut().set_len(0).unwrap();
    output.file_mut().write_all(b"replacement").unwrap();
    assert_eq!(
        fs::read(root.join("nested/output.txt")).unwrap(),
        b"replacement"
    );

    let created =
        create_external_regular_file_in(&pinned, &WorkspacePath::new("nested/new.txt").unwrap())
            .unwrap();
    assert_eq!(created.file().metadata().unwrap().len(), 0);
    drop(created);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn external_open_rejects_symlink_hardlink_special_and_aliases_by_identity() {
    let root = fixture("unsafe");
    fs::write(root.join("ordinary"), b"value").unwrap();
    fs::hard_link(root.join("ordinary"), root.join("hard")).unwrap();
    symlink("ordinary", root.join("link")).unwrap();
    let pinned = PinnedWorkspaceRoot::open(&root).unwrap();

    for name in ["ordinary", "hard", "link"] {
        assert!(
            open_external_regular_file_read_in(&pinned, &WorkspacePath::new(name).unwrap())
                .is_err(),
            "unsafe alias accepted: {name}"
        );
    }
    let missing =
        open_external_regular_file_write_in(&pinned, &WorkspacePath::new("missing.txt").unwrap())
            .unwrap_err();
    assert!(matches!(
        missing,
        WorkspaceMutationError::MissingTarget { .. }
    ));
    assert!(list_external_regular_files_in(&pinned).unwrap().is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn external_listing_bounds_unsafe_entries_that_are_not_returned() {
    let root = fixture("unsafe-entry-bound");
    for index in 0..=MAX_WORKSPACE_DIRECTORIES + MAX_WORKSPACE_FILES {
        symlink("missing", root.join(format!("unsafe-{index:04}"))).unwrap();
    }
    let pinned = PinnedWorkspaceRoot::open(&root).unwrap();
    let error = list_external_regular_files_in(&pinned).unwrap_err();
    assert!(matches!(
        error,
        WorkspaceMutationError::ListingLimitExceeded { kind: "entry", .. }
    ));
    fs::remove_dir_all(root).unwrap();
}
