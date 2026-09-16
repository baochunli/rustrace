//! Derived terminal symbols. Never use these strings for edits or evidence.
use std::borrow::Cow;
use unicode_width::UnicodeWidthStr;

/// A single pathological combining cluster must not become a huge terminal cell.
pub const MAX_GRAPHEME_BYTES: usize = 256;
pub const OVERSIZED_GRAPHEME: &str = "[grapheme]";

/// Controls and invisible formatting that must be visible in terminal displays.
/// Joiners and variation selectors are retained inside ordinary visible clusters;
/// standalone zero-width clusters are escaped by [`grapheme`].
pub fn must_escape(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{00ad}'
                | '\u{034f}'
                | '\u{061c}'
                | '\u{115f}'..='\u{1160}'
                | '\u{17b4}'..='\u{17b5}'
                | '\u{180b}'..='\u{180f}'
                | '\u{200b}'
                | '\u{200e}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{3164}'
                | '\u{feff}'
                | '\u{ffa0}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{1bca0}'..='\u{1bca3}'
                | '\u{1d173}'..='\u{1d17a}'
                | '\u{e0000}'..='\u{e007f}'
        )
}

/// One original extended grapheme, represented by safe cells and its exact width.
/// The caller keeps the original byte/scalar range for syntax and selection.
pub fn grapheme(value: &str, column: usize) -> (Cow<'_, str>, usize) {
    if value == "\t" {
        let width = 4 - column % 4;
        return (Cow::Borrowed(&"    "[..width]), width);
    }
    if value.len() > MAX_GRAPHEME_BYTES {
        return (Cow::Borrowed(OVERSIZED_GRAPHEME), OVERSIZED_GRAPHEME.len());
    }
    let width = renderer_width(value);
    if !value.is_empty() && (width == 0 || value.chars().any(must_escape)) {
        // At most 256 input bytes and 10 ASCII bytes per escaped scalar.
        let mut escaped = String::new();
        for character in value.chars() {
            if width == 0 || must_escape(character) {
                escaped.extend(character.escape_default());
            } else {
                escaped.push(character);
            }
        }
        let width = renderer_width(escaped.as_str());
        (Cow::Owned(escaped), width)
    } else {
        (Cow::Borrowed(value), width)
    }
}

fn renderer_width(value: &str) -> usize {
    // Match pinned Ratatui's CellWidth without coupling the editor to a UI
    // crate. Non-combining halfwidth sound marks occupy one extra cell each;
    // U+3099/U+309A keep their ordinary combining behavior. Input is bounded
    // to one source cluster or its safe expansion before this measurement.
    UnicodeWidthStr::width(value)
        + value
            .chars()
            .filter(|c| matches!(c, '\u{ff9e}' | '\u{ff9f}'))
            .count()
}

/// Shared by source rendering, caret following, vertical movement and tab stops.
pub fn grapheme_width(value: &str, column: usize) -> usize {
    // A long horizontally scrolled control-filled line must not allocate an
    // escaped String for each invisible-to-the-viewport source scalar.
    if let Some(character) = value.chars().next()
        && value.len() == character.len_utf8()
        && character != '\t'
        && must_escape(character)
    {
        return character.escape_default().count();
    }
    grapheme(value, column).1
}
