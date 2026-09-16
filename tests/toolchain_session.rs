use rustrace::{
    session::{ProductionSession, ResumeChoice, SessionBudgets},
    toolchain::{ProbeStatus, RuntimeToolchainMetadata, ToolProbe, ToolchainReport},
    tui::EditorCommand,
};
use rustrace_journal::{EventSubmission, Journal, JournalWriter};
use rustrace_model::{
    Event, Hash, RprovEntryKind, RprovInventoryEntry, SessionEnded, SessionId, rprov_raw_blake3,
    validate_rprov_payload,
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rustrace-tool-session-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("main.rs"), b"A").unwrap();
        Self(fs::canonicalize(root).unwrap())
    }
    fn start(&self) -> ProductionSession {
        ProductionSession::start(&self.0, MANIFEST).unwrap()
    }
    fn observations(&self) -> Vec<PathBuf> {
        let mut paths = fs::read_dir(self.0.join(".rustrace"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("toolchain-")
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// Legal immutable assignment text, intentionally unsupported as a rustup name.
// Lifecycle tests persist that genuine discovery rejection without host tools;
// exact successful executable evidence is covered by the real CLI/PTY fixture.
const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Tool lifecycle"
toolchain = "invalid name"
edition = "2024"
allowed_paths = ["*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

#[test]
fn runtime_toolchain_metadata_rprov_fixture_preserves_production_bytes() {
    let observation = RuntimeToolchainMetadata {
        version: 1,
        session_id: SessionId::new("session-fixture").unwrap(),
        manifest_hash: Hash::from_bytes([3; Hash::LENGTH]),
        sequence: 1,
        event_hash: Hash::from_bytes([4; Hash::LENGTH]),
        report: ToolchainReport {
            assignment_pin: Some("1.98.1".to_owned()),
            selected_toolchain: Some("1.98.1-aarch64-apple-darwin".to_owned()),
            working_directory: PathBuf::from("assignment"),
            probes: vec![ToolProbe {
                component: "rustc".to_owned(),
                purpose: "version".to_owned(),
                required: true,
                argv: vec!["rustc".to_owned(), "--version".to_owned()],
                status: ProbeStatus::Available,
                stdout: "rustc 1.98.1\n".to_owned(),
                stderr: String::new(),
                exit_code: Some(0),
                timeout_ms: None,
                output_limited: false,
                detail: String::new(),
                remediation: String::new(),
            }],
        },
    };
    let bytes = serde_json::to_vec(&observation).unwrap();
    let fixture = decode_hex(include_str!(
        "../crates/model/tests/fixtures/rprov/runtime-toolchain-metadata-v1.hex"
    ));
    assert_eq!(bytes, fixture, "fixture must be the actual production wire");

    let digest = rprov_raw_blake3(&bytes);
    assert_eq!(
        digest.to_string(),
        "edef642f84324ccf2c3eabe3a169618bc628427e5535a5faa4cedb1ec92be3e5"
    );
    let declaration = RprovInventoryEntry {
        path: format!("segments/0001/metadata/{digest}.json"),
        byte_length: bytes.len() as u64,
        blake3: digest,
        kind: RprovEntryKind::RuntimeMetadata,
    };
    validate_rprov_payload(&declaration, &bytes).unwrap();
}

fn decode_hex(value: &str) -> Vec<u8> {
    let value = value.trim();
    assert_eq!(value.len() % 2, 0);
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let pair = std::str::from_utf8(pair).unwrap();
            u8::from_str_radix(pair, 16).unwrap()
        })
        .collect()
}

#[test]
fn historical_session_has_no_invented_tools_and_resume_observation_binds_real_prefix() {
    let fixture = Fixture::new();
    let session = fixture.start();
    assert!(session.runtime_toolchain().is_none());
    let metadata = session.metadata().clone();
    session.quit().unwrap();
    let original = fs::read(fixture.0.join(".rustrace/session.json")).unwrap();
    assert!(fixture.observations().is_empty());
    let mut resumed =
        ProductionSession::resume(&fixture.0, MANIFEST, ResumeChoice::Resume).unwrap();
    assert!(resumed.runtime_toolchain().is_none());
    let sequence = resumed.health().unwrap().events;
    assert!(resumed.discover_toolchain().unwrap().has_blockers());
    assert_eq!(
        resumed.health().unwrap().events,
        sequence,
        "discovery is not a fabricated event"
    );
    assert!(
        resumed.discover_toolchain().is_err(),
        "one observation per controller"
    );
    assert!(
        resumed.recovery_reason().is_none(),
        "repeated-call preflight is not publication uncertainty"
    );
    resumed.quit().unwrap();
    assert_eq!(
        fs::read(fixture.0.join(".rustrace/session.json")).unwrap(),
        original
    );
    let paths = fixture.observations();
    assert_eq!(paths.len(), 1);
    let recorded: RuntimeToolchainMetadata =
        serde_json::from_slice(&fs::read(&paths[0]).unwrap()).unwrap();
    assert_eq!(recorded.session_id, metadata.session_id);
    assert_eq!(recorded.manifest_hash, metadata.manifest_hash);
    assert_eq!(recorded.sequence, sequence);
    assert!(recorded.report.selected_toolchain.is_none());
    let mut journal = Journal::open_read_only_no_follow(
        fixture
            .0
            .join(format!(".rustrace/{}.sqlite", metadata.session_id)),
    )
    .unwrap();
    let event = journal
        .read_events(&metadata.session_id, sequence, 1)
        .unwrap()
        .remove(0);
    assert!(matches!(event.event, Event::SessionResumed(_)));
    assert_eq!(event.event_hash, recorded.event_hash);
    let next = ProductionSession::resume(&fixture.0, MANIFEST, ResumeChoice::Resume).unwrap();
    assert!(
        next.runtime_toolchain().is_none(),
        "prior observation is not this controller's tools"
    );
    next.quit().unwrap();
}

#[test]
fn immutable_manifest_or_original_metadata_change_blocks_and_latches() {
    for name in ["manifest.toml", "session.json"] {
        let fixture = Fixture::new();
        let mut session = fixture.start();
        let path = fixture.0.join(".rustrace").join(name);
        let original = fs::read(&path).unwrap();
        let changed = if name == "manifest.toml" {
            String::from_utf8(original.clone())
                .unwrap()
                .replace("invalid name", "stable")
                .into_bytes()
        } else {
            let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
            value["assignment_id"] = "replacement".into();
            serde_json::to_vec(&value).unwrap()
        };
        fs::write(&path, changed).unwrap();
        assert!(session.discover_toolchain().is_err());
        assert!(fixture.observations().is_empty());
        fs::write(&path, original).unwrap();
        assert!(
            session.discover_toolchain().is_err(),
            "restoring a file does not heal the uncertain controller"
        );
        assert!(session.execute(EditorCommand::Insert('B')).is_err());
        assert_eq!(session.workspace().active_buffer().text(), "A");
        session.quit().unwrap();
    }
}

#[test]
fn root_rebinding_cannot_publish_tool_metadata_to_a_replacement() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let held = fixture.0.with_extension("held");
    fs::rename(&fixture.0, &held).unwrap();
    fs::create_dir(&fixture.0).unwrap();
    assert!(session.discover_toolchain().is_err());
    assert!(fs::read_dir(&fixture.0).unwrap().next().is_none());
    assert!(session.runtime_toolchain().is_none());
    assert!(session.quit().is_err());
    fs::remove_dir(&fixture.0).unwrap();
    fs::rename(held, &fixture.0).unwrap();
    assert!(fixture.observations().is_empty());
}

#[test]
fn no_clobber_publication_failure_preserves_evidence_and_stops_mutation() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    let sequence = session.health().unwrap().events;
    let path = fixture
        .0
        .join(format!(".rustrace/toolchain-{sequence:020}.json"));
    fs::write(&path, b"preserved observation").unwrap();
    assert!(session.discover_toolchain().is_err());
    assert_eq!(fs::read(path).unwrap(), b"preserved observation");
    assert!(session.runtime_toolchain().is_none());
    assert!(session.recovery_reason().is_some());
    assert!(session.execute(EditorCommand::Insert('B')).is_err());
    assert_eq!(fs::read(fixture.0.join("main.rs")).unwrap(), b"A");
    session.quit().unwrap();
}

#[test]
fn exhausted_storage_or_event_budgets_publish_no_observation() {
    for budgets in [
        SessionBudgets {
            storage_bytes: 1,
            ..SessionBudgets::default()
        },
        SessionBudgets {
            events: 2,
            ..SessionBudgets::default()
        },
    ] {
        let fixture = Fixture::new();
        let mut session = fixture.start();
        session.set_budgets(budgets).unwrap();
        assert!(session.discover_toolchain().is_err());
        assert!(fixture.observations().is_empty());
        assert!(session.recovery_reason().is_some());
        assert!(session.execute(EditorCommand::Insert('B')).is_err());
        session.quit().unwrap();
    }
}

#[test]
fn linked_recovery_records_only_new_session_tools_and_preserves_original_metadata() {
    let old = Fixture::new();
    let mut original = old.start();
    original.discover_toolchain().unwrap();
    original.quit().unwrap();
    let original_metadata = fs::read(old.0.join(".rustrace/session.json")).unwrap();
    let original_observation = fs::read(&old.observations()[0]).unwrap();
    let fresh = Fixture::new();
    let mut linked = ProductionSession::abandon_into(&old.0, &fresh.0, MANIFEST).unwrap();
    assert!(linked.metadata().parent_evidence.is_some());
    assert!(linked.runtime_toolchain().is_none());
    linked.discover_toolchain().unwrap();
    let expected = linked.session_id().clone();
    linked.quit().unwrap();
    assert_eq!(
        fs::read(old.0.join(".rustrace/session.json")).unwrap(),
        original_metadata
    );
    assert_eq!(
        fs::read(&old.observations()[0]).unwrap(),
        original_observation
    );
    let observation: RuntimeToolchainMetadata =
        serde_json::from_slice(&fs::read(&fresh.observations()[0]).unwrap()).unwrap();
    assert_eq!(observation.session_id, expected);
    assert!(ProductionSession::inspect(&fresh.0).is_ok());
}

#[test]
fn terminal_session_stays_unchanged_and_cannot_be_opened_for_discovery() {
    let fixture = Fixture::new();
    let session = fixture.start();
    let id = session.session_id().clone();
    let final_workspace_hash = session.metadata().starter_hash;
    let monotonic_millis = session.persisted_millis() + 1;
    session.quit().unwrap();
    let path = fixture.0.join(format!(".rustrace/{id}.sqlite"));
    let writer = JournalWriter::spawn(1, Journal::open_no_follow(&path).unwrap()).unwrap();
    assert!(
        writer
            .try_submit_event(EventSubmission {
                session_id: id,
                monotonic_millis,
                wall_clock_utc: None,
                event: Event::SessionEnded(SessionEnded {
                    final_workspace_hash
                }),
            })
            .unwrap()
            .wait()
            .unwrap()
            .persisted()
    );
    writer.shutdown().unwrap();
    let before = fs::read(&path).unwrap();
    assert!(ProductionSession::resume(&fixture.0, MANIFEST, ResumeChoice::Resume).is_err());
    assert!(fixture.observations().is_empty());
    assert_eq!(fs::read(path).unwrap(), before);
}
