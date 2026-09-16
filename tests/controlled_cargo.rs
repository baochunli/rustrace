//! Only this trusted, dependency-free disposable package is compiled/executed.
use rustrace::{
    cargo_policy::CargoAction,
    diagnostics::{DiagnosticEvidenceIssue, DiagnosticOutcome},
    session::ProductionSession,
};
use rustrace_model::{CommandOutcome, ControlledAction, OutputStream};
use std::{
    fs,
    time::{Duration, Instant},
};

#[test]
fn real_production_policy_runs_benign_offline_package() {
    let root =
        std::env::temp_dir().join(format!("rustrace-controlled-cargo-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        b"[package]\nname = \"rustrace-owned-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n",
    )
    .unwrap();
    fs::write(
        root.join("Cargo.lock"),
        b"version = 4\n\n[[package]]\nname = \"rustrace-owned-fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let source = b"fn main() { let unused = 1; println!(\"trusted disposable fixture\"); }\n";
    fs::write(root.join("src/main.rs"), source).unwrap();
    let manifest = br#"format_version = 1
course_id = "course"
assignment_id = "runner-real"
assignment_version = "v1"
title = "Trusted fixture"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["Cargo.toml", "Cargo.lock", "src/*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;
    let mut session = ProductionSession::start(&root, manifest).unwrap();
    // Run first so this test observes the compiler warning and the executed
    // program's stdout in the same fresh Cargo invocation.
    for action in [CargoAction::Run, CargoAction::Check, CargoAction::Test] {
        session.start_command(action).unwrap();
        let until = Instant::now() + Duration::from_secs(60);
        while session.command_active() && Instant::now() < until {
            session
                .poll_command()
                .unwrap_or_else(|error| panic!("{action:?}: {error}; retained {}", root.display()));
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            !session.command_active(),
            "hang guard; retained {}",
            root.display()
        );
        assert_eq!(
            session.command_outcome(),
            Some(&CommandOutcome::Exited { code: 0 }),
            "{action:?}; retained {}",
            root.display()
        );
        let diagnostics = session.command_diagnostics().unwrap_or_else(|| {
            panic!(
                "{action:?}: missing derived diagnostics; retained {}",
                root.display()
            )
        });
        assert!(
            diagnostics.issues.is_empty(),
            "{action:?}: {:?}; retained {}",
            diagnostics.issues,
            root.display()
        );
        assert_eq!(diagnostics.outcome, DiagnosticOutcome::Success);
        assert_eq!(
            diagnostics.identity.action,
            match action {
                CargoAction::Check => ControlledAction::Check,
                CargoAction::Test => ControlledAction::Test,
                CargoAction::Run => ControlledAction::Run,
                _ => unreachable!(),
            }
        );
        assert!(
            diagnostics
                .identity
                .argv
                .windows(2)
                .any(|args| args == ["--message-format=json", "--locked"]),
            "{action:?}: {:?}; retained {}",
            diagnostics.identity.argv,
            root.display()
        );
        assert!(
            diagnostics.structured_messages > 0 && !diagnostics.artifacts.is_empty(),
            "{action:?}: retained {}",
            root.display()
        );
        if matches!(action, CargoAction::Run | CargoAction::Check) {
            assert!(
                diagnostics.diagnostics.iter().any(|diagnostic| {
                    diagnostic.level == "warning"
                        && diagnostic.message.contains("unused variable")
                        && diagnostic.primary_span().is_some()
                }),
                "real warning missing; retained {}",
                root.display()
            );
        }
        if action == CargoAction::Run {
            assert!(diagnostics.output.iter().any(|line| {
                line.stream == OutputStream::Stdout
                    && line
                        .original_bytes()
                        .is_ok_and(|bytes| bytes == b"trusted disposable fixture")
            }));
            let rows = session.diagnostic_display_rows(3);
            assert_eq!(
                rows.iter()
                    .map(|row| row.diagnostic_index())
                    .collect::<Vec<_>>(),
                vec![None, Some(0), None],
                "diagnostic and captured-output rows lost their identities: {rows:?}"
            );
            assert!(
                rows.iter()
                    .any(|row| row.text().contains("unused variable")),
                "real warning missing from composed rows: {rows:?}; retained {}",
                root.display()
            );
            assert!(
                rows.iter()
                    .any(|row| row.text().contains("trusted disposable fixture")),
                "Run stdout missing from composed rows: {rows:?}; retained {}",
                root.display()
            );
        }
        assert_eq!(fs::read(root.join("src/main.rs")).unwrap(), source);
    }
    session.quit().unwrap();
    let views = ProductionSession::inspect(&root).unwrap();
    assert_eq!(views.saved, views.logical);
    assert_eq!(views.disk, views.logical);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn real_production_policy_distinguishes_compiler_errors_from_incomplete_evidence() {
    let root = std::env::temp_dir().join(format!(
        "rustrace-controlled-cargo-error-{}",
        std::process::id()
    ));
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        b"[package]\nname = \"rustrace-error-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n",
    )
    .unwrap();
    fs::write(
        root.join("Cargo.lock"),
        b"version = 4\n\n[[package]]\nname = \"rustrace-error-fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let source = b"fn main() { let value: u8 = \"not a number\"; println!(\"{value}\"); }\n";
    fs::write(root.join("src/main.rs"), source).unwrap();
    let manifest = br#"format_version = 1
course_id = "course"
assignment_id = "runner-real-error"
assignment_version = "v1"
title = "Trusted error fixture"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["Cargo.toml", "Cargo.lock", "src/*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;
    let mut session = ProductionSession::start(&root, manifest).unwrap();
    session.start_command(CargoAction::Check).unwrap();
    let until = Instant::now() + Duration::from_secs(60);
    while session.command_active() && Instant::now() < until {
        session
            .poll_command()
            .unwrap_or_else(|error| panic!("{error}; retained {}", root.display()));
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        !session.command_active(),
        "hang guard; retained {}",
        root.display()
    );
    assert_eq!(
        session.command_outcome(),
        Some(&CommandOutcome::Exited { code: 101 })
    );
    let diagnostics = session.command_diagnostics().expect("derived diagnostics");
    assert_eq!(diagnostics.outcome, DiagnosticOutcome::CompilerErrors);
    assert_eq!(diagnostics.issues, Vec::<DiagnosticEvidenceIssue>::new());
    let error = diagnostics
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.level == "error" && diagnostic.code.as_deref() == Some("E0308")
        })
        .unwrap_or_else(|| panic!("real E0308 missing; retained {}", root.display()));
    let span = error
        .primary_span()
        .unwrap_or_else(|| panic!("real E0308 span missing; retained {}", root.display()));
    assert!(span.file_name.ends_with("src/main.rs"));
    assert!(span.byte_start < span.byte_end && span.line_start == 1);
    assert!(!diagnostics.known_empty());
    assert_eq!(fs::read(root.join("src/main.rs")).unwrap(), source);
    session.quit().unwrap();
    fs::remove_dir_all(root).unwrap();
}
