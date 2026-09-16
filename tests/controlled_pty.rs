#![cfg(unix)]

#[path = "support/test_home.rs"]
mod test_home;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn recorded_events(mode: &str) -> String {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/command_pty.py"
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
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("RECORDED_EVENT_PARITY="))
        .unwrap_or_else(|| panic!("mode={mode} did not report recorded events"))
        .to_owned()
}

fn retained_recorded_events(mode: &str) -> (String, PathBuf) {
    let test_home = test_home::TestHome::new(false);
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "rustrace-save-check-{}-{}-{mode}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
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
    assert!(
        output.status.success(),
        "mode={mode}; {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let events = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("RECORDED_EVENT_PARITY="))
        .unwrap_or_else(|| panic!("mode={mode} did not report recorded events"))
        .to_owned();
    (events, root)
}

fn submit_verify_and_replay(root: &Path) -> Vec<u8> {
    let test_home = test_home::TestHome::new(false);
    let bundle = root.join("submission.zip");
    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("submit")
        .arg(root.join("assignment.work"))
        .args(["--student-id", "student-1", "--output"])
        .arg(&bundle)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "submit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = rustrace::verify::verify_path(&bundle, None);
    assert!(report.is_clean(), "{report:?}");
    let mut replay = rustrace::replay_tui::ReplayController::open(&bundle).unwrap();
    let last = replay.positions().last().expect("final replay event");
    replay.select(last).unwrap();
    replay
        .selected_event()
        .unwrap()
        .source
        .iter()
        .find(|(path, _)| path == "main.rs")
        .expect("replayed main.rs")
        .1
        .clone()
}

#[test]
fn controlled_commands_use_real_80x24_terminal_without_raw_output() {
    let test_home = test_home::TestHome::new(false);
    for mode in [
        "source",
        "cancel",
        "quit",
        "active_paste",
        "picker_paste",
        "diagnostic",
        "format",
        "format_rejected",
    ] {
        let output = test_home
            .command("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/command_pty.py"
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

#[test]
fn mouse_format_and_diagnostic_actions_emit_the_keyboard_events() {
    assert_eq!(recorded_events("format_mouse"), recorded_events("format"));
    assert_eq!(
        recorded_events("diagnostic_mouse"),
        recorded_events("diagnostic")
    );
}

#[test]
fn diagnostic_tints_click_to_accented_output_without_mouse_provenance() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/diagnostic_tints_pty.py"
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

#[test]
fn save_triggered_check_matches_manual_check_and_autosave_stays_silent() {
    let (save_events, save_root) = retained_recorded_events("save_check");
    let (manual_events, manual_root) = retained_recorded_events("manual_check");
    assert_eq!(save_events, manual_events);
    assert_eq!(submit_verify_and_replay(&save_root), b"XA");
    assert_eq!(submit_verify_and_replay(&manual_root), b"XA");
    std::fs::remove_dir_all(save_root).unwrap();
    std::fs::remove_dir_all(manual_root).unwrap();
}
