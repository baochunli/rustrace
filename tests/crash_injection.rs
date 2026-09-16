//! Real process-interruption coverage for production durability boundaries.
//!
//! These tests prove recovery after process termination. They do not simulate
//! controller, drive, or filesystem power loss and make no such guarantee.
#![cfg(all(unix, feature = "process-probes"))]

#[path = "support/test_home.rs"]
mod test_home;
use rustrace::{
    cargo_policy::CargoAction,
    session::{
        FinalizationReceipt, FinalizationStatus, ProductionSession, ResumeChoice, create_bundle,
        run_submit,
    },
    tui::EditorCommand,
    verify::{SubmittedSourceStatus, VerificationStatus, verify_path},
};
use rustrace_journal::{Journal, MAX_EVENTS_PER_READ};
use rustrace_model::{Event, SessionId, encode_rprov_manifest};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

const DEFAULT_RANDOM_ITERATIONS: usize = 12;
const RANDOM_SEED: u64 = 0x8_4c12_a55e_d15c;
const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "crash-injection"
assignment_version = "v1"
title = "Crash injection"
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

struct Fixture {
    home: test_home::TestHome,
    base: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "rustrace-crash-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let root = base.join("workspace");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("main.rs"), "A").unwrap();
        Self {
            home: test_home::TestHome::new(false),
            base: fs::canonicalize(base).unwrap(),
            root: fs::canonicalize(root).unwrap(),
        }
    }

    fn marker(&self, label: &str) -> PathBuf {
        self.base.join(format!("{label}.ready"))
    }

    fn destination(&self, label: &str) -> PathBuf {
        self.base.join(format!("{label}.zip"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CrashKind {
    Journal,
    Checkpoint,
    Cargo,
    Format,
    Finalization,
    Archive,
}

impl CrashKind {
    const ALL: [Self; 6] = [
        Self::Journal,
        Self::Checkpoint,
        Self::Cargo,
        Self::Format,
        Self::Finalization,
        Self::Archive,
    ];

    const fn mode(self) -> &'static str {
        match self {
            Self::Journal => "journal",
            Self::Checkpoint => "checkpoint",
            Self::Cargo => "cargo",
            Self::Format => "format",
            Self::Finalization => "finalization",
            Self::Archive => "archive",
        }
    }
}

fn install_tool_fixture(root: &Path, mode: &str) {
    let bin = root.join("target/bin");
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
    fs::write(root.join("Cargo.lock"), mode).unwrap();
    fs::write(
        root.join("target/runner-fixture.json"),
        serde_json::to_vec(&serde_json::json!({"mode": mode, "exit": 0})).unwrap(),
    )
    .unwrap();
}

fn child_path(root: &Path) -> OsString {
    let bin = root.join("target/bin");
    std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap()
}

fn spawn_child(
    fixture: &Fixture,
    kind: CrashKind,
    stage: &str,
    marker: &Path,
    resume: bool,
) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "crash_injection_child", "--nocapture"])
        .env("RUSTRACE_CRASH_ROOT", &fixture.root)
        .env("RUSTRACE_CRASH_MODE", kind.mode())
        .env("RUSTRACE_CRASH_STAGE", stage)
        .env("RUSTRACE_CRASH_MARKER", marker)
        .env(
            "RUSTRACE_CRASH_DESTINATION",
            fixture.destination("submission"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if matches!(kind, CrashKind::Cargo | CrashKind::Format) {
        command.env("PATH", child_path(&fixture.root));
    }
    if resume {
        command.env("RUSTRACE_CRASH_RESUME", "1");
    }
    command.spawn().unwrap()
}

fn spawn_submit_binary(fixture: &Fixture, label: &str, marker: &Path) -> Child {
    fixture
        .home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("submit")
        .arg(&fixture.root)
        .args(["--student-id", "student-1", "--output"])
        .arg(fixture.destination(label))
        .env("RUSTRACE_CRASH_STAGE", "archive-before-install")
        .env("RUSTRACE_CRASH_MARKER", marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn wait_for_marker(child: &mut Child, marker: &Path, stage: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if marker_ready(marker, stage) {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child exited before {stage}: {status}");
        }
        thread::sleep(Duration::from_millis(5));
    }
    let _ = unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    let status = child.wait().unwrap();
    panic!("child never reached {stage}; status={status}");
}

fn kill_child(child: Child, context: &str) {
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGKILL),
        "{context} was not killed with SIGKILL; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct KillOutcome {
    elapsed: Duration,
    marker_reached: bool,
}

fn kill_at_boundary(fixture: &Fixture, kind: CrashKind, stage: &str) -> KillOutcome {
    let marker = fixture.marker(stage);
    let started = Instant::now();
    let mut child = spawn_child(fixture, kind, stage, &marker, false);
    wait_for_marker(&mut child, &marker, stage);
    kill_child(child, &format!("{kind:?}/{stage}"));
    KillOutcome {
        elapsed: started.elapsed(),
        marker_reached: true,
    }
}

fn kill_at_elapsed_offset(
    fixture: &Fixture,
    kind: CrashKind,
    stage: &str,
    delay_millis: u64,
) -> KillOutcome {
    let marker = fixture.marker(stage);
    let started = Instant::now();
    let mut child = spawn_child(fixture, kind, stage, &marker, true);
    let target = started + Duration::from_millis(delay_millis);
    let marker_reached = loop {
        if marker_ready(&marker, stage) {
            break true;
        }
        if Instant::now() >= target {
            break false;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("{kind:?}/{stage} exited before termination: {status}");
        }
        thread::sleep(Duration::from_millis(1));
    };
    kill_child(child, &format!("{kind:?}/{stage}"));
    KillOutcome {
        elapsed: started.elapsed(),
        marker_reached,
    }
}

fn marker_ready(marker: &Path, stage: &str) -> bool {
    fs::read_to_string(marker).is_ok_and(|contents| contents.starts_with(stage))
}

fn session_journal(root: &Path) -> (SessionId, PathBuf) {
    let path = fs::read_dir(root.join(".rustrace"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "sqlite")
        })
        .expect("session journal");
    let id = SessionId::new(path.file_stem().unwrap().to_str().unwrap()).unwrap();
    (id, path)
}

fn assert_unique_valid_prefix(root: &Path) -> (u64, usize) {
    let (id, path) = session_journal(root);
    let mut journal = Journal::open_read_only_no_follow(&path).unwrap();
    let chain = journal.verify_session_chain(&id).unwrap();
    journal.verify_session_checkpoints(&id).unwrap();
    let mut sequences = BTreeSet::new();
    let mut terminals = 0;
    for first in (1..=chain.event_count).step_by(MAX_EVENTS_PER_READ) {
        for event in journal
            .read_events(&id, first, MAX_EVENTS_PER_READ)
            .unwrap()
        {
            assert!(sequences.insert(event.sequence), "duplicate sequence");
            terminals += usize::from(matches!(event.event, Event::SubmissionFinalized(_)));
        }
    }
    assert_eq!(sequences.len() as u64, chain.event_count);
    assert_eq!(
        sequences.into_iter().collect::<Vec<_>>(),
        (1..=chain.event_count).collect::<Vec<_>>()
    );
    (chain.event_count, terminals)
}

fn assert_resumes_and_continues(fixture: &Fixture) {
    let before = assert_unique_valid_prefix(&fixture.root).0;
    let mut resumed =
        ProductionSession::resume(&fixture.root, MANIFEST, ResumeChoice::Resume).unwrap();
    assert!(resumed.health().unwrap().events >= before);
    resumed.execute(EditorCommand::Insert('C')).unwrap();
    resumed.quit().unwrap();
    assert!(assert_unique_valid_prefix(&fixture.root).0 > before);
}

fn assert_clear_command_recovery(fixture: &Fixture) {
    let inspection = ProductionSession::inspect(&fixture.root).unwrap();
    assert!(!inspection.logical.is_empty());
    let error = ProductionSession::resume(&fixture.root, MANIFEST, ResumeChoice::Resume)
        .err()
        .expect("unfinished command must not resume as completed");
    assert!(error.to_string().contains("unfinished command"), "{error}");
    let destination = fixture.destination("should-not-exist");
    let mut output = Vec::new();
    let submit = run_submit(
        &[
            fixture.root.to_string_lossy().into_owned(),
            "--student-id".to_owned(),
            "student-1".to_owned(),
            "--output".to_owned(),
            destination.to_string_lossy().into_owned(),
        ],
        &mut output,
    );
    assert!(
        submit.is_err(),
        "unfinished command produced a clean package"
    );
    assert!(!destination.exists());
    assert_eq!(assert_unique_valid_prefix(&fixture.root).1, 0);
}

fn finalization_bytes(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(root.join(".rustrace"))
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            name.starts_with("finalization-")
                .then(|| (name, fs::read(entry.path()).unwrap()))
        })
        .collect()
}

fn finalized(status: FinalizationStatus) -> FinalizationReceipt {
    match status {
        FinalizationStatus::Finalized(receipt) => *receipt,
        FinalizationStatus::Incomplete(incomplete) => {
            panic!("expected exact finalization recovery, got {incomplete:?}")
        }
    }
}

fn receipt_bytes(receipt: &FinalizationReceipt) -> (Vec<u8>, BTreeMap<String, Vec<u8>>) {
    let manifest = encode_rprov_manifest(receipt.manifest()).unwrap();
    let payloads = receipt
        .payloads()
        .iter()
        .map(|payload| {
            (
                payload.entry.clone(),
                receipt.read_payload(&payload.entry).unwrap(),
            )
        })
        .collect();
    (manifest, payloads)
}

fn recover_exactly_and_stably(root: &Path) -> FinalizationReceipt {
    let first = finalized(ProductionSession::recover_finalization(root).unwrap());
    let first_bytes = receipt_bytes(&first);
    let state = finalization_bytes(root);
    let second = finalized(ProductionSession::recover_finalization(root).unwrap());
    assert_eq!(receipt_bytes(&second), first_bytes);
    assert_eq!(finalization_bytes(root), state);
    assert_eq!(first.final_workspace(), second.final_workspace());
    second
}

fn assert_clean_bundle(path: &Path) {
    let report = verify_path(path, None);
    assert_eq!(report.package_structure, VerificationStatus::Ok);
    assert_eq!(report.event_chain, VerificationStatus::Ok);
    assert_eq!(report.checkpoint_hashes, VerificationStatus::Ok);
    assert_eq!(report.replay, VerificationStatus::Ok);
    assert_eq!(report.submitted_source_match, SubmittedSourceStatus::Ok);
}

fn temporary_archives(parent: &Path) -> Vec<PathBuf> {
    fs::read_dir(parent)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".rustrace-submit-")
                .then(|| entry.path())
        })
        .collect()
}

fn prepare_case(fixture: &Fixture, kind: CrashKind) {
    match kind {
        CrashKind::Cargo => install_tool_fixture(&fixture.root, "short_sleep"),
        CrashKind::Format => {
            install_tool_fixture(&fixture.root, "format");
            fs::write(fixture.root.join("main.rs"), "// formatter fixture\n").unwrap();
        }
        CrashKind::Archive => {
            let mut session = ProductionSession::start(&fixture.root, MANIFEST).unwrap();
            session.execute(EditorCommand::Insert('B')).unwrap();
            session.finalize("student-1").unwrap();
        }
        CrashKind::Journal | CrashKind::Checkpoint | CrashKind::Finalization => {}
    }
}

fn prepare_random_case(fixture: &Fixture, kind: CrashKind) {
    prepare_case(fixture, kind);
    if kind != CrashKind::Archive {
        ProductionSession::start(&fixture.root, MANIFEST)
            .unwrap()
            .quit()
            .unwrap();
    }
}

fn assert_after_kill(fixture: &Fixture, kind: CrashKind, finalization_stage: &str) {
    match kind {
        CrashKind::Journal | CrashKind::Checkpoint => assert_resumes_and_continues(fixture),
        CrashKind::Cargo | CrashKind::Format => assert_clear_command_recovery(fixture),
        CrashKind::Finalization if finalization_stage == "finalization-before-capture" => {
            assert_eq!(assert_unique_valid_prefix(&fixture.root).1, 0);
            assert_resumes_and_continues(fixture);
        }
        CrashKind::Finalization => {
            let receipt = recover_exactly_and_stably(&fixture.root);
            assert_eq!(assert_unique_valid_prefix(&fixture.root).1, 1);
            assert!(matches!(
                receipt.manifest().package_state,
                rustrace_model::RprovPackageState::CleanFinalized
            ));
        }
        CrashKind::Archive => {
            assert!(!fixture.destination("submission").exists());
            let _ = recover_exactly_and_stably(&fixture.root);
            assert_eq!(assert_unique_valid_prefix(&fixture.root).1, 1);
        }
    }
}

#[test]
fn journal_append_is_killed_before_its_durable_acknowledgment() {
    let fixture = Fixture::new("journal");
    kill_at_boundary(&fixture, CrashKind::Journal, "journal-write");
    assert_resumes_and_continues(&fixture);
}

#[test]
fn checkpoint_creation_is_killed_before_its_durable_acknowledgment() {
    let fixture = Fixture::new("checkpoint");
    kill_at_boundary(&fixture, CrashKind::Checkpoint, "checkpoint-write");
    assert_resumes_and_continues(&fixture);
}

#[test]
fn controlled_cargo_process_is_killed_while_running() {
    let fixture = Fixture::new("cargo");
    prepare_case(&fixture, CrashKind::Cargo);
    kill_at_boundary(&fixture, CrashKind::Cargo, "command-running");
    assert_clear_command_recovery(&fixture);
}

#[test]
fn formatter_prefix_is_killed_after_a_durable_baseline() {
    let fixture = Fixture::new("format");
    prepare_case(&fixture, CrashKind::Format);
    kill_at_boundary(&fixture, CrashKind::Format, "format-baseline");
    assert_clear_command_recovery(&fixture);
}

#[test]
fn finalization_boundaries_recover_only_the_exact_immutable_capture() {
    for stage in [
        "finalization-before-capture",
        "finalization-capture",
        "finalization-terminal",
        "finalization-before-receipt",
    ] {
        let fixture = Fixture::new(stage);
        kill_at_boundary(&fixture, CrashKind::Finalization, stage);
        assert_after_kill(&fixture, CrashKind::Finalization, stage);
        if stage != "finalization-before-capture" {
            let receipt = recover_exactly_and_stably(&fixture.root);
            let destination = fixture.destination("recovered");
            create_bundle(&receipt, &destination).unwrap();
            assert_clean_bundle(&destination);
        }
    }
}

#[test]
fn archive_temporary_is_never_installed_and_retry_is_byte_stable() {
    let fixture = Fixture::new("archive");
    prepare_case(&fixture, CrashKind::Archive);
    kill_at_boundary(&fixture, CrashKind::Archive, "archive-before-install");
    assert_after_kill(&fixture, CrashKind::Archive, "archive-before-install");

    let receipt = recover_exactly_and_stably(&fixture.root);
    let first = fixture.destination("submission");
    let second = fixture.destination("retry");
    create_bundle(&receipt, &first).unwrap();
    create_bundle(&receipt, &second).unwrap();
    assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
    assert_clean_bundle(&first);
    assert!(
        temporary_archives(&fixture.base).is_empty(),
        "successful retry must clean process-interrupted temporary archives"
    );
}

#[test]
fn cleanup_removes_stale_real_export_temporaries_and_preserves_live_exporter() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("cleanup-real-temporaries");
    prepare_case(&fixture, CrashKind::Archive);

    let stale_marker = fixture.marker("stale-export");
    let mut stale_export = spawn_submit_binary(&fixture, "stale", &stale_marker);
    wait_for_marker(&mut stale_export, &stale_marker, "archive-before-install");
    kill_child(stale_export, "stale exporter");
    let stale = temporary_archives(&fixture.base)
        .into_iter()
        .collect::<BTreeSet<_>>();
    assert!(!stale.is_empty(), "killed submit left no real temporaries");

    let live_marker = fixture.marker("live-export");
    let before_live = temporary_archives(&fixture.base)
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut live_export = spawn_submit_binary(&fixture, "live", &live_marker);
    wait_for_marker(&mut live_export, &live_marker, "archive-before-install");
    let active = temporary_archives(&fixture.base)
        .into_iter()
        .filter(|path| !before_live.contains(path))
        .collect::<BTreeSet<_>>();
    assert!(
        !active.is_empty(),
        "live submit created no real temporaries"
    );

    let cleanup = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("cleanup")
        .arg(&fixture.root)
        .arg("--confirm")
        .output()
        .unwrap();
    kill_child(live_export, "live exporter");

    assert!(
        cleanup.status.success(),
        "cleanup failed: stdout={} stderr={}",
        String::from_utf8_lossy(&cleanup.stdout),
        String::from_utf8_lossy(&cleanup.stderr)
    );
    for path in stale {
        assert!(
            !path.exists(),
            "cleanup preserved stale production temporary {}",
            path.display()
        );
    }
    for path in active {
        assert!(
            path.exists(),
            "cleanup removed live exporter temporary {}",
            path.display()
        );
    }
}

#[test]
fn randomized_sigkill_mode_covers_every_injection_category() {
    let iterations = std::env::var("RUSTRACE_CRASH_ITERATIONS")
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(DEFAULT_RANDOM_ITERATIONS);
    assert!(
        (CrashKind::ALL.len()..=120).contains(&iterations),
        "RUSTRACE_CRASH_ITERATIONS must be in 6..=120"
    );
    let mut random = RANDOM_SEED;
    let mut covered = BTreeSet::new();
    let mut pre_marker_kills = 0;
    let finalization_stages = [
        "finalization-before-capture",
        "finalization-capture",
        "finalization-terminal",
        "finalization-before-receipt",
    ];
    for iteration in 0..iterations {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        let kind = CrashKind::ALL[iteration % CrashKind::ALL.len()];
        covered.insert(kind as u8);
        let stage = match kind {
            CrashKind::Journal => "journal-write",
            CrashKind::Checkpoint => "checkpoint-write",
            CrashKind::Cargo => "command-running",
            CrashKind::Format => "format-baseline",
            CrashKind::Finalization => {
                finalization_stages[(random as usize) % finalization_stages.len()]
            }
            CrashKind::Archive => "archive-before-install",
        };
        let fixture = Fixture::new(&format!("random-{iteration}-{}", kind.mode()));
        prepare_random_case(&fixture, kind);
        let offset = if iteration == 0 {
            random % 2
        } else {
            10_000 + random % 1_001
        };
        let outcome = kill_at_elapsed_offset(&fixture, kind, stage, offset);
        pre_marker_kills += usize::from(!outcome.marker_reached);
        assert_after_kill(&fixture, kind, stage);
        println!(
            "random crash iteration {iteration}: kind={} stage={stage} offset_ms={offset} elapsed_ms={}",
            kind.mode(),
            outcome.elapsed.as_millis()
        );
    }
    assert_eq!(covered.len(), CrashKind::ALL.len());
    assert!(
        pre_marker_kills > 0,
        "randomized mode must kill before an instrumented marker at least once"
    );
    println!("random crash seed={RANDOM_SEED:#x} iterations={iterations}");
}

#[test]
fn crash_injection_child() {
    let Some(root) = std::env::var_os("RUSTRACE_CRASH_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let open_session = || {
        if std::env::var_os("RUSTRACE_CRASH_RESUME").is_some() {
            ProductionSession::resume(&root, MANIFEST, ResumeChoice::Resume).unwrap()
        } else {
            ProductionSession::start(&root, MANIFEST).unwrap()
        }
    };
    match std::env::var("RUSTRACE_CRASH_MODE").unwrap().as_str() {
        "journal" => {
            let mut session = open_session();
            session.execute(EditorCommand::Insert('B')).unwrap();
        }
        "checkpoint" => {
            let mut session = open_session();
            session.execute(EditorCommand::Insert('B')).unwrap();
            session.capture_boundary().unwrap();
        }
        "cargo" => {
            let mut session = open_session();
            session.start_command(CargoAction::Check).unwrap();
            while session.command_active() {
                session.tick().unwrap();
                thread::sleep(Duration::from_millis(2));
            }
        }
        "format" => {
            let mut session = open_session();
            session.start_command(CargoAction::Format).unwrap();
            while session.command_active() {
                session.tick().unwrap();
                thread::sleep(Duration::from_millis(2));
            }
        }
        "finalization" => {
            let mut session = open_session();
            session.execute(EditorCommand::Insert('B')).unwrap();
            session.finalize("student-1").unwrap();
        }
        "archive" => {
            let destination =
                PathBuf::from(std::env::var_os("RUSTRACE_CRASH_DESTINATION").unwrap());
            let mut output = Vec::new();
            run_submit(
                &[
                    root.to_string_lossy().into_owned(),
                    "--student-id".to_owned(),
                    "student-1".to_owned(),
                    "--output".to_owned(),
                    destination.to_string_lossy().into_owned(),
                ],
                &mut output,
            )
            .unwrap();
        }
        mode => panic!("unknown crash mode {mode}"),
    }
}
