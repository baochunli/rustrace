#![cfg(unix)]

use rustrace::{
    cargo_policy::CargoAction,
    replay_tui::ReplayController,
    session::{ConsoleStart, ProductionSession, create_bundle},
    tui::EditorCommand,
    verify::{VerificationIssueKind, VerificationIssueLocation, VerificationStatus, verify_path},
};
use rustrace_journal::decode_checkpoint;
use rustrace_model::{
    CommandOutcome, ControlledAction, DecodeOutcome, DecodePolicy, EditOrigin, Event, Hash,
    RPROV_FORMAT_VERSION_V1, RPROV_RECORD_HEADER_BYTES, RprovContainerHeader, RprovEntryKind,
    RprovKnown, RprovManifest, RprovRecordHeader, RprovRecordType, SelectionState, TextEdit,
    decode_envelope, document_hash, encode_rprov_container_header, encode_rprov_manifest,
    encode_rprov_record_header, rprov_raw_blake3,
};
use rustrace_workspace::rprov_import::import_rprov;
use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "dependencies"
assignment_version = "v1"
title = "Dependencies"
toolchain = "fixture"
edition = "2024"
allowed_paths = ["Cargo.toml", "Cargo.lock", "src/*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

#[test]
fn dependency_actions_publish_exact_transactions_and_clean_consumers() {
    let (root, path) = prepare("clean");
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "dependency_provenance_child", "--nocapture"])
        .env("RUSTRACE_DEPENDENCY_ROOT", &root)
        .env("RUSTRACE_DEPENDENCY_MODE", "clean")
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture retained at {}; stdout={}; stderr={}",
        root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn failed_and_unauthorized_dependency_results_never_reach_the_live_workspace() {
    for mode in ["failure", "unauthorized"] {
        let (root, path) = prepare(mode);
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "dependency_provenance_child", "--nocapture"])
            .env("RUSTRACE_DEPENDENCY_ROOT", &root)
            .env("RUSTRACE_DEPENDENCY_MODE", mode)
            .env("PATH", path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "mode={mode}; root={}; stdout={}; stderr={}",
            root.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn resealed_dependency_source_edit_is_rejected_without_explained_indicators() {
    let (root, path) = prepare("forged-source");
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "dependency_provenance_child", "--nocapture"])
        .env("RUSTRACE_DEPENDENCY_ROOT", &root)
        .env("RUSTRACE_DEPENDENCY_MODE", "forged-source")
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture retained at {}; stdout={}; stderr={}",
        root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn resealed_invalid_dependency_argv_is_rejected_without_explained_indicators() {
    let (root, path) = prepare("forged-argv");
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "dependency_provenance_child", "--nocapture"])
        .env("RUSTRACE_DEPENDENCY_ROOT", &root)
        .env("RUSTRACE_DEPENDENCY_MODE", "forged-argv")
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture retained at {}; stdout={}; stderr={}",
        root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(root).unwrap();
}

#[cfg(feature = "process-probes")]
#[test]
fn dependency_interruption_preserves_each_lockfile_publication_prefix() {
    let lock = rustrace_model::WorkspacePath::new("Cargo.lock").unwrap();
    let manifest = rustrace_model::WorkspacePath::new("Cargo.toml").unwrap();
    for stage in [
        "dependency-preflight",
        "dependency-intent",
        "dependency-disk",
        "dependency-baseline",
    ] {
        let (root, path) = prepare(stage);
        let original = expected_files(&root);
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "dependency_provenance_child", "--nocapture"])
            .env("RUSTRACE_DEPENDENCY_ROOT", &root)
            .env("RUSTRACE_DEPENDENCY_MODE", "clean")
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
        let edit_durable = stage != "dependency-preflight";
        let disk_durable = matches!(stage, "dependency-disk" | "dependency-baseline");
        let baseline_durable = stage == "dependency-baseline";
        for (view, changed) in [
            (&inspection.logical, edit_durable),
            (&inspection.disk, disk_durable),
            (&inspection.saved, baseline_durable),
        ] {
            assert_eq!(
                view[&lock].as_slice(),
                if changed {
                    b"lock-after-add\n".as_slice()
                } else {
                    original[&lock].as_slice()
                },
                "{stage} lockfile prefix"
            );
            assert_eq!(
                view[&manifest], original[&manifest],
                "{stage} manifest prefix"
            );
        }
        let error =
            ProductionSession::resume(&root, MANIFEST, rustrace::session::ResumeChoice::Resume)
                .err()
                .expect("an interrupted dependency action must remain blocked");
        assert!(error.to_string().contains("unfinished command"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn dependency_provenance_child() {
    let Some(root) = std::env::var_os("RUSTRACE_DEPENDENCY_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let mode = std::env::var("RUSTRACE_DEPENDENCY_MODE").unwrap();
    fs::write(root.join("target/dependency-mode"), &mode).unwrap();
    let original = expected_files(&root);
    let mut session = ProductionSession::start(&root, MANIFEST).unwrap();
    let id = session.session_id().clone();
    assert!(
        session
            .workspace()
            .file_tree()
            .iter()
            .find(|file| file.path().as_str() == "Cargo.lock")
            .is_some_and(|file| !file.is_editable())
    );
    session.execute(EditorCommand::NextBuffer).unwrap();
    assert_ne!(session.workspace().active_path().as_str(), "Cargo.lock");
    session.execute(EditorCommand::NextBuffer).unwrap();
    assert_ne!(session.workspace().active_path().as_str(), "Cargo.lock");

    assert_eq!(
        session
            .start_console_command("cargo add serde@1.0.229")
            .unwrap(),
        ConsoleStart::Started
    );
    let error = wait_command(&mut session);
    if matches!(mode.as_str(), "failure" | "unauthorized") {
        if mode == "unauthorized" {
            assert!(
                error
                    .as_deref()
                    .is_some_and(|error| error.contains("unauthorized managed file")),
                "{error:?}"
            );
        } else {
            assert!(error.is_none(), "{error:?}");
            assert_eq!(
                session.command_outcome(),
                Some(&CommandOutcome::Exited { code: 7 })
            );
        }
        assert_eq!(session.workspace().logical_files().unwrap(), original);
        session.quit().unwrap();
        assert_eq!(dependency_edits(&root, &id).len(), 0);
        return;
    }

    assert!(error.is_none(), "{error:?}");
    session
        .start_dependency_command(CargoAction::Remove, Some("serde"))
        .unwrap();
    assert!(wait_command(&mut session).is_none());
    session
        .start_dependency_command(CargoAction::Update, None)
        .unwrap();
    assert!(wait_command(&mut session).is_none());

    let final_files = session.workspace().logical_files().unwrap();
    assert_eq!(
        final_files[&rustrace_model::WorkspacePath::new("Cargo.lock").unwrap()],
        b"lock-after-update\n"
    );
    let receipt = session.finalize("student-1").unwrap();
    assert_eq!(receipt.final_workspace(), &final_files);
    let bundle = root.join("dependency-submission.zip");
    create_bundle(&receipt, &bundle).unwrap();
    let report = verify_path(&bundle, None);
    assert!(report.is_clean(), "{report:?}");
    let attempt = &report.review_indicators.as_ref().unwrap().attempts[0];
    assert!(attempt.values.origins.dependency_tool > 0);
    assert_eq!(attempt.values.origins.unknown, 0);

    let edits = dependency_edits(&root, &id);
    assert_eq!(edits.len(), 5);
    assert!(
        edits
            .iter()
            .all(|origin| *origin == EditOrigin::DependencyTool)
    );
    let mut replay = ReplayController::open(&bundle).unwrap();
    let positions = replay.positions().collect::<Vec<_>>();
    let mut dependency_labels = 0;
    for position in &positions {
        replay.select(*position).unwrap();
        dependency_labels += usize::from(
            replay
                .timeline_rows(0)
                .first()
                .is_some_and(|row| row.event_name == "dependency tool"),
        );
    }
    assert_eq!(dependency_labels, edits.len());

    if mode == "forged-source" {
        let forged = reseal_dependency_edit_to_source(&bundle, &root);
        let report = verify_path(&forged, None);
        assert_eq!(report.replay, VerificationStatus::Failed, "{report:?}");
        assert!(
            report.review_indicators.is_none(),
            "an invalid source edit must never be reported as explained dependency provenance"
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.detail.contains("Cargo.toml or Cargo.lock")),
            "{report:?}"
        );
    } else if mode == "forged-argv" {
        let forged = reseal_dependency_start_argv(&bundle, &root);
        let report = verify_path(&forged, None);
        assert!(!report.is_clean(), "{report:?}");
        assert!(
            report.review_indicators.is_none(),
            "invalid dependency argv must never yield explained provenance"
        );
        assert_eq!(report.event_chain, VerificationStatus::Failed, "{report:?}");
        assert_eq!(report.issues.len(), 1, "{report:?}");
        assert_eq!(
            report.issues[0].kind,
            VerificationIssueKind::EventChain,
            "{report:?}"
        );
        assert_eq!(
            report.issues[0].location,
            VerificationIssueLocation::Decoder("events.jsonl envelope".to_owned()),
            "{report:?}"
        );
    }

    let last = replay.positions().last().unwrap();
    replay.select(last).unwrap();
    let replayed = replay
        .selected_event()
        .unwrap()
        .source
        .iter()
        .map(|(path, bytes)| {
            (
                rustrace_model::WorkspacePath::new(path).unwrap(),
                bytes.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(replayed, final_files);
}

fn reseal_dependency_edit_to_source(bundle: &Path, root: &Path) -> PathBuf {
    let rprov = stored_zip_entry(&fs::read(bundle).unwrap(), "session.rprov");
    let (manifest, payloads) = collect_rprov(&rprov);
    let checkpoint_path = manifest.segments[0].checkpoints[0].entry.clone();
    let checkpoint = decode_checkpoint(&payloads[&checkpoint_path]).unwrap();
    let source_id = checkpoint
        .documents()
        .iter()
        .find(|document| document.path.as_str() == "src/main.rs")
        .unwrap();
    let source_version = source_id.version;
    let source_id = source_id.document_id.clone();
    let source_before = checkpoint
        .files()
        .iter()
        .find(|file| file.path.as_str() == "src/main.rs")
        .and_then(|file| std::str::from_utf8(&file.contents).ok())
        .unwrap()
        .to_owned();
    let source_after = "fn forged_dependency_edit() {}\n";
    reseal_events(
        manifest,
        payloads,
        root,
        "dependency-source.rprov",
        move |event| {
            let Event::FileEdited(transaction) = event else {
                return false;
            };
            if transaction.origin != EditOrigin::DependencyTool {
                return false;
            }
            transaction.document_id = source_id.clone();
            transaction.version_before = source_version;
            transaction.version_after = source_version + 1;
            transaction.edits = vec![TextEdit {
                start_byte: 0,
                end_byte: source_before.len() as u64,
                inserted_text: source_after.to_owned(),
            }];
            transaction.selection_before = SelectionState::caret(0);
            transaction.selection_after = SelectionState::caret(0);
            transaction.hash_before = document_hash(&source_before);
            transaction.hash_after = document_hash(source_after);
            true
        },
    )
}

fn reseal_dependency_start_argv(bundle: &Path, root: &Path) -> PathBuf {
    let rprov = stored_zip_entry(&fs::read(bundle).unwrap(), "session.rprov");
    let (manifest, payloads) = collect_rprov(&rprov);
    reseal_events(manifest, payloads, root, "dependency-argv.rprov", |event| {
        let Event::ControlledCommandStarted(start) = event else {
            return false;
        };
        if start.action != ControlledAction::Add {
            return false;
        }
        start.argv[5] = "--git".to_owned();
        true
    })
}

fn unchecked_event_hash(previous: Hash, envelope: &rustrace_model::EventEnvelope) -> Hash {
    let material = format!(
        concat!(
            "{{\"format_version\":{},\"session_id\":{},\"sequence\":{},",
            "\"monotonic_millis\":{},\"wall_clock_utc\":{},\"event\":{}}}"
        ),
        envelope.format_version,
        serde_json::to_string(&envelope.session_id).unwrap(),
        envelope.sequence,
        envelope.monotonic_millis,
        serde_json::to_string(&envelope.wall_clock_utc).unwrap(),
        serde_json::to_string(&envelope.event).unwrap(),
    );
    let mut hasher = blake3::Hasher::new();
    hasher.update(previous.as_bytes());
    hasher.update(&(material.len() as u64).to_be_bytes());
    hasher.update(material.as_bytes());
    Hash::from_bytes(*hasher.finalize().as_bytes())
}

fn reseal_events(
    mut manifest: RprovManifest,
    mut payloads: BTreeMap<String, Vec<u8>>,
    root: &Path,
    output_name: &str,
    mut mutate: impl FnMut(&mut Event) -> bool,
) -> PathBuf {
    let event_path = manifest.segments[0].events.entry.clone();
    let mut changed = false;
    let mut previous = Hash::zero();
    let mut event_hashes = BTreeMap::new();
    let mut resealed = Vec::new();
    for line in payloads[&event_path].split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let DecodeOutcome::Decoded(mut envelope) =
            decode_envelope(line, DecodePolicy::RejectUnsupported).unwrap()
        else {
            panic!("production event must decode")
        };
        if !changed {
            changed = mutate(&mut envelope.event);
        }
        match &mut envelope.event {
            Event::ControlledCommandStarted(start) => {
                if let Some(hash) = event_hashes.get(&start.before.checkpoint_sequence) {
                    start.before.checkpoint_event_hash = *hash;
                }
            }
            Event::ControlledCommandFinished(finish) => {
                if let Some(hash) = event_hashes.get(&finish.after.checkpoint_sequence) {
                    finish.after.checkpoint_event_hash = *hash;
                }
            }
            _ => {}
        }
        envelope.previous_event_hash = previous;
        envelope.event_hash = unchecked_event_hash(previous, &envelope);
        previous = envelope.event_hash;
        event_hashes.insert(envelope.sequence, envelope.event_hash);
        resealed.extend_from_slice(&serde_json::to_vec(&envelope).unwrap());
        resealed.push(b'\n');
    }
    assert!(
        changed,
        "fixture must contain the selected dependency event"
    );
    manifest.segments[0].last_event_hash = previous;
    manifest.segments[0].terminal_event_hash = RprovKnown::Known { value: previous };
    for checkpoint in &mut manifest.segments[0].checkpoints {
        checkpoint.owner.event_hash = event_hashes[&checkpoint.owner.sequence];
    }
    replace_event_payload(&mut manifest, &mut payloads, &event_path, resealed);
    let output = root.join(output_name);
    fs::write(&output, encode_rprov(&manifest, &payloads)).unwrap();
    output
}

fn stored_zip_entry(archive: &[u8], wanted: &str) -> Vec<u8> {
    let mut offset = 0;
    while archive.get(offset..offset + 4) == Some(&0x0403_4b50_u32.to_le_bytes()) {
        let length = u32::from_le_bytes(archive[offset + 18..offset + 22].try_into().unwrap());
        let name_length =
            u16::from_le_bytes(archive[offset + 26..offset + 28].try_into().unwrap()) as usize;
        let extra_length =
            u16::from_le_bytes(archive[offset + 28..offset + 30].try_into().unwrap()) as usize;
        let name_start = offset + 30;
        let data_start = name_start + name_length + extra_length;
        let name = std::str::from_utf8(&archive[name_start..name_start + name_length]).unwrap();
        if name == wanted {
            return archive[data_start..data_start + length as usize].to_vec();
        }
        offset = data_start + length as usize;
    }
    panic!("missing stored ZIP entry {wanted}")
}

fn collect_rprov(rprov: &[u8]) -> (RprovManifest, BTreeMap<String, Vec<u8>>) {
    let imported = import_rprov(Cursor::new(rprov)).unwrap();
    let manifest = imported.manifest().clone();
    let payloads = manifest
        .inventory
        .iter()
        .map(|entry| {
            let mut bytes = Vec::new();
            imported
                .open_entry(&entry.path)
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
            (entry.path.clone(), bytes)
        })
        .collect();
    (manifest, payloads)
}

fn replace_event_payload(
    manifest: &mut RprovManifest,
    payloads: &mut BTreeMap<String, Vec<u8>>,
    path: &str,
    bytes: Vec<u8>,
) {
    let digest = rprov_raw_blake3(&bytes);
    let length = bytes.len() as u64;
    let inventory = manifest
        .inventory
        .iter_mut()
        .find(|entry| entry.path == path)
        .unwrap();
    assert_eq!(inventory.kind, RprovEntryKind::Events);
    inventory.blake3 = digest;
    inventory.byte_length = length;
    manifest.segments[0].events.blake3 = digest;
    manifest.segments[0].events.byte_length = length;
    payloads.insert(path.to_owned(), bytes);
}

fn encode_rprov(manifest: &RprovManifest, payloads: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let manifest_bytes = encode_rprov_manifest(manifest).unwrap();
    let records_bytes = std::iter::once(("manifest.json", manifest_bytes.len() as u64))
        .chain(
            manifest
                .inventory
                .iter()
                .map(|entry| (entry.path.as_str(), entry.byte_length)),
        )
        .map(|(path, length)| RPROV_RECORD_HEADER_BYTES as u64 + path.len() as u64 + length)
        .sum();
    let header = RprovContainerHeader {
        format_version: RPROV_FORMAT_VERSION_V1,
        entry_count: (manifest.inventory.len() + 1) as u32,
        stored_records_bytes: records_bytes,
        expanded_records_bytes: records_bytes,
    };
    let mut output = encode_rprov_container_header(&header).unwrap().to_vec();
    write_rprov_record(&mut output, "manifest.json", &manifest_bytes);
    for entry in &manifest.inventory {
        write_rprov_record(&mut output, &entry.path, &payloads[&entry.path]);
    }
    output
}

fn write_rprov_record(output: &mut Vec<u8>, path: &str, bytes: &[u8]) {
    let header = RprovRecordHeader {
        path_bytes: path.len() as u16,
        entry_type: RprovRecordType::RegularFile,
        payload_bytes: bytes.len() as u64,
    };
    output.extend_from_slice(&encode_rprov_record_header(&header).unwrap());
    output.extend_from_slice(path.as_bytes());
    output.extend_from_slice(bytes);
}

fn prepare(label: &str) -> (PathBuf, std::ffi::OsString) {
    let root = std::env::temp_dir().join(format!(
        "rustrace-dependency-{}-{label}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("src")).unwrap();
    let bin = root.join("target/bin");
    fs::create_dir_all(bin.join("v1")).unwrap();
    let script = include_bytes!("support/dependency_rustup.py");
    for path in std::iter::once(bin.join("rustup"))
        .chain(["rustc", "cargo", "rustdoc"].map(|name| bin.join("v1").join(name)))
    {
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\n[workspace]\n",
    )
    .unwrap();
    fs::write(root.join("Cargo.lock"), "initial-lock\n").unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    (root, path)
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
        .then(|| "dependency fixture hang guard exceeded".into())
}

fn expected_files(root: &Path) -> BTreeMap<rustrace_model::WorkspacePath, Vec<u8>> {
    ["Cargo.toml", "Cargo.lock", "src/main.rs"]
        .into_iter()
        .map(|path| {
            (
                rustrace_model::WorkspacePath::new(path).unwrap(),
                fs::read(root.join(path)).unwrap(),
            )
        })
        .collect()
}

fn dependency_edits(root: &Path, id: &rustrace_model::SessionId) -> Vec<EditOrigin> {
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
            .filter(|event| matches!(event.event, Event::ControlledCommandStarted(ref start)
                if matches!(start.action, ControlledAction::Add | ControlledAction::Remove | ControlledAction::Update)))
            .count(),
        if events.iter().any(|event| matches!(event.event, Event::SubmissionFinalized(_))) { 3 } else { 1 }
    );
    events
        .into_iter()
        .filter_map(|event| match event.event {
            Event::FileEdited(transaction) if transaction.origin == EditOrigin::DependencyTool => {
                Some(transaction.origin)
            }
            _ => None,
        })
        .collect()
}
