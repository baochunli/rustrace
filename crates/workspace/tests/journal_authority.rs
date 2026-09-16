use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustrace_model::SessionId;
use rustrace_workspace::hash::PinnedWorkspaceRoot;

struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rustrace-writer-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn startup_inspection_is_owned_bounded_and_never_initializes_a_journal() {
    let root = Root::new();
    let pinned = PinnedWorkspaceRoot::open(root.path()).unwrap();
    let state = pinned.open_state_directory().unwrap();
    let database = root.path().join(".rustrace/partial.sqlite");
    fs::write(&database, b"not SQLite").unwrap();
    fs::write(root.path().join(".rustrace/.artifact-1-0"), b"partial").unwrap();
    let owner = state.lock_for_inspection().unwrap();
    assert!(owner.inventory(5, 10).is_err());
    assert!(owner.inventory(100, 1).is_err());
    let files = owner.inventory(100, 10).unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[1].0, "partial.sqlite");
    assert_eq!(files[1].1, 10);
    assert!(
        pinned
            .open_state_directory()
            .unwrap()
            .open_journal_file(&SessionId::new("partial").unwrap())
            .is_err()
    );
    assert_eq!(fs::read(&database).unwrap(), b"not SQLite");
    #[cfg(unix)]
    {
        let outside = root.path().join("outside");
        fs::write(&outside, b"preserve outside").unwrap();
        std::os::unix::fs::symlink(&outside, root.path().join(".rustrace/linked")).unwrap();
        assert!(owner.inventory(100, 10).is_err());
        assert_eq!(fs::read(outside).unwrap(), b"preserve outside");
    }
}

#[test]
fn only_one_cooperative_writer_can_create_a_journal_until_owner_drops() {
    let root = Root::new();
    let pinned = PinnedWorkspaceRoot::open(root.path()).unwrap();
    let first = pinned
        .open_state_directory()
        .unwrap()
        .create_journal_file(&SessionId::new("first").unwrap())
        .unwrap();
    let second = pinned
        .open_state_directory()
        .unwrap()
        .create_journal_file(&SessionId::new("second").unwrap());
    assert!(
        second.is_err(),
        "a second workspace writer acquired ownership"
    );
    assert!(!root.path().join(".rustrace/second.sqlite").exists());
    drop(first);
    let second = pinned
        .open_state_directory()
        .unwrap()
        .create_journal_file(&SessionId::new("second").unwrap())
        .unwrap();
    second.verify().unwrap();
}

#[cfg(unix)]
#[test]
fn writer_lock_symlink_is_rejected_without_touching_its_target() {
    let root = Root::new();
    let pinned = PinnedWorkspaceRoot::open(root.path()).unwrap();
    let state = pinned.open_state_directory().unwrap();
    let outside = root.path().join("outside");
    fs::write(&outside, b"preserve").unwrap();
    std::os::unix::fs::symlink(&outside, root.path().join(".rustrace/writer.lock")).unwrap();
    assert!(
        state
            .create_journal_file(&SessionId::new("blocked").unwrap())
            .is_err()
    );
    assert_eq!(fs::read(outside).unwrap(), b"preserve");
    assert!(!root.path().join(".rustrace/blocked.sqlite").exists());
}

#[test]
fn orphan_sqlite_sidecars_are_rejected_before_creating_the_main_database() {
    for suffix in ["-wal", "-shm", "-journal"] {
        let root = Root::new();
        let pinned = PinnedWorkspaceRoot::open(root.path()).unwrap();
        let state = pinned.open_state_directory().unwrap();
        let orphan = root.path().join(format!(".rustrace/orphan.sqlite{suffix}"));
        fs::write(&orphan, b"retained evidence").unwrap();
        assert!(
            state
                .create_journal_file(&SessionId::new("orphan").unwrap())
                .is_err(),
            "{suffix} collision was accepted"
        );
        assert_eq!(fs::read(orphan).unwrap(), b"retained evidence");
        assert!(!root.path().join(".rustrace/orphan.sqlite").exists());
    }
}

#[test]
fn reopening_requires_ownership_and_preserves_existing_bytes() {
    let root = Root::new();
    let pinned = PinnedWorkspaceRoot::open(root.path()).unwrap();
    let id = SessionId::new("resume").unwrap();
    let first = pinned
        .open_state_directory()
        .unwrap()
        .create_journal_file(&id)
        .unwrap();
    fs::write(first.display_path(), b"existing evidence").unwrap();
    assert!(
        pinned
            .open_state_directory()
            .unwrap()
            .open_journal_file(&id)
            .is_err()
    );
    drop(first);
    let reopened = pinned
        .open_state_directory()
        .unwrap()
        .open_journal_file(&id)
        .unwrap();
    reopened.verify().unwrap();
    assert_eq!(
        fs::read(reopened.display_path()).unwrap(),
        b"existing evidence"
    );
}

#[test]
fn owned_artifacts_are_bounded_no_clobber_and_no_follow() {
    let root = Root::new();
    let pinned = PinnedWorkspaceRoot::open(root.path()).unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .create_journal_file(&SessionId::new("artifacts").unwrap())
        .unwrap();
    owner
        .publish_artifact("metadata.json", b"first", false)
        .unwrap();
    assert!(
        owner
            .publish_artifact("metadata.json", b"second", false)
            .is_err()
    );
    assert_eq!(owner.read_artifact("metadata.json", 5).unwrap(), b"first");
    assert!(owner.read_artifact("metadata.json", 4).is_err());
    assert!(owner.publish_artifact("../escape", b"bad", false).is_err());
    owner
        .publish_artifact("metadata.json", b"next", true)
        .unwrap();
    assert_eq!(owner.read_artifact("metadata.json", 5).unwrap(), b"next");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            root.path().join("outside"),
            root.path().join(".rustrace/link"),
        )
        .unwrap();
        assert!(owner.publish_artifact("link", b"bad", true).is_err());
        assert!(!root.path().join("outside").exists());
    }
}

#[test]
fn storage_reserve_is_materialized_and_its_identity_is_retained() {
    let root = Root::new();
    let pinned = PinnedWorkspaceRoot::open(root.path()).unwrap();
    let mut owner = pinned
        .open_state_directory()
        .unwrap()
        .create_journal_file(&SessionId::new("reserve").unwrap())
        .unwrap();
    owner.secure_reserve(1024 * 1024, true).unwrap();
    let reserve = root.path().join(".rustrace/reserve.bin");
    assert_eq!(fs::metadata(&reserve).unwrap().len(), 1024 * 1024);
    fs::rename(&reserve, reserve.with_extension("preserved")).unwrap();
    fs::write(&reserve, b"replacement").unwrap();
    assert!(owner.verify().is_err());
}

#[cfg(unix)]
#[test]
fn reopen_preflights_sidecar_links_before_sqlite_can_touch_them() {
    let root = Root::new();
    let pinned = PinnedWorkspaceRoot::open(root.path()).unwrap();
    let id = SessionId::new("sidecar-reopen").unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .create_journal_file(&id)
        .unwrap();
    drop(owner);
    let sentinel = root.path().join("sentinel");
    fs::write(&sentinel, b"untouched").unwrap();
    std::os::unix::fs::symlink(
        &sentinel,
        root.path().join(".rustrace/sidecar-reopen.sqlite-wal"),
    )
    .unwrap();
    assert!(
        pinned
            .open_state_directory()
            .unwrap()
            .open_journal_file(&id)
            .is_err()
    );
    assert_eq!(fs::read(sentinel).unwrap(), b"untouched");
}
