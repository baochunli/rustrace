//! Exact coordinates in immutable text, separate from terminal viewport coordinates.
//!
//! Bytes and scalars address every UTF-8 boundary (including between CR and LF).
//! UTF-16 positions use LSP's CRLF/CR/LF lines and exclude terminators. Conversion
//! is strict: unlike LSP's general past-end character clamping, invalid edit
//! coordinates are rejected. Only `utf-16` is supported by the client.
//!
//! Visual positions use Ropey's editor lines and the shared safe-display width
//! policy, including four-cell tab stops and bounded visible expansions.
//! Interior original graphemes and derived display cells are rejected.

use std::borrow::Cow;
use std::fmt;
use std::ops::Range;

use ropey::Rope;
use rustrace_model::DocumentId;
use unicode_segmentation::UnicodeSegmentation;

pub use crate::display::grapheme_width;

/// The sole advertised LSP encoding; omitted server negotiation has this default.
pub const LSP_POSITION_ENCODING: &str = "utf-16";
const LSP_MAX_INTEGER: usize = i32::MAX as usize;

/// Absolute UTF-8 byte offset, in the same units as transaction/selection offsets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteOffset(pub u64);

/// Absolute Rust Unicode scalar index, exactly Ropey's character-index unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScalarIndex(pub usize);

/// Zero-based LSP line and UTF-16 code-unit offset. Not a visual column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Utf16Position {
    pub line: u32,
    pub character: u32,
}

/// Zero-based editor (Rope) line and terminal cell column, before scrolling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VisualPosition {
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PositionError {
    ByteOutOfBounds,
    ScalarOutOfBounds,
    LineOutOfBounds,
    ColumnOutOfBounds,
    NotUtf8Boundary,
    NotUtf16Boundary,
    InsideLineEnding,
    NotGraphemeBoundary,
    NonAddressableVisualColumn,
    LspIntegerOverflow,
    StaleVersion { expected: u64, current: u64 },
}

impl fmt::Display for PositionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::ByteOutOfBounds => "byte offset is outside the document",
            Self::ScalarOutOfBounds => "scalar index is outside the document",
            Self::LineOutOfBounds => "line is outside the document",
            Self::ColumnOutOfBounds => "column is past the end of the line",
            Self::NotUtf8Boundary => "byte offset splits a UTF-8 code point",
            Self::NotUtf16Boundary => "UTF-16 offset splits a surrogate pair",
            Self::InsideLineEnding => "position is inside a line terminator",
            Self::NotGraphemeBoundary => "position splits a displayed grapheme",
            Self::NonAddressableVisualColumn => "visual position has no exact text boundary",
            Self::LspIntegerOverflow => "position exceeds the LSP uinteger range",
            Self::StaleVersion { expected, current } => {
                return write!(
                    f,
                    "requested text version {expected}; current version is {current}"
                );
            }
        };
        f.write_str(message)
    }
}

impl std::error::Error for PositionError {}

/// A borrowed current document view from [`crate::EditorBuffer::positions`].
///
/// Creating the view checks the caller's expected version. Its borrow prevents
/// edits while it is used. Retained request coordinates must be checked again
/// against the current buffer; session/document replacement and server restart
/// checks belong to the request owner, not to numeric position conversion.
#[derive(Debug)]
pub struct Positions<'a> {
    rope: &'a Rope,
    document_id: &'a DocumentId,
    version: u64,
}

impl<'a> Positions<'a> {
    pub(crate) fn new(rope: &'a Rope, document_id: &'a DocumentId, version: u64) -> Self {
        Self {
            rope,
            document_id,
            version,
        }
    }

    pub fn document_id(&self) -> &DocumentId {
        self.document_id
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// O(log N), no text allocation. Ropey's rounding is explicitly rejected.
    pub fn byte_to_scalar(&self, byte: ByteOffset) -> Result<ScalarIndex, PositionError> {
        let byte = bounded_byte(byte, self.rope.len_bytes())?;
        let scalar = self.rope.byte_to_char(byte);
        if self.rope.char_to_byte(scalar) != byte {
            return Err(PositionError::NotUtf8Boundary);
        }
        Ok(ScalarIndex(scalar))
    }

    /// Return the editor line containing an exact UTF-8 byte boundary.
    pub fn byte_to_line(&self, byte: ByteOffset) -> Result<usize, PositionError> {
        let scalar = self.byte_to_scalar(byte)?;
        Ok(self.rope.char_to_line(scalar.0))
    }

    /// O(log N), no text allocation. EOF is a valid scalar boundary.
    pub fn scalar_to_byte(&self, scalar: ScalarIndex) -> Result<ByteOffset, PositionError> {
        self.rope
            .try_char_to_byte(scalar.0)
            .map(|byte| ByteOffset(byte as u64))
            .map_err(|_| PositionError::ScalarOutOfBounds)
    }

    /// Streams the prefix without copying text. LSP lines differ from Rope lines
    /// at Unicode-only line separators, so Rope line counts cannot be reused.
    pub fn byte_to_utf16(&self, byte: ByteOffset) -> Result<Utf16Position, PositionError> {
        self.byte_to_scalar(byte)?;
        utf16_at_byte(self.rope.chars(), byte.0 as usize)
    }

    /// Strict inverse: no line/column clamping or surrogate rounding.
    pub fn utf16_to_byte(&self, position: Utf16Position) -> Result<ByteOffset, PositionError> {
        if position.line as usize > LSP_MAX_INTEGER || position.character as usize > LSP_MAX_INTEGER
        {
            return Err(PositionError::LspIntegerOverflow);
        }
        let mut found_line = false;
        for (byte, line, column) in utf16_boundaries(self.rope.chars()) {
            if line > position.line as usize {
                break;
            }
            if line == position.line as usize {
                found_line = true;
                match column.cmp(&(position.character as usize)) {
                    std::cmp::Ordering::Equal => return Ok(ByteOffset(byte as u64)),
                    std::cmp::Ordering::Greater => return Err(PositionError::NotUtf16Boundary),
                    std::cmp::Ordering::Less => {}
                }
            }
        }
        Err(if found_line {
            PositionError::ColumnOutOfBounds
        } else {
            PositionError::LineOutOfBounds
        })
    }

    /// Convert a diagnostic position while applying LSP's past-end clamping.
    /// Invalid UTF-16 boundaries still fail instead of rounding through a
    /// surrogate pair. This keeps diagnostic display coordinates on the same
    /// conversion path as edits and completion.
    pub fn utf16_to_byte_clamped(
        &self,
        position: Utf16Position,
    ) -> Result<ByteOffset, PositionError> {
        match self.utf16_to_byte(position) {
            Ok(byte) => Ok(byte),
            Err(PositionError::ColumnOutOfBounds) => {
                let mut line_end = None;
                for (byte, line, _) in utf16_boundaries(self.rope.chars()) {
                    match line.cmp(&(position.line as usize)) {
                        std::cmp::Ordering::Less => {}
                        std::cmp::Ordering::Equal => line_end = Some(byte),
                        std::cmp::Ordering::Greater => break,
                    }
                }
                line_end
                    .map(|byte| ByteOffset(byte as u64))
                    .ok_or(PositionError::LineOutOfBounds)
            }
            Err(PositionError::LineOutOfBounds) => Ok(ByteOffset(self.rope.len_bytes() as u64)),
            Err(error) => Err(error),
        }
    }

    /// Converts an exactly addressable grapheme boundary. At most one editor
    /// line is materialized, and contiguous Rope slices are borrowed directly.
    pub fn byte_to_visual(&self, byte: ByteOffset) -> Result<VisualPosition, PositionError> {
        let scalar = self.byte_to_scalar(byte)?.0;
        let line = self.rope.char_to_line(scalar);
        let range = line_content_range(self.rope, line)?;
        if scalar > range.end {
            return Err(PositionError::InsideLineEnding);
        }
        let local_byte = byte.0 as usize - self.rope.char_to_byte(range.start);
        let text: Cow<'_, str> = self.rope.slice(range).into();
        let mut column = 0;
        for (start, grapheme) in text.grapheme_indices(true) {
            let width = grapheme_width(grapheme, column);
            if start == local_byte {
                return if width == 0 {
                    Err(PositionError::NonAddressableVisualColumn)
                } else {
                    Ok(VisualPosition { line, column })
                };
            }
            if start > local_byte {
                return Err(PositionError::NotGraphemeBoundary);
            }
            column += width;
        }
        if local_byte == text.len() {
            Ok(VisualPosition { line, column })
        } else {
            Err(PositionError::NotGraphemeBoundary)
        }
    }

    /// Rejects interior tab, glyph and safe-expansion cells and past-end columns.
    pub fn visual_to_byte(&self, position: VisualPosition) -> Result<ByteOffset, PositionError> {
        let range = line_content_range(self.rope, position.line)?;
        let start = self.rope.char_to_byte(range.start);
        let text: Cow<'_, str> = self.rope.slice(range).into();
        let (byte, reached) = byte_at_display_column(&text, position.column);
        if reached != position.column {
            return Err(if byte == text.len() {
                PositionError::ColumnOutOfBounds
            } else {
                PositionError::NonAddressableVisualColumn
            });
        }
        Ok(ByteOffset((start + byte) as u64))
    }
}

/// Immutable-string adapter for the accepted protocol spike; shares the Rope
/// view's exact UTF-16 mapping, with no temporary Rope or document copy.
pub fn utf16_position(text: &str, byte: ByteOffset) -> Result<Utf16Position, PositionError> {
    let byte = bounded_byte(byte, text.len())?;
    if !text.is_char_boundary(byte) {
        return Err(PositionError::NotUtf8Boundary);
    }
    utf16_at_byte(text.chars(), byte)
}

fn bounded_byte(byte: ByteOffset, length: usize) -> Result<usize, PositionError> {
    usize::try_from(byte.0)
        .ok()
        .filter(|byte| *byte <= length)
        .ok_or(PositionError::ByteOutOfBounds)
}

fn utf16_at_byte(
    chars: impl Iterator<Item = char>,
    target: usize,
) -> Result<Utf16Position, PositionError> {
    for (byte, line, column) in utf16_boundaries(chars) {
        if byte > target {
            // Callers already checked scalar boundaries; only CRLF is skipped.
            return Err(PositionError::InsideLineEnding);
        }
        if byte == target {
            if line > LSP_MAX_INTEGER || column > LSP_MAX_INTEGER {
                return Err(PositionError::LspIntegerOverflow);
            }
            return Ok(Utf16Position {
                line: line as u32,
                character: column as u32,
            });
        }
    }
    Err(PositionError::ByteOutOfBounds)
}

/// Stream (absolute byte, LSP line, UTF-16 column), including EOF, skipping
/// only CRLF's interior. Counters are bounded by the source's byte length.
fn utf16_boundaries(
    chars: impl Iterator<Item = char>,
) -> impl Iterator<Item = (usize, usize, usize)> {
    let mut chars = chars.peekable();
    let mut next = Some((0, 0, 0));
    std::iter::from_fn(move || {
        let current = next?;
        let (byte, line, column) = current;
        next = chars.next().map(|character| {
            let mut bytes = character.len_utf8();
            if character == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
                bytes += 1;
            }
            if matches!(character, '\r' | '\n') {
                (byte + bytes, line + 1, 0)
            } else {
                (byte + bytes, line, column + character.len_utf16())
            }
        });
        Some(current)
    })
}

pub(crate) fn display_width(text: &str) -> usize {
    text.graphemes(true).fold(0, |column, grapheme| {
        column + grapheme_width(grapheme, column)
    })
}

/// Interactive vertical movement deliberately floors an interior cell and
/// clamps past-end targets, preserving the accepted editor behavior.
pub(crate) fn char_offset_at_display_column(text: &str, target: usize) -> usize {
    let (byte, _) = byte_at_display_column(text, target);
    text[..byte].chars().count()
}

fn byte_at_display_column(text: &str, target: usize) -> (usize, usize) {
    let mut column = 0;
    for (byte, grapheme) in text.grapheme_indices(true) {
        let width = grapheme_width(grapheme, column);
        if column + width > target {
            return (byte, column);
        }
        column += width;
    }
    (text.len(), column)
}

pub(crate) fn line_content_range(rope: &Rope, line: usize) -> Result<Range<usize>, PositionError> {
    if line >= rope.len_lines() {
        return Err(PositionError::LineOutOfBounds);
    }
    let start = rope.line_to_char(line);
    let slice = rope.line(line);
    let mut length = slice.len_chars();
    if length > 0
        && matches!(
            slice.char(length - 1),
            '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        )
    {
        let last = slice.char(length - 1);
        length -= 1;
        if last == '\n' && length > 0 && slice.char(length - 1) == '\r' {
            length -= 1;
        }
    }
    Ok(start..start + length)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_columns_keep_floor_and_clamp_behavior() {
        let text = "a\t界e\u{301}";
        assert_eq!(display_width(text), 7);
        for (column, scalar) in [
            (0, 0),
            (1, 1),
            (2, 1),
            (3, 1),
            (4, 2),
            (5, 2),
            (6, 3),
            (7, 5),
            (usize::MAX, 5),
        ] {
            assert_eq!(char_offset_at_display_column(text, column), scalar);
        }
        assert_eq!(char_offset_at_display_column("\u{301}a", 0), 0);
        assert_eq!(char_offset_at_display_column("\u{301}a", 6), 0);
        assert_eq!(char_offset_at_display_column("\u{301}a", 7), 1);
        assert_eq!(char_offset_at_display_column("", usize::MAX), 0);
    }

    #[test]
    fn immutable_string_adapter_checks_bounds_without_rope_rounding() {
        assert_eq!(
            utf16_position("😀\r\nx", ByteOffset(0)),
            Ok(Utf16Position {
                line: 0,
                character: 0
            })
        );
        assert_eq!(
            utf16_position("😀\r\nx", ByteOffset(4)),
            Ok(Utf16Position {
                line: 0,
                character: 2
            })
        );
        assert_eq!(
            utf16_position("😀\r\nx", ByteOffset(5)),
            Err(PositionError::InsideLineEnding)
        );
        assert_eq!(
            utf16_position("😀\r\nx", ByteOffset(6)),
            Ok(Utf16Position {
                line: 1,
                character: 0
            })
        );
        assert_eq!(
            utf16_position("😀\r\nx", ByteOffset(2)),
            Err(PositionError::NotUtf8Boundary)
        );
        assert_eq!(
            utf16_position("😀\r\nx", ByteOffset(u64::MAX)),
            Err(PositionError::ByteOutOfBounds)
        );
    }

    #[test]
    fn diagnostic_utf16_positions_clamp_only_past_document_edges() {
        let rope = Rope::from_str("a😀\r\n東京\n");
        let document = DocumentId::new("diagnostic-clamp").unwrap();
        let positions = Positions::new(&rope, &document, 7);

        assert_eq!(
            positions.utf16_to_byte_clamped(Utf16Position {
                line: 0,
                character: 99,
            }),
            Ok(ByteOffset(5)),
            "a past-end column clamps to the content end before CRLF"
        );
        assert_eq!(
            positions.utf16_to_byte_clamped(Utf16Position {
                line: 99,
                character: 99,
            }),
            Ok(ByteOffset(14)),
            "a past-end line clamps to document EOF"
        );
        assert_eq!(
            positions.utf16_to_byte_clamped(Utf16Position {
                line: 0,
                character: 2,
            }),
            Err(PositionError::NotUtf16Boundary),
            "clamping must not round through the middle of a surrogate pair"
        );
    }
}
