#![cfg(unix)]

#[cfg(feature = "process-probes")]
use rustrace::session::ResumeChoice;
use rustrace::{
    cargo_policy::CargoAction,
    session::{ProductionSession, SessionBudgets},
};
use rustrace_model::{
    CommandOutcome, CommandTermination, ControlledAction, EditOrigin, Event, WorkspacePath,
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "formatter"
assignment_version = "v1"
title = "Formatter"
toolchain = "fixture"
edition = "2024"
allowed_paths = ["*.rs", "Cargo.lock"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

const REAL_MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "formatter-real"
assignment_version = "v1"
title = "Real Formatter"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["Cargo.toml", "src/*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

#[test]
fn formatter_provenance_fixture_parent() {
    for mode in [
        "noop",
        "changed",
        "multiple",
        "nonzero",
        "lock_change",
        "create",
        "delete",
        "rename",
        "invalid_utf8",
        "oversize",
        "transaction_oversize",
        "cancel_partial",
        "external_during",
        "metadata_during",
        "event_budget",
        "missing_formatter",
    ] {
        run_case(mode);
    }
}

#[test]
fn real_installed_formatter_changes_a_disposable_workspace_when_available() {
    let installed = ["cargo-fmt", "rustfmt"].into_iter().all(|tool| {
        Command::new("rustup")
            .args(["which", "--toolchain", "1.98.1", tool])
            .env("RUSTUP_AUTO_INSTALL", "0")
            .output()
            .is_ok_and(|output| output.status.success())
    });
    if !installed {
        eprintln!("real formatter fixture skipped: 1.98.1 rustfmt components unavailable");
        return;
    }

    let root = std::env::temp_dir().join(format!("rustrace-real-formatter-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        b"[package]\nname='formatter_fixture'\nversion='0.0.0'\nedition='2024'\n[workspace]\n",
    )
    .unwrap();
    fs::write(root.join("src/main.rs"), b"fn main(){println!(\"ok\");}\n").unwrap();
    let mut session = ProductionSession::start(&root, REAL_MANIFEST).unwrap();
    let id = session.session_id().clone();
    let before_versions = session
        .workspace()
        .checkpoint_input(id.clone())
        .unwrap()
        .documents
        .into_iter()
        .map(|document| (document.path, document.version))
        .collect::<std::collections::BTreeMap<_, _>>();
    session.start_command(CargoAction::Format).unwrap();
    assert!(wait_command(&mut session).is_none());
    assert_eq!(
        fs::read_to_string(root.join("src/main.rs")).unwrap(),
        "fn main() {\n    println!(\"ok\");\n}\n"
    );
    assert_eq!(
        session.command_outcome(),
        Some(&CommandOutcome::Exited { code: 0 })
    );
    session.quit().unwrap();
    assert_command_events(&root, &id, 1, 1, 1, 0, &before_versions);
    fs::remove_dir_all(root).unwrap();
}

fn run_case(mode: &str) {
    let (root, path) = prepare_case(mode);
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "formatter_provenance_fixture_child",
            "--nocapture",
        ])
        .env("RUSTRACE_FORMATTER_ROOT", &root)
        .env("RUSTRACE_FORMATTER_MODE", mode)
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "mode={mode}; fixture retained at {}; stdout={}; stderr={}",
        root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(root).unwrap();
}

fn prepare_case(mode: &str) -> (PathBuf, std::ffi::OsString) {
    prepare_case_named(mode, mode)
}

fn prepare_case_named(label: &str, mode: &str) -> (PathBuf, std::ffi::OsString) {
    let root = std::env::temp_dir().join(format!(
        "rustrace-formatter-red-{}-{label}",
        std::process::id(),
    ));
    fs::create_dir(&root).unwrap();
    let bin = root.join("target/bin");
    fs::create_dir_all(bin.join("v1")).unwrap();
    let script = include_bytes!("support/formatter_rustup.py");
    for path in std::iter::once(bin.join("rustup")).chain(
        ["rustc", "cargo", "rustdoc", "cargo-fmt", "rustfmt"].map(|name| bin.join("v1").join(name)),
    ) {
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(root.join("main.rs"), "fn main(){}\n").unwrap();
    fs::write(root.join("other.rs"), "pub fn answer()->u8{42}\n").unwrap();
    fs::write(root.join("Cargo.lock"), mode).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    (root, path)
}

#[cfg(feature = "process-probes")]
#[test]
fn formatter_interruption_preserves_each_per_document_prefix_and_links_recovery() {
    let main = WorkspacePath::new("main.rs").unwrap();
    let other = WorkspacePath::new("other.rs").unwrap();
    let original_main = b"fn main(){}\n".to_vec();
    let original_other = b"pub fn answer()->u8{42}\n".to_vec();
    let formatted_main = b"fn main() {}\n".to_vec();

    for stage in [
        "format-preflight",
        "format-intent",
        "format-disk",
        "format-baseline",
    ] {
        let (root, path) = prepare_case_named(&format!("multiple-{stage}"), "multiple");
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "formatter_provenance_fixture_child",
                "--nocapture",
            ])
            .env("RUSTRACE_FORMATTER_ROOT", &root)
            .env("RUSTRACE_FORMATTER_MODE", "multiple")
            .env("RUSTRACE_INTERRUPT_AT", stage)
            .env("PATH", path)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(83),
            "stage={stage}; root={}; stderr={}",
            root.display(),
            String::from_utf8_lossy(&output.stderr)
        );

        let marker: serde_json::Value = serde_json::from_slice(
            &fs::read(root.join(".rustrace/command-activity.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(marker["active"], true, "{stage}");
        let inspection = ProductionSession::inspect(&root).unwrap();
        let edit_durable = stage != "format-preflight";
        let disk_durable = matches!(stage, "format-disk" | "format-baseline");
        let baseline_durable = stage == "format-baseline";
        assert_eq!(
            inspection.logical[&main].as_slice(),
            if edit_durable {
                formatted_main.as_slice()
            } else {
                original_main.as_slice()
            },
            "{stage} logical"
        );
        assert_eq!(
            inspection.disk[&main].as_slice(),
            if disk_durable {
                formatted_main.as_slice()
            } else {
                original_main.as_slice()
            },
            "{stage} disk"
        );
        assert_eq!(
            inspection.saved[&main].as_slice(),
            if baseline_durable {
                formatted_main.as_slice()
            } else {
                original_main.as_slice()
            },
            "{stage} saved"
        );
        for view in [&inspection.logical, &inspection.saved, &inspection.disk] {
            assert_eq!(view[&other], original_other, "{stage} bounded prefix");
        }
        let error = ProductionSession::resume(&root, MANIFEST, ResumeChoice::Resume)
            .err()
            .expect("an interrupted Format must not be resumed as completed");
        assert!(error.to_string().contains("unfinished command"), "{error}");

        let fresh = std::env::temp_dir().join(format!(
            "rustrace-formatter-recovery-{}-{}",
            std::process::id(),
            stage
        ));
        fs::create_dir(&fresh).unwrap();
        fs::write(fresh.join("main.rs"), &original_main).unwrap();
        fs::write(fresh.join("other.rs"), &original_other).unwrap();
        fs::write(fresh.join("Cargo.lock"), b"multiple").unwrap();
        let recovery = ProductionSession::abandon_into(&root, &fresh, MANIFEST).unwrap();
        assert!(recovery.metadata().parent_evidence.is_some(), "{stage}");
        recovery.quit().unwrap();
        assert!(root.join(".rustrace/abandoned.json").exists(), "{stage}");
        assert_eq!(
            fs::read(root.join("main.rs")).unwrap(),
            inspection.disk[&main]
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(fresh).unwrap();
    }
}

#[test]
fn formatter_provenance_fixture_child() {
    let Some(root) = std::env::var_os("RUSTRACE_FORMATTER_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let mode = std::env::var("RUSTRACE_FORMATTER_MODE").unwrap();
    let original_main = fs::read(root.join("main.rs")).unwrap();
    let original_other = fs::read(root.join("other.rs")).unwrap();
    let mut session = ProductionSession::start(&root, MANIFEST).unwrap();
    let id = session.session_id().clone();
    let metadata = fs::read(root.join(".rustrace/session.json")).unwrap();
    let before_versions = session
        .workspace()
        .file_tree()
        .iter()
        .filter_map(|file| {
            file.document_id().map(|id| {
                (
                    file.path().clone(),
                    session
                        .workspace()
                        .checkpoint_input(id_session(&session))
                        .unwrap()
                        .documents
                        .into_iter()
                        .find(|document| document.document_id == *id)
                        .unwrap()
                        .version,
                )
            })
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    if mode == "event_budget" {
        let used = session.health().unwrap().events;
        session
            .set_budgets(SessionBudgets {
                events: used + 527,
                ..SessionBudgets::default()
            })
            .unwrap();
        let error = session.start_command(CargoAction::Format).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("insufficient event budget for command evidence"),
            "{error}"
        );
        assert!(!session.command_active());
        session.quit().unwrap();
        assert_command_events(&root, &id, 0, 0, 0, 0, &before_versions);
        return;
    }
    session.start_command(CargoAction::Format).unwrap();
    if ["cancel_partial", "external_during", "metadata_during"].contains(&mode.as_str()) {
        let started = root.join("target/format-started");
        let until = Instant::now() + Duration::from_secs(15);
        while !started.exists() && Instant::now() < until {
            session.poll_command().unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            started.exists(),
            "formatter child did not reach mutation fixture"
        );
        if mode == "cancel_partial" {
            session.cancel_command();
        } else if mode == "external_during" {
            fs::write(root.join("main.rs"), b"unowned live replacement").unwrap();
            fs::write(root.join("target/format-continue"), b"continue").unwrap();
        } else {
            fs::write(root.join(".rustrace/session.json"), b"tampered").unwrap();
            fs::write(root.join("target/format-continue"), b"continue").unwrap();
        }
    }
    let error = wait_command(&mut session);

    if mode == "metadata_during" {
        let error = error.expect("immutable metadata change must stop Format publication");
        assert!(error.contains("line 1"), "{error}");
        assert!(session.command_active());
        assert!(session.recovery_reason().is_some());
        assert_eq!(fs::read(root.join("main.rs")).unwrap(), original_main);
        return;
    }

    if mode == "missing_formatter" {
        assert!(error.is_some());
        assert!(!session.command_active());
        assert!(session.recovery_reason().is_none());
        assert_eq!(fs::read(root.join("main.rs")).unwrap(), original_main);
        session.quit().unwrap();
        assert_command_events(&root, &id, 0, 0, 0, 0, &before_versions);
        return;
    }

    let rejected = [
        "lock_change",
        "create",
        "delete",
        "rename",
        "invalid_utf8",
        "oversize",
        "transaction_oversize",
    ]
    .contains(&mode.as_str());
    assert_eq!(error.is_some(), rejected, "mode={mode}; error={error:?}");
    assert!(!session.command_active());
    assert!(session.recovery_reason().is_none());
    assert_eq!(
        fs::read(root.join(".rustrace/session.json")).unwrap(),
        metadata
    );
    assert_eq!(fs::read(root.join("Cargo.lock")).unwrap(), mode.as_bytes());

    let expected_count = match mode.as_str() {
        "changed" | "external_during" => 1,
        "multiple" => 2,
        _ => 0,
    };
    if expected_count > 0 {
        assert_eq!(fs::read(root.join("main.rs")).unwrap(), b"fn main() {}\n");
    } else {
        assert_eq!(fs::read(root.join("main.rs")).unwrap(), original_main);
    }
    if mode == "multiple" {
        assert_eq!(
            fs::read(root.join("other.rs")).unwrap(),
            b"pub fn answer() -> u8 { 42 }\n"
        );
    } else {
        assert_eq!(fs::read(root.join("other.rs")).unwrap(), original_other);
    }
    if mode == "cancel_partial" {
        assert!(matches!(
            session.command_outcome(),
            Some(CommandOutcome::Terminated {
                reason: CommandTermination::Cancelled,
                ..
            })
        ));
    } else {
        assert_eq!(
            session.command_outcome(),
            Some(&CommandOutcome::Exited {
                code: if mode == "nonzero" { 7 } else { 0 }
            })
        );
    }
    session.quit().unwrap();
    assert_command_events(
        &root,
        &id,
        1,
        1,
        expected_count,
        usize::from(mode == "external_during"),
        &before_versions,
    );
}

fn id_session(session: &ProductionSession) -> rustrace_model::SessionId {
    session.session_id().clone()
}

fn assert_command_events(
    root: &Path,
    id: &rustrace_model::SessionId,
    starts: usize,
    finishes: usize,
    formatter_edits: usize,
    external_observations: usize,
    before_versions: &std::collections::BTreeMap<WorkspacePath, u64>,
) {
    let pinned = rustrace_workspace::hash::PinnedWorkspaceRoot::open(root).unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .open_journal_file(id)
        .unwrap();
    let mut journal =
        rustrace_journal::Journal::open_read_only_no_follow(owner.display_path()).unwrap();
    let events = journal.read_events(id, 1, 1000).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::ControlledCommandStarted(_)))
            .count(),
        starts
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::ControlledCommandFinished(_)))
            .count(),
        finishes
    );
    let edits = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::FileEdited(transaction) if transaction.origin == EditOrigin::Formatter => {
                Some(transaction)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(edits.len(), formatter_edits);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::ExternalObservation(_)))
            .count(),
        external_observations
    );
    for transaction in edits {
        let path = events
            .iter()
            .find_map(|event| match &event.event {
                Event::ControlledCommandStarted(start)
                    if start.action == ControlledAction::Format =>
                {
                    Some(start)
                }
                _ => None,
            })
            .expect("format start");
        assert!(transaction.version_after == transaction.version_before + 1);
        assert!(
            before_versions
                .values()
                .any(|version| *version == transaction.version_before)
        );
        assert!(path.before.workspace_version <= path.before.checkpoint_sequence);
    }
    if starts == 1 {
        let state = fs::read_dir(root.join(".rustrace"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            state
                .iter()
                .any(|name| name.ends_with("-format-before.bin"))
        );
        let result_name = state
            .iter()
            .find(|name| name.ends_with("-format-result.json"))
            .expect("format decision artifact");
        let result: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join(".rustrace").join(result_name)).unwrap())
                .unwrap();
        assert_eq!(
            state
                .iter()
                .any(|name| name.ends_with("-format-returned.bin")),
            !result["returned_workspace_hash"].is_null(),
            "exact returned state exists iff the bounded reader accepted it"
        );
    }
}

fn wait_command(session: &mut ProductionSession) -> Option<String> {
    let until = Instant::now() + Duration::from_secs(15);
    while session.command_active() && Instant::now() < until {
        if let Err(error) = session.poll_command() {
            return Some(error.to_string());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    session
        .command_active()
        .then(|| "functional formatter fixture hang guard exceeded".into())
}
