#[path = "support/test_home.rs"]
mod test_home;
use rustrace::session::ProductionSession;
use rustrace_journal::Journal;
use rustrace_model::{Hash, SessionId};
use rustrace_workspace::assignment_package::{ExtractionLimits, extract_assignment_package};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Output,
    sync::atomic::{AtomicU64, Ordering},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Retention CLI"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["**"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

const MANIFEST_V2: &[u8] = br#"format_version = 2
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Retention CLI"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["**"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

struct Fixture {
    base: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new(prefix: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "rustrace-{prefix}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = base.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("main.rs"), "A").unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n",
        )
        .unwrap();
        Self {
            base: fs::canonicalize(base).unwrap(),
            workspace: fs::canonicalize(workspace).unwrap(),
        }
    }

    fn started(prefix: &str) -> (Self, ProductionSession) {
        let fixture = Self::new(prefix);
        let mut session = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
        session
            .execute(rustrace::tui::EditorCommand::NextBuffer)
            .unwrap();
        (fixture, session)
    }

    fn package(&self) -> PathBuf {
        let path = self.base.join("assignment.rta");
        fs::write(&path, assignment_package()).unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn run(args: &[&Path]) -> Output {
    let test_home = test_home::TestHome::new(false);
    let mut command = test_home.command(env!("CARGO_BIN_EXE_rustrace"));
    for arg in args {
        command.arg(arg);
    }
    command.output().unwrap()
}

fn state_files(workspace: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(workspace.join(".rustrace"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let metadata = entry.metadata().unwrap();
            assert!(metadata.is_file(), "unexpected state entry {name}");
            (name, fs::read(entry.path()).unwrap())
        })
        .collect()
}

fn journal_chain(workspace: &Path, session_id: &SessionId) -> (u64, Hash) {
    let path = workspace
        .join(".rustrace")
        .join(format!("{session_id}.sqlite"));
    let mut journal = Journal::open_read_only_no_follow(path).unwrap();
    let chain = journal.verify_session_chain(session_id).unwrap();
    (chain.event_count, chain.final_hash)
}

#[test]
fn failed_export_preserves_provenance_and_successes_append_local_records() {
    let (fixture, session) = Fixture::started("retention-export");
    let receipt = session.finalize("student-1").unwrap();
    let session_id = receipt.manifest().latest_session_id.clone();
    let final_tree_hash = receipt.manifest().final_tree_hash.known().unwrap();
    let terminal_hash = receipt.manifest().segments.last().unwrap().last_event_hash;
    let chain_before = journal_chain(&fixture.workspace, &session_id);
    let state_before = state_files(&fixture.workspace);
    let occupied = fixture.base.join("occupied.zip");
    fs::write(&occupied, "keep").unwrap();

    let failed = run(&[
        Path::new("submit"),
        &fixture.workspace,
        Path::new("--student-id"),
        Path::new("student-1"),
        Path::new("--output"),
        &occupied,
    ]);
    assert!(!failed.status.success());
    assert_eq!(fs::read(&occupied).unwrap(), b"keep");
    assert_eq!(state_files(&fixture.workspace), state_before);
    assert_eq!(journal_chain(&fixture.workspace, &session_id), chain_before);

    let first = fixture.base.join("first.zip");
    let second = fixture.base.join("second.zip");
    for destination in [&first, &second] {
        let output = run(&[
            Path::new("submit"),
            &fixture.workspace,
            Path::new("--student-id"),
            Path::new("student-1"),
            Path::new("--output"),
            destination,
        ]);
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let first_bytes = fs::read(&first).unwrap();
    assert_eq!(first_bytes, fs::read(&second).unwrap());
    assert_eq!(journal_chain(&fixture.workspace, &session_id), chain_before);
    assert!(
        !first_bytes
            .windows(b"export-records.json".len())
            .any(|bytes| bytes == b"export-records.json")
    );
    assert!(
        !first_bytes
            .windows(first.as_os_str().len())
            .any(|bytes| bytes == first.as_os_str().as_encoded_bytes())
    );

    let records: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.workspace.join(".rustrace/export-records.json")).unwrap(),
    )
    .unwrap();
    let records = records["records"].as_array().unwrap();
    assert_eq!(records.len(), 2, "failed publication must not be recorded");
    for (record, path) in records.iter().zip([&first, &second]) {
        assert_eq!(record["bundle_path"], path.to_str().unwrap());
        assert_eq!(
            record["bundle_blake3"],
            blake3::hash(&first_bytes).to_hex().as_str()
        );
        assert!(record["exported_at_unix_millis"].as_u64().unwrap() > 0);
        assert_eq!(record["final_tree_hash"], final_tree_hash.to_string());
        assert_eq!(record["terminal_chain_hash"], terminal_hash.to_string());
        assert_eq!(
            record["ancestry_session_ids"].as_array().unwrap(),
            &[serde_json::Value::String(session_id.to_string())]
        );
    }
    let status = run(&[Path::new("status"), &fixture.workspace]);
    assert!(status.status.success());
    let text = String::from_utf8(status.stdout).unwrap();
    assert!(text.contains("Local export records: 2"), "{text}");
    assert!(text.contains(first.to_str().unwrap()), "{text}");
    assert!(text.contains(second.to_str().unwrap()), "{text}");
}

#[test]
fn status_distinguishes_unfinished_incomplete_and_finalized_receipts() {
    let (unfinished, session) = Fixture::started("retention-status-unfinished");
    let unfinished_id = session.session_id().clone();
    session.quit().unwrap();
    let output = run(&[Path::new("status"), &unfinished.workspace]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("UNFINISHED") && text.contains(unfinished_id.as_str()),
        "{text}"
    );

    let incomplete = Fixture::new("retention-status-incomplete");
    let session = ProductionSession::start(&incomplete.workspace, MANIFEST).unwrap();
    let incomplete_id = session.session_id().clone();
    session.quit().unwrap();
    fs::write(
        incomplete
            .workspace
            .join(".rustrace/finalization-incomplete.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "label": "INCOMPLETE RECOVERY",
            "reason": "interrupted before immutable capture",
            "session_id": incomplete_id,
            "capture_available": false
        }))
        .unwrap(),
    )
    .unwrap();
    let output = run(&[Path::new("status"), &incomplete.workspace]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("INCOMPLETE RECOVERY"), "{text}");
    assert!(
        text.contains("interrupted before immutable capture"),
        "{text}"
    );

    let (finalized, session) = Fixture::started("retention-status-finalized");
    let receipt = session.finalize("student-1").unwrap();
    fs::write(
        finalized
            .workspace
            .join(".rustrace/finalization-incomplete.json"),
        b"historical stale marker",
    )
    .unwrap();
    let output = run(&[Path::new("status"), &finalized.workspace]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("FINALIZED IMMUTABLE SNAPSHOT"), "{text}");
    assert!(
        text.contains(
            &receipt
                .manifest()
                .final_tree_hash
                .known()
                .unwrap()
                .to_string()
        ),
        "{text}"
    );
    assert!(
        text.contains("does not mean a successful LMS hand-in"),
        "{text}"
    );
    assert!(!text.contains("INCOMPLETE RECOVERY"), "{text}");
}

#[test]
fn cleanup_is_dry_run_first_and_provenance_destruction_names_descendants() {
    let (fixture, parent) = Fixture::started("retention-cleanup");
    let parent_receipt = parent.finalize("student-1").unwrap();
    let parent_id = parent_receipt.manifest().latest_session_id.clone();
    let child_root = fixture.base.join("child");
    materialize(parent_receipt.final_workspace(), &child_root);
    let child =
        ProductionSession::start_revision(&fixture.workspace, &child_root, MANIFEST).unwrap();
    let child_id = child.session_id().clone();
    child.quit().unwrap();

    fs::create_dir_all(fixture.workspace.join("target/debug")).unwrap();
    fs::write(fixture.workspace.join("target/debug/output"), "discardable").unwrap();
    let stale = fixture.base.join(".rustrace-submit-2147483647-1.tmp");
    fs::write(&stale, "discardable").unwrap();
    let active = fixture
        .base
        .join(format!(".rustrace-submit-{}-1.tmp", std::process::id()));
    fs::write(&active, "active").unwrap();
    let provenance_before = state_files(&fixture.workspace);

    let dry_run = run(&[Path::new("cleanup"), &fixture.workspace]);
    assert!(dry_run.status.success());
    let text = String::from_utf8(dry_run.stdout).unwrap();
    assert!(text.contains("DRY RUN"), "{text}");
    assert!(
        text.contains("target") && text.contains("PRESERVE"),
        "{text}"
    );
    assert!(
        text.contains("OUTSIDE ROOT") && text.contains(stale.to_str().unwrap()),
        "{text}"
    );
    assert!(fixture.workspace.join("target").exists());
    assert!(stale.exists());

    let confirmed = run(&[
        Path::new("cleanup"),
        &fixture.workspace,
        Path::new("--confirm"),
    ]);
    assert!(confirmed.status.success());
    assert!(!fixture.workspace.join("target").exists());
    assert!(!stale.exists());
    assert!(
        active.exists(),
        "an active exporter temporary must be preserved"
    );
    assert_eq!(state_files(&fixture.workspace), provenance_before);
    assert!(child_root.join(".rustrace").exists());

    let unknown = fixture.base.join("unknown-linked-attempt");
    fs::create_dir_all(unknown.join(".rustrace")).unwrap();
    fs::write(
        unknown.join(".rustrace/parent.json"),
        br#"{"kind":"finalized_revision"}"#,
    )
    .unwrap();

    let refused = run(&[
        Path::new("cleanup"),
        &fixture.workspace,
        Path::new("--destroy-provenance"),
    ]);
    assert!(!refused.status.success());
    let text = String::from_utf8(refused.stdout).unwrap();
    assert!(
        text.contains("--confirm") && text.contains(child_id.as_str()),
        "{text}"
    );
    assert!(
        text.contains("future clean self-contained exports"),
        "{text}"
    );
    assert!(
        text.contains("WOULD REMOVE")
            && text.contains(fixture.workspace.join(".rustrace").to_str().unwrap()),
        "{text}"
    );
    assert!(
        !text
            .lines()
            .any(|line| line.starts_with("PRESERVE: managed source plus all journals")),
        "{text}"
    );
    assert!(text.contains("UNKNOWN OR UNREACHABLE"), "{text}");
    assert!(fixture.workspace.join(".rustrace").exists());

    let blocked = run(&[
        Path::new("cleanup"),
        &fixture.workspace,
        Path::new("--destroy-provenance"),
        Path::new("--confirm"),
    ]);
    assert!(!blocked.status.success());
    assert!(fixture.workspace.join(".rustrace").exists());
    fs::remove_dir_all(unknown).unwrap();

    let destroyed = run(&[
        Path::new("cleanup"),
        &fixture.workspace,
        Path::new("--destroy-provenance"),
        Path::new("--confirm"),
    ]);
    assert!(destroyed.status.success());
    let text = String::from_utf8(destroyed.stdout).unwrap();
    assert!(
        text.contains(child_id.as_str()) && text.contains(parent_id.as_str()),
        "{text}"
    );
    assert!(!fixture.workspace.join(".rustrace").exists());
    assert!(child_root.join(".rustrace").exists());
}

#[test]
fn cleanup_refuses_a_directory_that_is_not_a_rustrace_workspace() {
    let fixture = Fixture::new("retention-cleanup-unowned");
    fs::create_dir_all(fixture.workspace.join("target/debug")).unwrap();
    fs::write(fixture.workspace.join("target/debug/output"), "preserve").unwrap();
    let stale = fixture.base.join(".rustrace-submit-2147483647-2.tmp");
    fs::write(&stale, "preserve").unwrap();

    for options in [&[][..], &[Path::new("--confirm")][..]] {
        let mut args = vec![Path::new("cleanup"), fixture.workspace.as_path()];
        args.extend_from_slice(options);
        let output = run(&args);
        assert!(!output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains(".rustrace"), "{text}");
        assert!(fixture.workspace.join("target/debug/output").exists());
        assert!(stale.exists());
    }
}

#[test]
fn revise_cli_starts_a_separate_linked_attempt_and_terminal_work_points_to_it() {
    let (fixture, parent) = Fixture::started("retention-revise");
    let parent_id = parent.session_id().clone();
    let receipt = parent.finalize("student-1").unwrap();
    let parent_chain = journal_chain(&fixture.workspace, &parent_id);
    let package = fixture.package();
    let child = fixture.base.join("revision");

    let no_choice = run(&[
        Path::new("work"),
        &package,
        Path::new("--workspace"),
        &fixture.workspace,
    ]);
    assert!(!no_choice.status.success());
    let text = String::from_utf8(no_choice.stdout).unwrap();
    assert!(
        text.contains("finalized") && text.contains("rustrace revise"),
        "{text}"
    );

    let refused = run(&[
        Path::new("work"),
        &package,
        Path::new("--workspace"),
        &fixture.workspace,
        Path::new("--resume"),
    ]);
    assert!(!refused.status.success());
    let text = String::from_utf8(refused.stdout).unwrap();
    assert!(text.contains("rustrace revise"), "{text}");

    let revised = run(&[Path::new("revise"), &fixture.workspace, &child, &package]);
    assert!(
        revised.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&revised.stdout),
        String::from_utf8_lossy(&revised.stderr)
    );
    let child_metadata = ProductionSession::read_metadata(&child).unwrap();
    assert_ne!(child_metadata.session_id, parent_id);
    assert_eq!(fs::read(child.join("main.rs")).unwrap(), b"A");
    assert_eq!(journal_chain(&fixture.workspace, &parent_id), parent_chain);
    let link: serde_json::Value =
        serde_json::from_slice(&fs::read(child.join(".rustrace/parent.json")).unwrap()).unwrap();
    assert_eq!(link["kind"], "finalized_revision");
    assert_eq!(
        link["parent_session_id"],
        receipt.manifest().latest_session_id.as_str()
    );

    let status = run(&[Path::new("status"), &child]);
    assert!(status.status.success());
    let text = String::from_utf8(status.stdout).unwrap();
    assert!(
        text.contains("LINKED REVISION") && text.contains(parent_id.as_str()),
        "{text}"
    );
}

#[test]
fn revised_work_rejects_each_tampered_assignment_identity() {
    let (fixture, mut parent) = Fixture::started("retention-revise-identity");
    parent
        .execute(rustrace::tui::EditorCommand::Insert('B'))
        .unwrap();
    let receipt = parent.finalize("student-1").unwrap();
    let package = fixture.package();

    for (name, field) in [
        ("wrong-original-starter", "original_starter_tree_hash"),
        ("wrong-parent-final", "parent_final_tree_hash"),
    ] {
        let child = fixture.base.join(name);
        materialize(receipt.final_workspace(), &child);
        ProductionSession::start_revision(&fixture.workspace, &child, MANIFEST)
            .unwrap()
            .quit()
            .unwrap();

        let link_path = child.join(".rustrace/parent.json");
        let mut link: serde_json::Value =
            serde_json::from_slice(&fs::read(&link_path).unwrap()).unwrap();
        link[field] = serde_json::to_value(Hash::zero()).unwrap();
        let link_bytes = serde_json::to_vec(&link).unwrap();
        fs::write(&link_path, &link_bytes).unwrap();

        let metadata_path = child.join(".rustrace/session.json");
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
        metadata["parent_evidence"] =
            serde_json::to_value(Hash::from_bytes(*blake3::hash(&link_bytes).as_bytes())).unwrap();
        fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();

        let output = run(&[
            Path::new("work"),
            &package,
            Path::new("--workspace"),
            &child,
            Path::new("--inspect"),
        ]);
        assert!(!output.status.success(), "{field} was not checked");
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.contains("assignment manifest/starter/test-case-suite identity mismatch"),
            "{field}: {text}"
        );
    }

    let v2 = Fixture::new("retention-revise-suite-identity");
    let parent_package = v2.base.join("parent-v2.rta");
    let extraction = v2.base.join("parent-v2-extraction");
    fs::write(&parent_package, assignment_package_v2(b"expected\n")).unwrap();
    let extracted = extract_assignment_package(
        fs::File::open(&parent_package).unwrap(),
        &extraction,
        ExtractionLimits::default(),
    )
    .unwrap();
    ProductionSession::start_from_assignment(&v2.workspace, &extracted)
        .unwrap()
        .finalize("student-1")
        .unwrap();
    fs::remove_dir_all(&extraction).unwrap();

    let revise_package = v2.base.join("revision-v2.rta");
    fs::write(&revise_package, assignment_package_v2(b"expecteD\n")).unwrap();
    let child = v2.base.join("suite-mismatch-child");
    let source_before = fs::read(v2.workspace.join("main.rs")).unwrap();
    let state_entries_before = directory_entries(&v2.workspace.join(".rustrace"));
    let entries_before = directory_entries(&v2.base);

    let output = run(&[Path::new("revise"), &v2.workspace, &child, &revise_package]);

    assert!(
        !output.status.success(),
        "suite hash mismatch was not checked"
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains(
            "revise stopped: revision assignment package differs from the finalized parent"
        ),
        "{text}"
    );
    assert!(!child.exists(), "revision workspace was published");
    assert!(!v2.base.join("test-cases").exists());
    assert_eq!(
        fs::read(v2.workspace.join("main.rs")).unwrap(),
        source_before
    );
    assert_eq!(
        directory_entries(&v2.workspace.join(".rustrace")),
        state_entries_before,
        "revision added provenance state"
    );
    assert_eq!(
        directory_entries(&v2.base),
        entries_before,
        "revision left an attempt-created path"
    );
}

#[test]
fn invalid_export_record_error_names_the_metadata_file() {
    let (fixture, session) = Fixture::started("retention-export-invalid");
    session.finalize("student-1").unwrap();
    fs::write(
        fixture.workspace.join(".rustrace/export-records.json"),
        br#"{"version":2,"records":[]}"#,
    )
    .unwrap();

    let output = run(&[Path::new("status"), &fixture.workspace]);
    assert!(!output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("export-records.json"), "{text}");
}

fn materialize(files: &BTreeMap<rustrace_model::WorkspacePath, Vec<u8>>, root: &Path) {
    fs::create_dir(root).unwrap();
    for (path, bytes) in files {
        let destination = root.join(path.as_str());
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(destination, bytes).unwrap();
    }
}

fn assignment_package() -> Vec<u8> {
    let mut archive = Vec::new();
    append_tar_entry(&mut archive, "assignment.toml", MANIFEST, b'0');
    append_tar_entry(&mut archive, "starter/", b"", b'5');
    append_tar_entry(
        &mut archive,
        "starter/Cargo.toml",
        b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n",
        b'0',
    );
    append_tar_entry(&mut archive, "starter/main.rs", b"A", b'0');
    archive.resize(archive.len() + 1024, 0);
    archive
}

fn assignment_package_v2(expected: &[u8]) -> Vec<u8> {
    let mut archive = Vec::new();
    append_tar_entry(&mut archive, "assignment.toml", MANIFEST_V2, b'0');
    append_tar_entry(&mut archive, "starter/", b"", b'5');
    append_tar_entry(
        &mut archive,
        "starter/Cargo.toml",
        b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n",
        b'0',
    );
    append_tar_entry(&mut archive, "starter/main.rs", b"A", b'0');
    append_tar_entry(&mut archive, "test-cases/", b"", b'5');
    append_tar_entry(&mut archive, "test-cases/case.in", b"input\n", b'0');
    append_tar_entry(&mut archive, "test-cases/case.expected", expected, b'0');
    archive.resize(archive.len() + 1024, 0);
    archive
}

fn directory_entries(path: &Path) -> BTreeSet<std::ffi::OsString> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect()
}

fn append_tar_entry(archive: &mut Vec<u8>, path: &str, contents: &[u8], kind: u8) {
    let mut header = [0_u8; 512];
    header[..path.len()].copy_from_slice(path.as_bytes());
    write_octal(&mut header[100..108], 0o644);
    write_octal(&mut header[108..116], 0);
    write_octal(&mut header[116..124], 0);
    write_octal(&mut header[124..136], contents.len() as u64);
    write_octal(&mut header[136..148], 0);
    header[148..156].fill(b' ');
    header[156] = kind;
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
    header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    archive.extend_from_slice(&header);
    archive.extend_from_slice(contents);
    archive.resize(archive.len().next_multiple_of(512), 0);
}

fn write_octal(field: &mut [u8], value: u64) {
    let encoded = format!("{:0width$o}\0", value, width = field.len() - 1);
    field.copy_from_slice(encoded.as_bytes());
}
