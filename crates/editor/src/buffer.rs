use std::error::Error;
use std::fmt;
use std::ops::Range;

use ropey::Rope;
use rustrace_model::{
    DecodeError, DocumentId, Hash, MAX_INSERTED_TEXT_BYTES, MAX_VECTOR_ITEMS, TextEdit,
    ValidationError, validate_inserted_text, validate_text_edits,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::codec::{MAX_TRANSACTION_JSON_BYTES, validate_transaction_encodable};
use crate::position::{
    ByteOffset, PositionError, Positions, char_offset_at_display_column, display_width,
    line_content_range,
};
use crate::{
    EditOrigin, EditorEffectError, EditorEffects, EditorTransaction, SelectionState, document_hash,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorPosition {
    pub char_index: usize,
    pub line: usize,
    pub display_column: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Movement {
    Left,
    Right,
    Up,
    Down,
    PreviousWord,
    NextWord,
    LineStart,
    LineEnd,
    DocumentStart,
    DocumentEnd,
}

#[derive(Clone, Debug)]
struct HistoryEntry {
    forward_edits: Vec<TextEdit>,
    inverse_edits: Vec<TextEdit>,
    selection_before: SelectionState,
    selection_after: SelectionState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectionField {
    Before,
    After,
    Current,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransactionError {
    DocumentMismatch {
        expected: DocumentId,
        found: DocumentId,
    },
    VersionBeforeMismatch {
        expected: u64,
        found: u64,
    },
    VersionAfterNotNext {
        before: u64,
        after: u64,
    },
    VersionOverflow,
    SelectionBeforeMismatch {
        expected: SelectionState,
        found: SelectionState,
    },
    SelectionOffsetOutOfBounds {
        field: &'static str,
        offset: u64,
        text_len: usize,
    },
    SelectionOffsetNotUtf8Boundary {
        field: &'static str,
        offset: u64,
    },
    HashBeforeMismatch {
        expected: Hash,
        found: Hash,
    },
    HashAfterMismatch {
        expected: Hash,
        found: Hash,
    },
    ModelValidation(ValidationError),
    EditOffsetOutOfBounds {
        edit_index: usize,
        offset: u64,
        text_len: usize,
    },
    EditOffsetNotUtf8Boundary {
        edit_index: usize,
        offset: u64,
    },
    EditsNotCanonical {
        previous_index: usize,
        edit_index: usize,
    },
    EditsOverlap {
        previous_index: usize,
        edit_index: usize,
    },
    HistoryMismatch {
        origin: EditOrigin,
    },
    TransactionTooLarge {
        maximum: usize,
    },
    TransactionSerialization {
        message: String,
    },
    HistoryNotRepresentable {
        reason: String,
    },
    DocumentBytesLimit {
        attempted: usize,
        maximum: usize,
    },
    CodecPreflight(DecodeError),
    Provenance(EditorEffectError),
}

impl fmt::Display for TransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DocumentMismatch { expected, found } => write!(
                formatter,
                "transaction document {found} does not match editor document {expected}"
            ),
            Self::VersionBeforeMismatch { expected, found } => write!(
                formatter,
                "transaction version_before is {found}; current version is {expected}"
            ),
            Self::VersionAfterNotNext { before, after } => write!(
                formatter,
                "transaction version must advance exactly once: {before} -> {after}"
            ),
            Self::VersionOverflow => {
                formatter.write_str("document version cannot advance past u64::MAX")
            }
            Self::SelectionBeforeMismatch { expected, found } => write!(
                formatter,
                "transaction selection_before {found:?} does not match current selection {expected:?}"
            ),
            Self::SelectionOffsetOutOfBounds {
                field,
                offset,
                text_len,
            } => write!(
                formatter,
                "{field} byte offset {offset} is outside the {text_len}-byte document"
            ),
            Self::SelectionOffsetNotUtf8Boundary { field, offset } => {
                write!(
                    formatter,
                    "{field} byte offset {offset} splits a UTF-8 code point"
                )
            }
            Self::HashBeforeMismatch { expected, found } => write!(
                formatter,
                "transaction hash_before {found} does not match document hash {expected}"
            ),
            Self::HashAfterMismatch { expected, found } => write!(
                formatter,
                "transaction hash_after {found} does not match edited document hash {expected}"
            ),
            Self::ModelValidation(error) => error.fmt(formatter),
            Self::EditOffsetOutOfBounds {
                edit_index,
                offset,
                text_len,
            } => write!(
                formatter,
                "edit {edit_index} byte offset {offset} is outside the {text_len}-byte document"
            ),
            Self::EditOffsetNotUtf8Boundary { edit_index, offset } => write!(
                formatter,
                "edit {edit_index} byte offset {offset} splits a UTF-8 code point"
            ),
            Self::EditsNotCanonical {
                previous_index,
                edit_index,
            } => write!(
                formatter,
                "edits are not in canonical order at indices {previous_index} and {edit_index}"
            ),
            Self::EditsOverlap {
                previous_index,
                edit_index,
            } => write!(
                formatter,
                "edits overlap at indices {previous_index} and {edit_index}"
            ),
            Self::HistoryMismatch { origin } => {
                write!(
                    formatter,
                    "{origin:?} transaction does not match editor history"
                )
            }
            Self::TransactionTooLarge { maximum } => write!(
                formatter,
                "encoded editor transaction exceeds the {maximum}-byte maximum"
            ),
            Self::TransactionSerialization { message } => {
                write!(formatter, "could not encode editor transaction: {message}")
            }
            Self::HistoryNotRepresentable { reason } => {
                write!(
                    formatter,
                    "undo/redo transaction is not representable: {reason}"
                )
            }
            Self::DocumentBytesLimit { attempted, maximum } => write!(
                formatter,
                "edited document would use {attempted} bytes; limit is {maximum}"
            ),
            Self::CodecPreflight(error) => {
                write!(
                    formatter,
                    "encoded transaction fails JSON preflight: {error}"
                )
            }
            Self::Provenance(error) => {
                write!(
                    formatter,
                    "transaction provenance was not persisted: {error}"
                )
            }
        }
    }
}

impl Error for TransactionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ModelValidation(error) => Some(error),
            Self::CodecPreflight(error) => Some(error),
            Self::Provenance(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ValidationError> for TransactionError {
    fn from(error: ValidationError) -> Self {
        Self::ModelValidation(error)
    }
}

struct PreparedTransaction {
    after_text: String,
    history_entry: Option<HistoryEntry>,
}

struct MaterializedTransaction {
    before_text: String,
    after_text: String,
    hash_after: Hash,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AutoInsertedCloser {
    byte: u64,
    character: char,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RustLexState {
    #[default]
    Code,
    String {
        escaped: bool,
    },
    RawString {
        hashes: usize,
    },
    LineComment,
    BlockComment {
        depth: usize,
    },
}

impl RustLexState {
    fn inside_string(self) -> bool {
        matches!(self, Self::String { .. } | Self::RawString { .. })
    }
}

/// Private Rope-backed document storage with one public mutation gateway.
pub struct EditorBuffer<E>
where
    E: EditorEffects,
{
    document_id: DocumentId,
    rope: Rope,
    version: u64,
    selection: SelectionState,
    hash: Hash,
    preferred_column: Option<usize>,
    undo: Vec<HistoryEntry>,
    redo: Vec<HistoryEntry>,
    effects: E,
    text_byte_limit: usize,
    auto_closer: Option<AutoInsertedCloser>,
    /// Rust lexical state at each line start, rebuilt from the first edited line.
    line_lex_states: Vec<RustLexState>,
}

impl<E> EditorBuffer<E>
where
    E: EditorEffects,
{
    pub fn new(document_id: DocumentId, initial: &str, effects: E) -> Self {
        Self {
            document_id,
            rope: Rope::from_str(initial),
            version: 0,
            selection: SelectionState::default(),
            hash: document_hash(initial),
            preferred_column: None,
            undo: Vec::new(),
            redo: Vec::new(),
            effects,
            text_byte_limit: usize::MAX,
            auto_closer: None,
            line_lex_states: line_lex_states(initial),
        }
    }

    /// Construct from a validated durable replay state. Undo history starts empty.
    pub fn from_recovered(
        document_id: DocumentId,
        initial: &str,
        version: u64,
        selection: SelectionState,
        effects: E,
    ) -> Result<Self, TransactionError> {
        let mut buffer = Self::new(document_id, initial, effects);
        buffer.set_selection(selection)?;
        buffer.version = version;
        Ok(buffer)
    }

    pub fn document_id(&self) -> &DocumentId {
        &self.document_id
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// Returns non-overlapping literal matches as exact UTF-8 byte ranges.
    pub fn literal_matches(&self, query: &str) -> Vec<SelectionState> {
        if query.is_empty() {
            return Vec::new();
        }
        self.text()
            .match_indices(query)
            .map(|(start, matched)| {
                SelectionState::new(start as u64, (start + matched.len()) as u64)
            })
            .collect()
    }

    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn revision(&self) -> u64 {
        self.version
    }

    /// Borrow coordinate conversions only for the expected current text version.
    /// The view cannot outlive a subsequent mutable borrow of this buffer.
    pub fn positions(&self, expected_version: u64) -> Result<Positions<'_>, PositionError> {
        if expected_version != self.version {
            return Err(PositionError::StaleVersion {
                expected: expected_version,
                current: self.version,
            });
        }
        Ok(Positions::new(&self.rope, &self.document_id, self.version))
    }

    pub fn hash(&self) -> Hash {
        self.hash
    }

    pub fn selection_state(&self) -> SelectionState {
        self.selection
    }

    /// Clears input-only state that must not survive unrelated navigation.
    pub fn clear_transient_edit_state(&mut self) {
        self.auto_closer = None;
    }

    /// Sets the maximum UTF-8 byte length accepted by future transactions.
    pub fn set_text_byte_limit(&mut self, maximum: usize) -> Result<(), TransactionError> {
        let current = self.rope.len_bytes();
        if current > maximum {
            return Err(TransactionError::DocumentBytesLimit {
                attempted: current,
                maximum,
            });
        }
        self.text_byte_limit = maximum;
        Ok(())
    }

    /// Heap and entry storage retained solely for undo/redo capability.
    pub fn retained_history_bytes(&self) -> usize {
        (self.undo.capacity() + self.redo.capacity()) * std::mem::size_of::<HistoryEntry>()
            + self
                .undo
                .iter()
                .chain(&self.redo)
                .map(|entry| {
                    [&entry.forward_edits, &entry.inverse_edits]
                        .into_iter()
                        .map(|edits| {
                            edits.capacity() * std::mem::size_of::<TextEdit>()
                                + edits
                                    .iter()
                                    .map(|edit| edit.inserted_text.capacity())
                                    .sum::<usize>()
                        })
                        .sum::<usize>()
                })
                .sum::<usize>()
    }

    /// Evict the oldest undo capability, never recorded provenance.
    pub fn trim_history_to(&mut self, maximum: usize) -> bool {
        let mut evicted = false;
        while self.retained_history_bytes() > maximum {
            self.undo.shrink_to_fit();
            self.redo.shrink_to_fit();
            if self.retained_history_bytes() <= maximum {
                break;
            }
            if !self.undo.is_empty() {
                self.undo.remove(0);
            } else if !self.redo.is_empty() {
                self.redo.remove(0);
            } else {
                break;
            }
            evicted = true;
        }
        if evicted {
            self.undo.shrink_to_fit();
            self.redo.shrink_to_fit();
        }
        evicted
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn cursor(&self) -> CursorPosition {
        let cursor = self.active_char();
        let line = self.rope.char_to_line(cursor);
        let line_start = self.rope.line_to_char(line);
        let before_cursor = self.rope.slice(line_start..cursor).to_string();
        CursorPosition {
            char_index: cursor,
            line,
            display_column: display_width(&before_cursor),
        }
    }

    pub fn selection(&self) -> Option<Range<usize>> {
        if self.selection.is_caret() {
            return None;
        }
        let anchor = self.byte_to_char(self.selection.anchor_byte);
        let active = self.byte_to_char(self.selection.active_byte);
        Some(anchor.min(active)..anchor.max(active))
    }

    /// Returns the UTF-8 byte offset of the bracket matching the cell on or
    /// immediately before the caret. This is derived display state only.
    pub fn matching_bracket_byte(&self) -> Option<u64> {
        let text = self.text();
        let cursor = self.selection.active_byte as usize;
        let candidate = text[cursor..]
            .char_indices()
            .next()
            .map(|(_, character)| (cursor, character))
            .filter(|(_, character)| is_bracket(*character))
            .or_else(|| {
                text[..cursor]
                    .char_indices()
                    .next_back()
                    .filter(|(_, character)| is_bracket(*character))
            })?;
        matching_bracket_in_text(&text, candidate.0, candidate.1).map(|byte| byte as u64)
    }

    pub fn set_selection(&mut self, selection: SelectionState) -> Result<bool, TransactionError> {
        self.auto_closer = None;
        let text = self.text();
        validate_selection(SelectionField::Current, selection, &text)?;
        if selection == self.selection {
            return Ok(false);
        }
        self.selection = selection;
        self.preferred_column = None;
        Ok(true)
    }

    pub fn slice(&self, range: Range<usize>) -> String {
        self.rope.slice(range).to_string()
    }

    pub fn move_cursor(&mut self, movement: Movement, selecting: bool) {
        self.auto_closer = None;
        let is_vertical = matches!(movement, Movement::Up | Movement::Down);
        if is_vertical && self.preferred_column.is_none() {
            self.preferred_column = Some(self.cursor().display_column);
        }
        self.selection = self.selection_after_movement(movement, selecting);
        if !is_vertical {
            self.preferred_column = None;
        }
    }

    /// Moves to the nearest addressable visual cell on a clamped editor line.
    ///
    /// Interior tab, wide-glyph, combining-cluster, and safe-display cells
    /// resolve to the grapheme start. Columns past the rendered line resolve
    /// to its end, matching vertical keyboard movement.
    pub fn move_to_visual(&mut self, line: usize, column: usize, selecting: bool) -> bool {
        self.auto_closer = None;
        let target = self.byte_at_visual_position(line, column);
        let selection = if selecting {
            SelectionState::new(self.selection.anchor_byte, target)
        } else {
            SelectionState::caret(target)
        };
        if selection == self.selection {
            return false;
        }
        self.selection = selection;
        self.preferred_column = None;
        true
    }

    /// Returns the Unicode word containing a lenient visual position.
    /// Non-word cells fall back to one complete grapheme, and an end-of-line
    /// position remains a caret.
    pub fn word_range_at(&self, line: usize, column: usize) -> SelectionState {
        let line = line.min(self.line_count().saturating_sub(1));
        let range = self.line_content_range(line);
        let text = self.rope.slice(range.clone()).to_string();
        let local_char = char_offset_at_display_column(&text, column);
        let local_byte = text
            .char_indices()
            .nth(local_char)
            .map_or(text.len(), |(byte, _)| byte);
        let line_start = self.rope.char_to_byte(range.start);
        if local_byte == text.len() {
            return SelectionState::caret((line_start + local_byte) as u64);
        }

        if let Some((start, word)) = text
            .split_word_bound_indices()
            .find(|(start, word)| local_byte >= *start && local_byte < start + word.len())
            .filter(|(_, word)| word.unicode_words().next().is_some())
        {
            return SelectionState::new(
                (line_start + start) as u64,
                (line_start + start + word.len()) as u64,
            );
        }

        let (start, grapheme) = text
            .grapheme_indices(true)
            .find(|(start, grapheme)| local_byte >= *start && local_byte < start + grapheme.len())
            .expect("a non-EOF visual position belongs to one grapheme");
        SelectionState::new(
            (line_start + start) as u64,
            (line_start + start + grapheme.len()) as u64,
        )
    }

    /// Computes the selection produced by a movement without changing editor state.
    pub fn selection_after_movement(&self, movement: Movement, selecting: bool) -> SelectionState {
        let cursor = self.active_char();
        let next = match movement {
            Movement::Left => previous_grapheme_boundary(&self.rope, cursor),
            Movement::Right => next_grapheme_boundary(&self.rope, cursor),
            Movement::Up => self.vertical_destination(-1),
            Movement::Down => self.vertical_destination(1),
            Movement::PreviousWord => previous_word_boundary(&self.rope, cursor),
            Movement::NextWord => next_word_boundary(&self.rope, cursor),
            Movement::LineStart => {
                let line = self.rope.char_to_line(cursor);
                self.line_content_range(line).start
            }
            Movement::LineEnd => {
                let line = self.rope.char_to_line(cursor);
                self.line_content_range(line).end
            }
            Movement::DocumentStart => 0,
            Movement::DocumentEnd => self.rope.len_chars(),
        };
        let next_byte = self.rope.char_to_byte(next) as u64;
        if selecting {
            SelectionState::new(self.selection.anchor_byte, next_byte)
        } else {
            SelectionState::caret(next_byte)
        }
    }

    pub fn insert_char(&mut self, character: char) -> Result<bool, TransactionError> {
        if let Some(marker) = self.auto_closer.take()
            && marker.character == character
            && self.selection == SelectionState::caret(marker.byte)
            && self.character_at_byte(marker.byte) == Some(character)
        {
            self.selection = SelectionState::caret(marker.byte + character.len_utf8() as u64);
            self.preferred_column = None;
            return Ok(false);
        }
        let auto_closer = self.auto_closer_for(character);
        let Some(transaction) = self.preview_insert_char(character)? else {
            return Ok(false);
        };
        let changed = self.apply_transaction(transaction)?;
        if changed && let Some(closer) = auto_closer {
            self.auto_closer = Some(AutoInsertedCloser {
                byte: self.selection.active_byte,
                character: closer,
            });
        }
        Ok(changed)
    }

    pub fn paste(&mut self, text: &str) -> Result<bool, TransactionError> {
        self.auto_closer = None;
        self.effects
            .check_input_origin(EditOrigin::Paste)
            .map_err(TransactionError::Provenance)?;
        let Some(transaction) = self.preview_paste(text)? else {
            return Ok(false);
        };
        self.apply_transaction(transaction)
    }

    pub fn delete_backward(&mut self) -> Result<bool, TransactionError> {
        if let Some(marker) = self.auto_closer.take()
            && let Some(transaction) = self.preview_delete_auto_pair(marker)?
        {
            return self.apply_transaction(transaction);
        }
        let Some(transaction) = self.preview_delete_backward()? else {
            return Ok(false);
        };
        self.apply_transaction(transaction)
    }

    pub fn delete_forward(&mut self) -> Result<bool, TransactionError> {
        self.auto_closer = None;
        let Some(transaction) = self.preview_delete_forward()? else {
            return Ok(false);
        };
        self.apply_transaction(transaction)
    }

    pub fn delete_previous_word(&mut self) -> Result<bool, TransactionError> {
        self.auto_closer = None;
        let cursor = self.active_char();
        let range = self
            .selection()
            .unwrap_or_else(|| previous_word_boundary(&self.rope, cursor)..cursor);
        let Some(transaction) = self.preview_edit_char_range(range, "", EditOrigin::Keyboard)?
        else {
            return Ok(false);
        };
        self.apply_transaction(transaction)
    }

    pub fn delete_to_line_start(&mut self) -> Result<bool, TransactionError> {
        self.auto_closer = None;
        let cursor = self.active_char();
        let range = self.selection().unwrap_or_else(|| {
            let line = self.rope.char_to_line(cursor);
            self.line_content_range(line).start..cursor
        });
        let Some(transaction) = self.preview_edit_char_range(range, "", EditOrigin::Keyboard)?
        else {
            return Ok(false);
        };
        self.apply_transaction(transaction)
    }

    pub fn delete_to_line_end(&mut self) -> Result<bool, TransactionError> {
        self.auto_closer = None;
        let cursor = self.active_char();
        let range = self.selection().unwrap_or_else(|| {
            let line = self.rope.char_to_line(cursor);
            cursor..self.line_content_range(line).end
        });
        let Some(transaction) = self.preview_edit_char_range(range, "", EditOrigin::Keyboard)?
        else {
            return Ok(false);
        };
        self.apply_transaction(transaction)
    }

    /// Adds or removes Rust line-comment markers for the selected lines.
    pub fn toggle_line_comment(&mut self) -> Result<bool, TransactionError> {
        self.auto_closer = None;
        let Some(transaction) = self.preview_toggle_line_comment()? else {
            return Ok(false);
        };
        self.apply_transaction(transaction)
    }

    /// Builds one exact keyboard transaction for a line-comment toggle.
    pub fn preview_toggle_line_comment(
        &self,
    ) -> Result<Option<EditorTransaction>, TransactionError> {
        let text = self.text();
        let starts = selected_line_byte_starts(&text, self.selection);
        let line_indents = starts
            .iter()
            .map(|start| (*start, leading_indentation(line_at(&text, *start)).len()))
            .collect::<Vec<_>>();
        let all_commented = line_indents
            .iter()
            .all(|(start, indentation)| text[start + indentation..].starts_with("//"));
        let edits = if all_commented {
            line_indents
                .into_iter()
                .map(|(start, indentation)| {
                    let marker = start + indentation;
                    let removed = if text[marker..].starts_with("// ") {
                        3
                    } else {
                        2
                    };
                    TextEdit {
                        start_byte: marker as u64,
                        end_byte: (marker + removed) as u64,
                        inserted_text: String::new(),
                    }
                })
                .collect::<Vec<_>>()
        } else {
            let common = line_indents
                .iter()
                .map(|(_, indentation)| *indentation)
                .min()
                .unwrap_or(0);
            starts
                .into_iter()
                .map(|start| TextEdit {
                    start_byte: (start + common) as u64,
                    end_byte: (start + common) as u64,
                    inserted_text: "// ".to_owned(),
                })
                .collect::<Vec<_>>()
        };
        let selection_after = SelectionState::new(
            transform_edit_offset(self.selection.anchor_byte, &edits),
            transform_edit_offset(self.selection.active_byte, &edits),
        );
        self.preview_edits(EditOrigin::Keyboard, edits, selection_after)
    }

    /// Builds the exact transaction for a typed character without mutating state.
    pub fn preview_insert_char(
        &self,
        character: char,
    ) -> Result<Option<EditorTransaction>, TransactionError> {
        if character == '\n' && self.selection.is_caret() {
            return self.preview_indented_newline();
        }
        if matches!(character, ')' | ']' | '}') && self.selection.is_caret() {
            return self.preview_dedented_closer(character);
        }
        if let Some(closer) = self.auto_closer_for(character) {
            let cursor = self.selection.active_byte;
            return self.preview_edits(
                EditOrigin::Keyboard,
                vec![TextEdit {
                    start_byte: cursor,
                    end_byte: cursor,
                    inserted_text: format!("{character}{closer}"),
                }],
                SelectionState::caret(cursor + character.len_utf8() as u64),
            );
        }
        let mut encoded = [0; 4];
        self.preview_edit_selection(character.encode_utf8(&mut encoded), EditOrigin::Keyboard)
    }

    fn auto_closer_for(&self, character: char) -> Option<char> {
        if !self.selection.is_caret() {
            return None;
        }
        let closer = match character {
            '(' => ')',
            '[' => ']',
            '{' => '}',
            '"' => '"',
            _ => return None,
        };
        let text = self.text();
        let cursor = self.selection.active_byte as usize;
        let next = text[cursor..].chars().next();
        if !next.is_none_or(|next| {
            matches!(next, '\r' | '\n' | ')' | ']' | '}' | '"') || next.is_whitespace()
        }) {
            return None;
        }
        if character == '"'
            && (previous_grapheme_is_identifier(&text[..cursor])
                || self.inside_string_at(cursor, &text))
        {
            return None;
        }
        Some(closer)
    }

    fn inside_string_at(&self, cursor: usize, text: &str) -> bool {
        let line = self.rope.byte_to_line(cursor);
        let state = self.line_lex_states.get(line).copied().unwrap_or_default();
        scan_rust_lex_state(&text[byte_line_start(text, cursor)..cursor], state).inside_string()
    }

    fn character_at_byte(&self, byte: u64) -> Option<char> {
        let byte = usize::try_from(byte).ok()?;
        self.text().get(byte..)?.chars().next()
    }

    fn preview_delete_auto_pair(
        &self,
        marker: AutoInsertedCloser,
    ) -> Result<Option<EditorTransaction>, TransactionError> {
        if self.selection != SelectionState::caret(marker.byte)
            || self.character_at_byte(marker.byte) != Some(marker.character)
        {
            return Ok(None);
        }
        let opener = match marker.character {
            ')' => '(',
            ']' => '[',
            '}' => '{',
            '"' => '"',
            _ => return Ok(None),
        };
        let text = self.text();
        let cursor = marker.byte as usize;
        let Some((start, previous)) = text[..cursor].char_indices().next_back() else {
            return Ok(None);
        };
        if previous != opener {
            return Ok(None);
        }
        self.preview_edits(
            EditOrigin::Keyboard,
            vec![TextEdit {
                start_byte: start as u64,
                end_byte: marker.byte + marker.character.len_utf8() as u64,
                inserted_text: String::new(),
            }],
            SelectionState::caret(start as u64),
        )
    }

    fn preview_indented_newline(&self) -> Result<Option<EditorTransaction>, TransactionError> {
        let text = self.text();
        let cursor = self.selection.active_byte as usize;
        let line_start = byte_line_start(&text, cursor);
        let before = &text[line_start..cursor];
        let indentation = leading_indentation(before);
        let trimmed = before.trim_end_matches(char::is_whitespace);
        let opener = trimmed
            .chars()
            .next_back()
            .filter(|character| matches!(character, '{' | '(' | '['));
        let newline = newline_at(&text, cursor);
        let extra = opener.map_or("", |_| {
            if indentation.contains('\t') {
                "\t"
            } else {
                "    "
            }
        });
        let matching_closer = opener.and_then(closing_delimiter);
        let between_pair = matching_closer
            .zip(text[cursor..].chars().next())
            .is_some_and(|(expected, next)| expected == next);

        let mut inserted = String::with_capacity(
            newline.len() * if between_pair { 2 } else { 1 }
                + indentation.len() * if between_pair { 2 } else { 1 }
                + extra.len(),
        );
        inserted.push_str(newline);
        inserted.push_str(indentation);
        inserted.push_str(extra);
        let caret = cursor + inserted.len();
        if between_pair {
            inserted.push_str(newline);
            inserted.push_str(indentation);
        }

        self.preview_edits(
            EditOrigin::Keyboard,
            vec![TextEdit {
                start_byte: cursor as u64,
                end_byte: cursor as u64,
                inserted_text: inserted,
            }],
            SelectionState::caret(caret as u64),
        )
    }

    fn preview_dedented_closer(
        &self,
        closer: char,
    ) -> Result<Option<EditorTransaction>, TransactionError> {
        let text = self.text();
        let cursor = self.selection.active_byte as usize;
        let line_start = byte_line_start(&text, cursor);
        let before = &text[line_start..cursor];
        if !before.chars().all(char::is_whitespace) {
            let mut encoded = [0; 4];
            return self
                .preview_edit_selection(closer.encode_utf8(&mut encoded), EditOrigin::Keyboard);
        }

        let opener = opening_delimiter(closer).expect("closing delimiter was checked by caller");
        let matching_indent = matching_opening_before(&text, cursor, opener, closer)
            .map(|opening| leading_indentation(&text[byte_line_start(&text, opening)..opening]));
        let indentation = matching_indent
            .filter(|indentation| {
                indentation.len() <= before.len() && before.starts_with(indentation)
            })
            .map_or_else(|| remove_one_indentation_level(before), str::to_owned);
        let mut inserted = indentation;
        inserted.push(closer);
        let caret = line_start + inserted.len();

        self.preview_edits(
            EditOrigin::Keyboard,
            vec![TextEdit {
                start_byte: line_start as u64,
                end_byte: cursor as u64,
                inserted_text: inserted,
            }],
            SelectionState::caret(caret as u64),
        )
    }

    /// Builds the exact transaction for a paste without mutating state.
    pub fn preview_paste(&self, text: &str) -> Result<Option<EditorTransaction>, TransactionError> {
        self.preview_edit_selection(text, EditOrigin::Paste)
    }

    /// Builds the exact grapheme-aware backward delete without mutating state.
    pub fn preview_delete_backward(&self) -> Result<Option<EditorTransaction>, TransactionError> {
        let cursor = self.active_char();
        let range = self
            .selection()
            .unwrap_or_else(|| previous_grapheme_boundary(&self.rope, cursor)..cursor);
        self.preview_edit_char_range(range, "", EditOrigin::Keyboard)
    }

    /// Builds the exact grapheme-aware forward delete without mutating state.
    pub fn preview_delete_forward(&self) -> Result<Option<EditorTransaction>, TransactionError> {
        let cursor = self.active_char();
        let range = self
            .selection()
            .unwrap_or_else(|| cursor..next_grapheme_boundary(&self.rope, cursor));
        self.preview_edit_char_range(range, "", EditOrigin::Keyboard)
    }

    /// Builds and applies a transaction for keyboard, paste, or programmatic
    /// edits. Ranges are in the current pre-transaction UTF-8 snapshot.
    pub fn apply_edits(
        &mut self,
        origin: EditOrigin,
        edits: Vec<TextEdit>,
        selection_after: SelectionState,
    ) -> Result<bool, TransactionError> {
        self.effects
            .check_input_origin(origin)
            .map_err(TransactionError::Provenance)?;
        let transaction = self.preflight_edits(origin, edits, selection_after)?;
        let before = self.text();
        let after = apply_edits_to_text(&before, &transaction.edits)?;
        self.apply_materialized_transaction(transaction, before, after)
    }

    /// Validates and atomically commits a caller-supplied transaction.
    ///
    /// This is the only content mutation path. Every validation, including
    /// history validation for undo/redo, completes before editor state changes.
    pub fn apply_transaction(
        &mut self,
        transaction: EditorTransaction,
    ) -> Result<bool, TransactionError> {
        self.effects
            .check_input_origin(transaction.origin)
            .map_err(TransactionError::Provenance)?;
        self.apply_transaction_inner(transaction, None)
    }

    fn apply_transaction_inner(
        &mut self,
        transaction: EditorTransaction,
        materialized: Option<MaterializedTransaction>,
    ) -> Result<bool, TransactionError> {
        self.auto_closer = None;
        let Some(prepared) = self.prepare_transaction(&transaction, materialized, true)? else {
            return Ok(false);
        };
        self.validate_history_action(&transaction)?;
        self.effects
            .record_provenance(&transaction)
            .map_err(TransactionError::Provenance)?;

        self.rope = Rope::from_str(&prepared.after_text);
        let edited_line = transaction
            .edits
            .first()
            .map_or(0, |edit| self.rope.byte_to_line(edit.start_byte as usize));
        self.rebuild_line_lex_states_from(edited_line);
        self.version = transaction.version_after;
        self.selection = transaction.selection_after;
        self.hash = transaction.hash_after;
        self.preferred_column = None;

        match transaction.origin {
            EditOrigin::Undo => {
                let entry = self
                    .undo
                    .pop()
                    .expect("undo history validated before commit");
                self.redo.push(entry);
            }
            EditOrigin::Redo => {
                let entry = self
                    .redo
                    .pop()
                    .expect("redo history validated before commit");
                self.undo.push(entry);
            }
            _ => {
                self.undo.push(
                    prepared
                        .history_entry
                        .expect("normal transaction history prepared before commit"),
                );
                self.redo.clear();
            }
        }

        self.effects.update_tree_sitter(&transaction);
        self.effects.send_lsp_did_change(&transaction);
        self.effects.record_replay(&transaction);
        Ok(true)
    }

    fn rebuild_line_lex_states_from(&mut self, first_line: usize) {
        let first_line = first_line.min(self.rope.len_lines().saturating_sub(1));
        let mut state = if first_line == 0 {
            RustLexState::Code
        } else {
            self.line_lex_states
                .get(first_line)
                .copied()
                .unwrap_or_default()
        };
        self.line_lex_states.truncate(first_line);
        for line in first_line..self.rope.len_lines() {
            self.line_lex_states.push(state);
            state = scan_rust_lex_state(&self.rope.line(line).to_string(), state);
        }
    }

    pub fn undo(&mut self) -> Result<bool, TransactionError> {
        let Some(transaction) = self.preview_undo()? else {
            return Ok(false);
        };
        self.apply_transaction(transaction)
    }

    pub fn redo(&mut self) -> Result<bool, TransactionError> {
        let Some(transaction) = self.preview_redo()? else {
            return Ok(false);
        };
        self.apply_transaction(transaction)
    }

    /// Builds the exact undo transaction without mutating editor or history state.
    pub fn preview_undo(&self) -> Result<Option<EditorTransaction>, TransactionError> {
        let Some(entry) = self.undo.last() else {
            return Ok(None);
        };
        self.preview_edits(
            EditOrigin::Undo,
            entry.inverse_edits.clone(),
            entry.selection_before,
        )
    }

    /// Builds the exact redo transaction without mutating editor or history state.
    pub fn preview_redo(&self) -> Result<Option<EditorTransaction>, TransactionError> {
        let Some(entry) = self.redo.last() else {
            return Ok(None);
        };
        self.preview_edits(
            EditOrigin::Redo,
            entry.forward_edits.clone(),
            entry.selection_after,
        )
    }

    pub fn line_text(&self, line: usize) -> String {
        let range = self.line_content_range(line);
        self.rope.slice(range).to_string()
    }

    pub fn line_start_char(&self, line: usize) -> usize {
        self.rope.line_to_char(line)
    }

    pub fn line_start_byte(&self, line: usize) -> usize {
        self.rope.line_to_byte(line)
    }

    fn prepare_transaction(
        &self,
        transaction: &EditorTransaction,
        materialized: Option<MaterializedTransaction>,
        record_history: bool,
    ) -> Result<Option<PreparedTransaction>, TransactionError> {
        if transaction.document_id != self.document_id {
            return Err(TransactionError::DocumentMismatch {
                expected: self.document_id.clone(),
                found: transaction.document_id.clone(),
            });
        }
        if transaction.version_before != self.version {
            return Err(TransactionError::VersionBeforeMismatch {
                expected: self.version,
                found: transaction.version_before,
            });
        }
        validate_transaction_encodable(transaction)?;
        if transaction.selection_before != self.selection {
            return Err(TransactionError::SelectionBeforeMismatch {
                expected: self.selection,
                found: transaction.selection_before,
            });
        }
        let materialized = match materialized {
            Some(materialized) => materialized,
            None => {
                let before_text = self.text();
                let after_text = apply_edits_to_text(&before_text, &transaction.edits)?;
                let hash_after = document_hash(&after_text);
                MaterializedTransaction {
                    before_text,
                    after_text,
                    hash_after,
                }
            }
        };
        let before = &materialized.before_text;
        validate_selection(SelectionField::Before, transaction.selection_before, before)?;
        if transaction.hash_before != self.hash {
            return Err(TransactionError::HashBeforeMismatch {
                expected: self.hash,
                found: transaction.hash_before,
            });
        }
        let after = &materialized.after_text;
        if after.len() > self.text_byte_limit {
            return Err(TransactionError::DocumentBytesLimit {
                attempted: after.len(),
                maximum: self.text_byte_limit,
            });
        }
        validate_selection(SelectionField::After, transaction.selection_after, after)?;
        let expected_after_hash = materialized.hash_after;
        if transaction.hash_after != expected_after_hash {
            return Err(TransactionError::HashAfterMismatch {
                expected: expected_after_hash,
                found: transaction.hash_after,
            });
        }
        if after == before {
            return Ok(None);
        }

        let history_entry = if !record_history
            || matches!(transaction.origin, EditOrigin::Undo | EditOrigin::Redo)
        {
            None
        } else {
            Some(build_history_entry(before, transaction)?)
        };
        Ok(Some(PreparedTransaction {
            after_text: materialized.after_text,
            history_entry,
        }))
    }

    fn preflight_edits(
        &self,
        origin: EditOrigin,
        edits: Vec<TextEdit>,
        selection_after: SelectionState,
    ) -> Result<EditorTransaction, TransactionError> {
        let version_after = self
            .version
            .checked_add(1)
            .ok_or(TransactionError::VersionOverflow)?;
        // Hash always serializes as 64 lowercase hexadecimal bytes, so zero is
        // wire-size-equivalent to the not-yet-computable post-edit hash.
        let transaction = EditorTransaction {
            document_id: self.document_id.clone(),
            version_before: self.version,
            version_after,
            origin,
            edits,
            selection_before: self.selection,
            selection_after,
            hash_before: self.hash,
            hash_after: Hash::zero(),
        };
        validate_transaction_encodable(&transaction)?;
        Ok(transaction)
    }

    fn apply_materialized_transaction(
        &mut self,
        mut transaction: EditorTransaction,
        before_text: String,
        after_text: String,
    ) -> Result<bool, TransactionError> {
        validate_selection(
            SelectionField::After,
            transaction.selection_after,
            &after_text,
        )?;
        if after_text == before_text {
            return Ok(false);
        }
        let hash_after = document_hash(&after_text);
        transaction.hash_after = hash_after;
        self.apply_transaction_inner(
            transaction,
            Some(MaterializedTransaction {
                before_text,
                after_text,
                hash_after,
            }),
        )
    }

    fn validate_history_action(
        &self,
        transaction: &EditorTransaction,
    ) -> Result<(), TransactionError> {
        let matches = match transaction.origin {
            EditOrigin::Undo => self.undo.last().is_some_and(|entry| {
                transaction.edits == entry.inverse_edits
                    && transaction.selection_after == entry.selection_before
            }),
            EditOrigin::Redo => self.redo.last().is_some_and(|entry| {
                transaction.edits == entry.forward_edits
                    && transaction.selection_after == entry.selection_after
            }),
            _ => true,
        };
        if matches {
            Ok(())
        } else {
            Err(TransactionError::HistoryMismatch {
                origin: transaction.origin,
            })
        }
    }

    fn preview_edit_selection(
        &self,
        inserted: &str,
        origin: EditOrigin,
    ) -> Result<Option<EditorTransaction>, TransactionError> {
        let range = self.selection().unwrap_or_else(|| {
            let cursor = self.active_char();
            cursor..cursor
        });
        self.preview_edit_char_range(range, inserted, origin)
    }

    fn preview_edit_char_range(
        &self,
        range: Range<usize>,
        inserted: &str,
        origin: EditOrigin,
    ) -> Result<Option<EditorTransaction>, TransactionError> {
        validate_inserted_text(inserted)?;
        let start_byte = self.rope.char_to_byte(range.start);
        let end_byte = self.rope.char_to_byte(range.end);
        let edit = TextEdit {
            start_byte: start_byte as u64,
            end_byte: end_byte as u64,
            inserted_text: inserted.to_owned(),
        };
        // Grapheme normalization determines the exact cursor only after the
        // edit. u64::MAX is its maximum JSON width, so an actual cursor cannot
        // make the final transaction larger than this early preflight shell.
        let mut transaction =
            self.preflight_edits(origin, vec![edit], SelectionState::caret(u64::MAX))?;
        let before = self.text();
        let after = apply_edits_to_text(&before, &transaction.edits)?;
        let requested_cursor = start_byte + inserted.len();
        let cursor_after = normalize_grapheme_byte(&after, requested_cursor) as u64;
        transaction.selection_after = SelectionState::caret(cursor_after);
        transaction.hash_after = document_hash(&after);
        let prepared = self.prepare_transaction(
            &transaction,
            Some(MaterializedTransaction {
                before_text: before,
                after_text: after,
                hash_after: transaction.hash_after,
            }),
            true,
        )?;
        self.validate_history_action(&transaction)?;
        Ok(prepared.map(|_| transaction))
    }

    /// Builds and validates an arbitrary transaction without mutating state.
    pub fn preview_edits(
        &self,
        origin: EditOrigin,
        edits: Vec<TextEdit>,
        selection_after: SelectionState,
    ) -> Result<Option<EditorTransaction>, TransactionError> {
        let mut transaction = self.preflight_edits(origin, edits, selection_after)?;
        let before = self.text();
        let after = apply_edits_to_text(&before, &transaction.edits)?;
        transaction.hash_after = document_hash(&after);
        let prepared = self.prepare_transaction(
            &transaction,
            Some(MaterializedTransaction {
                before_text: before,
                after_text: after,
                hash_after: transaction.hash_after,
            }),
            true,
        )?;
        self.validate_history_action(&transaction)?;
        Ok(prepared.map(|_| transaction))
    }

    /// Preflights a complete saved-file reload as bounded transactions.
    ///
    /// The model bounds each edit payload below the maximum supported document
    /// size. A minimal replacement handles the common case in one event; the
    /// fallback chunks both deletion and insertion so every transaction and
    /// its undo history remain representable. The caller can apply the returned
    /// transactions knowing that the entire sequence has already validated.
    pub fn preview_file_reload(
        &self,
        replacement: &str,
        text_byte_limit: usize,
    ) -> Result<Vec<EditorTransaction>, TransactionError> {
        let current = self.text();
        if current == replacement {
            return Ok(Vec::new());
        }

        let mut preview = EditorBuffer {
            document_id: self.document_id.clone(),
            rope: Rope::from_str(&current),
            version: self.version,
            selection: self.selection,
            hash: self.hash,
            preferred_column: None,
            undo: Vec::new(),
            redo: Vec::new(),
            effects: crate::NoopEditorEffects,
            text_byte_limit,
            auto_closer: None,
            line_lex_states: line_lex_states(&current),
        };
        preview.set_text_byte_limit(text_byte_limit)?;

        let minimal = minimal_replacement_edit(&current, replacement);
        if let Ok(Some(transaction)) = preview.preview_edits(
            EditOrigin::FileReload,
            vec![minimal],
            SelectionState::caret(0),
        ) {
            return Ok(vec![transaction]);
        }

        const CHUNK_BYTES: usize = MAX_INSERTED_TEXT_BYTES / 4;
        let mut transactions = Vec::new();

        let mut remaining = current.len();
        while remaining > 0 {
            let mut start = remaining.saturating_sub(CHUNK_BYTES);
            while !current.is_char_boundary(start) {
                start += 1;
            }
            let transaction = preview
                .preview_edits(
                    EditOrigin::FileReload,
                    vec![TextEdit {
                        start_byte: start as u64,
                        end_byte: remaining as u64,
                        inserted_text: String::new(),
                    }],
                    SelectionState::caret(0),
                )?
                .expect("a non-empty deletion changes the preview document");
            preview.apply_transaction(transaction.clone())?;
            transactions.push(transaction);
            remaining = start;
        }

        let mut inserted = 0;
        while inserted < replacement.len() {
            let mut end = (inserted + CHUNK_BYTES).min(replacement.len());
            while !replacement.is_char_boundary(end) {
                end -= 1;
            }
            let transaction = preview
                .preview_edits(
                    EditOrigin::FileReload,
                    vec![TextEdit {
                        start_byte: inserted as u64,
                        end_byte: inserted as u64,
                        inserted_text: replacement[inserted..end].to_owned(),
                    }],
                    SelectionState::caret(0),
                )?
                .expect("a non-empty insertion changes the preview document");
            preview.apply_transaction(transaction.clone())?;
            transactions.push(transaction);
            inserted = end;
        }

        debug_assert_eq!(preview.text(), replacement);
        Ok(transactions)
    }

    /// Builds one exact formatter transaction without mutating the document.
    ///
    /// Formatting is accepted per document, so a result that cannot fit one
    /// bounded transaction is rejected instead of being mislabeled as several
    /// independent formatter operations.
    pub fn preview_formatter_replacement(
        &self,
        replacement: &str,
    ) -> Result<Option<EditorTransaction>, TransactionError> {
        self.preview_tool_replacement(EditOrigin::Formatter, replacement)
    }

    /// Builds one exact tool-origin transaction without mutating the document.
    pub fn preview_tool_replacement(
        &self,
        origin: EditOrigin,
        replacement: &str,
    ) -> Result<Option<EditorTransaction>, TransactionError> {
        let current = self.text();
        if current == replacement {
            return Ok(None);
        }
        let edit = minimal_replacement_edit(&current, replacement);
        let selection_after = SelectionState::new(
            map_replacement_offset(self.selection.anchor_byte, &edit),
            map_replacement_offset(self.selection.active_byte, &edit),
        );
        self.preview_edits(origin, vec![edit], selection_after)
    }

    fn active_char(&self) -> usize {
        self.byte_to_char(self.selection.active_byte)
    }

    fn byte_at_visual_position(&self, line: usize, column: usize) -> u64 {
        let line = line.min(self.line_count().saturating_sub(1));
        let range = self.line_content_range(line);
        let text = self.rope.slice(range.clone()).to_string();
        let local_char = char_offset_at_display_column(&text, column);
        self.rope.char_to_byte(range.start + local_char) as u64
    }

    fn byte_to_char(&self, byte: u64) -> usize {
        Positions::new(&self.rope, &self.document_id, self.version)
            .byte_to_scalar(ByteOffset(byte))
            .expect("selection is a validated UTF-8 boundary")
            .0
    }

    fn vertical_destination(&self, delta: isize) -> usize {
        let position = self.cursor();
        let preferred = self.preferred_column.unwrap_or(position.display_column);
        let target_line = position
            .line
            .saturating_add_signed(delta)
            .min(self.line_count().saturating_sub(1));
        let range = self.line_content_range(target_line);
        let line = self.rope.slice(range.clone()).to_string();
        range.start + char_offset_at_display_column(&line, preferred)
    }

    fn line_content_range(&self, line: usize) -> Range<usize> {
        line_content_range(&self.rope, line).expect("editor line is in bounds")
    }
}

fn map_replacement_offset(offset: u64, edit: &TextEdit) -> u64 {
    if offset <= edit.start_byte {
        offset
    } else if offset >= edit.end_byte {
        offset
            .saturating_sub(edit.end_byte - edit.start_byte)
            .saturating_add(edit.inserted_text.len() as u64)
    } else {
        edit.start_byte
            .saturating_add(edit.inserted_text.len() as u64)
    }
}

fn previous_word_boundary(rope: &Rope, cursor: usize) -> usize {
    let cursor_line = rope.char_to_line(cursor);
    let mut line = cursor_line;
    loop {
        let range = line_content_range(rope, line).expect("editor line is in bounds");
        let text = rope.slice(range.clone()).to_string();
        let local_char = if line == cursor_line {
            cursor.saturating_sub(range.start).min(range.len())
        } else {
            range.len()
        };
        let local_byte = text
            .char_indices()
            .nth(local_char)
            .map_or(text.len(), |(byte, _)| byte);
        if let Some(word) = navigation_ranges(&text)
            .into_iter()
            .rev()
            .find(|word| word.start < local_byte)
        {
            return range.start + text[..word.start].chars().count();
        }
        if line == 0 {
            return 0;
        }
        line -= 1;
    }
}

fn next_word_boundary(rope: &Rope, cursor: usize) -> usize {
    let cursor_line = rope.char_to_line(cursor);
    let last_line = rope.len_lines().saturating_sub(1);
    let mut line = cursor_line;
    loop {
        let range = line_content_range(rope, line).expect("editor line is in bounds");
        let text = rope.slice(range.clone()).to_string();
        let local_char = if line == cursor_line {
            cursor.saturating_sub(range.start).min(range.len())
        } else {
            0
        };
        let local_byte = text
            .char_indices()
            .nth(local_char)
            .map_or(text.len(), |(byte, _)| byte);
        if let Some(word) = navigation_ranges(&text)
            .into_iter()
            .find(|word| word.end > local_byte)
        {
            return range.start + text[..word.end].chars().count();
        }
        if line == last_line {
            return rope.len_chars();
        }
        line += 1;
    }
}

fn navigation_ranges(text: &str) -> Vec<Range<usize>> {
    text.split_word_bound_indices()
        .flat_map(|(segment_start, segment)| {
            let is_word = segment.unicode_words().next().is_some();
            segment
                .grapheme_indices(true)
                .filter_map(move |(grapheme_start, grapheme)| {
                    if grapheme.chars().all(char::is_whitespace) {
                        None
                    } else if is_word && grapheme_start != 0 {
                        None
                    } else {
                        let start = segment_start + grapheme_start;
                        let end = if is_word {
                            segment_start + segment.len()
                        } else {
                            start + grapheme.len()
                        };
                        Some(start..end)
                    }
                })
        })
        .collect()
}

fn byte_line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind('\n').map_or(0, |newline| newline + 1)
}

fn line_at(text: &str, start: usize) -> &str {
    let end = text[start..]
        .find('\n')
        .map_or(text.len(), |newline| start + newline);
    text[start..end]
        .strip_suffix('\r')
        .unwrap_or(&text[start..end])
}

fn selected_line_byte_starts(text: &str, selection: SelectionState) -> Vec<usize> {
    let start = selection.anchor_byte.min(selection.active_byte) as usize;
    let end = selection.anchor_byte.max(selection.active_byte) as usize;
    let first_line = byte_line_start(text, start);
    let last_offset = if start != end && byte_line_start(text, end) == end {
        end.saturating_sub(1)
    } else {
        end
    };
    let last_line = byte_line_start(text, last_offset);
    let mut starts = vec![first_line];
    let mut current = first_line;
    while current < last_line {
        let Some(newline) = text[current..].find('\n') else {
            break;
        };
        current += newline + 1;
        if current <= last_line {
            starts.push(current);
        }
    }
    starts
}

fn transform_edit_offset(offset: u64, edits: &[TextEdit]) -> u64 {
    let original = i128::from(offset);
    let mut delta = 0_i128;
    for edit in edits {
        let start = i128::from(edit.start_byte);
        let end = i128::from(edit.end_byte);
        let inserted = edit.inserted_text.len() as i128;
        if original < start {
            break;
        }
        if start != end && original <= end {
            return u64::try_from(start + delta + inserted)
                .expect("validated edit offsets remain non-negative");
        }
        delta += inserted - (end - start);
    }
    u64::try_from(original + delta).expect("validated edit offsets remain non-negative")
}

fn leading_indentation(text: &str) -> &str {
    let end = text
        .char_indices()
        .find_map(|(byte, character)| (!is_indentation(character)).then_some(byte))
        .unwrap_or(text.len());
    &text[..end]
}

fn is_indentation(character: char) -> bool {
    matches!(character, ' ' | '\t')
}

fn is_identifier_character(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

fn previous_grapheme_is_identifier(text: &str) -> bool {
    text.graphemes(true)
        .next_back()
        .is_some_and(|grapheme| grapheme.chars().any(is_identifier_character))
}

fn line_lex_states(text: &str) -> Vec<RustLexState> {
    let mut states = Vec::new();
    let mut state = RustLexState::Code;
    for line in text.split_inclusive('\n') {
        states.push(state);
        state = scan_rust_lex_state(line, state);
    }
    if text.is_empty() || text.ends_with('\n') {
        states.push(state);
    }
    states
}

fn scan_rust_lex_state(text: &str, mut state: RustLexState) -> RustLexState {
    let bytes = text.as_bytes();
    let mut byte = 0;
    let mut previous_is_identifier = false;
    while byte < bytes.len() {
        match state {
            RustLexState::Code => {
                if bytes[byte..].starts_with(b"//") {
                    state = RustLexState::LineComment;
                    previous_is_identifier = false;
                    byte += 2;
                } else if bytes[byte..].starts_with(b"/*") {
                    state = RustLexState::BlockComment { depth: 1 };
                    previous_is_identifier = false;
                    byte += 2;
                } else if bytes[byte] == b'r' && !previous_is_identifier {
                    if let Some((hashes, opener_bytes)) = raw_string_opener(&bytes[byte..]) {
                        state = RustLexState::RawString { hashes };
                        byte += opener_bytes;
                    } else {
                        previous_is_identifier = true;
                        byte += 1;
                    }
                } else if bytes[byte] == b'"' {
                    state = RustLexState::String { escaped: false };
                    previous_is_identifier = false;
                    byte += 1;
                } else if bytes[byte] == b'\''
                    && let Some(literal_bytes) = char_literal_bytes(&text[byte..])
                {
                    previous_is_identifier = false;
                    byte += literal_bytes;
                } else {
                    let character = text[byte..]
                        .chars()
                        .next()
                        .expect("byte offset remains on a character boundary");
                    previous_is_identifier = is_identifier_character(character);
                    byte += character.len_utf8();
                }
            }
            RustLexState::String { escaped } => {
                let character = text[byte..]
                    .chars()
                    .next()
                    .expect("byte offset remains on a character boundary");
                state = if escaped {
                    RustLexState::String { escaped: false }
                } else if character == '\\' {
                    RustLexState::String { escaped: true }
                } else if character == '"' {
                    RustLexState::Code
                } else {
                    RustLexState::String { escaped: false }
                };
                byte += character.len_utf8();
            }
            RustLexState::RawString { hashes } => {
                if bytes[byte] == b'"'
                    && bytes[byte + 1..]
                        .get(..hashes)
                        .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
                {
                    state = RustLexState::Code;
                    byte += 1 + hashes;
                } else {
                    let character = text[byte..]
                        .chars()
                        .next()
                        .expect("byte offset remains on a character boundary");
                    byte += character.len_utf8();
                }
            }
            RustLexState::LineComment => {
                let character = text[byte..]
                    .chars()
                    .next()
                    .expect("byte offset remains on a character boundary");
                if character == '\n' {
                    state = RustLexState::Code;
                    previous_is_identifier = false;
                }
                byte += character.len_utf8();
            }
            RustLexState::BlockComment { depth } => {
                if bytes[byte..].starts_with(b"/*") {
                    state = RustLexState::BlockComment { depth: depth + 1 };
                    byte += 2;
                } else if bytes[byte..].starts_with(b"*/") {
                    state = if depth == 1 {
                        previous_is_identifier = false;
                        RustLexState::Code
                    } else {
                        RustLexState::BlockComment { depth: depth - 1 }
                    };
                    byte += 2;
                } else {
                    let character = text[byte..]
                        .chars()
                        .next()
                        .expect("byte offset remains on a character boundary");
                    byte += character.len_utf8();
                }
            }
        }
    }
    state
}

fn raw_string_opener(text: &[u8]) -> Option<(usize, usize)> {
    if text.first() != Some(&b'r') {
        return None;
    }
    let hashes = text[1..].iter().take_while(|byte| **byte == b'#').count();
    (text.get(1 + hashes) == Some(&b'"')).then_some((hashes, hashes + 2))
}

fn char_literal_bytes(text: &str) -> Option<usize> {
    let mut characters = text.char_indices();
    if characters.next()?.1 != '\'' {
        return None;
    }
    let (_, first) = characters.next()?;
    if matches!(first, '\r' | '\n') {
        return None;
    }
    if first != '\\' {
        let (closing_byte, closing) = characters.next()?;
        return (closing == '\'').then_some(closing_byte + closing.len_utf8());
    }

    let mut escaped = true;
    for (byte, character) in characters {
        if matches!(character, '\r' | '\n') {
            return None;
        }
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '\'' {
            return Some(byte + character.len_utf8());
        }
    }
    None
}

fn newline_at(text: &str, cursor: usize) -> &'static str {
    if let Some(newline) = text[cursor..].find('\n').map(|byte| cursor + byte) {
        return if newline > 0 && text.as_bytes()[newline - 1] == b'\r' {
            "\r\n"
        } else {
            "\n"
        };
    }
    text[..cursor]
        .rfind('\n')
        .filter(|newline| *newline > 0 && text.as_bytes()[newline - 1] == b'\r')
        .map_or("\n", |_| "\r\n")
}

fn opening_delimiter(closer: char) -> Option<char> {
    match closer {
        ')' => Some('('),
        ']' => Some('['),
        '}' => Some('{'),
        _ => None,
    }
}

fn is_bracket(character: char) -> bool {
    matches!(character, '(' | ')' | '[' | ']' | '{' | '}')
}

fn matching_bracket_in_text(text: &str, byte: usize, bracket: char) -> Option<usize> {
    let (opener, closer, forward) = match bracket {
        '(' => ('(', ')', true),
        ')' => ('(', ')', false),
        '[' => ('[', ']', true),
        ']' => ('[', ']', false),
        '{' => ('{', '}', true),
        '}' => ('{', '}', false),
        _ => return None,
    };
    let mut depth = 0_usize;
    if forward {
        for (relative, character) in text[byte + bracket.len_utf8()..].char_indices() {
            if character == opener {
                depth = depth.saturating_add(1);
            } else if character == closer {
                if depth == 0 {
                    return Some(byte + bracket.len_utf8() + relative);
                }
                depth -= 1;
            }
        }
    } else {
        for (candidate, character) in text[..byte].char_indices().rev() {
            if character == closer {
                depth = depth.saturating_add(1);
            } else if character == opener {
                if depth == 0 {
                    return Some(candidate);
                }
                depth -= 1;
            }
        }
    }
    None
}

fn closing_delimiter(opener: char) -> Option<char> {
    match opener {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        _ => None,
    }
}

fn matching_opening_before(text: &str, cursor: usize, opener: char, closer: char) -> Option<usize> {
    let mut depth = 0_usize;
    for (byte, character) in text[..cursor].char_indices().rev() {
        if character == closer {
            depth = depth.saturating_add(1);
        } else if character == opener {
            if depth == 0 {
                return Some(byte);
            }
            depth -= 1;
        }
    }
    None
}

fn remove_one_indentation_level(indentation: &str) -> String {
    if let Some(without_tab) = indentation.strip_suffix('\t') {
        return without_tab.to_owned();
    }
    let spaces = indentation
        .as_bytes()
        .iter()
        .rev()
        .take(4)
        .take_while(|byte| **byte == b' ')
        .count();
    indentation[..indentation.len() - spaces].to_owned()
}

/// Applies a recorded transaction without reconstructing live undo history.
///
/// Checkpoints do not persist undo/redo stacks. This path shares the live
/// editor's complete transaction preparation and materialization validation,
/// but treats the recorded origin as provenance and retains no history.
pub(crate) fn apply_recorded_transaction(
    document_id: &DocumentId,
    text: &str,
    version: u64,
    selection: SelectionState,
    transaction: &EditorTransaction,
) -> Result<Option<String>, TransactionError> {
    validate_selection(SelectionField::Current, selection, text)?;
    let editor = EditorBuffer {
        document_id: document_id.clone(),
        rope: Rope::from_str(text),
        version,
        selection,
        hash: document_hash(text),
        preferred_column: None,
        undo: Vec::new(),
        redo: Vec::new(),
        effects: crate::NoopEditorEffects,
        text_byte_limit: usize::MAX,
        auto_closer: None,
        line_lex_states: line_lex_states(text),
    };
    editor
        .prepare_transaction(transaction, None, false)
        .map(|prepared| prepared.map(|prepared| prepared.after_text))
}

pub(crate) fn validate_recorded_selection(
    text: &str,
    selection: SelectionState,
) -> Result<(), TransactionError> {
    validate_selection(SelectionField::Current, selection, text)
}

pub(crate) fn validate_transaction_structure(
    transaction: &EditorTransaction,
) -> Result<(), TransactionError> {
    transaction.validate().map_err(map_model_structure_error)
}

fn apply_edits_to_text(before: &str, edits: &[TextEdit]) -> Result<String, TransactionError> {
    validate_edits_structure(edits)?;

    let mut ranges = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let start = offset_to_usize(index, edit.start_byte, before.len())?;
        let end = offset_to_usize(index, edit.end_byte, before.len())?;
        if !before.is_char_boundary(start) {
            return Err(TransactionError::EditOffsetNotUtf8Boundary {
                edit_index: index,
                offset: edit.start_byte,
            });
        }
        if !before.is_char_boundary(end) {
            return Err(TransactionError::EditOffsetNotUtf8Boundary {
                edit_index: index,
                offset: edit.end_byte,
            });
        }
        ranges.push(start..end);
    }

    let mut after = before.to_owned();
    for (edit, range) in edits.iter().zip(ranges).rev() {
        after.replace_range(range, &edit.inserted_text);
    }
    Ok(after)
}

fn minimal_replacement_edit(current: &str, replacement: &str) -> TextEdit {
    let prefix = current
        .chars()
        .zip(replacement.chars())
        .take_while(|(current, replacement)| current == replacement)
        .map(|(character, _)| character.len_utf8())
        .sum::<usize>();
    let suffix = current[prefix..]
        .chars()
        .rev()
        .zip(replacement[prefix..].chars().rev())
        .take_while(|(current, replacement)| current == replacement)
        .map(|(character, _)| character.len_utf8())
        .sum::<usize>();

    TextEdit {
        start_byte: prefix as u64,
        end_byte: (current.len() - suffix) as u64,
        inserted_text: replacement[prefix..replacement.len() - suffix].to_owned(),
    }
}

fn validate_edits_structure(edits: &[TextEdit]) -> Result<(), TransactionError> {
    validate_text_edits(edits).map_err(map_model_structure_error)
}

fn map_model_structure_error(error: ValidationError) -> TransactionError {
    match error {
        ValidationError::InvalidVersionTransition { before, after } => {
            TransactionError::VersionAfterNotNext { before, after }
        }
        ValidationError::EditsNotCanonical {
            previous_index,
            edit_index,
        } => TransactionError::EditsNotCanonical {
            previous_index,
            edit_index,
        },
        ValidationError::EditsOverlap {
            previous_index,
            edit_index,
        } => TransactionError::EditsOverlap {
            previous_index,
            edit_index,
        },
        error => TransactionError::ModelValidation(error),
    }
}

fn offset_to_usize(
    edit_index: usize,
    offset: u64,
    text_len: usize,
) -> Result<usize, TransactionError> {
    let converted =
        usize::try_from(offset).map_err(|_| TransactionError::EditOffsetOutOfBounds {
            edit_index,
            offset,
            text_len,
        })?;
    if converted > text_len {
        Err(TransactionError::EditOffsetOutOfBounds {
            edit_index,
            offset,
            text_len,
        })
    } else {
        Ok(converted)
    }
}

fn validate_selection(
    field: SelectionField,
    selection: SelectionState,
    text: &str,
) -> Result<(), TransactionError> {
    for offset in [selection.anchor_byte, selection.active_byte] {
        let converted =
            usize::try_from(offset).map_err(|_| TransactionError::SelectionOffsetOutOfBounds {
                field: selection_field_name(field),
                offset,
                text_len: text.len(),
            })?;
        if converted > text.len() {
            return Err(TransactionError::SelectionOffsetOutOfBounds {
                field: selection_field_name(field),
                offset,
                text_len: text.len(),
            });
        }
        if !text.is_char_boundary(converted) {
            return Err(TransactionError::SelectionOffsetNotUtf8Boundary {
                field: selection_field_name(field),
                offset,
            });
        }
    }
    Ok(())
}

const fn selection_field_name(field: SelectionField) -> &'static str {
    match field {
        SelectionField::Before => "selection_before",
        SelectionField::After => "selection_after",
        SelectionField::Current => "selection",
    }
}

fn build_history_entry(
    before: &str,
    transaction: &EditorTransaction,
) -> Result<HistoryEntry, TransactionError> {
    let mut inverse_edits = inverse_edits(before, &transaction.edits)?;
    validate_history_edits_encodable(
        &transaction.document_id,
        &mut inverse_edits,
        EditOrigin::Undo,
    )?;

    let mut forward_edits = transaction.edits.clone();
    validate_history_edits_encodable(
        &transaction.document_id,
        &mut forward_edits,
        EditOrigin::Redo,
    )?;

    Ok(HistoryEntry {
        forward_edits,
        inverse_edits,
        selection_before: transaction.selection_before,
        selection_after: transaction.selection_after,
    })
}

fn validate_history_edits_encodable(
    document_id: &DocumentId,
    edits: &mut Vec<TextEdit>,
    origin: EditOrigin,
) -> Result<(), TransactionError> {
    // Maximum-width counters and the maximum selection offset reserve enough
    // JSON space for this history entry to remain usable at any later version.
    let transaction = EditorTransaction {
        document_id: document_id.clone(),
        version_before: u64::MAX - 1,
        version_after: u64::MAX,
        origin,
        edits: std::mem::take(edits),
        selection_before: SelectionState::caret(u64::MAX),
        selection_after: SelectionState::caret(u64::MAX),
        hash_before: Hash::zero(),
        hash_after: Hash::zero(),
    };
    let result = validate_transaction_encodable(&transaction).map_err(|error| {
        TransactionError::HistoryNotRepresentable {
            reason: error.to_string(),
        }
    });
    *edits = transaction.edits;
    result
}

fn inverse_edits(before: &str, edits: &[TextEdit]) -> Result<Vec<TextEdit>, TransactionError> {
    let deleted_bytes = edits.iter().try_fold(0_usize, |total, edit| {
        let deleted = usize::try_from(edit.end_byte - edit.start_byte).map_err(|_| {
            TransactionError::HistoryNotRepresentable {
                reason: "deleted byte count does not fit this platform".to_owned(),
            }
        })?;
        total
            .checked_add(deleted)
            .ok_or_else(|| TransactionError::HistoryNotRepresentable {
                reason: "deleted byte count overflowed".to_owned(),
            })
    })?;
    if deleted_bytes > MAX_TRANSACTION_JSON_BYTES {
        return Err(TransactionError::HistoryNotRepresentable {
            reason: format!(
                "deleted text is {deleted_bytes} bytes before JSON encoding; maximum transaction size is {MAX_TRANSACTION_JSON_BYTES} bytes"
            ),
        });
    }

    let mut inverse = Vec::with_capacity(edits.len());
    let mut byte_delta = 0_i128;
    for edit in edits {
        let start = edit.start_byte as usize;
        let end = edit.end_byte as usize;
        let adjusted_start = i128::from(edit.start_byte) + byte_delta;
        let inverse_start = u64::try_from(adjusted_start).map_err(|_| {
            TransactionError::HistoryNotRepresentable {
                reason: "inverse edit offset is outside the u64 range".to_owned(),
            }
        })?;
        let inverse_end = inverse_start
            .checked_add(edit.inserted_text.len() as u64)
            .ok_or_else(|| TransactionError::HistoryNotRepresentable {
                reason: "inverse edit range overflowed".to_owned(),
            })?;
        let deleted = &before[start..end];

        if deleted.is_empty() {
            push_inverse_edit(&mut inverse, inverse_start, inverse_end, "")?;
        } else {
            let mut chunk_start = 0;
            while chunk_start < deleted.len() {
                let mut chunk_end = (chunk_start + MAX_INSERTED_TEXT_BYTES).min(deleted.len());
                while !deleted.is_char_boundary(chunk_end) {
                    chunk_end -= 1;
                }
                let range_end = if chunk_end == deleted.len() {
                    inverse_end
                } else {
                    inverse_start
                };
                push_inverse_edit(
                    &mut inverse,
                    inverse_start,
                    range_end,
                    &deleted[chunk_start..chunk_end],
                )?;
                chunk_start = chunk_end;
            }
        }
        byte_delta += edit.inserted_text.len() as i128 - (end - start) as i128;
    }
    Ok(inverse)
}

fn push_inverse_edit(
    inverse: &mut Vec<TextEdit>,
    start_byte: u64,
    end_byte: u64,
    inserted_text: &str,
) -> Result<(), TransactionError> {
    if inverse.len() == MAX_VECTOR_ITEMS {
        return Err(TransactionError::HistoryNotRepresentable {
            reason: format!("inverse requires more than {MAX_VECTOR_ITEMS} edits"),
        });
    }
    inverse.push(TextEdit {
        start_byte,
        end_byte,
        inserted_text: inserted_text.to_owned(),
    });
    Ok(())
}

fn previous_grapheme_boundary(rope: &Rope, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let text = rope.to_string();
    let cursor_byte = rope.char_to_byte(cursor);
    text[..cursor_byte]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(byte, _)| text[..byte].chars().count())
}

fn next_grapheme_boundary(rope: &Rope, cursor: usize) -> usize {
    if cursor == rope.len_chars() {
        return cursor;
    }
    let text = rope.to_string();
    let cursor_byte = rope.char_to_byte(cursor);
    let grapheme_chars = text[cursor_byte..]
        .graphemes(true)
        .next()
        .map_or(0, |grapheme| grapheme.chars().count());
    cursor + grapheme_chars
}

fn normalize_grapheme_byte(text: &str, cursor_byte: usize) -> usize {
    if cursor_byte == 0 || cursor_byte == text.len() {
        return cursor_byte;
    }
    text.grapheme_indices(true)
        .map(|(byte, _)| byte)
        .find(|byte| *byte >= cursor_byte)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoopEditorEffects;

    #[test]
    fn undo_budget_counts_and_releases_spare_entry_allocations() {
        let mut editor = EditorBuffer::new(
            DocumentId::new("budget-document").unwrap(),
            "",
            NoopEditorEffects,
        );
        editor.undo.reserve(32);
        editor.redo.reserve(16);
        assert!(editor.retained_history_bytes() >= 48 * std::mem::size_of::<HistoryEntry>());
        editor.trim_history_to(0);
        assert_eq!(editor.undo.capacity(), 0);
        assert_eq!(editor.redo.capacity(), 0);
        assert_eq!(editor.retained_history_bytes(), 0);
    }

    #[test]
    fn aggregate_codec_preflight_is_a_pre_materialization_boundary() {
        let editor = EditorBuffer::new(
            DocumentId::new("preflight-document").unwrap(),
            "",
            NoopEditorEffects,
        );
        let maximum_text = "x".repeat(MAX_INSERTED_TEXT_BYTES);
        let edits = (0..4)
            .map(|_| TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: maximum_text.clone(),
            })
            .collect();

        assert!(matches!(
            editor.preflight_edits(
                EditOrigin::Formatter,
                edits,
                SelectionState::caret((4 * MAX_INSERTED_TEXT_BYTES) as u64),
            ),
            Err(TransactionError::TransactionTooLarge { .. })
        ));
    }

    #[test]
    fn preflight_hash_placeholder_has_the_exact_final_wire_size() {
        let editor = EditorBuffer::new(
            DocumentId::new("preflight-document").unwrap(),
            "",
            NoopEditorEffects,
        );
        let placeholder = editor
            .preflight_edits(
                EditOrigin::Keyboard,
                vec![TextEdit {
                    start_byte: 0,
                    end_byte: 0,
                    inserted_text: "x".to_owned(),
                }],
                SelectionState::caret(1),
            )
            .unwrap();
        let placeholder_len = crate::encode_transaction(&placeholder).unwrap().len();
        let actual = EditorTransaction {
            hash_after: document_hash("x"),
            ..placeholder
        };

        assert_eq!(
            crate::encode_transaction(&actual).unwrap().len(),
            placeholder_len
        );
    }
}
