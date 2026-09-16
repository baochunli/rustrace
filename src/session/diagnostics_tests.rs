use super::*;
use crate::diagnostics::{
    CargoDiagnostic, CargoTarget, CommandDiagnostics, DiagnosticIdentity, DiagnosticNavigation,
    DiagnosticOutcome, DiagnosticSpan,
};
use crate::language_service::{LiveDiagnostic, LiveDiagnosticSeverity, PublishedDiagnostics};
use crate::tui::DiagnosticMarkerKind;
use rustrace_editor::position::Utf16Position;
use rustrace_model::{
    CaptureCompleteness, CommandCapture, CommandCaptureMode, CommandId, ControlledAction,
};
use std::sync::atomic::{AtomicU64, Ordering};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "live-diagnostics"
assignment_version = "v1"
title = "Live diagnostics"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

const MIXED_MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "live-diagnostics-mixed"
assignment_version = "v1"
title = "Rust-only live diagnostics"
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

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> (Self, ProductionSession) {
        Self::with_main("let 名称 = 1;\n")
    }

    fn with_main(main: &str) -> (Self, ProductionSession) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rustrace-live-diagnostics-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("main.rs"), main).unwrap();
        fs::write(root.join("other.rs"), "other\n").unwrap();
        let session = ProductionSession::start(&root, MANIFEST).unwrap();
        (Self(root), session)
    }

    fn mixed() -> (Self, ProductionSession) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rustrace-live-diagnostics-mixed-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\n[workspace]\n").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        let session = ProductionSession::start(&root, MIXED_MANIFEST).unwrap();
        (Self(root), session)
    }
}

#[test]
fn live_diagnostic_ranges_accept_scalar_boundaries_inside_a_displayed_grapheme() {
    let (_fixture, mut session) = Fixture::with_main("e\u{301}x\n");
    let diagnostic = published(
        &session,
        7,
        session.workspace().active_buffer().version(),
        Utf16Position {
            line: 0,
            character: 1,
        },
        Utf16Position {
            line: 0,
            character: 2,
        },
        LiveDiagnosticSeverity::Warning,
        "combining mark",
    );

    assert!(session.apply_live_diagnostic_poll(Some(7), vec![diagnostic]));
    let spans = session.active_live_diagnostic_spans();
    assert_eq!(spans.len(), 1);
    assert_eq!((spans[0].start_byte, spans[0].end_byte), (1, 3));
    assert_eq!((spans[0].start_line, spans[0].end_line), (0, 0));
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn published(
    session: &ProductionSession,
    generation: u64,
    version: u64,
    start: Utf16Position,
    end: Utf16Position,
    severity: LiveDiagnosticSeverity,
    message: &str,
) -> PublishedDiagnostics {
    PublishedDiagnostics {
        generation,
        document_id: session.workspace().active_document_id().clone(),
        path: session.workspace().active_path().clone(),
        version,
        diagnostics: vec![LiveDiagnostic {
            start,
            end,
            severity,
            message: message.to_owned(),
        }],
    }
}

fn one_line_diagnostic(
    session: &ProductionSession,
    severity: LiveDiagnosticSeverity,
    message: &str,
) -> PublishedDiagnostics {
    published(
        session,
        7,
        session.workspace().active_buffer().version(),
        Utf16Position {
            line: 0,
            character: 4,
        },
        Utf16Position {
            line: 0,
            character: 6,
        },
        severity,
        message,
    )
}

#[test]
fn live_diagnostics_map_utf16_ranges_without_recording_events() {
    let (_fixture, mut session) = Fixture::new();
    let before = session.health().unwrap().events;
    let notification = one_line_diagnostic(&session, LiveDiagnosticSeverity::Error, "unknown name");

    assert!(session.apply_live_diagnostic_poll(Some(7), vec![notification]));
    let spans = session.active_live_diagnostic_spans();
    assert_eq!(spans.len(), 1);
    assert_eq!((spans[0].start_byte, spans[0].end_byte), (4, 10));
    assert_eq!((spans[0].start_line, spans[0].end_line), (0, 0));
    assert_eq!(spans[0].message, "unknown name");
    assert!(
        session
            .live_diagnostic_header_for_caret()
            .is_some_and(|header| header.contains("unknown name"))
    );
    let markers = session.active_diagnostic_markers();
    assert_eq!(markers.len(), 1);
    assert!(markers[0].live);
    assert_eq!(session.health().unwrap().events, before);
}

#[test]
fn caret_diagnostic_header_exposes_only_the_message_and_disappears_off_line() {
    let (_fixture, mut session) = Fixture::with_main("let value = 1;\nvalue;\n");
    let diagnostic = one_line_diagnostic(
        &session,
        LiveDiagnosticSeverity::Error,
        "unknown\u{1b}[2J name",
    );

    assert!(session.apply_live_diagnostic_poll(Some(7), vec![diagnostic]));
    assert_eq!(
        session.live_diagnostic_header_for_caret().as_deref(),
        Some("unknown\\u{1b}[2J name")
    );

    session
        .execute(EditorCommand::Move {
            movement: rustrace_editor::Movement::Down,
            selecting: false,
        })
        .unwrap();
    assert_eq!(session.live_diagnostic_header_for_caret(), None);

    session
        .execute(EditorCommand::Move {
            movement: rustrace_editor::Movement::Up,
            selecting: false,
        })
        .unwrap();
    assert_eq!(
        session.live_diagnostic_header_for_caret().as_deref(),
        Some("unknown\\u{1b}[2J name")
    );
}

#[test]
fn non_rust_publish_diagnostics_are_dropped_before_rendering() {
    let (_fixture, mut session) = Fixture::mixed();
    assert_eq!(session.workspace().active_path().as_str(), "Cargo.toml");
    let toml = one_line_diagnostic(
        &session,
        LiveDiagnosticSeverity::Error,
        "TOML must stay quiet",
    );

    assert!(
        !session.apply_live_diagnostic_poll(Some(7), vec![toml]),
        "dropping a non-Rust notification must not request a redraw"
    );
    assert!(session.active_live_diagnostic_spans().is_empty());
    assert!(session.active_diagnostic_markers().is_empty());
    assert_eq!(session.live_diagnostic_header_for_caret(), None);

    session
        .workspace_mut()
        .activate_path(&WorkspacePath::new("main.rs").unwrap())
        .unwrap();
    let rust = one_line_diagnostic(&session, LiveDiagnosticSeverity::Error, "Rust stays live");
    assert!(session.apply_live_diagnostic_poll(Some(7), vec![rust]));
    assert_eq!(session.active_live_diagnostic_spans().len(), 1);
    assert!(
        session
            .live_diagnostic_header_for_caret()
            .is_some_and(|header| header.contains("Rust stays live"))
    );
}

#[test]
fn live_diagnostic_ranges_clamp_to_source_and_stale_state_is_silent() {
    let (_fixture, mut session) = Fixture::new();
    let original_version = session.workspace().active_buffer().version();
    let clamped = published(
        &session,
        7,
        original_version,
        Utf16Position {
            line: 0,
            character: 4,
        },
        Utf16Position {
            line: 0,
            character: 999,
        },
        LiveDiagnosticSeverity::Warning,
        "clamped warning",
    );
    assert!(session.apply_live_diagnostic_poll(Some(7), vec![clamped]));
    assert_eq!(session.active_live_diagnostic_spans()[0].end_byte, 15);

    session.execute(EditorCommand::Insert('x')).unwrap();
    assert!(session.active_live_diagnostic_spans().is_empty());
    assert!(session.active_diagnostic_markers().is_empty());
    assert_eq!(session.live_diagnostic_header_for_caret(), None);

    let stale = published(
        &session,
        7,
        original_version,
        Utf16Position {
            line: 0,
            character: 0,
        },
        Utf16Position {
            line: 0,
            character: 1,
        },
        LiveDiagnosticSeverity::Error,
        "stale",
    );
    assert!(session.apply_live_diagnostic_poll(Some(7), vec![stale]));
    assert!(session.active_live_diagnostic_spans().is_empty());

    assert!(session.apply_live_diagnostic_poll(None, vec![]));
    assert!(!session.apply_live_diagnostic_poll(None, vec![]));
    assert!(session.active_live_diagnostic_spans().is_empty());
}

#[test]
fn workspace_document_replacement_drops_live_diagnostics_immediately() {
    let (_fixture, mut session) = Fixture::new();
    let notification = one_line_diagnostic(
        &session,
        LiveDiagnosticSeverity::Error,
        "must not survive replacement",
    );
    assert!(session.apply_live_diagnostic_poll(Some(7), vec![notification]));

    session.rename_selected("renamed.rs").unwrap();

    assert!(session.live_diagnostics.is_empty());
    assert_eq!(session.live_diagnostic_generation, None);
    assert!(session.active_live_diagnostic_spans().is_empty());
}

fn install_cargo_diagnostic(session: &mut ProductionSession) {
    let workspace = session
        .effects
        .0
        .borrow()
        .replay
        .as_ref()
        .and_then(rustrace_replay::ReplayEngine::command_tree_link)
        .unwrap()
        .clone();
    session.command.diagnostics = Some(CommandDiagnostics {
        version: 1,
        identity: DiagnosticIdentity {
            command_id: CommandId::new("cargo-diagnostic").unwrap(),
            action: ControlledAction::Check,
            argv: vec!["cargo".into(), "check".into()],
            selected_toolchain: "1.98.1".into(),
            tools: vec![],
            workspace,
        },
        stdout: CommandCapture {
            bytes: 0,
            completeness: CaptureCompleteness::Complete,
            mode: CommandCaptureMode::Captured,
        },
        stderr: CommandCapture {
            bytes: 0,
            completeness: CaptureCompleteness::Complete,
            mode: CommandCaptureMode::Captured,
        },
        issues: vec![],
        outcome: DiagnosticOutcome::CompilerErrors,
        diagnostics: vec![CargoDiagnostic {
            package_id: "student".into(),
            manifest_path: "Cargo.toml".into(),
            target: CargoTarget {
                name: "student".into(),
                kind: vec!["bin".into()],
                crate_types: vec!["bin".into()],
                src_path: "other.rs".into(),
                edition: "2024".into(),
            },
            code: None,
            level: "error".into(),
            message: "cargo error".into(),
            rendered: None,
            spans: vec![DiagnosticSpan {
                file_name: "other.rs".into(),
                byte_start: 1,
                byte_end: 3,
                line_start: 1,
                line_end: 1,
                column_start: 2,
                column_end: 4,
                is_primary: true,
                label: None,
                suggested_replacement: None,
                suggestion_applicability: None,
            }],
            children: vec![],
        }],
        artifacts: vec![],
        structured_messages: 1,
        output: vec![],
        omitted_output_lines: 0,
    });
}

#[test]
fn navigation_merges_live_and_cargo_diagnostics_in_document_order() {
    let (_fixture, mut session) = Fixture::new();
    install_cargo_diagnostic(&mut session);
    let live = one_line_diagnostic(&session, LiveDiagnosticSeverity::Warning, "live warning");
    assert!(session.apply_live_diagnostic_poll(Some(7), vec![live]));

    assert_eq!(
        session.select_diagnostic(1).unwrap(),
        DiagnosticNavigation::Navigated {
            path: WorkspacePath::new("main.rs").unwrap(),
            start_byte: 4,
            end_byte: 10,
        }
    );
    assert_eq!(session.workspace().active_path().as_str(), "main.rs");
    assert_eq!(
        session.select_diagnostic(1).unwrap(),
        DiagnosticNavigation::Navigated {
            path: WorkspacePath::new("other.rs").unwrap(),
            start_byte: 1,
            end_byte: 3,
        }
    );
    assert_eq!(session.workspace().active_path().as_str(), "other.rs");
    assert!(
        session
            .diagnostic_display_rows(4)
            .into_iter()
            .any(|row| row.diagnostic_index() == Some(0) && row.selected()),
        "Alt navigation did not mark the selected compiler output row"
    );
    assert_eq!(
        session.select_diagnostic(1).unwrap(),
        DiagnosticNavigation::Navigated {
            path: WorkspacePath::new("main.rs").unwrap(),
            start_byte: 4,
            end_byte: 10,
        },
        "merged navigation must wrap"
    );

    assert!(matches!(
        session.select_diagnostic_index(0).unwrap(),
        DiagnosticNavigation::Navigated { ref path, .. } if path.as_str() == "other.rs"
    ));
}

#[test]
fn compiler_line_model_prefers_error_and_can_select_output_without_moving_the_caret() {
    let (_fixture, mut session) = Fixture::new();
    install_cargo_diagnostic(&mut session);
    let error = session.command.diagnostics.as_ref().unwrap().diagnostics[0].clone();
    let mut warning = error.clone();
    warning.level = "warning".into();
    warning.message = "warning first".into();
    session.command.diagnostics.as_mut().unwrap().diagnostics = vec![warning, error];
    session
        .workspace_mut()
        .activate_path(&WorkspacePath::new("other.rs").unwrap())
        .unwrap();

    let before_selection = session.workspace().active_buffer().selection_state();
    let before_events = session.health().unwrap().events;
    let markers = session.active_diagnostic_markers();
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].kind, DiagnosticMarkerKind::Error);
    assert_eq!(markers[0].diagnostic_index(), Some(1));

    assert!(session.select_diagnostic_for_output(1));
    assert_eq!(
        session.workspace().active_buffer().selection_state(),
        before_selection
    );
    assert_eq!(session.health().unwrap().events, before_events);
    let selected = session
        .diagnostic_display_rows(4)
        .into_iter()
        .find(|row| row.diagnostic_index() == Some(1))
        .unwrap();
    assert!(selected.selected());
}
