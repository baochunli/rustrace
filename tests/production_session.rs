#[cfg(all(unix, feature = "process-probes"))]
#[path = "support/test_home.rs"]
mod test_home;
use rustrace::{
    session::{ProductionSession, ResumeChoice},
    tui::EditorCommand,
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rustrace-production-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("main.rs"), "A").unwrap();
        Self(root)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn manifest() -> &'static [u8] {
    br#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Recovery"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["*.rs", "Cargo.toml"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#
}

#[test]
fn external_p2_dirty_save_restores_b_without_buffer_churn() {
    let dir = Directory::new();
    fs::write(dir.0.join("other.rs"), "other").unwrap();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    let id = session.session_id().clone();
    session.execute(EditorCommand::Insert('B')).unwrap();
    session.execute(EditorCommand::NextBuffer).unwrap();
    session.execute(EditorCommand::Insert('U')).unwrap();
    let unaffected_id = session.workspace().active_document_id().clone();
    session.execute(EditorCommand::PreviousBuffer).unwrap();
    let before = (
        session.workspace().active_buffer().version(),
        session.workspace().active_buffer().selection_state(),
        session.workspace().retained_undo_bytes(),
        session.workspace().active_document_id().clone(),
    );
    fs::write(dir.0.join("main.rs"), "C").unwrap();
    session.save_all().unwrap();
    assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"BA");
    assert_eq!(
        before,
        (
            session.workspace().active_buffer().version(),
            session.workspace().active_buffer().selection_state(),
            session.workspace().retained_undo_bytes(),
            session.workspace().active_document_id().clone()
        )
    );
    let evidence =
        rustrace::session::read_recovery_evidence(&fs::read(&session.evidence_paths()[0]).unwrap())
            .unwrap();
    let path = rustrace_model::WorkspacePath::new("main.rs").unwrap();
    assert_eq!(evidence.saved[&path], b"A");
    assert_eq!(evidence.logical[&path], b"BA");
    assert_eq!(evidence.disk[&path], b"C");
    assert!(!session.recheck_external().unwrap());
    session.execute(EditorCommand::NextBuffer).unwrap();
    assert_eq!(session.workspace().active_document_id(), &unaffected_id);
    assert_eq!(session.workspace().active_buffer().text(), "Uother");
    assert_eq!(session.workspace().active_buffer().version(), 1);
    assert!(session.workspace().active_buffer().can_undo());
    session.execute(EditorCommand::PreviousBuffer).unwrap();
    session.execute(EditorCommand::Undo).unwrap();
    assert_eq!(session.workspace().active_buffer().text(), "A");
    session.save_all().unwrap();
    session.quit().unwrap();
    let views = ProductionSession::inspect(&dir.0).unwrap();
    assert_eq!(views.disk, views.logical);
    assert_eq!(views.saved, views.logical);
    let root = rustrace_workspace::hash::PinnedWorkspaceRoot::open(&dir.0).unwrap();
    let owner = root
        .open_state_directory()
        .unwrap()
        .open_journal_file(&id)
        .unwrap();
    let mut journal =
        rustrace_journal::Journal::open_read_only_no_follow(owner.display_path()).unwrap();
    let events = journal.read_events(&id, 1, 100).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.event, rustrace_model::Event::ExternalObservation(_)))
            .count(),
        1
    );
    assert!(!events.iter().any(|e| matches!(
        &e.event,
        rustrace_model::Event::ExternalFileChange(_)
            | rustrace_model::Event::FileEdited(rustrace_model::EditorTransaction {
                origin: rustrace_model::EditOrigin::FileReload,
                ..
            })
    )));
}

#[test]
fn external_p2_canonical_presence_and_excluded_files() {
    let dir = Directory::new();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    fs::create_dir(dir.0.join("target")).unwrap();
    fs::write(dir.0.join("target/output.rs"), "excluded").unwrap();
    fs::write(dir.0.join("new.rs"), "rejected addition").unwrap();
    fs::remove_file(dir.0.join("main.rs")).unwrap();
    assert!(session.recheck_external().unwrap());
    assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"A");
    assert!(!dir.0.join("new.rs").exists());
    assert_eq!(
        fs::read(dir.0.join("target/output.rs")).unwrap(),
        b"excluded"
    );
    assert_eq!(session.workspace().file_tree().len(), 1);
    session.quit().unwrap();
    let views = ProductionSession::inspect(&dir.0).unwrap();
    assert_eq!(views.disk, views.logical);
}

#[test]
fn external_p2_valid_restart_automatically_restores_dirty_canonical() {
    let dir = Directory::new();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    session.execute(EditorCommand::Insert('B')).unwrap();
    session.quit().unwrap();
    fs::write(dir.0.join("main.rs"), "C").unwrap();
    let session = ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).unwrap();
    assert_eq!(session.workspace().active_buffer().text(), "BA");
    assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"BA");
    assert!(!session.external_pending());
}

#[test]
fn external_p2_out_of_policy_and_binary_stop_without_sweep() {
    for (path, bytes) in [
        ("notes.txt", b"unmanaged".as_slice()),
        ("main.rs", &[0xff, 0]),
    ] {
        let dir = Directory::new();
        let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
        fs::write(dir.0.join(path), bytes).unwrap();
        assert!(session.save_all().is_err());
        assert_eq!(fs::read(dir.0.join(path)).unwrap(), bytes);
        assert!(session.capture_boundary().is_err());
    }
}

#[test]
fn external_p2_corrupt_outcome_blocks_restart() {
    let dir = Directory::new();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    fs::write(dir.0.join("main.rs"), "C").unwrap();
    session.recheck_external().unwrap();
    session.quit().unwrap();
    let path = fs::read_dir(dir.0.join(".rustrace"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("restored-")
        })
        .unwrap();
    fs::write(path, b"corrupt outcome").unwrap();
    assert!(ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).is_err());
}

#[test]
fn external_p2_due_checkpoint_rechecks_without_waiting_for_poll() {
    let dir = Directory::new();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    for _ in 0..100 {
        session.execute(EditorCommand::Insert('x')).unwrap();
    }
    fs::write(dir.0.join("main.rs"), "C").unwrap();
    session.tick().unwrap();
    assert_eq!(
        fs::read(dir.0.join("main.rs")).unwrap(),
        session.workspace().active_buffer().text().as_bytes()
    );
}

#[cfg(all(unix, feature = "process-probes"))]
#[test]
fn real_cli_preserves_external_replacement_during_multi_file_save() {
    let test_home = test_home::TestHome::new(false);
    for external in [false, true] {
        let dir = Directory::new();
        fs::write(
            dir.0.join("Cargo.toml"),
            b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n",
        )
        .unwrap();
        for index in 0..29 {
            fs::write(dir.0.join(format!("f{index:02}.rs")), "A").unwrap();
        }
        let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
        session.execute(EditorCommand::NextBuffer).unwrap();
        for _ in 0..30 {
            session.execute(EditorCommand::Insert('B')).unwrap();
            session.execute(EditorCommand::NextBuffer).unwrap();
        }
        session.quit().unwrap();
        let output = test_home
            .command("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/live_save_pty.py"
            ))
            .arg(env!("CARGO_BIN_EXE_rustrace"))
            .arg(&dir.0)
            .arg(std::str::from_utf8(manifest()).unwrap())
            .arg(if external { "external" } else { "normal" })
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let session = ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).unwrap();
        assert_eq!(session.external_notice().is_some(), external);
        if external {
            let evidence = rustrace::session::read_recovery_evidence(
                &fs::read(&session.evidence_paths()[0]).unwrap(),
            )
            .unwrap();
            let path = rustrace_model::WorkspacePath::new("main.rs").unwrap();
            assert_eq!(evidence.saved[&path], b"A");
            assert_eq!(evidence.logical[&path], b"BA");
            assert_eq!(evidence.disk[&path], b"NEW EXTERNAL C");
        }
        session.quit().unwrap();
        let views = ProductionSession::inspect(&dir.0).unwrap();
        assert_eq!(views.disk, views.logical);
        assert_eq!(views.saved, views.logical);
    }
}

#[cfg(feature = "process-probes")]
#[test]
fn completed_outcome_followed_by_repeated_c_gets_fresh_identity() {
    for contents in ["C", "D", "BA"] {
        let dir = Directory::new();
        let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
        session.execute(EditorCommand::Insert('B')).unwrap();
        session.quit().unwrap();
        fs::write(dir.0.join("main.rs"), "C").unwrap();
        assert_eq!(
            interrupted_child(&dir, "external-outcome", None)
                .status
                .code(),
            Some(83)
        );
        fs::write(dir.0.join("main.rs"), contents).unwrap();
        for _ in 0..2 {
            let session =
                ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).unwrap();
            assert_eq!(session.workspace().active_buffer().text(), "BA");
            assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"BA");
            session.quit().unwrap();
        }
        let outcomes = fs::read_dir(dir.0.join(".rustrace"))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_str()
                    .unwrap()
                    .starts_with("restored-")
            })
            .count();
        assert_eq!(outcomes, if contents == "BA" { 1 } else { 2 });
    }
}

#[cfg(feature = "process-probes")]
#[test]
fn partial_owned_save_resumes_without_external_provenance() {
    for unexplained in [false, true] {
        let dir = Directory::new();
        fs::write(
            dir.0.join("Cargo.toml"),
            b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n",
        )
        .unwrap();
        for index in 0..29 {
            fs::write(dir.0.join(format!("f{index:02}.rs")), "A").unwrap();
        }
        let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
        session.execute(EditorCommand::NextBuffer).unwrap();
        for _ in 0..30 {
            session.execute(EditorCommand::Insert('B')).unwrap();
            session.execute(EditorCommand::NextBuffer).unwrap();
        }
        session.quit().unwrap();
        assert_eq!(
            interrupted_child(&dir, "own-save-file", None).status.code(),
            Some(83)
        );
        let published = fs::read_dir(&dir.0)
            .unwrap()
            .filter(|e| {
                let path = e.as_ref().unwrap().path();
                path.extension().is_some_and(|e| e == "rs") && fs::read(path).unwrap() == b"BA"
            })
            .count();
        assert_eq!(published, 1);
        if unexplained {
            fs::write(dir.0.join("main.rs"), "C").unwrap();
        } else {
            // Interrupted completion must retain the original owned intent,
            // without appending a resume event that invalidates its prefix.
            assert_eq!(
                interrupted_child(&dir, "external-file", None).status.code(),
                Some(83)
            );
        }
        let session = ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).unwrap();
        assert_eq!(session.external_notice().is_some(), unexplained);
        assert_eq!(!session.evidence_paths().is_empty(), unexplained);
        let id = session.session_id().clone();
        session.quit().unwrap();
        {
            let root = rustrace_workspace::hash::PinnedWorkspaceRoot::open(&dir.0).unwrap();
            let owner = root
                .open_state_directory()
                .unwrap()
                .open_journal_file(&id)
                .unwrap();
            let mut journal =
                rustrace_journal::Journal::open_read_only_no_follow(owner.display_path()).unwrap();
            let events = journal.read_events(&id, 1, 1000).unwrap();
            assert_eq!(
                events
                    .iter()
                    .any(|e| matches!(e.event, rustrace_model::Event::ExternalObservation(_))),
                unexplained
            );
        }
        let views = ProductionSession::inspect(&dir.0).unwrap();
        assert_eq!(views.disk, views.logical);
        assert_eq!(views.saved, views.logical);
        assert!(views.logical.values().all(|v| v == b"BA"));
        let session = ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).unwrap();
        assert!(session.external_notice().is_none());
    }
}

#[cfg(feature = "process-probes")]
#[test]
fn external_p2_interrupted_own_save_is_not_external_input() {
    let dir = Directory::new();
    ProductionSession::start(&dir.0, manifest())
        .unwrap()
        .quit()
        .unwrap();
    assert_eq!(
        interrupted_child(&dir, "disk", None).status.code(),
        Some(83)
    );
    let session = ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).unwrap();
    assert!(session.external_notice().is_none());
    assert!(session.evidence_paths().is_empty());
    assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"BA");
}

#[cfg(feature = "process-probes")]
#[test]
fn external_p2_process_interruption_retains_canonical_and_evidence() {
    for stage in [
        "external-evidence",
        "external-observation",
        "external-intent",
        "external-file",
        "external-disk",
        "external-checkpoint",
        "external-outcome",
        "external-baseline",
    ] {
        let dir = Directory::new();
        let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
        session.execute(EditorCommand::Insert('B')).unwrap();
        session.quit().unwrap();
        fs::write(dir.0.join("main.rs"), "C").unwrap();
        let output = interrupted_child(&dir, stage, None);
        assert_eq!(
            output.status.code(),
            Some(83),
            "{stage}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let before = ProductionSession::inspect(&dir.0).unwrap();
        assert_eq!(
            before.logical[&rustrace_model::WorkspacePath::new("main.rs").unwrap()],
            b"BA"
        );
        ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume)
            .unwrap()
            .quit()
            .unwrap();
        let after = ProductionSession::inspect(&dir.0).unwrap();
        assert_eq!(after.saved, after.logical);
        assert_eq!(after.disk, after.logical);
        assert!(fs::read_dir(dir.0.join(".rustrace")).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_str()
                .unwrap()
                .starts_with("evidence-")
        }));
    }
}

#[test]
fn damaged_declared_session_id_can_be_preserved_without_opening_its_journal() {
    let dir = Directory::new();
    ProductionSession::start(&dir.0, manifest())
        .unwrap()
        .quit()
        .unwrap();
    let path = dir.0.join(".rustrace/session.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut declared = metadata["session_id"].as_str().unwrap().to_owned();
    let last = declared.pop().unwrap();
    declared.push(if last == '0' { '1' } else { '0' });
    metadata["session_id"] = declared.clone().into();
    fs::write(path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    let state = dir.0.join(".rustrace");
    let before: Vec<_> = fs::read_dir(&state)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    assert!(ProductionSession::inspect(&dir.0).is_err());
    let fresh = Directory::new();
    ProductionSession::abandon_into(&dir.0, &fresh.0, manifest())
        .unwrap()
        .quit()
        .unwrap();
    for (path, bytes) in before {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    let link: serde_json::Value =
        serde_json::from_slice(&fs::read(fresh.0.join(".rustrace/parent.json")).unwrap()).unwrap();
    assert_eq!(link["original_session"], declared);
    assert_eq!(
        link["identity_status"],
        "metadata-declared; journal not validated"
    );
    assert!(
        link["state_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["name"] == "session.json")
    );
    let views = ProductionSession::inspect(&fresh.0).unwrap();
    assert_eq!(views.disk, views.saved);
    assert_eq!(views.saved, views.logical);
    assert_eq!(fs::read(fresh.0.join("main.rs")).unwrap(), b"A");
}

#[test]
fn dirty_quit_resumes_exact_logical_buffer_with_real_elapsed_offset() {
    let dir = Directory::new();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    let id = session.session_id().clone();
    assert!(ProductionSession::start(&dir.0, manifest()).is_err());
    std::thread::sleep(Duration::from_millis(20));
    session.execute(EditorCommand::Insert('B')).unwrap();
    let offset = session.persisted_millis();
    assert!(offset >= 20);
    session.quit().unwrap();
    assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"A");
    let mut resumed = ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).unwrap();
    assert_eq!(resumed.session_id(), &id);
    assert_eq!(resumed.workspace().active_buffer().text(), "BA");
    assert!(resumed.workspace().active_is_dirty());
    assert!(resumed.persisted_millis() >= offset);
    resumed.execute(EditorCommand::Insert('!')).unwrap();
    resumed.save_all().unwrap();
    assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"B!A");
    resumed.quit().unwrap();
}

#[test]
fn divergent_disk_is_preserved_and_automatically_restored_under_p2() {
    let dir = Directory::new();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    session.execute(EditorCommand::Insert('B')).unwrap();
    session.quit().unwrap();
    fs::write(dir.0.join("main.rs"), "C").unwrap();
    let mut restored = ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).unwrap();
    assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"BA");
    assert!(!restored.evidence_paths().is_empty());
    restored.execute(EditorCommand::Insert('!')).unwrap();
    restored.save_all().unwrap();
    restored.quit().unwrap();
}

#[test]
fn budgets_stop_mutation_without_pruning_and_expose_undo_eviction() {
    use rustrace::session::SessionBudgets;
    let dir = Directory::new();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    session
        .set_budgets(SessionBudgets {
            events: 8,
            undo_bytes: 1,
            ..SessionBudgets::default()
        })
        .unwrap();
    session.execute(EditorCommand::Insert('B')).unwrap();
    assert!(!session.workspace().active_buffer().can_undo());
    assert!(session.health().unwrap().undo_limited);
    for _ in 0..4 {
        session.execute(EditorCommand::Insert('x')).unwrap();
    }
    assert!(session.execute(EditorCommand::Insert('!')).is_err());
    assert!(session.recovery_reason().is_some());
    let text = session.workspace().active_buffer().text();
    session.quit().unwrap();
    let resumed = ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).unwrap();
    assert_eq!(resumed.workspace().active_buffer().text(), text);
    resumed.quit().unwrap();
}

#[test]
fn periodic_capture_skips_unchanged_state_but_explicit_boundary_is_retained() {
    let dir = Directory::new();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    let first = session.health().unwrap().events;
    assert!(!session.tick().unwrap());
    assert_eq!(session.health().unwrap().events, first);
    session.capture_boundary().unwrap();
    assert_eq!(session.health().unwrap().events, first + 1);
    for _ in 0..100 {
        session.execute(EditorCommand::Insert('x')).unwrap();
    }
    assert!(session.tick().unwrap());
    session.drain().unwrap();
    assert_eq!(session.health().unwrap().events, first + 102);
    assert!(!session.tick().unwrap());
    session.quit().unwrap();
}

#[test]
fn storage_reserve_failure_latches_without_publishing_source() {
    use rustrace::session::SessionBudgets;
    let dir = Directory::new();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    session
        .set_budgets(SessionBudgets {
            storage_bytes: 1,
            ..SessionBudgets::default()
        })
        .unwrap();
    assert!(session.execute(EditorCommand::Insert('!')).is_err());
    assert!(session.recovery_reason().is_some());
    assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"A");
    session.quit().unwrap();
}

#[test]
fn restart_retains_due_checkpoint_work_and_skips_an_unchanged_restart() {
    let dir = Directory::new();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    for _ in 0..100 {
        session.execute(EditorCommand::Insert('x')).unwrap();
    }
    session.quit().unwrap();
    let mut resumed = ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).unwrap();
    assert!(
        resumed.tick().unwrap(),
        "durable uncaptured edits remain due after restart"
    );
    resumed.drain().unwrap();
    resumed.quit().unwrap();
    let mut unchanged =
        ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).unwrap();
    assert!(!unchanged.tick().unwrap());
    unchanged.quit().unwrap();
}

#[test]
fn inspection_retains_all_three_views_and_validates_evidence_on_restart() {
    let dir = Directory::new();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    session.execute(EditorCommand::Insert('B')).unwrap();
    session.quit().unwrap();
    fs::write(dir.0.join("main.rs"), "C").unwrap();
    let inspect = ProductionSession::inspect(&dir.0).unwrap();
    let path = rustrace_model::WorkspacePath::new("main.rs").unwrap();
    assert_eq!(inspect.saved[&path], b"A");
    assert_eq!(inspect.logical[&path], b"BA");
    assert_eq!(inspect.disk[&path], b"C");
    let resumed =
        ProductionSession::resume(&dir.0, manifest(), ResumeChoice::RestoreLogical).unwrap();
    let evidence = resumed.evidence_paths()[0].clone();
    let exact = rustrace::session::read_recovery_evidence(&fs::read(&evidence).unwrap()).unwrap();
    assert_eq!(exact.saved[&path], b"A");
    assert_eq!(exact.logical[&path], b"BA");
    assert_eq!(exact.disk[&path], b"C");
    resumed.quit().unwrap();
    fs::write(&evidence, b"corrupt").unwrap();
    assert!(ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).is_err());
}

#[test]
fn abandoning_preserves_corrupt_original_and_links_new_session_before_genesis() {
    let dir = Directory::new();
    let session = ProductionSession::start(&dir.0, manifest()).unwrap();
    let old_id = session.session_id().clone();
    session.quit().unwrap();
    let journal = dir.0.join(format!(".rustrace/{old_id}.sqlite"));
    fs::write(&journal, b"corrupt original").unwrap();
    let fresh = Directory::new();
    let session = ProductionSession::abandon_into(&dir.0, &fresh.0, manifest()).unwrap();
    assert!(session.metadata().parent_evidence.is_some());
    assert_eq!(fs::read(&journal).unwrap(), b"corrupt original");
    assert_ne!(session.session_id(), &old_id);
    session.quit().unwrap();
    assert!(ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).is_err());
    ProductionSession::resume(&fresh.0, manifest(), ResumeChoice::Resume)
        .unwrap()
        .quit()
        .unwrap();
}

#[test]
fn recovered_syntax_tracks_the_restored_document_version() {
    use rustrace_editor::Movement;
    let dir = Directory::new();
    fs::write(dir.0.join("main.rs"), "fn main() {}\n").unwrap();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    session
        .execute(EditorCommand::Move {
            movement: Movement::DocumentEnd,
            selecting: false,
        })
        .unwrap();
    session.execute(EditorCommand::Insert(' ')).unwrap();
    session.quit().unwrap();
    let mut session = ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).unwrap();
    assert!(!session.workspace().active_highlights().is_empty());
    session.execute(EditorCommand::Insert(' ')).unwrap();
    assert_eq!(session.workspace().active_buffer().version(), 2);
    assert!(!session.workspace().active_highlights().is_empty());
    session.quit().unwrap();
}

#[test]
fn incomplete_startup_can_be_inspected_and_preserved_without_inventing_identity() {
    for metadata in [
        None,
        Some(b"{\"session_id\":".as_slice()),
        Some(b"not json".as_slice()),
    ] {
        let dir = Directory::new();
        let state = dir.0.join(".rustrace");
        fs::create_dir(&state).unwrap();
        fs::write(
            state.join("interrupted.sqlite"),
            b"uninitialized journal bytes",
        )
        .unwrap();
        fs::write(state.join("reserve.bin"), b"partial reserve").unwrap();
        fs::write(state.join(".artifact-1-0"), b"partial publication").unwrap();
        if let Some(bytes) = metadata {
            fs::write(state.join("session.json"), bytes).unwrap();
        }
        let evidence = ProductionSession::inspect_preserved(&dir.0).unwrap();
        assert!(evidence["original_session"].is_null());
        assert_eq!(
            evidence["identity_status"],
            "unknown; metadata missing or invalid"
        );
        assert!(ProductionSession::start(&dir.0, manifest()).is_err());
        let fresh = Directory::new();
        let mut session = ProductionSession::abandon_into(&dir.0, &fresh.0, manifest()).unwrap();
        let link: serde_json::Value =
            serde_json::from_slice(&fs::read(fresh.0.join(".rustrace/parent.json")).unwrap())
                .unwrap();
        assert!(link["original_session"].is_null());
        assert!(
            link["state_files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["name"] == ".artifact-1-0")
        );
        assert_eq!(
            fs::read(state.join("interrupted.sqlite")).unwrap(),
            b"uninitialized journal bytes"
        );
        assert_eq!(
            fs::read(state.join("reserve.bin")).unwrap(),
            b"partial reserve"
        );
        if let Some(bytes) = metadata {
            assert_eq!(fs::read(state.join("session.json")).unwrap(), bytes);
        }
        session.execute(EditorCommand::Insert('B')).unwrap();
        session.quit().unwrap();
        ProductionSession::resume(&fresh.0, manifest(), ResumeChoice::Resume)
            .unwrap()
            .quit()
            .unwrap();
    }
}

#[test]
fn incomplete_startup_preservation_respects_ownership_and_known_manifest_identity() {
    use rustrace_workspace::hash::PinnedWorkspaceRoot;
    let dir = Directory::new();
    let pinned = PinnedWorkspaceRoot::open(&dir.0).unwrap();
    let owner = pinned
        .open_state_directory()
        .unwrap()
        .create_journal_file(&rustrace_model::SessionId::new("startup").unwrap())
        .unwrap();
    owner
        .publish_artifact("manifest.toml", manifest(), false)
        .unwrap();
    assert!(ProductionSession::inspect_preserved(&dir.0).is_err());
    let fresh = Directory::new();
    assert!(ProductionSession::abandon_into(&dir.0, &fresh.0, manifest()).is_err());
    assert!(!fresh.0.join(".rustrace").exists());
    drop(owner);
    let wrong = String::from_utf8(manifest().to_vec())
        .unwrap()
        .replace("assignment_version = \"v1\"", "assignment_version = \"v2\"");
    assert!(ProductionSession::abandon_into(&dir.0, &fresh.0, wrong.as_bytes()).is_err());
    assert!(!fresh.0.join(".rustrace").exists());
    ProductionSession::abandon_into(&dir.0, &fresh.0, manifest())
        .unwrap()
        .quit()
        .unwrap();
}

#[cfg(feature = "process-probes")]
#[test]
fn process_probe_child() {
    let Ok(root) = std::env::var("RUSTRACE_PROBE_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let stage = std::env::var("RUSTRACE_INTERRUPT_AT").unwrap();
    if stage.starts_with("startup-") {
        ProductionSession::start(&root, manifest()).unwrap();
        panic!("startup interruption did not fire");
    }
    if stage == "ownership" {
        assert!(ProductionSession::resume(&root, manifest(), ResumeChoice::Resume).is_err());
        return;
    }
    if stage == "abandon" {
        ProductionSession::abandon_into(
            &root,
            &PathBuf::from(std::env::var("RUSTRACE_PROBE_NEW_ROOT").unwrap()),
            manifest(),
        )
        .unwrap();
        panic!("abandon interruption did not fire");
    }
    let mut session = ProductionSession::resume(&root, manifest(), ResumeChoice::Resume).unwrap();
    if stage != "own-save-file" {
        session.execute(EditorCommand::Insert('B')).unwrap();
    }
    session.save_all().unwrap();
    session.capture_boundary().unwrap();
    panic!("requested interruption did not fire");
}

#[cfg(feature = "process-probes")]
fn interrupted_child(
    dir: &Directory,
    stage: &str,
    fresh: Option<&Directory>,
) -> std::process::Output {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "process_probe_child", "--nocapture"])
        .env("RUSTRACE_PROBE_ROOT", &dir.0)
        .env("RUSTRACE_INTERRUPT_AT", stage);
    if let Some(fresh) = fresh {
        command.env("RUSTRACE_PROBE_NEW_ROOT", &fresh.0);
    }
    command.output().unwrap()
}

#[cfg(feature = "process-probes")]
#[test]
fn process_interruption_during_startup_retains_originals_and_allows_linked_preservation() {
    for stage in [
        "startup-journal",
        "startup-reserve",
        "startup-manifest",
        "startup-artifact",
        "startup-metadata",
        "startup-baseline",
        "startup-genesis",
    ] {
        let dir = Directory::new();
        let output = interrupted_child(&dir, stage, None);
        assert_eq!(
            output.status.code(),
            Some(83),
            "{stage}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let state = dir.0.join(".rustrace");
        let original = fs::read_dir(&state)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                (path.clone(), fs::read(path).unwrap())
            })
            .collect::<Vec<_>>();
        let evidence = ProductionSession::inspect_preserved(&dir.0).unwrap();
        if matches!(
            stage,
            "startup-journal" | "startup-reserve" | "startup-manifest" | "startup-artifact"
        ) {
            assert!(evidence["original_session"].is_null(), "{stage}");
        }
        if stage == "startup-artifact" {
            assert!(!state.join("session.json").exists());
            assert!(original.iter().any(|(p, b)| {
                p.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with(".artifact-")
                    && !b.is_empty()
                    && serde_json::from_slice::<serde_json::Value>(b).is_err()
            }));
        }
        let fresh = Directory::new();
        ProductionSession::abandon_into(&dir.0, &fresh.0, manifest())
            .unwrap()
            .quit()
            .unwrap();
        for (path, bytes) in original {
            assert_eq!(fs::read(path).unwrap(), bytes, "{stage}");
        }
        let inspection = ProductionSession::inspect(&fresh.0).unwrap();
        assert_eq!(inspection.saved, inspection.logical);
        assert_eq!(inspection.saved, inspection.disk);
        assert_eq!(
            inspection.logical[&rustrace_model::WorkspacePath::new("main.rs").unwrap()],
            b"A"
        );
    }
}

#[cfg(feature = "process-probes")]
#[test]
fn process_interruption_preserves_exact_prefix_at_each_production_boundary() {
    for stage in ["intent", "disk", "baseline", "checkpoint", "resume"] {
        let dir = Directory::new();
        ProductionSession::start(&dir.0, manifest())
            .unwrap()
            .quit()
            .unwrap();
        let output = interrupted_child(&dir, stage, None);
        assert_eq!(
            output.status.code(),
            Some(83),
            "stage {stage}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let inspection = ProductionSession::inspect(&dir.0).unwrap();
        let path = rustrace_model::WorkspacePath::new("main.rs").unwrap();
        let expected = if stage == "resume" {
            b"A".as_slice()
        } else {
            b"BA".as_slice()
        };
        assert_eq!(inspection.logical[&path], expected, "{stage}");
        let expected_disk = if matches!(stage, "intent" | "resume") {
            b"A".as_slice()
        } else {
            b"BA".as_slice()
        };
        assert_eq!(inspection.disk[&path], expected_disk, "{stage}");
        let expected_saved = if matches!(stage, "intent" | "resume" | "disk") {
            b"A".as_slice()
        } else {
            b"BA".as_slice()
        };
        assert_eq!(inspection.saved[&path], expected_saved, "{stage}");
        let choice = if stage == "disk" {
            ResumeChoice::RestoreLogical
        } else {
            ResumeChoice::Resume
        };
        let mut resumed = ProductionSession::resume(&dir.0, manifest(), choice).unwrap();
        resumed.execute(EditorCommand::Insert('!')).unwrap();
        resumed.save_all().unwrap();
        resumed.quit().unwrap();
    }
}

#[cfg(feature = "process-probes")]
#[test]
fn process_ownership_and_abandon_preservation_are_real_boundaries() {
    let dir = Directory::new();
    let session = ProductionSession::start(&dir.0, manifest()).unwrap();
    assert!(interrupted_child(&dir, "ownership", None).status.success());
    session.quit().unwrap();
    let fresh = Directory::new();
    assert_eq!(
        interrupted_child(&dir, "abandon", Some(&fresh))
            .status
            .code(),
        Some(83)
    );
    assert!(dir.0.join(".rustrace/abandoned.json").exists());
    let new = ProductionSession::resume(&fresh.0, manifest(), ResumeChoice::Resume).unwrap();
    assert!(new.metadata().parent_evidence.is_some());
    new.quit().unwrap();
}

#[test]
fn production_lifecycle_barriers_resume_without_false_divergence_or_reused_ids() {
    let dir = Directory::new();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    session.execute(EditorCommand::Insert('B')).unwrap();
    session.create_file("new.rs").unwrap();
    let created = session.workspace().active_document_id().clone();
    session.delete_selected().unwrap();
    session.quit().unwrap();
    let mut resumed = ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).unwrap();
    assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"BA");
    resumed.create_file("later.rs").unwrap();
    assert_ne!(resumed.workspace().active_document_id(), &created);
    resumed.quit().unwrap();
    ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume)
        .unwrap()
        .quit()
        .unwrap();
}

#[test]
fn resume_rejects_empty_database_without_initializing_or_repairing_it() {
    let dir = Directory::new();
    let session = ProductionSession::start(&dir.0, manifest()).unwrap();
    let id = session.session_id().clone();
    session.quit().unwrap();
    let database = dir.0.join(format!(".rustrace/{id}.sqlite"));
    for suffix in ["-wal", "-shm"] {
        fs::remove_file(dir.0.join(format!(".rustrace/{id}.sqlite{suffix}"))).unwrap();
    }
    fs::write(&database, []).unwrap();
    assert!(ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).is_err());
    assert!(
        fs::read(&database).unwrap().is_empty(),
        "recovery initialized an empty original"
    );
}

#[test]
fn periodic_capture_is_pending_until_its_actual_receipt_is_drained() {
    let dir = Directory::new();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    for _ in 0..100 {
        session.execute(EditorCommand::Insert('x')).unwrap();
    }
    let before = session.health().unwrap().events;
    assert!(session.tick().unwrap());
    let health = session.health().unwrap();
    assert!(health.checkpoint_pending);
    assert_eq!(
        health.events, before,
        "queue acceptance was labeled persisted"
    );
    session.drain().unwrap();
    assert_eq!(session.health().unwrap().events, before + 1);
    assert!(!session.health().unwrap().checkpoint_pending);
    session.quit().unwrap();
}

#[test]
fn binary_external_views_are_preserved_without_automatic_restoration_under_p2() {
    let dir = Directory::new();
    let bytes = String::from_utf8(manifest().to_vec()).unwrap().replace(
        "allowed_paths = [\"*.rs\", \"Cargo.toml\"]",
        "allowed_paths = [\"*.rs\", \"*.bin\", \"Cargo.toml\"]",
    );
    fs::write(dir.0.join("data.bin"), [0xff, 0x00]).unwrap();
    ProductionSession::start(&dir.0, bytes.as_bytes())
        .unwrap()
        .quit()
        .unwrap();
    fs::write(dir.0.join("data.bin"), [0xfe, 0x01]).unwrap();
    assert!(ProductionSession::resume(&dir.0, bytes.as_bytes(), ResumeChoice::Resume).is_err());
    assert_eq!(fs::read(dir.0.join("data.bin")).unwrap(), [0xfe, 0x01]);
    let path = fs::read_dir(dir.0.join(".rustrace"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("evidence-")
        })
        .unwrap();
    let evidence = rustrace::session::read_recovery_evidence(&fs::read(path).unwrap()).unwrap();
    let path = rustrace_model::WorkspacePath::new("data.bin").unwrap();
    assert_eq!(evidence.disk[&path], [0xfe, 0x01]);
    assert!(ProductionSession::resume(&dir.0, bytes.as_bytes(), ResumeChoice::Resume).is_err());
}

#[test]
fn saved_baseline_is_a_small_receipt_bound_to_the_durable_prefix() {
    let dir = Directory::new();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    session.execute(EditorCommand::Insert('B')).unwrap();
    session.save_all().unwrap();
    session.quit().unwrap();
    let receipt_path = dir.0.join(".rustrace/baseline.json");
    let bytes = fs::read(&receipt_path).unwrap();
    assert!(bytes.len() < 1024);
    let mut receipt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    receipt["event_hash"] = serde_json::Value::String("00".repeat(32));
    fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    assert!(ProductionSession::resume(&dir.0, manifest(), ResumeChoice::Resume).is_err());
    assert_eq!(fs::read(dir.0.join("main.rs")).unwrap(), b"BA");
}

#[test]
fn dropping_a_pending_session_keeps_ownership_until_the_writer_is_drained() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let dir = Directory::new();
    let mut session = ProductionSession::start(&dir.0, manifest()).unwrap();
    for _ in 0..100 {
        session.execute(EditorCommand::Insert('x')).unwrap();
    }
    let id = session.session_id().clone();
    // A lock-only test connection deterministically delays the accepted worker job.
    let connection =
        rusqlite::Connection::open(dir.0.join(format!(".rustrace/{id}.sqlite"))).unwrap();
    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(session.tick().unwrap());
    let released = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&released);
    let unblock = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        connection.execute_batch("COMMIT").unwrap();
        flag.store(true, Ordering::SeqCst);
    });
    drop(session);
    assert!(
        released.load(Ordering::SeqCst),
        "ownership was released while the accepted writer job was still blocked"
    );
    unblock.join().unwrap();
    let inspection = ProductionSession::inspect(&dir.0).unwrap();
    assert_eq!(
        inspection.logical[&rustrace_model::WorkspacePath::new("main.rs").unwrap()].len(),
        101
    );
}
