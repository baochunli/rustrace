//! Bounded derived display for source, labels, errors and tool bytes.
//!
//! Input is never trusted terminal framing. SGR sequences set styles only when
//! they match the small allowlist. Unsupported controls remain visible text.
use std::fmt::{self, Write as _};

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
pub use rustrace_editor::display::{MAX_GRAPHEME_BYTES, grapheme, grapheme_width, must_escape};
use unicode_segmentation::UnicodeSegmentation;

pub const MAX_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 32 * 1024;
pub const MAX_SPANS: usize = 1024;
pub const MAX_LINES: usize = 128;
const MAX_SGR_BYTES: usize = 64;
const MAX_SGR_PARAMS: usize = 16;
pub const TRUNCATED: &str = "[display truncated]";

/// Per-call display budgets; requests are clamped to the fixed ceilings above.
/// These never increase source, capture, event or evidence limits.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub spans: usize,
    pub lines: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            input_bytes: MAX_INPUT_BYTES,
            output_bytes: MAX_OUTPUT_BYTES,
            spans: MAX_SPANS,
            lines: MAX_LINES,
        }
    }
}

#[derive(Debug)]
pub struct DisplayText {
    pub text: Text<'static>,
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub spans: usize,
    pub truncated: bool,
}

/// Plain single-line labels: no ANSI interpretation, including safe SGR.
pub fn label(value: &str, max_bytes: usize) -> String {
    parse(
        value.as_bytes(),
        Limits {
            output_bytes: max_bytes,
            lines: 1,
            ..Limits::default()
        },
        false,
        false,
    )
    .text
    .to_string()
}

/// Bound formatting before it can allocate an arbitrarily large error/path.
pub fn label_fmt(value: fmt::Arguments<'_>, max_bytes: usize) -> String {
    let mut capture = BoundedFormat::new(MAX_INPUT_BYTES);
    let _ = capture.write_fmt(value);
    let mut result = label(&capture.text, max_bytes);
    if capture.truncated && !result.ends_with(TRUNCATED) {
        append_marker(&mut result, max_bytes.min(MAX_OUTPUT_BYTES));
    }
    result
}

/// Parse one captured output record. LF/CRLF are display line breaks; tabs use
/// four-cell stops. No escape or UTF-8 state is carried across separate calls.
pub fn output(bytes: &[u8], limits: Limits) -> DisplayText {
    parse(bytes, limits, true, true)
}

/// Plain multiline preview (e.g. serialized inspection), with no SGR styles.
pub fn plain(bytes: &[u8], limits: Limits) -> DisplayText {
    parse(bytes, limits, false, true)
}

/// Bounded human inspection, not a JSON export or an evidence serialization API.
pub fn json_preview(value: &impl serde::Serialize) -> Result<String, serde_json::Error> {
    struct Capture {
        bytes: Vec<u8>,
        truncated: bool,
    }
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let available = (16 * 1024_usize).saturating_sub(self.bytes.len());
            if bytes.len() > available {
                self.bytes.extend_from_slice(&bytes[..available]);
                self.truncated = true;
                return Err(std::io::Error::other("display preview limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut capture = Capture {
        bytes: Vec::new(),
        truncated: false,
    };
    let result = serde_json::to_writer_pretty(&mut capture, value);
    if capture.truncated {
        capture.bytes.extend_from_slice(b"\n[display truncated]");
    } else {
        result?;
    }
    Ok(plain(&capture.bytes, Limits::default()).text.to_string())
}

struct BoundedFormat {
    text: String,
    limit: usize,
    truncated: bool,
}

impl BoundedFormat {
    fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            limit,
            truncated: false,
        }
    }
}

impl fmt::Write for BoundedFormat {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let available = self.limit.saturating_sub(self.text.len());
        let end = value.floor_char_boundary(value.len().min(available));
        self.text.push_str(&value[..end]);
        if end < value.len() {
            self.truncated = true;
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

fn append_marker(value: &mut String, limit: usize) {
    let marker = &TRUNCATED[..TRUNCATED.len().min(limit)];
    let end = value.floor_char_boundary(value.len().min(limit - marker.len()));
    value.truncate(end);
    value.push_str(marker);
}

fn parse(bytes: &[u8], limits: Limits, sgr: bool, multiline: bool) -> DisplayText {
    let input_limit = limits.input_bytes.min(MAX_INPUT_BYTES);
    let output_limit = limits.output_bytes.min(MAX_OUTPUT_BYTES);
    let span_limit = limits.spans.min(MAX_SPANS);
    let line_limit = limits.lines.min(MAX_LINES);
    let content_limit = output_limit;
    let mut truncated = bytes.len() > input_limit;
    let bytes = &bytes[..bytes.len().min(input_limit)];
    let mut logical = String::new();
    let mut styles = vec![(0, Style::default())];
    let mut style = Style::default();
    let mut offset = 0;
    // Decode once, bounded before each append. Never allocate from raw lengths.
    while offset < bytes.len() && span_limit > 0 && line_limit > 0 {
        if sgr && let Some((length, next)) = parse_sgr(&bytes[offset..], style) {
            if next != style {
                if styles.last().is_some_and(|(at, _)| *at == logical.len()) {
                    styles.last_mut().expect("initial style").1 = next;
                } else if styles.len() < span_limit {
                    styles.push((logical.len(), next));
                } else {
                    truncated = true;
                    break;
                }
                style = next;
            }
            offset += length;
            continue;
        }
        let (character, length) = decode(&bytes[offset..]);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let invalid = [
            b'\\',
            b'x',
            HEX[usize::from(bytes[offset] >> 4)],
            HEX[usize::from(bytes[offset] & 15)],
        ];
        let mut encoded = [0; 4];
        let value = match character {
            Some(character) => character.encode_utf8(&mut encoded),
            None => std::str::from_utf8(&invalid).expect("ASCII hex escape"),
        };
        if logical.len() + value.len() > content_limit {
            truncated = true;
            break;
        }
        logical.push_str(value);
        offset += length;
    }
    truncated |= offset < bytes.len();

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut line = Line::default();
    let mut output_bytes = 0;
    let mut spans = 0;
    let mut column = 0;
    let mut style_index = 0;
    for (at, cluster) in logical.grapheme_indices(true) {
        while style_index + 1 < styles.len() && styles[style_index + 1].0 <= at {
            style_index += 1;
        }
        if multiline && matches!(cluster, "\n" | "\r\n") {
            if lines.len() + 1 >= line_limit || output_bytes == content_limit {
                truncated = true;
                break;
            }
            lines.push(std::mem::take(&mut line));
            output_bytes += 1;
            column = 0;
            continue;
        }
        let (symbol, width) = if !multiline && cluster == "\t" {
            (std::borrow::Cow::Borrowed("\\t"), 2)
        } else {
            grapheme(cluster, column)
        };
        let style = styles[style_index].1;
        let symbol_bytes = symbol.len();
        let new_span = line.spans.last().is_none_or(|span| span.style != style);
        if output_bytes + symbol.len() > content_limit
            || (new_span && spans >= span_limit)
            || line_limit == 0
        {
            truncated = true;
            break;
        }
        if new_span {
            line.spans.push(Span::styled(symbol.into_owned(), style));
            spans += 1;
        } else {
            line.spans
                .last_mut()
                .expect("existing span")
                .content
                .to_mut()
                .push_str(&symbol);
        }
        output_bytes += symbol_bytes;
        column += width;
    }
    if line_limit > 0 {
        lines.push(line);
    }
    if truncated && span_limit > 0 && line_limit > 0 && output_limit > 0 {
        append_truncation(
            &mut lines,
            &mut output_bytes,
            &mut spans,
            output_limit,
            span_limit,
        );
    }
    DisplayText {
        text: Text::from(lines),
        input_bytes: offset,
        output_bytes,
        spans,
        truncated,
    }
}

fn append_truncation(
    lines: &mut Vec<Line<'static>>,
    bytes: &mut usize,
    spans: &mut usize,
    byte_limit: usize,
    span_limit: usize,
) {
    let marker = &TRUNCATED[..TRUNCATED.len().min(byte_limit)];
    let keep = byte_limit - marker.len();
    loop {
        let line = lines.last_mut().expect("at least one display line");
        let can_extend = line
            .spans
            .last()
            .is_some_and(|span| span.style == Style::default() || *spans == span_limit);
        if *bytes <= keep && (*spans < span_limit || can_extend) {
            if can_extend {
                line.spans
                    .last_mut()
                    .expect("existing span")
                    .content
                    .to_mut()
                    .push_str(marker);
            } else {
                line.spans.push(Span::raw(marker));
                *spans += 1;
            }
            *bytes += marker.len();
            break;
        }
        if let Some(span) = line.spans.last_mut() {
            let desired = span
                .content
                .len()
                .saturating_sub(bytes.saturating_sub(keep));
            // Truncate only on an original safe-display grapheme boundary.
            let end = if desired == span.content.len() {
                0
            } else {
                span.content
                    .grapheme_indices(true)
                    .map(|(at, _)| at)
                    .take_while(|at| *at <= desired)
                    .last()
                    .unwrap_or(0)
            };
            *bytes -= span.content.len() - end;
            span.content.to_mut().truncate(end);
            if end == 0 {
                line.spans.pop();
                *spans -= 1;
            }
        } else {
            lines.pop();
            *bytes -= 1; // Removed an empty trailing line and its separator.
        }
    }
}

fn decode(bytes: &[u8]) -> (Option<char>, usize) {
    let length = match bytes[0] {
        0..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return (None, 1),
    };
    match bytes
        .get(..length)
        .and_then(|value| std::str::from_utf8(value).ok())
    {
        Some(value) => (value.chars().next(), length),
        None => (None, 1),
    }
}

fn parse_sgr(bytes: &[u8], current: Style) -> Option<(usize, Style)> {
    if !bytes.starts_with(b"\x1b[") {
        return None;
    }
    let mut params = [0_u16; MAX_SGR_PARAMS];
    let mut index = 0;
    for (offset, byte) in bytes
        .iter()
        .copied()
        .take(MAX_SGR_BYTES)
        .enumerate()
        .skip(2)
    {
        match byte {
            b'0'..=b'9' => {
                params[index] = params[index]
                    .checked_mul(10)?
                    .checked_add(u16::from(byte - b'0'))?;
                if params[index] > 255 {
                    return None;
                }
            }
            b';' if index + 1 < params.len() => index += 1,
            b'm' => return Some((offset + 1, apply_sgr(current, &params[..=index])?)),
            _ => return None,
        }
    }
    None
}

fn apply_sgr(mut style: Style, params: &[u16]) -> Option<Style> {
    let mut index = 0;
    while index < params.len() {
        match params[index] {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            30..=37 | 90..=97 => style = style.fg(ansi_color(params[index])),
            40..=47 | 100..=107 => style = style.bg(ansi_color(params[index] - 10)),
            39 => style = style.fg(Color::Reset),
            49 => style = style.bg(Color::Reset),
            38 | 48 => {
                let foreground = params[index] == 38;
                let color = match *params.get(index + 1)? {
                    5 => {
                        let color = Color::Indexed(*params.get(index + 2)? as u8);
                        index += 2;
                        color
                    }
                    2 => {
                        let color = Color::Rgb(
                            *params.get(index + 2)? as u8,
                            *params.get(index + 3)? as u8,
                            *params.get(index + 4)? as u8,
                        );
                        index += 4;
                        color
                    }
                    _ => return None,
                };
                style = if foreground {
                    style.fg(color)
                } else {
                    style.bg(color)
                };
            }
            _ => return None,
        }
        index += 1;
    }
    Some(style)
}

fn ansi_color(code: u16) -> Color {
    const COLORS: [Color; 16] = [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ];
    COLORS[if code >= 90 {
        (code - 90 + 8) as usize
    } else {
        (code - 30) as usize
    }]
}
