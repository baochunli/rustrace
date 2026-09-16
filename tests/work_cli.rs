#[path = "support/test_home.rs"]
mod test_home;
use rustrace::session::ProductionSession;
use rustrace_journal::Journal;
use rustrace_model::{Event, SessionId, SessionResumed};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "automatic-resume"
assignment_version = "v1"
title = "Automatic resume"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["*.rs", "Cargo.toml"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

#[cfg(unix)]
#[test]
fn self_contained_package_checks_beneath_a_foreign_cargo_workspace() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/self_contained_work_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .env_remove("TMPDIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn pty_screen_decoder_regressions() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/pty_screen_tests.py"
        ))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn package_v2_deploys_and_repairs_cases_without_clobbering_siblings() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/package_v2_work_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn rejected_paste_does_not_hide_prompt_continuation_or_reentry() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/prompt_continuation_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn g3_m1_production_search_next_survives_unicode_changes() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/search_revision_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn production_clipboard_respects_workspace_recovery_after_failed_create() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/clipboard_recovery_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn production_clipboard_policy_precedes_every_modal_and_focus_route() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/clipboard_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn real_work_cli_pty_emits_only_trusted_framing_for_hostile_source() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/safe_display_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn real_work_cli_pty_rejects_external_without_a_conflict_choice() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/work_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .arg("external")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn damaged_session_id_cli_can_preserve_and_link_without_orphan_extraction() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/startup_recovery.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .arg("damaged-id")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn incomplete_startup_cli_has_usable_preserve_choices() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/startup_recovery.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn real_work_cli_pty_starts_resumes_and_restores_terminal() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/work_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn valid_unfinished_session_automatically_resumes_on_a_pty_without_prompt() {
    let test_home = test_home::TestHome::new(false);
    let fixture = WorkFixture::started("automatic-resume-pty");
    let (last_sequence, _) = fixture.chain();
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/automatic_resume_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .arg(&fixture.package)
        .arg(&fixture.workspace)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fixture.only_new_resume(last_sequence),
        SessionResumed { last_sequence }
    );
}

#[test]
fn valid_unfinished_session_automatically_resumes_with_piped_stdin_like_resume_flag() {
    let implicit = WorkFixture::started("automatic-resume-pipe-implicit");
    let explicit = WorkFixture::started("automatic-resume-pipe-explicit");
    let (implicit_last, _) = implicit.chain();
    let (explicit_last, _) = explicit.chain();
    assert_eq!(implicit_last, explicit_last);

    let implicit_output = implicit.run_piped(None);
    let explicit_output = explicit.run_piped(Some("--resume"));
    assert_eq!(implicit_output.status.code(), explicit_output.status.code());
    assert_no_recovery_prompt(&implicit_output);
    assert_eq!(
        implicit.only_new_resume(implicit_last),
        explicit.only_new_resume(explicit_last)
    );
}

#[test]
fn automatic_resume_keeps_explicit_recovery_flags() {
    for flag in ["--resume", "--restore-logical"] {
        let fixture = WorkFixture::started(&format!(
            "automatic-resume-{}",
            flag.trim_start_matches('-')
        ));
        let (last_sequence, _) = fixture.chain();
        let output = fixture.run_piped(Some(flag));
        assert_no_recovery_prompt(&output);
        assert_eq!(
            fixture.only_new_resume(last_sequence),
            SessionResumed { last_sequence },
            "{flag} did not retain resume behavior"
        );
    }

    let inspected = WorkFixture::started("automatic-resume-inspect");
    let chain = inspected.chain();
    let output = inspected.run_piped(Some("--inspect"));
    assert!(output.status.success(), "{}", output_text(&output));
    assert!(
        output_text(&output).contains("Validated saved / logical / observed disk views"),
        "{}",
        output_text(&output)
    );
    assert_eq!(inspected.chain(), chain, "--inspect mutated the journal");

    let abandoned = WorkFixture::started("automatic-resume-abandon");
    let original_chain = abandoned.chain();
    let output = abandoned.run_piped(Some("--abandon"));
    assert!(
        output_text(&output).contains("Original preserved at"),
        "{}",
        output_text(&output)
    );
    assert!(
        abandoned
            .workspace
            .join(".rustrace/abandoned.json")
            .is_file(),
        "--abandon did not mark the original"
    );
    assert_eq!(abandoned.chain(), original_chain);
    assert_eq!(abandoned.recovery_workspaces().len(), 1);
}

#[test]
fn automatic_resume_keeps_incomplete_finalized_and_mismatch_errors() {
    let incomplete = WorkFixture::new("automatic-resume-incomplete", MANIFEST);
    let output = incomplete.run_piped(None);
    let text = output_text(&output);
    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains("Incomplete startup or invalid session metadata")
            && text.contains("Original preserved; session identity is unknown")
            && text.contains("incomplete startup cannot Resume/Restore; original preserved"),
        "{text}"
    );

    let finalized = WorkFixture::new("automatic-resume-finalized", MANIFEST);
    finalized.start().finalize("student-1").unwrap();
    let output = finalized.run_piped(None);
    assert_eq!(
        output_text(&output),
        "work stopped: session is finalized and immutable; use `rustrace revise` for a new linked attempt\n"
    );

    let mismatch = WorkFixture::started("automatic-resume-mismatch");
    let mismatched_manifest = String::from_utf8(MANIFEST.to_vec())
        .unwrap()
        .replace("\"v1\"", "\"v2\"");
    fs::write(
        &mismatch.package,
        assignment_package(mismatched_manifest.as_bytes()),
    )
    .unwrap();
    let output = mismatch.run_piped(None);
    assert_eq!(
        output_text(&output),
        "work stopped: assignment manifest/starter/test-case-suite identity mismatch; selected session left unchanged\n"
    );
}

#[cfg(unix)]
#[test]
fn real_work_cli_pty_drives_every_editing_convenience_at_80x24() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/editing_conveniences_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn real_work_cli_pty_drives_command_and_control_navigation_chords() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/navigation_chords_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn revised_attempts_open_edit_export_verify_and_link_again() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/revision_work_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn real_work_cli_pty_preserves_file_operation_targets() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/operation_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn real_work_cli_mouse_drives_editor_sidebar_tabs_and_scroll_without_churn() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/mouse_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn real_work_cli_files_menu_deletes_after_confirmation_without_mouse_provenance() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/files_menu_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn quit_menu_enter_and_left_down_share_clean_and_dirty_production_paths() {
    let test_home = test_home::TestHome::new(false);
    for mode in [
        "enter_clean",
        "mouse_clean",
        "enter_dirty_cancel_ctrl_confirm",
        "mouse_dirty_confirm",
    ] {
        let output = test_home
            .command("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/quit_menu_pty.py"
            ))
            .arg(env!("CARGO_BIN_EXE_rustrace"))
            .arg(mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "mode={mode}; {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

struct WorkFixture {
    home: test_home::TestHome,
    base: PathBuf,
    workspace: PathBuf,
    package: PathBuf,
    manifest: &'static [u8],
}

impl WorkFixture {
    fn new(prefix: &str, manifest: &'static [u8]) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "rustrace-{prefix}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = base.join("workspace");
        let package = base.join("assignment.rta");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("main.rs"), b"A").unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            b"[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n",
        )
        .unwrap();
        fs::write(&package, assignment_package(manifest)).unwrap();
        Self {
            home: test_home::TestHome::new(false),
            base: fs::canonicalize(base).unwrap(),
            workspace: fs::canonicalize(workspace).unwrap(),
            package,
            manifest,
        }
    }

    fn started(prefix: &str) -> Self {
        let fixture = Self::new(prefix, MANIFEST);
        fixture.start().quit().unwrap();
        fixture
    }

    fn start(&self) -> ProductionSession {
        let mut session = ProductionSession::start(&self.workspace, self.manifest).unwrap();
        session
            .execute(rustrace::tui::EditorCommand::NextBuffer)
            .unwrap();
        session
    }

    fn run_piped(&self, flag: Option<&str>) -> Output {
        let mut command = self.home.command(env!("CARGO_BIN_EXE_rustrace"));
        command
            .arg("work")
            .arg(&self.package)
            .arg("--workspace")
            .arg(&self.workspace)
            .stdin(Stdio::piped());
        if let Some(flag) = flag {
            command.arg(flag);
        }
        command.output().unwrap()
    }

    fn chain(&self) -> (u64, rustrace_model::Hash) {
        let metadata = ProductionSession::read_metadata(&self.workspace).unwrap();
        let mut journal =
            Journal::open_read_only_no_follow(self.journal_path(&metadata.session_id)).unwrap();
        let chain = journal.verify_session_chain(&metadata.session_id).unwrap();
        (chain.event_count, chain.final_hash)
    }

    fn only_new_resume(&self, previous_last_sequence: u64) -> SessionResumed {
        let metadata = ProductionSession::read_metadata(&self.workspace).unwrap();
        let mut journal =
            Journal::open_read_only_no_follow(self.journal_path(&metadata.session_id)).unwrap();
        let chain = journal.verify_session_chain(&metadata.session_id).unwrap();
        assert_eq!(chain.event_count, previous_last_sequence + 1);
        let event = journal
            .read_events(&metadata.session_id, previous_last_sequence + 1, 1)
            .unwrap()
            .remove(0);
        match event.event {
            Event::SessionResumed(resumed) => resumed,
            other => panic!("expected SessionResumed, got {other:?}"),
        }
    }

    fn journal_path(&self, session_id: &SessionId) -> PathBuf {
        self.workspace
            .join(".rustrace")
            .join(format!("{session_id}.sqlite"))
    }

    fn recovery_workspaces(&self) -> Vec<PathBuf> {
        fs::read_dir(&self.base)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("workspace.recovery-")
            })
            .collect()
    }
}

impl Drop for WorkFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn assert_no_recovery_prompt(output: &Output) {
    let text = output_text(output);
    assert!(!text.contains("Unfinished session"), "{text}");
    assert!(!text.contains("Resume [r]"), "{text}");
    assert!(!text.contains("unfinished session: use"), "{text}");
    assert!(!text.contains("no recovery choice"), "{text}");
}

fn output_text(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn assignment_package(manifest: &[u8]) -> Vec<u8> {
    let mut archive = Vec::new();
    append_tar_entry(&mut archive, "assignment.toml", manifest, b'0');
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

#[cfg(unix)]
#[test]
fn representative_work_fixture_is_hermetic_without_ambient_xdg_state() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = WorkFixture::new("hermetic-work", MANIFEST);
    let tools = fixture.base.join("recording-tools");
    fs::create_dir(&tools).unwrap();
    let marker = fixture.base.join("unexpected-curl");
    let curl = tools.join("curl");
    fs::write(
        &curl,
        format!(
            "#!/bin/sh\nprintf invoked >> '{}'\nexit 7\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&curl, fs::Permissions::from_mode(0o700)).unwrap();
    let fallback_home = fixture.base.join("fallback-home");
    fs::create_dir(&fallback_home).unwrap();
    let original_home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
    let path = std::env::join_paths(
        std::iter::once(tools).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "valid_unfinished_session_automatically_resumes_with_piped_stdin_like_resume_flag",
            "--test-threads=1",
        ])
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", &fallback_home)
        .env(
            "RUSTUP_HOME",
            std::env::var_os("RUSTUP_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| original_home.join(".rustup")),
        )
        .env(
            "CARGO_HOME",
            std::env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| original_home.join(".cargo")),
        )
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", output_text(&output));
    assert!(
        output_text(&output).contains("1 passed"),
        "representative child test did not run"
    );
    assert!(
        !marker.exists(),
        "work fixture invoked curl with ambient XDG_STATE_HOME unset"
    );
    assert!(
        !fallback_home.join(".local/state").exists(),
        "fixture wrote HOME fallback update state"
    );
}

#[test]
fn pty_exit_wait_drains_output_and_keeps_its_deadline() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/pty_process_tests.py"
        ))
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", output_text(&output));
}
