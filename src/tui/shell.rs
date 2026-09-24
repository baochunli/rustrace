use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use unicode_segmentation::UnicodeSegmentation;

use crate::display;

use super::{MIN_TERMINAL_HEIGHT, MIN_TERMINAL_WIDTH};

pub const SIDEBAR_WIDTH: u16 = 26;
pub const SIDEBAR_MIN_WIDTH: u16 = 18;
pub const SIDEBAR_MAX_WIDTH: u16 = 36;
const EDITOR_MIN_HEIGHT: u16 = 8;
const OUTPUT_MIN_HEIGHT: u16 = 4;
const OUTPUT_MAX_HEIGHT: u16 = 7;
const CONSOLE_MIN_HEIGHT: u16 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BottomPane {
    Output,
    Console,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShellLayout {
    pub sidebar: Rect,
    pub sidebar_divider: Rect,
    pub tab_bar: Rect,
    pub editor: Rect,
    pub editor_scrollbar: Rect,
    pub gap: Rect,
    pub bottom: Rect,
    pub mode_bar: Rect,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EditorSourceLayout {
    pub area: Rect,
    pub show_scrollbar: bool,
}

pub fn editor_source_layout(editor: Rect, line_count: usize) -> EditorSourceLayout {
    let show_scrollbar = line_count > usize::from(editor.height);
    EditorSourceLayout {
        area: Rect::new(
            editor.x,
            editor.y,
            editor.width.saturating_sub(u16::from(show_scrollbar)),
            editor.height,
        ),
        show_scrollbar,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellLayoutResult {
    Full(ShellLayout),
    TooSmall(Rect),
}

pub fn shell_layout(area: Rect, bottom: BottomPane, show_mode_bar: bool) -> ShellLayoutResult {
    shell_layout_with_bottom_height(area, bottom, show_mode_bar, None)
}

pub fn shell_layout_with_bottom_height(
    area: Rect,
    bottom: BottomPane,
    show_mode_bar: bool,
    resized_bottom_height: Option<u16>,
) -> ShellLayoutResult {
    shell_layout_with_sizes(area, bottom, show_mode_bar, resized_bottom_height, None)
}

pub fn shell_layout_with_sizes(
    area: Rect,
    bottom: BottomPane,
    _show_mode_bar: bool,
    resized_bottom_height: Option<u16>,
    resized_sidebar_width: Option<u16>,
) -> ShellLayoutResult {
    if area.width < MIN_TERMINAL_WIDTH || area.height < MIN_TERMINAL_HEIGHT {
        return ShellLayoutResult::TooSmall(area);
    }

    let sidebar_width = resized_sidebar_width
        .unwrap_or(SIDEBAR_WIDTH)
        .clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
        .min(area.width.saturating_sub(1));
    let sidebar = Rect::new(area.x, area.y, sidebar_width, area.height);
    let sidebar_divider = Rect::new(sidebar.right().saturating_sub(1), area.y, 1, area.height);
    let main = Rect::new(
        sidebar.right(),
        area.y,
        area.width.saturating_sub(sidebar.width),
        area.height,
    );
    let tab_bar = Rect::new(main.x, main.y, main.width, 1);
    let mode_height = 1;
    let mode_bar = Rect::new(
        main.x,
        main.bottom().saturating_sub(mode_height),
        main.width,
        mode_height,
    );
    let surface_bottom = mode_bar.y;

    let desired_bottom = resized_bottom_height.unwrap_or_else(|| match bottom {
        BottomPane::Output => (area.height / 4).clamp(OUTPUT_MIN_HEIGHT, OUTPUT_MAX_HEIGHT),
        // The console grows with the terminal; the editor minimum still caps it below.
        BottomPane::Console => (area.height * 2 / 5).max(CONSOLE_MIN_HEIGHT),
    });
    let available = surface_bottom.saturating_sub(tab_bar.bottom());
    let maximum_bottom = available.saturating_sub(EDITOR_MIN_HEIGHT + 1);
    let bottom_height = desired_bottom.clamp(OUTPUT_MIN_HEIGHT, maximum_bottom);
    let bottom_y = surface_bottom.saturating_sub(bottom_height);
    let gap = Rect::new(main.x, bottom_y.saturating_sub(1), main.width, 1);
    let editor = Rect::new(
        main.x,
        tab_bar.bottom(),
        main.width,
        gap.y.saturating_sub(tab_bar.bottom()),
    );
    let bottom = Rect::new(main.x, bottom_y, main.width, bottom_height);
    let editor_scrollbar = Rect::new(
        editor.right().saturating_sub(u16::from(editor.width > 0)),
        editor.y,
        u16::from(editor.width > 0),
        editor.height,
    );

    ShellLayoutResult::Full(ShellLayout {
        sidebar,
        sidebar_divider,
        tab_bar,
        editor,
        editor_scrollbar,
        gap,
        bottom,
        mode_bar,
    })
}

pub(super) fn bottom_height_for_divider_row(layout: ShellLayout, row: u16) -> u16 {
    let first_divider_row = layout.tab_bar.bottom().saturating_add(EDITOR_MIN_HEIGHT);
    let last_divider_row = layout
        .mode_bar
        .y
        .saturating_sub(OUTPUT_MIN_HEIGHT.saturating_add(1));
    let divider_row = row.clamp(first_divider_row, last_divider_row);
    layout
        .mode_bar
        .y
        .saturating_sub(divider_row.saturating_add(1))
}

pub(super) fn sidebar_width_for_divider_column(layout: ShellLayout, column: u16) -> u16 {
    column
        .saturating_sub(layout.sidebar.x)
        .saturating_add(1)
        .clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
}

pub const REPLAY_MIN_WIDTH: u16 = 80;
pub const REPLAY_MIN_HEIGHT: u16 = 24;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReplayShellLayout {
    pub sidebar: Rect,
    pub sidebar_divider: Rect,
    pub files: Rect,
    pub events: Rect,
    pub tab_bar: Rect,
    pub source: Rect,
    pub gap: Rect,
    pub details: Rect,
    pub mode_bar: Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayShellLayoutResult {
    Full(ReplayShellLayout),
    TooSmall(Rect),
}

pub fn replay_shell_layout(area: Rect) -> ReplayShellLayoutResult {
    if area.width < REPLAY_MIN_WIDTH || area.height < REPLAY_MIN_HEIGHT {
        return ReplayShellLayoutResult::TooSmall(area);
    }
    let sidebar_width = SIDEBAR_WIDTH.min(area.width.saturating_sub(1));
    let sidebar = Rect::new(area.x, area.y, sidebar_width, area.height);
    let sidebar_divider = Rect::new(sidebar.right().saturating_sub(1), area.y, 1, area.height);
    let files = Rect::new(sidebar.x, sidebar.y, sidebar.width.saturating_sub(1), 5);
    let events = Rect::new(
        sidebar.x,
        files.bottom(),
        sidebar.width.saturating_sub(1),
        sidebar.height.saturating_sub(files.height + 1),
    );
    let main = Rect::new(
        sidebar.right(),
        area.y,
        area.width.saturating_sub(sidebar.width),
        area.height,
    );
    let tab_bar = Rect::new(main.x, main.y, main.width, 1);
    let mode_bar = Rect::new(main.x, main.bottom().saturating_sub(1), main.width, 1);
    let details_height = (area.height / 3).clamp(10, 12);
    let details = Rect::new(
        main.x,
        mode_bar.y.saturating_sub(details_height),
        main.width,
        details_height,
    );
    let gap = Rect::new(main.x, details.y.saturating_sub(1), main.width, 1);
    let source = Rect::new(
        main.x,
        tab_bar.bottom(),
        main.width,
        gap.y.saturating_sub(tab_bar.bottom()),
    );
    ReplayShellLayoutResult::Full(ReplayShellLayout {
        sidebar,
        sidebar_divider,
        files,
        events,
        tab_bar,
        source,
        gap,
        details,
        mode_bar,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SidebarTarget {
    File(usize),
    New,
    Menu,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EditorHit {
    pub rect: Rect,
    pub top_line: usize,
    pub left_column: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HitMap {
    pub sidebar_rows: Vec<(Rect, SidebarTarget)>,
    pub sidebar_find: Rect,
    pub sidebar_files: Rect,
    pub sidebar_new: Rect,
    pub sidebar_menu: Rect,
    pub sidebar_split: Rect,
    pub tab_pills: Vec<(Rect, String)>,
    pub new_tab: Rect,
    pub tab_scroll_left: Rect,
    pub tab_scroll_right: Rect,
    pub editor: EditorHit,
    pub editor_diagnostic_rows: Vec<(Rect, usize)>,
    pub editor_scrollbar_track: Rect,
    pub editor_scrollbar_thumb: Rect,
    pub pane_split: Rect,
    pub output: Rect,
    pub output_rows: Vec<(Rect, usize)>,
    pub completion_popup: Rect,
    pub completion_rows: Vec<(Rect, usize)>,
    pub keybinds_scrollbar_track: Rect,
    pub keybinds_scrollbar_thumb: Rect,
    pub test_case_rows: Vec<(Rect, usize)>,
    pub test_case_scrollbar_track: Rect,
    pub test_case_scrollbar_thumb: Rect,
    pub console: Rect,
    pub context_menu_rows: Vec<(Rect, usize)>,
    pub editor_context_menu_rows: Vec<(Rect, usize, bool)>,
    pub files_context_menu_rows: Vec<(Rect, usize, bool)>,
    pub overlay_confirm: Rect,
    pub overlay_cancel: Rect,
    pub find_next: Rect,
    pub find_replace: Rect,
    pub find_replace_all: Rect,
    pub find_close: Rect,
    pub overlay: Rect,
}

pub fn set_style(buffer: &mut Buffer, area: Rect, style: Style) {
    let area = area.intersection(buffer.area);
    if !area.is_empty() {
        buffer.set_style(area, style);
    }
}

/// Writes one terminal-safe line and adds an ellipsis when it does not fit.
pub fn put_text(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    if width == 0 || y < buffer.area.y || y >= buffer.area.bottom() || x >= buffer.area.right() {
        return;
    }
    let width = width.min(buffer.area.right().saturating_sub(x));
    let text = display::label(text, display::MAX_OUTPUT_BYTES);
    let text = truncate_to_width(&text, usize::from(width));
    buffer.set_stringn(x, y, text, usize::from(width), style);
}

pub fn put_right_text(buffer: &mut Buffer, area: Rect, y: u16, text: &str, style: Style) {
    if area.width == 0 {
        return;
    }
    let text = display::label(text, display::MAX_OUTPUT_BYTES);
    let text = truncate_from_left(&text, usize::from(area.width));
    let width = display_width(&text).min(usize::from(area.width)) as u16;
    put_text(
        buffer,
        area.right().saturating_sub(width),
        y,
        width,
        &text,
        style,
    );
}

pub fn display_width(text: &str) -> usize {
    text.graphemes(true).fold(0, |column, grapheme| {
        column + display::grapheme_width(grapheme, column)
    })
}

pub fn truncate_to_width(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let target = width.saturating_sub(1);
    let mut value = String::new();
    let mut columns: usize = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = display::grapheme_width(grapheme, columns);
        if columns.saturating_add(grapheme_width) > target {
            break;
        }
        value.push_str(grapheme);
        columns += grapheme_width;
    }
    value.push('…');
    value
}

pub fn truncate_from_left(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let target = width.saturating_sub(1);
    let graphemes = text.graphemes(true).collect::<Vec<_>>();
    let mut kept = Vec::new();
    let mut columns: usize = 0;
    for grapheme in graphemes.into_iter().rev() {
        let grapheme_width = display::grapheme_width(grapheme, 0);
        if columns.saturating_add(grapheme_width) > target {
            break;
        }
        kept.push(grapheme);
        columns += grapheme_width;
    }
    kept.reverse();
    format!("…{}", kept.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full(area: Rect, bottom: BottomPane, mode: bool) -> ShellLayout {
        let ShellLayoutResult::Full(layout) = shell_layout(area, bottom, mode) else {
            panic!("expected full layout");
        };
        layout
    }

    #[test]
    fn student_layout_arithmetic_at_minimum_and_default_sizes() {
        for area in [Rect::new(0, 0, 60, 15), Rect::new(7, 3, 80, 24)] {
            let layout = full(area, BottomPane::Output, false);
            assert_eq!(layout.sidebar.width, SIDEBAR_WIDTH);
            assert_eq!(layout.sidebar, Rect::new(area.x, area.y, 26, area.height));
            assert_eq!(layout.sidebar_divider.x, area.x + 25);
            assert_eq!(layout.sidebar_divider.height, area.height);
            assert_eq!(layout.tab_bar.y, area.y);
            assert_eq!(layout.editor.y, layout.tab_bar.bottom());
            assert_eq!(layout.gap.y, layout.editor.bottom());
            assert_eq!(layout.bottom.y, layout.gap.bottom());
            assert_eq!(layout.bottom.bottom(), layout.mode_bar.y);
            assert_eq!(
                layout.mode_bar,
                Rect::new(
                    area.x + SIDEBAR_WIDTH,
                    area.bottom() - 1,
                    area.width - SIDEBAR_WIDTH,
                    1,
                )
            );
        }
    }

    #[test]
    fn console_and_mode_bar_reserve_their_declared_rows() {
        let area = Rect::new(0, 0, 120, 40);
        let output = full(area, BottomPane::Output, true);
        let console = full(area, BottomPane::Console, true);
        assert_eq!(output.bottom.height, 7);
        assert_eq!(console.bottom.height, 16);
        assert_eq!(console.mode_bar, Rect::new(26, 39, 94, 1));
        assert_eq!(console.gap.height, 1);
        assert_eq!(console.editor.bottom(), console.gap.y);
        // The console grows with the terminal and never squeezes the editor minimum.
        let tall = full(Rect::new(0, 0, 200, 80), BottomPane::Console, true);
        assert_eq!(tall.bottom.height, 32);
        let short = full(Rect::new(0, 0, 80, 24), BottomPane::Console, true);
        assert_eq!(short.bottom.height, 9);
        assert!(short.editor.height >= EDITOR_MIN_HEIGHT);
    }

    #[test]
    fn test_cases_modal_preserves_the_workspace_and_output_layout() {
        let area = Rect::new(0, 0, 80, 24);
        let ShellLayoutResult::Full(layout) = shell_layout(area, BottomPane::Output, true) else {
            panic!("expected full layout");
        };
        assert_eq!(layout.editor, Rect::new(26, 1, 54, 15));
        assert_eq!(layout.mode_bar, Rect::new(26, 23, 54, 1));
        assert_eq!(layout.bottom, Rect::new(26, 17, 54, 6));
        assert_eq!(layout.gap, Rect::new(26, 16, 54, 1));
    }

    #[test]
    fn unsupported_student_and_replay_sizes_keep_the_fallback() {
        assert_eq!(
            shell_layout(Rect::new(0, 0, 59, 15), BottomPane::Output, false,),
            ShellLayoutResult::TooSmall(Rect::new(0, 0, 59, 15))
        );
        assert_eq!(
            replay_shell_layout(Rect::new(0, 0, 79, 24)),
            ReplayShellLayoutResult::TooSmall(Rect::new(0, 0, 79, 24))
        );
    }

    #[test]
    fn replay_layout_has_sidebar_tabs_gap_details_and_permanent_mode_bar() {
        let area = Rect::new(4, 2, 120, 40);
        let ReplayShellLayoutResult::Full(layout) = replay_shell_layout(area) else {
            panic!("expected replay layout");
        };
        assert_eq!(layout.sidebar, Rect::new(4, 2, 26, 40));
        assert_eq!(layout.files.height, 5);
        assert_eq!(layout.events.bottom(), area.bottom() - 1);
        assert_eq!(layout.source.bottom(), layout.gap.y);
        assert_eq!(layout.details.y, layout.gap.bottom());
        assert_eq!(layout.details.bottom(), layout.mode_bar.y);
        assert_eq!(layout.mode_bar, Rect::new(30, 41, 94, 1));
    }

    #[test]
    fn buffer_helpers_escape_controls_and_truncate_by_display_width() {
        assert_eq!(truncate_to_width("東京abc", 5), "東京…");
        assert_eq!(truncate_from_left("東京abc", 5), "…abc");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        put_text(&mut buffer, 0, 0, 8, "x\u{1b}[2J\n", Style::default());
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_eq!(rendered, "x\\u{1b}…");
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\n'));
    }
}
