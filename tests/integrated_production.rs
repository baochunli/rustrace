#![cfg(unix)]

#[path = "support/test_home.rs"]
mod test_home;
use rustrace::{
    cargo_policy::CargoAction,
    replay_tui::ReplayController,
    review_flags::{ReviewFlagKind, review_flags},
    session::{ProductionSession, ResumeChoice, SessionBudgets},
    tui::EditorCommand,
    verify::verify_path,
};
use rustrace_journal::{CheckpointFile, CheckpointSnapshot, OpenDocument, StoredCheckpoint};
use rustrace_model::{
    ControlledAction, DecodeOutcome, DecodePolicy, DocumentId, EditOrigin, EditorTransaction,
    Event, EventEnvelope, Hash, RPROV_FORMAT_VERSION_V1, RPROV_RECORD_HEADER_BYTES,
    RprovContainerHeader, RprovManifest, RprovRecordHeader, RprovRecordType, RprovSourceLink,
    SelectionState, SessionId, TextEdit, WorkspacePath, decode_envelope, document_hash,
    encode_envelope, encode_rprov_container_header, encode_rprov_record_header,
};
use rustrace_replay::ReplayEngine;
use rustrace_workspace::rprov_import::import_rprov;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "t8-7"
assignment_version = "v1"
title = "T8.7 integrated production"
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

const COMMAND_MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "t8-7-process"
assignment_version = "v1"
title = "T8.7 process indicators"
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

fn cli(args: &[&std::ffi::OsStr]) -> std::process::Output {
    let test_home = test_home::TestHome::new(false);
    test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .args(args)
        .output()
        .unwrap()
}

fn stored_zip_entry(archive: &[u8], wanted: &str) -> Vec<u8> {
    let mut offset = 0;
    while archive.get(offset..offset + 4) == Some(&0x0403_4b50_u32.to_le_bytes()) {
        let length = u32::from_le_bytes(archive[offset + 18..offset + 22].try_into().unwrap());
        let name_len =
            u16::from_le_bytes(archive[offset + 26..offset + 28].try_into().unwrap()) as usize;
        let extra_len =
            u16::from_le_bytes(archive[offset + 28..offset + 30].try_into().unwrap()) as usize;
        let name_start = offset + 30;
        let data_start = name_start + name_len + extra_len;
        let name = std::str::from_utf8(&archive[name_start..name_start + name_len]).unwrap();
        if name == wanted {
            return archive[data_start..data_start + length as usize].to_vec();
        }
        offset = data_start + length as usize;
    }
    panic!("missing stored ZIP entry {wanted}");
}

fn collect_rprov(bytes: &[u8]) -> (RprovManifest, BTreeMap<String, Vec<u8>>) {
    let imported = import_rprov(Cursor::new(bytes)).unwrap();
    let manifest = imported.manifest().clone();
    let mut payloads = BTreeMap::new();
    for entry in &manifest.inventory {
        let mut bytes = Vec::new();
        imported
            .open_entry(&entry.path)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        payloads.insert(entry.path.clone(), bytes);
    }
    (manifest, payloads)
}

fn write_record(output: &mut Vec<u8>, path: &str, payload: &[u8]) {
    let header = RprovRecordHeader {
        path_bytes: path.len() as u16,
        entry_type: RprovRecordType::RegularFile,
        payload_bytes: payload.len() as u64,
    };
    output.extend_from_slice(&encode_rprov_record_header(&header).unwrap());
    output.extend_from_slice(path.as_bytes());
    output.extend_from_slice(payload);
}

fn encode_rprov_unchecked(
    manifest: &RprovManifest,
    payloads: &BTreeMap<String, Vec<u8>>,
) -> Vec<u8> {
    let mut manifest_bytes = serde_json::to_vec(manifest).unwrap();
    manifest_bytes.push(b'\n');
    let mut records_bytes = RPROV_RECORD_HEADER_BYTES as u64
        + "manifest.json".len() as u64
        + manifest_bytes.len() as u64;
    for entry in &manifest.inventory {
        records_bytes +=
            RPROV_RECORD_HEADER_BYTES as u64 + entry.path.len() as u64 + entry.byte_length;
    }
    let header = RprovContainerHeader {
        format_version: RPROV_FORMAT_VERSION_V1,
        entry_count: (manifest.inventory.len() + 1) as u32,
        stored_records_bytes: records_bytes,
        expanded_records_bytes: records_bytes,
    };
    let mut output = encode_rprov_container_header(&header).unwrap().to_vec();
    write_record(&mut output, "manifest.json", &manifest_bytes);
    for entry in &manifest.inventory {
        write_record(&mut output, &entry.path, &payloads[&entry.path]);
    }
    output
}

fn case<'a>(summary: &'a [Value], name: &str) -> &'a Value {
    summary.iter().find(|case| case["name"] == name).unwrap()
}

fn assert_output(output: &std::process::Output, context: &str) {
    assert!(
        output.status.success(),
        "{context}: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn submit_verify_replay(root: &Path) -> rustrace::verify::VerificationReport {
    let work = root.join("assignment.work");
    let bundle = root.join("submission.zip");
    let submitted = cli(&[
        "submit".as_ref(),
        work.as_os_str(),
        "--student-id".as_ref(),
        "student-1".as_ref(),
        "--output".as_ref(),
        bundle.as_os_str(),
    ]);
    assert_output(&submitted, "submit");
    let verified = cli(&["verify".as_ref(), bundle.as_os_str()]);
    assert_output(&verified, "verify");
    fs::write(root.join("verify.txt"), &verified.stdout).unwrap();
    let scanned = cli(&["scan".as_ref(), root.as_os_str()]);
    assert_output(&scanned, "scan");
    fs::write(root.join("scan.txt"), &scanned.stdout).unwrap();
    let report = verify_path(&bundle, None);
    assert!(report.is_clean(), "{report:?}");
    assert!(
        ReplayController::open(&bundle)
            .unwrap()
            .timeline_available()
    );
    report
}

fn journal_events(workspace: &Path) -> Vec<Value> {
    let metadata: Value =
        serde_json::from_slice(&fs::read(workspace.join(".rustrace/session.json")).unwrap())
            .unwrap();
    let session = metadata["session_id"].as_str().unwrap();
    let connection =
        rusqlite::Connection::open(workspace.join(format!(".rustrace/{session}.sqlite"))).unwrap();
    let mut statement = connection
        .prepare("SELECT payload FROM events ORDER BY sequence")
        .unwrap();
    statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|row| serde_json::from_slice(&row.unwrap()).unwrap())
        .collect()
}

fn historical_checkpoint(initial: &str, selection: SelectionState) -> StoredCheckpoint {
    let session_id = SessionId::new("t8-7-historical").unwrap();
    let document_id = DocumentId::new("main").unwrap();
    let path = WorkspacePath::new("main.rs").unwrap();
    let snapshot = CheckpointSnapshot::new(
        session_id.clone(),
        1,
        vec![CheckpointFile {
            path: path.clone(),
            contents: initial.as_bytes().to_vec(),
        }],
        Some(document_id.clone()),
        vec![OpenDocument {
            document_id,
            path,
            version: 0,
            selection,
        }],
    )
    .unwrap();
    let owning_event = EventEnvelope {
        format_version: 1,
        session_id,
        sequence: 1,
        monotonic_millis: 0,
        wall_clock_utc: None,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event: Event::WorkspaceCheckpoint(snapshot.event_payload()),
    }
    .seal(Hash::zero())
    .unwrap();
    StoredCheckpoint {
        owning_event,
        snapshot,
    }
}

fn wait_for_command(session: &mut ProductionSession) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while session.command_active() && Instant::now() < deadline {
        session.poll_command().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!session.command_active(), "controlled fixture timed out");
}

fn process_suggestion_child(work: &Path) {
    let mut session = ProductionSession::start(work, COMMAND_MANIFEST).unwrap();
    for _ in 0..1_000 {
        session.execute(EditorCommand::Insert('x')).unwrap();
    }
    session.start_command(CargoAction::Check).unwrap();
    wait_for_command(&mut session);
    session.execute(EditorCommand::Insert('y')).unwrap();
    session.start_command(CargoAction::Check).unwrap();
    wait_for_command(&mut session);
    session.save_all().unwrap();
    session.quit().unwrap();
}

fn install_command_fixture(case: &Path) -> PathBuf {
    let work = case.join("assignment.work");
    let bin = case.join("bin");
    fs::create_dir_all(bin.join("v1")).unwrap();
    fs::create_dir(&work).unwrap();
    fs::create_dir(work.join("target")).unwrap();
    fs::write(work.join("main.rs"), "").unwrap();
    fs::write(work.join("Cargo.lock"), "lock fixture").unwrap();
    fs::write(
        work.join("target/runner-fixture.json"),
        br#"{"mode":"diagnostic","exit":101}"#,
    )
    .unwrap();
    let script = include_bytes!("support/command_rustup.py");
    let rustup = bin.join("rustup");
    fs::write(&rustup, script).unwrap();
    fs::set_permissions(&rustup, fs::Permissions::from_mode(0o755)).unwrap();
    for tool in [
        "rustc",
        "cargo",
        "rustdoc",
        "cargo-clippy",
        "cargo-fmt",
        "rustfmt",
    ] {
        let path = bin.join("v1").join(tool);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rustrace-t8-7-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(fs::canonicalize(root).unwrap())
    }
}

#[test]
fn p2_pty_driver_keeps_trailing_undo_redo_after_restoration() {
    let driver = include_str!("support/integrated_production_pty.py");
    let p2 = driver.split("        if p2:").nth(1).unwrap();
    let p2 = p2.split("        else:").next().unwrap();
    let receipt = p2.find("P2 restoration receipt").unwrap();
    assert!(
        p2[receipt..].contains(r#"send(b"\x1a\x19")"#),
        "P2 driver must exercise Undo/Redo after the rejected external change is restored"
    );
}

fn run_driver(fixture: &Fixture, mode: &str) -> std::process::Output {
    let test_home = test_home::TestHome::new(false);
    test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/integrated_production_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .arg(&fixture.0)
        .arg(mode)
        .output()
        .unwrap()
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn production_policy_precedes_search_prompt_at_80x24() {
    let fixture = Fixture::new("ingress");
    let output = run_driver(&fixture, "ingress-red");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn clipboard_p2_persistence_and_final_consumers_compose_at_80x24() {
    let fixture = Fixture::new("clipboard-p2-consumers");
    let output = run_driver(&fixture, "suite");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let summary: Vec<Value> =
        serde_json::from_slice(&fs::read(fixture.0.join("summary.json")).unwrap()).unwrap();
    assert_eq!(summary.len(), 11);
    assert_eq!(
        summary
            .iter()
            .filter(|case| case["paste_rejections"] == 1)
            .count(),
        8
    );
    assert_eq!(
        summary
            .iter()
            .filter(|case| case["internal_pastes"] == 1)
            .count(),
        2
    );
    assert_eq!(
        summary
            .iter()
            .filter(|case| case["external_observations"] == 1)
            .count(),
        1
    );

    for case in &summary {
        let bundle = PathBuf::from(case["bundle"].as_str().unwrap());
        let report = verify_path(&bundle, None);
        assert!(report.is_clean(), "{}: {report:?}", case["name"]);
        let replay = ReplayController::open(&bundle).unwrap();
        assert!(replay.timeline_available(), "{}", case["name"]);
        assert!(report.review_indicators.is_some(), "{}", case["name"]);
    }

    let copy = case(&summary, "copy-flow");
    let copy_bundle = PathBuf::from(copy["bundle"].as_str().unwrap());
    let copy_root = copy_bundle.parent().unwrap();
    let parent = copy_root.join("assignment.work");
    let assignment = copy_root.join("assignment.rta");
    let child = copy_root.join("linked.work");
    let latest = copy_root.join("latest-linked.zip");
    let revised = cli(&[
        "revise".as_ref(),
        parent.as_os_str(),
        child.as_os_str(),
        assignment.as_os_str(),
    ]);
    assert!(
        revised.status.success(),
        "{}",
        String::from_utf8_lossy(&revised.stdout)
    );
    let submitted = cli(&[
        "submit".as_ref(),
        child.as_os_str(),
        "--student-id".as_ref(),
        "student-1".as_ref(),
        "--output".as_ref(),
        latest.as_os_str(),
    ]);
    assert!(
        submitted.status.success(),
        "{}",
        String::from_utf8_lossy(&submitted.stdout)
    );
    let linked_report = verify_path(&latest, None);
    assert!(linked_report.is_clean(), "{linked_report:?}");
    let linked_rprov = stored_zip_entry(&fs::read(&latest).unwrap(), "session.rprov");
    let (mut linked_manifest, linked_payloads) = collect_rprov(&linked_rprov);
    assert_eq!(linked_manifest.segments.len(), 2);
    assert!(
        ReplayController::open(&latest)
            .unwrap()
            .timeline_available()
    );

    let links = &mut linked_manifest.segments[0].source_links;
    let link = links
        .iter()
        .position(|link| matches!(link, RprovSourceLink::InternalPaste { .. }))
        .expect("production internal paste source link");
    links.remove(link);
    let missing_source = copy_root.join("missing-source-link.rprov");
    fs::write(
        &missing_source,
        encode_rprov_unchecked(&linked_manifest, &linked_payloads),
    )
    .unwrap();
    assert!(
        !verify_path(&missing_source, None).is_clean(),
        "missing required internal source link passed clean verification"
    );

    let p2_bundle = PathBuf::from(case(&summary, "p2-separate")["bundle"].as_str().unwrap());
    let p2_rprov = stored_zip_entry(&fs::read(&p2_bundle).unwrap(), "session.rprov");
    let (mut p2_manifest, mut p2_payloads) = collect_rprov(&p2_rprov);
    let evidence = p2_manifest.segments[0]
        .evidence
        .pop()
        .expect("production P2 evidence");
    p2_manifest
        .inventory
        .retain(|entry| entry.path != evidence.entry);
    p2_payloads.remove(&evidence.entry);
    let missing_p2 = copy_root.join("missing-p2-evidence.rprov");
    fs::write(
        &missing_p2,
        encode_rprov_unchecked(&p2_manifest, &p2_payloads),
    )
    .unwrap();
    let missing_p2_report = verify_path(&missing_p2, None);
    assert!(!missing_p2_report.is_clean());
    assert!(
        review_flags(&missing_p2_report)
            .iter()
            .any(|flag| flag.kind == ReviewFlagKind::UnprovenancedExternalChange),
        "{missing_p2_report:?}"
    );
}

#[test]
fn completion_acceptance_staleness_and_failure_reach_final_consumers() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("completion-consumers");
    for (mode, expected) in [
        ("completion", (1, 1, 1)),
        ("completion_console_retirement", (1, 0, 0)),
        ("missing", (0, 0, 0)),
    ] {
        let root = fixture.0.join(mode);
        fs::create_dir(&root).unwrap();
        let output = test_home
            .command("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/lsp_work.py"
            ))
            .arg(env!("CARGO_BIN_EXE_rustrace"))
            .arg(mode)
            .arg(&root)
            .output()
            .unwrap();
        assert_output(&output, mode);
        fs::write(root.join("work-driver.stdout"), &output.stdout).unwrap();
        fs::write(root.join("work-driver.stderr"), &output.stderr).unwrap();

        let events = journal_events(&root.join("assignment.work"));
        let requested = events
            .iter()
            .filter(|event| event["event"]["type"] == "lsp_completion_requested")
            .count();
        let accepted = events
            .iter()
            .filter(|event| event["event"]["type"] == "lsp_completion_accepted")
            .count();
        let completion_edits = events
            .iter()
            .filter(|event| {
                event["event"]["type"] == "file_edited"
                    && event["event"]["payload"]["origin"] == "completion"
            })
            .count();
        assert_eq!((requested, accepted, completion_edits), expected, "{mode}");
        let report = submit_verify_replay(&root);
        let indicators = report.review_indicators.unwrap();
        assert_eq!(
            indicators.factual.allowed_internal_paste.transactions, 0,
            "{mode}"
        );
    }
}

#[test]
fn command_evidence_completeness_drives_exact_neutral_consumer_facts() {
    let test_home = test_home::TestHome::new(false);
    if let Some(work) = std::env::var_os("RUSTRACE_T87_PROCESS_CHILD") {
        process_suggestion_child(&PathBuf::from(work));
        return;
    }
    let fixture = Fixture::new("command-consumers");
    for mode in ["diagnostic", "missing", "read_failed", "cancel", "huge"] {
        let root = fixture.0.join(mode);
        let output = test_home
            .command("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/command_pty.py"
            ))
            .arg(env!("CARGO_BIN_EXE_rustrace"))
            .arg(mode)
            .env("RUSTRACE_T87_COMMAND_ROOT", &root)
            .output()
            .unwrap();
        assert_output(&output, mode);
        fs::write(root.join("driver.stdout"), &output.stdout).unwrap();
        fs::write(root.join("driver.stderr"), &output.stderr).unwrap();
        let report = submit_verify_replay(&root);
        let indicators = report.review_indicators.unwrap();
        let check = indicators
            .factual
            .commands
            .iter()
            .find(|command| command.action == ControlledAction::Check)
            .unwrap();
        assert_eq!(check.started, 1, "{mode}");
        if mode == "diagnostic" {
            assert_eq!(
                (
                    check.complete,
                    check.nonzero_exit,
                    check.compiler_errors,
                    check.unknown
                ),
                (1, 1, 1, 0)
            );
            assert!(matches!(
                indicators.attempts[0].outcome,
                rustrace::process_indicators::ProcessRuleOutcome::NotEligible { .. }
            ));
        } else {
            assert_eq!(
                (check.complete, check.compiler_errors, check.unknown),
                (0, 0, 1)
            );
            let rustrace::process_indicators::ProcessRuleOutcome::Unavailable { reason } =
                &indicators.attempts[0].outcome
            else {
                panic!("{mode}: incomplete evidence did not suppress the rule")
            };
            assert!(
                reason.contains(match mode {
                    "missing" => "missing diagnostics",
                    "cancel" => "cancellation",
                    "read_failed" | "huge" => "incomplete execution",
                    _ => unreachable!(),
                }),
                "{mode}: {reason}"
            );
        }
        let scan = fs::read_to_string(root.join("scan.txt")).unwrap();
        assert!(!scan.contains("score="), "{scan}");
        assert!(scan.contains("process-review-v1"), "{scan}");
        assert!(
            scan.matches("process-review-v1 attempt 1:").count() <= 1,
            "{scan}"
        );
    }

    let process_case = fixture.0.join("process-suggestion");
    fs::create_dir(&process_case).unwrap();
    let bin = install_command_fixture(&process_case);
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "command_evidence_completeness_drives_exact_neutral_consumer_facts",
            "--nocapture",
        ])
        .env(
            "RUSTRACE_T87_PROCESS_CHILD",
            process_case.join("assignment.work"),
        )
        .env("PATH", path)
        .env("RUSTUP_AUTO_INSTALL", "0")
        .output()
        .unwrap();
    assert_output(&child, "process-suggestion child");
    let report = submit_verify_replay(&process_case);
    let indicators = report.review_indicators.unwrap();
    assert_eq!(indicators.attempts.len(), 1);
    assert!(matches!(
        indicators.attempts[0].outcome,
        rustrace::process_indicators::ProcessRuleOutcome::Suggested
    ));
    let check = indicators
        .factual
        .commands
        .iter()
        .find(|command| command.action == ControlledAction::Check)
        .unwrap();
    assert_eq!(
        (
            check.started,
            check.complete,
            check.nonzero_exit,
            check.compiler_errors,
            check.unknown,
        ),
        (2, 2, 2, 2, 0)
    );
    let scan = fs::read_to_string(process_case.join("scan.txt")).unwrap();
    assert_eq!(scan.matches("Review suggested:").count(), 1, "{scan}");
    assert!(scan.contains("Planned or familiar work can show the same pattern"));
    assert!(scan.contains("do not establish transcription, AI use or intent"));
    assert!(!scan.contains("score="));
    assert!(!scan.to_lowercase().contains("probability"));
}

#[test]
fn rejection_budget_and_append_interruptions_preserve_then_reopen_cleanly() {
    let fixture = Fixture::new("rejection-interruptions");
    for mode in ["budget", "append"] {
        let root = fixture.0.join(mode);
        let work = root.join("assignment.work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("main.rs"), "canonical").unwrap();
        let mut session = ProductionSession::start(&work, MANIFEST).unwrap();
        session.execute(EditorCommand::SelectAll).unwrap();
        let before = (
            session.workspace().active_buffer().text(),
            session.workspace().active_buffer().version(),
            session.workspace().active_buffer().selection_state(),
            session.workspace().active_buffer().can_undo(),
        );
        let sentinel = format!("T87_{mode}_REJECTED_PAYLOAD");
        let journal_path = {
            let metadata = ProductionSession::read_metadata(&work).unwrap();
            work.join(format!(".rustrace/{}.sqlite", metadata.session_id))
        };
        let connection = (mode == "append").then(|| {
            let connection = rusqlite::Connection::open(&journal_path).unwrap();
            connection
                .execute_batch(
                    "CREATE TRIGGER fail_t87_rejection BEFORE INSERT ON events
                     BEGIN SELECT RAISE(ABORT, 'T8.7 append failure'); END;",
                )
                .unwrap();
            connection
        });
        if mode == "budget" {
            let events = session.health().unwrap().events;
            session
                .set_budgets(SessionBudgets {
                    events: events + 5,
                    ..SessionBudgets::default()
                })
                .unwrap();
        }
        if mode == "budget" {
            for _ in 0..3 {
                assert!(
                    session
                        .execute(EditorCommand::PasteExternal(sentinel.clone()))
                        .unwrap_err()
                        .to_string()
                        .contains("Paste blocked:")
                );
                assert!(session.recovery_reason().is_none());
            }
        }
        let error = session
            .execute(EditorCommand::PasteExternal(sentinel.clone()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("Paste blocked:"));
        assert!(!error.contains(&sentinel));
        assert_eq!(
            (
                session.workspace().active_buffer().text(),
                session.workspace().active_buffer().version(),
                session.workspace().active_buffer().selection_state(),
                session.workspace().active_buffer().can_undo(),
            ),
            before
        );
        assert_eq!(fs::read(work.join("main.rs")).unwrap(), b"canonical");
        assert!(session.recovery_reason().is_some());
        assert!(session.execute(EditorCommand::Insert('!')).is_err());
        drop(session);
        if let Some(connection) = connection {
            connection
                .execute_batch("DROP TRIGGER fail_t87_rejection")
                .unwrap();
        }
        assert!(
            !fs::read(&journal_path)
                .unwrap()
                .windows(sentinel.len())
                .any(|window| window == sentinel.as_bytes())
        );
        let resumed = ProductionSession::resume(&work, MANIFEST, ResumeChoice::Resume).unwrap();
        assert_eq!(resumed.workspace().active_buffer().text(), "canonical");
        resumed.quit().unwrap();
        let report = submit_verify_replay(&root);
        assert_eq!(
            report
                .review_indicators
                .unwrap()
                .factual
                .rejected_paste
                .attempts,
            if mode == "budget" { 3 } else { 0 },
            "failed metadata append cannot invent an accepted attempt"
        );
        assert!(
            !fs::read(root.join("submission.zip"))
                .unwrap()
                .windows(sentinel.len())
                .any(|window| window == sentinel.as_bytes())
        );
    }
}

#[test]
fn retained_t3_5_golden_paste_and_unknown_schema_replay_unchanged() {
    let cases: Vec<Value> = serde_json::from_slice(include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sessions/paste-provenance/cases.json"
    )))
    .unwrap();
    let case = cases
        .iter()
        .find(|case| case["name"] == "crlf_selection")
        .unwrap();
    let initial = case["initial"].as_str().unwrap();
    let inserted = case["inserted"].as_str().unwrap();
    let after = case["after"].as_str().unwrap();
    let start = case["start_byte"].as_u64().unwrap();
    let end = case["end_byte"].as_u64().unwrap();

    for origin in [EditOrigin::Paste, EditOrigin::Unknown] {
        let mut replay = ReplayEngine::from_initial_checkpoint(historical_checkpoint(
            initial,
            SelectionState::new(start, end),
        ))
        .unwrap();
        let event = EventEnvelope {
            format_version: 1,
            session_id: replay.session_id().clone(),
            sequence: replay.next_sequence(),
            monotonic_millis: 1,
            wall_clock_utc: None,
            previous_event_hash: replay.last_event_hash(),
            event_hash: Hash::zero(),
            event: Event::FileEdited(EditorTransaction {
                document_id: DocumentId::new("main").unwrap(),
                version_before: 0,
                version_after: 1,
                origin,
                edits: vec![TextEdit {
                    start_byte: start,
                    end_byte: end,
                    inserted_text: inserted.to_owned(),
                }],
                selection_before: SelectionState::new(start, end),
                selection_after: SelectionState::caret(start + inserted.len() as u64),
                hash_before: document_hash(initial),
                hash_after: document_hash(after),
            }),
        }
        .seal(replay.last_event_hash())
        .unwrap();
        let encoded = encode_envelope(&event).unwrap();
        let DecodeOutcome::Decoded(decoded) =
            decode_envelope(&encoded, DecodePolicy::RejectUnsupported).unwrap()
        else {
            panic!("historical schema event skipped")
        };
        assert_eq!(encode_envelope(&decoded).unwrap(), encoded);
        replay.apply(&decoded).unwrap();
        assert_eq!(
            replay
                .workspace_state()
                .file(&WorkspacePath::new("main.rs").unwrap())
                .unwrap(),
            after.as_bytes()
        );
    }
}

#[test]
fn restart_clears_authority_and_direct_commands_fail_closed_to_consumers() {
    let fixture = Fixture::new("restart-direct");
    let work = fixture.0.join("assignment.work");
    fs::create_dir(&work).unwrap();
    fs::write(work.join("a.rs"), "source").unwrap();
    fs::write(work.join("b.rs"), "destination").unwrap();
    let mut session = ProductionSession::start(&work, MANIFEST).unwrap();
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Copy).unwrap();
    session.quit().unwrap();

    let mut resumed = ProductionSession::resume(&work, MANIFEST, ResumeChoice::Resume).unwrap();
    resumed.execute(EditorCommand::NextBuffer).unwrap();
    resumed.execute(EditorCommand::SelectAll).unwrap();
    let before = (
        resumed.workspace().active_buffer().text(),
        resumed.workspace().active_buffer().version(),
        resumed.workspace().active_buffer().selection_state(),
        resumed.workspace().active_buffer().can_undo(),
    );
    for command in [
        EditorCommand::PasteExternal("source".into()),
        EditorCommand::Paste,
    ] {
        let error = resumed.execute(command).unwrap_err().to_string();
        assert!(error.contains("Paste blocked:"), "{error}");
        assert_eq!(
            (
                resumed.workspace().active_buffer().text(),
                resumed.workspace().active_buffer().version(),
                resumed.workspace().active_buffer().selection_state(),
                resumed.workspace().active_buffer().can_undo(),
            ),
            before
        );
    }
    resumed.quit().unwrap();
    let report = submit_verify_replay(&fixture.0);
    assert_eq!(
        report
            .review_indicators
            .unwrap()
            .factual
            .rejected_paste
            .attempts,
        2
    );
}
