#![cfg(unix)]

#[path = "support/test_home.rs"]
mod test_home;
fn run_fixture(mode: &str) {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/console_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .arg(mode)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if std::env::var_os("RUSTRACE_CONSOLE_PTY_RETAIN").is_some() {
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }
}

#[test]
fn embedded_console_focus_transitions_use_real_80x24_terminal_and_reap_before_restoration() {
    run_fixture("baseline");
}

#[test]
fn rolling_output_keeps_active_prompt_and_input_visible_at_80x24() {
    run_fixture("rolling-output");
}

#[test]
fn maximum_test_case_picker_keeps_late_selection_and_run_all_visible_at_80x24() {
    run_fixture("maximum-test-cases");
}

#[test]
fn workspace_confirmations_precede_console_input_and_restore_console_focus() {
    run_fixture("workspace-confirmations");
}

#[test]
fn exact_ctrl_w_opens_delete_confirmation_from_console_focus() {
    run_fixture("delete-confirmations");
}

#[test]
fn menu_and_console_command_lifecycle_states_do_not_render_notice_toasts() {
    run_fixture("lifecycle-notices");
}

#[test]
fn natural_doc_check_and_run_console_output_preserves_program_indentation() {
    run_fixture("natural-output");
}
