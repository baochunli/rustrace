use std::cell::RefCell;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier};
use rustrace::config::PrimaryModifier;
use rustrace::editor::{
    EditorBuffer, HighlightKind, HighlightSpan, LiveDiagnosticSpan, NoopEditorEffects, Viewport,
};
use rustrace::ghostty::{GhosttyBindingStatus, GhosttyKeyBindings};
use rustrace::session::PASTE_BLOCKED_WARNING;
use rustrace::tui::shell::{
    BottomPane, SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH, ShellLayout, ShellLayoutResult, shell_layout,
    shell_layout_with_bottom_height, shell_layout_with_sizes,
};
use rustrace::tui::theme::{Palette, ThemeConfig, ThemeName};
use rustrace::tui::{
    BufferTabViewEntry, COMMAND_MENU_ENTRIES, CTRL_W_DELETE_SELECTED_HELP_ENTRY, CommandMenuAction,
    CompletionViewItem, ConfirmationState, DiagnosticLineMarker, DiagnosticMarkerKind, DrawGate,
    EDITOR_CONTEXT_MENU_ENTRIES, EDITOR_KEY_HINTS, EditorContextMenuKeyAction,
    EditorContextMenuState, FILES_CONTEXT_MENU_ENTRIES, FilePromptKind, FilePromptState,
    FileTreeViewEntry, FilesContextMenuAction, FilesContextMenuKeyAction, FilesContextMenuState,
    FindPanelField, FindPanelState, JournalHealth, KEYBIND_ROWS, MIN_TERMINAL_HEIGHT,
    MIN_TERMINAL_WIDTH, MainLayout, MainView, MainViewState, ModeBarKind, ModeBarState, MouseState,
    OBVIOUS_EDITOR_KEYBIND_ROWS, OutputRow, PaneResizeState, RecordingState, ShellInput,
    ShellModal, ShellState, TerminalOperations, TerminalSession, TestCasePickerRow,
    TestCasePickerState, ToastKind, ToastState, WorkspaceInput, command_menu_action,
    editor_context_menu_key_action, files_context_menu_key_action, main_layout,
    mouse_input_for_event, primary_modifier_text, primary_modifier_text_with_ghostty,
};
use rustrace_model::{DocumentId, OutputStream, SelectionState};

fn view_state(recording: RecordingState, journal: JournalHealth) -> MainViewState {
    MainViewState::new(
        "Ownership and Borrowing Lab",
        vec![
            BufferTabViewEntry::new("src/main.rs", true, false),
            BufferTabViewEntry::new("src/parser.rs", false, false),
        ],
        vec![
            "warning: unused variable on line 8".to_owned(),
            "cargo check: finished".to_owned(),
        ],
        recording,
        journal,
        "active",
    )
}

fn editor() -> EditorBuffer<NoopEditorEffects> {
    EditorBuffer::new(
        DocumentId::new("tui-test").unwrap(),
        "fn main() {\n  let crab = \"🦀\";\n}\n",
        NoopEditorEffects,
    )
}

fn render(width: u16, height: u16, state: &MainViewState) -> Terminal<TestBackend> {
    let editor = editor();
    render_with_editor(width, height, state, &editor)
}

fn render_with_editor(
    width: u16,
    height: u16,
    state: &MainViewState,
    editor: &EditorBuffer<NoopEditorEffects>,
) -> Terminal<TestBackend> {
    render_with_editor_palette(width, height, state, editor, Palette::terminal())
}

fn render_with_editor_palette(
    width: u16,
    height: u16,
    state: &MainViewState,
    editor: &EditorBuffer<NoopEditorEffects>,
    palette: Palette,
) -> Terminal<TestBackend> {
    let viewport = Viewport::default();
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| {
            frame.render_widget(
                MainView::new(state, editor, &viewport, &[]).with_palette(palette.clone()),
                frame.area(),
            );
        })
        .unwrap();

    terminal
}

fn rendered_lines(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let width = terminal.backend().buffer().area.width as usize;
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect())
        .collect()
}

fn buffer_lines(buffer: &Buffer) -> Vec<String> {
    let width = usize::from(buffer.area.width);
    buffer
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect())
        .collect()
}

fn completion_items() -> Vec<CompletionViewItem> {
    (0..16)
        .map(|index| {
            CompletionViewItem::new(
                if index == 15 {
                    "malicious\u{1b}[2J\nlabel".to_owned()
                } else {
                    format!("item-{index}")
                },
                Some(6),
            )
        })
        .collect()
}

#[test]
fn completion_popup_golden_at_80x24_anchors_below_the_caret() {
    let area = Rect::new(0, 0, 80, 24);
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_mode_bar(ModeBarState::new(
            ModeBarKind::Complete,
            "esc close  tab/↵ accept  ↑↓ select",
        ))
        .with_completion_popup(completion_items(), 0);
    let editor = editor();
    let viewport = Viewport::default();
    let mut buffer = Buffer::empty(area);
    let hits = MainView::new(&state, &editor, &viewport, &[])
        .with_palette(Palette::terminal())
        .render_with_hit_map(area, &mut buffer);
    let lines = buffer_lines(&buffer);

    assert_eq!(hits.completion_rows.len(), 8);
    assert_eq!(hits.completion_rows[0].1, 0);
    assert_eq!(hits.completion_rows[7].1, 7);
    let first = hits.completion_rows[0].0;
    assert_eq!(first.y, 3, "first item is below the popup frame and caret");
    assert_eq!(lines[2].chars().nth(first.x as usize - 1), Some('┌'));
    assert!(lines[3].contains("item-0"), "{}", lines[3]);
    assert!(lines[3].contains("kind 6"), "{}", lines[3]);
    assert!(
        lines.iter().any(|line| line.contains('▐')),
        "scrollbar missing:\n{}",
        lines.join("\n")
    );
    let selected = buffer.cell((first.x, first.y)).unwrap();
    assert_eq!(selected.bg, Palette::terminal().accent);
    assert_eq!(selected.fg, Palette::terminal().panel_contrast_fg());
}

#[test]
fn completion_popup_mouse_targets_accept_scroll_and_dismiss_on_left_down_only() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_mode_bar(ModeBarState::new(
            ModeBarKind::Complete,
            "esc close  tab/↵ accept  ↑↓ select",
        ))
        .with_completion_popup(completion_items(), 0);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = rustrace::tui::shell::HitMap::default();
    terminal
        .draw(|frame| {
            hits = MainView::new(&state, &editor(), &Viewport::default(), &[])
                .with_palette(Palette::terminal())
                .render_with_hit_map(frame.area(), frame.buffer_mut());
        })
        .unwrap();
    let shell = ShellState {
        modal: ShellModal::Completion,
    };
    let (row, index) = hits.completion_rows[3];
    let mut reducer = MouseState::default();

    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Down(MouseButton::Left), row.x, row.y),
            1,
            &hits,
            shell,
        ),
        Some(ShellInput::AcceptCompletion(index))
    );
    for kind in [
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
    ] {
        assert_eq!(
            reduce_and_map(&mut reducer, mouse(kind, row.x, row.y), 2, &hits, shell),
            None,
            "{kind:?} activated a completion row"
        );
    }
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(
                MouseEventKind::ScrollDown,
                hits.completion_popup.x,
                hits.completion_popup.y,
            ),
            3,
            &hits,
            shell,
        ),
        Some(ShellInput::ScrollCompletion(3))
    );
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                hits.sidebar_new.x,
                hits.sidebar_new.y,
            ),
            4,
            &hits,
            shell,
        ),
        Some(ShellInput::DismissCompletion)
    );
}

#[test]
fn completion_popup_golden_at_80x24_moves_above_and_sanitizes_labels() {
    let area = Rect::new(0, 0, 80, 24);
    let source = (0..15)
        .map(|index| format!("line_{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut editor = EditorBuffer::new(
        DocumentId::new("completion-above").unwrap(),
        &source,
        NoopEditorEffects,
    );
    editor.move_cursor(rustrace::editor::Movement::DocumentEnd, false);
    let mut viewport = Viewport::default();
    let MainLayout::Full(layout) = shell_layout(area, BottomPane::Output, true) else {
        unreachable!();
    };
    viewport.follow_cursor(
        &editor,
        usize::from(layout.editor.width),
        usize::from(layout.editor.height),
    );
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_mode_bar(ModeBarState::new(
            ModeBarKind::Complete,
            "esc close  tab/↵ accept  ↑↓ select",
        ))
        .with_completion_popup(completion_items(), 15);
    let mut buffer = Buffer::empty(area);
    let hits = MainView::new(&state, &editor, &viewport, &[])
        .with_palette(Palette::terminal())
        .render_with_hit_map(area, &mut buffer);
    let lines = buffer_lines(&buffer);
    let caret_y = layout.editor.y + 14;

    assert_eq!(hits.completion_rows.len(), 8);
    assert!(
        hits.completion_rows.last().unwrap().0.bottom() <= caret_y,
        "popup is not above the caret: {:?}",
        hits.completion_rows
    );
    let output = lines.join("\n");
    assert!(output.contains("malicious\\u{1b}[2J\\nlabel"), "{output}");
    assert!(!output.contains(''));
    assert_eq!(hits.completion_rows.last().unwrap().1, 15);
}

#[test]
fn completion_popup_golden_at_80x24_never_intersects_a_middle_caret() {
    let area = Rect::new(0, 0, 80, 24);
    let source = (0..15)
        .map(|index| format!("line_{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut editor = EditorBuffer::new(
        DocumentId::new("completion-middle").unwrap(),
        &source,
        NoopEditorEffects,
    );
    for _ in 0..7 {
        editor.move_cursor(rustrace::editor::Movement::Down, false);
    }
    let viewport = Viewport::default();
    let MainLayout::Full(layout) = shell_layout(area, BottomPane::Output, true) else {
        unreachable!();
    };
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_mode_bar(ModeBarState::new(
            ModeBarKind::Complete,
            "esc close  tab/↵ accept  ↑↓ select",
        ))
        .with_completion_popup(completion_items(), 8);
    let mut buffer = Buffer::empty(area);
    let hits = MainView::new(&state, &editor, &viewport, &[])
        .with_palette(Palette::terminal())
        .render_with_hit_map(area, &mut buffer);
    let caret_y = layout.editor.y + 7;

    assert_eq!(hits.completion_rows.len(), 5);
    assert_eq!(hits.completion_rows.first().unwrap().1, 4);
    assert_eq!(hits.completion_rows.last().unwrap().1, 8);
    let popup_top = hits.completion_rows.first().unwrap().0.y - 1;
    let popup_bottom = hits.completion_rows.last().unwrap().0.bottom() + 1;

    assert!(
        popup_bottom <= caret_y || popup_top > caret_y,
        "popup {popup_top}..{popup_bottom} intersects caret row {caret_y}"
    );
    assert_eq!(
        (popup_top, popup_bottom),
        (caret_y + 1, layout.editor.bottom())
    );
    let output = buffer_lines(&buffer).join("\n");
    assert!(output.contains("item-8"), "{output}");
    assert!(output.contains('▐'), "{output}");
}

fn assert_student_shell_golden(width: u16, height: u16) {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy).with_file_tree(vec![
        FileTreeViewEntry::new("src/main.rs", true, true, false, true),
        FileTreeViewEntry::new("src/parser.rs", false, false, true, true),
        FileTreeViewEntry::new("Cargo.toml", false, false, false, false),
    ]);
    let terminal = render(width, height, &state);
    let rows = rendered_lines(&terminal);
    let output = rows.join("\n");

    let contract = [
        ("sidebar header", output.contains(" files")),
        ("active file dot", output.contains("● src/main.rs")),
        ("sidebar footer", output.contains(" new")),
        ("sidebar menu", output.contains("menu")),
        ("tab position", output.contains("Ln 1, Col 1")),
        (
            "assignment title hidden",
            !output.contains("Ownership") && !output.contains("Borrowing Lab"),
        ),
        ("output header", output.contains("output")),
        ("borderless", !output.contains(['┌', '┐', '└', '┘'])),
        ("no status line", !output.contains("Status")),
        ("healthy recording silent", !output.contains("Recording:")),
        ("healthy journal silent", !output.contains("Journal:")),
    ];
    assert!(
        contract.iter().all(|(_, passed)| *passed),
        "student shell golden {width}x{height} failed: {contract:?}\n{output}"
    );
    let ShellLayoutResult::Full(layout) =
        shell_layout(Rect::new(0, 0, width, height), BottomPane::Output, false)
    else {
        unreachable!();
    };
    let divider_x = usize::from(layout.sidebar_divider.x);
    assert!(
        rows.iter()
            .enumerate()
            .all(|(y, row)| row.chars().nth(divider_x)
                == Some(if y == usize::from(layout.gap.y) {
                    '├'
                } else {
                    '│'
                })),
        "student shell golden {width}x{height} lacks the joined full-height divider\n{output}"
    );
    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer
            .cell((layout.sidebar_divider.x, layout.gap.y))
            .unwrap()
            .symbol(),
        "├",
        "inactive divider does not join the sidebar at {width}x{height}\n{output}"
    );
    for x in layout.gap.x..layout.gap.right() {
        let cell = buffer.cell((x, layout.gap.y)).unwrap();
        assert_eq!(
            cell.symbol(),
            "─",
            "missing divider at ({x}, {})",
            layout.gap.y
        );
        assert_eq!(cell.fg, Palette::terminal().surface_dim);
    }

    let focused = state.with_console_body_view(
        "Embedded Cargo console",
        b"program output\n".to_vec(),
        b"> ".to_vec(),
        Some(2),
        true,
    );
    let focused_terminal = render(width, height, &focused);
    let ShellLayoutResult::Full(focused_layout) =
        shell_layout(Rect::new(0, 0, width, height), BottomPane::Console, true)
    else {
        unreachable!();
    };
    let focused_buffer = focused_terminal.backend().buffer();
    assert_eq!(
        focused_buffer
            .cell((focused_layout.sidebar_divider.x, focused_layout.gap.y))
            .unwrap()
            .symbol(),
        "├",
        "focused divider does not join the sidebar at {width}x{height}"
    );
    for x in focused_layout.gap.x..focused_layout.gap.right() {
        let cell = focused_buffer.cell((x, focused_layout.gap.y)).unwrap();
        assert_eq!(
            cell.symbol(),
            "─",
            "missing focused divider at ({x}, {})",
            focused_layout.gap.y
        );
        assert_eq!(cell.fg, Palette::terminal().accent);
    }
}

#[test]
fn student_shell_golden_at_80x24() {
    assert_student_shell_golden(80, 24);
}

#[test]
fn student_shell_golden_at_120x40() {
    assert_student_shell_golden(120, 40);
}

#[test]
fn pane_labels_are_flush_left_at_both_supported_golden_sizes() {
    for (width, height) in [(80, 24), (120, 40)] {
        let area = Rect::new(0, 0, width, height);
        let output_state = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_output_header_message("caret-local diagnostic");
        let output_terminal = render(width, height, &output_state);
        let ShellLayoutResult::Full(output_layout) = shell_layout(area, BottomPane::Output, false)
        else {
            unreachable!();
        };
        let output_label = text_in_rect(
            &output_terminal,
            Rect::new(
                output_layout.bottom.x,
                output_layout.bottom.y,
                output_layout.bottom.width,
                1,
            ),
        )[0]
        .trim_end()
        .to_owned();
        assert_eq!(output_label, "output · caret-local diagnostic");
        assert_eq!(
            output_terminal
                .backend()
                .buffer()
                .cell((output_layout.bottom.x, output_layout.bottom.y))
                .unwrap()
                .symbol(),
            "o",
        );
        assert_eq!(
            output_terminal
                .backend()
                .buffer()
                .cell((output_layout.bottom.x + 9, output_layout.bottom.y))
                .unwrap()
                .symbol(),
            "c",
            "diagnostic suffix must immediately follow the flush-left output label",
        );

        let console_state = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_console_body_view(
                "Embedded Cargo console",
                Vec::new(),
                b"> ".to_vec(),
                None,
                false,
            );
        let console_terminal = render(width, height, &console_state);
        let ShellLayoutResult::Full(console_layout) =
            shell_layout(area, BottomPane::Console, false)
        else {
            unreachable!();
        };
        let console_label = text_in_rect(
            &console_terminal,
            Rect::new(
                console_layout.bottom.x,
                console_layout.bottom.y,
                console_layout.bottom.width,
                1,
            ),
        )[0]
        .trim_end()
        .to_owned();
        assert_eq!(console_label, "console");
        assert_eq!(
            console_terminal
                .backend()
                .buffer()
                .cell((console_layout.bottom.x, console_layout.bottom.y))
                .unwrap()
                .symbol(),
            "c",
        );
    }
}

fn find_text(terminal: &Terminal<TestBackend>, needle: &str) -> (u16, u16) {
    rendered_lines(terminal)
        .iter()
        .enumerate()
        .find_map(|(y, line)| line.find(needle).map(|x| (x as u16, y as u16)))
        .unwrap_or_else(|| panic!("expected rendered output to contain {needle:?}"))
}

fn text_in_rect(terminal: &Terminal<TestBackend>, rect: Rect) -> Vec<String> {
    (rect.y..rect.bottom())
        .map(|y| {
            (rect.x..rect.right())
                .filter_map(|x| terminal.backend().buffer().cell((x, y)))
                .map(|cell| cell.symbol())
                .collect()
        })
        .collect()
}

fn find_text_in_rect(terminal: &Terminal<TestBackend>, rect: Rect, needle: &str) -> (u16, u16) {
    text_in_rect(terminal, rect)
        .iter()
        .enumerate()
        .find_map(|(row, line)| {
            line.find(needle)
                .map(|column| (rect.x + column as u16, rect.y + row as u16))
        })
        .unwrap_or_else(|| panic!("expected {rect:?} to contain {needle:?}"))
}

fn rendered_toast_body(terminal: &Terminal<TestBackend>, title: &str) -> String {
    let (title_x, top) = rendered_lines(terminal)
        .iter()
        .enumerate()
        .find_map(|(y, line)| {
            line.find(title)
                .map(|byte| (line[..byte].chars().count() as u16, y as u16))
        })
        .unwrap_or_else(|| panic!("expected rendered output to contain {title:?}"));
    let left = title_x.saturating_sub(2);
    let bottom = (top + 1..terminal.backend().buffer().area.bottom())
        .find(|&y| {
            terminal
                .backend()
                .buffer()
                .cell((left, y))
                .is_some_and(|cell| cell.symbol() == "└")
        })
        .unwrap_or_else(|| panic!("expected toast {title:?} to have a bottom border"));
    let inner = Rect::new(
        left + 2,
        top + 1,
        terminal
            .backend()
            .buffer()
            .area
            .right()
            .saturating_sub(left + 4),
        bottom.saturating_sub(top + 1),
    );

    text_in_rect(terminal, inner)
        .into_iter()
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_bounded_by(rect: Rect, bounds: Rect) -> bool {
    rect.x >= bounds.x
        && rect.y >= bounds.y
        && rect.right() <= bounds.right()
        && rect.bottom() <= bounds.bottom()
}

fn overlaps(left: Rect, right: Rect) -> bool {
    left.x < right.right()
        && right.x < left.right()
        && left.y < right.bottom()
        && right.y < left.bottom()
}

#[test]
fn full_layout_is_deterministic_bounded_and_non_overlapping() {
    for area in [
        Rect::new(0, 0, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT),
        Rect::new(0, 0, 80, 24),
        Rect::new(7, 3, 120, 40),
    ] {
        let first = main_layout(area);
        let second = main_layout(area);
        assert_eq!(first, second);

        let MainLayout::Full(panes) = first else {
            panic!("supported size {area:?} unexpectedly used the fallback");
        };
        let all = [
            panes.sidebar,
            panes.tab_bar,
            panes.editor,
            panes.gap,
            panes.bottom,
            panes.mode_bar,
        ];

        assert!(all.iter().all(|pane| pane.width > 0 && pane.height > 0));
        assert!(all.iter().all(|pane| is_bounded_by(*pane, area)));
        for (index, left) in all.iter().enumerate() {
            for right in &all[index + 1..] {
                assert!(!overlaps(*left, *right), "{left:?} overlaps {right:?}");
            }
        }

        assert_eq!(panes.sidebar.height, area.height);
        assert_eq!(panes.sidebar.right(), panes.tab_bar.x);
        assert_eq!(panes.tab_bar.bottom(), panes.editor.y);
        assert_eq!(panes.editor.bottom(), panes.gap.y);
        assert_eq!(panes.gap.bottom(), panes.bottom.y);
        assert_eq!(panes.bottom.bottom(), panes.mode_bar.y);
        assert_eq!(panes.mode_bar.bottom(), area.bottom());
        assert_eq!(panes.sidebar_divider.x, panes.sidebar.right() - 1);
    }
}

#[test]
fn eighty_by_twenty_four_body_view_is_full_width_safe_and_marked() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy).with_console_body_view(
        "Embedded Cargo console",
        b"program output\n\x1b]52;c;blocked\x07\ncaf\xc3\xa9\n".to_vec(),
        b"stdin> ".to_vec(),
        Some(7),
        true,
    );
    let output = rendered_lines(&render(80, 24, &state)).join("\n");
    assert!(output.contains("console"));
    assert!(output.contains(" CONSOLE "));
    assert!(output.contains("program output"));
    assert!(output.contains("café"));
    assert!(output.contains("stdin> ▏"));
    assert!(output.contains("esc close  ↵ run/send"));
    assert!(!output.contains("eof"));
    assert!(!output.contains("cancel"));
    assert!(output.contains("fn main"));
    assert!(output.contains("src/parser.rs"));
    assert!(!output.contains('\u{1b}'));
    assert!(output.contains("\\u{1b}]52;c;blocked\\u{7}"));
}

#[test]
fn recovery_reason_overrides_healthy_status_at_normal_and_constrained_sizes() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_recovery_reason(Some("journal authority changed"));
    for (width, height) in [(100, 24), (60, 15), (40, 10)] {
        let output = rendered_lines(&render(width, height, &state)).join("\n");
        assert!(output.contains("ERROR"), "{width}x{height}: {output}");
        assert!(
            output.contains("journal authority changed"),
            "{width}x{height}: {output}"
        );
        assert!(!output.contains("Recording:"));
        assert!(!output.contains("Journal:"));
    }
}

#[test]
fn recovery_reason_is_terminal_safe_and_does_not_reset_on_later_status_updates() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_recovery_reason(Some("journal\u{1b}[31m\nchanged"))
        .with_recovery_reason(None);
    let output = rendered_lines(&render(100, 24, &state)).join("\n");
    assert!(output.contains("ERROR"));
    assert!(output.contains("journal\\u{1b}[31m · changed"));
    assert!(!output.contains(r"\n"));
    assert!(!output.contains('\u{1b}'));
}

#[test]
fn external_reconciliation_has_a_persistent_error_and_a_dismissible_toast() {
    let external = "External changes are not accepted; restored canonical bytes";
    let initial = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_error_condition(external)
        .with_toast(ToastState::new(
            ToastKind::Error,
            "recovery required",
            external,
        ));
    let initial_output = rendered_lines(&render(100, 24, &initial)).join("\n");
    assert!(initial_output.contains(" ERROR "), "{initial_output}");
    assert!(
        initial_output.contains("● recovery required"),
        "{initial_output}"
    );
    assert!(
        initial_output.matches(external).count() >= 2,
        "{initial_output}"
    );

    let dismissed =
        view_state(RecordingState::Active, JournalHealth::Healthy).with_error_condition(external);
    let dismissed_output = rendered_lines(&render(100, 24, &dismissed)).join("\n");
    assert!(dismissed_output.contains(" ERROR "), "{dismissed_output}");
    assert!(dismissed_output.contains(external), "{dismissed_output}");
    assert!(!dismissed_output.contains("● recovery required"));
}

#[test]
fn active_prompt_keeps_its_mode_pill_alongside_the_persistent_error_pill() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_error_condition("External changes are not accepted")
        .with_mode_bar(ModeBarState::new(ModeBarKind::Menu, "esc close  ↵ run"));
    let terminal = render(80, 24, &state);
    let mode_row = rendered_lines(&terminal)[23].clone();

    assert!(mode_row.contains(" MENU "), "{mode_row}");
    assert!(mode_row.contains("esc close  ↵ run"), "{mode_row}");
    assert!(mode_row.contains(" ERROR "), "{mode_row}");
    assert!(mode_row.contains("External"), "{mode_row}");
}

#[test]
fn console_overwrite_keeps_the_composed_confirmation_mode() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_mode_bar(ModeBarState::new(
            ModeBarKind::Confirm,
            "↵ confirm  esc cancel",
        ))
        .with_console_body_view(
            "Embedded Cargo console",
            b"existing output".to_vec(),
            b"cargo test".to_vec(),
            Some(10),
            true,
        )
        .with_confirmation(ConfirmationState::new(
            "Console output exists. Replace it with the next command?",
        ));
    let output = rendered_lines(&render(80, 24, &state)).join("\n");

    assert!(output.contains(" CONFIRM "), "{output}");
    assert!(!output.contains(" CONSOLE "), "{output}");
    assert!(output.contains(" ↵ confirm "), "{output}");
    assert!(output.contains(" esc cancel "), "{output}");
}

#[test]
fn smaller_terminals_use_a_clear_safe_fallback() {
    for (width, height) in [
        (MIN_TERMINAL_WIDTH - 1, MIN_TERMINAL_HEIGHT),
        (MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT - 1),
        (24, 5),
        (1, 1),
    ] {
        assert_eq!(
            main_layout(Rect::new(0, 0, width, height)),
            MainLayout::TooSmall(Rect::new(0, 0, width, height))
        );
        let _ = render(
            width,
            height,
            &view_state(RecordingState::Active, JournalHealth::Healthy),
        );
    }

    let terminal = render(
        24,
        5,
        &view_state(RecordingState::Active, JournalHealth::Healthy),
    );
    let output = rendered_lines(&terminal).join("\n");
    assert!(output.contains("Terminal too small"));
    assert!(output.contains(&format!("Need {MIN_TERMINAL_WIDTH}x{MIN_TERMINAL_HEIGHT}")));
}

#[test]
fn main_view_renders_every_region_and_composes_the_editor_widget() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy);
    let terminal = render(100, 24, &state);
    let output = rendered_lines(&terminal).join("\n");

    for expected in [
        " files",
        "src/main.rs",
        "fn main()",
        "output",
        "warning: unused variable on line 8",
        "Ln 1, Col 1",
        " new",
        "menu",
    ] {
        assert!(output.contains(expected), "missing {expected:?}\n{output}");
    }
    assert!(!output.contains("Ownership and Borrowing Lab"), "{output}");

    let MainLayout::Full(panes) = main_layout(Rect::new(0, 0, 100, 24)) else {
        unreachable!();
    };
    let editor_cursor = terminal
        .backend()
        .buffer()
        .cell((panes.editor.x, panes.editor.y))
        .expect("editor content cell");
    assert_eq!(editor_cursor.symbol(), "f");
    assert!(editor_cursor.modifier.contains(Modifier::REVERSED));
}

#[test]
fn the_three_builtin_palettes_have_stable_colours_and_explicit_tints_and_tabs() {
    assert_eq!(
        Palette::catppuccin(),
        Palette {
            accent: Color::Rgb(137, 180, 250),
            panel_bg: Color::Rgb(24, 24, 37),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(30, 30, 46),
            selection_bg: Color::Rgb(49, 50, 68),
            surface0: Color::Rgb(49, 50, 68),
            surface1: Color::Rgb(69, 71, 90),
            surface_dim: Color::Rgb(30, 30, 46),
            overlay0: Color::Rgb(108, 112, 134),
            overlay1: Color::Rgb(127, 132, 156),
            text: Color::Rgb(205, 214, 244),
            subtext0: Color::Rgb(166, 173, 200),
            mauve: Color::Rgb(203, 166, 247),
            green: Color::Rgb(166, 227, 161),
            yellow: Color::Rgb(249, 226, 175),
            red: Color::Rgb(243, 139, 168),
            blue: Color::Rgb(137, 180, 250),
            teal: Color::Rgb(148, 226, 213),
            peach: Color::Rgb(250, 179, 135),
            error_bg: Color::Rgb(67, 47, 63),
            warning_bg: Color::Rgb(69, 64, 64),
            tab_active_bg: Color::Rgb(137, 180, 250),
            tab_active_fg: Color::Rgb(24, 24, 37),
        }
    );
    assert_eq!(
        Palette::catppuccin_latte(),
        Palette {
            accent: Color::Rgb(30, 102, 245),
            panel_bg: Color::Rgb(239, 241, 245),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(230, 233, 239),
            selection_bg: Color::Rgb(189, 208, 245),
            surface0: Color::Rgb(204, 208, 218),
            surface1: Color::Rgb(188, 192, 204),
            surface_dim: Color::Rgb(230, 233, 239),
            overlay0: Color::Rgb(156, 160, 176),
            overlay1: Color::Rgb(140, 143, 161),
            text: Color::Rgb(76, 79, 105),
            subtext0: Color::Rgb(108, 111, 133),
            mauve: Color::Rgb(136, 57, 239),
            green: Color::Rgb(64, 160, 43),
            yellow: Color::Rgb(223, 142, 29),
            red: Color::Rgb(210, 15, 57),
            blue: Color::Rgb(30, 102, 245),
            teal: Color::Rgb(23, 146, 153),
            peach: Color::Rgb(254, 100, 11),
            error_bg: Color::Rgb(233, 195, 207),
            warning_bg: Color::Rgb(235, 221, 201),
            tab_active_bg: Color::Rgb(30, 102, 245),
            tab_active_fg: Color::Rgb(239, 241, 245),
        }
    );
    assert_eq!(
        Palette::terminal(),
        Palette {
            accent: Color::Blue,
            panel_bg: Color::Reset,
            sidebar_bg: Color::Reset,
            active_row_bg: Color::DarkGray,
            selection_bg: Color::Reset,
            surface0: Color::Reset,
            surface1: Color::DarkGray,
            surface_dim: Color::DarkGray,
            overlay0: Color::Gray,
            overlay1: Color::White,
            text: Color::Reset,
            subtext0: Color::Gray,
            mauve: Color::Magenta,
            green: Color::Green,
            yellow: Color::Yellow,
            red: Color::LightRed,
            blue: Color::Blue,
            teal: Color::Cyan,
            peach: Color::Yellow,
            error_bg: Color::Red,
            warning_bg: Color::Yellow,
            tab_active_bg: Color::Blue,
            tab_active_fg: Color::DarkGray,
        }
    );
}

#[test]
fn every_builtin_and_a_custom_palette_reach_editor_tabs_output_tints_and_overlay() {
    let mut custom = Palette::catppuccin();
    custom.mauve = Color::Rgb(1, 2, 3);
    custom.overlay0 = Color::Rgb(4, 5, 6);
    custom.error_bg = Color::Rgb(7, 8, 9);
    custom.warning_bg = Color::Rgb(10, 11, 12);
    custom.tab_active_bg = Color::Rgb(13, 14, 15);
    custom.tab_active_fg = Color::Rgb(16, 17, 18);
    custom.accent = Color::Rgb(19, 20, 21);
    custom.panel_bg = Color::Rgb(22, 23, 24);

    for (label, palette, keyword, output, warning, error, tab_bg, tab_fg, frame, panel) in [
        (
            "catppuccin",
            Palette::catppuccin(),
            Color::Rgb(203, 166, 247),
            Color::Rgb(108, 112, 134),
            Color::Rgb(69, 64, 64),
            Color::Rgb(67, 47, 63),
            Color::Rgb(137, 180, 250),
            Color::Rgb(24, 24, 37),
            Color::Rgb(137, 180, 250),
            Color::Rgb(24, 24, 37),
        ),
        (
            "catppuccin-latte",
            Palette::catppuccin_latte(),
            Color::Rgb(136, 57, 239),
            Color::Rgb(156, 160, 176),
            Color::Rgb(235, 221, 201),
            Color::Rgb(233, 195, 207),
            Color::Rgb(30, 102, 245),
            Color::Rgb(239, 241, 245),
            Color::Rgb(30, 102, 245),
            Color::Rgb(239, 241, 245),
        ),
        (
            "terminal",
            Palette::terminal(),
            Color::Magenta,
            Color::Gray,
            Color::Yellow,
            Color::Red,
            Color::Blue,
            Color::DarkGray,
            Color::Blue,
            Color::Reset,
        ),
        (
            "custom",
            custom,
            Color::Rgb(1, 2, 3),
            Color::Rgb(4, 5, 6),
            Color::Rgb(10, 11, 12),
            Color::Rgb(7, 8, 9),
            Color::Rgb(13, 14, 15),
            Color::Rgb(16, 17, 18),
            Color::Rgb(19, 20, 21),
            Color::Rgb(22, 23, 24),
        ),
    ] {
        let source = "fn warning() {}\nfn error() {}\n";
        let editor = EditorBuffer::new(
            DocumentId::new(format!("theme-{label}")).unwrap(),
            source,
            NoopEditorEffects,
        );
        let state = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_diagnostic_markers(vec![
                DiagnosticLineMarker::new(0, DiagnosticMarkerKind::Warning, false),
                DiagnosticLineMarker::new(1, DiagnosticMarkerKind::Error, false),
            ]);
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        MainView::new(
            &state,
            &editor,
            &Viewport::default(),
            &[HighlightSpan {
                byte_range: 0..2,
                kind: HighlightKind::Keyword,
            }],
        )
        .with_palette(palette.clone())
        .render_with_hit_map(area, &mut buffer);
        let MainLayout::Full(layout) = main_layout(area) else {
            unreachable!();
        };
        assert_eq!(
            buffer[(layout.editor.x, layout.editor.y)].fg,
            keyword,
            "{label}"
        );
        assert_eq!(
            buffer[(layout.editor.x, layout.editor.y)].bg,
            warning,
            "{label}"
        );
        assert_eq!(
            buffer[(layout.editor.x, layout.editor.y + 1)].bg,
            error,
            "{label}"
        );
        assert_eq!(
            buffer[(layout.bottom.x, layout.bottom.y)].fg,
            output,
            "{label}"
        );
        let tab = buffer
            .content()
            .iter()
            .find(|cell| cell.symbol() == "s" && cell.bg == tab_bg)
            .unwrap_or_else(|| panic!("{label}: active tab background not rendered"));
        assert_eq!(tab.fg, tab_fg, "{label}");

        let overlay_state =
            view_state(RecordingState::Active, JournalHealth::Healthy).with_keybinds_overlay(0);
        let mut overlay = Buffer::empty(area);
        let hits = MainView::new(&overlay_state, &editor, &Viewport::default(), &[])
            .with_palette(palette)
            .render_with_hit_map(area, &mut overlay);
        assert_eq!(
            overlay[(hits.overlay.x, hits.overlay.y)].fg,
            frame,
            "{label}"
        );
        assert_eq!(
            overlay[(hits.overlay.x, hits.overlay.y)].bg,
            panel,
            "{label}"
        );
    }
}

#[test]
fn diagnostic_lines_start_at_column_zero_and_tint_the_full_row_in_both_themes() {
    let source = "warning_line\nerror_line\nplain_line\n";
    let editor = EditorBuffer::new(
        DocumentId::new("diagnostic-tints").unwrap(),
        source,
        NoopEditorEffects,
    );
    let state =
        view_state(RecordingState::Active, JournalHealth::Healthy).with_diagnostic_markers(vec![
            DiagnosticLineMarker::new(0, DiagnosticMarkerKind::Warning, false),
            DiagnosticLineMarker::new(1, DiagnosticMarkerKind::Error, false),
        ]);

    for (width, height) in [(80, 24), (120, 40)] {
        for (palette, warning_bg, error_bg) in [
            (Palette::terminal(), Color::Yellow, Color::Red),
            (
                Palette::catppuccin(),
                Color::Rgb(69, 64, 64),
                Color::Rgb(67, 47, 63),
            ),
        ] {
            let terminal =
                render_with_editor_palette(width, height, &state, &editor, palette.clone());
            let MainLayout::Full(panes) = main_layout(Rect::new(0, 0, width, height)) else {
                unreachable!();
            };
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer[(panes.editor.x, panes.editor.y)].symbol(), "w");
            assert_eq!(buffer[(panes.editor.x, panes.editor.y + 1)].symbol(), "e");
            assert_eq!(
                buffer[(panes.editor.x, panes.editor.y)].fg,
                palette.text,
                "warning tint changed the source foreground"
            );
            for x in panes.editor.x..panes.editor.right() {
                assert_eq!(buffer[(x, panes.editor.y)].bg, warning_bg, "x={x}");
                assert_eq!(buffer[(x, panes.editor.y + 1)].bg, error_bg, "x={x}");
            }
            assert_eq!(
                buffer[(panes.editor.right() - 1, panes.editor.y + 2)].bg,
                Color::Reset
            );
        }
    }
}

#[test]
fn diagnostic_error_wins_over_warning_and_overlays_compose_in_order() {
    let source = "fn main() {}\n";
    let mut editor = EditorBuffer::new(
        DocumentId::new("diagnostic-composition").unwrap(),
        source,
        NoopEditorEffects,
    );
    let opening = source.find('{').unwrap();
    let closing = source.find('}').unwrap();
    editor
        .set_selection(SelectionState::new(0, opening as u64))
        .unwrap();
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_diagnostic_markers(vec![
            DiagnosticLineMarker::new(0, DiagnosticMarkerKind::Warning, false),
            DiagnosticLineMarker::new(0, DiagnosticMarkerKind::Error, false),
        ])
        .with_live_diagnostics(vec![LiveDiagnosticSpan {
            start_byte: closing as u64,
            end_byte: closing as u64 + 1,
            start_line: 0,
            end_line: 0,
            kind: DiagnosticMarkerKind::Warning,
            message: "live warning".to_owned(),
        }]);
    let highlights = [HighlightSpan {
        byte_range: 0..2,
        kind: HighlightKind::Keyword,
    }];
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| {
            frame.render_widget(
                MainView::new(&state, &editor, &Viewport::default(), &highlights)
                    .with_palette(Palette::terminal()),
                frame.area(),
            );
        })
        .unwrap();
    let MainLayout::Full(panes) = main_layout(Rect::new(0, 0, 80, 24)) else {
        unreachable!();
    };
    let buffer = terminal.backend().buffer();
    let selected_token = &buffer[(panes.editor.x, panes.editor.y)];
    assert_eq!(selected_token.fg, Palette::terminal().mauve);
    assert_eq!(selected_token.bg, Palette::terminal().active_row_bg);

    let matched_live = &buffer[(panes.editor.x + closing as u16, panes.editor.y)];
    assert_eq!(matched_live.bg, Color::Red);
    assert_eq!(matched_live.fg, Palette::terminal().accent);
    assert!(matched_live.modifier.contains(Modifier::BOLD));
    assert!(matched_live.modifier.contains(Modifier::UNDERLINED));
    assert_eq!(
        buffer[(panes.editor.right() - 1, panes.editor.y)].bg,
        Color::Red,
        "diagnostic tint did not cover the past-end cells"
    );
}

#[test]
fn live_diagnostic_ascii_golden_has_underline_inline_and_header() {
    let source = "fn main() {\n  let crab = 1;\n}\n";
    let editor = EditorBuffer::new(
        DocumentId::new("live-ascii").unwrap(),
        source,
        NoopEditorEffects,
    );
    let start = source.find("crab").unwrap() as u64;
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_diagnostic_markers(vec![DiagnosticLineMarker::live(
            1,
            DiagnosticMarkerKind::Error,
            false,
        )])
        .with_live_diagnostics(vec![LiveDiagnosticSpan {
            start_byte: start,
            end_byte: start + 4,
            start_line: 1,
            end_line: 1,
            kind: DiagnosticMarkerKind::Error,
            message: "unknown name".to_owned(),
        }])
        .with_output_header_message("unknown name");
    let terminal = render_with_editor(80, 24, &state, &editor);
    let MainLayout::Full(panes) = main_layout(Rect::new(0, 0, 80, 24)) else {
        unreachable!();
    };
    let row = panes.editor.y + 1;
    let source_x = panes.editor.x;

    let lines = rendered_lines(&terminal);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("output · unknown name"))
    );
    assert!(lines[usize::from(row)].contains("  let crab = 1;  unknown name"));
    for x in source_x + 6..source_x + 10 {
        let cell = terminal.backend().buffer().cell((x, row)).unwrap();
        assert!(
            cell.modifier.contains(Modifier::UNDERLINED),
            "ASCII diagnostic cell {x} was not underlined: {cell:?}"
        );
        assert_eq!(cell.fg, Color::LightRed);
    }
    let inline = terminal
        .backend()
        .buffer()
        .cell((source_x + 17, row))
        .unwrap();
    assert_eq!(inline.symbol(), "u");
    assert!(inline.modifier.contains(Modifier::DIM));
}

#[test]
fn output_header_is_plain_after_idle_and_command_and_dims_a_safe_live_message() {
    let idle = MainViewState::new(
        "Header regression",
        vec![BufferTabViewEntry::new("main.rs", true, false)],
        vec![],
        RecordingState::Active,
        JournalHealth::Healthy,
        "saved",
    );
    let idle_terminal = render(80, 24, &idle);
    let MainLayout::Full(layout) = main_layout(Rect::new(0, 0, 80, 24)) else {
        unreachable!();
    };
    let idle_row = text_in_rect(
        &idle_terminal,
        Rect::new(layout.bottom.x, layout.bottom.y, layout.bottom.width, 1),
    )[0]
    .trim_end()
    .to_owned();
    assert_eq!(idle_row, "output");

    let post_command = idle.clone().with_output_rows(vec![
        OutputRow::captured("student result", rustrace_model::OutputStream::Stdout),
        OutputRow::captured("student trace", rustrace_model::OutputStream::Stderr),
    ]);
    let post_terminal = render(80, 24, &post_command);
    let post_row = text_in_rect(
        &post_terminal,
        Rect::new(layout.bottom.x, layout.bottom.y, layout.bottom.width, 1),
    )[0]
    .trim_end()
    .to_owned();
    assert_eq!(post_row, "output");

    let live = post_command.with_output_header_message(
        "unsafe\u{1b}[2J diagnostic message that must be safely truncated at the pane edge",
    );
    let live_terminal = render(80, 24, &live);
    let live_row = text_in_rect(
        &live_terminal,
        Rect::new(layout.bottom.x, layout.bottom.y, layout.bottom.width, 1),
    )[0]
    .trim_end()
    .to_owned();
    assert!(
        live_row.starts_with(r"output · unsafe\u{1b}[2J diagnostic"),
        "{live_row:?}"
    );
    assert!(live_row.ends_with('…'), "{live_row:?}");
    assert!(!live_row.contains('\u{1b}'), "{live_row:?}");
    let message_cell = live_terminal
        .backend()
        .buffer()
        .cell((layout.bottom.x + 9, layout.bottom.y))
        .unwrap();
    assert!(message_cell.modifier.contains(Modifier::DIM));
    assert!(!message_cell.modifier.contains(Modifier::BOLD));

    let restored = render(80, 24, &idle);
    let restored_row = text_in_rect(
        &restored,
        Rect::new(layout.bottom.x, layout.bottom.y, layout.bottom.width, 1),
    )[0]
    .trim_end()
    .to_owned();
    assert_eq!(restored_row, "output");
}

#[test]
fn output_rows_trim_leading_whitespace_only_in_the_rendered_projection() {
    let stdout_evidence = b"    student result".to_vec();
    let stderr_evidence = b"\t  student trace".to_vec();
    let rows = vec![
        OutputRow::captured(
            String::from_utf8(stdout_evidence.clone()).unwrap(),
            rustrace_model::OutputStream::Stdout,
        ),
        OutputRow::captured(
            String::from_utf8(stderr_evidence.clone()).unwrap(),
            rustrace_model::OutputStream::Stderr,
        ),
        OutputRow::diagnostic("    > error[E0308] src/main.rs:1:1: mismatch", 0),
    ];

    for (width, height) in [(80, 24), (120, 40)] {
        let state = MainViewState::new(
            "Output alignment regression",
            vec![BufferTabViewEntry::new("main.rs", true, false)],
            vec![],
            RecordingState::Active,
            JournalHealth::Healthy,
            "saved",
        )
        .with_output_rows(rows.clone());
        let terminal = render(width, height, &state);
        let MainLayout::Full(layout) = main_layout(Rect::new(0, 0, width, height)) else {
            unreachable!();
        };

        for (offset, expected) in [
            (1, "student result"),
            (2, "student trace"),
            (3, "> error[E0308] src/main.rs:1:1: mismatch"),
        ] {
            assert_eq!(
                text_in_rect(
                    &terminal,
                    Rect::new(
                        layout.bottom.x,
                        layout.bottom.y + offset,
                        layout.bottom.width,
                        1,
                    ),
                )[0]
                .trim_end(),
                expected,
                "{width}x{height} row {offset} was not flush left",
            );
        }
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((layout.bottom.x, layout.bottom.y + 1))
                .unwrap()
                .fg,
            Palette::terminal().text,
        );
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((layout.bottom.x, layout.bottom.y + 2))
                .unwrap()
                .fg,
            Palette::terminal().red,
        );
    }

    assert_eq!(stdout_evidence, b"    student result");
    assert_eq!(stderr_evidence, b"\t  student trace");
    assert_eq!(rows[0].text(), "    student result");
    assert_eq!(rows[1].text(), "\t  student trace");
    assert_eq!(
        rows[2].text(),
        "    > error[E0308] src/main.rs:1:1: mismatch"
    );
}

#[test]
fn multiline_notice_uses_rows_in_output_and_middle_dots_in_the_mode_bar() {
    let notice = String::from(
        "External changes are not accepted.\nRustrace preserves recovery evidence.\nRestoring current contents, including unsaved edits.",
    );
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_output_rows(vec![OutputRow::plain(notice.clone())])
        .with_error_condition(notice.clone());
    let terminal = render(120, 40, &state);
    let MainLayout::Full(layout) = main_layout(Rect::new(0, 0, 120, 40)) else {
        unreachable!();
    };
    let rendered = rendered_lines(&terminal);
    let mode_row = &rendered[usize::from(layout.mode_bar.y)];
    assert!(
        mode_row.contains(
            "External changes are not accepted. · Rustrace preserves recovery evidence. · "
        ),
        "{mode_row:?}",
    );
    assert!(!mode_row.contains(r"\n"), "{mode_row:?}");
    for (offset, expected) in [
        (1, "External changes are not accepted."),
        (2, "Rustrace preserves recovery evidence."),
        (3, "Restoring current contents, including unsaved edits."),
    ] {
        assert_eq!(
            text_in_rect(
                &terminal,
                Rect::new(
                    layout.bottom.x,
                    layout.bottom.y + offset,
                    layout.bottom.width,
                    1,
                ),
            )[0]
            .trim_end(),
            expected,
        );
    }
    assert!(!rendered.join("\n").contains(r"\n"));
    assert_eq!(
        notice,
        "External changes are not accepted.\nRustrace preserves recovery evidence.\nRestoring current contents, including unsaved edits."
    );
}

#[test]
fn live_diagnostic_wide_golden_preserves_source_cells_and_uses_display_columns() {
    let source = "let 名称 = 東京;\n";
    let editor = EditorBuffer::new(
        DocumentId::new("live-wide").unwrap(),
        source,
        NoopEditorEffects,
    );
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_diagnostic_markers(vec![DiagnosticLineMarker::live(
            0,
            DiagnosticMarkerKind::Warning,
            false,
        )])
        .with_live_diagnostics(vec![LiveDiagnosticSpan {
            start_byte: 4,
            end_byte: 10,
            start_line: 0,
            end_line: 0,
            kind: DiagnosticMarkerKind::Warning,
            message: "wide warning".to_owned(),
        }]);
    let terminal = render_with_editor(80, 24, &state, &editor);
    let MainLayout::Full(panes) = main_layout(Rect::new(0, 0, 80, 24)) else {
        unreachable!();
    };
    let row = panes.editor.y;
    let source_x = panes.editor.x;

    let lines = rendered_lines(&terminal);
    assert!(
        lines[usize::from(row)].contains("let 名 称  = 東 京 ;  wide warning"),
        "{:?}",
        lines[usize::from(row)]
    );
    for x in [source_x + 4, source_x + 6] {
        let cell = terminal.backend().buffer().cell((x, row)).unwrap();
        assert!(
            cell.modifier.contains(Modifier::UNDERLINED),
            "wide diagnostic cell {x} was not underlined: {cell:?}"
        );
        assert_eq!(cell.fg, Color::Yellow);
    }
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((source_x + 15, row))
            .unwrap()
            .symbol(),
        ";"
    );
    let inline = terminal
        .backend()
        .buffer()
        .cell((source_x + 18, row))
        .unwrap();
    assert_eq!(inline.symbol(), "w");
    assert!(inline.modifier.contains(Modifier::DIM));
}

#[test]
fn live_diagnostic_inline_is_omitted_when_fewer_than_eight_columns_remain() {
    let area = Rect::new(0, 0, 80, 24);
    let MainLayout::Full(panes) = main_layout(area) else {
        unreachable!();
    };
    let source_width = usize::from(panes.editor.width);
    let source = format!("{}x\n", "a".repeat(source_width.saturating_sub(7)));
    let editor = EditorBuffer::new(
        DocumentId::new("live-no-inline").unwrap(),
        &source,
        NoopEditorEffects,
    );
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_diagnostic_markers(vec![DiagnosticLineMarker::live(
            0,
            DiagnosticMarkerKind::Error,
            false,
        )])
        .with_live_diagnostics(vec![LiveDiagnosticSpan {
            start_byte: 0,
            end_byte: 1,
            start_line: 0,
            end_line: 0,
            kind: DiagnosticMarkerKind::Error,
            message: "must stay hidden".to_owned(),
        }]);
    let terminal = render_with_editor(80, 24, &state, &editor);
    let row = &rendered_lines(&terminal)[usize::from(panes.editor.y)];
    assert!(!row.contains("must stay hidden"));
    assert!(row.contains(&"a".repeat(source_width.saturating_sub(7))));
}

#[test]
fn diagnostic_navigation_keeps_far_right_selection_and_caret_visible_at_80_by_24() {
    let area = Rect::new(0, 0, 80, 24);
    let MainLayout::Full(panes) = main_layout(area) else {
        unreachable!();
    };
    let editor_inner_width = panes.editor.width as usize;
    let source_width = editor_inner_width;
    let text = "x".repeat(editor_inner_width.saturating_sub(1));
    let mut editor = EditorBuffer::new(
        DocumentId::new("tint-far-right").unwrap(),
        &text,
        NoopEditorEffects,
    );
    editor
        .set_selection(SelectionState::new(
            (text.len() - 1) as u64,
            text.len() as u64,
        ))
        .unwrap();
    let mut viewport = Viewport::default();
    viewport.follow_cursor(&editor, source_width, panes.editor.height as usize);
    let state =
        view_state(RecordingState::Active, JournalHealth::Healthy).with_diagnostic_markers(vec![
            DiagnosticLineMarker::new(0, DiagnosticMarkerKind::Error, true),
        ]);
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            frame.render_widget(
                MainView::new(&state, &editor, &viewport, &[]).with_palette(Palette::terminal()),
                frame.area(),
            );
        })
        .unwrap();

    let source_start = panes.editor.x;
    let source_end = panes.editor.right();
    let row = panes.editor.y;
    let visible = (source_start..source_end)
        .filter_map(|x| terminal.backend().buffer().cell((x, row)))
        .collect::<Vec<_>>();
    assert!(
        visible
            .iter()
            .any(|cell| cell.modifier.contains(Modifier::REVERSED)),
        "navigated caret is outside the full-width source area"
    );
    assert!(
        visible.iter().any(|cell| cell.bg == Color::DarkGray),
        "navigated far-right selection is outside the source area"
    );
}

#[test]
fn keybinds_overlay_inventory_is_complete_and_fully_rendered() {
    let expected = [
        "EDITOR",
        "{line-navigation}",
        "{document-navigation}",
        "{word-navigation}",
        "{navigation-deletions}",
        "{navigation-selection}",
        "Ctrl-/                 toggle line comments",
        "{select-all}",
        "Ctrl-C / Ctrl-X        copy / cut",
        "Ctrl-V                 internal paste",
        "Ctrl-Z / Ctrl-Y        undo / redo",
        "Typing pause / Ctrl-Space  open completion",
        "Ctrl-F / F3            find panel / next match",
        "Ctrl-S                 save all, then Check",
        "Alt-Up / Alt-Down      Cargo or live diagnostic previous / next",
        "FILES",
        "Right-click file        file menu",
        "TABS",
        "Ctrl-Tab / Ctrl-BackTab  next / previous buffer",
        CTRL_W_DELETE_SELECTED_HELP_ENTRY,
        "F5 / F6                previous / next buffer",
        "COMMANDS",
        "F1                     keybinds",
        "F4                     test cases",
        "F7                     menu",
        "F8                     reload language service",
        "F9                     console",
        "Esc                    stop active Cargo command",
        "MENU",
        "Update dependencies    update Cargo dependencies",
        "Update Rustrace        cached version information",
        "Automatic checks: On/Off  persist preference for next launch",
        "Left / Up              previous entry",
        "Right / Down           next entry",
        "Enter                  run or open entry",
        "Esc                    close menu",
        "KEYBINDS",
        "Up / Down              scroll one row",
        "PageUp / PageDown      scroll ten rows",
        "Esc / Enter / F1       dismiss",
        "COMPLETION",
        "Up / Down              select completion",
        "Tab / Enter            accept completion",
        "Esc                    close completion",
        "PROMPTS",
        "Typing / Backspace     edit prompt",
        "Enter / Esc            submit / cancel",
        "CONFIRMATIONS",
        "Y / Enter              confirm",
        "N / Esc                cancel",
        "CONSOLE",
        "Typing                 edit command or stdin",
        "Left / Right           move line cursor",
        "Home / End             line start / end",
        "Backspace / Delete     edit line",
        "Enter                  run command / send stdin",
        "PgUp / PgDn / wheel    scroll output",
        "Ctrl-C                 stop running command",
        "Esc                    cancel command / close console",
        "TEST CASES",
        "Up / Down              select case",
        "Enter                  run selected case",
        "R                      refresh cases",
        "Esc                    close test cases",
        "SESSION",
        "Ctrl-Q                 quit",
    ];
    assert_eq!(
        KEYBIND_ROWS
            .iter()
            .find(|row| row.starts_with("Update Rustrace "))
            .unwrap()
            .find("cached version information"),
        Some(23)
    );
    assert_eq!(KEYBIND_ROWS.as_slice(), expected.as_slice());
    assert_eq!(
        OBVIOUS_EDITOR_KEYBIND_ROWS,
        [
            "Typing / Enter         insert text / newline",
            "Arrows                 move caret",
            "Shift+movement         select",
            "Home / End             line start / end",
            "PageUp / PageDown      move one page",
            "Backspace / Delete     delete",
            "Tab / Shift-Tab        indent / outdent",
        ]
    );

    let reachable = [
        OBVIOUS_EDITOR_KEYBIND_ROWS.as_slice(),
        KEYBIND_ROWS.as_slice(),
    ]
    .concat();
    assert_eq!(reachable.len(), 73);
    let editor_rows = &KEYBIND_ROWS[..KEYBIND_ROWS
        .iter()
        .position(|row| *row == "FILES")
        .expect("FILES follows the curated editor group")];
    for omitted in OBVIOUS_EDITOR_KEYBIND_ROWS {
        assert!(!editor_rows.contains(&omitted));
    }

    let output = (0..KEYBIND_ROWS.len())
        .into_iter()
        .map(|scroll| {
            let state = MainViewState::new(
                "Ownership and Borrowing Lab",
                vec![
                    BufferTabViewEntry::new("src/main.rs", true, false),
                    BufferTabViewEntry::new("src/parser.rs", false, false),
                ],
                vec![format!(
                    "ready\n{}",
                    primary_modifier_text(EDITOR_KEY_HINTS, PrimaryModifier::Control)
                )],
                RecordingState::Active,
                JournalHealth::Healthy,
                "saved",
            )
            .with_keybinds_overlay(scroll);
            rendered_lines(&render(100, 24, &state)).join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n");

    for row in expected {
        let rendered =
            primary_modifier_text(row, PrimaryModifier::Control).replace("Ctrl-", "Control-");
        let mut columns = rendered.splitn(2, "  ");
        let binding = columns.next().unwrap_or(&rendered).trim_end();
        let description = columns.next().unwrap_or("").trim_start();
        assert!(
            output.contains(binding),
            "missing rendered binding {binding:?}\n{output}"
        );
        assert!(
            output.contains(description),
            "missing rendered binding description {description:?}\n{output}"
        );
    }
    for phase_three_state in ["Ln 1, Col 1", "main.rs"] {
        assert!(
            output.contains(phase_three_state),
            "missing Phase 3.1 state {phase_three_state:?}\n{output}"
        );
    }
}

#[test]
fn keybinds_overlay_rows_are_never_ellipsised() {
    for primary_modifier in [PrimaryModifier::Control, PrimaryModifier::Command] {
        for (width, height) in [(80, 24), (120, 40)] {
            let mut rendered_at_all_scroll_positions = String::new();
            for scroll in 0..KEYBIND_ROWS.len() {
                let state = view_state(RecordingState::Active, JournalHealth::Healthy)
                    .with_primary_modifier(primary_modifier)
                    .with_keybinds_overlay(scroll);
                let area = Rect::new(0, 0, width, height);
                let mut buffer = Buffer::empty(area);
                let hits = MainView::new(&state, &editor(), &Viewport::default(), &[])
                    .with_palette(Palette::terminal())
                    .render_with_hit_map(area, &mut buffer);
                let body = Rect::new(
                    hits.overlay.x + 1,
                    hits.overlay.y + 3,
                    hits.overlay.width.saturating_sub(2),
                    hits.overlay.height.saturating_sub(5),
                );

                for y in body.y..body.bottom() {
                    let row = (body.x..body.right())
                        .filter_map(|x| buffer.cell((x, y)))
                        .map(|cell| cell.symbol())
                        .collect::<String>();
                    assert!(
                        !row.contains('…'),
                        "{primary_modifier:?} overlay row ellipsised at {width}x{height}, \
                         scroll {scroll}: {row:?}"
                    );
                    rendered_at_all_scroll_positions.push_str(&row);
                    rendered_at_all_scroll_positions.push('\n');
                }
            }

            for source_row in &KEYBIND_ROWS {
                let rendered = primary_modifier_text(source_row, primary_modifier)
                    .replace("Ctrl-", "Control-");
                let (binding, description) = rendered
                    .split_once("  ")
                    .map_or((rendered.as_str(), ""), |(binding, description)| {
                        (binding.trim_end(), description.trim_start())
                    });
                assert!(
                    rendered_at_all_scroll_positions.contains(binding),
                    "missing complete {primary_modifier:?} binding {binding:?} at \
                     {width}x{height}"
                );
                assert!(
                    rendered_at_all_scroll_positions.contains(description),
                    "missing complete {primary_modifier:?} description {description:?} at \
                     {width}x{height}"
                );
            }
        }
    }
}

#[test]
fn keybinds_overlay_golden_at_80x24_has_herdr_frame_scrollbar_footer_and_close_pill() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_primary_modifier(PrimaryModifier::Command)
        .with_keybinds_overlay(0);
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    let hits = MainView::new(&state, &editor(), &Viewport::default(), &[])
        .with_palette(Palette::terminal())
        .render_with_hit_map(area, &mut buffer);
    let output = buffer_lines(&buffer).join("\n");

    assert_eq!(hits.overlay, Rect::new(2, 1, 76, 22));
    assert!(!hits.overlay_cancel.is_empty());
    assert!(output.contains("keybinds"), "{output}");
    assert!(output.contains(" esc close "), "{output}");
    assert!(
        output.contains("scroll ↑↓/pgup/pgdn · close esc/enter"),
        "{output}"
    );
    assert!(output.contains(" KEYBINDS "), "{output}");
    assert!(output.contains("⌘/"), "{output}");
    assert!(output.contains('▐'), "{output}");
    assert!(!hits.keybinds_scrollbar_track.is_empty());
    assert!(!hits.keybinds_scrollbar_thumb.is_empty());
    assert!(!output.contains("/ press / to filter"), "{output}");

    let close = hits.overlay_cancel;
    let mut reducer = MouseState::default();
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Down(MouseButton::Left), close.x, close.y),
            10,
            &hits,
            ShellState {
                modal: ShellModal::Keybinds,
            },
        ),
        Some(ShellInput::CloseKeybinds)
    );
}

#[test]
fn keybinds_overlay_spells_control_in_full_in_both_modifier_modes() {
    for modifier in [PrimaryModifier::Control, PrimaryModifier::Command] {
        let output = (0..KEYBIND_ROWS.len())
            .map(|scroll| {
                let state = view_state(RecordingState::Active, JournalHealth::Healthy)
                    .with_primary_modifier(modifier)
                    .with_keybinds_overlay(scroll);
                rendered_lines(&render(80, 24, &state)).join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(output.contains("Control-Q"), "{modifier:?}: {output}");
        assert!(output.contains("Control-A"), "{modifier:?}: {output}");
        if modifier == PrimaryModifier::Control {
            assert!(
                output.contains("Control-Home / Control-End"),
                "{modifier:?}: {output}"
            );
        }
        assert!(!output.contains("ctrl-"), "{modifier:?}: {output}");
        assert!(!output.contains("Ctrl-"), "{modifier:?}: {output}");
    }
}

#[test]
fn keybinds_overlay_golden_at_80x24_omits_scrollbar_when_rows_fit() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_primary_modifier(PrimaryModifier::Command)
        .with_keybind_rows(vec!["EDITOR", "Ctrl-S                 save"])
        .with_keybinds_overlay(0);
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    let hits = MainView::new(&state, &editor(), &Viewport::default(), &[])
        .with_palette(Palette::terminal())
        .render_with_hit_map(area, &mut buffer);
    let output = buffer_lines(&buffer).join("\n");

    assert_eq!(hits.overlay, Rect::new(2, 1, 76, 22));
    assert!(hits.keybinds_scrollbar_track.is_empty());
    assert!(hits.keybinds_scrollbar_thumb.is_empty());
    assert!(output.contains("⌘S"), "{output}");
    assert!(output.contains(" esc close "), "{output}");
    assert!(
        output.contains("scroll ↑↓/pgup/pgdn · close esc/enter"),
        "{output}"
    );
}

#[test]
fn new_file_panel_renders_a_fixed_dim_src_prefix_before_an_empty_field() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_file_prompt(FilePromptState::new(FilePromptKind::Create, ""));
    let terminal = render(80, 24, &state);
    let lines = rendered_lines(&terminal);
    assert!(
        lines.iter().any(|line| line.contains("src/▏")),
        "new-file field must show its fixed src/ label"
    );
    let (prefix_x, row) = find_text_in_rect(&terminal, Rect::new(13, 9, 54, 5), "src/▏");
    let caret_x = prefix_x + 4;

    assert_eq!(
        caret_x,
        prefix_x + 4,
        "the editable remainder did not start empty"
    );
    for x in prefix_x..caret_x {
        let cell = terminal.backend().buffer().cell((x, row)).unwrap();
        assert!(cell.modifier.contains(Modifier::DIM), "{cell:?}");
    }
    assert!(
        !terminal
            .backend()
            .buffer()
            .cell((caret_x, row))
            .unwrap()
            .modifier
            .contains(Modifier::DIM)
    );
}

#[test]
fn primary_modifier_helper_drives_overlay_mode_bar_and_toast_rendering() {
    assert_eq!(
        primary_modifier_text("Ctrl-S / Ctrl-Q", PrimaryModifier::Control),
        "Ctrl-S / Ctrl-Q"
    );
    assert_eq!(
        primary_modifier_text("Ctrl-S / Ctrl-Q", PrimaryModifier::Command),
        "⌘S / Ctrl-Q"
    );
    assert_eq!(
        primary_modifier_text(
            "Ctrl-Q Ctrl-W Ctrl-A Ctrl-C Ctrl-X Ctrl-V",
            PrimaryModifier::Command,
        ),
        "Ctrl-Q Ctrl-W Ctrl-A Ctrl-C Ctrl-X Ctrl-V",
    );
    assert_eq!(
        primary_modifier_text("Ctrl-S Ctrl-F Ctrl-Z Ctrl-Y", PrimaryModifier::Command),
        "⌘S ⌘F ⌘Z ⌘Y",
    );
    assert_eq!(
        primary_modifier_text("Ctrl-Space Ctrl-Tab Ctrl-BackTab", PrimaryModifier::Command,),
        "Ctrl-Space Ctrl-Tab Ctrl-BackTab",
    );
    assert_eq!(
        primary_modifier_text("Alt-Up / alt-Down", PrimaryModifier::Control),
        "alt-Up / alt-Down"
    );
    assert_eq!(
        primary_modifier_text("Alt-Up / alt-Down", PrimaryModifier::Command),
        "Option-Up / Option-Down"
    );

    let navigation_rows = [
        (
            "{line-navigation}",
            "⌘Left / ⌘Right       line start / end",
            "Home / End             line start / end",
        ),
        (
            "{document-navigation}",
            "⌘Up / ⌘Down           document start / end",
            "Ctrl-Home / Ctrl-End   document start / end",
        ),
        (
            "{word-navigation}",
            "Option-Left / Option-Right  previous / next word",
            "Ctrl-Left / Ctrl-Right  previous / next word",
        ),
        (
            "{navigation-deletions}",
            "Option-Backspace / ⌘Backspace  word / line-start delete",
            "Ctrl-Backspace         delete previous word",
        ),
        (
            "{navigation-selection}",
            "Shift + navigation     extend selection",
            "Shift + navigation     extend selection",
        ),
    ];
    for (row, command, control) in navigation_rows {
        assert_eq!(
            primary_modifier_text(row, PrimaryModifier::Command),
            command
        );
        assert_eq!(
            primary_modifier_text(row, PrimaryModifier::Control),
            control
        );
    }

    for modifier in [PrimaryModifier::Control, PrimaryModifier::Command] {
        let state = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_primary_modifier(modifier)
            .with_mode_bar(ModeBarState::new(
                ModeBarKind::Confirm,
                "Ctrl-Q Ctrl-W Ctrl-A Ctrl-C Ctrl-X Ctrl-V",
            ))
            .with_toast(ToastState::new(
                ToastKind::Info,
                "notice",
                "Ctrl-Q Ctrl-W Ctrl-A Ctrl-C Ctrl-X Ctrl-V",
            ));
        let output = rendered_lines(&render(80, 24, &state)).join("\n");
        for chord in ["Ctrl-Q", "Ctrl-W", "Ctrl-A", "Ctrl-C", "Ctrl-X", "Ctrl-V"] {
            assert!(
                output.contains(chord),
                "{modifier:?}: missing {chord}: {output}"
            );
        }
        for chord in ["⌘Q", "⌘W", "⌘A", "⌘C", "⌘X", "⌘V"] {
            assert!(
                !output.contains(chord),
                "{modifier:?}: advertised {chord}: {output}"
            );
        }
    }
}

#[test]
fn mode_bar_and_toasts_spell_ctrl_in_both_modifier_modes() {
    for modifier in [PrimaryModifier::Control, PrimaryModifier::Command] {
        let state = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_primary_modifier(modifier)
            .with_ghostty_key_bindings(GhosttyKeyBindings::defaults())
            .with_mode_bar(ModeBarState::new(
                ModeBarKind::Confirm,
                "Ctrl-Q Ctrl-A Ctrl-Home / Ctrl-End",
            ))
            .with_toast(ToastState::new(
                ToastKind::Info,
                "notice",
                "Ctrl-Q Ctrl-A Ctrl-Home / Ctrl-End",
            ));
        let output = rendered_lines(&render(120, 40, &state)).join("\n");
        for chord in ["Ctrl-Q", "Ctrl-A", "Ctrl-Home", "Ctrl-End"] {
            assert!(
                output.matches(chord).count() >= 2,
                "{modifier:?}: missing mode-bar/toast {chord:?}: {output}"
            );
        }
        assert!(!output.contains("ctrl-"), "{modifier:?}: {output}");

        let paste = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_primary_modifier(modifier)
            .with_toast(ToastState::new(
                ToastKind::Warning,
                "paste blocked",
                PASTE_BLOCKED_WARNING,
            ));
        let paste_output = rendered_lines(&render(120, 40, &paste)).join("\n");
        for chord in ["Ctrl-C", "Ctrl-X", "Ctrl-V"] {
            assert!(
                paste_output.contains(chord),
                "{modifier:?}: paste toast missing {chord:?}: {paste_output}"
            );
        }
        assert!(
            !paste_output.contains("ctrl-"),
            "{modifier:?}: {paste_output}"
        );
    }
}

#[test]
fn ghostty_command_hints_follow_defaults_applied_snippet_and_missing_cli_states() {
    let defaults = GhosttyKeyBindings::from_list_output(
        r#"keybind = super+arrow_up=jump_to_prompt:-1
keybind = super+arrow_down=jump_to_prompt:1
keybind = super+arrow_left=text:\x01
keybind = super+arrow_right=text:\x05
keybind = super+backspace=text:\x15
keybind = alt+arrow_left=esc:b
keybind = alt+arrow_right=esc:f
keybind = super+f=start_search
keybind = super+z=undo
keybind = super+k=clear_screen
keybind = super+home=scroll_to_top
keybind = super+end=scroll_to_bottom
"#,
    );
    let applied = GhosttyKeyBindings::from_list_output(
        r#"keybind = super+arrow_up=unbind
keybind = super+arrow_down=unbind
keybind = super+arrow_left=unbind
keybind = super+arrow_right=unbind
keybind = super+backspace=unbind
keybind = alt+arrow_left=unbind
keybind = alt+arrow_right=unbind
keybind = super+f=unbind
keybind = super+z=unbind
keybind = super+k=unbind
keybind = super+home=unbind
keybind = super+end=unbind
"#,
    );
    let missing_cli = GhosttyKeyBindings::defaults();
    let no_report = GhosttyKeyBindings::from_list_output("");
    let shortcut_rows = "Ctrl-F                 find panel\nCtrl-Z                 undo\nCtrl-K                 delete to line end\nCtrl-S                 save\nCtrl-Y                 redo";

    assert_eq!(
        primary_modifier_text_with_ghostty(
            "{document-navigation}\n{line-navigation}\n{word-navigation}\n{navigation-deletions}",
            PrimaryModifier::Command,
            &defaults,
        ),
        "Ctrl-Home / Ctrl-End   document start / end\n⌘Left / ⌘Right       line start / end\nOption-Left / Option-Right  previous / next word\nOption-Backspace / ⌘Backspace  word / line-start delete"
    );
    for bindings in [&missing_cli, &no_report] {
        assert_eq!(
            primary_modifier_text_with_ghostty(
                "{document-navigation}\n{line-navigation}\n{word-navigation}\n{navigation-deletions}",
                PrimaryModifier::Command,
                bindings,
            ),
            "Ctrl-Home / Ctrl-End   document start / end\nHome / End             line start / end\nCtrl-Left / Ctrl-Right  previous / next word\nOption-Backspace       delete previous word"
        );
        assert_eq!(
            primary_modifier_text_with_ghostty(shortcut_rows, PrimaryModifier::Command, bindings,),
            "Ctrl-F                 find panel\nCtrl-Z                 undo\n⌘S                 save\n⌘Y                 redo"
        );
    }

    assert_eq!(
        primary_modifier_text_with_ghostty(
            &format!("{{document-navigation}}\n{shortcut_rows}"),
            PrimaryModifier::Command,
            &applied,
        ),
        "⌘Up / ⌘Down           document start / end\n⌘F                 find panel\n⌘Z                 undo\n⌘K                 delete to line end\n⌘S                 save\n⌘Y                 redo"
    );

    for (bindings, expected, forbidden, command_k_available) in [
        (
            &defaults,
            ["Control-F", "Control-Z", "Control-Home"],
            ["⌘F", "⌘Z", "Control-K"],
            false,
        ),
        (
            &missing_cli,
            ["Control-F", "Control-Z", "Control-Home"],
            ["⌘F", "⌘Z", "Control-K"],
            false,
        ),
        (
            &applied,
            ["⌘F", "⌘Z", "⌘K"],
            ["Control-F", "Control-Z", "Control-K"],
            true,
        ),
    ] {
        let state = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_primary_modifier(PrimaryModifier::Command)
            .with_ghostty_key_bindings(*bindings)
            .with_keybind_rows(vec![
                "EDITOR",
                "{document-navigation}",
                "Ctrl-F                 find panel",
                "Ctrl-Z                 undo",
                "Ctrl-K                 delete to line end",
            ])
            .with_keybinds_overlay(0);
        let output = rendered_lines(&render(80, 24, &state)).join("\n");
        for chord in expected {
            assert!(output.contains(chord), "missing {chord:?} in:\n{output}");
        }
        for chord in forbidden {
            assert!(
                !output.contains(chord),
                "advertised {chord:?} in:\n{output}"
            );
        }
        if !command_k_available {
            assert!(!output.contains("⌘K"), "advertised ⌘K in:\n{output}");
        }
    }
}

#[test]
fn select_all_hints_follow_the_observed_ctrl_a_rewrite_on_every_surface() {
    let exact_rewrite = GhosttyKeyBindings::from_list_output(
        r#"keybind = super+arrow_left=text:\x01
"#,
    );
    let passed = GhosttyKeyBindings::passed();
    let unavailable = GhosttyKeyBindings::defaults();

    assert_eq!(
        primary_modifier_text_with_ghostty(
            "{select-all}",
            PrimaryModifier::Command,
            &exact_rewrite,
        ),
        "right-click → Select all",
    );
    for bindings in [&passed, &unavailable] {
        assert_eq!(
            primary_modifier_text_with_ghostty("{select-all}", PrimaryModifier::Command, bindings,),
            "Ctrl-A                 select all",
        );
    }

    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_primary_modifier(PrimaryModifier::Command)
        .with_ghostty_key_bindings(exact_rewrite)
        .with_mode_bar(ModeBarState::new(ModeBarKind::Confirm, "Ctrl-A select all"))
        .with_toast(ToastState::new(
            ToastKind::Info,
            "selection",
            "Ctrl-A select all",
        ));
    let output = rendered_lines(&render(120, 40, &state)).join("\n");
    assert!(!output.contains("Ctrl-A"), "{output}");
    assert!(!output.contains("⌘A"), "{output}");
    assert!(output.contains("right-click → Select all"), "{output}");
}

#[test]
fn ghostty_action_hints_require_bindings_to_pass() {
    let rewritten = GhosttyKeyBindings::from_list_output(
        r#"keybind = super+arrow_up=text:x
keybind = super+arrow_down=text:x
keybind = super+f=text:x
keybind = super+z=text:x
keybind = super+k=text:x
keybind = super+home=text:x
keybind = super+end=text:x
"#,
    );
    let passed = GhosttyKeyBindings::passed();
    let source = "{document-navigation}\nCtrl-F  find\nCtrl-Z  undo\nCtrl-K  delete to line end\nCtrl-Home  document start\nCtrl-End  document end";

    assert_eq!(
        primary_modifier_text_with_ghostty(source, PrimaryModifier::Command, &rewritten),
        "Ctrl-Home / Ctrl-End   document start / end\nCtrl-F  find\nCtrl-Z  undo\nCtrl-Home  document start\nCtrl-End  document end"
    );
    assert_eq!(
        primary_modifier_text_with_ghostty(source, PrimaryModifier::Command, &passed),
        "⌘Up / ⌘Down           document start / end\n⌘F  find\n⌘Z  undo\n⌘K  delete to line end\n⌘Home  document start\n⌘End  document end"
    );
}

#[test]
fn ghostty_navigation_hints_accept_only_exact_translations_or_passed_bindings() {
    let arbitrary_rewrites = GhosttyKeyBindings::from_list_output(
        r#"keybind = super+arrow_left=text:x
keybind = super+arrow_right=text:x
keybind = super+backspace=text:x
keybind = alt+arrow_left=esc:x
keybind = alt+arrow_right=esc:x
"#,
    );
    let exact_translations = GhosttyKeyBindings::from_list_output(
        r#"keybind = super+arrow_left=text:\x01
keybind = super+arrow_right=text:\x05
keybind = super+backspace=text:\x15
keybind = alt+arrow_left=esc:b
keybind = alt+arrow_right=esc:f
"#,
    );
    let passed = GhosttyKeyBindings::passed();
    let source = "{line-navigation}\n{word-navigation}\n{navigation-deletions}";
    let command_forms = "⌘Left / ⌘Right       line start / end\nOption-Left / Option-Right  previous / next word\nOption-Backspace / ⌘Backspace  word / line-start delete";

    assert_eq!(
        primary_modifier_text_with_ghostty(source, PrimaryModifier::Command, &arbitrary_rewrites),
        "Home / End             line start / end\nCtrl-Left / Ctrl-Right  previous / next word\nOption-Backspace       delete previous word"
    );
    assert_eq!(
        primary_modifier_text_with_ghostty(source, PrimaryModifier::Command, &exact_translations),
        command_forms
    );
    assert_eq!(
        primary_modifier_text_with_ghostty(source, PrimaryModifier::Command, &passed),
        command_forms
    );
}

#[test]
fn ghostty_empty_probe_output_does_not_infer_available_bindings() {
    let bindings = GhosttyKeyBindings::from_list_output("");

    for key in rustrace::ghostty::SETUP_KEYS {
        assert!(
            !bindings.command_hint_available(key),
            "empty probe inferred {key}"
        );
    }
    assert_eq!(
        primary_modifier_text_with_ghostty(
            "{line-navigation}\n{document-navigation}\n{word-navigation}\n{navigation-deletions}\n{select-all}",
            PrimaryModifier::Command,
            &bindings,
        ),
        "Home / End             line start / end\nCtrl-Home / Ctrl-End   document start / end\nCtrl-Left / Ctrl-Right  previous / next word\nOption-Backspace       delete previous word\nCtrl-A                 select all"
    );
}

#[test]
fn ghostty_1_3_1_verbatim_text_bindings_are_exact_translations() {
    let bindings = GhosttyKeyBindings::from_list_output(
        r#"keybind = super+arrow_right=text:\\x05
keybind = super+arrow_left=text:\\x01
keybind = super+backspace=text:\\x15
"#,
    );

    for key in ["super+arrow_left", "super+arrow_right", "super+backspace"] {
        assert!(
            bindings.command_hint_available(key),
            "Ghostty 1.3.1 serialization was not recognized for {key}"
        );
    }
    assert_eq!(
        primary_modifier_text_with_ghostty(
            "{line-navigation}\n{navigation-deletions}",
            PrimaryModifier::Command,
            &bindings,
        ),
        "⌘Left / ⌘Right       line start / end\nOption-Backspace / ⌘Backspace  word / line-start delete"
    );
}

#[test]
fn ghostty_binding_parser_ignores_all_action_flag_prefixes_on_key_tokens() {
    let bindings = GhosttyKeyBindings::from_list_output(
        r#"keybind = performable:super+f=start_search
keybind = all:super+z=text:x
keybind = global:super+arrow_left=text:\x01
keybind = unconsumed:super+k=text:\x0b
keybind = future_flag:super+home=scroll_to_top
"#,
    );

    for (key, status) in [
        ("super+f", GhosttyBindingStatus::Owned),
        ("super+z", GhosttyBindingStatus::Rewritten),
        ("super+arrow_left", GhosttyBindingStatus::Rewritten),
        ("super+k", GhosttyBindingStatus::Rewritten),
        ("super+home", GhosttyBindingStatus::Owned),
    ] {
        assert_eq!(bindings.status(key), Some(status), "{key}");
    }
    assert!(bindings.command_hint_available("super+arrow_left"));
}

#[test]
fn file_name_panel_goldens_cover_new_and_prefilled_rename() {
    for (kind, title, input, action) in [
        (FilePromptKind::Create, "new file", "", " ↵ create "),
        (
            FilePromptKind::Rename,
            "rename file",
            "src/main.rs",
            " ↵ rename ",
        ),
    ] {
        let state = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_file_prompt(FilePromptState::new(kind, input));
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        let hits = MainView::new(&state, &editor(), &Viewport::default(), &[])
            .with_palette(Palette::terminal())
            .render_with_hit_map(area, &mut buffer);
        let output = buffer_lines(&buffer).join("\n");

        assert_eq!(hits.overlay, Rect::new(12, 8, 56, 7));
        assert!(output.contains(title), "{output}");
        assert!(output.contains(action), "{output}");
        assert!(output.contains(" esc cancel "), "{output}");
        let expected_field = match kind {
            FilePromptKind::Create => format!("src/{input}▏"),
            FilePromptKind::Rename => format!(" {input}▏"),
        };
        assert!(output.contains(&expected_field), "{output}");
        assert!(!hits.overlay_confirm.is_empty());
        assert!(!hits.overlay_cancel.is_empty());
        assert!(!output.contains(" NEW FILE "), "{output}");
        assert!(!output.contains(" RENAME "), "{output}");
    }
}

#[test]
fn file_name_panel_pills_dispatch_submit_and_cancel() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_file_prompt(FilePromptState::new(FilePromptKind::Create, "src/new.rs"));
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    let hits = MainView::new(&state, &editor(), &Viewport::default(), &[])
        .with_palette(Palette::terminal())
        .render_with_hit_map(area, &mut buffer);

    for (rect, expected) in [
        (hits.overlay_confirm, ShellInput::SubmitFilePrompt),
        (hits.overlay_cancel, ShellInput::CancelFilePrompt),
    ] {
        let mut reducer = MouseState::default();
        assert_eq!(
            reduce_and_map(
                &mut reducer,
                mouse(MouseEventKind::Down(MouseButton::Left), rect.x, rect.y),
                10,
                &hits,
                ShellState {
                    modal: ShellModal::FilePrompt,
                },
            ),
            Some(expected)
        );
    }
}

#[test]
fn files_header_find_label_is_dim_and_opens_the_payload_free_panel_route() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy);
    let terminal = render(80, 24, &state);
    let hits = render_hits("fn main() {}\n", &state, &Viewport::default());
    let header = rendered_lines(&terminal)[0].clone();

    assert!(header.starts_with(" files"), "{header}");
    assert!(header.contains("find"), "{header}");
    assert!(!hits.sidebar_find.is_empty());
    for x in hits.sidebar_find.x..hits.sidebar_find.right() {
        assert!(
            terminal
                .backend()
                .buffer()
                .cell((x, hits.sidebar_find.y))
                .unwrap()
                .modifier
                .contains(Modifier::DIM)
        );
    }

    let mut reducer = MouseState::default();
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                hits.sidebar_find.x,
                hits.sidebar_find.y,
            ),
            10,
            &hits,
            ShellState::default(),
        ),
        Some(ShellInput::OpenFind)
    );
}

#[test]
fn find_panel_goldens_cover_80x24_and_120x40_with_both_active_fields() {
    for (width, height, expected_panel) in [
        (80, 24, Rect::new(12, 7, 56, 9)),
        (120, 40, Rect::new(32, 15, 56, 9)),
    ] {
        for field in [FindPanelField::Find, FindPanelField::Replace] {
            let state = view_state(RecordingState::Active, JournalHealth::Healthy)
                .with_find_panel(FindPanelState::new("crab", "🦀", field, "2 of 3"));
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            let hits = MainView::new(&state, &editor(), &Viewport::default(), &[])
                .with_palette(Palette::terminal())
                .render_with_hit_map(area, &mut buffer);
            let output = buffer_lines(&buffer).join("\n");

            assert_eq!(hits.overlay, expected_panel);
            for expected in [
                "find and replace",
                "find",
                "replace",
                "2 of 3",
                " ↵ next ",
                " replace ",
                " replace all ",
                " esc close ",
            ] {
                assert!(output.contains(expected), "{width}x{height}: {output}");
            }
            assert_eq!(output.matches('▏').count(), 1, "{width}x{height}: {output}");
            assert!(!hits.find_next.is_empty());
            assert!(!hits.find_replace.is_empty());
            assert!(!hits.find_replace_all.is_empty());
            assert!(!hits.find_close.is_empty());
        }
    }
}

#[test]
fn find_panel_counter_distinguishes_empty_and_no_match_queries() {
    for (query, counter, expected, rejected) in [
        ("", "", None, Some("no matches")),
        ("missing", "no matches", Some("no matches"), Some("0 of 0")),
    ] {
        let state = view_state(RecordingState::Active, JournalHealth::Healthy).with_find_panel(
            FindPanelState::new(query, "replacement", FindPanelField::Find, counter),
        );
        let output = rendered_lines(&render(80, 24, &state)).join("\n");
        if let Some(expected) = expected {
            assert!(output.contains(expected), "{output}");
        }
        if let Some(rejected) = rejected {
            assert!(!output.contains(rejected), "{output}");
        }
    }
}

#[test]
fn find_panel_pills_and_outside_click_dispatch_the_keyboard_parity_routes() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy).with_find_panel(
        FindPanelState::new("crab", "🦀", FindPanelField::Find, "1 of 1"),
    );
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    let hits = MainView::new(&state, &editor(), &Viewport::default(), &[])
        .with_palette(Palette::terminal())
        .render_with_hit_map(area, &mut buffer);
    let routes = [
        (hits.find_next, ShellInput::FindNext),
        (hits.find_replace, ShellInput::Replace),
        (hits.find_replace_all, ShellInput::ReplaceAll),
        (hits.find_close, ShellInput::CloseFind),
    ];

    for (rect, expected) in routes {
        let mut reducer = MouseState::default();
        assert_eq!(
            reduce_and_map(
                &mut reducer,
                mouse(MouseEventKind::Down(MouseButton::Left), rect.x, rect.y),
                10,
                &hits,
                ShellState {
                    modal: ShellModal::FindPanel,
                },
            ),
            Some(expected)
        );
    }

    let mut reducer = MouseState::default();
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            10,
            &hits,
            ShellState {
                modal: ShellModal::FindPanel,
            },
        ),
        Some(ShellInput::CloseFind)
    );
}

#[test]
fn command_menu_is_anchored_above_sidebar_menu_with_all_current_entries() {
    assert_eq!(
        COMMAND_MENU_ENTRIES.as_slice(),
        [
            "Check",
            "Run",
            "Clippy",
            "Format",
            "Doc",
            "Update dependencies",
            "Update Rustrace",
            "Automatic checks: On",
            "Console",
            "Test cases",
            "Keybinds",
            "Quit",
        ]
        .as_slice()
    );
    for modifier in [PrimaryModifier::Control, PrimaryModifier::Command] {
        let state = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_primary_modifier(modifier)
            .with_command_menu(8);
        let terminal = render(80, 24, &state);
        let output = rendered_lines(&terminal).join("\n");
        for entry in COMMAND_MENU_ENTRIES {
            assert!(
                output.contains(entry),
                "{modifier:?}: missing menu entry {entry:?}\n{output}"
            );
        }
        assert!(!output.contains("Add"), "{modifier:?}: {output}");
        assert!(!output.contains("Remove"), "{modifier:?}: {output}");
        assert!(output.contains("Quit"), "{modifier:?}: {output}");
        assert!(!output.contains("Quit ("), "{modifier:?}: {output}");
        assert!(!output.contains("⌘Q"), "{modifier:?}: {output}");
    }
    let state = view_state(RecordingState::Active, JournalHealth::Healthy).with_command_menu(8);
    let terminal = render(80, 24, &state);
    let output = rendered_lines(&terminal).join("\n");
    let (menu_x, menu_y) = find_text(&terminal, "menu");
    let (picker_x, picker_y) = find_text(&terminal, "Console");
    assert!(
        picker_y < menu_y,
        "menu did not open above its anchor\n{output}"
    );
    assert!(
        picker_x <= menu_x,
        "menu did not align with its anchor\n{output}"
    );
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((picker_x, picker_y))
            .unwrap()
            .bg,
        Palette::terminal().accent,
        "selected menu row did not use the accent\n{output}"
    );
}

#[test]
fn command_menu_has_no_parallel_editor_command_dispatch() {
    assert!(
        !include_str!("../src/tui/mod.rs").contains("command_menu_editor_command"),
        "menu actions must dispatch only through the production activation path"
    );
}

#[test]
fn test_case_picker_is_a_dimmed_scrolling_modal_at_80x24_and_larger() {
    let rows = (0..30)
        .map(|index| {
            TestCasePickerRow::new(
                format!("case-{index:02}"),
                match index % 4 {
                    0 => "—".to_owned(),
                    1 => "PASS".to_owned(),
                    2 => format!("FAIL line {index}"),
                    _ => "ERROR".to_owned(),
                },
            )
        })
        .collect();
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_test_case_picker(TestCasePickerState::new(rows, 29, None));

    for (width, height) in [(80, 24), (120, 40)] {
        let editor = editor();
        let viewport = Viewport::default();
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        let hits = MainView::new(&state, &editor, &viewport, &[])
            .with_palette(Palette::terminal())
            .render_with_hit_map(area, &mut buffer);
        let screen = buffer_lines(&buffer).join("\n");
        assert!(screen.contains("Test cases"), "{width}x{height}: {screen}");
        assert!(screen.contains("case-29"), "{width}x{height}: {screen}");
        assert!(screen.contains("PASS"), "{width}x{height}: {screen}");
        assert!(screen.contains("Run all"), "{width}x{height}: {screen}");
        assert!(!screen.contains("read-only, refreshed"), "{screen}");
        assert!(hits.test_case_rows.iter().any(|(_, index)| *index == 29));
        assert!(hits.test_case_rows.iter().any(|(_, index)| *index == 30));
        assert!(!hits.test_case_scrollbar_thumb.is_empty());
        let selected = hits
            .test_case_rows
            .iter()
            .find(|(_, index)| *index == 29)
            .unwrap()
            .0;
        assert_eq!(
            buffer.cell((selected.x, selected.y)).unwrap().bg,
            Palette::terminal().accent
        );
        assert!(
            buffer
                .cell((0, 0))
                .unwrap()
                .modifier
                .contains(Modifier::DIM)
        );
    }
}

#[test]
fn test_case_picker_absence_and_safe_statuses_are_explicit() {
    let unavailable = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_test_case_picker(TestCasePickerState::new(
            Vec::new(),
            0,
            Some("No packaged test cases (assignment format v1)".to_owned()),
        ));
    let screen = rendered_lines(&render(80, 24, &unavailable)).join("\n");
    assert!(
        screen.contains("No packaged test cases (assignment format v1)"),
        "{screen}"
    );
    assert!(screen.contains("Run all"), "{screen}");

    let unsafe_status = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_test_case_picker(TestCasePickerState::new(
            vec![TestCasePickerRow::new(
                "unsafe",
                format!("FAIL line 1 {}[2J\\\\xff", '\u{1b}'),
            )],
            0,
            None,
        ));
    let terminal = render(80, 24, &unsafe_status);
    let screen = rendered_lines(&terminal).join("\n");
    assert!(screen.contains("\\u{1b}"), "{screen}");
    assert!(
        !terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.symbol().as_bytes().contains(&0x1b))
    );
}

#[test]
fn test_case_picker_mouse_hover_click_double_click_and_exclusivity_are_explicit() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy).with_test_case_picker(
        TestCasePickerState::new(
            vec![
                TestCasePickerRow::new("alpha", "—"),
                TestCasePickerRow::new("beta", "PASS"),
            ],
            0,
            None,
        ),
    );
    let hits = render_hits("alpha beta", &state, &Viewport::default());
    let shell = ShellState {
        modal: ShellModal::TestCases,
    };
    let row = hits.test_case_rows[1].0;
    let mut reducer = MouseState::default();

    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Moved, row.x, row.y),
            1,
            &hits,
            shell,
        ),
        Some(ShellInput::SelectTestCase(1))
    );
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Down(MouseButton::Left), row.x, row.y),
            10,
            &hits,
            shell,
        ),
        Some(ShellInput::SelectTestCase(1))
    );
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Up(MouseButton::Left), row.x, row.y),
            11,
            &hits,
            shell,
        ),
        None
    );
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Down(MouseButton::Left), row.x, row.y),
            20,
            &hits,
            shell,
        ),
        Some(ShellInput::RunTestCase(1))
    );
    assert_eq!(
        reduce_and_map(
            &mut MouseState::default(),
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                hits.editor.rect.x,
                hits.editor.rect.y
            ),
            30,
            &hits,
            shell,
        ),
        None,
        "the modal leaked a background editor click"
    );
    assert_eq!(
        reduce_and_map(
            &mut MouseState::default(),
            mouse(MouseEventKind::ScrollDown, row.x, row.y),
            31,
            &hits,
            shell,
        ),
        Some(ShellInput::ScrollTestCases(3))
    );
    assert_eq!(
        reduce_and_map(
            &mut MouseState::default(),
            mouse(MouseEventKind::ScrollUp, row.x, row.y),
            32,
            &hits,
            shell,
        ),
        Some(ShellInput::ScrollTestCases(-3))
    );
}

#[test]
fn rendered_context_menu_rows_resolve_to_the_enter_actions() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy).with_command_menu(0);
    let editor = editor();
    let viewport = Viewport::default();
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    let hits = MainView::new(&state, &editor, &viewport, &[])
        .with_palette(Palette::terminal())
        .render_with_hit_map(area, &mut buffer);
    let expected = [
        CommandMenuAction::Check,
        CommandMenuAction::Run,
        CommandMenuAction::Clippy,
        CommandMenuAction::Format,
        CommandMenuAction::Doc,
        CommandMenuAction::Update,
        CommandMenuAction::UpdateRustrace,
        CommandMenuAction::AutomaticChecks,
        CommandMenuAction::Console,
        CommandMenuAction::TestCases,
        CommandMenuAction::Keybinds,
        CommandMenuAction::Quit,
    ];

    assert_eq!(hits.context_menu_rows.len(), expected.len());
    for ((row, index), action) in hits.context_menu_rows.iter().zip(expected) {
        assert_eq!(command_menu_action(*index), Some(action));
        let rendered = (row.x..row.right())
            .filter_map(|x| buffer.cell((x, row.y)))
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            rendered.contains(COMMAND_MENU_ENTRIES[*index]),
            "row {index} does not render its Enter action: {rendered:?}"
        );
    }
}

#[test]
fn editor_context_menu_goldens_anchor_at_the_click_and_render_exact_disabled_states() {
    let area = Rect::new(0, 0, 80, 24);
    let anchor = Position::new(38, 5);
    let expected = ["Cut", "Copy", "Paste", "Select all"];
    assert_eq!(EDITOR_CONTEXT_MENU_ENTRIES, expected);

    for (has_selection, has_clipboard, enabled) in [
        (false, false, [false, false, false, true]),
        (true, false, [true, true, false, true]),
        (false, true, [false, false, true, true]),
        (true, true, [true, true, true, true]),
    ] {
        let menu = EditorContextMenuState::new(anchor, has_selection, has_clipboard);
        let state = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_editor_context_menu(menu);
        let editor = editor();
        let mut buffer = Buffer::empty(area);
        let hits = MainView::new(&state, &editor, &Viewport::default(), &[])
            .with_palette(Palette::terminal())
            .render_with_hit_map(area, &mut buffer);

        assert_eq!(hits.overlay, Rect::new(anchor.x, anchor.y, 12, 6));
        assert_eq!(hits.editor_context_menu_rows.len(), 4);
        for (index, ((row, actual_index, actual_enabled), label)) in hits
            .editor_context_menu_rows
            .iter()
            .zip(expected)
            .enumerate()
        {
            assert_eq!((*actual_index, *actual_enabled), (index, enabled[index]));
            let rendered = (row.x..row.right())
                .filter_map(|x| buffer.cell((x, row.y)))
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains(label), "row {index}: {rendered:?}");
            let style = buffer.cell((row.x, row.y)).unwrap();
            assert_eq!(
                style.modifier.contains(Modifier::DIM),
                !enabled[index],
                "row {index} has the wrong disabled style"
            );
            assert_eq!(
                style.bg == Palette::terminal().accent,
                enabled[index] && index == menu.selected(),
                "row {index} has the wrong selected style"
            );
        }
    }
}

#[test]
fn editor_context_menu_right_down_and_modal_left_down_routes_are_exact() {
    let base = view_state(RecordingState::Active, JournalHealth::Healthy);
    let hits = render_hits("alpha", &base, &Viewport::default());
    let anchor = Position::new(hits.editor.rect.x + 4, hits.editor.rect.y + 2);
    let mut reducer = MouseState::default();
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Down(MouseButton::Right), anchor.x, anchor.y,),
            1,
            &hits,
            ShellState::default(),
        ),
        Some(ShellInput::OpenEditorContextMenu(anchor))
    );
    for rect in [hits.tab_pills[0].0, hits.output, hits.pane_split] {
        let mut reducer = MouseState::default();
        assert_eq!(
            reduce_and_map(
                &mut reducer,
                mouse(MouseEventKind::Down(MouseButton::Right), rect.x, rect.y),
                1,
                &hits,
                ShellState::default(),
            ),
            None,
            "right down outside the source editor must stay ignored"
        );
    }

    let menu = EditorContextMenuState::new(anchor, false, true);
    let state = base.with_editor_context_menu(menu);
    let menu_hits = render_hits("alpha", &state, &Viewport::default());
    let shell = ShellState {
        modal: ShellModal::EditorContextMenu,
    };
    for (row, index, enabled) in &menu_hits.editor_context_menu_rows {
        let mut reducer = MouseState::default();
        assert_eq!(
            reduce_and_map(
                &mut reducer,
                mouse(MouseEventKind::Down(MouseButton::Left), row.x, row.y),
                2,
                &menu_hits,
                shell,
            ),
            enabled.then_some(ShellInput::ActivateEditorContextMenu(*index)),
            "row {index} activation ignored its disabled state"
        );
    }
    let mut reducer = MouseState::default();
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                hits.sidebar_rows[0].0.x,
                hits.sidebar_rows[0].0.y
            ),
            3,
            &menu_hits,
            shell,
        ),
        Some(ShellInput::CancelEditorContextMenu)
    );
    for kind in [
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Down(MouseButton::Right),
    ] {
        let mut reducer = MouseState::default();
        assert_eq!(
            reduce_and_map(
                &mut reducer,
                mouse(kind, anchor.x, anchor.y),
                4,
                &menu_hits,
                shell,
            ),
            None,
            "{kind:?} escaped the context-menu modal"
        );
    }
    let mut reducer = MouseState::default();
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::ScrollDown, anchor.x, anchor.y),
            5,
            &menu_hits,
            shell,
        ),
        Some(ShellInput::ScrollEditorContextMenu(3)),
        "wheel inside the context menu must use its bounded modal route"
    );
}

#[test]
fn editor_context_menu_enter_and_escape_share_the_modal_actions() {
    let anchor = Position::new(38, 5);
    let mut paste_only = EditorContextMenuState::new(anchor, false, true);
    assert_eq!(paste_only.selected(), 2);
    assert_eq!(
        editor_context_menu_key_action(
            &mut paste_only,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ),
        EditorContextMenuKeyAction::Activate(2)
    );
    assert_eq!(
        editor_context_menu_key_action(
            &mut paste_only,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        ),
        EditorContextMenuKeyAction::Cancel
    );

    let mut all = EditorContextMenuState::new(anchor, true, true);
    assert_eq!(
        editor_context_menu_key_action(&mut all, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),),
        EditorContextMenuKeyAction::SelectionChanged
    );
    assert_eq!(all.selected(), 1);
    assert_eq!(
        editor_context_menu_key_action(&mut all, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),),
        EditorContextMenuKeyAction::Activate(1)
    );

    let select_all_only = EditorContextMenuState::new(anchor, false, false);
    assert_eq!(select_all_only.selected(), 3);
    assert_eq!(
        select_all_only.command(3),
        Some(rustrace::tui::EditorCommand::SelectAll)
    );
}

#[test]
fn files_context_menu_goldens_at_80x24_and_120x40_anchor_and_dim_exact_rows() {
    assert_eq!(
        FILES_CONTEXT_MENU_ENTRIES,
        ["open", "rename…", "delete…", "new file…"]
    );
    let anchor = Position::new(4, 4);
    for (width, height) in [(80, 24), (120, 40)] {
        for (has_file, enabled) in [
            (true, [true, true, true, true]),
            (false, [false, false, false, true]),
        ] {
            let menu = FilesContextMenuState::new(anchor, has_file);
            let state = view_state(RecordingState::Active, JournalHealth::Healthy)
                .with_files_context_menu(menu);
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            let hits = MainView::new(&state, &editor(), &Viewport::default(), &[])
                .with_palette(Palette::terminal())
                .render_with_hit_map(area, &mut buffer);

            assert_eq!(hits.overlay, Rect::new(anchor.x, anchor.y, 11, 6));
            let rendered = (hits.overlay.y..hits.overlay.bottom())
                .map(|y| {
                    (hits.overlay.x..hits.overlay.right())
                        .filter_map(|x| buffer.cell((x, y)))
                        .map(|cell| cell.symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            assert_eq!(
                rendered,
                [
                    "┌─────────┐",
                    "│open     │",
                    "│rename…  │",
                    "│delete…  │",
                    "│new file…│",
                    "└─────────┘",
                ],
                "files menu golden changed at {width}x{height}"
            );
            assert_eq!(hits.files_context_menu_rows.len(), 4);
            for (index, (row, actual_index, actual_enabled)) in
                hits.files_context_menu_rows.iter().enumerate()
            {
                assert_eq!((*actual_index, *actual_enabled), (index, enabled[index]));
                let cell = buffer.cell((row.x, row.y)).unwrap();
                assert_eq!(
                    cell.modifier.contains(Modifier::DIM),
                    !enabled[index],
                    "row {index} has the wrong disabled style"
                );
                assert_eq!(
                    cell.bg == Palette::terminal().accent,
                    enabled[index] && index == menu.selected(),
                    "row {index} has the wrong selected style"
                );
            }
        }
    }
}

#[test]
fn files_context_menu_right_down_routes_rows_header_and_empty_space() {
    let base = view_state(RecordingState::Active, JournalHealth::Healthy);
    let hits = render_hits("alpha", &base, &Viewport::default());
    let (row, rustrace::tui::shell::SidebarTarget::File(index)) = hits.sidebar_rows[1] else {
        panic!("second sidebar row was not a file")
    };
    let cases = [
        (Position::new(row.x + 2, row.y), Some(index)),
        (
            Position::new(hits.sidebar_files.x + 1, hits.sidebar_files.y),
            None,
        ),
        (
            Position::new(
                hits.sidebar_files.x + 1,
                hits.sidebar_rows.last().unwrap().0.bottom() + 2,
            ),
            None,
        ),
    ];
    for (position, target) in cases {
        assert!(hits.sidebar_files.contains(position));
        let mut reducer = MouseState::default();
        assert_eq!(
            reduce_and_map(
                &mut reducer,
                mouse(
                    MouseEventKind::Down(MouseButton::Right),
                    position.x,
                    position.y,
                ),
                1,
                &hits,
                ShellState::default(),
            ),
            Some(ShellInput::OpenFilesContextMenu(position, target))
        );
    }
}

#[test]
fn files_context_menu_modal_activates_enabled_rows_and_ignores_disabled_rows() {
    let anchor = Position::new(4, 4);
    for (has_file, expected) in [
        (
            true,
            [
                Some(FilesContextMenuAction::Open),
                Some(FilesContextMenuAction::Rename),
                Some(FilesContextMenuAction::Delete),
                Some(FilesContextMenuAction::NewFile),
            ],
        ),
        (
            false,
            [None, None, None, Some(FilesContextMenuAction::NewFile)],
        ),
    ] {
        let menu = FilesContextMenuState::new(anchor, has_file);
        let state = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_files_context_menu(menu);
        let hits = render_hits("alpha", &state, &Viewport::default());
        let shell = ShellState {
            modal: ShellModal::FilesContextMenu,
        };
        for ((row, index, _), expected_action) in hits.files_context_menu_rows.iter().zip(expected)
        {
            assert_eq!(menu.action(*index), expected_action);
            let mut reducer = MouseState::default();
            assert_eq!(
                reduce_and_map(
                    &mut reducer,
                    mouse(MouseEventKind::Down(MouseButton::Left), row.x, row.y),
                    2,
                    &hits,
                    shell,
                ),
                expected_action.map(|_| ShellInput::ActivateFilesContextMenu(*index)),
            );
        }
    }
}

#[test]
fn files_context_menu_escape_outside_second_right_and_wheel_are_modal() {
    let anchor = Position::new(4, 4);
    let mut menu = FilesContextMenuState::new(anchor, true);
    assert_eq!(
        files_context_menu_key_action(&mut menu, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),),
        FilesContextMenuKeyAction::Cancel
    );
    let state =
        view_state(RecordingState::Active, JournalHealth::Healthy).with_files_context_menu(menu);
    let hits = render_hits("alpha", &state, &Viewport::default());
    let shell = ShellState {
        modal: ShellModal::FilesContextMenu,
    };
    for (kind, position, expected) in [
        (
            MouseEventKind::Down(MouseButton::Left),
            Position::new(hits.editor.rect.x, hits.editor.rect.y),
            Some(ShellInput::CancelFilesContextMenu),
        ),
        (
            MouseEventKind::Down(MouseButton::Right),
            anchor,
            Some(ShellInput::CancelFilesContextMenu),
        ),
        (
            MouseEventKind::ScrollDown,
            Position::new(hits.overlay.x + 1, hits.overlay.y + 1),
            Some(ShellInput::ScrollFilesContextMenu(3)),
        ),
    ] {
        let mut reducer = MouseState::default();
        assert_eq!(
            reduce_and_map(
                &mut reducer,
                mouse(kind, position.x, position.y),
                3,
                &hits,
                shell,
            ),
            expected,
        );
    }

    assert_eq!(
        files_context_menu_key_action(&mut menu, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),),
        FilesContextMenuKeyAction::SelectionChanged
    );
    assert_eq!(menu.selected(), 1);
}

#[test]
fn buffer_tab_golden_excludes_interspersed_read_only_files_without_false_overflow() {
    let state = MainViewState::new(
        "課題🦀 Ownership and Borrowing Lab",
        vec![
            BufferTabViewEntry::new("src/main.rs", true, false),
            BufferTabViewEntry::new("src/parser.rs", false, false),
        ],
        vec!["ready".to_owned()],
        RecordingState::Active,
        JournalHealth::Healthy,
        "saved",
    )
    .with_file_tree(vec![
        FileTreeViewEntry::new("src/main.rs", true, true, false, true),
        FileTreeViewEntry::new("Cargo.toml", false, false, false, false),
        FileTreeViewEntry::new("src/parser.rs", false, false, false, true),
    ]);
    let terminal = render(MIN_TERMINAL_WIDTH, 24, &state);
    let editor = editor();
    let viewport = Viewport::default();
    let mut hit_buffer = Buffer::empty(Rect::new(0, 0, MIN_TERMINAL_WIDTH, 24));
    let hits = MainView::new(&state, &editor, &viewport, &[])
        .with_palette(Palette::terminal())
        .render_with_hit_map(hit_buffer.area, &mut hit_buffer);
    let MainLayout::Full(layout) = main_layout(Rect::new(0, 0, MIN_TERMINAL_WIDTH, 24)) else {
        unreachable!();
    };
    let sidebar = text_in_rect(&terminal, layout.sidebar).join("\n");
    let tabs = text_in_rect(&terminal, layout.tab_bar).join("\n");

    assert!(sidebar.contains("Cargo.toml"), "{sidebar}");
    assert!(tabs.contains("main.rs"), "{tabs}");
    assert!(tabs.contains("parser.rs"), "{tabs}");
    assert!(!tabs.contains("Cargo.toml"), "{tabs}");
    assert!(!tabs.contains(" < ") && !tabs.contains(" > "), "{tabs}");
    assert!(tabs.ends_with("Ln …"), "{tabs}");
    assert!(
        !tabs.contains("課題") && !tabs.contains("Ownership"),
        "{tabs}"
    );
    assert_eq!(tabs.matches('…').count(), 1, "{tabs}");
    let (position_x, _) = find_text_in_rect(&terminal, layout.tab_bar, "Ln …");
    assert_eq!(position_x + 4, layout.tab_bar.right(), "{tabs}");
    assert_eq!(
        hits.tab_pills
            .iter()
            .map(|(_, path)| path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/main.rs", "src/parser.rs"]
    );
}

fn assert_overflowing_tab_window(active: usize, hides_left: bool, hides_right: bool) {
    let buffers = (0..10)
        .map(|index| {
            BufferTabViewEntry::new(
                format!("src/long-buffer-{index:02}.rs"),
                index == active,
                false,
            )
        })
        .collect();
    let state = MainViewState::new(
        "Tab window",
        buffers,
        vec![],
        RecordingState::Active,
        JournalHealth::Healthy,
        "saved",
    );
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    let hits = MainView::new(&state, &editor(), &Viewport::default(), &[])
        .with_palette(Palette::terminal())
        .render_with_hit_map(area, &mut buffer);
    let paths = hits
        .tab_pills
        .iter()
        .map(|(_, path)| path.clone())
        .collect::<Vec<_>>();
    let active_path = format!("src/long-buffer-{active:02}.rs");
    let (active_rect, hit_active_path) = hits
        .tab_pills
        .iter()
        .find(|(_, path)| path == &active_path)
        .unwrap_or_else(|| panic!("active buffer {active_path} is outside tab window {paths:?}"));

    assert_eq!(hit_active_path, &active_path);
    let all_paths = (0..10)
        .map(|index| format!("src/long-buffer-{index:02}.rs"))
        .collect::<Vec<_>>();
    let start = all_paths.iter().position(|path| path == &paths[0]).unwrap();
    assert_eq!(
        paths,
        all_paths[start..start + paths.len()],
        "tab hit targets must retain the contiguous displayed identities"
    );
    assert_eq!(!hits.tab_scroll_left.is_empty(), hides_left, "{paths:?}");
    assert_eq!(!hits.tab_scroll_right.is_empty(), hides_right, "{paths:?}");
    let active_cell = buffer.cell((active_rect.x, active_rect.y)).unwrap();
    assert_eq!(active_cell.bg, Palette::terminal().accent, "{paths:?}");
    assert!(
        active_cell.modifier.contains(Modifier::BOLD),
        "active tab is not styled as active: {paths:?}"
    );
}

#[test]
fn overflowing_tabs_keep_the_first_active_buffer_visible_at_80x24() {
    assert_overflowing_tab_window(0, false, true);
}

#[test]
fn overflowing_tabs_keep_a_middle_active_buffer_visible_at_80x24() {
    assert_overflowing_tab_window(5, true, true);
}

#[test]
fn overflowing_tabs_keep_the_final_active_buffer_visible_at_80x24() {
    assert_overflowing_tab_window(9, true, false);
}

#[test]
fn editor_scrollbar_hit_map_matches_track_and_thumb_geometry_above_the_threshold() {
    let area = Rect::new(0, 0, 80, 24);
    let MainLayout::Full(layout) = main_layout(area) else {
        unreachable!();
    };
    let state = view_state(RecordingState::Active, JournalHealth::Healthy);

    let exact_text = (0..layout.editor.height)
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let exact_editor = EditorBuffer::new(
        DocumentId::new("scrollbar-boundary").unwrap(),
        &exact_text,
        NoopEditorEffects,
    );
    let mut exact_buffer = Buffer::empty(area);
    let exact_hits = MainView::new(&state, &exact_editor, &Viewport::default(), &[])
        .with_palette(Palette::terminal())
        .render_with_hit_map(area, &mut exact_buffer);
    assert!(exact_hits.editor_scrollbar_track.is_empty());
    assert!(exact_hits.editor_scrollbar_thumb.is_empty());
    assert_eq!(exact_hits.editor.rect, layout.editor);

    let overflow_text = (0..usize::from(layout.editor.height) * 3)
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let overflow_editor = EditorBuffer::new(
        DocumentId::new("scrollbar-overflow").unwrap(),
        &overflow_text,
        NoopEditorEffects,
    );
    let mut viewport = Viewport::default();
    viewport.scroll_vertical(
        isize::try_from(layout.editor.height).unwrap(),
        overflow_editor.line_count(),
        usize::from(layout.editor.height),
    );
    let mut overflow_buffer = Buffer::empty(area);
    let hits = MainView::new(&state, &overflow_editor, &viewport, &[])
        .with_palette(Palette::terminal())
        .render_with_hit_map(area, &mut overflow_buffer);

    assert_eq!(hits.editor_scrollbar_track, layout.editor_scrollbar);
    assert_eq!(hits.editor.rect.width, layout.editor.width - 1);
    assert_eq!(hits.editor.rect.right(), hits.editor_scrollbar_track.x);
    assert_eq!(hits.editor_scrollbar_thumb.width, 1);
    assert!(hits.editor_scrollbar_thumb.height > 0);
    assert!(hits.editor_scrollbar_thumb.height < hits.editor_scrollbar_track.height);
    assert!(hits.editor_scrollbar_thumb.y > hits.editor_scrollbar_track.y);
    assert!(hits.editor_scrollbar_thumb.bottom() <= hits.editor_scrollbar_track.bottom());
    for y in hits.editor_scrollbar_track.y..hits.editor_scrollbar_track.bottom() {
        let symbol = overflow_buffer
            .cell((hits.editor_scrollbar_track.x, y))
            .unwrap()
            .symbol();
        let in_thumb =
            y >= hits.editor_scrollbar_thumb.y && y < hits.editor_scrollbar_thumb.bottom();
        assert_eq!(symbol, if in_thumb { "▐" } else { "▕" });
    }
}

fn output_hits(rows: Vec<OutputRow>) -> (Vec<(Rect, usize)>, String) {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy).with_output_rows(rows);
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    let hits = MainView::new(&state, &editor(), &Viewport::default(), &[])
        .with_palette(Palette::terminal())
        .render_with_hit_map(area, &mut buffer);
    let output = buffer.content().iter().map(|cell| cell.symbol()).collect();
    (hits.output_rows, output)
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn mouse_with_modifiers(
    kind: MouseEventKind,
    column: u16,
    row: u16,
    modifiers: KeyModifiers,
) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers,
    }
}

fn render_hits(
    text: &str,
    state: &MainViewState,
    viewport: &Viewport,
) -> rustrace::tui::shell::HitMap {
    render_hits_at(80, 24, text, state, viewport)
}

fn render_hits_at(
    width: u16,
    height: u16,
    text: &str,
    state: &MainViewState,
    viewport: &Viewport,
) -> rustrace::tui::shell::HitMap {
    let editor = EditorBuffer::new(
        DocumentId::new("mouse-hit-test").unwrap(),
        text,
        NoopEditorEffects,
    );
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = rustrace::tui::shell::HitMap::default();
    terminal
        .draw(|frame| {
            hits = MainView::new(state, &editor, viewport, &[])
                .with_palette(Palette::terminal())
                .render_with_hit_map(frame.area(), frame.buffer_mut());
        })
        .unwrap();
    hits
}

fn reduce_and_map(
    state: &mut MouseState,
    event: MouseEvent,
    now_ms: u64,
    hits: &rustrace::tui::shell::HitMap,
    shell: ShellState,
) -> Option<ShellInput> {
    state.reduce(&event, now_ms, shell.modal);
    mouse_input_for_event(&event, hits, &shell, state)
}

fn full_shell_layout(
    area: Rect,
    bottom: BottomPane,
    mode_bar: bool,
    bottom_height: Option<u16>,
) -> ShellLayout {
    let ShellLayoutResult::Full(layout) =
        shell_layout_with_bottom_height(area, bottom, mode_bar, bottom_height)
    else {
        panic!("expected a full shell layout");
    };
    layout
}

fn full_resized_shell_layout(
    area: Rect,
    bottom: BottomPane,
    mode_bar: bool,
    bottom_height: Option<u16>,
    sidebar_width: Option<u16>,
) -> ShellLayout {
    let ShellLayoutResult::Full(layout) =
        shell_layout_with_sizes(area, bottom, mode_bar, bottom_height, sidebar_width)
    else {
        panic!("expected a full shell layout");
    };
    layout
}

#[test]
fn pane_resize_reducer_requires_divider_down_and_preserves_a_click_without_drag() {
    let area = Rect::new(0, 0, 80, 24);
    let layout = full_shell_layout(area, BottomPane::Output, false, None);
    let state = view_state(RecordingState::Active, JournalHealth::Healthy);
    let hits = render_hits("alpha", &state, &Viewport::default());
    let mut resize = PaneResizeState::default();

    let ignored = resize.reduce(
        &mouse(
            MouseEventKind::Drag(MouseButton::Left),
            hits.pane_split.x,
            layout.gap.y.saturating_sub(3),
        ),
        &hits,
        ShellState::default(),
        layout,
    );
    assert!(!ignored.consumed);
    assert!(!ignored.changed);
    assert_eq!(resize.bottom_height(), None);

    let down = resize.reduce(
        &mouse(
            MouseEventKind::Down(MouseButton::Left),
            hits.pane_split.x,
            hits.pane_split.y,
        ),
        &hits,
        ShellState::default(),
        layout,
    );
    assert!(down.consumed);
    assert!(!down.changed);
    let up = resize.reduce(
        &mouse(
            MouseEventKind::Up(MouseButton::Left),
            hits.pane_split.x,
            hits.pane_split.y,
        ),
        &hits,
        ShellState::default(),
        layout,
    );
    assert!(up.consumed);
    assert!(!up.changed);
    assert_eq!(resize.bottom_height(), None);
}

#[test]
fn pane_resize_reducer_tracks_drag_sequence_and_clamps_both_ends() {
    let area = Rect::new(0, 0, 80, 24);
    let layout = full_shell_layout(area, BottomPane::Output, false, None);
    let state = view_state(RecordingState::Active, JournalHealth::Healthy);
    let hits = render_hits("alpha", &state, &Viewport::default());
    let mut resize = PaneResizeState::default();
    let shell = ShellState::default();

    assert!(
        resize
            .reduce(
                &mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    hits.pane_split.x + 1,
                    hits.pane_split.y,
                ),
                &hits,
                shell,
                layout,
            )
            .consumed
    );
    let upper = resize.reduce(
        &mouse(
            MouseEventKind::Drag(MouseButton::Left),
            hits.pane_split.x + 1,
            0,
        ),
        &hits,
        shell,
        layout,
    );
    assert!(upper.consumed && upper.changed);
    assert_eq!(resize.bottom_height(), Some(13));
    let upper_layout = full_shell_layout(area, BottomPane::Output, false, resize.bottom_height());
    assert_eq!(upper_layout.editor.height, 8);

    let lower = resize.reduce(
        &mouse(
            MouseEventKind::Drag(MouseButton::Left),
            hits.pane_split.x + 1,
            u16::MAX,
        ),
        &hits,
        shell,
        upper_layout,
    );
    assert!(lower.consumed && lower.changed);
    assert_eq!(resize.bottom_height(), Some(4));
    let lower_layout = full_shell_layout(area, BottomPane::Output, false, resize.bottom_height());
    assert_eq!(lower_layout.bottom.height, 4);

    let up = resize.reduce(
        &mouse(
            MouseEventKind::Up(MouseButton::Left),
            hits.pane_split.x + 1,
            lower_layout.gap.y,
        ),
        &hits,
        shell,
        lower_layout,
    );
    assert!(up.consumed && !up.changed);
    let after_up = resize.reduce(
        &mouse(
            MouseEventKind::Drag(MouseButton::Left),
            hits.pane_split.x + 1,
            0,
        ),
        &hits,
        shell,
        lower_layout,
    );
    assert!(!after_up.consumed && !after_up.changed);
    assert_eq!(resize.bottom_height(), Some(4));
}

#[test]
fn sidebar_resize_reducer_requires_divider_down_tracks_drag_and_clamps_both_bounds() {
    let area = Rect::new(0, 0, 80, 24);
    let layout = full_shell_layout(area, BottomPane::Output, false, None);
    let state = view_state(RecordingState::Active, JournalHealth::Healthy);
    let hits = render_hits("alpha", &state, &Viewport::default());
    let mut resize = PaneResizeState::default();
    let shell = ShellState::default();

    let ignored = resize.reduce(
        &mouse(MouseEventKind::Drag(MouseButton::Left), 0, 2),
        &hits,
        shell,
        layout,
    );
    assert!(!ignored.consumed && !ignored.changed);
    assert_eq!(resize.sidebar_width(), None);

    let down = resize.reduce(
        &mouse(
            MouseEventKind::Down(MouseButton::Left),
            hits.sidebar_split.x,
            hits.sidebar_split.y + 2,
        ),
        &hits,
        shell,
        layout,
    );
    assert!(down.consumed && !down.changed);
    assert!(resize.sidebar_dragging());

    let minimum = resize.reduce(
        &mouse(MouseEventKind::Drag(MouseButton::Left), 0, 2),
        &hits,
        shell,
        layout,
    );
    assert!(minimum.consumed && minimum.changed);
    assert_eq!(resize.sidebar_width(), Some(SIDEBAR_MIN_WIDTH));

    let minimum_layout = full_resized_shell_layout(
        area,
        BottomPane::Output,
        false,
        None,
        resize.sidebar_width(),
    );
    let maximum = resize.reduce(
        &mouse(MouseEventKind::Drag(MouseButton::Left), u16::MAX, 2),
        &hits,
        shell,
        minimum_layout,
    );
    assert!(maximum.consumed && maximum.changed);
    assert_eq!(resize.sidebar_width(), Some(SIDEBAR_MAX_WIDTH));

    let up = resize.reduce(
        &mouse(MouseEventKind::Up(MouseButton::Left), u16::MAX, 2),
        &hits,
        shell,
        minimum_layout,
    );
    assert!(up.consumed && !up.changed);
    assert!(!resize.sidebar_dragging());
    let after_up = resize.reduce(
        &mouse(MouseEventKind::Drag(MouseButton::Left), 0, 2),
        &hits,
        shell,
        minimum_layout,
    );
    assert!(!after_up.consumed && !after_up.changed);
    assert_eq!(resize.sidebar_width(), Some(SIDEBAR_MAX_WIDTH));
}

#[test]
fn sidebar_width_reclamps_every_layout_after_terminal_resize() {
    let large = full_resized_shell_layout(
        Rect::new(0, 0, 120, 40),
        BottomPane::Output,
        false,
        None,
        Some(u16::MAX),
    );
    assert_eq!(large.sidebar.width, SIDEBAR_MAX_WIDTH);

    let shrunk = full_resized_shell_layout(
        Rect::new(0, 0, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT),
        BottomPane::Output,
        false,
        None,
        Some(large.sidebar.width),
    );
    assert_eq!(shrunk.sidebar.width, SIDEBAR_MAX_WIDTH);
    assert_eq!(shrunk.tab_bar.width, MIN_TERMINAL_WIDTH - SIDEBAR_MAX_WIDTH);

    let below_minimum = full_resized_shell_layout(
        Rect::new(0, 0, 80, 24),
        BottomPane::Output,
        false,
        None,
        Some(0),
    );
    assert_eq!(below_minimum.sidebar.width, SIDEBAR_MIN_WIDTH);
}

#[test]
fn sidebar_reflow_goldens_cover_all_regions_at_both_terminal_sizes() {
    for area in [Rect::new(0, 0, 80, 24), Rect::new(0, 0, 120, 40)] {
        for sidebar_width in [SIDEBAR_MIN_WIDTH, 26, SIDEBAR_MAX_WIDTH] {
            for bottom in [BottomPane::Output, BottomPane::Console] {
                let layout = full_resized_shell_layout(
                    area,
                    bottom,
                    bottom == BottomPane::Console,
                    None,
                    Some(sidebar_width),
                );
                let main_width = area.width - sidebar_width;
                assert_eq!(layout.sidebar, Rect::new(0, 0, sidebar_width, area.height));
                assert_eq!(
                    layout.sidebar_divider,
                    Rect::new(sidebar_width - 1, 0, 1, area.height)
                );
                assert_eq!(layout.tab_bar, Rect::new(sidebar_width, 0, main_width, 1));
                assert_eq!(layout.editor.x, sidebar_width);
                assert_eq!(layout.editor.width, main_width);
                assert_eq!(
                    layout.editor_scrollbar,
                    Rect::new(area.width - 1, layout.editor.y, 1, layout.editor.height)
                );
                assert_eq!(layout.gap.x, sidebar_width);
                assert_eq!(layout.gap.width, main_width);
                assert_eq!(layout.bottom.x, sidebar_width);
                assert_eq!(layout.bottom.width, main_width);
                assert_eq!(
                    layout.mode_bar,
                    Rect::new(sidebar_width, area.height - 1, main_width, 1)
                );
                assert_eq!(layout.editor.bottom(), layout.gap.y);
                assert_eq!(layout.gap.bottom(), layout.bottom.y);
                assert_eq!(layout.bottom.bottom(), layout.mode_bar.y);
            }
        }
    }
}

#[test]
fn rendered_sidebar_reflow_registers_split_editor_scrollbar_output_and_console() {
    let source = (0..80)
        .map(|line| format!("line {line:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let editor = EditorBuffer::new(
        DocumentId::new("sidebar-reflow").unwrap(),
        &source,
        NoopEditorEffects,
    );
    for (width, height) in [(80, 24), (120, 40)] {
        for sidebar_width in [SIDEBAR_MIN_WIDTH, 26, SIDEBAR_MAX_WIDTH] {
            let output_state = view_state(RecordingState::Active, JournalHealth::Healthy)
                .with_sidebar_width(sidebar_width);
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut output_hits = rustrace::tui::shell::HitMap::default();
            terminal
                .draw(|frame| {
                    output_hits = MainView::new(&output_state, &editor, &Viewport::default(), &[])
                        .with_palette(Palette::terminal())
                        .render_with_hit_map(frame.area(), frame.buffer_mut());
                })
                .unwrap();
            let output_layout = full_resized_shell_layout(
                Rect::new(0, 0, width, height),
                BottomPane::Output,
                false,
                None,
                Some(sidebar_width),
            );
            assert_eq!(output_hits.sidebar_split, output_layout.sidebar_divider);
            assert_eq!(output_hits.editor.rect.x, sidebar_width);
            assert_eq!(
                output_hits.editor_scrollbar_track,
                output_layout.editor_scrollbar
            );
            assert_eq!(output_hits.output, output_layout.bottom);

            let console_state = output_state.with_console_body_view(
                "Embedded Cargo console",
                b"output\n".to_vec(),
                b"> ".to_vec(),
                Some(2),
                true,
            );
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut console_hits = rustrace::tui::shell::HitMap::default();
            terminal
                .draw(|frame| {
                    console_hits =
                        MainView::new(&console_state, &editor, &Viewport::default(), &[])
                            .with_palette(Palette::terminal())
                            .render_with_hit_map(frame.area(), frame.buffer_mut());
                })
                .unwrap();
            let console_layout = full_resized_shell_layout(
                Rect::new(0, 0, width, height),
                BottomPane::Console,
                true,
                None,
                Some(sidebar_width),
            );
            assert_eq!(console_hits.sidebar_split, console_layout.sidebar_divider);
            assert_eq!(console_hits.editor.rect.x, sidebar_width);
            assert_eq!(
                console_hits.editor_scrollbar_track,
                console_layout.editor_scrollbar
            );
            assert_eq!(console_hits.console, console_layout.bottom);
        }
    }
}

#[test]
fn sidebar_divider_uses_accent_only_during_its_drag() {
    for (dragging, expected) in [
        (false, Palette::terminal().surface_dim),
        (true, Palette::terminal().accent),
    ] {
        let state = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_sidebar_dragging(dragging);
        let terminal = render(80, 24, &state);
        let layout = full_shell_layout(Rect::new(0, 0, 80, 24), BottomPane::Output, false, None);
        let cell = terminal
            .backend()
            .buffer()
            .cell((layout.sidebar_divider.x, layout.editor.y))
            .unwrap();
        assert_eq!(cell.symbol(), "│");
        assert_eq!(cell.fg, expected);
    }
}

#[test]
fn student_mode_bar_is_permanent_and_idle_hint_is_exact_dim_and_pill_free() {
    let area = Rect::new(0, 0, 80, 24);
    let idle_layout = full_shell_layout(area, BottomPane::Output, false, None);
    let active_layout = full_shell_layout(area, BottomPane::Output, true, None);
    assert_eq!(idle_layout.editor, active_layout.editor);
    assert_eq!(idle_layout.gap, active_layout.gap);
    assert_eq!(idle_layout.bottom, active_layout.bottom);
    assert_eq!(idle_layout.mode_bar, active_layout.mode_bar);
    assert_eq!(idle_layout.mode_bar.height, 1);

    let state = view_state(RecordingState::Active, JournalHealth::Healthy);
    let terminal = render(80, 24, &state);
    let row = text_in_rect(&terminal, idle_layout.mode_bar)[0]
        .trim_end()
        .to_owned();
    assert_eq!(row, "F1 keybinds · F7 menu · F9 console");
    let first = terminal
        .backend()
        .buffer()
        .cell((idle_layout.mode_bar.x, idle_layout.mode_bar.y))
        .unwrap();
    assert!(first.modifier.contains(Modifier::DIM));
    assert_eq!(first.bg, Palette::terminal().panel_bg);
    assert!(!row.contains(['[', ']']));
}

#[test]
fn manual_bottom_height_is_shared_and_reclamped_after_terminal_shrink() {
    let area = Rect::new(0, 0, 120, 40);
    let output = full_shell_layout(area, BottomPane::Output, false, Some(9));
    let console = full_shell_layout(area, BottomPane::Console, true, Some(9));
    assert_eq!(output.bottom.height, 9);
    assert_eq!(console.bottom.height, 9);

    let shrunk = full_shell_layout(
        Rect::new(0, 0, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT),
        BottomPane::Console,
        true,
        Some(14),
    );
    assert_eq!(shrunk.editor.height, 8);
    assert_eq!(shrunk.gap.height, 1);
    assert_eq!(shrunk.bottom.height, 4);
}

#[test]
fn pane_resize_refollows_the_caret_in_the_smaller_editor() {
    let area = Rect::new(0, 0, 80, 24);
    let source = (0..30)
        .map(|line| format!("line {line:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut editor = EditorBuffer::new(
        DocumentId::new("resize-follow").unwrap(),
        &source,
        NoopEditorEffects,
    );
    editor.move_cursor(rustrace::editor::Movement::DocumentEnd, false);
    let initial_layout = full_shell_layout(area, BottomPane::Output, false, None);
    let mut viewport = Viewport::default();
    viewport.follow_cursor(
        &editor,
        usize::from(initial_layout.editor.width),
        usize::from(initial_layout.editor.height),
    );
    let initial_top = viewport.top_line();

    let state = view_state(RecordingState::Active, JournalHealth::Healthy);
    let hits = render_hits(&source, &state, &viewport);
    let mut resize = PaneResizeState::default();
    let shell = ShellState::default();
    resize.reduce(
        &mouse(
            MouseEventKind::Down(MouseButton::Left),
            hits.pane_split.x + 1,
            hits.pane_split.y,
        ),
        &hits,
        shell,
        initial_layout,
    );
    let outcome = resize.reduce(
        &mouse(
            MouseEventKind::Drag(MouseButton::Left),
            hits.pane_split.x + 1,
            0,
        ),
        &hits,
        shell,
        initial_layout,
    );
    assert!(outcome.changed, "drag did not request a viewport refollow");

    let resized = full_shell_layout(area, BottomPane::Output, false, resize.bottom_height());
    viewport.follow_cursor(
        &editor,
        usize::from(resized.editor.width),
        usize::from(resized.editor.height),
    );
    assert!(viewport.top_line() > initial_top);
    let cursor_line = editor.cursor().line;
    assert!(cursor_line >= viewport.top_line());
    assert!(cursor_line < viewport.top_line() + usize::from(resized.editor.height));
}

#[test]
fn console_at_four_rows_keeps_its_header_and_stdin_row() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_bottom_pane_height(4)
        .with_console_body_view(
            "Embedded Cargo console",
            b"older output\nlatest output\n".to_vec(),
            b"stdin> ".to_vec(),
            Some(7),
            true,
        );
    let terminal = render(80, 24, &state);
    let layout = full_shell_layout(Rect::new(0, 0, 80, 24), BottomPane::Console, true, Some(4));
    let rows = text_in_rect(&terminal, layout.bottom);

    assert_eq!(layout.bottom.height, 4);
    assert!(rows[0].starts_with("console"), "{rows:?}");
    assert!(rows[3].starts_with("stdin> ▏"), "{rows:?}");
}

fn console_state(output: &[u8], scroll: Option<usize>) -> MainViewState {
    view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_console_body_view(
            "Embedded Cargo console",
            output.to_vec(),
            b"stdin> ".to_vec(),
            Some(7),
            true,
        )
        .with_console_scroll(scroll)
}

#[test]
fn console_wraps_long_lines_and_scrolls_back_through_older_output() {
    let mut output = String::new();
    for index in 0..40 {
        output.push_str(&format!("line-{index:02}\n"));
    }
    output.push_str(&format!("long-{}-END\n", "x".repeat(150)));
    let area = Rect::new(0, 0, 120, 40);
    let layout = full_shell_layout(area, BottomPane::Console, true, None);
    assert_eq!(layout.bottom.height, 16);

    let rows = text_in_rect(
        &render(120, 40, &console_state(output.as_bytes(), None)),
        layout.bottom,
    );
    assert!(rows[0].starts_with("console "), "{rows:?}");
    assert!(!rows[0].contains("more lines"), "{rows:?}");
    // 94 columns: the 160-column line wraps onto two rows above the prompt.
    assert!(rows[13].starts_with("long-xxx"), "{rows:?}");
    assert!(rows[14].trim_end().ends_with("-END"), "{rows:?}");
    assert!(rows[15].starts_with("stdin> "), "{rows:?}");
    assert!(rows[1].starts_with("line-28"), "{rows:?}");

    let rows = text_in_rect(
        &render(120, 40, &console_state(output.as_bytes(), Some(0))),
        layout.bottom,
    );
    // Rows 14 onward hold 26 short lines and the wrapped long line.
    assert!(
        rows[0].trim_end().ends_with("↓ 27 more lines · PgDn"),
        "{rows:?}"
    );
    assert!(rows[1].starts_with("line-00"), "{rows:?}");
    assert!(rows[14].starts_with("line-13"), "{rows:?}");

    // A stale scroll position past the end still shows the newest rows.
    let rows = text_in_rect(
        &render(120, 40, &console_state(output.as_bytes(), Some(999))),
        layout.bottom,
    );
    assert!(rows[14].trim_end().ends_with("-END"), "{rows:?}");
}

#[test]
fn console_scrollback_marks_output_older_than_its_bound() {
    let mut output = Vec::new();
    for index in 0..8_000 {
        output.extend_from_slice(format!("ROLLING-{index:05}-safe-output-line\n").as_bytes());
    }
    let layout = full_shell_layout(Rect::new(0, 0, 120, 40), BottomPane::Console, true, None);
    let rows = text_in_rect(
        &render(120, 40, &console_state(&output, Some(0))),
        layout.bottom,
    );
    assert!(
        rows[1].starts_with("[older console output omitted]"),
        "{rows:?}"
    );
    assert!(rows[2].starts_with("ROLLING-"), "{rows:?}");
    assert!(!rows[2].starts_with("ROLLING-00000"), "{rows:?}");
    let rows = text_in_rect(
        &render(120, 40, &console_state(&output, None)),
        layout.bottom,
    );
    assert!(rows[14].starts_with("ROLLING-07999"), "{rows:?}");
}

#[test]
fn console_shows_the_newest_bytes_of_a_line_without_newlines() {
    let layout = full_shell_layout(Rect::new(0, 0, 120, 40), BottomPane::Console, true, None);
    for length in [20 * 1024, 200 * 1024] {
        let mut output = b"x".repeat(length);
        output.extend_from_slice(b"NEWEST");
        let rows = text_in_rect(
            &render(120, 40, &console_state(&output, None)),
            layout.bottom,
        );
        assert!(
            rows[14].trim_end().ends_with("xNEWEST"),
            "{length}: {rows:?}"
        );
        let rows = text_in_rect(
            &render(120, 40, &console_state(&output, Some(0))),
            layout.bottom,
        );
        let first = if length > 128 * 1024 {
            assert!(
                rows[1].starts_with("[older console output omitted]"),
                "{rows:?}"
            );
            2
        } else {
            1
        };
        assert!(
            rows[first].starts_with("[line start omitted] xxx"),
            "{rows:?}"
        );
        assert!(
            rows[0].trim_end().ends_with("↓ 1 more line · PgDn"),
            "{rows:?}"
        );
    }
}

#[test]
fn running_command_keeps_the_console_wheel_and_divider_live() {
    let state = console_state(b"older\nnewer\n", None);
    let hits = render_hits("alpha", &state, &Viewport::default());
    let prompt = ShellState {
        modal: ShellModal::Prompt,
    };
    let mut reducer = MouseState::default();
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(
                MouseEventKind::ScrollUp,
                hits.console.x + 1,
                hits.console.y + 1
            ),
            1,
            &hits,
            prompt,
        ),
        Some(ShellInput::ScrollConsole(-3))
    );
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(
                MouseEventKind::ScrollDown,
                hits.console.x + 1,
                hits.console.y + 1
            ),
            2,
            &hits,
            ShellState::default(),
        ),
        Some(ShellInput::ScrollConsole(3))
    );
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(
                MouseEventKind::ScrollUp,
                hits.editor.rect.x + 1,
                hits.editor.rect.y
            ),
            3,
            &hits,
            prompt,
        ),
        None,
        "a running command must not scroll the editor"
    );

    let layout = full_shell_layout(Rect::new(0, 0, 80, 24), BottomPane::Console, true, None);
    let divider = |kind| mouse(kind, hits.pane_split.x + 1, hits.pane_split.y);
    let mut resize = PaneResizeState::default();
    assert!(
        resize
            .reduce(
                &divider(MouseEventKind::Down(MouseButton::Left)),
                &hits,
                prompt,
                layout
            )
            .consumed
    );
    let drag = resize.reduce(
        &mouse(
            MouseEventKind::Drag(MouseButton::Left),
            hits.pane_split.x + 1,
            3,
        ),
        &hits,
        prompt,
        layout,
    );
    assert!(drag.changed);
    assert!(resize.bottom_height().unwrap() > layout.bottom.height);

    let mut resize = PaneResizeState::default();
    let confirmation = ShellState {
        modal: ShellModal::Confirmation,
    };
    assert!(
        !resize
            .reduce(
                &divider(MouseEventKind::Down(MouseButton::Left)),
                &hits,
                confirmation,
                layout
            )
            .consumed
    );
}

#[test]
fn editor_mouse_mapping_covers_ascii_tab_wide_combining_past_end_and_no_gutter() {
    let text = "a\t界e\u{301}";
    let normal = view_state(RecordingState::Active, JournalHealth::Healthy);
    let viewport = Viewport::default();
    let hits = render_hits(text, &normal, &viewport);
    let source_x = hits.editor.rect.x;
    let source_y = hits.editor.rect.y;
    let shell = ShellState::default();
    for (screen_column, expected_column) in [(0, 0), (2, 2), (5, 5), (6, 6), (40, 40)] {
        let mut mouse_state = MouseState::default();
        assert_eq!(
            reduce_and_map(
                &mut mouse_state,
                mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    source_x + screen_column,
                    source_y,
                ),
                10,
                &hits,
                shell,
            ),
            Some(ShellInput::Workspace(WorkspaceInput::Editor(
                rustrace::tui::SessionInput::Command(rustrace::tui::EditorCommand::MoveTo {
                    line: 0,
                    column: expected_column,
                    selecting: false,
                })
            )))
        );
    }

    let tint_state = normal.with_diagnostic_markers(vec![DiagnosticLineMarker::new(
        0,
        DiagnosticMarkerKind::Error,
        false,
    )]);
    for (width, height) in [(80, 24), (120, 40)] {
        let hits = render_hits_at(width, height, text, &tint_state, &viewport);
        let rustrace::tui::shell::EditorHit {
            rect: _,
            top_line: _,
            left_column: _,
        } = hits.editor;
        let mut mouse_state = MouseState::default();
        assert_eq!(
            reduce_and_map(
                &mut mouse_state,
                mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    hits.editor.rect.x + 1,
                    hits.editor.rect.y,
                ),
                10,
                &hits,
                shell,
            ),
            Some(ShellInput::Workspace(WorkspaceInput::Editor(
                rustrace::tui::SessionInput::Command(rustrace::tui::EditorCommand::MoveTo {
                    line: 0,
                    column: 1,
                    selecting: false,
                })
            )))
        );
    }
}

#[test]
fn plain_left_click_on_a_compiler_tint_keeps_the_source_column_and_selects_its_diagnostic() {
    let state =
        view_state(RecordingState::Active, JournalHealth::Healthy).with_diagnostic_markers(vec![
            DiagnosticLineMarker::compiler(1, DiagnosticMarkerKind::Warning, false, 7),
        ]);
    for (width, height) in [(80, 24), (120, 40)] {
        let hits = render_hits_at(
            width,
            height,
            "zero\nwarning\n",
            &state,
            &Viewport::default(),
        );
        let event = mouse(
            MouseEventKind::Down(MouseButton::Left),
            hits.editor.rect.x + 3,
            hits.editor.rect.y + 1,
        );
        let mut mouse_state = MouseState::default();
        assert_eq!(
            reduce_and_map(&mut mouse_state, event, 10, &hits, ShellState::default(),),
            Some(ShellInput::SelectEditorDiagnostic {
                line: 1,
                column: 3,
                diagnostic_index: 7,
            })
        );
    }
}

#[test]
fn selected_diagnostic_output_row_uses_accent_foreground() {
    let state = MainViewState::new(
        "Diagnostic selection",
        vec![BufferTabViewEntry::new("src/main.rs", true, false)],
        Vec::new(),
        RecordingState::Active,
        JournalHealth::Healthy,
        "saved",
    )
    .with_output_rows(vec![
        OutputRow::diagnostic("error: mismatch", 4).with_selected(true),
    ]);
    let terminal = render(80, 24, &state);
    let MainLayout::Full(layout) = main_layout(Rect::new(0, 0, 80, 24)) else {
        unreachable!();
    };
    let first = terminal
        .backend()
        .buffer()
        .cell((layout.bottom.x, layout.bottom.y + 1))
        .unwrap();
    assert_eq!(first.fg, Palette::terminal().accent);
    assert!(first.modifier.contains(Modifier::BOLD));
}

#[test]
fn timer_wakes_draw_command_output_and_replay_playback_without_input() {
    for source in ["student command output", "replay playback"] {
        let mut draws = DrawGate::default();
        draws.request_timer();
        assert!(draws.take_draw(), "{source} timer did not request a draw");
        assert!(
            !draws.take_draw(),
            "{source} timer requested more than one draw"
        );
    }
}

#[test]
fn mouse_state_recognizes_only_a_bounded_same_cell_down_up_down_double_click() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy);
    let hits = render_hits("alpha beta", &state, &Viewport::default());
    let cell = (hits.editor.rect.x + 2, hits.editor.rect.y);
    let shell = ShellState::default();
    let mut reducer = MouseState::default();

    assert!(matches!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Down(MouseButton::Left), cell.0, cell.1),
            100,
            &hits,
            shell,
        ),
        Some(ShellInput::Workspace(WorkspaceInput::Editor(_)))
    ));
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Up(MouseButton::Left), cell.0, cell.1),
            110,
            &hits,
            shell,
        ),
        None
    );
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Moved, cell.0, cell.1),
            120,
            &hits,
            shell,
        ),
        None
    );
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Down(MouseButton::Left), cell.0, cell.1),
            450,
            &hits,
            shell,
        ),
        Some(ShellInput::Workspace(WorkspaceInput::Editor(
            rustrace::tui::SessionInput::Command(rustrace::tui::EditorCommand::SelectWord)
        )))
    );

    for (second_cell, second_time) in [((cell.0, cell.1), 451), ((cell.0 + 1, cell.1), 300)] {
        let mut reducer = MouseState::default();
        reducer.reduce(
            &mouse(MouseEventKind::Down(MouseButton::Left), cell.0, cell.1),
            100,
            ShellModal::None,
        );
        reducer.reduce(
            &mouse(MouseEventKind::Up(MouseButton::Left), cell.0, cell.1),
            110,
            ShellModal::None,
        );
        assert!(matches!(
            reduce_and_map(
                &mut reducer,
                mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    second_cell.0,
                    second_cell.1,
                ),
                second_time,
                &hits,
                shell,
            ),
            Some(ShellInput::Workspace(WorkspaceInput::Editor(
                rustrace::tui::SessionInput::Command(rustrace::tui::EditorCommand::MoveTo { .. })
            )))
        ));
    }
}

#[test]
fn mouse_state_resets_double_click_after_drag_wheel_other_button_or_modal_change() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy);
    let hits = render_hits("alpha beta", &state, &Viewport::default());
    let cell = (hits.editor.rect.x + 2, hits.editor.rect.y);
    let reset_events = [
        mouse(MouseEventKind::Drag(MouseButton::Left), cell.0 + 1, cell.1),
        mouse(MouseEventKind::ScrollDown, cell.0, cell.1),
        mouse(MouseEventKind::Down(MouseButton::Right), cell.0, cell.1),
        mouse(MouseEventKind::Moved, cell.0 + 1, cell.1),
    ];

    for reset in reset_events {
        let mut reducer = MouseState::default();
        reducer.reduce(
            &mouse(MouseEventKind::Down(MouseButton::Left), cell.0, cell.1),
            100,
            ShellModal::None,
        );
        reducer.reduce(
            &mouse(MouseEventKind::Up(MouseButton::Left), cell.0, cell.1),
            110,
            ShellModal::None,
        );
        reducer.reduce(&reset, 120, ShellModal::None);
        assert!(matches!(
            reduce_and_map(
                &mut reducer,
                mouse(MouseEventKind::Down(MouseButton::Left), cell.0, cell.1),
                200,
                &hits,
                ShellState::default(),
            ),
            Some(ShellInput::Workspace(WorkspaceInput::Editor(
                rustrace::tui::SessionInput::Command(rustrace::tui::EditorCommand::MoveTo { .. })
            )))
        ));
    }

    let mut reducer = MouseState::default();
    reducer.reduce(
        &mouse(MouseEventKind::Down(MouseButton::Left), cell.0, cell.1),
        100,
        ShellModal::None,
    );
    reducer.reduce(
        &mouse(MouseEventKind::Up(MouseButton::Left), cell.0, cell.1),
        110,
        ShellModal::None,
    );
    reducer.reduce(
        &mouse(MouseEventKind::Moved, cell.0, cell.1),
        120,
        ShellModal::Prompt,
    );
    reducer.reduce(
        &mouse(MouseEventKind::Moved, cell.0, cell.1),
        130,
        ShellModal::None,
    );
    assert!(matches!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Down(MouseButton::Left), cell.0, cell.1),
            200,
            &hits,
            ShellState::default(),
        ),
        Some(ShellInput::Workspace(WorkspaceInput::Editor(
            rustrace::tui::SessionInput::Command(rustrace::tui::EditorCommand::MoveTo { .. })
        )))
    ));
}

#[test]
fn drag_shift_click_modal_drops_pill_down_only_and_wheel_mapping_are_explicit() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_confirmation(ConfirmationState::new("Discard?"));
    let hits = render_hits("alpha\nbeta", &state, &Viewport::default());
    let cell = (hits.editor.rect.x + 2, hits.editor.rect.y);

    let mut reducer = MouseState::default();
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse_with_modifiers(
                MouseEventKind::Down(MouseButton::Left),
                cell.0,
                cell.1,
                KeyModifiers::SHIFT,
            ),
            1,
            &hits,
            ShellState::default(),
        ),
        Some(ShellInput::Workspace(WorkspaceInput::Editor(
            rustrace::tui::SessionInput::Command(rustrace::tui::EditorCommand::MoveTo {
                line: 0,
                column: 2,
                selecting: true,
            })
        )))
    );
    assert!(matches!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Drag(MouseButton::Left), cell.0 + 1, cell.1),
            2,
            &hits,
            ShellState::default(),
        ),
        Some(ShellInput::Workspace(WorkspaceInput::Editor(
            rustrace::tui::SessionInput::Command(rustrace::tui::EditorCommand::MoveTo {
                selecting: true,
                ..
            })
        )))
    ));
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse_with_modifiers(
                MouseEventKind::Down(MouseButton::Left),
                cell.0,
                cell.1,
                KeyModifiers::CONTROL,
            ),
            2,
            &hits,
            ShellState::default(),
        ),
        None
    );
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::ScrollUp, cell.0, cell.1),
            3,
            &hits,
            ShellState::default(),
        ),
        Some(ShellInput::ScrollEditor(-3))
    );

    for modal in [
        ShellModal::Prompt,
        ShellModal::Confirmation,
        ShellModal::Keybinds,
        ShellModal::ConsoleOverwrite,
    ] {
        let mut reducer = MouseState::default();
        assert_eq!(
            reduce_and_map(
                &mut reducer,
                mouse(MouseEventKind::Down(MouseButton::Left), cell.0, cell.1),
                10,
                &hits,
                ShellState { modal },
            ),
            None,
            "{modal:?} leaked a mouse event"
        );
    }

    let menu = ShellState {
        modal: ShellModal::CommandMenu,
    };
    for kind in [
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
    ] {
        let mut reducer = MouseState::default();
        assert_eq!(
            reduce_and_map(&mut reducer, mouse(kind, cell.0, cell.1), 10, &hits, menu),
            None,
            "{kind:?} dismissed the command menu"
        );
    }
    let mut reducer = MouseState::default();
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Down(MouseButton::Left), cell.0, cell.1),
            10,
            &hits,
            menu,
        ),
        Some(ShellInput::CancelMenu),
        "an outside left down must dismiss only the command menu"
    );

    let mut reducer = MouseState::default();
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Down(MouseButton::Left), cell.0, cell.1),
            10,
            &hits,
            ShellState {
                modal: ShellModal::Completion,
            },
        ),
        Some(ShellInput::DismissCompletion),
        "an outside completion click must close the popup for normal remapping"
    );

    let confirm = (hits.overlay_confirm.x, hits.overlay_confirm.y);
    let shell = ShellState {
        modal: ShellModal::Confirmation,
    };
    let mut reducer = MouseState::default();
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Up(MouseButton::Left), confirm.0, confirm.1),
            10,
            &hits,
            shell,
        ),
        None
    );
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                confirm.0,
                confirm.1
            ),
            11,
            &hits,
            shell,
        ),
        Some(ShellInput::Workspace(WorkspaceInput::ConfirmDestructive))
    );
}

#[test]
fn student_shell_mouse_targets_cover_rows_footers_tabs_output_and_picker_pills() {
    let buffers = (0..14)
        .map(|index| {
            BufferTabViewEntry::new(format!("src/long-buffer-{index:02}.rs"), index == 7, false)
        })
        .collect();
    let state = MainViewState::new(
        "Mouse targets",
        buffers,
        Vec::new(),
        RecordingState::Active,
        JournalHealth::Healthy,
        "active",
    )
    .with_output_rows(vec![OutputRow::diagnostic("error row", 4)]);
    let hits = render_hits("alpha", &state, &Viewport::default());
    let shell = ShellState::default();
    let map_down = |rect: Rect| {
        let mut reducer = MouseState::default();
        reduce_and_map(
            &mut reducer,
            mouse(MouseEventKind::Down(MouseButton::Left), rect.x, rect.y),
            1,
            &hits,
            shell,
        )
    };

    let (file_rect, rustrace::tui::shell::SidebarTarget::File(file_index)) = hits.sidebar_rows[0]
    else {
        panic!("first sidebar row was not a file")
    };
    assert_eq!(
        map_down(file_rect),
        Some(ShellInput::ActivateFile(file_index))
    );
    assert_eq!(
        map_down(hits.sidebar_new),
        Some(ShellInput::Workspace(WorkspaceInput::BeginCreate))
    );
    assert_eq!(map_down(hits.sidebar_menu), Some(ShellInput::OpenMenu));
    let (tab_rect, tab_path) = hits.tab_pills[0].clone();
    assert_eq!(map_down(tab_rect), Some(ShellInput::ActivateTab(tab_path)));
    assert_eq!(
        map_down(hits.new_tab),
        Some(ShellInput::Workspace(WorkspaceInput::BeginCreate))
    );
    assert_eq!(
        map_down(hits.tab_scroll_left),
        Some(ShellInput::Workspace(WorkspaceInput::Editor(
            rustrace::tui::SessionInput::Command(rustrace::tui::EditorCommand::PreviousBuffer)
        )))
    );
    assert_eq!(
        map_down(hits.tab_scroll_right),
        Some(ShellInput::Workspace(WorkspaceInput::Editor(
            rustrace::tui::SessionInput::Command(rustrace::tui::EditorCommand::NextBuffer)
        )))
    );
    let (diagnostic_rect, diagnostic_index) = hits.output_rows[0];
    assert_eq!(
        map_down(diagnostic_rect),
        Some(ShellInput::SelectDiagnostic(diagnostic_index))
    );

    let menu_state = state.with_command_menu(0);
    let menu_hits = render_hits("alpha", &menu_state, &Viewport::default());
    let (entry_rect, entry_index) = menu_hits.context_menu_rows[4];
    let mut reducer = MouseState::default();
    assert_eq!(
        reduce_and_map(
            &mut reducer,
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                entry_rect.x,
                entry_rect.y
            ),
            1,
            &menu_hits,
            ShellState {
                modal: ShellModal::CommandMenu,
            },
        ),
        Some(ShellInput::ActivateMenu(entry_index))
    );
}

#[test]
fn scrolled_output_rows_keep_their_real_diagnostic_indices() {
    let (hits, output) = output_hits(vec![
        OutputRow::diagnostic("warning fourth", 3),
        OutputRow::diagnostic("error fifth", 4),
    ]);

    assert!(output.contains("warning fourth"), "{output}");
    assert!(output.contains("error fifth"), "{output}");
    assert_eq!(
        hits.iter().map(|(_, index)| *index).collect::<Vec<_>>(),
        vec![3, 4]
    );
}

#[test]
fn captured_output_rows_are_not_registered_as_diagnostics() {
    let (hits, output) = output_hits(vec![
        OutputRow::diagnostic("error: managed source", 7),
        OutputRow::captured("student result", OutputStream::Stdout),
        OutputRow::captured("student trace", OutputStream::Stderr),
    ]);

    assert!(output.contains("student result"), "{output}");
    assert!(output.contains("student trace"), "{output}");
    assert!(!output.contains("stdout:") && !output.contains("stderr:"));
    assert_eq!(
        hits.iter().map(|(_, index)| *index).collect::<Vec<_>>(),
        vec![7]
    );
}

#[test]
fn cargo_lock_is_absent_from_sidebar_and_tabs_without_shifting_file_targets() {
    let state = MainViewState::new(
        "Managed lockfile regression",
        vec![
            BufferTabViewEntry::new("src/a.rs", true, false),
            BufferTabViewEntry::new("Cargo.lock", false, false),
            BufferTabViewEntry::new("src/b.rs", false, false),
        ],
        vec![],
        RecordingState::Active,
        JournalHealth::Healthy,
        "saved",
    )
    .with_file_tree(vec![
        FileTreeViewEntry::new("src/a.rs", true, true, false, true),
        FileTreeViewEntry::new("Cargo.lock", false, false, false, false),
        FileTreeViewEntry::new("src/b.rs", false, false, false, true),
    ]);
    let editor = EditorBuffer::new(
        DocumentId::new("lockfile-hidden").unwrap(),
        "fn main() {}\n",
        NoopEditorEffects,
    );
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    let hits = MainView::new(&state, &editor, &Viewport::default(), &[])
        .with_palette(Palette::terminal())
        .render_with_hit_map(area, &mut buffer);
    let output = buffer_lines(&buffer).join("\n");

    assert!(!output.contains("Cargo.lock"), "{output}");
    assert_eq!(hits.sidebar_rows.len(), 2);
    assert!(matches!(
        hits.sidebar_rows[0].1,
        rustrace::tui::shell::SidebarTarget::File(0)
    ));
    assert!(matches!(
        hits.sidebar_rows[1].1,
        rustrace::tui::shell::SidebarTarget::File(2)
    ));
    assert_eq!(hits.tab_pills.len(), 2);
    assert_eq!(hits.tab_pills[0].1, "src/a.rs");
    assert_eq!(hits.tab_pills[1].1, "src/b.rs");
}

#[test]
fn confirmation_overlay_has_square_accent_frame_and_action_pills() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy)
        .with_confirmation(ConfirmationState::new("Delete src/main.rs?"));
    let terminal = render(80, 24, &state);
    let output = rendered_lines(&terminal).join("\n");
    assert!(output.contains("Delete src/main.rs?"), "{output}");
    assert!(output.contains(" ↵ confirm "), "{output}");
    assert!(output.contains(" esc cancel "), "{output}");
    assert!(output.contains('┌') && output.contains('┘'), "{output}");
}

#[test]
fn warning_toast_is_terminal_safe_and_uses_the_exact_paste_policy_text() {
    for primary_modifier in [PrimaryModifier::Control, PrimaryModifier::Command] {
        let state = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_primary_modifier(primary_modifier)
            .with_mode_bar(ModeBarState::new(ModeBarKind::Menu, "esc close  ↵ run"))
            .with_toast(ToastState::new(
                ToastKind::Warning,
                "paste blocked",
                format!("{PASTE_BLOCKED_WARNING}\u{1b}[2J"),
            ));
        let terminal = render(120, 40, &state);
        let output = rendered_lines(&terminal).join("\n");
        assert_eq!(
            rendered_toast_body(&terminal, "● paste blocked"),
            format!(r"{PASTE_BLOCKED_WARNING}\u{{1b}}[2J"),
            "{primary_modifier:?} mode changed literal policy text"
        );
        assert!(output.contains("\\u{1b}"), "{output}");
        assert!(!output.contains('\u{1b}'), "{output}");
        let ShellLayoutResult::Full(layout) =
            shell_layout(Rect::new(0, 0, 120, 40), BottomPane::Output, true)
        else {
            unreachable!();
        };
        let (_, toast_y) = find_text(&terminal, "● paste blocked");
        assert!(
            toast_y >= layout.bottom.y,
            "toast is not over output: {output}"
        );
        assert!(
            toast_y < layout.mode_bar.y,
            "toast overlaps mode bar: {output}"
        );
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((layout.bottom.right() - 1, layout.bottom.bottom() - 1))
                .unwrap()
                .symbol(),
            "┘",
            "toast is not anchored to the terminal's bottom-right content corner\n{output}"
        );
    }
}

#[test]
fn file_tree_shows_active_dirty_read_only_and_scrolls_without_focus_highlighting() {
    let files = (0..30)
        .map(|index| {
            FileTreeViewEntry::new(
                format!("src/{index:02}.rs"),
                index == 29,
                index == 29,
                matches!(index, 27 | 29),
                index != 28,
            )
        })
        .collect();
    let state = MainViewState::new(
        "Ownership and Borrowing Lab",
        vec![],
        vec!["ready".to_owned()],
        RecordingState::Active,
        JournalHealth::Healthy,
        "modified",
    )
    .with_file_tree(files);

    let terminal = render(100, 24, &state);
    let output = rendered_lines(&terminal).join("\n");

    assert!(output.contains(" files"));
    assert!(output.contains("● src/29.rs*"));
    assert!(output.contains("src/28.rs"));
    assert!(output.contains("src/27.rs*"));
    assert!(!output.contains("src/00.rs"));

    let (active_x, active_y) = find_text(&terminal, "src/29.rs");
    let active = terminal
        .backend()
        .buffer()
        .cell((active_x, active_y))
        .unwrap();
    assert_eq!(active.bg, Color::Reset);
    assert!(!active.modifier.contains(Modifier::BOLD));

    let MainLayout::Full(layout) = main_layout(Rect::new(0, 0, 100, 24)) else {
        unreachable!();
    };
    let (read_only_x, read_only_y) = find_text_in_rect(&terminal, layout.sidebar, "src/28.rs");
    let read_only = terminal
        .backend()
        .buffer()
        .cell((read_only_x, read_only_y))
        .unwrap();
    assert!(read_only.modifier.contains(Modifier::DIM));
}

#[test]
fn long_file_names_keep_dirty_and_read_only_markers_visible() {
    let state = MainViewState::new(
        "Ownership and Borrowing Lab",
        vec![],
        vec!["ready".to_owned()],
        RecordingState::Active,
        JournalHealth::Healthy,
        "modified",
    )
    .with_file_tree(vec![
        FileTreeViewEntry::new(
            "src/a-very-long-file-name-that-will-be-truncated.rs",
            true,
            false,
            true,
            true,
        ),
        FileTreeViewEntry::new(
            "Cargo-a-very-long-read-only-name.lock",
            false,
            true,
            false,
            false,
        ),
    ]);

    let terminal = render(100, 24, &state);
    let output = rendered_lines(&terminal).join("\n");
    assert!(output.contains("● src/a-very-long-file…*"), "{output}");
    assert!(output.contains("Cargo-a-very-long-rea…"), "{output}");
    let MainLayout::Full(layout) = main_layout(Rect::new(0, 0, 100, 24)) else {
        unreachable!();
    };
    let (x, y) = find_text_in_rect(&terminal, layout.sidebar, "Cargo-a-very-long-rea…");
    assert!(
        terminal
            .backend()
            .buffer()
            .cell((x, y))
            .unwrap()
            .modifier
            .contains(Modifier::DIM)
    );
}

#[test]
fn paste_rejection_warning_is_visible_with_external_notice_and_modal_panels() {
    for prompt in ["find panel", "new file panel", "rename file panel"] {
        let state = MainViewState::new(
            "Ownership and Borrowing Lab",
            vec![],
            vec![format!(
                "external change detected\nrestored canonical bytes\nchoose a recovery action\n{PASTE_BLOCKED_WARNING}"
            )],
            RecordingState::Active,
            JournalHealth::Healthy,
            "active",
        );

        let output = rendered_lines(&render(80, 24, &state)).join("\n");
        assert!(
            output.contains("Paste blocked:"),
            "paste rejection must remain visible for {prompt:?} in the minimum supported layout\n{output}"
        );
    }

    let state = MainViewState::new(
        "Ownership and Borrowing Lab",
        vec![],
        vec![PASTE_BLOCKED_WARNING.to_owned()],
        RecordingState::Active,
        JournalHealth::Healthy,
        "active",
    );
    assert!(
        rendered_lines(&render(80, 24, &state))
            .join("\n")
            .contains("Paste blocked:")
    );
    assert!(
        rendered_lines(&render(300, 36, &state))
            .join("\n")
            .contains(PASTE_BLOCKED_WARNING)
    );
}

#[test]
fn footer_formats_the_active_file_and_one_based_cursor_position() {
    let state = view_state(RecordingState::Active, JournalHealth::Healthy);
    let mut editor = editor();
    editor.move_cursor(rustrace::editor::Movement::Down, false);
    editor.move_cursor(rustrace::editor::Movement::Right, false);
    editor.move_cursor(rustrace::editor::Movement::Right, false);
    let viewport = Viewport::default();
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| {
            frame.render_widget(
                MainView::new(&state, &editor, &viewport, &[]).with_palette(Palette::terminal()),
                frame.area(),
            );
        })
        .unwrap();

    let MainLayout::Full(layout) = main_layout(Rect::new(0, 0, 100, 24)) else {
        unreachable!();
    };
    let tab_row = text_in_rect(&terminal, layout.tab_bar).join("\n");
    assert!(tab_row.contains("main.rs"), "{tab_row}");
    assert!(tab_row.contains("Ln 2, Col 3"), "{tab_row}");
    let (x, y) = find_text_in_rect(&terminal, layout.tab_bar, "main.rs");
    let active_tab = terminal.backend().buffer().cell((x, y)).unwrap();
    assert_eq!(active_tab.bg, Palette::terminal().accent);
    assert!(active_tab.modifier.contains(Modifier::BOLD));
}

#[test]
fn recording_and_degraded_journal_states_are_explicit_and_alert_colored() {
    let state = view_state(RecordingState::Inactive, JournalHealth::Degraded);
    let terminal = render(100, 24, &state);
    let output = rendered_lines(&terminal).join("\n");

    assert_eq!(output.matches(" ERROR ").count(), 1);
    assert!(!output.contains("Recording:"));
    assert!(!output.contains("Journal:"));

    let (x, y) = find_text(&terminal, "ERROR");
    let cell = terminal.backend().buffer().cell((x, y)).unwrap();
    assert_eq!(cell.bg, Color::LightRed);
    assert!(cell.modifier.contains(Modifier::BOLD));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Failure {
    RawMode,
    AlternateScreen,
    EnableBracketedPaste,
    EnableMouseCapture,
    KeyboardEnhancementSupport,
    PushKeyboardEnhancement,
    PopKeyboardEnhancement,
    DisableMouseCapture,
    DisableBracketedPaste,
    ShowCursor,
    LeaveScreen,
    DisableRawMode,
}

struct FakeTerminalOperations {
    calls: Rc<RefCell<Vec<String>>>,
    failure: Option<Failure>,
    keyboard_enhancement_supported: bool,
    background_response: Option<[u8; 3]>,
}

impl FakeTerminalOperations {
    fn new(calls: Rc<RefCell<Vec<String>>>, failure: Option<Failure>) -> Self {
        Self {
            calls,
            failure,
            keyboard_enhancement_supported: true,
            background_response: None,
        }
    }

    fn without_keyboard_enhancement(mut self) -> Self {
        self.keyboard_enhancement_supported = false;
        self
    }

    fn with_background_response(mut self, response: Option<[u8; 3]>) -> Self {
        self.background_response = response;
        self
    }

    fn call(&mut self, name: &'static str, failure: Failure) -> io::Result<()> {
        self.calls.borrow_mut().push(name.to_owned());
        if self.failure == Some(failure) {
            Err(io::Error::other(format!("{name} failed")))
        } else {
            Ok(())
        }
    }
}

impl TerminalOperations for FakeTerminalOperations {
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        self.call("enable_raw_mode", Failure::RawMode)
    }

    fn enter_alternate_screen(&mut self) -> io::Result<()> {
        self.call("enter_alternate_screen", Failure::AlternateScreen)
    }

    fn enable_bracketed_paste(&mut self) -> io::Result<()> {
        self.call("enable_bracketed_paste", Failure::EnableBracketedPaste)
    }

    fn enable_mouse_capture(&mut self) -> io::Result<()> {
        self.call("enable_mouse_capture", Failure::EnableMouseCapture)
    }

    fn supports_keyboard_enhancement(&mut self) -> io::Result<bool> {
        self.call(
            "supports_keyboard_enhancement",
            Failure::KeyboardEnhancementSupport,
        )?;
        Ok(self.keyboard_enhancement_supported)
    }

    fn push_keyboard_enhancement(&mut self, flags: KeyboardEnhancementFlags) -> io::Result<()> {
        self.calls
            .borrow_mut()
            .push(format!("push_keyboard_enhancement:{}", flags.bits()));
        if self.failure == Some(Failure::PushKeyboardEnhancement) {
            return Err(io::Error::other("push_keyboard_enhancement failed"));
        }
        Ok(())
    }

    fn pop_keyboard_enhancement(&mut self) -> io::Result<()> {
        self.call("pop_keyboard_enhancement", Failure::PopKeyboardEnhancement)
    }

    fn query_background_color(
        &mut self,
        timeout: std::time::Duration,
    ) -> io::Result<Option<[u8; 3]>> {
        self.calls
            .borrow_mut()
            .push(format!("query_background_color:{}", timeout.as_millis()));
        Ok(self.background_response)
    }

    fn disable_mouse_capture(&mut self) -> io::Result<()> {
        self.call("disable_mouse_capture", Failure::DisableMouseCapture)
    }

    fn disable_bracketed_paste(&mut self) -> io::Result<()> {
        self.call("disable_bracketed_paste", Failure::DisableBracketedPaste)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.call("show_cursor", Failure::ShowCursor)
    }

    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        self.call("leave_alternate_screen", Failure::LeaveScreen)
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        self.call("disable_raw_mode", Failure::DisableRawMode)
    }
}

#[test]
fn startup_theme_auto_switch_queries_once_and_handles_light_dark_no_reply_and_disabled() {
    for (label, auto_switch, response, name, expected, queried) in [
        (
            "light",
            true,
            Some([0xff, 0xff, 0xff]),
            ThemeName::Terminal,
            ThemeName::CatppuccinLatte,
            true,
        ),
        (
            "dark",
            true,
            Some([0x10, 0x20, 0x30]),
            ThemeName::CatppuccinLatte,
            ThemeName::Catppuccin,
            true,
        ),
        (
            "no-reply",
            true,
            None,
            ThemeName::Terminal,
            ThemeName::Terminal,
            true,
        ),
        (
            "disabled",
            false,
            Some([0xff, 0xff, 0xff]),
            ThemeName::Catppuccin,
            ThemeName::Catppuccin,
            false,
        ),
    ] {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let operations =
            FakeTerminalOperations::new(Rc::clone(&calls), None).with_background_response(response);
        let mut session = TerminalSession::enter(operations).unwrap();
        let config = ThemeConfig {
            name: Some(name),
            auto_switch,
            ..ThemeConfig::default()
        };

        let effective = session.resolve_theme(&config, Some("truecolor"));
        assert_eq!(effective.name, expected, "{label}");
        let query_calls = calls
            .borrow()
            .iter()
            .filter(|call| call.starts_with("query_background_color:"))
            .count();
        assert_eq!(query_calls, usize::from(queried), "{label}");
        if queried {
            let timeout = calls
                .borrow()
                .iter()
                .find(|call| call.starts_with("query_background_color:"))
                .unwrap()
                .split_once(':')
                .unwrap()
                .1
                .parse::<u128>()
                .unwrap();
            assert!(timeout > 0 && timeout <= 250, "{label}: {timeout} ms");
        }
    }
}

#[test]
fn terminal_session_restores_once_after_normal_exit() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let session = TerminalSession::enter(FakeTerminalOperations::new(Rc::clone(&calls), None))
        .expect("terminal setup succeeds");

    assert!(session.keyboard_enhancement_supported());
    assert!(session.keyboard_enhancement_active());
    assert_eq!(
        *calls.borrow(),
        [
            "enable_raw_mode",
            "enter_alternate_screen",
            "enable_bracketed_paste",
            "enable_mouse_capture",
            "supports_keyboard_enhancement",
            "push_keyboard_enhancement:5",
        ]
    );
    drop(session);
    assert_eq!(
        *calls.borrow(),
        [
            "enable_raw_mode",
            "enter_alternate_screen",
            "enable_bracketed_paste",
            "enable_mouse_capture",
            "supports_keyboard_enhancement",
            "push_keyboard_enhancement:5",
            "pop_keyboard_enhancement",
            "disable_mouse_capture",
            "disable_bracketed_paste",
            "show_cursor",
            "leave_alternate_screen",
            "disable_raw_mode",
        ]
    );
}

#[test]
fn terminal_session_does_not_push_or_pop_enhancement_when_support_is_absent() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let session = TerminalSession::enter(
        FakeTerminalOperations::new(Rc::clone(&calls), None).without_keyboard_enhancement(),
    )
    .expect("terminal setup succeeds without keyboard enhancement");

    assert!(!session.keyboard_enhancement_supported());
    assert!(!session.keyboard_enhancement_active());
    drop(session);
    assert_eq!(
        *calls.borrow(),
        [
            "enable_raw_mode",
            "enter_alternate_screen",
            "enable_bracketed_paste",
            "enable_mouse_capture",
            "supports_keyboard_enhancement",
            "disable_mouse_capture",
            "disable_bracketed_paste",
            "show_cursor",
            "leave_alternate_screen",
            "disable_raw_mode",
        ]
    );
}

#[test]
fn terminal_session_treats_an_enhancement_probe_error_as_unsupported() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let session = TerminalSession::enter(FakeTerminalOperations::new(
        Rc::clone(&calls),
        Some(Failure::KeyboardEnhancementSupport),
    ))
    .expect("an unavailable capability probe leaves Control input available");

    assert!(!session.keyboard_enhancement_supported());
    assert!(!session.keyboard_enhancement_active());
    drop(session);
    assert_eq!(
        *calls.borrow(),
        [
            "enable_raw_mode",
            "enter_alternate_screen",
            "enable_bracketed_paste",
            "enable_mouse_capture",
            "supports_keyboard_enhancement",
            "disable_mouse_capture",
            "disable_bracketed_paste",
            "show_cursor",
            "leave_alternate_screen",
            "disable_raw_mode",
        ]
    );
}

#[test]
fn terminal_session_restores_without_pop_when_enhancement_push_fails() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let result = TerminalSession::enter(FakeTerminalOperations::new(
        Rc::clone(&calls),
        Some(Failure::PushKeyboardEnhancement),
    ));

    assert!(result.is_err());
    assert_eq!(
        *calls.borrow(),
        [
            "enable_raw_mode",
            "enter_alternate_screen",
            "enable_bracketed_paste",
            "enable_mouse_capture",
            "supports_keyboard_enhancement",
            "push_keyboard_enhancement:5",
            "disable_mouse_capture",
            "disable_bracketed_paste",
            "show_cursor",
            "leave_alternate_screen",
            "disable_raw_mode",
        ]
    );
}

#[test]
fn terminal_session_restores_after_partial_setup_failure() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let result = TerminalSession::enter(FakeTerminalOperations::new(
        Rc::clone(&calls),
        Some(Failure::AlternateScreen),
    ));

    assert!(result.is_err());
    assert_eq!(
        *calls.borrow(),
        [
            "enable_raw_mode",
            "enter_alternate_screen",
            "disable_mouse_capture",
            "disable_bracketed_paste",
            "show_cursor",
            "leave_alternate_screen",
            "disable_raw_mode",
        ]
    );
}

#[test]
fn terminal_session_restores_after_bracketed_paste_setup_failure() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let result = TerminalSession::enter(FakeTerminalOperations::new(
        Rc::clone(&calls),
        Some(Failure::EnableBracketedPaste),
    ));

    assert!(result.is_err());
    assert_eq!(
        *calls.borrow(),
        [
            "enable_raw_mode",
            "enter_alternate_screen",
            "enable_bracketed_paste",
            "disable_mouse_capture",
            "disable_bracketed_paste",
            "show_cursor",
            "leave_alternate_screen",
            "disable_raw_mode",
        ]
    );
}

#[test]
fn terminal_session_continues_after_mouse_capture_setup_failure() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let session = TerminalSession::enter(FakeTerminalOperations::new(
        Rc::clone(&calls),
        Some(Failure::EnableMouseCapture),
    ))
    .expect("mouse capture failure falls back to keyboard input");

    drop(session);
    assert_eq!(
        *calls.borrow(),
        [
            "enable_raw_mode",
            "enter_alternate_screen",
            "enable_bracketed_paste",
            "enable_mouse_capture",
            "supports_keyboard_enhancement",
            "push_keyboard_enhancement:5",
            "pop_keyboard_enhancement",
            "disable_mouse_capture",
            "disable_bracketed_paste",
            "show_cursor",
            "leave_alternate_screen",
            "disable_raw_mode",
        ]
    );
}

#[test]
fn terminal_session_does_not_restore_when_raw_mode_was_not_enabled() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let result = TerminalSession::enter(FakeTerminalOperations::new(
        Rc::clone(&calls),
        Some(Failure::RawMode),
    ));

    assert!(result.is_err());
    assert_eq!(*calls.borrow(), ["enable_raw_mode"]);
}

#[test]
fn terminal_session_restores_during_panic_unwind() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let result = catch_unwind(AssertUnwindSafe(|| {
        let _session = TerminalSession::enter(FakeTerminalOperations::new(Rc::clone(&calls), None))
            .expect("terminal setup succeeds");
        panic!("event loop panic");
    }));

    assert!(result.is_err());
    assert_eq!(
        *calls.borrow(),
        [
            "enable_raw_mode",
            "enter_alternate_screen",
            "enable_bracketed_paste",
            "enable_mouse_capture",
            "supports_keyboard_enhancement",
            "push_keyboard_enhancement:5",
            "pop_keyboard_enhancement",
            "disable_mouse_capture",
            "disable_bracketed_paste",
            "show_cursor",
            "leave_alternate_screen",
            "disable_raw_mode",
        ]
    );
}

#[test]
fn terminal_session_attempts_every_later_cleanup_after_each_failure() {
    for failure in [
        Failure::PopKeyboardEnhancement,
        Failure::DisableMouseCapture,
        Failure::DisableBracketedPaste,
        Failure::ShowCursor,
        Failure::LeaveScreen,
        Failure::DisableRawMode,
    ] {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let session = TerminalSession::enter(FakeTerminalOperations::new(
            Rc::clone(&calls),
            Some(failure),
        ))
        .expect("terminal setup succeeds");

        drop(session);
        assert_eq!(
            *calls.borrow(),
            [
                "enable_raw_mode",
                "enter_alternate_screen",
                "enable_bracketed_paste",
                "enable_mouse_capture",
                "supports_keyboard_enhancement",
                "push_keyboard_enhancement:5",
                "pop_keyboard_enhancement",
                "disable_mouse_capture",
                "disable_bracketed_paste",
                "show_cursor",
                "leave_alternate_screen",
                "disable_raw_mode",
            ],
            "cleanup stopped after {failure:?} failed"
        );
    }
}

#[test]
fn update_menu_goldens_cover_badge_toggle_and_minimum_sidebar() {
    for newer in [false, true] {
        for enabled in [false, true] {
            for width in [18, 26] {
                let state = view_state(RecordingState::Active, JournalHealth::Healthy)
                    .with_sidebar_width(width)
                    .with_update_state(rustrace::update::UpdateState {
                        checks_enabled: enabled,
                        latest: Some(rustrace::update::ReleaseIdentity {
                            version: if newer { "99.0.0" } else { "0.0.0" }.into(),
                            tag: if newer { "v99.0.0" } else { "v0.0.0" }.into(),
                            commit: "a".repeat(40),
                        }),
                        ..Default::default()
                    })
                    .with_command_menu(6);
                let terminal = render(80, 24, &state);
                let lines = rendered_lines(&terminal);
                let rows = lines
                    .iter()
                    .filter_map(|line| {
                        let row = line.split('│').nth(1)?.trim_end();
                        COMMAND_MENU_ENTRIES
                            .iter()
                            .any(|entry| row.starts_with(entry.split(':').next().unwrap()))
                            .then_some(row)
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    rows,
                    vec![
                        "Check",
                        "Run",
                        "Clippy",
                        "Format",
                        "Doc",
                        "Update dependencies",
                        if newer {
                            "Update Rustrace  NEW"
                        } else {
                            "Update Rustrace"
                        },
                        if enabled {
                            "Automatic checks: On"
                        } else {
                            "Automatic checks: Off"
                        },
                        "Console",
                        "Test cases",
                        "Keybinds",
                        "Quit"
                    ],
                    "{}",
                    lines.join("\n")
                );
            }
        }
    }
}

#[test]
fn update_panel_goldens_cover_current_available_and_cargo() {
    for (version, managed, message) in [
        (
            None,
            true,
            "Latest release is unknown. Quit, then run: rustrace update --check",
        ),
        (
            None,
            false,
            "Latest release is unknown. Quit, then run: rustrace update --check",
        ),
        (
            Some(env!("CARGO_PKG_VERSION")),
            true,
            "Rustrace is up to date.",
        ),
        (
            Some(env!("CARGO_PKG_VERSION")),
            false,
            "Rustrace is up to date.",
        ),
        (Some("0.0.0"), false, "Rustrace is up to date."),
        (Some("0.0.0"), true, "Rustrace is up to date."),
        (
            Some("99.0.0"),
            true,
            "Rustrace v99.0.0 is available. Quit, then run: rustrace update",
        ),
        (
            Some("99.0.0"),
            false,
            "cargo +1.98.1 install --git https://github.com/baochunli/rustrace --tag v99.0.0 rustrace --locked --force",
        ),
    ] {
        let update = rustrace::update::UpdateState {
            last_success: version.map(|_| 0),
            latest: version.map(|version| rustrace::update::ReleaseIdentity {
                version: version.into(),
                tag: format!("v{version}"),
                commit: "a".repeat(40),
            }),
            ..Default::default()
        };
        let latest_label = version.unwrap_or("unknown");
        let checked = if version.is_some() {
            "2 hours ago"
        } else {
            "never"
        };
        let state = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_update_state(update.clone())
            .with_update_panel(managed, 7200);
        let terminal = render(80, 24, &state);
        let output = rendered_lines(&terminal).join("\n");
        assert!(
            output.contains(&format!("Installed: {}", env!("CARGO_PKG_VERSION"))),
            "{output}"
        );
        assert!(
            output.contains(&format!("Latest known: {latest_label}")),
            "{output}"
        );
        assert!(
            output.contains(&format!("Last checked: {checked}")),
            "{output}"
        );
        let rows = rendered_lines(&terminal)
            .iter()
            .filter_map(|line| line.split('│').nth(1))
            .map(|row| row.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(rows.contains(message), "{output}");
        assert!(output.contains("esc / enter close"), "{output}");
        assert!(!output.contains("vX.Y.Z"), "{output}");
        let body_rows = (9..16)
            .map(|y| {
                (4..76)
                    .map(|x| terminal.backend().buffer().cell((x, y)).unwrap().symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        let (first, second) = if message.starts_with("cargo ") {
            (
                "cargo +1.98.1 install --git https://github.com/baochunli/rustrace --tag",
                "v99.0.0 rustrace --locked --force",
            )
        } else {
            (message, "")
        };
        assert_eq!(
            body_rows,
            vec![
                format!("Installed: {}", env!("CARGO_PKG_VERSION")),
                format!("Latest known: {latest_label}"),
                format!("Last checked: {checked}"),
                "".into(),
                first.into(),
                second.into(),
                "".into()
            ],
            "{output}"
        );
        assert!(
            terminal
                .backend()
                .buffer()
                .cell((0, 0))
                .unwrap()
                .modifier
                .contains(Modifier::DIM)
        );

        assert_eq!(
            update.panel_lines(env!("CARGO_PKG_VERSION"), managed, 7200),
            vec![
                format!("Installed: {}", env!("CARGO_PKG_VERSION")),
                format!("Latest known: {latest_label}"),
                format!("Last checked: {checked}"),
                "".into(),
                message.into(),
            ]
        );
    }
    let update = rustrace::update::UpdateState::default();
    assert_eq!(
        update.panel_lines("0.1.0", true, 0),
        vec![
            "Installed: 0.1.0",
            "Latest known: unknown",
            "Last checked: never",
            "",
            "Latest release is unknown. Quit, then run: rustrace update --check"
        ]
    );
}

#[allow(dead_code)]
#[path = "support/test_home.rs"]
mod test_home;

#[test]
fn automatic_checks_persistence_is_reflected_in_testbackend_rows() {
    let home = test_home::TestHome::new(true);
    let path = home.root.join("state/rustrace/update-state.json");
    for enabled in [false, true] {
        rustrace::update::set_checks_enabled_at(&path, enabled).unwrap();
        let state = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_update_state(rustrace::update::UpdateState::load(&path))
            .with_command_menu(7);
        let output = rendered_lines(&render(80, 24, &state)).join("\n");
        assert!(
            output.contains(if enabled {
                "Automatic checks: On"
            } else {
                "Automatic checks: Off"
            }),
            "{output}"
        );
        assert_eq!(
            rustrace::update::UpdateState::load(&path).checks_enabled,
            enabled
        );
    }
}

#[test]
fn update_footer_goldens_and_dot_mouse_target() {
    for newer in [false, true] {
        let state = view_state(RecordingState::Active, JournalHealth::Healthy)
            .with_sidebar_width(18)
            .with_update_state(rustrace::update::UpdateState {
                latest: Some(rustrace::update::ReleaseIdentity {
                    version: if newer { "99.0.0" } else { "0.0.0" }.into(),
                    tag: if newer { "v99.0.0" } else { "v0.0.0" }.into(),
                    commit: "a".repeat(40),
                }),
                ..Default::default()
            });
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        let hits = MainView::new(&state, &editor(), &Viewport::default(), &[])
            .with_palette(Palette::terminal())
            .render_with_hit_map(area, &mut buffer);
        assert_eq!(hits.sidebar_menu.width, if newer { 7 } else { 5 });
        let footer = hits.sidebar_menu;
        let text = (footer.x..footer.right())
            .map(|x| buffer.cell((x, footer.y)).unwrap().symbol())
            .collect::<String>();
        assert_eq!(text, if newer { " ● menu" } else { " menu" });
        assert_eq!(
            buffer.cell((footer.x + 1, footer.y)).unwrap().fg,
            if newer {
                Palette::terminal().accent
            } else {
                Palette::terminal().overlay0
            }
        );
        let mut reducer = MouseState::default();
        for x in footer.x..footer.right() {
            assert_eq!(
                reduce_and_map(
                    &mut reducer,
                    mouse(MouseEventKind::Down(MouseButton::Left), x, footer.y),
                    1,
                    &hits,
                    ShellState::default()
                ),
                Some(ShellInput::OpenMenu)
            );
        }
        let output = buffer_lines(&buffer).join("\n");
        assert!(!output.contains("ERROR"));
        assert!(output.contains("F1 keybinds · F7 menu · F9 console"));
    }
}

#[test]
fn update_panel_mouse_is_modal_and_close_is_explicit() {
    let state =
        view_state(RecordingState::Active, JournalHealth::Healthy).with_update_panel(true, 0);
    let hits = render_hits("alpha", &state, &Viewport::default());
    let mut reducer = MouseState::default();
    for (position, expected) in [
        (hits.overlay_cancel, Some(ShellInput::CloseUpdateNotice)),
        (Rect::new(0, 0, 1, 1), None),
    ] {
        assert_eq!(
            reduce_and_map(
                &mut reducer,
                mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    position.x,
                    position.y
                ),
                1,
                &hits,
                ShellState {
                    modal: ShellModal::UpdateNotice
                }
            ),
            expected
        );
    }
}

#[test]
fn keybinds_overlay_lists_update_menu_rows() {
    let output = (0..KEYBIND_ROWS.len())
        .map(|scroll| {
            let state = view_state(RecordingState::Active, JournalHealth::Healthy)
                .with_keybinds_overlay(scroll);
            rendered_lines(&render(80, 24, &state)).join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n");
    for label in [
        "Update dependencies",
        "Update Rustrace",
        "Automatic checks: On/Off",
    ] {
        assert!(
            output.contains(label),
            "missing {label} in keybinds overlay"
        );
    }
}
