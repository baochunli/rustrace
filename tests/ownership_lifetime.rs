#![cfg(any(target_os = "linux", target_os = "macos"))]

use rustrace::session::{ProductionSession, ResumeChoice};
use rustrace::tui::EditorCommand;
use rustrace_model::{SessionId, WorkspacePath};
use rustrace_workspace::hash::{PinnedStateInspection, PinnedWorkspaceRoot};
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rustrace-ownership-lifetime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn inspect(&self) -> Result<PinnedStateInspection, String> {
        PinnedWorkspaceRoot::open(&self.0)
            .and_then(|root| root.open_state_directory())
            .and_then(|state| state.lock_for_inspection())
            .map_err(|error| error.to_string())
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// The child has copied the parent's descriptors when its first FIFO open
// rendezvous completes. Its second open blocks before exec/CLOEXEC processing.
// Only the legitimate owner releases ownership; child cleanup never unlocks.
struct PreExecChild {
    directory: Directory,
    ready: std::fs::File,
    thread: Option<std::thread::JoinHandle<(i32, libc::pid_t)>>,
}
impl PreExecChild {
    fn start() -> Self {
        let directory = Directory::new();
        for name in ["ready", "gate"] {
            let path = CString::new(directory.0.join(name).as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: path is a live NUL-terminated string with no interior NUL.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }
        let ready_path = directory.0.join("ready");
        let gate_path = directory.0.join("gate");
        let thread = std::thread::spawn(move || {
            let ready = CString::new(ready_path.as_os_str().as_encoded_bytes()).unwrap();
            let gate = CString::new(gate_path.as_os_str().as_encoded_bytes()).unwrap();
            let executable = c"/usr/bin/true";
            // SAFETY: initialized actions and all argument storage stay alive
            // through spawn. The child runs only kernel file actions and exec.
            unsafe {
                let mut actions = std::mem::MaybeUninit::uninit();
                assert_eq!(libc::posix_spawn_file_actions_init(actions.as_mut_ptr()), 0);
                let mut actions = actions.assume_init();
                assert_eq!(
                    libc::posix_spawn_file_actions_addopen(
                        &mut actions,
                        500,
                        ready.as_ptr(),
                        libc::O_WRONLY,
                        0,
                    ),
                    0
                );
                assert_eq!(
                    libc::posix_spawn_file_actions_addopen(
                        &mut actions,
                        501,
                        gate.as_ptr(),
                        libc::O_RDONLY,
                        0,
                    ),
                    0
                );
                let mut pid = 0;
                let argv = [executable.as_ptr().cast_mut(), std::ptr::null_mut()];
                let env = [std::ptr::null_mut()];
                let result = libc::posix_spawn(
                    &mut pid,
                    executable.as_ptr(),
                    &actions,
                    std::ptr::null(),
                    argv.as_ptr(),
                    env.as_ptr(),
                );
                libc::posix_spawn_file_actions_destroy(&mut actions);
                (result, pid)
            }
        });
        let ready = fs::File::open(directory.0.join("ready")).unwrap();
        Self {
            directory,
            ready,
            thread: Some(thread),
        }
    }
}
impl Drop for PreExecChild {
    fn drop(&mut self) {
        // Always unblock/reap before a regression assertion can fail.
        let gate = fs::OpenOptions::new()
            .write(true)
            .open(self.directory.0.join("gate"))
            .unwrap();
        let (result, pid) = self.thread.take().unwrap().join().unwrap();
        assert_eq!(result, 0);
        let mut status = 0;
        // SAFETY: pid is the successfully spawned child; status is writable.
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert_eq!(status, 0);
        let _ = (&self.ready, gate);
    }
}

fn assert_retired_owner_releases(inspection: bool, unwind: bool) {
    let directory = Directory::new();
    let root = PinnedWorkspaceRoot::open(&directory.0).unwrap();
    let owner: Box<dyn std::any::Any> = if inspection {
        Box::new(directory.inspect().unwrap())
    } else {
        Box::new(
            root.open_state_directory()
                .unwrap()
                .create_journal_file(&SessionId::new("lifetime").unwrap())
                .unwrap(),
        )
    };
    let inode = fs::metadata(directory.0.join(".rustrace/writer.lock"))
        .unwrap()
        .ino();
    let child = PreExecChild::start();
    // Failed contenders must not unlock the still-live successful owner.
    assert!(directory.inspect().is_err());
    assert!(directory.inspect().is_err());
    if unwind {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _owner = owner;
            panic!("owned operation failed");
        }));
        assert!(result.is_err());
    } else {
        drop(owner);
    }
    let reacquired = directory.inspect().map(|_| ());
    let same_inode = fs::metadata(directory.0.join(".rustrace/writer.lock"))
        .unwrap()
        .ino();
    drop(child);
    assert_eq!(inode, same_inode, "writer lock inode must remain stable");
    assert!(
        reacquired.is_ok(),
        "owner retired before child exec: {reacquired:?}"
    );
    directory.inspect().unwrap().verify().unwrap();
}

#[test]
fn inspection_drop_releases_while_sibling_is_before_exec() {
    assert_retired_owner_releases(true, false);
}

#[test]
fn journal_drop_releases_while_sibling_is_before_exec() {
    assert_retired_owner_releases(false, false);
}

#[test]
fn inspection_unwind_releases_while_sibling_is_before_exec() {
    assert_retired_owner_releases(true, true);
}

#[test]
fn journal_unwind_releases_while_sibling_is_before_exec() {
    assert_retired_owner_releases(false, true);
}

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "fixture"
assignment_id = "ownership"
assignment_version = "v1"
title = "Ownership lifetime"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

fn assert_production_releases(quit: bool) {
    let directory = Directory::new();
    fs::write(directory.0.join("main.rs"), "A").unwrap();
    let mut session = ProductionSession::start(&directory.0, MANIFEST).unwrap();
    for _ in 0..100 {
        session.execute(EditorCommand::Insert('x')).unwrap();
    }
    assert!(session.tick().unwrap());
    assert!(session.health().unwrap().checkpoint_pending);
    let child = PreExecChild::start();
    assert!(ProductionSession::inspect(&directory.0).is_err());
    assert!(ProductionSession::resume(&directory.0, MANIFEST, ResumeChoice::Resume).is_err());
    if quit {
        session.quit().unwrap();
    } else {
        drop(session);
    }
    let inspection = ProductionSession::inspect(&directory.0);
    drop(child);
    let evidence = inspection.expect("retired production owner must release before sibling exec");
    assert_eq!(
        evidence.logical[&WorkspacePath::new("main.rs").unwrap()],
        format!("{}A", "x".repeat(100)).as_bytes()
    );
    // Retained inspection output must not retain authority; validate resume too.
    let resumed = ProductionSession::resume(&directory.0, MANIFEST, ResumeChoice::Resume).unwrap();
    resumed.quit().unwrap();
}

#[test]
fn production_quit_to_inspect_releases_while_sibling_is_before_exec() {
    assert_production_releases(true);
}

#[test]
fn production_drop_to_inspect_releases_while_sibling_is_before_exec() {
    assert_production_releases(false);
}

#[test]
fn failed_production_quit_releases_after_shutdown_while_sibling_is_before_exec() {
    let directory = Directory::new();
    fs::write(directory.0.join("main.rs"), "A").unwrap();
    let session = ProductionSession::start(&directory.0, MANIFEST).unwrap();
    let child = PreExecChild::start();
    // Force retained-authority verification to fail after worker shutdown.
    fs::rename(
        directory.0.join(".rustrace/reserve.bin"),
        directory.0.join(".rustrace/retained-reserve.bin"),
    )
    .unwrap();
    let quit = session.quit();
    let reacquired = directory.inspect().map(|_| ());
    drop(child);
    assert!(
        quit.is_err(),
        "failed owner verification must not report clean quit"
    );
    assert!(
        reacquired.is_ok(),
        "failed quit retained ownership: {reacquired:?}"
    );
}

#[test]
fn failed_inspection_operation_releases_while_sibling_is_before_exec() {
    let directory = Directory::new();
    let owner = directory.inspect().unwrap();
    let child = PreExecChild::start();
    let result = (move || {
        owner.read_artifact("missing-evidence", 16)?;
        Ok::<_, rustrace_workspace::hash::WorkspaceHashError>(())
    })();
    let reacquired = directory.inspect().map(|_| ());
    drop(child);
    assert!(result.is_err());
    assert!(
        reacquired.is_ok(),
        "failed inspection retained ownership: {reacquired:?}"
    );
}
