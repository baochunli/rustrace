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
