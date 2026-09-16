//! Hostile samples are constructed only in memory and rendered into captures.
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier},
    widgets::Widget,
};
use rustrace::display::{self, Limits};
use rustrace::{
    CommandExecution, CommandRunner, ProbeCommand,
    editor::{EditorBuffer, EditorWidget, Movement, NoopEditorEffects, Viewport},
    toolchain::{ProbeStatus, ToolProbe, ToolchainReport},
    tui::{
        BufferTabViewEntry, JournalHealth, MainView, MainViewState, RecordingState, theme::Palette,
    },
};
use rustrace_editor::position::{ByteOffset, PositionError, VisualPosition};
use rustrace_model::DocumentId;

#[test]
fn renderer_width_source_diff_caret_and_vertical_coordinates_agree() {
    for cluster in ["ｶﾞ", "ﾊﾟ", "aﾞ", "aﾟ", "ﾞ", "ﾟ", "ｶ゙", "ﾊ゚", "e\u{301}", "界", "👩🏽‍🔬"]
    {
        let mut native = Buffer::empty(Rect::new(0, 0, 40, 1));
        let (width, _) = native.set_stringn(0, 0, cluster, 40, ratatui::style::Style::default());
        let source = format!("{cluster}X\tY\n0123456789");
        let mut editor = EditorBuffer::new(
            DocumentId::new("renderer-width").unwrap(),
            &source,
            NoopEditorEffects,
        );
        let hash = editor.hash();
        editor.move_cursor(Movement::Right, false);
        assert_eq!(
            editor.cursor().display_column,
            usize::from(width),
            "caret for {cluster:?}"
        );
        assert_eq!(editor.selection_state().active_byte, cluster.len() as u64);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 2));
        EditorWidget::new(&editor, &Viewport::default(), &[]).render(buffer.area, &mut buffer);
        assert_eq!(buffer[(width, 0)].symbol(), "X");
        assert!(buffer[(width, 0)].modifier.contains(Modifier::REVERSED));
        let emitted = Buffer::empty(buffer.area)
            .diff_iter(&buffer)
            .map(|(_, _, cell)| cell.symbol())
            .collect::<String>();
        assert!(
            emitted.contains('X'),
            "native diff omitted source after {cluster:?}"
        );
        let tab_stop = (width + 1) / 4 * 4 + 4;
        assert_eq!(buffer[(tab_stop, 0)].symbol(), "Y");
        editor.move_cursor(Movement::Down, false);
        assert_eq!(editor.cursor().display_column, usize::from(width));
        assert_eq!(
            editor.selection_state().active_byte,
            (cluster.len() + 4 + usize::from(width)) as u64
        );
        editor.move_cursor(Movement::Up, false);
        assert_eq!(editor.selection_state().active_byte, cluster.len() as u64);
        assert_eq!(editor.hash(), hash);
        assert_eq!(editor.text(), source);
    }
}

#[test]
fn exact_position_cells_match_native_rendering_of_safe_source_symbols() {
    let clusters = [
        "ｶﾞ".to_owned(),
        "ﾊﾟ".to_owned(),
        "aﾞ".to_owned(),
        "ﾞ".to_owned(),
        "\u{301}".to_owned(),
        "\u{200b}".to_owned(),
        "\u{200d}".to_owned(),
        "\u{fe0f}".to_owned(),
        "\u{1b}".to_owned(),
        "\u{202e}".to_owned(),
        "e\u{301}".to_owned(),
        "👩🏽‍🔬".to_owned(),
        format!("e{}", "\u{301}".repeat(128)),
    ];
    for cluster in clusters {
        let (symbol, declared_width) = display::grapheme(&cluster, 0);
        let mut native = Buffer::empty(Rect::new(0, 0, 80, 1));
        let (native_width, _) =
            native.set_stringn(0, 0, symbol.as_ref(), 80, ratatui::style::Style::default());
        assert_eq!(declared_width, usize::from(native_width), "{cluster:?}");

        let source = format!("{cluster}X");
        let editor = EditorBuffer::new(
            DocumentId::new("position-native-width").unwrap(),
            &source,
            NoopEditorEffects,
        );
        let positions = editor.positions(0).unwrap();
        let boundary = ByteOffset(cluster.len() as u64);
        let visual = VisualPosition {
            line: 0,
            column: usize::from(native_width),
        };
        assert_eq!(
            positions.byte_to_visual(boundary),
            Ok(visual),
            "{cluster:?}"
        );
        assert_eq!(
            positions.visual_to_byte(visual),
            Ok(boundary),
            "{cluster:?}"
        );
        for column in 1..usize::from(native_width) {
            assert_eq!(
                positions.visual_to_byte(VisualPosition { line: 0, column }),
                Err(PositionError::NonAddressableVisualColumn),
                "{cluster:?} column {column}"
            );
        }
        assert_eq!(editor.text(), source);
    }
}

#[test]
fn renderer_width_output_tabs_and_escaped_clusters_agree() {
    for cluster in [
        "ｶﾞ",
        "ﾊﾟ",
        "aﾞ",
        "aﾟ",
        "ﾞ",
        "ﾟ",
        "ｶ゙",
        "ﾊ゚",
        "e\u{301}",
        "界",
        "👩🏽‍🔬",
        "a\u{034f}ﾞ",
    ] {
        let (symbol, width) = display::grapheme(cluster, 0);
        let mut native = Buffer::empty(Rect::new(0, 0, 80, 1));
        let (actual, _) =
            native.set_stringn(0, 0, symbol.as_ref(), 80, ratatui::style::Style::default());
        assert_eq!(
            width,
            usize::from(actual),
            "safe symbol width for {cluster:?}"
        );
        for styled in [false, true] {
            let input = if styled {
                format!("\u{1b}[31m{cluster}\u{1b}[0m\tX")
            } else {
                format!("{cluster}\tX")
            };
            let parsed = display::output(input.as_bytes(), Limits::default());
            let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 1));
            ratatui::widgets::Paragraph::new(parsed.text).render(buffer.area, &mut buffer);
            assert_eq!(
                buffer[((actual / 4 + 1) * 4, 0)].symbol(),
                "X",
                "tab stop for {cluster:?}"
            );
        }
    }
}

fn symbols(buffer: &Buffer) -> String {
    buffer.content().iter().map(|cell| cell.symbol()).collect()
}

#[test]
fn source_controls_have_visible_width_and_original_caret_mapping() {
    let source = "a\u{1b}b\u{202e}c";
    let mut editor = EditorBuffer::new(
        DocumentId::new("display-source").unwrap(),
        source,
        NoopEditorEffects,
    );
    editor.move_cursor(Movement::Right, false);
    editor.move_cursor(Movement::Right, false);
    let before_hash = editor.hash();
    let before_selection = editor.selection_state();
    let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 1));
    EditorWidget::new(&editor, &Viewport::default(), &[]).render(buffer.area, &mut buffer);

    assert!(symbols(&buffer).starts_with("a\\u{1b}b\\u{202e}c"));
    assert_eq!(editor.cursor().display_column, 7);
    assert!(buffer[(7, 0)].modifier.contains(Modifier::REVERSED));
    assert_eq!(editor.text(), source);
    assert_eq!(editor.hash(), before_hash);
    assert_eq!(editor.selection_state(), before_selection);
}

#[test]
fn existing_main_output_parses_colors_and_exposes_bidi_controls() {
    let editor = EditorBuffer::new(
        DocumentId::new("display-output").unwrap(),
        "fn main() {}",
        NoopEditorEffects,
    );
    let state = MainViewState::new(
        "Lab\u{202e}name",
        vec![BufferTabViewEntry::new("src/main.rs", true, false)],
        vec!["\u{1b}[31merror\u{1b}[0m: bad\u{202e}name".into()],
        RecordingState::Active,
        JournalHealth::Healthy,
        "saved",
    );
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 24));
    MainView::new(&state, &editor, &Viewport::default(), &[]).render(buffer.area, &mut buffer);
    let text = symbols(&buffer);
    assert!(!text.contains('\u{202e}'));
    assert!(!text.contains("Lab") && !text.contains("Lab\\u{202e}name"));
    assert!(text.contains("error: bad\\u{202e}name"));
    assert!(
        buffer
            .content()
            .iter()
            .any(|cell| cell.symbol() == "e" && cell.fg == Color::Red)
    );
}

#[test]
fn discovery_display_is_safe_without_rewriting_observations() {
    let report = ToolchainReport {
        assignment_pin: Some("1.98.1".into()),
        selected_toolchain: Some("1.98.1".into()),
        working_directory: ".".into(),
        probes: vec![ToolProbe {
            component: "rustc\u{1b}]0;title\u{7}".into(),
            purpose: "version".into(),
            required: true,
            argv: vec!["rustc".into(), "-vV".into()],
            status: ProbeStatus::Failed,
            stdout: "original\u{1b}[31m".into(),
            stderr: "bad\u{202e}name".into(),
            exit_code: Some(1),
            timeout_ms: None,
            output_limited: false,
            detail: "version unavailable".into(),
            remediation: "Install the selected toolchain manually".into(),
        }],
    };
    let exact = serde_json::to_vec(&report).unwrap();
    let mut capture = Vec::new();
    report.write_diagnostics(&mut capture).unwrap();
    let display = String::from_utf8(capture).unwrap();
    assert!(!display.contains('\u{1b}'));
    assert!(!display.contains('\u{202e}'));
    assert!(display.contains("Install the selected toolchain manually"));
    assert!(display.contains("Failed"));
    assert_eq!(serde_json::to_vec(&report).unwrap(), exact);
}

struct HostileVersion;
impl CommandRunner for HostileVersion {
    fn run(&self, _: &ProbeCommand) -> CommandExecution {
        CommandExecution::Succeeded {
            stdout: "version\u{202e}spoof".into(),
            stderr: String::new(),
        }
    }
}

#[test]
fn environment_cli_does_not_emit_bidi_controls() {
    let mut capture = Vec::new();
    rustrace::run_cli(["rustrace", "environment"], &HostileVersion, &mut capture);
    let display = String::from_utf8(capture).unwrap();
    assert!(!display.contains('\u{202e}'));
    assert!(display.contains("version\\u{202e}spoof"));
}

fn assert_safe_symbols(buffer: &Buffer) {
    for cell in buffer.content() {
        assert!(
            !cell.symbol().chars().any(display::must_escape),
            "unsafe cell"
        );
        assert!(cell.symbol().len() <= display::MAX_GRAPHEME_BYTES);
    }
}

#[test]
fn ansi_allowlist_maps_colors_and_resets_without_raw_escape_bytes() {
    let parsed = display::output(
        b"\x1b[1;31mred\x1b[22;39mplain\x1b[38;5;123mindex\x1b[48;2;1;2;3mrgb\x1b[0mreset",
        Limits::default(),
    );
    assert!(!parsed.truncated);
    let spans = &parsed.text.lines[0].spans;
    assert_eq!(spans.len(), 5);
    assert_eq!(spans[0].content, "red");
    assert_eq!(spans[0].style.fg, Some(Color::Red));
    assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(spans[1].style.fg, Some(Color::Reset));
    assert!(spans[1].style.sub_modifier.contains(Modifier::BOLD));
    assert_eq!(spans[2].style.fg, Some(Color::Indexed(123)));
    assert_eq!(spans[3].style.bg, Some(Color::Rgb(1, 2, 3)));
    assert_eq!(spans[4].style, ratatui::style::Style::default());
    assert!(!parsed.text.to_string().contains('\u{1b}'));
}

#[test]
fn unsafe_malformed_truncated_and_oversized_escapes_are_literal() {
    let samples = [
        "\u{1b}]0;title\u{7}",
        "\u{1b}]52;c;clipboard\u{1b}\\",
        "\u{1b}[999;999Houtside",
        "\u{1b}Ppayload\u{1b}\\",
        "\u{1b}_payload\u{1b}\\",
        "\u{1b}[5;8mhidden/blink",
        "\u{1b}[31;999munsupported",
        "\u{1b}[38;2;1;2mshort RGB",
        "\u{1b}[38:2:1:2:3mcolon",
        "\u{1b}[31",
        "\u{1b}",
        "\u{9b}31mC1\u{9d}0;title\u{9c}",
    ];
    for sample in samples {
        let parsed = display::output(sample.as_bytes(), Limits::default());
        assert!(parsed.text.to_string().starts_with("\\u{"));
        assert!(
            parsed
                .text
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style == ratatui::style::Style::default())
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 3));
        ratatui::widgets::Paragraph::new(parsed.text).render(buffer.area, &mut buffer);
        assert_safe_symbols(&buffer);
    }
    let oversized = format!("\u{1b}[{}mend", "0;".repeat(4096));
    let parsed = display::output(oversized.as_bytes(), Limits::default());
    assert!(parsed.text.to_string().starts_with("\\u{1b}[0;"));
    assert!(parsed.spans <= display::MAX_SPANS);
}

#[test]
fn binary_bytes_and_partial_utf8_have_deterministic_visible_representations() {
    let bytes = [b'A', 0xff, 0xc3, b'(', 0x9b, 0, 0xe2, 0x82];
    let exact = bytes;
    let parsed = display::output(&bytes, Limits::default());
    assert_eq!(parsed.text.to_string(), "A\\xff\\xc3(\\x9b\\u{0}\\xe2\\x82");
    assert_eq!(bytes, exact);
    assert!(!parsed.truncated);
}

#[test]
fn unicode_tabs_line_breaks_and_styles_inside_a_grapheme_remain_readable() {
    let text = "東京 e\u{301} 👩🏽‍🔬 العربية\tend\r\nnext\rstill\n";
    let parsed = display::output(text.as_bytes(), Limits::default());
    assert!(!parsed.truncated);
    assert!(
        parsed
            .text
            .to_string()
            .starts_with("東京 e\u{301} 👩🏽‍🔬 العربية")
    );
    assert!(parsed.text.to_string().contains("end\nnext\\rstill\n"));
    assert!(!parsed.text.to_string().contains('\t'));
    assert_eq!(parsed.text.lines.len(), 3);

    let split = display::output("e\u{1b}[31m\u{301}X".as_bytes(), Limits::default());
    assert_eq!(split.text.to_string(), "e\u{301}X");
    assert_eq!(split.text.lines[0].spans[0].content, "e\u{301}");
    assert_eq!(split.text.lines[0].spans[1].style.fg, Some(Color::Red));
}

#[test]
fn nonprinting_policy_is_explicit_and_large_clusters_never_form_huge_cells() {
    for character in [
        '\u{061c}', '\u{200e}', '\u{202e}', '\u{2066}', '\u{2069}', '\u{feff}', '\u{00ad}',
        '\u{034f}', '\u{200b}', '\u{200d}', '\u{fe0f}', '\u{0301}',
    ] {
        let value = character.to_string();
        let label = display::label(&value, 160);
        assert!(
            label.starts_with("\\u{"),
            "invisible scalar must be explicit"
        );
        assert!(!label.contains(character));
    }
    assert_eq!(display::label("é e\u{301} 👩🏽‍🔬", 160), "é e\u{301} 👩🏽‍🔬");
    let huge = format!("e{}", "\u{301}".repeat(524_287));
    let (symbol, width) = display::grapheme(&huge, 0);
    assert_eq!(symbol, "[grapheme]");
    assert_eq!(width, 10);
    let editor = EditorBuffer::new(
        DocumentId::new("huge-cluster").unwrap(),
        &huge,
        NoopEditorEffects,
    );
    let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 1));
    EditorWidget::new(&editor, &Viewport::default(), &[]).render(buffer.area, &mut buffer);
    assert!(symbols(&buffer).starts_with("[grapheme]"));
    assert_safe_symbols(&buffer);
    assert_eq!(editor.text(), huge);
}

#[test]
fn source_selection_and_syntax_keep_original_byte_ranges() {
    use rustrace::editor::{HighlightKind, HighlightSpan};
    let source = "A\u{1b}界e\u{301}\tZ";
    let mut editor = EditorBuffer::new(
        DocumentId::new("mapped-source").unwrap(),
        source,
        NoopEditorEffects,
    );
    editor
        .set_selection(rustrace_model::SelectionState {
            anchor_byte: 1,
            active_byte: 2,
        })
        .unwrap();
    let highlights = [HighlightSpan {
        byte_range: 2..5,
        kind: HighlightKind::Function,
    }];
    let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 1));
    EditorWidget::new(&editor, &Viewport::default(), &highlights).render(buffer.area, &mut buffer);
    for x in 1..7 {
        assert_eq!(buffer[(x, 0)].bg, Color::Blue);
    }
    assert_eq!(buffer[(7, 0)].symbol(), "界");
    assert_eq!(buffer[(7, 0)].fg, Color::Cyan);
    assert!(buffer[(7, 0)].modifier.contains(Modifier::REVERSED));
    assert_eq!(buffer[(9, 0)].symbol(), "e\u{301}");
    assert_eq!(buffer[(12, 0)].symbol(), "Z");
    assert_eq!(editor.selection_state().active_byte, 2);
    assert_eq!(editor.text(), source);
}

#[test]
fn matching_bracket_cell_uses_the_theme_accent_without_recorded_or_text_changes() {
    let source = "fn main() {}";
    let mut editor = EditorBuffer::new(
        DocumentId::new("matching-bracket").unwrap(),
        source,
        NoopEditorEffects,
    );
    let opening = source.find('{').unwrap();
    let closing = source.find('}').unwrap();
    editor
        .set_selection(rustrace_model::SelectionState::caret((closing + 1) as u64))
        .unwrap();
    let palette = Palette::catppuccin();
    let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 1));

    EditorWidget::new(&editor, &Viewport::default(), &[])
        .with_palette(&palette)
        .render(buffer.area, &mut buffer);

    assert_eq!(buffer[(opening as u16, 0)].symbol(), "{");
    assert_eq!(buffer[(opening as u16, 0)].fg, palette.accent);
    assert!(
        buffer[(opening as u16, 0)]
            .modifier
            .contains(Modifier::BOLD)
    );
    assert_eq!(editor.text(), source);
    assert_eq!(editor.version(), 0);
}

#[test]
fn parser_limits_bound_input_output_lines_and_spans_including_markers() {
    let alternating = "\u{1b}[31ma\u{1b}[32mb\n".repeat(100_000);
    for input in [0, 1, 3, 64, 1024, usize::MAX] {
        for output in [0, 1, 18, 19, 20, 64, 1024, usize::MAX] {
            for spans in [0, 1, 2, 8, usize::MAX] {
                for lines in [0, 1, 4, usize::MAX] {
                    let limits = Limits {
                        input_bytes: input,
                        output_bytes: output,
                        spans,
                        lines,
                    };
                    let parsed = display::output(alternating.as_bytes(), limits);
                    assert!(parsed.input_bytes <= input.min(display::MAX_INPUT_BYTES));
                    assert!(parsed.output_bytes <= output.min(display::MAX_OUTPUT_BYTES));
                    assert!(parsed.text.to_string().len() <= output.min(display::MAX_OUTPUT_BYTES));
                    assert!(parsed.spans <= spans.min(display::MAX_SPANS));
                    assert!(parsed.text.lines.len() <= lines.min(display::MAX_LINES));
                    assert!(parsed.truncated);
                }
            }
        }
    }
}

#[test]
fn seeded_binary_property_never_emits_input_control_and_preserves_bytes() {
    // 512 deterministic generated records, independent of parser implementation.
    let mut seed = 0x0743_8020_0d15_a1a9_u64;
    for case in 0..512 {
        let mut bytes = Vec::new();
        for _ in 0..case * 3 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            bytes.push(seed as u8);
        }
        let hash = blake3::hash(&bytes);
        let parsed = display::output(
            &bytes,
            Limits {
                output_bytes: 4096,
                lines: 8,
                ..Limits::default()
            },
        );
        assert_eq!(blake3::hash(&bytes), hash);
        assert!(
            !parsed
                .text
                .to_string()
                .chars()
                .any(|c| c != '\n' && display::must_escape(c))
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 8));
        ratatui::widgets::Paragraph::new(parsed.text).render(buffer.area, &mut buffer);
        assert_safe_symbols(&buffer);
    }
}

#[test]
fn narrow_offset_panes_never_write_into_neighboring_cells() {
    for source in ["\u{1b}]52;c;data\u{7}界", "👩🏽‍🔬e\u{301}\tZ", "\u{202e}abc"] {
        let editor = EditorBuffer::new(
            DocumentId::new("narrow").unwrap(),
            source,
            NoopEditorEffects,
        );
        for width in 0..12 {
            let mut buffer =
                Buffer::filled(Rect::new(0, 0, 20, 5), ratatui::buffer::Cell::new("#"));
            let area = Rect::new(3, 2, width, 1);
            EditorWidget::new(&editor, &Viewport::default(), &[]).render(area, &mut buffer);
            for y in 0..5 {
                for x in 0..20 {
                    if y != 2 || x < 3 || x >= 3 + width {
                        assert_eq!(buffer[(x, y)].symbol(), "#");
                    }
                }
            }
            assert_safe_symbols(&buffer);
        }
    }
}

#[test]
fn a_wide_source_caret_remains_visible_in_a_one_cell_pane() {
    let editor = EditorBuffer::new(
        DocumentId::new("wide-caret").unwrap(),
        "界",
        NoopEditorEffects,
    );
    let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
    EditorWidget::new(&editor, &Viewport::default(), &[]).render(buffer.area, &mut buffer);
    assert!(buffer[(0, 0)].modifier.contains(Modifier::REVERSED));
}

#[test]
fn small_display_budgets_do_not_truncate_text_that_fits() {
    for value in ["a", "abc", "界", "e\u{301}", "\\u{1b}"] {
        assert_eq!(display::label(value, value.len()), value);
        let parsed = display::output(
            value.as_bytes(),
            Limits {
                output_bytes: value.len(),
                spans: 1,
                lines: 1,
                ..Limits::default()
            },
        );
        assert!(!parsed.truncated);
        assert_eq!(parsed.text.to_string(), value);
    }
}

#[test]
fn captured_crossterm_output_contains_only_trusted_styles_and_pane_positions() {
    use ratatui::backend::{Backend, CrosstermBackend};
    let input = b"\x1b[31merror\x1b[0m\x1b]52;c;data\x07\x1b[999;999H\xff";
    let parsed = display::output(input, Limits::default());
    let area = Rect::new(7, 4, 80, 2);
    let mut buffer = Buffer::empty(area);
    ratatui::widgets::Paragraph::new(parsed.text).render(area, &mut buffer);
    let mut capture = Vec::new();
    {
        let mut backend = CrosstermBackend::new(&mut capture);
        backend
            .draw(buffer.content().iter().enumerate().map(|(index, cell)| {
                (
                    area.x + index as u16 % area.width,
                    area.y + index as u16 / area.width,
                    cell,
                )
            }))
            .unwrap();
        Backend::flush(&mut backend).unwrap();
    }
    let mut printable = Vec::new();
    let mut at = 0;
    let mut framing = 0;
    while at < capture.len() {
        if capture[at] != 0x1b {
            printable.push(capture[at]);
            at += 1;
            continue;
        }
        assert_eq!(capture[at + 1], b'[');
        let start = at + 2;
        at = start;
        while capture[at].is_ascii_digit() || capture[at] == b';' {
            at += 1;
        }
        match capture[at] {
            b'm' => {}
            b'H' => {
                let coordinates: Vec<u16> = std::str::from_utf8(&capture[start..at])
                    .unwrap()
                    .split(';')
                    .map(|value| value.parse().unwrap())
                    .collect();
                assert_eq!(coordinates.len(), 2);
                assert!((area.y + 1..=area.bottom()).contains(&coordinates[0]));
                assert!((area.x + 1..=area.right()).contains(&coordinates[1]));
            }
            _ => panic!("untrusted terminal framing"),
        }
        framing += 1;
        at += 1;
    }
    assert!(framing > 0);
    let printable = String::from_utf8(printable).unwrap();
    assert!(!printable.chars().any(display::must_escape));
    assert!(printable.contains("error\\u{1b}]52;c;data\\u{7}\\u{1b}[999;999H\\xff"));
}

#[test]
#[ignore = "run in release mode during a coordinated responsiveness window"]
fn bounded_safe_display_responsiveness_probe() {
    use std::time::Instant;
    let records = [
        ("plain", b"readable compiler output\n".repeat(50_000)),
        ("style-churn", b"\x1b[31ma\x1b[32mb".repeat(100_000)),
        (
            "malformed",
            b"\x1b]52;c;\xff\x00\x1b[999;999H".repeat(50_000),
        ),
        (
            "oversized-grapheme",
            format!("e{}", "\u{301}".repeat(524_287)).into_bytes(),
        ),
    ];
    for (name, bytes) in records {
        let exact = blake3::hash(&bytes);
        let mut samples = Vec::new();
        for round in 0..210 {
            let start = Instant::now();
            let view = display::output(&bytes, Limits::default());
            let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
            ratatui::widgets::Paragraph::new(view.text).render(buffer.area, &mut buffer);
            std::hint::black_box(&buffer);
            if round >= 10 {
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
            }
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "safe-output-{name}: input_bytes={} n=200 p95_ms={:.3} p99_ms={:.3} max_ms={:.3}",
            bytes.len(),
            samples[189],
            samples[197],
            samples[199]
        );
        assert_eq!(blake3::hash(&bytes), exact);
        assert!(samples[189] <= 50.0 && samples[197] <= 100.0);
    }
    for (name, source) in [
        ("control-line", "\u{1b}".repeat(1_048_576)),
        (
            "combining-cluster",
            format!("e{}", "\u{301}".repeat(524_287)),
        ),
    ] {
        let mut editor = EditorBuffer::new(
            DocumentId::new("safe-display-probe").unwrap(),
            &source,
            NoopEditorEffects,
        );
        editor.move_cursor(Movement::DocumentEnd, false);
        let exact = editor.hash();
        let mut samples = Vec::new();
        for round in 0..110 {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
            let start = Instant::now();
            EditorWidget::new(&editor, &Viewport::default(), &[]).render(buffer.area, &mut buffer);
            std::hint::black_box(&buffer);
            if round >= 10 {
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
            }
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "safe-source-{name}: input_bytes={} n=100 p95_ms={:.3} p99_ms={:.3} max_ms={:.3}",
            source.len(),
            samples[94],
            samples[98],
            samples[99]
        );
        assert_eq!(editor.hash(), exact);
        assert_eq!(editor.text(), source);
        assert!(samples[94] <= 50.0 && samples[98] <= 100.0);
    }
}
