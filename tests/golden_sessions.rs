//! T10.1 deterministic golden-session assembly.
#![cfg(unix)]

#[path = "support/test_home.rs"]
mod test_home;
use rustrace::{
    cargo_policy::CargoAction,
    process_indicators::{ProcessRuleOutcome, ProcessValues, indicator_links},
    replay_tui::ReplayController,
    review_flags::{AdvisoryFlagKind, ReviewFlagKind, review_flags},
    scan::run_scan,
    session::{ProductionSession, ResumeChoice, TestCaseOutcome, create_bundle},
    session_fixture::{FixtureAction, maximum_workspace, representative_hour},
    tui::EditorCommand,
    verify::{
        SubmittedSourceStatus, TestCaseEvidenceStatus, VerificationIssueKind, VerificationStatus,
        verify_path,
    },
};
use rustrace_model::{
    DecodeOutcome, DecodePolicy, Event, Hash, MAX_RPROV_ARCHIVE_ENTRIES, MAX_RPROV_EVENTS,
    RPROV_FORMAT_VERSION_V1, RPROV_RECORD_HEADER_BYTES, RprovContainerHeader,
    RprovInterAttemptTime, RprovManifest, RprovRecordHeader, RprovRecordType,
    TestCaseComparisonError, TestCaseComparisonOutcome, decode_envelope, encode_envelope,
    encode_rprov_container_header, encode_rprov_record_header, rprov_raw_blake3,
};
use rustrace_workspace::{
    assignment_package::{ExtractionLimits, extract_assignment_package},
    rprov_import::import_rprov,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    io::{Cursor, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

const FIXTURE_SEED: u64 = 54;
const MANIFEST_BYTES: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/sessions/golden/manifest.json"
));
const SESSION_MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "golden"
assignment_version = "v1"
title = "T10.1 golden sessions"
toolchain = "fixture"
edition = "2024"
allowed_paths = ["*.rs", "Cargo.lock", "Cargo.toml"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

const BUILT_FIXTURES: &[&str] = &[
    "test-case-pass",
    "test-case-mismatch",
    "test-case-error",
    "normal-exploratory",
    "fluent-legitimate",
    "paste-policy",
    "manual-transcription",
    "compiler-debugging",
    "external-replacement",
    "external-create-delete",
    "external-failure-recovery",
    "external-restart",
    "crash-recovery",
    "completion-basic",
    "completion-unavailable",
    "completion-stale",
    "completion-failing-server",
    "formatting-heavy",
    "source-mismatch",
    "malformed-traversal",
    "malformed-link",
    "malformed-duplicate",
    "malformed-over-budget",
    "malformed-nested-archive",
    "malformed-host-path-link",
    "revisions-three",
    "revision-missing-link",
    "revision-tampered-link",
    "revision-reordered",
    "revision-duplicate",
    "revision-cyclic",
    "revision-child-start-mismatch",
    "revision-host-path-link",
    "revision-nested-old-zip",
    "revision-aggregate-over-budget",
    "receipt-retry",
    "receipt-interruption",
    "cleanup-preservation",
    "process-exact-boundaries",
    "process-scalar-and-origin-counting",
    "process-unavailable-shapes",
    "process-zero-and-single-signals",
    "process-command-known",
    "process-command-unknown",
    "process-origin-mix",
    "process-deduplicated-suggestion",
];

fn manifest() -> Value {
    serde_json::from_slice(MANIFEST_BYTES).expect("valid golden manifest")
}

fn declared_fixture_ids() -> BTreeSet<String> {
    let manifest = manifest();
    assert_eq!(manifest["format_version"], 1);
    assert_eq!(manifest["seed"], FIXTURE_SEED);
    manifest["fixtures"]
        .as_array()
        .expect("fixture table")
        .iter()
        .map(|fixture| fixture["id"].as_str().expect("fixture id").to_owned())
        .collect()
}

fn fixture_row(id: &str) -> Value {
    manifest()["fixtures"]
        .as_array()
        .unwrap()
        .iter()
        .find(|fixture| fixture["id"] == id)
        .unwrap_or_else(|| panic!("missing manifest row {id}"))
        .clone()
}

fn assert_navigable_source(source: &str) {
    let (path, selector) = source
        .split_once("::")
        .unwrap_or_else(|| panic!("source must include a non-empty ::selector: {source}"));
    assert!(!selector.is_empty(), "empty source selector: {source}");
    assert!(
        selector
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
        "source selector must be an identifier: {source}"
    );
    let absolute = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    let contents = fs::read_to_string(&absolute)
        .unwrap_or_else(|error| panic!("{}: {error}", absolute.display()));
    let is_identifier_byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let found = contents.match_indices(selector).any(|(start, matched)| {
        let end = start + matched.len();
        (start == 0 || !is_identifier_byte(contents.as_bytes()[start - 1]))
            && contents
                .as_bytes()
                .get(end)
                .is_none_or(|byte| !is_identifier_byte(*byte))
    });
    assert!(
        found,
        "{} does not contain identifier {selector}",
        absolute.display()
    );
}

struct FixtureRoot(PathBuf);

impl FixtureRoot {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rustrace-t10-1-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(fs::canonicalize(root).unwrap())
    }

    fn case(&self, id: &str) -> PathBuf {
        let path = self.0.join(id);
        fs::create_dir(&path).unwrap();
        path
    }
}

impl Drop for FixtureRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn materialize(files: &BTreeMap<rustrace_model::WorkspacePath, Vec<u8>>, root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (path, bytes) in files {
        let destination = root.join(path.as_str());
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(destination, bytes).unwrap();
    }
}

fn finalize_bundle(session: ProductionSession, case: &Path) -> PathBuf {
    let receipt = session.finalize("student-1").unwrap();
    create_bundle(&receipt, &case.join("session.zip"))
        .unwrap()
        .path
}

fn final_source(replay: &mut ReplayController) -> BTreeMap<String, Vec<u8>> {
    let last = replay.positions().last().expect("final event position");
    replay.select(last).unwrap();
    replay
        .selected_event()
        .expect("selected final event")
        .source
        .iter()
        .cloned()
        .collect()
}

fn expected_workspace(root: &Path) -> BTreeMap<String, Vec<u8>> {
    serde_json::from_slice::<BTreeMap<String, String>>(
        &fs::read(root.join("expected.json")).unwrap(),
    )
    .unwrap()
    .into_iter()
    .map(|(path, text)| (path, text.into_bytes()))
    .collect()
}

fn command_ok(output: &std::process::Output, context: &str) {
    assert!(
        output.status.success(),
        "{context}: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn submit_workspace(root: &Path) -> PathBuf {
    let test_home = test_home::TestHome::new(false);
    let work = root.join("assignment.work");
    let bundle = root.join("submission.zip");
    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("submit")
        .arg(&work)
        .args(["--student-id", "student-1", "--output"])
        .arg(&bundle)
        .output()
        .unwrap();
    command_ok(&output, "golden fixture submit");
    bundle
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

fn assert_clean_consumers(
    fixture_id: Option<&str>,
    bundle: &Path,
    expected_files: &BTreeMap<String, Vec<u8>>,
) -> rustrace::verify::VerificationReport {
    let report = verify_path(bundle, None);
    assert!(report.is_clean(), "{}: {report:?}", bundle.display());
    assert_eq!(report.package_structure, VerificationStatus::Ok);
    assert_eq!(report.event_chain, VerificationStatus::Ok);
    assert_eq!(report.checkpoint_hashes, VerificationStatus::Ok);
    assert_eq!(report.replay, VerificationStatus::Ok);
    assert_eq!(report.submitted_source_match, SubmittedSourceStatus::Ok);
    assert!(review_flags(&report).is_empty(), "{report:?}");

    let mut scan = Vec::new();
    run_scan(
        &[bundle.parent().unwrap().to_string_lossy().into_owned()],
        &mut scan,
    )
    .unwrap();
    let scan = String::from_utf8(scan).unwrap();
    assert!(scan.contains(bundle.file_name().unwrap().to_str().unwrap()));
    assert!(scan.contains(" OK "), "{scan}");

    let mut replay = ReplayController::open(bundle).unwrap();
    assert!(replay.timeline_available());
    assert_eq!(final_source(&mut replay), *expected_files);
    if let Some(id) = fixture_id {
        assert_manifest_process(id, &report);
    }
    report
}

fn process_values(report: &rustrace::verify::VerificationReport) -> &ProcessValues {
    &report
        .review_indicators
        .as_ref()
        .expect("clean package indicators")
        .attempts[0]
        .values
}

fn assert_process_values(values: &ProcessValues, expected: [u64; 8]) {
    assert_eq!(
        [
            values.inserted_keyboard_scalars,
            values.keyboard_transactions,
            values.appended_keyboard_scalars,
            values.removed_keyboard_scalars,
            values.forward_inserted_scalars,
            values.before_first_build_scalars,
            values.feedback_opportunities,
            values.error_edit_rebuild_sequences,
        ],
        expected
    );
}

fn assert_manifest_process(id: &str, report: &rustrace::verify::VerificationReport) {
    let row = fixture_row(id);
    let process = row["expected"]["process"]
        .as_object()
        .unwrap_or_else(|| panic!("{id}: expected.process must be an object"));
    if let Some(reference) = process.get("reference") {
        assert_eq!(process.len(), 1, "{id}: reference-only process expectation");
        assert_eq!(reference, &row["source"], "{id}");
        assert_navigable_source(reference.as_str().unwrap());
        return;
    }

    let indicators = report
        .review_indicators
        .as_ref()
        .unwrap_or_else(|| panic!("{id}: missing real process indicators"));
    if let Some(expected_attempts) = process.get("attempts") {
        assert_eq!(
            indicators.attempts.len() as u64,
            expected_attempts.as_u64().unwrap(),
            "{id}: process attempt count"
        );
    }
    if let Some(expected_values) = process.get("values") {
        let expected = expected_values.as_array().unwrap();
        assert_eq!(expected.len(), 8, "{id}: process value width");
        assert_process_values(
            &indicators.attempts[0].values,
            std::array::from_fn(|index| expected[index].as_u64().unwrap()),
        );
    }
    if let Some(expected_outcome) = process.get("outcome") {
        for attempt in &indicators.attempts {
            let actual = match attempt.outcome {
                ProcessRuleOutcome::Suggested => "suggested",
                ProcessRuleOutcome::NotEligible { .. } => "not-eligible",
                ProcessRuleOutcome::Unavailable { .. } => "unavailable",
            };
            assert_eq!(actual, expected_outcome.as_str().unwrap(), "{id}");
        }
    }
}

fn fake_tool_path(workspace: &Path, mode: &str) -> OsString {
    let bin = workspace.join("target/bin");
    fs::create_dir_all(bin.join("v1")).unwrap();
    let script = include_bytes!("support/command_rustup.py");
    for path in std::iter::once(bin.join("rustup")).chain(
        [
            "rustc",
            "cargo",
            "rustdoc",
            "cargo-clippy",
            "cargo-fmt",
            "rustfmt",
        ]
        .map(|name| bin.join("v1").join(name)),
    ) {
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(
        workspace.join("target/runner-fixture.json"),
        serde_json::to_vec(&serde_json::json!({
            "mode": mode,
            "exit": if mode == "diagnostic" { 101 } else { 0 }
        }))
        .unwrap(),
    )
    .unwrap();
    std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap()
}

fn wait_for_command(session: &mut ProductionSession) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while session.command_active() && Instant::now() < deadline {
        session.poll_command().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!session.command_active(), "controlled fixture timed out");
}

fn run_command(session: &mut ProductionSession, action: CargoAction) {
    session.start_command(action).unwrap();
    wait_for_command(session);
}

fn run_controlled_child(case: &Path, mode: &str) {
    let workspace = case.join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::write(workspace.join("Cargo.lock"), "fixture").unwrap();
    if mode.starts_with("comparison-") {
        fs::write(
            workspace.join("Cargo.toml"),
            b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n",
        )
        .unwrap();
        let test_cases = case.join("test-cases");
        fs::create_dir(&test_cases).unwrap();
        fs::write(test_cases.join("sample.in"), b"input\n").unwrap();
        fs::write(
            test_cases.join("sample.expected"),
            if mode == "comparison-mismatch" {
                b"different\n".as_slice()
            } else {
                b"stdout:input\n".as_slice()
            },
        )
        .unwrap();
        fs::write(workspace.join("main.rs"), "").unwrap();
        let manifest = String::from_utf8(SESSION_MANIFEST.to_vec())
            .unwrap()
            .replacen("format_version = 1", "format_version = 2", 1);
        fs::write(
            case.join("assignment.rta"),
            assignment_tar(&[
                StoredZipEntry::file("assignment.toml", manifest.as_bytes()),
                StoredZipEntry::file(
                    "starter/Cargo.toml",
                    b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n",
                ),
                StoredZipEntry::file("starter/Cargo.lock", b"fixture"),
                StoredZipEntry::file("starter/main.rs", b""),
                StoredZipEntry::file("test-cases/sample.in", b"input\n"),
                StoredZipEntry::file(
                    "test-cases/sample.expected",
                    fs::read(test_cases.join("sample.expected")).unwrap(),
                ),
            ]),
        )
        .unwrap();
    } else if mode == "formatting" {
        for index in 0..5 {
            fs::write(
                workspace.join(format!("file-{index}.rs")),
                format!("// formatter fixture {index}\n"),
            )
            .unwrap();
        }
    } else {
        fs::write(workspace.join("main.rs"), "").unwrap();
    }
    let tool_mode = if mode.starts_with("comparison-") {
        "console_io"
    } else if mode == "formatting" {
        "format"
    } else {
        "diagnostic"
    };
    let path = fake_tool_path(&workspace, tool_mode);
    if mode.starts_with("comparison-") {
        fs::write(
            workspace.join("target/runner-fixture.json"),
            serde_json::to_vec(&serde_json::json!({
                "mode": "console_io",
                "mutate_source": false,
                "echo": true,
                "exit": if mode == "comparison-error" { 7 } else { 0 }
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "golden_session_child", "--nocapture"])
        .env("RUSTRACE_GOLDEN_CHILD", mode)
        .env("RUSTRACE_GOLDEN_ROOT", case)
        .env("PATH", path)
        .env("RUSTUP_AUTO_INSTALL", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "mode={mode}; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn insert(session: &mut ProductionSession, character: char, count: usize) {
    for _ in 0..count {
        session.execute(EditorCommand::Insert(character)).unwrap();
    }
}

fn assignment_tar(entries: &[StoredZipEntry]) -> Vec<u8> {
    let mut archive = Vec::new();
    for entry in entries {
        let mut header = [0_u8; 512];
        header[..entry.name.len()].copy_from_slice(entry.name.as_bytes());
        for (range, value) in [
            (100..108, 0o644),
            (108..116, 0),
            (116..124, 0),
            (124..136, entry.bytes.len() as u64),
            (136..148, 0),
        ] {
            let width = range.len() - 1;
            header[range].copy_from_slice(format!("{value:0width$o}\0").as_bytes());
        }
        header[148..156].fill(b' ');
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        archive.extend_from_slice(&header);
        archive.extend_from_slice(&entry.bytes);
        archive.resize(archive.len().next_multiple_of(512), 0);
    }
    archive.resize(archive.len() + 1024, 0);
    archive
}

#[test]
fn find_replace_bundle_is_clean_and_replays_both_keyboard_transactions() {
    let root = FixtureRoot::new("find-replace");
    let case = root.case("find-replace");
    fs::write(case.join("main.rs"), "cat\r\ncat 猫 cat\r\n").unwrap();
    let mut session = ProductionSession::start(&case, SESSION_MANIFEST).unwrap();
    session
        .execute(EditorCommand::Search("cat".to_owned()))
        .unwrap();
    session
        .execute(EditorCommand::ReplaceCurrent {
            query: "cat".to_owned(),
            replacement: "dog".to_owned(),
        })
        .unwrap();
    session
        .execute(EditorCommand::ReplaceAll {
            query: "cat".to_owned(),
            replacement: "狐".to_owned(),
        })
        .unwrap();
    let bundle = finalize_bundle(session, &case);
    let expected = BTreeMap::from([(
        "main.rs".to_owned(),
        "dog\r\n狐 猫 狐\r\n".as_bytes().to_vec(),
    )]);

    assert_clean_consumers(None, &bundle, &expected);
    let keyboard_edits = journal_events(&case)
        .into_iter()
        .filter(|event| {
            event["event"]["type"] == "file_edited"
                && event["event"]["payload"]["origin"] == "keyboard"
        })
        .count();
    assert_eq!(keyboard_edits, 2);
}

#[test]
fn golden_session_child() {
    let Some(mode) = std::env::var_os("RUSTRACE_GOLDEN_CHILD") else {
        return;
    };
    let mode = mode.to_str().unwrap();
    let case = PathBuf::from(std::env::var_os("RUSTRACE_GOLDEN_ROOT").unwrap());
    let workspace = case.join("workspace");
    let mut session = if mode.starts_with("comparison-") {
        let extracted = extract_assignment_package(
            fs::File::open(case.join("assignment.rta")).unwrap(),
            &case.join("extraction"),
            ExtractionLimits::default(),
        )
        .unwrap();
        ProductionSession::start_from_assignment(&workspace, &extracted).unwrap()
    } else {
        ProductionSession::start(&workspace, SESSION_MANIFEST).unwrap()
    };
    match mode {
        "exploratory" => {
            insert(&mut session, 'a', 24);
            run_command(&mut session, CargoAction::Check);
            insert(&mut session, 'b', 24);
            run_command(&mut session, CargoAction::Check);
        }
        "fluent" => {
            insert(&mut session, 'x', 1_000);
            run_command(&mut session, CargoAction::Check);
            for _ in 0..200 {
                session.execute(EditorCommand::DeleteBackward).unwrap();
            }
            insert(&mut session, 'y', 200);
            run_command(&mut session, CargoAction::Check);
        }
        "transcription" => {
            insert(&mut session, 'x', 1_000);
            run_command(&mut session, CargoAction::Check);
            insert(&mut session, 'y', 1);
            run_command(&mut session, CargoAction::Check);
        }
        "debugging" => {
            insert(&mut session, 'a', 1);
            run_command(&mut session, CargoAction::Check);
            insert(&mut session, 'b', 1);
            run_command(&mut session, CargoAction::Check);
            insert(&mut session, 'c', 1);
            run_command(&mut session, CargoAction::Check);
        }
        "formatting" => {
            for _ in 0..5 {
                run_command(&mut session, CargoAction::Format);
            }
        }
        "comparison-pass" | "comparison-mismatch" | "comparison-error" => {
            let case = session
                .list_test_cases()
                .unwrap()
                .into_iter()
                .find(|case| case.name() == "sample")
                .unwrap();
            session.start_test_case(case).unwrap();
            wait_for_command(&mut session);
            let comparison = session.take_test_case_result().unwrap();
            match mode {
                "comparison-pass" => {
                    assert!(matches!(comparison.outcome, TestCaseOutcome::Pass));
                }
                "comparison-mismatch" => assert!(matches!(
                    comparison.outcome,
                    TestCaseOutcome::Fail(ref mismatch) if mismatch.line == 1
                )),
                "comparison-error" => assert!(matches!(
                    comparison.outcome,
                    TestCaseOutcome::Error(ref reason) if reason == "exit 7"
                )),
                _ => unreachable!(),
            }
        }
        unknown => panic!("unknown golden child mode {unknown}"),
    }
    session.save_all().unwrap();
    let receipt = session.finalize("student-1").unwrap();
    create_bundle(&receipt, &case.join("session.zip")).unwrap();
}

#[test]
fn every_declared_golden_fixture_has_a_builder_and_navigable_source() {
    let declared = declared_fixture_ids();
    let implemented = BUILT_FIXTURES
        .iter()
        .map(|id| (*id).to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(implemented, declared);

    for fixture in manifest()["fixtures"].as_array().unwrap() {
        let source = fixture["source"].as_str().unwrap();
        assert_navigable_source(source);
        assert!(fixture["expected"].is_object(), "{fixture}");
        assert!(fixture["expected"]["final_state"].is_object(), "{fixture}");
    }
    let fixture_manifest = manifest();
    let matrix = fixture_manifest["process_matrix_sources"]
        .as_array()
        .unwrap();
    assert_eq!(matrix.len(), 16);
    for source in matrix {
        assert_navigable_source(source.as_str().unwrap());
    }
}

#[test]
fn process_matrix_rows_reference_exact_owning_tests() {
    let fixture_manifest = manifest();
    let mut rows = 0;
    for fixture in fixture_manifest["fixtures"].as_array().unwrap() {
        let id = fixture["id"].as_str().unwrap();
        if !id.starts_with("process-") {
            continue;
        }
        rows += 1;

        let process = fixture["expected"]["process"].as_object().unwrap();
        assert_eq!(
            process.len(),
            1,
            "{id}: reference-backed process expectation must not declare synthetic results"
        );
        let reference = process["reference"]
            .as_str()
            .unwrap_or_else(|| panic!("{id}: process expectation must name its owning test"));
        assert_eq!(reference, fixture["source"].as_str().unwrap(), "{id}");
        assert_navigable_source(reference);
    }
    assert_eq!(rows, 8);
}

#[test]
fn production_development_fixtures_reach_verify_scan_and_replay() {
    let fixture = FixtureRoot::new("development");
    let cases = [
        ("normal-exploratory", "exploratory"),
        ("fluent-legitimate", "fluent"),
        ("manual-transcription", "transcription"),
        ("compiler-debugging", "debugging"),
        ("formatting-heavy", "formatting"),
    ];
    for (id, mode) in cases {
        let case = fixture.case(id);
        run_controlled_child(&case, mode);
        let bundle = case.join("session.zip");
        let files = if mode == "formatting" {
            (0..5)
                .map(|index| {
                    (
                        format!("file-{index}.rs"),
                        format!("// Formatter fixture {index}\n").into_bytes(),
                    )
                })
                .chain(std::iter::once((
                    "Cargo.lock".to_owned(),
                    b"fixture".to_vec(),
                )))
                .collect()
        } else {
            let text = match mode {
                "exploratory" => format!("{}{}", "a".repeat(24), "b".repeat(24)),
                "fluent" => format!("{}{}", "x".repeat(800), "y".repeat(200)),
                "transcription" => format!("{}y", "x".repeat(1_000)),
                "debugging" => "abc".to_owned(),
                _ => unreachable!(),
            };
            BTreeMap::from([
                ("Cargo.lock".to_owned(), b"fixture".to_vec()),
                ("main.rs".to_owned(), text.into_bytes()),
            ])
        };
        let report = assert_clean_consumers(Some(id), &bundle, &files);
        let indicators = report.review_indicators.as_ref().unwrap();
        match mode {
            "exploratory" => {
                assert_process_values(process_values(&report), [48, 48, 48, 0, 48, 24, 1, 1]);
                assert!(matches!(
                    indicators.attempts[0].outcome,
                    ProcessRuleOutcome::NotEligible { .. }
                ));
            }
            "fluent" => {
                assert_process_values(
                    process_values(&report),
                    [1_200, 1_200, 1_200, 200, 1_200, 1_000, 1, 1],
                );
                assert!(!indicators.attempts[0].observations.entry);
                assert!(matches!(
                    indicators.attempts[0].outcome,
                    ProcessRuleOutcome::NotEligible { .. }
                ));
            }
            "transcription" => {
                assert_process_values(
                    process_values(&report),
                    [1_001, 1_001, 1_001, 0, 1_001, 1_000, 1, 1],
                );
                assert_eq!(
                    indicators.attempts[0].outcome,
                    ProcessRuleOutcome::Suggested
                );
                assert!(indicators.attempts[0].observations.entry);
                assert!(indicators.attempts[0].observations.before_build);
                assert!(!indicator_links(indicators).is_empty());
                let mut scan = Vec::new();
                run_scan(&[case.to_string_lossy().into_owned()], &mut scan).unwrap();
                let scan = String::from_utf8(scan).unwrap();
                assert_eq!(scan.matches("Review suggested:").count(), 1, "{scan}");
                assert!(scan.contains("Planned or familiar work can show the same pattern"));
            }
            "debugging" => {
                assert_process_values(process_values(&report), [3, 3, 3, 0, 3, 1, 2, 2]);
                let check = indicators
                    .factual
                    .commands
                    .iter()
                    .find(|command| command.action == rustrace_model::ControlledAction::Check)
                    .unwrap();
                assert_eq!(check.compiler_errors, 3);
            }
            "formatting" => {
                let attempt = &indicators.attempts[0];
                assert_eq!(attempt.values.origins.formatter, 5);
                assert_eq!(attempt.values.forward_inserted_scalars, 5);
                let format = indicators
                    .factual
                    .commands
                    .iter()
                    .find(|command| command.action == rustrace_model::ControlledAction::Format)
                    .unwrap();
                assert_eq!((format.started, format.complete), (5, 5));
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn v2_reference_rejects_resealed_false_mismatch_details() {
    let fixture = FixtureRoot::new("comparison-reference-details");
    let case = fixture.case("mismatch");
    run_controlled_child(&case, "comparison-mismatch");
    let original = stored_zip_entry(
        &fs::read(case.join("session.zip")).unwrap(),
        "session.rprov",
    );
    for (name, line, expected_len, actual_len) in [
        ("false-length", 1, 0, 12),
        ("false-first-line", 2, 0, 0),
        ("false-expected-hash", 1, 9, 12),
    ] {
        let (mut manifest, mut payloads) = collect_rprov(&original);
        let event_path = manifest.segments[0].events.entry.clone();
        let mut previous = Hash::zero();
        let mut events = Vec::new();
        for bytes in payloads[&event_path]
            .split(|byte| *byte == b'\n')
            .filter(|bytes| !bytes.is_empty())
        {
            let DecodeOutcome::Decoded(mut envelope) =
                decode_envelope(bytes, DecodePolicy::RejectUnsupported).unwrap()
            else {
                panic!("golden envelope must decode")
            };
            if let Event::TestCaseCompared(comparison) = &mut envelope.event {
                if name == "false-expected-hash" {
                    comparison.expected_blake3 = Hash::from_bytes([42; Hash::LENGTH]);
                }
                comparison.outcome = TestCaseComparisonOutcome::Mismatch {
                    line,
                    expected_len,
                    actual_len,
                };
            }
            let envelope = envelope.seal(previous).unwrap();
            previous = envelope.event_hash;
            for checkpoint in &mut manifest.segments[0].checkpoints {
                if checkpoint.owner.sequence == envelope.sequence {
                    checkpoint.owner.event_hash = envelope.event_hash;
                }
            }
            events.extend_from_slice(&encode_envelope(&envelope).unwrap());
            events.push(b'\n');
        }
        let digest = rprov_raw_blake3(&events);
        let segment = &mut manifest.segments[0];
        segment.last_event_hash = previous;
        segment.terminal_event_hash = rustrace_model::RprovKnown::Known { value: previous };
        segment.events.blake3 = digest;
        segment.events.byte_length = events.len() as u64;
        let inventory = manifest
            .inventory
            .iter_mut()
            .find(|entry| entry.path == event_path)
            .unwrap();
        inventory.blake3 = digest;
        inventory.byte_length = events.len() as u64;
        payloads.insert(event_path, events);
        let path = case.join(format!("{name}.rprov"));
        fs::write(&path, encode_rprov_unchecked(&manifest, &payloads)).unwrap();
        let recorded = verify_path(&path, None);
        assert!(
            recorded.is_clean(),
            "bounded unverified facts: {recorded:#?}"
        );
        let authenticated = verify_path(&path, Some(&case.join("assignment.rta")));
        if name == "false-expected-hash" {
            assert_eq!(authenticated.replay, VerificationStatus::Ok);
            assert_eq!(
                authenticated.assignment_reference,
                rustrace::verify::AssignmentReferenceStatus::Mismatch
            );
            assert_eq!(
                authenticated.test_case_evidence,
                Some(TestCaseEvidenceStatus::Recorded)
            );
            assert!(authenticated.issues.iter().any(|issue| {
                issue.kind == VerificationIssueKind::AssignmentReference
                    && issue
                        .detail
                        .contains("expected output hash mismatch for test case sample")
            }));
            continue;
        }
        assert_eq!(
            authenticated.event_chain,
            VerificationStatus::Ok,
            "{name}: {authenticated:#?}"
        );
        assert_eq!(
            authenticated.replay,
            VerificationStatus::Failed,
            "{name}: {authenticated:#?}"
        );
        assert!(
            authenticated.issues.iter().any(|issue| {
                issue.kind == VerificationIssueKind::Replay
                    && issue
                        .detail
                        .contains("comparison differs from reference output")
            }),
            "{name}: {authenticated:#?}"
        );
    }
}

#[test]
fn test_case_comparison_goldens_reach_verify_scan_replay_and_cli_rows() {
    let test_home = test_home::TestHome::new(false);
    let fixture = FixtureRoot::new("test-case-comparisons");
    for (id, mode, counts, first_failure) in [
        ("test-case-pass", "comparison-pass", [1, 1, 0, 0], "none"),
        (
            "test-case-mismatch",
            "comparison-mismatch",
            [1, 0, 1, 0],
            "sample, line 1",
        ),
        (
            "test-case-error",
            "comparison-error",
            [1, 0, 0, 1],
            "sample",
        ),
    ] {
        let case = fixture.case(id);
        run_controlled_child(&case, mode);
        let bundle = case.join("session.zip");
        let expected = BTreeMap::from([
            (
                "Cargo.toml".to_owned(),
                b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n".to_vec(),
            ),
            ("Cargo.lock".to_owned(), b"fixture".to_vec()),
            ("main.rs".to_owned(), Vec::new()),
        ]);

        let report = assert_clean_consumers(None, &bundle, &expected);
        assert_eq!(
            [
                report.test_case_runs.unwrap(),
                report.test_case_passes.unwrap(),
                report.test_case_mismatches.unwrap(),
                report.test_case_errors.unwrap(),
            ],
            counts,
            "{id}"
        );
        assert_eq!(
            report.test_case_evidence,
            Some(TestCaseEvidenceStatus::Recorded)
        );
        let authenticated = verify_path(&bundle, Some(&case.join("assignment.rta")));
        assert!(authenticated.is_clean(), "{id}: {authenticated:#?}");
        assert_eq!(
            authenticated.test_case_evidence,
            Some(TestCaseEvidenceStatus::ReferenceVerified)
        );
        let verified_cli = test_home
            .command(env!("CARGO_BIN_EXE_rustrace"))
            .arg("verify")
            .arg(&bundle)
            .arg("--reference")
            .arg(case.join("assignment.rta"))
            .output()
            .unwrap();
        command_ok(&verified_cli, id);
        assert!(
            String::from_utf8(verified_cli.stdout)
                .unwrap()
                .lines()
                .any(|line| {
                    line.starts_with("Test-case evidence") && line.ends_with("reference-verified")
                })
        );

        let verify = test_home
            .command(env!("CARGO_BIN_EXE_rustrace"))
            .arg("verify")
            .arg(&bundle)
            .output()
            .unwrap();
        command_ok(&verify, id);
        let verify = String::from_utf8(verify.stdout).unwrap();
        for (label, value) in [
            ("Test-case runs", counts[0].to_string()),
            ("Test-case passes", counts[1].to_string()),
            ("Test-case mismatches", counts[2].to_string()),
            ("Test-case errors", counts[3].to_string()),
            ("Test-case evidence", "recorded (unverified)".to_owned()),
            ("First failing case", first_failure.to_owned()),
        ] {
            assert!(
                verify
                    .lines()
                    .any(|line| line.starts_with(label) && line.ends_with(&value)),
                "{id}: missing {label}={value:?}\n{verify}"
            );
        }

        let mut scan = Vec::new();
        run_scan(&[case.to_string_lossy().into_owned()], &mut scan).unwrap();
        let scan = String::from_utf8(scan).unwrap();
        let expected_scan = format!(
            "Test cases: runs={} passes={} mismatches={} errors={} evidence=recorded (unverified)",
            counts[0], counts[1], counts[2], counts[3]
        );
        assert!(scan.contains(&expected_scan), "{id}: {scan}");
        let package_row = scan
            .lines()
            .find(|line| line.contains("session.zip"))
            .unwrap();
        assert!(package_row.contains("Normal"), "{id}: {package_row}");
        let mut cells = package_row.split_whitespace();
        assert!(cells.any(|cell| cell == "Normal"));
        assert_eq!(
            cells.take(4).collect::<Vec<_>>(),
            counts
                .map(|count| count.to_string())
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            "{id}: terminal comparison columns {package_row}"
        );

        let mut replay = ReplayController::open(&bundle).unwrap();
        let comparison_position = replay
            .timeline_rows(replay.event_count())
            .into_iter()
            .find(|row| row.event_name == "test case compared")
            .unwrap_or_else(|| panic!("{id}: missing comparison timeline row"))
            .position;
        replay.select(comparison_position).unwrap();
        let selected = replay.selected_event().unwrap();
        let comparison = selected.test_case_comparison.as_ref().unwrap();
        assert_eq!(comparison.case, "sample");
        assert!(!selected.command_output.is_empty());
        match id {
            "test-case-pass" => {
                assert!(matches!(
                    comparison.outcome,
                    TestCaseComparisonOutcome::Pass
                ));
            }
            "test-case-mismatch" => assert!(matches!(
                comparison.outcome,
                TestCaseComparisonOutcome::Mismatch { line: 1, .. }
            )),
            "test-case-error" => assert!(matches!(
                comparison.outcome,
                TestCaseComparisonOutcome::Error {
                    reason: TestCaseComparisonError::NonzeroExit
                }
            )),
            _ => unreachable!(),
        }
    }
}

#[test]
fn editing_conveniences_verify_and_replay_to_identical_bytes() {
    let fixture = FixtureRoot::new("editing-conveniences");
    let case = fixture.case("all-conveniences");
    let workspace = case.join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::write(workspace.join("main.rs"), "fn main() ").unwrap();
    let mut session = ProductionSession::start(&workspace, SESSION_MANIFEST).unwrap();
    session
        .execute(EditorCommand::Move {
            movement: rustrace_editor::Movement::DocumentEnd,
            selecting: false,
        })
        .unwrap();

    session.execute(EditorCommand::Insert('{')).unwrap();
    session.execute(EditorCommand::Insert('\n')).unwrap();
    session.execute(EditorCommand::Insert('(')).unwrap();
    session.execute(EditorCommand::Insert(')')).unwrap();
    session.execute(EditorCommand::Insert('\n')).unwrap();
    session.execute(EditorCommand::Insert('[')).unwrap();
    session.execute(EditorCommand::DeleteBackward).unwrap();
    for character in "if ".chars() {
        session.execute(EditorCommand::Insert(character)).unwrap();
    }
    session.execute(EditorCommand::Insert('{')).unwrap();
    session.execute(EditorCommand::DeleteForward).unwrap();
    session.execute(EditorCommand::Insert('\n')).unwrap();
    session.execute(EditorCommand::Insert('}')).unwrap();
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::ToggleComment).unwrap();

    let expected = "// fn main() {\n//     ()\n//     if {\n//     }\n// }";
    assert_eq!(session.workspace().active_buffer().text(), expected);
    let opening = expected.find('{').unwrap();
    let matching = expected.rfind('}').unwrap();
    session
        .execute(EditorCommand::MoveTo {
            line: 0,
            column: opening,
            selecting: false,
        })
        .unwrap();
    assert_eq!(
        session.workspace().active_buffer().matching_bracket_byte(),
        Some(matching as u64)
    );

    session.save_all().unwrap();
    let events = journal_events(&workspace);
    let keyboard_edits = events
        .iter()
        .filter(|event| {
            event["event"]["type"] == "file_edited"
                && event["event"]["payload"]["origin"] == "keyboard"
        })
        .collect::<Vec<_>>();
    assert_eq!(keyboard_edits.len(), 14);
    assert!(keyboard_edits.iter().all(|event| {
        event["event"]["payload"]["edits"]
            .as_array()
            .is_some_and(|edits| !edits.is_empty())
    }));

    let bundle = finalize_bundle(session, &case);
    let report = assert_clean_consumers(
        None,
        &bundle,
        &BTreeMap::from([("main.rs".to_owned(), expected.as_bytes().to_vec())]),
    );
    assert!(report.advisories.is_empty(), "{:#?}", report.advisories);
}

#[test]
fn clipboard_and_external_reconciliation_fixtures_reach_consumers() {
    let fixture = FixtureRoot::new("clipboard-external");

    let paste_case = fixture.case("paste-policy");
    let paste_work = paste_case.join("workspace");
    fs::create_dir(&paste_work).unwrap();
    let source = "x".repeat(3_000);
    fs::write(paste_work.join("a.rs"), &source).unwrap();
    fs::write(paste_work.join("b.rs"), "").unwrap();
    let mut session = ProductionSession::start(&paste_work, SESSION_MANIFEST).unwrap();
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Copy).unwrap();
    session.execute(EditorCommand::NextBuffer).unwrap();
    session.execute(EditorCommand::Paste).unwrap();
    let external = session
        .execute(EditorCommand::PasteExternal("external sentinel".to_owned()))
        .unwrap_err()
        .to_string();
    assert!(external.contains("Paste blocked:"));
    assert!(!external.contains("external sentinel"));
    session.clear_clipboard();
    assert!(
        session
            .execute(EditorCommand::Paste)
            .unwrap_err()
            .to_string()
            .contains("Paste blocked:")
    );
    session.save_all().unwrap();
    let bundle = finalize_bundle(session, &paste_case);
    let report = assert_clean_consumers(
        Some("paste-policy"),
        &bundle,
        &BTreeMap::from([
            ("a.rs".to_owned(), source.as_bytes().to_vec()),
            ("b.rs".to_owned(), source.as_bytes().to_vec()),
        ]),
    );
    let indicators = report.review_indicators.as_ref().unwrap();
    assert_eq!(
        (
            indicators.factual.allowed_internal_paste.transactions,
            indicators.factual.allowed_internal_paste.inserted_scalars,
            indicators.factual.rejected_paste.attempts,
        ),
        (1, 3_000, 2)
    );
    assert_eq!(indicators.factual.rejected_paste.links.len(), 2);
    assert_eq!(process_values(&report).forward_inserted_scalars, 3_000);

    let presence_case = fixture.case("external-create-delete");
    let presence_work = presence_case.join("workspace");
    fs::create_dir(&presence_work).unwrap();
    fs::write(presence_work.join("main.rs"), "A").unwrap();
    let mut session = ProductionSession::start(&presence_work, SESSION_MANIFEST).unwrap();
    fs::create_dir(presence_work.join("target")).unwrap();
    fs::write(presence_work.join("target/output.rs"), "excluded").unwrap();
    fs::write(presence_work.join("new.rs"), "rejected addition").unwrap();
    fs::remove_file(presence_work.join("main.rs")).unwrap();
    assert!(session.recheck_external().unwrap());
    assert_eq!(fs::read(presence_work.join("main.rs")).unwrap(), b"A");
    assert!(!presence_work.join("new.rs").exists());
    assert_eq!(
        fs::read(presence_work.join("target/output.rs")).unwrap(),
        b"excluded"
    );
    let bundle = finalize_bundle(session, &presence_case);
    let report = assert_clean_consumers(
        Some("external-create-delete"),
        &bundle,
        &BTreeMap::from([("main.rs".to_owned(), b"A".to_vec())]),
    );
    assert_eq!(report.external_changes, Some(2));

    let restart_case = fixture.case("external-restart");
    let restart_work = restart_case.join("workspace");
    fs::create_dir(&restart_work).unwrap();
    fs::write(restart_work.join("main.rs"), "A").unwrap();
    let mut session = ProductionSession::start(&restart_work, SESSION_MANIFEST).unwrap();
    session.execute(EditorCommand::Insert('B')).unwrap();
    session.quit().unwrap();
    fs::write(restart_work.join("main.rs"), "C").unwrap();
    let session =
        ProductionSession::resume(&restart_work, SESSION_MANIFEST, ResumeChoice::Resume).unwrap();
    assert_eq!(session.workspace().active_buffer().text(), "BA");
    assert_eq!(fs::read(restart_work.join("main.rs")).unwrap(), b"BA");
    let bundle = finalize_bundle(session, &restart_case);
    let report = assert_clean_consumers(
        Some("external-restart"),
        &bundle,
        &BTreeMap::from([("main.rs".to_owned(), b"BA".to_vec())]),
    );
    assert_eq!(report.external_changes, Some(1));
}

#[test]
fn fast_unpaced_generators_are_deterministic_and_measurements_are_referenced() {
    let manifest = manifest();
    let actions = representative_hour(FIXTURE_SEED);
    assert_eq!(actions, representative_hour(FIXTURE_SEED));
    assert_eq!(actions.len(), 3_600);
    assert_eq!(actions.last().unwrap().at_millis, 3_600_000);
    for required in [
        FixtureAction::Undo,
        FixtureAction::Redo,
        FixtureAction::Save,
        FixtureAction::Restart,
        FixtureAction::External,
    ] {
        assert!(actions.iter().any(|step| {
            std::mem::discriminant(&step.action) == std::mem::discriminant(&required)
        }));
    }
    let maximum = maximum_workspace(FIXTURE_SEED);
    assert_eq!(maximum, maximum_workspace(FIXTURE_SEED));
    assert_eq!(maximum.len(), 256);
    assert_eq!(
        maximum.values().map(Vec::len).sum::<usize>(),
        10 * 1024 * 1024
    );
    assert_eq!(maximum.values().map(Vec::len).max(), Some(1024 * 1024));

    let measurements = manifest["measurement_references"].as_array().unwrap();
    let expected_measurements = [
        (
            "g3-paced-hour",
            "paced hour referenced, not rerun; 3600.748 s, 4006 events, typing/render p95 35.058 ms and p99 38.868 ms",
        ),
        (
            "g6-fast-hour-and-maximum-export",
            "seed 54 fast-hour and maximum export/storage measurements referenced; unpaced fixture shapes rerun",
        ),
        (
            "g7-replay-budgets",
            "paced-hour and maximum open, 200-seek p95, charged cache, and quiet-host RSS stayed within G7 limits",
        ),
    ];
    assert_eq!(measurements.len(), expected_measurements.len());
    for (measurement, (id, expectation)) in measurements.iter().zip(expected_measurements) {
        assert_eq!(measurement["id"].as_str(), Some(id));
        assert_eq!(measurement["expectation"].as_str(), Some(expectation));
    }
}

#[test]
fn real_80x24_clipboard_and_external_replacement_suite_reaches_all_consumers() {
    let test_home = test_home::TestHome::new(false);
    let fixture = FixtureRoot::new("real-pty");
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/integrated_production_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .arg(&fixture.0)
        .arg("suite")
        .env_remove("TMPDIR")
        .output()
        .unwrap();
    command_ok(&output, "T8.7 80x24 suite");

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
        let work = bundle.parent().unwrap().join("assignment.work");
        let expected = fs::read_dir(&work)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                (path.extension().is_some_and(|extension| extension == "rs")
                    || path.file_name().is_some_and(|name| name == "Cargo.toml"))
                .then(|| {
                    (
                        path.file_name().unwrap().to_str().unwrap().to_owned(),
                        fs::read(path).unwrap(),
                    )
                })
            })
            .collect();
        let fixture_id = (case["name"] == "p2-separate").then_some("external-replacement");
        assert_clean_consumers(fixture_id, &bundle, &expected);
        let transcript = fs::read(bundle.parent().unwrap().join("work.pty")).unwrap();
        assert!(
            transcript
                .windows(b" files".len())
                .any(|window| window == b" files")
        );
    }
}

#[test]
fn fake_server_completion_and_degradation_fixtures_reach_all_consumers() {
    let test_home = test_home::TestHome::new(false);
    let fixture = FixtureRoot::new("completion");
    for (mode, expected_counts) in [
        ("completion", (1, 1, 1)),
        ("missing", (0, 0, 0)),
        ("completion_console_retirement", (1, 0, 0)),
        ("crash", (0, 0, 0)),
    ] {
        let root = fixture.case(mode);
        let output = test_home
            .command("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/lsp_work.py"
            ))
            .arg(env!("CARGO_BIN_EXE_rustrace"))
            .arg(mode)
            .arg(&root)
            .env_remove("TMPDIR")
            .output()
            .unwrap();
        command_ok(&output, mode);

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
        assert_eq!(
            (requested, accepted, completion_edits),
            expected_counts,
            "{mode}"
        );
        if mode == "crash" {
            let launches = fs::read_to_string(root.join("tools/server-launches.jsonl")).unwrap();
            assert!(launches.lines().count() >= 2);
        }
        let bundle = submit_workspace(&root);
        let fixture_id = match mode {
            "completion" => "completion-basic",
            "missing" => "completion-unavailable",
            "completion_console_retirement" => "completion-stale",
            "crash" => "completion-failing-server",
            _ => unreachable!(),
        };
        let report = assert_clean_consumers(Some(fixture_id), &bundle, &expected_workspace(&root));
        if mode == "completion" {
            assert!(
                report.advisories.is_empty(),
                "completion acceptance raised advisories: {:#?}",
                report.advisories
            );
        }
        let (expected_values, unavailable) = match mode {
            "completion" => ([1, 1, 0, 0, 2, 1, 0, 0], true),
            "missing" => ([1, 1, 0, 0, 1, 1, 0, 0], false),
            "completion_console_retirement" => ([0; 8], false),
            "crash" => ([2, 2, 0, 0, 2, 2, 0, 0], false),
            _ => unreachable!(),
        };
        assert_process_values(process_values(&report), expected_values);
        assert_eq!(
            matches!(
                report.review_indicators.as_ref().unwrap().attempts[0].outcome,
                ProcessRuleOutcome::Unavailable { .. }
            ),
            unavailable,
            "{mode}"
        );
    }
}

#[test]
fn advisory_kind_vocabulary_is_reachable_from_the_golden_suite() {
    assert_eq!(
        AdvisoryFlagKind::ALL.map(AdvisoryFlagKind::name),
        [
            "LARGE_SINGLE_INSERTION",
            "UNIFORM_KEY_TIMING",
            "SUSTAINED_HIGH_RATE",
        ]
    );
}

#[test]
fn external_failure_is_preserved_before_a_clean_linked_recovery() {
    let fixture = FixtureRoot::new("external-failure");
    let failed = fixture.case("original");
    fs::write(failed.join("main.rs"), "A").unwrap();
    let mut session = ProductionSession::start(&failed, SESSION_MANIFEST).unwrap();
    fs::write(failed.join("main.rs"), [0xff, 0]).unwrap();
    assert!(session.save_all().is_err());
    assert!(session.capture_boundary().is_err());
    drop(session);

    let preserved = fs::read(failed.join(".rustrace/session.json")).unwrap();
    let recovered = fixture.case("recovered");
    fs::write(recovered.join("main.rs"), "A").unwrap();
    let mut recovery = ProductionSession::abandon_into(&failed, &recovered, SESSION_MANIFEST)
        .expect("unsafe original can be preserved and linked into a fresh session");
    assert_eq!(
        fs::read(failed.join(".rustrace/session.json")).unwrap(),
        preserved
    );
    recovery.execute(EditorCommand::Insert('R')).unwrap();
    recovery.save_all().unwrap();
    let bundle = finalize_bundle(recovery, &fixture.0);
    let expected = BTreeMap::from([("main.rs".to_owned(), b"RA".to_vec())]);
    assert_clean_consumers(Some("external-failure-recovery"), &bundle, &expected);
}

#[test]
fn receipt_retry_is_byte_stable_and_cleanup_preserves_provenance() {
    let test_home = test_home::TestHome::new(false);
    let fixture = FixtureRoot::new("receipt-cleanup");
    let work = fixture.case("workspace");
    fs::write(work.join("main.rs"), "A").unwrap();
    let mut session = ProductionSession::start(&work, SESSION_MANIFEST).unwrap();
    session.execute(EditorCommand::Insert('B')).unwrap();
    let receipt = session.finalize("student-1").unwrap();
    let first = create_bundle(&receipt, &fixture.0.join("first.zip"))
        .unwrap()
        .path;
    let second = create_bundle(&receipt, &fixture.0.join("retry.zip"))
        .unwrap()
        .path;
    assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
    assert_clean_consumers(
        Some("receipt-retry"),
        &first,
        &BTreeMap::from([("main.rs".to_owned(), b"BA".to_vec())]),
    );

    fs::create_dir_all(work.join("target/debug")).unwrap();
    fs::write(work.join("target/debug/discardable"), "temporary").unwrap();
    let before = fs::read_dir(work.join(".rustrace"))
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (
                path.file_name().unwrap().to_owned(),
                fs::read(path).unwrap(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let cleaned = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("cleanup")
        .arg(&work)
        .arg("--confirm")
        .output()
        .unwrap();
    command_ok(&cleaned, "ordinary cleanup");
    assert!(!work.join("target").exists());
    let after = fs::read_dir(work.join(".rustrace"))
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (
                path.file_name().unwrap().to_owned(),
                fs::read(path).unwrap(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(after, before);
    assert_clean_consumers(
        None,
        &second,
        &BTreeMap::from([("main.rs".to_owned(), b"BA".to_vec())]),
    );
}

#[test]
fn controlled_command_unknown_shapes_suppress_the_process_rule() {
    let test_home = test_home::TestHome::new(false);
    let fixture = FixtureRoot::new("command-unknown");
    for (mode, reason) in [
        ("missing", "missing diagnostics"),
        ("read_failed", "incomplete execution"),
        ("cancel", "cancellation"),
        ("huge", "incomplete execution"),
    ] {
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
            .env_remove("TMPDIR")
            .output()
            .unwrap();
        command_ok(&output, mode);
        let bundle = submit_workspace(&root);
        let report = verify_path(&bundle, None);
        assert!(report.is_clean(), "{mode}: {report:?}");
        let attempt = &report.review_indicators.as_ref().unwrap().attempts[0];
        let ProcessRuleOutcome::Unavailable { reason: actual } = &attempt.outcome else {
            panic!("{mode}: incomplete command evidence did not suppress the rule")
        };
        assert!(actual.contains(reason), "{mode}: {actual}");
        let mut scan = Vec::new();
        run_scan(&[root.to_string_lossy().into_owned()], &mut scan).unwrap();
        let scan = String::from_utf8(scan).unwrap();
        assert!(scan.contains("process-review-v1"), "{mode}: {scan}");
        assert!(!scan.contains("score="), "{mode}: {scan}");
        let replay = ReplayController::open(&bundle).unwrap();
        assert!(replay.timeline_available(), "{mode}");
    }
}

#[test]
fn accepted_process_matrix_references_have_clean_consumer_controls() {
    let fixture = FixtureRoot::new("process-controls");
    for id in [
        "process-exact-boundaries",
        "process-scalar-and-origin-counting",
        "process-unavailable-shapes",
        "process-zero-and-single-signals",
        "process-command-known",
        "process-command-unknown",
        "process-origin-mix",
        "process-deduplicated-suggestion",
    ] {
        let case = fixture.case(id);
        fs::write(case.join("main.rs"), "A").unwrap();
        let bundle = finalize_bundle(
            ProductionSession::start(&case, SESSION_MANIFEST).unwrap(),
            &case,
        );
        assert_clean_consumers(
            Some(id),
            &bundle,
            &BTreeMap::from([("main.rs".to_owned(), b"A".to_vec())]),
        );
        let row = manifest()["fixtures"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap()
            .clone();
        assert!(row["expected"]["process"].is_object(), "{id}");
        assert!(row["expected"]["final_state"].is_object(), "{id}");
    }
}

#[test]
fn process_probe_backed_scenarios_have_clean_post_recovery_controls() {
    let fixture = FixtureRoot::new("probe-controls");
    for id in ["crash-recovery", "receipt-interruption"] {
        let case = fixture.case(id);
        fs::write(case.join("main.rs"), "A").unwrap();
        let mut session = ProductionSession::start(&case, SESSION_MANIFEST).unwrap();
        session.execute(EditorCommand::Insert('B')).unwrap();
        let bundle = finalize_bundle(session, &case);
        assert_clean_consumers(
            Some(id),
            &bundle,
            &BTreeMap::from([("main.rs".to_owned(), b"BA".to_vec())]),
        );
    }
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

fn tamper_stored_zip_entry(mut archive: Vec<u8>, wanted: &str, replacement: &[u8]) -> Vec<u8> {
    let end = archive.len() - 22;
    let mut central = u32::from_le_bytes(archive[end + 16..end + 20].try_into().unwrap()) as usize;
    loop {
        let expanded =
            u32::from_le_bytes(archive[central + 24..central + 28].try_into().unwrap()) as usize;
        let name_len =
            u16::from_le_bytes(archive[central + 28..central + 30].try_into().unwrap()) as usize;
        let extra_len =
            u16::from_le_bytes(archive[central + 30..central + 32].try_into().unwrap()) as usize;
        let comment_len =
            u16::from_le_bytes(archive[central + 32..central + 34].try_into().unwrap()) as usize;
        let name = std::str::from_utf8(&archive[central + 46..central + 46 + name_len]).unwrap();
        if name == wanted {
            assert_eq!(replacement.len(), expanded);
            let local = u32::from_le_bytes(archive[central + 42..central + 46].try_into().unwrap())
                as usize;
            let local_name_len =
                u16::from_le_bytes(archive[local + 26..local + 28].try_into().unwrap()) as usize;
            let local_extra_len =
                u16::from_le_bytes(archive[local + 28..local + 30].try_into().unwrap()) as usize;
            let data = local + 30 + local_name_len + local_extra_len;
            archive[data..data + replacement.len()].copy_from_slice(replacement);
            let crc = crc32fast::hash(replacement).to_le_bytes();
            archive[local + 14..local + 18].copy_from_slice(&crc);
            archive[central + 16..central + 20].copy_from_slice(&crc);
            return archive;
        }
        central += 46 + name_len + extra_len + comment_len;
    }
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

fn write_rprov_record(output: &mut Vec<u8>, path: &str, payload: &[u8]) {
    output.extend_from_slice(
        &encode_rprov_record_header(&RprovRecordHeader {
            path_bytes: path.len() as u16,
            entry_type: RprovRecordType::RegularFile,
            payload_bytes: payload.len() as u64,
        })
        .unwrap(),
    );
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
    write_rprov_record(&mut output, "manifest.json", &manifest_bytes);
    for entry in &manifest.inventory {
        write_rprov_record(&mut output, &entry.path, &payloads[&entry.path]);
    }
    output
}

#[derive(Clone)]
struct StoredZipEntry {
    name: String,
    bytes: Vec<u8>,
    external_attributes: u32,
}

impl StoredZipEntry {
    fn file(name: &str, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            name: name.to_owned(),
            bytes: bytes.into(),
            external_attributes: 0o100644 << 16,
        }
    }

    fn link(name: &str, target: &str) -> Self {
        Self {
            name: name.to_owned(),
            bytes: target.as_bytes().to_vec(),
            external_attributes: 0o120777 << 16,
        }
    }
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend(value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend(value.to_le_bytes());
}

fn stored_zip(entries: &[StoredZipEntry]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut central = Vec::new();
    for entry in entries {
        let offset = output.len() as u32;
        let crc = crc32fast::hash(&entry.bytes);
        let length = entry.bytes.len() as u32;
        let name = entry.name.as_bytes();
        push_u32(&mut output, 0x0403_4b50);
        push_u16(&mut output, 20);
        push_u16(&mut output, 0);
        push_u16(&mut output, 0);
        push_u16(&mut output, 0);
        push_u16(&mut output, 0);
        push_u32(&mut output, crc);
        push_u32(&mut output, length);
        push_u32(&mut output, length);
        push_u16(&mut output, name.len() as u16);
        push_u16(&mut output, 0);
        output.extend(name);
        output.extend(&entry.bytes);

        push_u32(&mut central, 0x0201_4b50);
        push_u16(&mut central, (3 << 8) | 20);
        push_u16(&mut central, 20);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, crc);
        push_u32(&mut central, length);
        push_u32(&mut central, length);
        push_u16(&mut central, name.len() as u16);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, entry.external_attributes);
        push_u32(&mut central, offset);
        central.extend(name);
    }
    let central_offset = output.len() as u32;
    let central_size = central.len() as u32;
    output.extend(central);
    push_u32(&mut output, 0x0605_4b50);
    push_u16(&mut output, 0);
    push_u16(&mut output, 0);
    push_u16(&mut output, entries.len() as u16);
    push_u16(&mut output, entries.len() as u16);
    push_u32(&mut output, central_size);
    push_u32(&mut output, central_offset);
    push_u16(&mut output, 0);
    output
}

fn expected_package_detail(id: &str) -> String {
    fixture_row(id)["expected"]["package_detail"]
        .as_str()
        .unwrap_or_else(|| panic!("{id}: missing expected package detail"))
        .to_owned()
}

fn assert_package_invalid(path: &Path) {
    let id = path.file_stem().unwrap().to_str().unwrap();
    let expected_detail = expected_package_detail(id);
    let report = verify_path(path, None);
    assert_eq!(
        report.package_structure,
        VerificationStatus::Failed,
        "{}: {report:?}",
        path.display()
    );
    let issue = report
        .issues
        .iter()
        .find(|issue| issue.kind == VerificationIssueKind::PackageStructure)
        .unwrap_or_else(|| panic!("{id}: missing package-structure issue: {report:?}"));
    assert_eq!(issue.detail, expected_detail, "{id}");
    assert!(
        review_flags(&report)
            .iter()
            .any(|flag| flag.kind == ReviewFlagKind::PackageInvalid),
        "{}: {report:?}",
        path.display()
    );
    let replay = ReplayController::open(path).unwrap();
    assert!(!replay.timeline_available(), "{}", path.display());
    assert!(replay.selected_event().is_none(), "{}", path.display());
}

fn event_sequences(bytes: &[u8]) -> Vec<u64> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice::<Value>(line).unwrap()["sequence"]
                .as_u64()
                .unwrap()
        })
        .collect()
}

#[test]
fn source_mismatch_and_each_hostile_class_are_isolated_by_consumers() {
    let fixture = FixtureRoot::new("hostile");
    let clean_case = fixture.case("clean");
    fs::write(clean_case.join("main.rs"), "A").unwrap();
    let clean = finalize_bundle(
        ProductionSession::start(&clean_case, SESSION_MANIFEST).unwrap(),
        &clean_case,
    );
    let clean_bytes = fs::read(&clean).unwrap();
    let clean_rprov = stored_zip_entry(&clean_bytes, "session.rprov");

    let mismatch = fixture.0.join("source-mismatch.zip");
    fs::write(
        &mismatch,
        tamper_stored_zip_entry(clean_bytes, "main.rs", b"B"),
    )
    .unwrap();
    let mismatch_report = verify_path(&mismatch, None);
    assert_eq!(
        mismatch_report.submitted_source_match,
        SubmittedSourceStatus::SourceMismatch
    );
    assert!(
        review_flags(&mismatch_report)
            .iter()
            .any(|flag| flag.kind == ReviewFlagKind::SourceMismatch)
    );
    assert_manifest_process("source-mismatch", &mismatch_report);
    let mut replay = ReplayController::open(&mismatch).unwrap();
    assert!(replay.timeline_available());
    assert_eq!(
        final_source(&mut replay),
        BTreeMap::from([("main.rs".to_owned(), b"A".to_vec())])
    );

    let mut too_many = encode_rprov_container_header(&RprovContainerHeader {
        format_version: RPROV_FORMAT_VERSION_V1,
        entry_count: MAX_RPROV_ARCHIVE_ENTRIES as u32,
        stored_records_bytes: 0,
        expanded_records_bytes: 0,
    })
    .unwrap()
    .to_vec();
    too_many[16..20].copy_from_slice(&((MAX_RPROV_ARCHIVE_ENTRIES + 1) as u32).to_le_bytes());
    let malformed = [
        (
            "malformed-traversal.zip",
            stored_zip(&[
                StoredZipEntry::file("session.rprov", clean_rprov.clone()),
                StoredZipEntry::file("../escape.rs", b"private".to_vec()),
            ]),
        ),
        (
            "malformed-link.zip",
            stored_zip(&[
                StoredZipEntry::file("session.rprov", clean_rprov.clone()),
                StoredZipEntry::link("linked.rs", "main.rs"),
            ]),
        ),
        (
            "malformed-duplicate.zip",
            stored_zip(&[
                StoredZipEntry::file("session.rprov", clean_rprov.clone()),
                StoredZipEntry::file("session.rprov", clean_rprov.clone()),
            ]),
        ),
        ("malformed-over-budget.rprov", too_many),
        (
            "malformed-nested-archive.zip",
            stored_zip(&[
                StoredZipEntry::file("session.rprov", clean_rprov.clone()),
                StoredZipEntry::file("nested.rprov", clean_rprov.clone()),
            ]),
        ),
        (
            "malformed-host-path-link.zip",
            stored_zip(&[
                StoredZipEntry::file("session.rprov", clean_rprov.clone()),
                StoredZipEntry::link("host.rs", "/private/tmp/outside.rs"),
            ]),
        ),
        (
            "revision-host-path-link.zip",
            stored_zip(&[
                StoredZipEntry::file("session.rprov", clean_rprov.clone()),
                StoredZipEntry::link("prior.rprov", "/private/tmp/prior.rprov"),
            ]),
        ),
        (
            "revision-nested-old-zip.zip",
            stored_zip(&[
                StoredZipEntry::file("session.rprov", clean_rprov),
                StoredZipEntry::file("old.zip", b"old archive dependency".to_vec()),
            ]),
        ),
    ];
    for (name, bytes) in malformed {
        let path = fixture.0.join(name);
        fs::write(&path, bytes).unwrap();
        assert_package_invalid(&path);
    }

    let mut scan = Vec::new();
    run_scan(&[fixture.0.to_string_lossy().into_owned()], &mut scan).unwrap();
    let scan = String::from_utf8(scan).unwrap();
    assert!(scan.contains("source-mismatch.zip") && scan.contains("SOURCE_MISMATCH"));
    for name in [
        "malformed-traversal.zip",
        "malformed-link.zip",
        "malformed-duplicate.zip",
        "malformed-over-budget.rprov",
        "malformed-nested-archive.zip",
        "malformed-host-path-link.zip",
        "revision-host-path-link.zip",
        "revision-nested-old-zip.zip",
    ] {
        let display_key = name.chars().take(24).collect::<String>();
        let row = scan
            .lines()
            .find(|line| line.contains(&display_key))
            .unwrap_or_else(|| panic!("missing {name} from scan:\n{scan}"));
        assert!(row.contains("PACKAGE_INVALID"), "{row}");
        assert!(
            row.contains(&expected_package_detail(
                Path::new(name).file_stem().unwrap().to_str().unwrap()
            )),
            "{row}"
        );
    }
}

#[test]
fn three_revision_latest_bundle_is_self_contained_and_rejects_bad_ancestry() {
    let test_home = test_home::TestHome::new(false);
    let fixture = FixtureRoot::new("revisions");
    let first_root = fixture.case("attempt-1");
    fs::write(first_root.join("main.rs"), "A").unwrap();
    let mut first = ProductionSession::start(&first_root, SESSION_MANIFEST).unwrap();
    first.execute(EditorCommand::Insert('B')).unwrap();
    let first_receipt = first.finalize("student-1").unwrap();
    let first_segment = first_receipt.manifest().segments[0].clone();
    let first_events = first_receipt
        .read_payload(&first_segment.events.entry)
        .unwrap();

    let second_root = fixture.case("attempt-2");
    materialize(first_receipt.final_workspace(), &second_root);
    let mut second =
        ProductionSession::start_revision(&first_root, &second_root, SESSION_MANIFEST).unwrap();
    second.execute(EditorCommand::Insert('C')).unwrap();
    let second_receipt = second.finalize("student-1").unwrap();
    let second_segment = second_receipt.manifest().segments[1].clone();
    let second_events = second_receipt
        .read_payload(&second_segment.events.entry)
        .unwrap();
    assert_eq!(
        second_receipt
            .read_payload(&first_segment.events.entry)
            .unwrap(),
        first_events
    );

    let third_root = fixture.case("attempt-3");
    materialize(second_receipt.final_workspace(), &third_root);
    let mut third =
        ProductionSession::start_revision(&second_root, &third_root, SESSION_MANIFEST).unwrap();
    third.execute(EditorCommand::Insert('D')).unwrap();
    let third_receipt = third.finalize("student-1").unwrap();
    assert_eq!(third_receipt.manifest().segments.len(), 3);
    assert_eq!(
        third_receipt
            .read_payload(&first_segment.events.entry)
            .unwrap(),
        first_events
    );
    assert_eq!(
        third_receipt
            .read_payload(&second_segment.events.entry)
            .unwrap(),
        second_events
    );

    let manifest = third_receipt.manifest();
    for (index, segment) in manifest.segments.iter().enumerate() {
        assert_eq!(segment.ordinal as usize, index + 1);
        assert_eq!(segment.course_id, manifest.course_id);
        assert_eq!(segment.assignment_id, manifest.assignment_id);
        assert_eq!(segment.assignment_version, manifest.assignment_version);
        assert_eq!(
            segment.original_starter_tree_hash,
            manifest.original_starter_tree_hash
        );
        assert_eq!(
            event_sequences(&third_receipt.read_payload(&segment.events.entry).unwrap()),
            (1..=segment.inclusive_event_count).collect::<Vec<_>>()
        );
        if index == 0 {
            assert!(segment.parent.is_none());
        } else {
            let parent = &manifest.segments[index - 1];
            let link = segment.parent.as_ref().unwrap();
            assert_eq!(link.session_id, parent.session_id);
            assert_eq!(
                link.terminal_event_hash,
                *parent.terminal_event_hash.known().unwrap()
            );
            assert_eq!(
                link.final_tree_hash,
                *parent.final_tree_hash.known().unwrap()
            );
            assert_eq!(segment.initial_tree_hash, link.final_tree_hash);
            assert_eq!(
                segment.time.inter_attempt_time,
                RprovInterAttemptTime::Unknown
            );
        }
    }

    let latest = create_bundle(&third_receipt, &fixture.0.join("latest.zip"))
        .unwrap()
        .path;
    let expected = BTreeMap::from([("main.rs".to_owned(), b"DCBA".to_vec())]);
    let report = assert_clean_consumers(Some("revisions-three"), &latest, &expected);
    assert_eq!(report.review_indicators.unwrap().attempts.len(), 3);
    let imported = import_rprov(Cursor::new(fs::read(&latest).unwrap())).unwrap();
    assert_eq!(imported.outer_source_files().len(), 1);
    assert_eq!(imported.outer_source_files()[0].path.as_str(), "main.rs");

    let clean_rprov = stored_zip_entry(&fs::read(&latest).unwrap(), "session.rprov");
    let (clean_manifest, payloads) = collect_rprov(&clean_rprov);
    enum RevisionMutation {
        MissingLink,
        TamperedLink,
        Reordered,
        Duplicate,
        Cyclic,
        ChildStart,
        AggregateBudget,
    }
    let mutations = [
        ("revision-missing-link.rprov", RevisionMutation::MissingLink),
        (
            "revision-tampered-link.rprov",
            RevisionMutation::TamperedLink,
        ),
        ("revision-reordered.rprov", RevisionMutation::Reordered),
        ("revision-duplicate.rprov", RevisionMutation::Duplicate),
        ("revision-cyclic.rprov", RevisionMutation::Cyclic),
        (
            "revision-child-start-mismatch.rprov",
            RevisionMutation::ChildStart,
        ),
        (
            "revision-aggregate-over-budget.rprov",
            RevisionMutation::AggregateBudget,
        ),
    ];
    for (name, mutation) in mutations {
        let mut changed = clean_manifest.clone();
        match mutation {
            RevisionMutation::MissingLink => changed.segments[1].parent = None,
            RevisionMutation::TamperedLink => {
                changed.segments[1]
                    .parent
                    .as_mut()
                    .unwrap()
                    .terminal_event_hash = Hash::zero();
            }
            RevisionMutation::Reordered => changed.segments.swap(1, 2),
            RevisionMutation::Duplicate => changed.segments.push(changed.segments[1].clone()),
            RevisionMutation::Cyclic => {
                changed.segments[1].parent.as_mut().unwrap().session_id =
                    changed.segments[1].session_id.clone();
            }
            RevisionMutation::ChildStart => {
                changed.segments[1].initial_tree_hash = Hash::zero();
            }
            RevisionMutation::AggregateBudget => {
                changed.aggregate_event_count = MAX_RPROV_EVENTS + 1;
            }
        }
        let path = fixture.0.join(name);
        fs::write(&path, encode_rprov_unchecked(&changed, &payloads)).unwrap();
        assert_package_invalid(&path);
    }

    let mut scan = Vec::new();
    run_scan(&[fixture.0.to_string_lossy().into_owned()], &mut scan).unwrap();
    let scan = String::from_utf8(scan).unwrap();
    for name in [
        "revision-missing-link.rprov",
        "revision-tampered-link.rprov",
        "revision-reordered.rprov",
        "revision-duplicate.rprov",
        "revision-cyclic.rprov",
        "revision-child-start-mismatch.rprov",
        "revision-aggregate-over-budget.rprov",
    ] {
        let display_key = name.chars().take(24).collect::<String>();
        let row = scan
            .lines()
            .find(|line| line.contains(&display_key))
            .unwrap_or_else(|| panic!("missing {name} from scan:\n{scan}"));
        assert!(row.contains("PACKAGE_INVALID"), "{row}");
        assert!(
            row.contains(&expected_package_detail(
                Path::new(name).file_stem().unwrap().to_str().unwrap()
            )),
            "{row}"
        );
    }

    fs::create_dir_all(third_root.join("target/debug")).unwrap();
    fs::write(third_root.join("target/debug/discardable"), b"temporary").unwrap();
    let cleanup = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("cleanup")
        .arg(&third_root)
        .arg("--confirm")
        .output()
        .unwrap();
    command_ok(&cleanup, "linked attempt cleanup");
    assert!(!third_root.join("target").exists());
    assert!(third_root.join(".rustrace").exists());
    assert_clean_consumers(Some("cleanup-preservation"), &latest, &expected);
}
