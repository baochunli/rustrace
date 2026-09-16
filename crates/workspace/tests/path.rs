use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustrace_model::WorkspacePath;
use rustrace_model::assignment::AssignmentManifest;
use rustrace_workspace::{
    AllowedPathSet, AllowedPathSetError, WorkspaceContainmentError, validate_workspace_path,
};

const MANIFEST: &str = r#"
format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Path tests"
toolchain = "1.85.0"
edition = "2024"
allowed_paths = ["Cargo.toml", "Cargo.lock", "src/**/*.rs", "tests/**/*.rs"]

[commands]
check = ["cargo", "check", "--locked"]
test = ["cargo", "test", "--locked"]
run = ["cargo", "run", "--locked"]
clippy = ["cargo", "clippy", "--locked", "--", "-D", "warnings"]
format = ["cargo", "fmt"]
"#;

fn manifest() -> AssignmentManifest {
    AssignmentManifest::parse(MANIFEST.as_bytes()).unwrap()
}

fn path(value: &str) -> WorkspacePath {
    WorkspacePath::new(value).unwrap()
}

#[test]
fn assignment_patterns_match_only_expected_canonical_paths() {
    let allowed = AllowedPathSet::from_manifest(&manifest()).unwrap();

    for candidate in [
        "Cargo.toml",
        "Cargo.lock",
        "src/lib.rs",
        "src/nested/module.rs",
        "tests/smoke.rs",
        "tests/nested/smoke.rs",
    ] {
        allowed.validate(&path(candidate)).unwrap();
    }

    for candidate in [
        "cargo.toml",
        "src",
        "src/lib.txt",
        "examples/demo.rs",
        "test/smoke.rs",
    ] {
        assert!(matches!(
            allowed.validate(&path(candidate)),
            Err(AllowedPathSetError::NotAllowed { .. })
        ));
    }
}

#[test]
fn assignment_patterns_match_nfc_unicode_literals() {
    let mut manifest = manifest();
    manifest.allowed_paths = vec!["src/**/caf\u{e9}*.rs".to_owned()];
    let allowed = AllowedPathSet::from_manifest(&manifest).unwrap();

    allowed
        .validate(&path("src/nested/caf\u{e9}_test.rs"))
        .unwrap();
    allowed.validate(&path("src/caf\u{e9}.rs")).unwrap();
}

#[test]
fn assignment_patterns_reject_backslashes_before_compilation() {
    for pattern in ["src\\**\\*.rs", "src\\*.rs", "Cargo\\.toml"] {
        let mut manifest = manifest();
        manifest.allowed_paths = vec![pattern.to_owned()];
        let error = AllowedPathSet::from_manifest(&manifest).unwrap_err();
        assert!(
            matches!(
                &error,
                AllowedPathSetError::InvalidPattern {
                    index: 0,
                    reason,
                    ..
                } if reason.contains("backslash")
            ),
            "unexpected error for `{pattern}`: {error}"
        );
    }
}

#[test]
fn rejects_glob_syntax_outside_the_assignment_pattern_language() {
    for pattern in [
        "src[!x]lib.rs",
        "src/?.rs",
        "src/caf\u{e9}?.rs",
        "src/{lib,main}.rs",
        "src/[a-z].rs",
        "src/file\\[x].rs",
        "src/file\\?.rs",
        "src/foo**/bar.rs",
        "src/**foo/bar.rs",
        "src/***.rs",
        "src/file**.rs",
    ] {
        let mut manifest = manifest();
        manifest.allowed_paths = vec![pattern.to_owned()];
        assert!(
            matches!(
                AllowedPathSet::from_manifest(&manifest),
                Err(AllowedPathSetError::InvalidPattern { index: 0, .. })
            ),
            "accepted policy-expanding pattern `{pattern}`"
        );
    }
}

#[test]
fn rejects_non_nfc_assignment_patterns() {
    let mut manifest = manifest();
    manifest.allowed_paths = vec!["src/cafe\u{301}*.rs".to_owned()];

    assert!(matches!(
        AllowedPathSet::from_manifest(&manifest),
        Err(AllowedPathSetError::InvalidPattern { index: 0, .. })
    ));
}

#[test]
fn component_star_does_not_cross_directories() {
    let mut manifest = manifest();
    manifest.allowed_paths = vec!["src/*.rs".to_owned()];
    let allowed = AllowedPathSet::from_manifest(&manifest).unwrap();

    allowed.validate(&path("src/lib.rs")).unwrap();
    assert!(matches!(
        allowed.validate(&path("src/nested/lib.rs")),
        Err(AllowedPathSetError::NotAllowed { .. })
    ));
}

#[test]
fn invalid_assignment_patterns_are_rejected_with_their_index() {
    for pattern in [
        "../outside.rs",
        "/tmp/*.rs",
        "C:*.rs",
        "src//*.rs",
        "src/[.rs",
        ".",
        "",
    ] {
        let mut manifest = manifest();
        manifest.allowed_paths = vec!["Cargo.toml".to_owned(), pattern.to_owned()];
        assert!(
            matches!(
                AllowedPathSet::from_manifest(&manifest),
                Err(AllowedPathSetError::InvalidPattern { index: 1, .. })
            ),
            "accepted pattern `{pattern}`"
        );
    }

    for pattern in [
        "x".repeat(rustrace_model::assignment::MAX_ALLOWED_PATH_BYTES + 1),
        std::iter::repeat_n("x", rustrace_model::MAX_WORKSPACE_PATH_DEPTH + 1)
            .collect::<Vec<_>>()
            .join("/"),
    ] {
        let mut manifest = manifest();
        manifest.allowed_paths = vec![pattern];
        assert!(matches!(
            AllowedPathSet::from_manifest(&manifest),
            Err(AllowedPathSetError::InvalidPattern { index: 0, .. })
        ));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn rejects_external_and_internal_symlinks_and_symlink_ancestors() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new();
    let root = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    fs::create_dir_all(root.join("src/real")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("outside.rs"), "outside").unwrap();
    fs::write(root.join("src/real/internal.rs"), "internal").unwrap();

    symlink(outside.join("outside.rs"), root.join("external.rs")).unwrap();
    symlink(root.join("src/real/internal.rs"), root.join("internal.rs")).unwrap();
    symlink(root.join("src/real"), root.join("src/internal-alias")).unwrap();
    symlink(&outside, root.join("src/linked-dir")).unwrap();

    for candidate in [
        "external.rs",
        "internal.rs",
        "src/internal-alias",
        "src/linked-dir/outside.rs",
    ] {
        assert!(matches!(
            validate_workspace_path(&root, &path(candidate)),
            Err(WorkspaceContainmentError::SymlinkComponent { .. })
        ));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn accepts_safe_existing_and_nonexistent_targets_below_the_real_root() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new();
    let root = temp.path().join("workspace");
    fs::create_dir_all(root.join("src/nested")).unwrap();
    fs::write(root.join("src/lib.rs"), "safe").unwrap();

    let canonical_root = fs::canonicalize(&root).unwrap();
    assert_eq!(
        validate_workspace_path(&root, &path("src/lib.rs")).unwrap(),
        canonical_root.join("src/lib.rs")
    );
    assert_eq!(
        validate_workspace_path(&root, &path("src/nested")).unwrap(),
        canonical_root.join("src/nested")
    );
    assert_eq!(
        validate_workspace_path(&root, &path("src/nested/new.rs")).unwrap(),
        canonical_root.join("src/nested/new.rs")
    );
    assert_eq!(
        validate_workspace_path(&root, &path("missing/child/new.rs")).unwrap(),
        canonical_root.join("missing/child/new.rs")
    );

    let linked_root = temp.path().join("linked-workspace");
    symlink(&root, &linked_root).unwrap();
    assert_eq!(
        validate_workspace_path(&linked_root, &path("src/lib.rs")).unwrap(),
        canonical_root.join("src/lib.rs")
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn rejects_a_root_that_is_not_a_directory_and_a_file_ancestor() {
    let temp = TempDir::new();
    let root_file = temp.path().join("not-a-directory");
    fs::write(&root_file, "file").unwrap();
    assert!(matches!(
        validate_workspace_path(&root_file, &path("src/lib.rs")),
        Err(WorkspaceContainmentError::RootNotDirectory { .. })
    ));

    let root = temp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("src"), "file").unwrap();
    assert!(matches!(
        validate_workspace_path(&root, &path("src/lib.rs")),
        Err(WorkspaceContainmentError::NotDirectory { .. })
    ));
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rustrace-t1-2-path-tests-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            panic!("failed to remove {}: {error}", self.0.display());
        }
    }
}
