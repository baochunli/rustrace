//! A long attempt past the 1,024-checkpoint limit of v0.1.7 submits and
//! verifies, and a v0.1.7 workspace stuck after its failed submit recovers.
//!
//! Checkpoints come from `ProductionSession::capture_boundary`, the path the
//! recorder uses, so the journal stays replay-consistent.
#[path = "support/test_home.rs"]
mod test_home;

use rustrace::{
    replay_tui::ReplayController,
    session::{ProductionSession, ResumeChoice},
    tui::EditorCommand,
    verify::verify_path,
};
use rustrace_journal::Journal;
use rustrace_model::SessionId;
use rustrace_workspace::rprov_import::import_rprov;
use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    process::Output,
    sync::atomic::{AtomicU64, Ordering},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "ece1724"
assignment_id = "lab1"
assignment_version = "2026-09"
title = "Long session"
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

/// More than the 1,024 checkpoints one v0.1.7 submission could hold.
const RECORDED_BOUNDARIES: usize = 1_030;

struct Fixture {
    base: PathBuf,
    workspace: PathBuf,
    submissions: PathBuf,
}

impl Fixture {
    fn new(prefix: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "rustrace-{prefix}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = base.join("lab1.work");
        let submissions = base.join("submissions");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::create_dir(&submissions).unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname = \"reversi\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(
            workspace.join("src/main.rs"),
            "fn main() {\n    println!(\"reversi\");\n}\n",
        )
        .unwrap();
        Self {
            base: fs::canonicalize(&base).unwrap(),
            workspace: fs::canonicalize(&workspace).unwrap(),
            submissions: fs::canonicalize(&submissions).unwrap(),
        }
    }

    /// Records `boundaries` boundary checkpoints, editing before every
    /// sixteenth as a command boundary pair often has no edit between them,
    /// then closes.
    fn record_long_attempt(&self, boundaries: usize) -> SessionId {
        let mut session = ProductionSession::start(&self.workspace, MANIFEST).unwrap();
        let id = session.session_id().clone();
        for index in 0..boundaries {
            if index % 16 == 0 {
                let character = char::from(b'a' + (index / 16 % 26) as u8);
                session.execute(EditorCommand::Insert(character)).unwrap();
            }
            session.capture_boundary().unwrap();
        }
        session.quit().unwrap();
        id
    }

    fn checkpoint_count(&self, id: &SessionId) -> u64 {
        let mut journal = Journal::open_read_only_no_follow(
            self.workspace.join(format!(".rustrace/{id}.sqlite")),
        )
        .unwrap();
        journal.checkpoint_totals(id).unwrap().count
    }

    fn run(&self, home: &test_home::TestHome, args: &[&std::ffi::OsStr]) -> Output {
        home.command(env!("CARGO_BIN_EXE_rustrace"))
            .args(args)
            .output()
            .unwrap()
    }

    fn status(&self, home: &test_home::TestHome) -> String {
        let output = self.run(home, &["status".as_ref(), self.workspace.as_os_str()]);
        assert!(output.status.success(), "{}", text(&output));
        text(&output)
    }

    fn submit(&self, home: &test_home::TestHome, destination: &Path) -> Output {
        self.run(
            home,
            &[
                "submit".as_ref(),
                self.workspace.as_os_str(),
                "--student-id".as_ref(),
                "student-1".as_ref(),
                "--output".as_ref(),
                destination.as_os_str(),
            ],
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_clean_verification(home: &test_home::TestHome, fixture: &Fixture, zip: &Path) {
    let report = verify_path(zip, None);
    assert!(report.is_clean(), "{report:#?}");
    let verified = fixture.run(home, &["verify".as_ref(), zip.as_os_str()]);
    assert!(verified.status.success(), "{}", text(&verified));
}

#[test]
fn long_attempt_past_the_old_checkpoint_limit_submits_verifies_scans_and_replays() {
    let home = test_home::TestHome::new(false);
    let fixture = Fixture::new("long-session");
    let id = fixture.record_long_attempt(RECORDED_BOUNDARIES);
    let checkpoints = fixture.checkpoint_count(&id);
    assert!(checkpoints > 1_024, "{checkpoints}");

    let status = fixture.status(&home);
    assert!(
        status.contains(&format!(
            "Recorded checkpoints: {} of the 8,192 one submission can hold",
            grouped(checkpoints)
        )) && status.contains("Recorded launches: 0;")
            && !status.contains("Warning:"),
        "{status}"
    );

    let zip = fixture.submissions.join("student-1-lab1.zip");
    let submitted = fixture.submit(&home, &zip);
    assert!(submitted.status.success(), "{}", text(&submitted));
    assert_clean_verification(&home, &fixture, &zip);
    let imported = import_rprov(Cursor::new(fs::read(&zip).unwrap())).unwrap();
    let segment = &imported.manifest().segments[0];
    // The submission's own boundary checkpoint is the last one exported.
    assert_eq!(segment.checkpoints.len() as u64, checkpoints + 1);
    assert_eq!(segment.session_id, id);

    let review = fixture.base.join("review.csv");
    let scanned = fixture.run(
        &home,
        &[
            "scan".as_ref(),
            fixture.submissions.as_os_str(),
            "--output".as_ref(),
            review.as_os_str(),
        ],
    );
    assert!(scanned.status.success(), "{}", text(&scanned));
    assert!(
        fs::read_to_string(&review)
            .unwrap()
            .contains("student-1-lab1.zip")
    );

    let replay = ReplayController::open(&zip).unwrap();
    assert!(replay.positions().count() > RECORDED_BOUNDARIES);
    assert!(
        fixture
            .status(&home)
            .contains("FINALIZED IMMUTABLE SNAPSHOT")
    );
}

/// A Lab 1 student on v0.1.7 recorded more than 1,024 checkpoints; `submit`
/// resumed, captured its boundary checkpoint, stopped at the checkpoint limit
/// before any capture and published this marker, and every later submit
/// repeated the failure. The journal itself is an ordinary unfinished one.
#[test]
fn v0_1_7_workspace_stuck_after_its_over_limit_submit_now_submits() {
    let home = test_home::TestHome::new(false);
    let fixture = Fixture::new("long-session-v017");
    let id = fixture.record_long_attempt(RECORDED_BOUNDARIES);
    // What the failed v0.1.7 submit recorded before it stopped.
    let mut failed_submit =
        ProductionSession::resume(&fixture.workspace, MANIFEST, ResumeChoice::Resume).unwrap();
    failed_submit.capture_boundary().unwrap();
    failed_submit.quit().unwrap();
    // The exact v0.1.7 marker bytes: serde_json of PersistedIncompleteFinalization.
    fs::write(
        fixture
            .workspace
            .join(".rustrace/finalization-incomplete.json"),
        format!(
            r#"{{"version":1,"label":"INCOMPLETE RECOVERY","reason":"checkpoint count is outside finalization limits","session_id":"{id}","capture_available":false}}"#
        ),
    )
    .unwrap();
    let checkpoints = fixture.checkpoint_count(&id);
    assert!(checkpoints > 1_024, "{checkpoints}");
    let state = fs::read_dir(fixture.workspace.join(".rustrace"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("finalization-"))
        .collect::<Vec<_>>();
    assert_eq!(state, ["finalization-incomplete.json"]);

    let status = fixture.status(&home);
    assert!(
        status.contains(&format!("UNFINISHED session {id}"))
            && status.contains(
                "Unfinished (last submit failed: checkpoint count is outside finalization limits)"
            )
            && !status.contains("INCOMPLETE RECOVERY"),
        "{status}"
    );

    let zip = fixture.submissions.join("student-1-lab1.zip");
    let submitted = fixture.submit(&home, &zip);
    assert!(submitted.status.success(), "{}", text(&submitted));
    assert!(
        !text(&submitted).contains("INCOMPLETE RECOVERY"),
        "{}",
        text(&submitted)
    );
    assert_clean_verification(&home, &fixture, &zip);
    let imported = import_rprov(Cursor::new(fs::read(&zip).unwrap())).unwrap();
    assert_eq!(imported.manifest().latest_session_id, id);
    assert_eq!(
        imported.manifest().segments[0].checkpoints.len() as u64,
        checkpoints + 1
    );
    assert!(
        fixture
            .status(&home)
            .contains("FINALIZED IMMUTABLE SNAPSHOT")
    );
}

fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut result = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(',');
        }
        result.push(digit);
    }
    result
}
