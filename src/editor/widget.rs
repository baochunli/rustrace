use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use unicode_segmentation::UnicodeSegmentation;

use crate::display;
use crate::tui::theme::Palette;

use super::buffer::EditorBuffer;
use super::{EditorEffects, HighlightKind, HighlightSpan, Viewport};

const MAX_HIGHLIGHT_SPANS: usize = 262_144;
const MAX_OVERLAPPING_SPANS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticMarkerKind {
    Error,
    Warning,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiagnosticLineMarker {
    pub line: usize,
    pub kind: DiagnosticMarkerKind,
    pub selected: bool,
    pub live: bool,
    diagnostic_index: Option<usize>,
}

impl DiagnosticLineMarker {
    pub const fn new(line: usize, kind: DiagnosticMarkerKind, selected: bool) -> Self {
        Self {
            line,
            kind,
            selected,
            live: false,
            diagnostic_index: None,
        }
    }

    pub const fn live(line: usize, kind: DiagnosticMarkerKind, selected: bool) -> Self {
        Self {
            line,
            kind,
            selected,
            live: true,
            diagnostic_index: None,
        }
    }

    pub const fn compiler(
        line: usize,
        kind: DiagnosticMarkerKind,
        selected: bool,
        diagnostic_index: usize,
    ) -> Self {
        Self {
            line,
            kind,
            selected,
            live: false,
            diagnostic_index: Some(diagnostic_index),
        }
    }

    pub const fn diagnostic_index(&self) -> Option<usize> {
        self.diagnostic_index
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveDiagnosticSpan {
    pub start_byte: u64,
    pub end_byte: u64,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: DiagnosticMarkerKind,
    pub message: String,
}

pub struct EditorWidget<'a, S>
where
    S: EditorEffects,
{
    editor: &'a EditorBuffer<S>,
    viewport: &'a Viewport,
    highlights: &'a [HighlightSpan],
    diagnostic_markers: &'a [DiagnosticLineMarker],
    live_diagnostics: &'a [LiveDiagnosticSpan],
    palette: Option<&'a Palette>,
    cursor_visible: bool,
}

impl<'a, S> EditorWidget<'a, S>
where
    S: EditorEffects,
{
    pub fn new(
        editor: &'a EditorBuffer<S>,
        viewport: &'a Viewport,
        highlights: &'a [HighlightSpan],
    ) -> Self {
        Self {
            editor,
            viewport,
            highlights,
            diagnostic_markers: &[],
            live_diagnostics: &[],
            palette: None,
            cursor_visible: true,
        }
    }

    pub fn with_diagnostic_markers(mut self, markers: &'a [DiagnosticLineMarker]) -> Self {
        self.diagnostic_markers = markers;
        self
    }

    pub fn with_live_diagnostics(mut self, diagnostics: &'a [LiveDiagnosticSpan]) -> Self {
        self.live_diagnostics = diagnostics;
        self
    }

    pub fn with_palette(mut self, palette: &'a Palette) -> Self {
        self.palette = Some(palette);
        self
    }

    pub fn with_cursor_visible(mut self, visible: bool) -> Self {
        self.cursor_visible = visible;
        self
    }
}

impl<S> Widget for EditorWidget<'_, S>
where
    S: EditorEffects,
{
    fn render(self, area: Rect, buffer: &mut Buffer) {
        buffer.set_style(
            area,
            Style::default().fg(self.palette.map_or(Color::White, |palette| palette.text)),
        );
        // The production source cap also bounds line materialization and Unicode
        // segmentation here. Generic editor/spike callers cannot bypass it.
        if self.editor.len_bytes() > rustrace_workspace::hash::MAX_WORKSPACE_FILE_BYTES as usize {
            if area.width > 0 && area.height > 0 {
                buffer.set_stringn(
                    area.x,
                    area.y,
                    "[source display limit]",
                    area.width as usize,
                    Style::default(),
                );
            }
            return;
        }
        let source_area = area;
        let selection = self.editor.selection();
        let cursor = self.editor.cursor().char_index;
        let matching_bracket = self
            .editor
            .matching_bracket_byte()
            .map(|byte| byte as usize);
        // Captures and rendered graphemes are ordered by byte offset. Sweep
        // once, keeping only overlapping spans, instead of searching the whole
        // document for every cell. Last overlapping capture keeps precedence.
        let highlights = if self.highlights.len() <= MAX_HIGHLIGHT_SPANS {
            self.highlights
        } else {
            &[]
        };
        let mut upcoming = highlights.iter().peekable();
        let mut active_spans: Vec<&HighlightSpan> = Vec::new();
        let mut highlighting = true;

        for screen_row in 0..source_area.height as usize {
            let line_index = self.viewport.top_line() + screen_row;
            if line_index >= self.editor.line_count() {
                break;
            }

            if let Some(background) =
                diagnostic_line_background(self.diagnostic_markers, line_index, self.palette)
            {
                buffer.set_style(
                    Rect::new(
                        source_area.x,
                        source_area.y + screen_row as u16,
                        source_area.width,
                        1,
                    ),
                    Style::default().bg(background),
                );
            }

            let line = self.editor.line_text(line_index);
            let line_char_start = self.editor.line_start_char(line_index);
            let line_byte_start = self.editor.line_start_byte(line_index);
            let mut display_column = 0;
            let mut local_char = 0;

            for (local_byte, grapheme) in line.grapheme_indices(true) {
                let width = display::grapheme_width(grapheme, display_column);
                let next_column = display_column + width;
                let grapheme_chars = grapheme.chars().count();
                if next_column <= self.viewport.left_column() {
                    display_column = next_column;
                    local_char += grapheme_chars;
                    continue;
                }
                if display_column < self.viewport.left_column() {
                    display_column = next_column;
                    local_char += grapheme_chars;
                    continue;
                }

                let screen_column = display_column - self.viewport.left_column();
                if screen_column >= source_area.width as usize {
                    break;
                }
                let global_char = line_char_start + local_char;
                let global_byte = line_byte_start + local_byte;
                if highlighting {
                    active_spans.retain(|span| span.byte_range.end > global_byte);
                    while let Some(span) =
                        upcoming.next_if(|span| span.byte_range.start <= global_byte)
                    {
                        if span.byte_range.end > global_byte {
                            if active_spans.len() == MAX_OVERLAPPING_SPANS {
                                // Derived syntax may fall back to plain text. It
                                // must not multiply work by unbounded overlaps.
                                active_spans.clear();
                                highlighting = false;
                                break;
                            }
                            active_spans.push(span);
                        }
                    }
                }
                let mut style =
                    highlight_style(active_spans.last().map(|span| span.kind), self.palette);
                if let Some(kind) = live_diagnostic_kind(
                    self.live_diagnostics,
                    global_byte,
                    global_byte.saturating_add(grapheme.len()),
                ) {
                    style = style
                        .fg(diagnostic_color(kind, self.palette))
                        .add_modifier(Modifier::UNDERLINED);
                }
                if matching_bracket == Some(global_byte) {
                    style = style
                        .fg(self.palette.map_or(Color::Blue, |palette| palette.accent))
                        .add_modifier(Modifier::BOLD);
                }
                if selection
                    .as_ref()
                    .is_some_and(|range| range.contains(&global_char))
                {
                    style = style.bg(self.palette.map_or(Color::Blue, |palette| {
                        if palette.selection_bg == Color::Reset {
                            palette.active_row_bg
                        } else {
                            palette.selection_bg
                        }
                    }));
                }
                if self.cursor_visible && global_char == cursor {
                    style = style.add_modifier(Modifier::REVERSED);
                }

                let x = source_area.x + screen_column as u16;
                let y = source_area.y + screen_row as u16;
                let remaining = source_area.right().saturating_sub(x) as usize;
                let (symbol, _) = display::grapheme(grapheme, display_column);
                let (written_x, _) = buffer.set_stringn(x, y, symbol.as_ref(), remaining, style);
                if written_x == x && self.cursor_visible && global_char == cursor {
                    // A two-cell glyph cannot fit a one-cell pane. Keep a
                    // visible caret without writing half a glyph outside it.
                    buffer.set_stringn(x, y, " ", remaining, style);
                }

                display_column = next_column;
                local_char += grapheme_chars;
            }

            let line_end = line_char_start + line.chars().count();
            let cursor_column = display_column.saturating_sub(self.viewport.left_column());
            if self.cursor_visible
                && cursor == line_end
                && cursor_column < source_area.width as usize
            {
                let x = source_area.x + cursor_column as u16;
                let y = source_area.y + screen_row as u16;
                buffer.set_string(x, y, " ", Style::default().add_modifier(Modifier::REVERSED));
            }

            render_live_diagnostic_inline(
                source_area,
                buffer,
                screen_row,
                line_index,
                display_column,
                self.viewport.left_column(),
                self.live_diagnostics,
                self.palette,
            );
        }
    }
}

fn live_diagnostic_kind(
    diagnostics: &[LiveDiagnosticSpan],
    start_byte: usize,
    end_byte: usize,
) -> Option<DiagnosticMarkerKind> {
    diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.start_byte < end_byte as u64 && diagnostic.end_byte > start_byte as u64
        })
        .map(|diagnostic| diagnostic.kind)
        .min_by_key(|kind| diagnostic_priority(*kind))
}

fn diagnostic_priority(kind: DiagnosticMarkerKind) -> u8 {
    match kind {
        DiagnosticMarkerKind::Error => 0,
        DiagnosticMarkerKind::Warning => 1,
        DiagnosticMarkerKind::Other => 2,
    }
}

fn diagnostic_color(kind: DiagnosticMarkerKind, palette: Option<&Palette>) -> Color {
    match kind {
        DiagnosticMarkerKind::Error => palette.map_or(Color::LightRed, |palette| palette.red),
        DiagnosticMarkerKind::Warning | DiagnosticMarkerKind::Other => {
            palette.map_or(Color::Yellow, |palette| palette.yellow)
        }
    }
}

fn diagnostic_line_background(
    markers: &[DiagnosticLineMarker],
    line: usize,
    palette: Option<&Palette>,
) -> Option<Color> {
    match markers
        .iter()
        .filter(|marker| !marker.live && marker.line == line)
        .map(|marker| marker.kind)
        .min_by_key(|kind| diagnostic_priority(*kind))?
    {
        DiagnosticMarkerKind::Error => {
            Some(palette.map_or(Color::Red, Palette::diagnostic_error_bg))
        }
        DiagnosticMarkerKind::Warning => {
            Some(palette.map_or(Color::Yellow, Palette::diagnostic_warning_bg))
        }
        DiagnosticMarkerKind::Other => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_live_diagnostic_inline(
    area: Rect,
    buffer: &mut Buffer,
    screen_row: usize,
    line_index: usize,
    line_width: usize,
    left_column: usize,
    diagnostics: &[LiveDiagnosticSpan],
    palette: Option<&Palette>,
) {
    let Some(diagnostic) = diagnostics
        .iter()
        .filter(|diagnostic| (diagnostic.start_line..=diagnostic.end_line).contains(&line_index))
        .min_by_key(|diagnostic| diagnostic_priority(diagnostic.kind))
    else {
        return;
    };
    let visible_end = line_width.saturating_sub(left_column);
    if visible_end >= usize::from(area.width) {
        return;
    }
    let available = usize::from(area.width).saturating_sub(visible_end);
    if available < 8 {
        return;
    }
    let message = display::label(
        &diagnostic.message,
        crate::language_service::MAX_LIVE_DIAGNOSTIC_MESSAGE_BYTES,
    );
    let text = crate::tui::shell::truncate_to_width(&message, available - 2);
    if text.is_empty() {
        return;
    }
    let style = Style::default()
        .fg(diagnostic_color(diagnostic.kind, palette))
        .add_modifier(Modifier::DIM);
    buffer.set_stringn(
        area.x + visible_end as u16 + 2,
        area.y + screen_row as u16,
        text,
        available - 2,
        style,
    );
}

fn highlight_style(kind: Option<HighlightKind>, palette: Option<&Palette>) -> Style {
    let color = |fallback, token: fn(&Palette) -> Color| palette.map_or(fallback, token);
    match kind {
        Some(HighlightKind::Keyword) => Style::default()
            .fg(color(Color::Magenta, |palette| palette.mauve))
            .add_modifier(Modifier::BOLD),
        Some(HighlightKind::Function) => {
            Style::default().fg(color(Color::Cyan, |palette| palette.teal))
        }
        Some(HighlightKind::String) => {
            Style::default().fg(color(Color::Green, |palette| palette.green))
        }
        Some(HighlightKind::Comment) => {
            Style::default().fg(color(Color::DarkGray, |palette| palette.overlay0))
        }
        Some(HighlightKind::Type) => {
            Style::default().fg(color(Color::Yellow, |palette| palette.blue))
        }
        Some(HighlightKind::Constant) => {
            Style::default().fg(color(Color::LightBlue, |palette| palette.peach))
        }
        Some(HighlightKind::Other) | None => {
            Style::default().fg(color(Color::White, |palette| palette.text))
        }
    }
}
