//! Format 3 runs through the production session: packaged cases with
//! arguments, closed or hashed stdin, and a pinned fixture working directory;
//! console Runs with literal arguments; and the refusals and warnings that
//! guard the fixture folder.
#![cfg(unix)]

use rustrace::{
    session::{ConsoleStart, ProductionSession, TestCaseOutcome, create_bundle},
    verify::{
        AssignmentReferenceStatus, TestCaseEvidenceStatus, VerificationIssueKind, verify_path,
    },
};
use rustrace_model::{Hash, rprov_raw_blake3, test_case_args_blake3};
use rustrace_workspace::assignment_package::{ExtractionLimits, extract_assignment_package};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const MANIFEST_V3: &str = r#"format_version = 3
course_id = "course"
assignment_id = "format3-runner"
assignment_version = "v1"
title = "Format 3 runner"
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

const CARGO_TOML: &[u8] = b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n";
const DATA: &[u8] = b"fixture data\n";

/// What the report-mode fake program prints when run from `lab.test-cases/files`.
fn report(args: &str, stdin: &str, data: &str) -> Vec<u8> {
    format!("cwd=lab.test-cases/files\nargs={args}\nstdin={stdin}\ndata={data}").into_bytes()
}

/// Every packaged case-folder file, relative to `test-cases/`.
fn package_cases() -> Vec<(String, Vec<u8>)> {
    let data = String::from_utf8(DATA.to_vec()).unwrap();
    vec![
        ("no-input.args".into(), b"--count\n".to_vec()),
        (
            "no-input.expected".into(),
            report("--count", "closed:", &data),
        ),
        ("plain.expected".into(), report("", "closed:", &data)),
        ("with-input.args".into(), b"-i\ntwo words\n".to_vec()),
        ("with-input.in".into(), b"alpha\nbeta\n".to_vec()),
        (
            "with-input.expected".into(),
            report("-i|two words", "pipe:alpha\nbeta\n", &data),
        ),
        ("files/data.txt".into(), DATA.to_vec()),
        ("files/sub/note.txt".into(), b"note\n".to_vec()),
    ]
}

fn tar(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut archive = Vec::new();
    for (name, bytes) in entries {
        let mut header = [0_u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        for (range, value) in [
            (100..108, 0o644),
            (108..116, 0),
            (116..124, 0),
            (124..136, bytes.len() as u64),
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
        archive.extend_from_slice(bytes);
        archive.resize(archive.len().next_multiple_of(512), 0);
    }
    archive.resize(archive.len() + 1024, 0);
    archive
}

/// A deployed format 3 workspace `lab.work` with its own `lab.test-cases/`,
/// exactly as `rustrace work` leaves them, and the fake tools.
fn format3_fixture(name: &str) -> (PathBuf, PathBuf) {
    format3_fixture_with(name, |_| package_cases())
}

/// Like [`format3_fixture`], with the case-folder files `cases` returns for
/// the fixture's parent directory name.
fn format3_fixture_with(
    name: &str,
    cases: impl FnOnce(&str) -> Vec<(String, Vec<u8>)>,
) -> (PathBuf, PathBuf) {
    let parent =
        std::env::temp_dir().join(format!("rustrace-format3-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&parent);
    let root = parent.join("lab.work");
    fs::create_dir_all(root.join("target/bin/v1")).unwrap();
    let parent = fs::canonicalize(parent).unwrap();
    let root = parent.join("lab.work");
    fs::write(root.join("Cargo.toml"), CARGO_TOML).unwrap();
    fs::write(root.join("Cargo.lock"), b"fixture").unwrap();
    fs::write(root.join("main.rs"), b"").unwrap();
    let mut entries = vec![
        (
            "assignment.toml".to_owned(),
            MANIFEST_V3.as_bytes().to_vec(),
        ),
        ("starter/Cargo.toml".to_owned(), CARGO_TOML.to_vec()),
        ("starter/Cargo.lock".to_owned(), b"fixture".to_vec()),
        ("starter/main.rs".to_owned(), Vec::new()),
    ];
    let cases = cases(parent.file_name().unwrap().to_str().unwrap());
    entries.extend(
        cases
            .iter()
            .map(|(name, bytes)| (format!("test-cases/{name}"), bytes.clone())),
    );
    fs::write(parent.join("lab.rta"), tar(&entries)).unwrap();
    let folder = parent.join("lab.test-cases");
    for (name, bytes) in &cases {
        let path = folder.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    let suite = extracted(&parent).test_cases.unwrap();
    fs::write(
        folder.join(".rustrace-cases.json"),
        format!(
            "{{\"version\":1,\"workspace\":\"lab.work\",\"test_case_suite_hash\":\"{}\"}}\n",
            suite.hash
        ),
    )
    .unwrap();
    fs::write(
        root.join("target/runner-fixture.json"),
        br#"{"mode":"console_io","mutate_source":false,"echo":true,"report":true}"#,
    )
    .unwrap();
    install_tools(&root);
    (parent, root)
}

fn extracted(parent: &Path) -> rustrace_workspace::assignment_package::ExtractedAssignment {
    let destination = parent.join(format!(
        "extraction-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    extract_assignment_package(
        fs::File::open(parent.join("lab.rta")).unwrap(),
        &destination,
        ExtractionLimits::default(),
    )
    .unwrap()
}

fn install_tools(root: &Path) {
    let script = include_bytes!("support/command_rustup.py");
    for path in std::iter::once(root.join("target/bin/rustup")).chain(
        [
            "rustc",
            "cargo",
            "rustdoc",
            "rust-analyzer",
            "cargo-clippy",
            "cargo-fmt",
            "rustfmt",
        ]
        .map(|tool| root.join("target/bin/v1").join(tool)),
    ) {
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn run_child(root: &Path, exact_test: &str) {
    let bin = root.join("target/bin");
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", exact_test, "--nocapture"])
        .env("RUSTRACE_FORMAT3_FIXTURE", root)
        .env("PATH", path)
        .env("RUSTUP_AUTO_INSTALL", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{exact_test}: fixture retained {}; stdout={}; stderr={}",
        root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn child_root() -> Option<PathBuf> {
    std::env::var_os("RUSTRACE_FORMAT3_FIXTURE").map(PathBuf::from)
}

fn wait(session: &mut ProductionSession) {
    let until = Instant::now() + Duration::from_secs(20);
    while session.command_active() && Instant::now() < until {
        session.poll_command().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!session.command_active(), "fixture exceeded its hang guard");
}

/// A console Run reads submitted lines; close stdin so the fake finishes.
fn finish_console_run(session: &mut ProductionSession) {
    let until = Instant::now() + Duration::from_secs(20);
    while session.command_active() && !session.console_accepts_stdin() && Instant::now() < until {
        session.poll_command().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    if session.console_accepts_stdin() {
        assert!(session.close_console_stdin().unwrap());
    }
    wait(session);
}

fn run_case(session: &mut ProductionSession, name: &str) -> (TestCaseOutcome, Option<String>) {
    let case = session
        .list_test_cases()
        .unwrap()
        .into_iter()
        .find(|case| case.name() == name)
        .unwrap();
    session.start_test_case(case).unwrap();
    let warning = session.take_run_warning();
    wait(session);
    let result = session.take_test_case_result().unwrap();
    assert_eq!(result.case.name(), name);
    (result.outcome, warning)
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

fn payloads<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["event"]["type"] == kind)
        .map(|event| &event["event"]["payload"])
        .collect()
}

fn stdout_of(events: &[Value], command_id: &Value) -> Vec<u8> {
    payloads(events, "controlled_command_output")
        .into_iter()
        .filter(|output| output["command_id"] == *command_id && output["stream"] == "stdout")
        .flat_map(|output| {
            let hex = output["bytes_hex"].as_str().unwrap();
            (0..hex.len())
                .step_by(2)
                .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn format3_runner_parent() {
    if child_root().is_some() {
        return;
    }
    let (parent, root) = format3_fixture("runner");
    run_child(&root, "format3_runner_child");
    fs::remove_dir_all(parent).unwrap();
}

#[test]
fn format3_runner_child() {
    let Some(root) = child_root() else {
        return;
    };
    let parent = root.parent().unwrap().to_owned();
    let folder = parent.join("lab.test-cases");
    let files = folder.join("files");
    let reference = parent.join("lab.rta");
    let extracted = extracted(&parent);
    let packaged = extracted
        .test_cases
        .as_ref()
        .unwrap()
        .fixtures
        .as_ref()
        .unwrap()
        .hash();
    let mut session = ProductionSession::start_from_assignment(&root, &extracted).unwrap();
    assert_eq!(session.packaged_fixtures_hash(), Some(packaged));

    // Format 3 lists every case with a `.expected`, with or without `.in`.
    assert_eq!(
        session
            .list_test_cases()
            .unwrap()
            .iter()
            .map(|case| case.name().to_owned())
            .collect::<Vec<_>>(),
        ["no-input", "plain", "with-input"]
    );
    for name in ["no-input", "plain", "with-input"] {
        assert_eq!(
            run_case(&mut session, name),
            (TestCaseOutcome::Pass, None),
            "{name}"
        );
    }

    // Console Runs start in the fixture folder too, with or without
    // arguments, so a case can be reproduced exactly.
    assert_eq!(
        session.start_console_command("cargo run -- a b").unwrap(),
        ConsoleStart::Started
    );
    assert_eq!(session.take_run_warning(), None);
    finish_console_run(&mut session);
    let invocation: Value =
        serde_json::from_slice(&fs::read(root.join("target/invocation.json")).unwrap()).unwrap();
    assert_eq!(invocation["working_directory"], files.to_str().unwrap());
    assert_eq!(invocation["program_args"], json!(["a", "b"]));
    assert_eq!(
        session
            .start_console_command("cargo run --release -- -i x < with-input.in > out.txt")
            .unwrap(),
        ConsoleStart::Started
    );
    wait(&mut session);
    assert_eq!(
        fs::read(folder.join("out.txt")).unwrap(),
        report("-i|x", "file:alpha\nbeta\n", "fixture data\n")
    );
    // Other console commands still run in the workspace.
    assert_eq!(
        session.start_console_command("cargo check").unwrap(),
        ConsoleStart::Started
    );
    wait(&mut session);
    let invocation: Value =
        serde_json::from_slice(&fs::read(root.join("target/invocation.json")).unwrap()).unwrap();
    assert_eq!(invocation["working_directory"], root.to_str().unwrap());

    // Rustrace's marker and the fixture tree are not output targets.
    for line in [
        "cargo run > files/out.txt",
        "cargo run -- x > FILES/sub/out.txt",
        "cargo run > .rustrace-cases.json",
    ] {
        let error = session.start_console_command(line).unwrap_err().to_string();
        assert!(
            error.contains("choose another file name"),
            "{line}: {error}"
        );
    }
    assert!(!files.join("out.txt").exists());

    // A changed fixture tree still runs, with a warning, and the Run records
    // the deployed tree it actually used.
    fs::write(files.join("data.txt"), b"student edit\n").unwrap();
    let (outcome, warning) = run_case(&mut session, "no-input");
    assert!(matches!(outcome, TestCaseOutcome::Fail(_)), "{outcome:?}");
    assert_eq!(
        warning.as_deref(),
        Some(
            "warning: the files in lab.test-cases/files differ from the assignment package; the case runs with them as they are and will not verify against the package. To restore them, remove the files you changed or added, then quit and resume the workspace"
        )
    );
    assert_eq!(
        session.start_console_command("cargo run").unwrap(),
        ConsoleStart::Started
    );
    assert!(
        session
            .take_run_warning()
            .unwrap()
            .contains("; your program runs with them as they are.")
    );
    assert_eq!(session.take_run_warning(), None, "shown once");
    finish_console_run(&mut session);
    fs::write(files.join("data.txt"), DATA).unwrap();

    // Cargo configuration that a fixture Run would read differently from
    // workspace commands is refused before anything starts.
    fs::create_dir(root.join(".cargo")).unwrap();
    let error = session
        .start_test_case(session.list_test_cases().unwrap().remove(0))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("remove `.cargo` from the workspace"),
        "{error}"
    );
    let error = session
        .start_console_command("cargo run -- a")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("remove `.cargo` from the workspace"),
        "{error}"
    );
    assert!(!session.command_active());
    fs::remove_dir(root.join(".cargo")).unwrap();
    for configuration in [folder.join(".cargo"), files.join(".cargo")] {
        fs::create_dir(&configuration).unwrap();
        let error = session
            .start_console_command("cargo run")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Cargo would read it as configuration"),
            "{error}"
        );
        fs::remove_dir(&configuration).unwrap();
    }

    // A missing fixture folder leaves the Run nowhere to start.
    fs::rename(&files, folder.join("files-aside")).unwrap();
    let error = session
        .start_test_case(session.list_test_cases().unwrap().remove(0))
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "the fixture folder lab.test-cases/files is missing; quit and resume the workspace to deploy it again"
    );
    fs::rename(folder.join("files-aside"), &files).unwrap();
    assert_eq!(
        run_case(&mut session, "plain"),
        (TestCaseOutcome::Pass, None)
    );

    session.save_all().unwrap();
    let receipt = session.finalize("student-1").unwrap();
    let bundle = parent.join("session.zip");
    create_bundle(&receipt, &bundle).unwrap();

    let events = journal_events(&root);
    let starts = payloads(&events, "controlled_command_started");
    let comparisons = payloads(&events, "test_case_compared");
    assert_eq!(starts.len(), 9, "{starts:#?}");
    assert_eq!(comparisons.len(), 5);
    let fixtures = |hash: Hash| json!({"kind": "fixtures", "fixtures_blake3": hash.to_string()});
    let manifest_path = "../../lab.work/Cargo.toml";
    let tail = |start: &Value| start["argv"].as_array().unwrap()[4..].to_vec();
    // Packaged cases: closed stdin, arguments, and hashed input.
    assert_eq!(
        starts[0]["console"],
        json!({"stdin": {"kind": "closed"}, "stdout": {"kind": "console"},
            "args": ["--count"], "working_directory": fixtures(packaged),
            "test_case": "no-input"})
    );
    assert_eq!(
        tail(starts[0]),
        [
            "run",
            "--locked",
            "--manifest-path",
            manifest_path,
            "--",
            "--count"
        ]
        .map(Value::from)
    );
    assert_eq!(
        starts[1]["console"],
        json!({"stdin": {"kind": "closed"}, "stdout": {"kind": "console"},
            "working_directory": fixtures(packaged), "test_case": "plain"})
    );
    assert_eq!(
        tail(starts[1]),
        ["run", "--locked", "--manifest-path", manifest_path].map(Value::from)
    );
    assert_eq!(
        starts[2]["console"],
        json!({"stdin": {"kind": "file", "path": "with-input.in"},
            "stdout": {"kind": "console"}, "args": ["-i", "two words"],
            "working_directory": fixtures(packaged), "test_case": "with-input"})
    );
    assert_eq!(
        comparisons[2]["invocation"],
        json!({
            "args_blake3": test_case_args_blake3(&["-i", "two words"]).to_string(),
            "stdin": {"kind": "file", "blake3": rprov_raw_blake3(b"alpha\nbeta\n").to_string()},
            "fixtures_blake3": packaged.to_string(),
        })
    );
    assert_eq!(
        comparisons[1]["invocation"],
        json!({
            "args_blake3": test_case_args_blake3::<&str>(&[]).to_string(),
            "stdin": {"kind": "closed"},
            "fixtures_blake3": packaged.to_string(),
        })
    );
    // Console Runs: unmarked, their arguments recorded, in the fixture folder.
    assert_eq!(
        starts[3]["console"],
        json!({"stdin": {"kind": "submitted"}, "stdout": {"kind": "console"},
            "args": ["a", "b"], "working_directory": fixtures(packaged)})
    );
    assert_eq!(
        tail(starts[3]),
        [
            "run",
            "--locked",
            "--manifest-path",
            manifest_path,
            "--",
            "a",
            "b"
        ]
        .map(Value::from)
    );
    assert_eq!(
        stdout_of(&events, &starts[3]["command_id"]),
        report("a|b", "pipe:", "fixture data\n")
    );
    assert_eq!(
        starts[4]["console"],
        json!({"stdin": {"kind": "file", "path": "with-input.in"},
            "stdout": {"kind": "file", "path": "out.txt"},
            "args": ["-i", "x"], "working_directory": fixtures(packaged)})
    );
    assert_eq!(
        tail(starts[4]),
        [
            "run",
            "--release",
            "--locked",
            "--manifest-path",
            manifest_path,
            "--",
            "-i",
            "x"
        ]
        .map(Value::from)
    );
    assert_eq!(
        starts[5]["console"],
        json!({"stdin": {"kind": "closed"}, "stdout": {"kind": "console"}})
    );
    // The changed tree's Runs record its deployed hash, not the package's.
    let changed = starts[6]["console"]["working_directory"]["fixtures_blake3"].clone();
    assert_ne!(changed, Value::from(packaged.to_string()));
    assert_eq!(
        starts[7]["console"]["working_directory"]["fixtures_blake3"],
        changed
    );
    assert_eq!(comparisons[3]["invocation"]["fixtures_blake3"], changed);
    assert_eq!(comparisons[3]["outcome"]["kind"], "mismatch");
    assert_eq!(
        starts[8]["console"]["working_directory"],
        fixtures(packaged),
        "the restored tree matches the package again"
    );

    // The evidence replays; only the changed-tree case fails the reference.
    let recorded = verify_path(&bundle, None);
    assert!(recorded.is_clean(), "{recorded:#?}");
    let report = verify_path(&bundle, Some(&reference));
    assert_eq!(
        report.assignment_reference,
        AssignmentReferenceStatus::Mismatch,
        "{report:#?}"
    );
    assert_eq!(
        report.test_case_evidence,
        Some(TestCaseEvidenceStatus::Recorded)
    );
    assert!(report.issues.iter().any(|issue| {
        issue.kind == VerificationIssueKind::AssignmentReference
            && issue.detail == "fixture tree mismatch for test case no-input"
    }));
}

#[test]
fn format3_without_fixtures_parent() {
    if child_root().is_some() {
        return;
    }
    let (parent, root) = format3_fixture_with("without-fixtures", |parent| {
        vec![
            ("spaced.args".into(), b"x\ny z\n".to_vec()),
            (
                "spaced.expected".into(),
                format!("cwd={parent}/lab.work\nargs=x|y z\nstdin=closed:\ndata=none\n")
                    .into_bytes(),
            ),
        ]
    });
    run_child(&root, "format3_without_fixtures_child");
    fs::remove_dir_all(parent).unwrap();
}

/// A format 3 package without `files/` runs its cases, and console Runs, in
/// the workspace, and its evidence still verifies against the package.
#[test]
fn format3_without_fixtures_child() {
    let Some(root) = child_root() else {
        return;
    };
    let parent = root.parent().unwrap().to_owned();
    let extracted = extracted(&parent);
    assert!(extracted.test_cases.as_ref().unwrap().fixtures.is_none());
    let mut session = ProductionSession::start_from_assignment(&root, &extracted).unwrap();
    assert_eq!(session.packaged_fixtures_hash(), None);
    // A stray `files/` on disk never moves the Run: the package decides.
    fs::create_dir(parent.join("lab.test-cases/files")).unwrap();
    assert_eq!(
        run_case(&mut session, "spaced"),
        (TestCaseOutcome::Pass, None)
    );
    // A workspace `.cargo` matters only for Runs from a fixture folder.
    fs::create_dir(root.join(".cargo")).unwrap();
    assert_eq!(
        session.start_console_command("cargo run -- q").unwrap(),
        ConsoleStart::Started
    );
    finish_console_run(&mut session);
    fs::remove_dir(root.join(".cargo")).unwrap();
    let invocation: Value =
        serde_json::from_slice(&fs::read(root.join("target/invocation.json")).unwrap()).unwrap();
    assert_eq!(invocation["working_directory"], root.to_str().unwrap());
    session.save_all().unwrap();
    let receipt = session.finalize("student-1").unwrap();
    let bundle = parent.join("session.zip");
    create_bundle(&receipt, &bundle).unwrap();

    let events = journal_events(&root);
    let starts = payloads(&events, "controlled_command_started");
    assert_eq!(
        starts[0]["console"],
        json!({"stdin": {"kind": "closed"}, "stdout": {"kind": "console"},
            "args": ["x", "y z"], "test_case": "spaced"})
    );
    assert_eq!(
        starts[0]["argv"].as_array().unwrap()[4..],
        ["run", "--locked", "--", "x", "y z"].map(Value::from)
    );
    assert_eq!(
        starts[1]["console"],
        json!({"stdin": {"kind": "submitted"}, "stdout": {"kind": "console"}, "args": ["q"]})
    );
    let comparisons = payloads(&events, "test_case_compared");
    assert_eq!(
        comparisons[0]["invocation"],
        json!({"args_blake3": test_case_args_blake3(&["x", "y z"]).to_string(),
            "stdin": {"kind": "closed"}})
    );
    let report = verify_path(&bundle, Some(&parent.join("lab.rta")));
    assert!(report.is_clean(), "{report:#?}");
    assert_eq!(
        report.test_case_evidence,
        Some(TestCaseEvidenceStatus::ReferenceVerified)
    );
}

#[test]
fn format3_fixture_change_during_start_parent() {
    if child_root().is_some() {
        return;
    }
    let (parent, root) = format3_fixture("start-race");
    run_child(&root, "format3_fixture_change_during_start_child");
    fs::remove_dir_all(parent).unwrap();
}

/// The fixture tree is hashed again just before the start is recorded; a
/// change after the student was told it matches cancels that launch.
#[test]
fn format3_fixture_change_during_start_child() {
    let Some(root) = child_root() else {
        return;
    };
    let parent = root.parent().unwrap().to_owned();
    let files = parent.join("lab.test-cases/files");
    let mut session = ProductionSession::start_from_assignment(&root, &extracted(&parent)).unwrap();
    fs::write(
        root.join("target/runner-fixture.json"),
        br#"{"mode":"console_io","mutate_source":false,"echo":true,"report":true,"resolve_delay_millis":400}"#,
    )
    .unwrap();
    let case = session.list_test_cases().unwrap().remove(0);
    session.start_test_case(case).unwrap();
    assert_eq!(session.take_run_warning(), None);
    let resolving = root.join("target/resolving");
    let until = Instant::now() + Duration::from_secs(20);
    while !resolving.exists() && Instant::now() < until {
        session.poll_command().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(resolving.exists());
    fs::write(files.join("data.txt"), b"changed while starting\n").unwrap();
    let mut error = None;
    let until = Instant::now() + Duration::from_secs(20);
    while session.command_active() && Instant::now() < until {
        if let Err(failure) = session.poll_command() {
            error = Some(failure.to_string());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        error.as_deref(),
        Some("the files in lab.test-cases/files changed while the run was starting; run it again")
    );
    let result = session.take_test_case_result().unwrap();
    assert_eq!(
        result.outcome,
        TestCaseOutcome::Error("could not start".into())
    );
    assert!(
        !root.join("target/invocation.json").exists(),
        "Cargo never ran"
    );
    session.quit().unwrap();
    let events = journal_events(&root);
    assert!(payloads(&events, "controlled_command_started").is_empty());
}

#[test]
fn console_arguments_in_a_workspace_run_parent() {
    if child_root().is_some() {
        return;
    }
    let parent =
        std::env::temp_dir().join(format!("rustrace-console-arguments-{}", std::process::id()));
    let _ = fs::remove_dir_all(&parent);
    let root = parent.join("assignment.work");
    fs::create_dir_all(root.join("target/bin/v1")).unwrap();
    fs::create_dir(parent.join("test-cases")).unwrap();
    fs::write(parent.join("test-cases/input.txt"), b"typed\n").unwrap();
    fs::write(root.join("main.rs"), b"").unwrap();
    fs::write(root.join("Cargo.lock"), b"fixture").unwrap();
    fs::write(
        root.join("target/runner-fixture.json"),
        br#"{"mode":"console_io","mutate_source":false,"echo":true,"report":true}"#,
    )
    .unwrap();
    install_tools(&root);
    run_child(&root, "console_arguments_in_a_workspace_run_child");
    fs::remove_dir_all(parent).unwrap();
}

/// Lab 1 style (no fixture tree): `cargo run -- ARG...` records its
/// arguments and still runs in the workspace.
#[test]
fn console_arguments_in_a_workspace_run_child() {
    let Some(root) = child_root() else {
        return;
    };
    let root = fs::canonicalize(root).unwrap();
    let manifest = MANIFEST_V3.replacen("format_version = 3", "format_version = 1", 1);
    let mut session = ProductionSession::start(&root, manifest.as_bytes()).unwrap();
    assert_eq!(
        session
            .start_console_command("cargo run -- -n fn=main < input.txt")
            .unwrap(),
        ConsoleStart::Started
    );
    assert_eq!(session.take_run_warning(), None);
    wait(&mut session);
    let invocation: Value =
        serde_json::from_slice(&fs::read(root.join("target/invocation.json")).unwrap()).unwrap();
    assert_eq!(invocation["working_directory"], root.to_str().unwrap());
    assert_eq!(invocation["program_args"], json!(["-n", "fn=main"]));
    for line in [
        "cargo run -- a < in.txt extra",
        "cargo run < input.txt -- a",
        "cargo run -- 'quoted'",
        "cargo run -- *.rs",
    ] {
        assert!(session.start_console_command(line).is_err(), "{line}");
    }
    session.quit().unwrap();
    let events = journal_events(&root);
    let starts = payloads(&events, "controlled_command_started");
    assert_eq!(starts.len(), 1);
    assert_eq!(
        starts[0]["console"],
        json!({"stdin": {"kind": "file", "path": "input.txt"},
            "stdout": {"kind": "console"}, "args": ["-n", "fn=main"]})
    );
    assert_eq!(
        starts[0]["argv"].as_array().unwrap()[4..],
        ["run", "--locked", "--", "-n", "fn=main"].map(Value::from)
    );
    let workspace_name = root.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        stdout_of(&events, &starts[0]["command_id"]),
        format!(
            "cwd={}/{workspace_name}\nargs=-n|fn=main\nstdin=file:typed\n\ndata=none\n",
            root.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        )
        .into_bytes()
    );
}
