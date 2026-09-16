/// Derived counts for one exact inserted-text payload. Not serialized into events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InsertedTextCounts {
    /// Unicode scalar values, including CR and LF; not bytes or graphemes.
    pub character_count: usize,
    /// Zero for empty text, otherwise one plus the number of LF characters.
    /// A trailing LF adds a final empty line; CRLF contributes one LF.
    pub line_count: usize,
}

/// Summarizes exact inserted text for read-only provenance consumers.
///
/// Pass a persisted `TextEdit::inserted_text` directly; this does not normalize
/// newlines or infer an edit's origin. Counts describe the inserted payload,
/// not the document after selection replacement. No allocation is required.
pub fn inserted_text_counts(text: &str) -> InsertedTextCounts {
    InsertedTextCounts {
        character_count: text.chars().count(),
        line_count: if text.is_empty() {
            0
        } else {
            1 + text.bytes().filter(|byte| *byte == b'\n').count()
        },
    }
}
