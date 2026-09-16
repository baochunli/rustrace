#[path = "support/test_home.rs"]
mod test_home;
use rustrace::{
    session::{ProductionSession, create_bundle},
    tui::EditorCommand,
    verify::verify_path,
};
use rustrace_model::{PasteInputChannel, PasteRejectionReason, RprovEntryKind, WorkspacePath};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Output,
    sync::atomic::{AtomicU64, Ordering},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Privacy CLI"
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

const POLICY_LINES: [&str; 3] = [
    "The data is used for grading only. There is no research use of the recorded data.",
    "Only the course's TAs and the instructor may review the data.",
    "All recorded data is permanently deleted after the term's grades are released.",
];

const EVENT_KINDS: [(&str, &str); 29] = [
    ("ControlledCommandStarted", "controlled_command_started"),
    ("ControlledCommandOutput", "controlled_command_output"),
    ("ControlledCommandFinished", "controlled_command_finished"),
    ("TestCaseCompared", "test_case_compared"),
    ("SessionStarted", "session_started"),
    ("SessionResumed", "session_resumed"),
    ("SessionEnded", "session_ended"),
    ("FileCreated", "file_created"),
    ("FileDeleted", "file_deleted"),
    ("FileRenamed", "file_renamed"),
    ("FileFocused", "file_focused"),
    ("FileEdited", "file_edited"),
    ("ClipboardCopied", "clipboard_copied"),
    ("InternalPaste", "internal_paste"),
    ("PasteRejected", "paste_rejected"),
    ("SelectionChanged", "selection_changed"),
    ("ViewportChanged", "viewport_changed"),
    ("CargoCommandStarted", "cargo_command_started"),
    ("CargoDiagnostic", "cargo_diagnostic"),
    ("CargoOutput", "cargo_output"),
    ("CargoCommandFinished", "cargo_command_finished"),
    ("LspCompletionRequested", "lsp_completion_requested"),
    ("LspCompletionAccepted", "lsp_completion_accepted"),
    ("LspCodeActionApplied", "lsp_code_action_applied"),
    ("WorkspaceCheckpoint", "workspace_checkpoint"),
    ("ExternalFileChange", "external_file_change"),
    ("ExternalObservation", "external_observation"),
    ("RecoveryRecorded", "recovery_recorded"),
    ("SubmissionFinalized", "submission_finalized"),
];

struct Fixture {
    base: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn started(prefix: &str) -> (Self, ProductionSession) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "rustrace-{prefix}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = base.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("main.rs"), "A").unwrap();
        let fixture = Self {
            base: fs::canonicalize(base).unwrap(),
            workspace: fs::canonicalize(workspace).unwrap(),
        };
        let session = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
        (fixture, session)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn run_privacy(workspace: &Path) -> Output {
    let test_home = test_home::TestHome::new(false);
    test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("privacy")
        .arg(workspace)
        .output()
        .unwrap()
}

fn successful_text(output: Output) -> String {
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "status={} stdout={text} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    text
}

fn failed_text(output: Output) -> String {
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        !output.status.success(),
        "status={} stdout={text} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    text
}

fn scalar_line(text: &str, label: &str) -> u64 {
    text.lines()
        .find_map(|line| line.trim().strip_prefix(label))
        .unwrap_or_else(|| panic!("missing {label:?} in:\n{text}"))
        .parse()
        .unwrap()
}

fn pair_line(text: &str, label: &str) -> (u64, u64) {
    let value = text
        .lines()
        .find_map(|line| line.trim().strip_prefix(label))
        .unwrap_or_else(|| panic!("missing {label:?} in:\n{text}"));
    let (count, bytes) = value.split_once(" entries, ").unwrap();
    (
        count.parse().unwrap(),
        bytes.strip_suffix(" bytes").unwrap().parse().unwrap(),
    )
}

fn event_count(text: &str, kind: &str) -> u64 {
    scalar_line(text, &format!("{kind}: "))
}

fn assert_policy_and_safe_vocabulary(text: &str) {
    for line in POLICY_LINES {
        assert!(
            text.lines().any(|actual| actual == line),
            "{line:?}: {text}"
        );
    }
    let lower = text.to_ascii_lowercase();
    for forbidden in ["misconduct", "authorship", "hand-in", "uploaded"] {
        assert!(
            !lower.contains(forbidden),
            "forbidden {forbidden:?}: {text}"
        );
    }
}

fn materialize(files: &BTreeMap<WorkspacePath, Vec<u8>>, root: &Path) {
    fs::create_dir(root).unwrap();
    for (path, bytes) in files {
        let destination = root.join(path.as_str());
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(destination, bytes).unwrap();
    }
}

fn state_fingerprints(root: &Path) -> BTreeMap<std::ffi::OsString, (u64, blake3::Hash)> {
    fs::read_dir(root.join(".rustrace"))
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let mut file = fs::File::open(&path).unwrap();
            let length = file.metadata().unwrap().len();
            let mut hasher = blake3::Hasher::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = file.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                hasher.update(&buffer[..count]);
            }
            (
                path.file_name().unwrap().to_owned(),
                (length, hasher.finalize()),
            )
        })
        .collect()
}

#[test]
fn finalized_single_attempt_matches_verifier_and_receipt_inventory() {
    let (fixture, mut session) = Fixture::started("privacy-finalized");
    session.execute(EditorCommand::Insert('B')).unwrap();
    session
        .reject_paste(
            PasteInputChannel::TerminalBracketed,
            PasteRejectionReason::ExternalInput,
        )
        .unwrap();
    fs::write(fixture.workspace.join("main.rs"), "external").unwrap();
    assert!(session.recheck_external().unwrap());
    let receipt = session.finalize("student-1").unwrap();

    let before = state_fingerprints(&fixture.workspace);
    let text = successful_text(run_privacy(&fixture.workspace));
    assert_eq!(
        state_fingerprints(&fixture.workspace),
        before,
        "privacy inspection changed finalized state"
    );
    assert!(text.contains("State: FINALIZED"), "{text}");
    assert!(text.contains("Student ID: student-1"), "{text}");
    assert!(
        text.contains(&format!("Assignment manifest: {} bytes", MANIFEST.len())),
        "{text}"
    );
    assert!(!text.contains("Assignment manifest: 1 entries"), "{text}");
    assert_eq!(scalar_line(&text, "Attempts: "), 1);
    assert_eq!(scalar_line(&text, "Event records: "), 8);
    assert_eq!(event_count(&text, "workspace_checkpoint"), 3);
    assert_eq!(event_count(&text, "file_edited"), 1);
    assert_eq!(event_count(&text, "paste_rejected"), 1);
    assert_eq!(event_count(&text, "external_observation"), 1);
    assert_eq!(event_count(&text, "recovery_recorded"), 1);
    assert_eq!(event_count(&text, "submission_finalized"), 1);
    assert_eq!(scalar_line(&text, "Edit transactions: "), 1);
    assert_eq!(scalar_line(&text, "Inserted Unicode scalars: "), 1);
    assert_eq!(scalar_line(&text, "Deleted Unicode scalars: "), 0);
    assert_eq!(scalar_line(&text, "Blocked-paste metadata records: "), 1);

    let expected = |kind| {
        let entries = receipt
            .manifest()
            .inventory
            .iter()
            .filter(|entry| entry.kind == kind)
            .collect::<Vec<_>>();
        (
            entries.len() as u64,
            entries.iter().map(|entry| entry.byte_length).sum(),
        )
    };
    assert_eq!(
        pair_line(&text, "Checkpoints: "),
        expected(RprovEntryKind::Checkpoint)
    );
    assert_eq!(
        pair_line(&text, "External-change evidence: "),
        expected(RprovEntryKind::ExternalRecoveryEvidence)
    );
    assert_eq!(
        pair_line(&text, "Runtime metadata: "),
        expected(RprovEntryKind::RuntimeMetadata)
    );

    let bundle = create_bundle(&receipt, &fixture.base.join("submission.zip")).unwrap();
    let verification = verify_path(&bundle.path, None);
    assert!(verification.is_clean(), "{:?}", verification.issues);
    assert_eq!(
        scalar_line(&text, "External-change observations: "),
        verification.external_changes.unwrap()
    );
    assert_eq!(
        scalar_line(&text, "Blocked-paste metadata records: "),
        verification.rejected_paste_attempts.unwrap()
    );
    assert_policy_and_safe_vocabulary(&text);
}

#[test]
fn finalized_two_attempt_history_is_complete_and_ordered() {
    let (fixture, mut parent) = Fixture::started("privacy-two-attempts");
    parent.execute(EditorCommand::Insert('B')).unwrap();
    let parent_id = parent.session_id().clone();
    let parent_receipt = parent.finalize("student-1").unwrap();
    let child_root = fixture.base.join("child");
    materialize(parent_receipt.final_workspace(), &child_root);
    let mut child =
        ProductionSession::start_revision(&fixture.workspace, &child_root, MANIFEST).unwrap();
    child.execute(EditorCommand::Insert('C')).unwrap();
    let child_id = child.session_id().clone();
    child.finalize("student-1").unwrap();

    let text = successful_text(run_privacy(&child_root));
    assert_eq!(scalar_line(&text, "Attempts: "), 2);
    assert_eq!(scalar_line(&text, "Event records: "), 8);
    assert_eq!(event_count(&text, "workspace_checkpoint"), 4);
    assert_eq!(event_count(&text, "file_edited"), 2);
    assert_eq!(event_count(&text, "submission_finalized"), 2);
    assert_eq!(scalar_line(&text, "Edit transactions: "), 2);
    assert_eq!(scalar_line(&text, "Inserted Unicode scalars: "), 2);
    assert_eq!(scalar_line(&text, "Deleted Unicode scalars: "), 0);
    let parent_line = format!("Attempt 1 session {parent_id}");
    let child_line = format!("Attempt 2 session {child_id}");
    assert!(text.find(&parent_line) < text.find(&child_line), "{text}");
    assert!(
        text.contains(
            "Revised bundles include all prior recorded attempts from the original starter."
        ),
        "{text}"
    );
    assert_policy_and_safe_vocabulary(&text);
}

#[test]
fn unfinished_session_is_a_durable_prefix_preview_with_deleted_text_counts() {
    let (fixture, mut session) = Fixture::started("privacy-unfinished");
    session.execute(EditorCommand::Insert('B')).unwrap();
    session.execute(EditorCommand::DeleteBackward).unwrap();
    session
        .reject_paste(
            PasteInputChannel::Programmatic,
            PasteRejectionReason::UnverifiableInput,
        )
        .unwrap();
    session.quit().unwrap();
    let before = state_fingerprints(&fixture.workspace);

    let text = successful_text(run_privacy(&fixture.workspace));
    assert!(
        text.contains("State: PREVIEW - UNFINISHED DURABLE PREFIX"),
        "{text}"
    );
    assert!(
        text.contains("Student ID: not recorded until finalization"),
        "{text}"
    );
    assert_eq!(scalar_line(&text, "Attempts: "), 1);
    assert_eq!(scalar_line(&text, "Event records: "), 4);
    assert_eq!(event_count(&text, "workspace_checkpoint"), 1);
    assert_eq!(event_count(&text, "file_edited"), 2);
    assert_eq!(event_count(&text, "paste_rejected"), 1);
    assert_eq!(scalar_line(&text, "Edit transactions: "), 2);
    assert_eq!(scalar_line(&text, "Inserted Unicode scalars: "), 1);
    assert_eq!(scalar_line(&text, "Deleted Unicode scalars: "), 1);
    assert_eq!(scalar_line(&text, "Blocked-paste metadata records: "), 1);
    assert!(
        text.contains("Preview only: this session is unfinished"),
        "{text}"
    );
    assert_policy_and_safe_vocabulary(&text);

    assert_eq!(
        state_fingerprints(&fixture.workspace),
        before,
        "privacy preview changed durable state"
    );
}

#[test]
fn live_session_failure_tells_the_student_to_close_rustrace() {
    let (fixture, _session) = Fixture::started("privacy-live-session");

    let text = failed_text(run_privacy(&fixture.workspace));

    assert!(
        text.contains("a rustrace session is currently open on this workspace; close it and retry"),
        "{text}"
    );
    assert!(!text.contains("writer ownership"), "{text}");
}

#[test]
fn missing_linked_parent_error_names_the_expected_session_and_path() {
    let (fixture, mut parent) = Fixture::started("privacy-missing-parent");
    parent.execute(EditorCommand::Insert('B')).unwrap();
    let parent_id = parent.session_id().clone();
    let parent_receipt = parent.finalize("student-1").unwrap();
    let child_root = fixture.base.join("child");
    materialize(parent_receipt.final_workspace(), &child_root);
    let child =
        ProductionSession::start_revision(&fixture.workspace, &child_root, MANIFEST).unwrap();
    child.quit().unwrap();
    fs::remove_dir_all(fixture.workspace.join(".rustrace")).unwrap();

    let text = failed_text(run_privacy(&child_root));

    assert!(text.contains(parent_id.as_str()), "{text}");
    assert!(
        text.contains(&fixture.workspace.to_string_lossy().into_owned()),
        "{text}"
    );
    assert!(text.contains("expected finalized provenance"), "{text}");
    assert!(!text.contains("privacy stopped: No such file"), "{text}");
}

#[test]
fn privacy_document_tracks_the_complete_event_schema_and_fixed_policy() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let event_source = fs::read_to_string(root.join("crates/model/src/event.rs")).unwrap();
    let event_body = event_source
        .split_once("pub enum Event {")
        .unwrap()
        .1
        .split_once("\n}")
        .unwrap()
        .0;
    let schema_variants = event_body
        .lines()
        .filter_map(|line| line.trim().split_once('(').map(|(name, _)| name))
        .collect::<Vec<_>>();
    assert_eq!(
        schema_variants,
        EVENT_KINDS
            .iter()
            .map(|(variant, _)| *variant)
            .collect::<Vec<_>>(),
        "update the privacy vocabulary whenever the event schema changes"
    );

    let document = fs::read_to_string(root.join("docs/privacy.md")).unwrap();
    assert!(
        document.contains("required message and optional code, document, and range"),
        "cargo_diagnostic optionality must stay precise"
    );
    assert!(
        document.contains(
            "command ID, case name, expected BLAKE3 digest, optional actual BLAKE3 digest, and typed outcome"
        ),
        "test-case comparison fields must stay exact"
    );
    assert!(
        document.contains(
            "No raw test input, expected output, or actual output bytes are stored in this event"
        ),
        "test-case comparison privacy boundary must stay explicit"
    );
    for (_, kind) in EVENT_KINDS {
        assert!(document.contains(&format!("`{kind}`")), "missing {kind}");
    }
    for line in POLICY_LINES {
        assert!(
            document.lines().any(|actual| actual == line),
            "missing {line:?}"
        );
    }
    let lower = document.to_ascii_lowercase();
    for forbidden in [
        "cheating detected",
        "research participant",
        "study participation",
        "i consent",
        "consent is implied",
        "authorship score",
        "misconduct score",
        "successful lms hand-in",
    ] {
        assert!(!lower.contains(forbidden), "forbidden {forbidden:?}");
    }
}
