//! Guardrails for the student-facing documentation.
#[path = "support/test_home.rs"]
mod test_home;
use rustrace::review_flags::{
    LARGE_SINGLE_INSERTION_BYTES, SUSTAINED_HIGH_RATE_CHARACTERS_PER_SECOND,
    SUSTAINED_HIGH_RATE_WINDOW_MILLIS, UNIFORM_KEY_TIMING_COEFFICIENT_OF_VARIATION,
    UNIFORM_KEY_TIMING_TRANSACTIONS,
};
use std::{fs, path::PathBuf};

const FORBIDDEN: [&str; 7] = [
    "misconduct",
    "cheat",
    "plagiar",
    "authorship",
    "dishonest",
    "detect",
    "accura",
];

fn docs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs")
}

fn read_doc(name: &str) -> String {
    let path = docs_dir().join(name);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn visit_files(directory: &std::path::Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            visit_files(&path, files);
        } else {
            files.push(path);
        }
    }
}

/// Markdown wraps prose at 80 columns; compare sentences with single spaces.
fn unwrapped(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn usage_line() -> String {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let line = stdout
        .lines()
        .find(|line| line.starts_with("Usage: "))
        .unwrap_or_else(|| panic!("no usage line in {stdout:?}"));
    line.trim_start_matches("Usage: ").to_owned()
}

/// Every `rustrace <command>` form named in `USAGE`, including flag-style
/// commands such as `--version` once they exist.
fn usage_commands() -> Vec<String> {
    usage_line()
        .split('|')
        .filter_map(|form| {
            let mut words = form.split_whitespace();
            (words.next() == Some("rustrace")).then(|| words.next().unwrap().to_owned())
        })
        .collect()
}

#[test]
fn guides_pin_assignment_package_isolation_in_both_formats() {
    let student = unwrapped(&read_doc("student-guide.md"));
    assert!(student.contains(
        "Assignments work from any directory because each is its own Cargo workspace root."
    ));
    let environment = unwrapped(&read_doc("supported-environment.md"));
    for wording in [
        "For v1 and v2, `starter/Cargo.toml` must declare a `[package]` table and an empty `[workspace]` table",
        "Comments are allowed in the empty table",
        "Every key under `[workspace]` is rejected",
        "`members`, `exclude`, `default-members`, `package`, and `dependencies`",
        "`package.workspace` is also rejected",
        "A missing, non-UTF-8 or unparsable `starter/Cargo.toml` is also rejected as a starter package structure error before publication.",
        "assignment starter must be a self-contained package: add an empty [workspace] table to starter/Cargo.toml",
    ] {
        assert!(
            environment.contains(wording),
            "supported environment must say: {wording}"
        );
    }
}

#[test]
fn guides_avoid_forbidden_vocabulary() {
    for name in ["student-guide.md"] {
        let lower = read_doc(name).to_lowercase();
        for word in FORBIDDEN {
            assert!(!lower.contains(word), "{name} contains {word:?}");
        }
    }
}

#[test]
fn guides_mention_every_usage_command() {
    let commands = usage_commands();
    assert!(commands.len() >= 10, "unexpected usage forms: {commands:?}");
    let student = read_doc("student-guide.md");
    for command in commands {
        if ["replay", "scan", "verify"].contains(&command.as_str()) {
            continue;
        }
        let form = format!("rustrace {command}");
        assert!(student.contains(&form), "student guide must mention {form}");
    }
}

#[test]
fn student_guide_explains_packaged_test_case_placement_and_collisions() {
    let guide = unwrapped(&read_doc("student-guide.md"));
    for wording in [
        "Version 2 assignment packages include a nonempty `test-cases/` directory",
        "`test-cases/NAME.in` and `test-cases/NAME.expected`",
        "`NAME` is 1 to 64 ASCII bytes",
        "at most 256 cases",
        "1 MiB per case file",
        "10 MiB across all case files",
        "The `.rta` file itself must be no larger than 32 MiB",
        "next to the workspace as `WORKSPACE_PARENT/test-cases/`",
        "checks every packaged case path before publishing a fresh workspace",
        "never replaces a different file, symlink, directory, or other special entry",
        "Resume recreates missing packaged files, accepts byte-identical files, and preserves unrelated files",
        "`--inspect` and `doctor` never write the sibling directory",
        "Version 1 packages remain accepted unchanged",
    ] {
        assert!(guide.contains(wording), "student guide must say: {wording}");
    }
}

#[test]
fn student_guide_pins_test_case_picker_execution_and_complete_capture_comparison() {
    let guide = unwrapped(&read_doc("student-guide.md"));
    for wording in [
        "F4 or the Test cases menu entry opens a modal test-case picker",
        "paired cases in bytewise name order",
        "Run all runs every listed case serially in that order",
        "Esc cancels the active run and the rest of the queue",
        "F1, F7, and F9 wait until that sequence finishes",
        "closing its final reopened modal restores that view",
        "A later non-test command takes ownership of the output pane",
        "Console commands print Cargo's normal output flush-left and program output verbatim",
        "Cargo status lines render flush-left; diagnostic blocks are dedented by their minimum indentation to keep source gutters and carets aligned",
        "Esc cancels a running console command and closes the console",
        "Esc cancels any running console command, closes the console, restores the diagnostics pane, and returns focus to the editor",
        "There is no separate EOF shortcut",
        "missing packaged data for a version 2 assignment is reported as unavailable",
        "closes before a run starts and reopens after the run or queue finishes",
        "PASS only when the complete captured stdout bytes exactly equal the `.expected` bytes",
        "first differing LF-delimited line",
        "expected and actual byte lengths without the LF",
        "CR bytes and invalid UTF-8 are content",
        "bounded terminal-safe previews",
        "launch, exit, termination, capture, or expected-file problem is ERROR",
        "Input and bounded expected bytes are opened before launch",
        "Every completed picker run records one `test_case_compared` event immediately after its controlled command finishes",
        "command ID, case name, expected BLAKE3 digest, optional actual BLAKE3 digest, and typed result",
        "Mismatch details retain the positive one-based line and expected and actual line-byte lengths",
        "The recorded line is at most 1,048,577; expected line lengths are at most 1 MiB and actual line lengths at most 8 MiB",
        "Pre-launch input or expected-file errors show ERROR without creating a completed-run comparison record",
        "No raw test input, expected output, or actual output bytes are added by the comparison event",
    ] {
        assert!(guide.contains(wording), "student guide must say: {wording}");
    }

    let environment = unwrapped(&read_doc("supported-environment.md"));
    assert!(environment.contains(
        "Test-case comparison uses only complete captured stdout, never the 256 KiB live tail"
    ));
    assert!(environment.contains(
        "natural-output console commands, including packaged test-case runs, are complete when both captures are complete and the process exited"
    ));
    assert!(environment.contains(
        "Natural console output never becomes missing or unexpected structured-diagnostic evidence; F7 commands retain structured-diagnostic classification"
    ));
    assert!(environment.contains(
        "The additive version 1 `test_case_compared` event follows its controlled command finish in the same durable step"
    ));
    assert!(environment.contains(
        "Case names remain limited to 1 through 64 ASCII bytes using letters, digits, `-`, or `_`"
    ));
    assert!(environment.contains("Mismatch lines are positive, one-based, and at most 1,048,577"));
    assert!(environment.contains(
        "Expected mismatch line lengths are at most 1,048,576 bytes, while actual lengths are at most the 8 MiB captured-output limit"
    ));
}

#[test]
fn every_live_git_install_command_pins_the_toolchain_tag_and_package() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for directory in ["docs", "scripts", ".github/workflows"] {
        visit_files(&root.join(directory), &mut files);
    }
    files.push(root.join("README.md"));

    let mut commands = 0;
    for path in files {
        let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
            continue;
        };
        if !["md", "sh", "yml", "yaml"].contains(&extension) {
            continue;
        }
        let contents = fs::read_to_string(&path).unwrap();
        for line in contents
            .lines()
            .filter(|line| line.contains("install --git"))
        {
            commands += 1;
            let validated_installer_argv = path == root.join("scripts/install.sh")
                && line
                    == "set -- +1.98.1 install --git \"$repository\" --tag \"$tag\" rustrace --locked";
            assert!(
                validated_installer_argv || line.contains("cargo +1.98.1 install --git https://github.com/baochunli/rustrace --tag vX.Y.Z rustrace --locked"),
                "{} must pin the release tag and build toolchain: {line}",
                path.display()
            );
            assert!(
                line.contains(" rustrace --locked"),
                "{} has an unqualified Git install command: {line}",
                path.display()
            );
        }
    }
    assert!(
        commands >= 5,
        "expected the root README and live installation guides"
    );
}

#[test]
fn student_guide_reproduces_the_privacy_notice_verbatim() {
    let privacy = read_doc("privacy.md");
    let notice = privacy
        .split("## Student privacy notice\n")
        .nth(1)
        .expect("privacy.md has a student privacy notice")
        .split("\n## ")
        .next()
        .unwrap()
        .trim();
    assert!(notice.contains("The data is used for grading only."));
    assert!(
        read_doc("student-guide.md").contains(notice),
        "student guide must reproduce the notice verbatim, including line breaks"
    );
}

#[test]
fn student_guide_lists_modern_navigation_and_terminal_capture_boundaries() {
    let guide = read_doc("student-guide.md");
    for chord in [
        "⌘Left",
        "⌘Right",
        "⌘Up",
        "⌘Down",
        "Option-Left",
        "Option-Right",
        "Option-Backspace",
        "⌘Backspace",
        "Ctrl-Left",
        "Ctrl-Right",
        "Ctrl-Home",
        "Ctrl-End",
        "Ctrl-Backspace",
    ] {
        assert!(guide.contains(chord), "student guide must mention {chord}");
    }
    assert!(guide.contains("Shift"));
    assert!(guide.contains("outside Rustrace's control"));
}

#[test]
fn student_guide_explains_updates_and_terminal_owned_shortcuts() {
    let guide = unwrapped(&read_doc("student-guide.md"));
    for wording in [
        "Updating Rustrace",
        "cargo install --path . --locked --force",
        "cargo +1.98.1 install --git https://github.com/baochunli/rustrace --tag vX.Y.Z rustrace --locked --force",
        "an older installed binary silently lacks newer shortcuts",
        "⌘A is consumed by Ghostty's own Select All",
        "applications built on Ghostty",
        "keybind = super+a=unbind",
        "Ctrl-A selects all unless the startup probe observes Ghostty's exact `super+arrow_left` → `text:\\x01` rewrite",
        "Right-click in the editor and choose `Select all`",
        "macOS owns ⌘Q as the application Quit command",
        "Ghostty binds ⌘W, ⌘A, ⌘C and ⌘V by default",
        "Rustrace still accepts the Command forms when a terminal delivers them",
    ] {
        assert!(guide.contains(wording), "student guide must say: {wording}");
    }
    for chord in ["Ctrl-Q", "Ctrl-W", "Ctrl-A", "Ctrl-C", "Ctrl-X", "Ctrl-V"] {
        assert!(
            guide.contains(chord),
            "student guide must advertise {chord}"
        );
    }
    let raw_guide = read_doc("student-guide.md");
    let key_table = raw_guide
        .split("| Key | In the editor |")
        .nth(1)
        .expect("student guide key table")
        .split("\n\nThe on-screen keybind panel")
        .next()
        .unwrap();
    assert!(
        !key_table.contains("ctrl-"),
        "student guide key table must spell every Control chord with `Ctrl-`: {key_table}"
    );
    for terminal_owned in ["⌘Space", "⌘Tab", "⌘BackTab"] {
        assert!(
            !guide.contains(terminal_owned),
            "student guide must not advertise {terminal_owned}"
        );
    }
    assert!(!guide.contains("Ctrl-A always selects all"));
}

#[test]
fn guides_document_the_ghostty_doctor_setup_and_truthful_hint_rule() {
    let student = unwrapped(&read_doc("student-guide.md"));
    for wording in [
        "rustrace doctor assignment.rta",
        "rustrace doctor --write-ghostty-keys",
        "config-file = rustrace-keys",
        "Ghostty's own `super+shift+,=reload_config` binding",
        "never edits Ghostty's main config",
        "⌘A, ⌘C, ⌘V, ⌘Q, and ⌘W stay assigned to Ghostty",
    ] {
        assert!(
            student.contains(wording),
            "student guide must say: {wording}"
        );
    }

    let environment = unwrapped(&read_doc("supported-environment.md"));
    for wording in [
        "ghostty +list-keybinds",
        "owns, rewrites, or passes each setup chord",
        "informational and does not change doctor's exit code",
        "missing Ghostty CLI is treated as the default binding set",
        "Command-mode hints use Control forms unless Ghostty passes the setup chord",
        "Observed exact control-byte translations keep the matching Command form",
        "The Option translations keep their forms because their escape-prefixed inputs remain enhancement-gated",
        "Any other rewrite uses the Control fallback",
        "reports the rewrite target",
    ] {
        assert!(
            environment.contains(wording),
            "supported environment must say: {wording}"
        );
    }
}

#[test]
fn guides_explain_enhancement_gated_injected_text_translations() {
    let student = unwrapped(&read_doc("student-guide.md"));
    let ghostty_defaults = [
        "super+arrow_left=text:\\\\x01",
        "super+arrow_right=text:\\\\x05",
        "super+backspace=text:\\\\x15",
        "alt+arrow_left=esc:b",
        "alt+arrow_right=esc:f",
        "super+arrow_up=jump_to_prompt:-1",
        "super+arrow_down=jump_to_prompt:1",
        "super+a=select_all",
        "super+q=quit",
        "super+w=close_surface",
        "super+f=start_search",
        "super+z=undo",
        "super+k=clear_screen",
        "super+c=copy_to_clipboard",
        "super+v=paste_from_clipboard",
        "super+home=scroll_to_top",
        "super+end=scroll_to_bottom",
    ];
    for wording in [
        "Injected macOS editing text",
        "Only an exact startup-probe rewrite activates each control-byte translation",
        "`super+arrow_left` → `text:\\x01`: Ctrl-A → line start",
        "`super+arrow_right` → `text:\\x05`: Ctrl-E → line end",
        "`super+backspace` → `text:\\x15`: Ctrl-U → delete to line start",
        "`super+k` → `text:\\x0b`: Ctrl-K → delete to line end",
        "A passed binding, an unavailable probe, a non-Ghostty terminal, or an inactive keyboard enhancement keeps the legacy meanings",
        "Esc b → previous word",
        "Esc f → next word",
        "Esc Delete → delete previous word",
        "Ghostty's default ⌘Backspace binding sends `text:\\x15`",
        "Ghostty 1.3.1 defaults send `text:\\x01` for ⌘Left, `text:\\x05` for ⌘Right, `text:\\x15` for ⌘Backspace, `esc:b` for Option-Left, and `esc:f` for Option-Right",
        "Option-Backspace has no Ghostty default binding",
        "arrives through the kitty protocol as Alt-Backspace",
        "⌘Up and ⌘Down have no injected equivalent",
        "`jump_to_prompt:-1` and `jump_to_prompt:1`",
        "Ctrl-Home and Ctrl-End are the always-available forms of document start and end",
        "`⌘A`, `⌘C`, `⌘V`, `⌘Q`, and `⌘W` are left to the terminal on purpose because Ghostty bindings are global",
        "Compiler errors tint the full source line red and warnings tint it yellow",
        "Click a tinted line to keep the caret at that source position and reveal its accent-marked diagnostic row",
    ] {
        assert!(
            student.contains(wording),
            "student guide must say: {wording}"
        );
    }
    assert!(
        !student.to_ascii_lowercase().contains("gutter marker"),
        "student guide must not describe removed diagnostic gutter letters"
    );
    for binding in ghostty_defaults {
        assert!(
            student.contains(binding),
            "student guide must list Ghostty default {binding}"
        );
    }
    for unbind in [
        "keybind = super+arrow_left=unbind",
        "keybind = super+arrow_right=unbind",
        "keybind = super+backspace=unbind",
        "keybind = alt+arrow_left=unbind",
        "keybind = alt+arrow_right=unbind",
        "keybind = super+arrow_up=unbind",
        "keybind = super+arrow_down=unbind",
        "keybind = super+f=unbind",
        "keybind = super+z=unbind",
        "keybind = super+k=unbind",
        "keybind = super+home=unbind",
        "keybind = super+end=unbind",
    ] {
        assert!(student.contains(unbind), "student guide must list {unbind}");
    }
    for capture_fragment in [
        "python3 -c",
        "/dev/tty",
        "tty.setraw",
        "time.sleep(15)",
        "\\x1b[>5u",
    ] {
        assert!(
            student.contains(capture_fragment),
            "capture command must contain {capture_fragment}"
        );
    }

    let environment = unwrapped(&read_doc("supported-environment.md"));
    for wording in [
        "only when that keyboard enhancement frame is active and the startup probe observed the corresponding exact Ghostty rewrite",
        "legacy control and escape-prefixed bytes as terminal-injected editing text",
        "A passed binding, an unavailable probe, a non-Ghostty terminal, or an inactive enhancement keeps Ctrl-A as select all and the other control bytes at their legacy meanings",
        "Ghostty 1.3.1 default bindings",
        "Option-Backspace is not bound by default",
        "Ctrl-Home and Ctrl-End remain the always-available document-start and document-end forms",
        "`⌘A`, `⌘C`, `⌘V`, `⌘Q`, and `⌘W` are left to the terminal on purpose because Ghostty bindings are global",
    ] {
        assert!(
            environment.contains(wording),
            "supported environment must say: {wording}"
        );
    }
    for binding in ghostty_defaults {
        assert!(
            environment.contains(binding),
            "supported environment must list Ghostty default {binding}"
        );
    }
    for unbind in [
        "keybind = super+arrow_left=unbind",
        "keybind = super+arrow_right=unbind",
        "keybind = super+backspace=unbind",
        "keybind = alt+arrow_left=unbind",
        "keybind = alt+arrow_right=unbind",
        "keybind = super+arrow_up=unbind",
        "keybind = super+arrow_down=unbind",
        "keybind = super+f=unbind",
        "keybind = super+z=unbind",
        "keybind = super+k=unbind",
        "keybind = super+home=unbind",
        "keybind = super+end=unbind",
    ] {
        assert!(
            environment.contains(unbind),
            "supported environment must list {unbind}"
        );
    }
    for capture_fragment in [
        "python3 -c",
        "/dev/tty",
        "tty.setraw",
        "time.sleep(15)",
        "\\x1b[>5u",
    ] {
        assert!(
            environment.contains(capture_fragment),
            "supported environment capture command must contain {capture_fragment}"
        );
    }
}

#[test]
fn theme_configuration_documents_builtins_auto_switch_and_every_override() {
    let student = unwrapped(&read_doc("student-guide.md"));
    let configuration = unwrapped(&read_doc("configuration.md"));
    for wording in [
        "[theme]",
        "name = \"catppuccin\"",
        "auto_switch = false",
        "light_name = \"catppuccin-latte\"",
        "dark_name = \"catppuccin\"",
        "[theme.custom]",
        "error_bg = \"#f38ba8\"",
        "warning_bg = \"#f9e2af\"",
        "tab_active_bg = \"#89b4fa\"",
        "tab_active_fg = \"#1e1e2e\"",
        "built-in, then [theme.custom]",
        "OSC 11",
        "no reply keeps name",
        "rustrace doctor",
    ] {
        assert!(
            student.contains(wording),
            "student guide must contain {wording:?}"
        );
        assert!(
            configuration.contains(wording),
            "configuration guide must contain {wording:?}"
        );
    }
    for field in rustrace::tui::theme::PALETTE_FIELD_NAMES {
        assert!(
            configuration.contains(field),
            "configuration guide must list {field}"
        );
    }
}

#[test]
fn dependency_execution_and_local_side_effects_are_explicit_everywhere() {
    let required = "Downloaded dependencies and their build scripts execute on the student's \
machine under the retained Cargo configuration and may have local side effects.";
    for name in ["privacy.md", "student-guide.md"] {
        let document = unwrapped(&read_doc(name));
        assert!(
            document.contains(required),
            "{name} must state the dependency execution consequence exactly"
        );
    }
}

#[test]
fn student_guide_explains_editing_conveniences_and_their_limits() {
    let guide = unwrapped(&read_doc("student-guide.md"));
    for wording in [
        "Enter keeps the current line's leading spaces or tabs",
        "Closing `}`, `)`, or `]` on an indentation-only line moves back one level",
        "Ctrl-/ toggles `// ` on the current or selected lines as one undo step",
        "Tab and Enter still accept an open completion popup; Esc closes it first",
        "Bracket matching does not distinguish brackets inside strings or comments",
        "Inside ordinary Rust strings, including escaped quotes and multiline LF or CRLF text, `\"` stays single",
        "inside `r\"...\"` and `r#\"...\"#` raw strings",
    ] {
        assert!(guide.contains(wording), "student guide must say: {wording}");
    }
    for pair in ["`()`", "`[]`", "`{}`", "`\"\"`"] {
        assert!(guide.contains(pair), "student guide must mention {pair}");
    }
}

#[test]
fn student_guide_explains_find_and_replace_behavior_and_limits() {
    let guide = unwrapped(&read_doc("student-guide.md"));
    for wording in [
        "find and replace panel",
        "literal and case-sensitive",
        "replace all",
        "one undo step",
        "no matches",
        "F3 repeats the last find while the panel is closed",
        "External paste is blocked in both find and replace fields",
    ] {
        assert!(guide.contains(wording), "student guide must say: {wording}");
    }
}

#[test]
fn live_diagnostic_and_save_check_guidance_is_explicit() {
    let student = unwrapped(&read_doc("student-guide.md"));
    for wording in [
        "Live hints from rust-analyzer are display only and are never recorded",
        "Ctrl-S saves every open buffer and then starts Check",
        "One successful explicit save shows exactly one `File saved` message",
        "A successful command started from the menu or console also adds no completion message",
        "Command preparation, tool resolution, execution, evidence saving, and owned-process cleanup do not raise notices",
        "Command outcomes do not raise notices",
        "The ERROR pill and output or console pane keep showing failed command results",
        "Failure-note diagnostics and spanless Cargo note/help boilerplate stay out of the output pane",
        "Spanned diagnostics remain visible, and the recorded command evidence is unchanged",
        "The command menu contains Check, Run, Clippy, Format, Doc, Update dependencies, Update Rustrace, Automatic checks: On/Off, Console, Test cases, Keybinds, and Quit",
        "Manage dependencies by typing `cargo add` or `cargo remove` in the console",
        "The keybinds overlay spells the modifier `Control`; mode-bar hints and toasts use `Ctrl`",
        "The save-triggered Check is recorded exactly like a manual Check",
        "Autosave never starts Check",
    ] {
        assert!(
            student.contains(wording),
            "student guide must say: {wording}"
        );
    }
}

#[test]
fn rust_only_diagnostics_and_clipboard_context_menu_guidance_is_explicit() {
    let student = unwrapped(&read_doc("student-guide.md"));
    for wording in [
        "Use Ctrl-C, Ctrl-X and Ctrl-V for Rustrace's internal clipboard",
        "⌘C, ⌘X and ⌘V work only when the terminal delivers them",
        "macOS terminals capture ⌘C and ⌘V for the system clipboard",
        "Right-click the source editor to open Cut, Copy and Paste",
        "Cut and Copy are disabled without a selection; Paste is disabled when Rustrace's internal clipboard is empty",
        "Only Rust (`.rs`) documents receive live hints",
    ] {
        assert!(
            student.contains(wording),
            "student guide must say: {wording}"
        );
    }
}

#[test]
fn files_panel_context_menu_is_the_documented_file_management_route() {
    let student = unwrapped(&read_doc("student-guide.md"));
    for wording in [
        "Right-click a file row to open, rename, delete, or create a file",
        "Right-click the ` files` header or empty space to open a menu with only `new file…` enabled",
        "The sidebar ` new` label opens the same new-file panel",
        "The files panel is mouse-only and has no keyboard focus mode",
    ] {
        assert!(
            student.contains(wording),
            "student guide must say: {wording}"
        );
    }
    for removed in [
        "Press F2 to move focus to the file tree",
        "In the file tree (after F2)",
        "Esc or F2 returns to the editor",
    ] {
        assert!(
            !student.contains(removed),
            "student guide retained removed file-tree keyboard guidance: {removed}"
        );
    }
}

#[test]
fn typing_shape_advisory_thresholds_are_stable() {
    assert_eq!(LARGE_SINGLE_INSERTION_BYTES, 200);
    assert_eq!(UNIFORM_KEY_TIMING_TRANSACTIONS, 60);
    assert_eq!(UNIFORM_KEY_TIMING_COEFFICIENT_OF_VARIATION, 0.15);
    assert_eq!(SUSTAINED_HIGH_RATE_CHARACTERS_PER_SECOND, 15);
    assert_eq!(SUSTAINED_HIGH_RATE_WINDOW_MILLIS, 60_000);
}

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Docs guide submit sequence"
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

fn submit(workspace: &std::path::Path, extra: &[&str]) -> (i32, String) {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("submit")
        .arg(workspace)
        .args(["--student-id", "student-1"])
        .args(extra)
        .output()
        .unwrap();
    (
        output.status.code().unwrap(),
        String::from_utf8(output.stdout).unwrap(),
    )
}

/// The student guide's "Run rustrace submit" section: a rerun never
/// overwrites the default ZIP, and `--output` rebuilds an identical bundle.
#[test]
fn student_guide_submit_rerun_sequence_matches_the_binary() {
    let base = std::env::temp_dir().join(format!("rustrace-docs-submit-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let workspace = base.join("assignment.work");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("main.rs"), "fn main() {}\n").unwrap();
    let workspace = fs::canonicalize(&workspace).unwrap();
    rustrace::session::ProductionSession::start(&workspace, MANIFEST)
        .unwrap()
        .finalize("student-1")
        .unwrap();

    let (code, first) = submit(&workspace, &[]);
    assert_eq!(code, 0, "{first}");
    let default_zip = base.join("student-1-assignment.zip");
    assert!(default_zip.is_file());
    assert!(first.contains("local artifact only; this does not mean a successful LMS hand-in"));
    let hash = first.split_whitespace().next().unwrap().to_owned();

    let (code, rerun) = submit(&workspace, &[]);
    assert_eq!(code, 1, "{rerun}");
    assert!(
        rerun.contains("bundle destination already exists and was not replaced"),
        "{rerun}"
    );

    let again = base.join("again.zip");
    let (code, rebuilt) = submit(&workspace, &["--output", again.to_str().unwrap()]);
    assert_eq!(code, 0, "{rebuilt}");
    assert_eq!(rebuilt.split_whitespace().next().unwrap(), hash);
    assert!(again.is_file());

    let guide = read_doc("student-guide.md");
    assert!(guide.contains("bundle destination already exists and was not replaced"));
    fs::remove_dir_all(base).unwrap();
}

/// The student guide's "Revise" and "Prepare the revised ZIP" sections, run
/// against the binary in one folder as the guide instructs: the parent is
/// submitted with the default name, `revise` starts the child, and packaging
/// the child needs `--output` because the default name is already taken.
#[test]
fn student_guide_revision_packaging_sequence_matches_the_binary() {
    let test_home = test_home::TestHome::new(false);
    let base = std::env::temp_dir().join(format!("rustrace-docs-revise-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let parent = base.join("assignment.work");
    fs::create_dir_all(&parent).unwrap();
    fs::write(parent.join("main.rs"), "A").unwrap();
    let base = fs::canonicalize(&base).unwrap();
    let parent = base.join("assignment.work");
    let package = base.join("assignment.rta");
    fs::write(&package, assignment_package()).unwrap();
    rustrace::session::ProductionSession::start(&parent, MANIFEST)
        .unwrap()
        .finalize("student-1")
        .unwrap();
    let (code, submitted) = submit(&parent, &[]);
    assert_eq!(code, 0, "{submitted}");
    let default_zip = base.join("student-1-assignment.zip");
    assert!(default_zip.is_file());

    let child = base.join("assignment-v2.work");
    let revised = test_home
        .command(env!("CARGO_BIN_EXE_rustrace"))
        .arg("revise")
        .args([&parent, &child, &package])
        .output()
        .unwrap();
    assert!(
        revised.status.success(),
        "{}",
        String::from_utf8_lossy(&revised.stdout)
    );

    // The guide's documented command must carry --output: without it the
    // child resolves to the parent's default ZIP and submit refuses.
    let (code, refused) = submit(&child, &[]);
    assert_eq!(code, 1, "{refused}");
    assert!(
        refused.contains("bundle destination already exists and was not replaced"),
        "{refused}"
    );
    let guide = read_doc("student-guide.md");
    let documented = guide
        .split("## Prepare the revised ZIP")
        .nth(1)
        .expect("student guide has a revised ZIP section")
        .lines()
        .find(|line| line.starts_with("rustrace submit assignment-v2.work"))
        .expect("revised ZIP section documents a submit command");
    let words: Vec<&str> = documented.split_whitespace().collect();
    assert_eq!(
        &words[..5],
        [
            "rustrace",
            "submit",
            "assignment-v2.work",
            "--student-id",
            "YOUR_ID"
        ],
        "{documented}"
    );
    assert_eq!(words.get(5), Some(&"--output"), "{documented}");
    let revised_zip = base.join(words[6].replace("YOUR_ID", "student-1"));
    let (code, packaged) = submit(&child, &["--output", revised_zip.to_str().unwrap()]);
    assert_eq!(code, 0, "{packaged}");
    assert!(revised_zip.is_file());
    assert!(packaged.contains("local artifact only; this does not mean a successful LMS hand-in"));
    fs::remove_dir_all(base).unwrap();
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

#[test]
fn clipboard_mirror_matching_paste_and_terminal_guidance_are_explicit() {
    let student = unwrapped(&read_doc("student-guide.md"));
    for wording in [
        "## Recommended terminals",
        "Install Ghostty, kitty, WezTerm, or iTerm2 before the course",
        "Applications in terminal may access clipboard",
        "Terminal.app ignores the mirror",
        "ctrl-V is the fallback there",
        "Text copied elsewhere does not match the live internal clipboard and stays blocked",
        "never reads your system clipboard",
    ] {
        assert!(
            student.contains(wording),
            "student guide must say: {wording}"
        );
    }

    let privacy = unwrapped(&read_doc("privacy.md"));
    assert!(privacy.contains("one-way system clipboard write"));
    assert!(privacy.contains("never reads the system clipboard"));

    let environment = unwrapped(&read_doc("supported-environment.md"));
    for terminal in [
        "Ghostty",
        "kitty",
        "WezTerm",
        "Recent iTerm2",
        "Terminal.app",
    ] {
        assert!(
            environment.contains(terminal),
            "supported environment must cover {terminal}"
        );
    }
    assert!(environment.contains("sends no OSC 52 query"));
}

#[test]
fn update_menu_and_panel_student_guidance_is_pinned() {
    let guide = unwrapped(&read_doc("student-guide.md"));
    for required in [
        "| Update dependencies |",
        "| Update Rustrace |",
        "| Automatic checks: On/Off |",
        "Installed:",
        "Latest known:",
        "Last checked:",
        "Esc or Enter closes",
        "NEW",
        "● menu",
        "reads only the cache and installation receipt",
        "never makes a network request or starts an update during a session",
        "The preference is persisted immediately",
        "takes effect at the next launch",
        "There is no `config.toml` update knob",
        "automatic checks on/off",
        "An unknown cache shows `Latest release is unknown. Quit, then run: rustrace update --check` for every copy.",
        "A latest release that is not newer shows `Rustrace is up to date.` for every copy.",
        "A newer release shows `Rustrace vA.B.C is available. Quit, then run: rustrace update` for installer-managed copies and the exact Cargo install command below with the real tag for other copies.",
    ] {
        assert!(
            guide.contains(required),
            "missing update menu guidance: {required}"
        );
    }
}
