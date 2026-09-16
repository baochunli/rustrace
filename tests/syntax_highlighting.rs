#[path = "support/test_home.rs"]
mod test_home;
use std::cell::RefCell;
use std::rc::Rc;

use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier},
    widgets::Widget,
};
use rustrace::editor::{
    EditorEffects, EditorWidget, HighlightKind, HighlightSpan, LiveDiagnosticSpan,
    NoopEditorEffects,
};
use rustrace::tui::theme::Palette;
use rustrace::tui::{DiagnosticMarkerKind, EditorCommand, EditorSession};
use rustrace_model::{DocumentId, EditOrigin, EditorTransaction, SelectionState, TextEdit};

fn id(name: &str) -> DocumentId {
    DocumentId::new(name).unwrap()
}

fn assert_fresh<E: EditorEffects>(session: &EditorSession<E>) {
    let fresh = EditorSession::new(
        id("fresh"),
        session.active_path().into(),
        &session.active_buffer().text(),
        NoopEditorEffects,
    );
    assert_eq!(&*session.active_highlights(), &*fresh.active_highlights());
}

#[test]
fn committed_unicode_multiline_multi_edit_undo_redo_and_reload_match_fresh() {
    let source = "// café\nfn main() {\n    let s = \"你好\";\n}\n";
    let mut session = EditorSession::new(id("rust"), "main.rs".into(), source, NoopEditorEffects);
    assert!(!session.active_highlights().is_empty());
    let start = source.find("你好").unwrap();
    session
        .active_buffer_mut()
        .apply_edits(
            EditOrigin::Keyboard,
            vec![
                TextEdit {
                    start_byte: 0,
                    end_byte: 0,
                    inserted_text: "// 🦀\n".into(),
                },
                TextEdit {
                    start_byte: start as u64,
                    end_byte: (start + "你好".len()) as u64,
                    inserted_text: "e\u{301}\n世界".into(),
                },
            ],
            SelectionState::caret(0),
        )
        .unwrap();
    assert_fresh(&session);
    session.execute(EditorCommand::Undo).unwrap();
    assert_eq!(session.active_buffer().text(), source);
    assert_fresh(&session);
    session.execute(EditorCommand::Redo).unwrap();
    assert_fresh(&session);
    let transactions = session
        .preview_document_replacement(&id("rust"), "fn reloaded() {}\n", 1_048_576)
        .unwrap();
    for transaction in transactions {
        session
            .apply_document_transaction(&id("rust"), transaction)
            .unwrap();
    }
    assert_fresh(&session);
    session.execute(EditorCommand::Undo).unwrap();
    assert_fresh(&session);
}

#[test]
fn independent_buffers_rename_language_switch_close_and_reopen() {
    let mut session = EditorSession::new(id("a"), "a.rs".into(), "fn a() {}", NoopEditorEffects);
    let rust = session.active_highlights().to_vec();
    session.open_buffer(
        id("b"),
        "Cargo.toml".into(),
        "[package]\nname = \"世界\"\nversion = 12\n",
        NoopEditorEffects,
    );
    assert!(
        session
            .active_highlights()
            .iter()
            .any(|s| s.kind == HighlightKind::String)
    );
    assert!(
        session
            .active_highlights()
            .iter()
            .any(|s| s.kind == HighlightKind::Constant)
    );
    assert_fresh(&session);
    session.rename_document(&id("b"), "notes.txt".into());
    assert!(session.active_highlights().is_empty());
    session.rename_document(&id("b"), "again.toml".into());
    assert_fresh(&session);
    session.rename_document(&id("b"), "wrong.rs".into());
    assert_fresh(&session);
    session.remove_document(&id("b"));
    assert_eq!(&*session.active_highlights(), &rust);
    session.open_buffer(
        id("b"),
        "fresh.rs".into(),
        "// fresh\nfn b() {}",
        NoopEditorEffects,
    );
    assert_fresh(&session);
    assert_ne!(&*session.active_highlights(), &rust);
}

#[test]
fn invalid_intermediate_syntax_keeps_valid_tokens_and_recovers() {
    let mut session = EditorSession::new(id("a"), "a.rs".into(), "fn main() {}", NoopEditorEffects);
    session.execute(EditorCommand::Insert('"')).unwrap();
    assert!(
        session
            .active_highlights()
            .iter()
            .any(|span| span.kind == HighlightKind::Keyword)
    );
    let mut output = Buffer::empty(Rect::new(0, 0, 40, 4));
    EditorWidget::new(
        session.active_buffer(),
        session.active_viewport(),
        &session.active_highlights(),
    )
    .render(output.area, &mut output);
    assert_eq!(output[(1, 0)].fg, Color::Magenta);
    session.execute(EditorCommand::Undo).unwrap();
    assert!(!session.active_highlights().is_empty());
    assert_fresh(&session);
}

#[test]
fn invalid_tail_keeps_untouched_cell_styles_in_an_80x24_test_backend() {
    let source = "fn main() {\n    let message = \"kept\";\n}";
    let mut session = EditorSession::new(id("golden"), "main.rs".into(), source, NoopEditorEffects);
    session
        .active_buffer_mut()
        .set_selection(SelectionState::caret(source.len() as u64))
        .unwrap();
    for character in ['\n', 'i', 'f'] {
        session.execute(EditorCommand::Insert(character)).unwrap();
    }

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| {
            frame.render_widget(
                EditorWidget::new(
                    session.active_buffer(),
                    session.active_viewport(),
                    &session.active_highlights(),
                )
                .with_palette(&Palette::terminal()),
                frame.area(),
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(0, 0)].symbol(), "f");
    assert_eq!(buffer[(0, 0)].fg, Palette::terminal().mauve);
    let string_start = source.lines().nth(1).unwrap().find("kept").unwrap() as u16;
    assert_eq!(buffer[(string_start, 1)].symbol(), "k");
    assert_eq!(buffer[(string_start, 1)].fg, Palette::terminal().green);
}

#[test]
fn bracket_and_diagnostic_overlays_compose_with_token_styles() {
    let source = "fn main() {}";
    let mut session =
        EditorSession::new(id("overlays"), "main.rs".into(), source, NoopEditorEffects);
    let opening = source.find('{').unwrap();
    let closing = source.find('}').unwrap();
    session
        .active_buffer_mut()
        .set_selection(SelectionState::caret(opening as u64))
        .unwrap();
    let highlights = [
        HighlightSpan {
            byte_range: 0..2,
            kind: HighlightKind::Keyword,
        },
        HighlightSpan {
            byte_range: closing..closing + 1,
            kind: HighlightKind::Other,
        },
    ];
    let diagnostics = [LiveDiagnosticSpan {
        start_byte: 0,
        end_byte: 2,
        start_line: 0,
        end_line: 0,
        kind: DiagnosticMarkerKind::Error,
        message: "invalid item".into(),
    }];
    let mut output = Buffer::empty(Rect::new(0, 0, 80, 24));
    EditorWidget::new(
        session.active_buffer(),
        session.active_viewport(),
        &highlights,
    )
    .with_live_diagnostics(&diagnostics)
    .with_palette(&Palette::terminal())
    .render(output.area, &mut output);

    assert_eq!(output[(0, 0)].fg, Palette::terminal().red);
    assert!(output[(0, 0)].modifier.contains(Modifier::BOLD));
    assert!(output[(0, 0)].modifier.contains(Modifier::UNDERLINED));
    assert_eq!(output[(closing as u16, 0)].fg, Palette::terminal().accent);
    assert!(
        output[(closing as u16, 0)]
            .modifier
            .contains(Modifier::BOLD)
    );
}

#[cfg(unix)]
#[test]
fn real_80x24_pty_keeps_colours_after_typing_invalid_syntax() {
    let test_home = test_home::TestHome::new(false);
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/highlighting_pty.py"
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
fn rust_and_toml_cached_spans_color_the_custom_widget() {
    for (path, text, x, color) in [
        ("a.rs", "fn main() {}", 0, Color::Magenta),
        ("Cargo.toml", "name = \"demo\"", 8, Color::Green),
    ] {
        let session = EditorSession::new(id("a"), path.into(), text, NoopEditorEffects);
        let mut output = Buffer::empty(Rect::new(0, 0, 40, 4));
        EditorWidget::new(
            session.active_buffer(),
            session.active_viewport(),
            &session.active_highlights(),
        )
        .render(output.area, &mut output);
        assert_eq!(output[(x, 0)].fg, color);
    }
}

#[derive(Clone)]
struct Effects {
    calls: Rc<RefCell<Vec<(&'static str, EditorTransaction)>>>,
    reject: bool,
}
impl EditorEffects for Effects {
    fn record_provenance(
        &mut self,
        t: &EditorTransaction,
    ) -> Result<(), rustrace_editor::EditorEffectError> {
        if self.reject {
            return Err(rustrace_editor::EditorEffectError::new(
                "journal unavailable",
            ));
        }
        self.calls.borrow_mut().push(("provenance", t.clone()));
        Ok(())
    }
    fn update_tree_sitter(&mut self, t: &EditorTransaction) {
        self.calls.borrow_mut().push(("syntax", t.clone()));
    }
    fn send_lsp_did_change(&mut self, t: &EditorTransaction) {
        self.calls.borrow_mut().push(("lsp", t.clone()));
    }
    fn record_replay(&mut self, t: &EditorTransaction) {
        self.calls.borrow_mut().push(("replay", t.clone()));
    }
}

#[test]
fn derived_syntax_preserves_exact_once_effects_and_provenance_failure_boundary() {
    for reject in [false, true] {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut session = EditorSession::new(
            id("a"),
            "a.rs".into(),
            "fn a() {}",
            Effects {
                calls: calls.clone(),
                reject,
            },
        );
        let before = session.active_highlights().to_vec();
        let result = session.execute(EditorCommand::Insert('"'));
        if reject {
            assert!(result.is_err());
            assert_eq!(&*session.active_highlights(), &before);
            assert_eq!(session.active_buffer().version(), 0);
            assert!(calls.borrow().is_empty());
        } else {
            result.unwrap();
            assert!(
                session
                    .active_highlights()
                    .iter()
                    .any(|span| span.kind == HighlightKind::Keyword)
            );
            let calls = calls.borrow();
            assert_eq!(
                calls.iter().map(|c| c.0).collect::<Vec<_>>(),
                ["provenance", "syntax", "lsp", "replay"]
            );
            assert!(calls.iter().all(|c| c.1 == calls[0].1));
            assert_eq!(calls[0].1.version_after, session.active_buffer().version());
        }
    }
}

/// Opt-in wall-clock probe: release mode, deterministic bounded fixtures,
/// includes transaction preflight/commit, syntax update and 120x40 widget render.
#[test]
#[ignore = "run in release mode on a documented host for G3 latency evidence"]
fn bounded_syntax_responsiveness_probe() {
    use std::time::Instant;
    let rust_line = "fn item() { let text = \"café 世界\"; }\n";
    let toml_line = "key = \"café 世界\"\n";
    let fixtures = [
        ("rust-5000-lines", "a.rs", rust_line.repeat(5000)),
        ("toml-5000-lines", "Cargo.toml", toml_line.repeat(5000)),
        ("rust-1MiB", "a.rs", bounded_source(rust_line, 1_048_575)),
        (
            "toml-1MiB",
            "Cargo.toml",
            bounded_source(toml_line, 1_048_575),
        ),
    ];
    for (name, path, source) in fixtures {
        let opened = Instant::now();
        let mut session = EditorSession::new(id("probe"), path.into(), &source, NoopEditorEffects);
        let open_ms = opened.elapsed().as_secs_f64() * 1000.;
        assert!(!session.active_highlights().is_empty());
        let mut samples = Vec::new();
        let mut draws = Vec::new();
        let mut undos = Vec::new();
        let mut fallback_samples = 0;
        for round in 0..210 {
            // Alternate edits/undo at beginning, middle and end. Ignore ten
            // warmup iterations. UTF-8 offsets always start a source line.
            let line = match round % 3 {
                0 => 0,
                1 => 2500,
                _ => source.lines().count() - 1,
            };
            let byte = session.active_buffer().line_start_byte(line);
            session
                .active_buffer_mut()
                .set_selection(SelectionState::caret(byte as u64))
                .unwrap();
            let start = Instant::now();
            session.execute(EditorCommand::Insert(' ')).unwrap();
            session.follow_cursor(120, 40);
            let mut output = Buffer::empty(Rect::new(0, 0, 120, 40));
            EditorWidget::new(
                session.active_buffer(),
                session.active_viewport(),
                &session.active_highlights(),
            )
            .render(output.area, &mut output);
            if round >= 10 {
                if session.active_highlights().is_empty() {
                    fallback_samples += 1;
                }
                samples.push(start.elapsed().as_secs_f64() * 1000.);
            }
            let start = Instant::now();
            EditorWidget::new(
                session.active_buffer(),
                session.active_viewport(),
                &session.active_highlights(),
            )
            .render(output.area, &mut output);
            if round >= 10 {
                draws.push(start.elapsed().as_secs_f64() * 1000.);
            }
            let start = Instant::now();
            session.execute(EditorCommand::Undo).unwrap();
            session.follow_cursor(120, 40);
            EditorWidget::new(
                session.active_buffer(),
                session.active_viewport(),
                &session.active_highlights(),
            )
            .render(output.area, &mut output);
            if round >= 10 {
                undos.push(start.elapsed().as_secs_f64() * 1000.);
            }
        }
        undos.sort_by(f64::total_cmp);
        samples.sort_by(f64::total_cmp);
        draws.sort_by(f64::total_cmp);
        println!(
            "{name}: bytes={} lines={} n={} open_ms={open_ms:.3} edit_render_p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} max_ms={:.3} cached_draw_p95_ms={:.3} fallback_samples={fallback_samples}",
            source.len(),
            source.lines().count(),
            samples.len(),
            samples[99],
            samples[189],
            samples[197],
            samples[199],
            draws[189]
        );
        println!(
            "{name}: undo_render_n={} p95_ms={:.3} p99_ms={:.3}",
            undos.len(),
            undos[189],
            undos[197]
        );
        assert!(
            undos[189] <= 50. && undos[197] <= 100.,
            "{name} exceeds G3 undo/render budgets"
        );
        assert!(
            samples[189] <= 50. && samples[197] <= 100.,
            "{name} exceeds G3 typing/render budgets"
        );
    }
}

fn bounded_source(line: &str, bytes: usize) -> String {
    let mut source = line.repeat(bytes / line.len());
    source.extend(std::iter::repeat_n(' ', bytes - source.len()));
    source
}

#[test]
fn ordered_same_offset_edits_crlf_and_contextual_captures_match_fresh() {
    for (path, source) in [
        (
            "a.rs",
            "#[derive(Debug)]\r\nstruct Café { n: u32 }\r\nmod m { pub fn f() { let s = r#\"世界\"#; } }\r\n// tail\r\n",
        ),
        (
            "Cargo.toml",
            "# header\r\n[package]\r\nname = \"世界\"\r\n[dependencies]\r\nthing = { version = \"1\", optional = true }\r\n",
        ),
    ] {
        let mut session = EditorSession::new(id("a"), path.into(), source, NoopEditorEffects);
        let boundary = source.find("世界").unwrap();
        session
            .active_buffer_mut()
            .apply_edits(
                EditOrigin::Keyboard,
                vec![
                    TextEdit {
                        start_byte: boundary as u64,
                        end_byte: boundary as u64,
                        inserted_text: "e\u{301}".into(),
                    },
                    TextEdit {
                        start_byte: boundary as u64,
                        end_byte: boundary as u64,
                        inserted_text: "🦀".into(),
                    },
                ],
                SelectionState::caret(0),
            )
            .unwrap();
        assert_fresh(&session);
        session.execute(EditorCommand::Undo).unwrap();
        assert_fresh(&session);
        // Drop an early multiline construct, moving all following captures.
        let end = source.find('\n').unwrap() + 1;
        session
            .active_buffer_mut()
            .apply_edits(
                EditOrigin::Keyboard,
                vec![TextEdit {
                    start_byte: 0,
                    end_byte: end as u64,
                    inserted_text: String::new(),
                }],
                SelectionState::caret(0),
            )
            .unwrap();
        assert_fresh(&session);
    }
}

#[test]
fn inactive_document_reload_does_not_change_active_syntax() {
    let mut session = EditorSession::new(id("a"), "a.rs".into(), "fn a() {}", NoopEditorEffects);
    session.open_buffer(
        id("b"),
        "Cargo.toml".into(),
        "name = \"demo\"",
        NoopEditorEffects,
    );
    let active = session.active_highlights().to_vec();
    let transactions = session
        .preview_document_replacement(&id("a"), "// 世界\nfn renamed() {}\n", 1_048_576)
        .unwrap();
    for transaction in transactions {
        session
            .apply_document_transaction(&id("a"), transaction)
            .unwrap();
    }
    assert_eq!(&*session.active_highlights(), &active);
    assert_eq!(session.active_buffer().version(), 0);
    session.execute(EditorCommand::PreviousBuffer).unwrap();
    assert_fresh(&session);
    assert!(
        session
            .active_highlights()
            .iter()
            .any(|span| span.kind == HighlightKind::Comment)
    );
    session.execute(EditorCommand::Undo).unwrap();
    assert_fresh(&session);
}

#[test]
fn widget_preserves_overlapping_capture_precedence_and_plain_gaps() {
    use rustrace::editor::HighlightSpan;
    let session = EditorSession::new(id("a"), "plain.txt".into(), "abcdefghi", NoopEditorEffects);
    let spans = [
        HighlightSpan {
            byte_range: 0..7,
            kind: HighlightKind::String,
        },
        HighlightSpan {
            byte_range: 2..4,
            kind: HighlightKind::Constant,
        },
        HighlightSpan {
            byte_range: 3..5,
            kind: HighlightKind::Keyword,
        },
    ];
    let mut output = Buffer::empty(Rect::new(0, 0, 12, 1));
    EditorWidget::new(session.active_buffer(), session.active_viewport(), &spans)
        .render(output.area, &mut output);
    for (x, color) in [
        (1, Color::Green),
        (2, Color::LightBlue),
        (3, Color::Magenta),
        (4, Color::Magenta),
        (5, Color::Green),
        (7, Color::White),
    ] {
        assert_eq!(output[(x, 0)].fg, color);
    }
}
