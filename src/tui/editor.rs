use std::cell::{Ref, RefCell};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use rustrace_model::{
    DocumentId, EditorTransaction, Hash, MAX_INSERTED_TEXT_BYTES, MAX_VECTOR_ITEMS, SelectionState,
    TextEdit,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::config::PrimaryModifier;
use crate::editor::{
    EditOrigin, EditorBuffer, EditorEffects, HighlightSpan, Movement, SyntaxEffects, SyntaxState,
    TransactionError, Viewport,
};
use crate::ghostty::GhosttyKeyBindings;

const INDENT_WIDTH: usize = 4;

pub const EDITOR_KEY_HINTS: &str = "Arrows Home End PageUp PageDown | Shift+movement select | Backspace/Delete | F5 previous F6 next\n\
Edit: Tab indent | Shift-Tab outdent | Ctrl-/ comment | {select-all} | Ctrl-C Ctrl-X Ctrl-V | Ctrl-Z undo Ctrl-Y redo\n\
Cmd: Ctrl-Space complete | Ctrl-F/F3 find | Ctrl-S save | Ctrl-Tab/Ctrl-BackTab switch | Ctrl-W delete Ctrl-Q quit";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditorCommand {
    Insert(char),
    DeleteBackward,
    DeleteForward,
    DeletePreviousWord,
    DeleteToLineStart,
    DeleteToLineEnd,
    Move {
        movement: Movement,
        selecting: bool,
    },
    MoveTo {
        line: usize,
        column: usize,
        selecting: bool,
    },
    SelectWord,
    PageUp {
        lines: usize,
        selecting: bool,
    },
    PageDown {
        lines: usize,
        selecting: bool,
    },
    SelectAll,
    Copy,
    Cut,
    Paste,
    PasteExternal(String),
    Undo,
    Redo,
    Indent,
    Outdent,
    ToggleComment,
    Search(String),
    SearchNext,
    ReplaceCurrent {
        query: String,
        replacement: String,
    },
    ReplaceAll {
        query: String,
        replacement: String,
    },
    NextBuffer,
    PreviousBuffer,
    CloseActive,
    RequestQuit,
    ConfirmDiscard,
    CancelDiscard,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SearchOutcome {
    Match {
        start_byte: u64,
        end_byte: u64,
        wrapped: bool,
    },
    NoMatch,
    EmptyQuery,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SearchSummary {
    pub current: Option<usize>,
    pub total: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DestructiveAction {
    CloseBuffer,
    Quit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditorOutcome {
    NoChange,
    Edited,
    Replaced(usize),
    SelectionChanged,
    Copied,
    Search(SearchOutcome),
    BufferSwitched,
    ConfirmationRequired(DestructiveAction),
    BufferClosed,
    LastBuffer,
    Quit,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionInput {
    Command(EditorCommand),
    Save,
    BeginFind,
    Complete,
}

pub trait EditorStorage {
    fn write(&mut self, path: &Path, contents: &[u8]) -> io::Result<()>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FileSystemStorage;

impl EditorStorage for FileSystemStorage {
    fn write(&mut self, path: &Path, contents: &[u8]) -> io::Result<()> {
        fs::write(path, contents)
    }
}

#[derive(Clone, Debug)]
struct SearchState {
    query: String,
    last_match: Option<(usize, usize)>,
    match_version: u64,
}

struct OpenBuffer<E>
where
    E: EditorEffects,
{
    editor: EditorBuffer<SyntaxEffects<E>>,
    path: PathBuf,
    viewport: Viewport,
    saved_hash: Hash,
    syntax: Rc<RefCell<SyntaxState>>,
    search: Option<SearchState>,
}

impl<E> OpenBuffer<E>
where
    E: EditorEffects,
{
    fn new(document_id: DocumentId, path: PathBuf, initial: &str, effects: E) -> Self {
        let syntax = Rc::new(RefCell::new(SyntaxState::new(
            document_id.clone(),
            &path,
            initial,
        )));
        let editor = EditorBuffer::new(
            document_id,
            initial,
            SyntaxEffects::new(effects, syntax.clone()),
        );
        let saved_hash = editor.hash();
        Self {
            editor,
            path,
            viewport: Viewport::default(),
            saved_hash,
            syntax,
            search: None,
        }
    }

    fn is_dirty(&self) -> bool {
        self.editor.hash() != self.saved_hash
    }
}

#[derive(Clone, Debug)]
enum PendingConfirmation {
    CloseBuffer(DocumentId),
    Quit,
}

/// Owns interactive state around one or more Rustrace editor buffers.
///
/// Content changes are deliberately delegated to [`EditorBuffer`]. The
/// session owns only user-facing behavior such as clipboard, viewport, search,
/// saved hashes, and destructive confirmation, so every mutation still crosses
/// the editor's transaction gateway exactly once. Copying or cutting an empty
/// selection is a no-op.
pub struct EditorSession<E>
where
    E: EditorEffects,
{
    buffers: Vec<OpenBuffer<E>>,
    active_index: usize,
    clipboard: String,
    pending_confirmation: Option<PendingConfirmation>,
}

impl<E> EditorSession<E>
where
    E: EditorEffects,
{
    pub fn new(document_id: DocumentId, path: PathBuf, initial: &str, effects: E) -> Self {
        Self {
            buffers: vec![OpenBuffer::new(document_id, path, initial, effects)],
            active_index: 0,
            clipboard: String::new(),
            pending_confirmation: None,
        }
    }

    pub(crate) fn recover_buffer(
        &mut self,
        document_id: &DocumentId,
        text: &str,
        version: u64,
        selection: rustrace_model::SelectionState,
        saved_hash: Hash,
        effects: E,
    ) -> Result<(), TransactionError> {
        let buffer = self
            .buffers
            .iter_mut()
            .find(|buffer| buffer.editor.document_id() == document_id)
            .expect("recovered buffer was opened");
        buffer.syntax = Rc::new(RefCell::new(SyntaxState::from_recovered(
            document_id.clone(),
            &buffer.path,
            text,
            version,
        )));
        buffer.editor = EditorBuffer::from_recovered(
            document_id.clone(),
            text,
            version,
            selection,
            SyntaxEffects::new(effects, buffer.syntax.clone()),
        )?;
        buffer.saved_hash = saved_hash;
        Ok(())
    }

    pub fn open_buffer(
        &mut self,
        document_id: DocumentId,
        path: PathBuf,
        initial: &str,
        effects: E,
    ) -> usize {
        self.clear_transient_edit_state();
        let index = self.open_buffer_inactive(document_id, path, initial, effects);
        self.active_index = index;
        index
    }

    pub(crate) fn open_buffer_inactive(
        &mut self,
        document_id: DocumentId,
        path: PathBuf,
        initial: &str,
        effects: E,
    ) -> usize {
        self.buffers
            .push(OpenBuffer::new(document_id, path, initial, effects));
        self.buffers.len() - 1
    }

    pub(crate) fn trim_history_to(&mut self, maximum: usize) -> bool {
        let mut remaining = maximum;
        let mut evicted = false;
        // Prefer the active buffer; eviction does not affect transaction history on disk.
        let active = self.active_index;
        for index in std::iter::once(active).chain((0..self.buffers.len()).filter(|i| *i != active))
        {
            let buffer = &mut self.buffers[index].editor;
            evicted |= buffer.trim_history_to(remaining);
            remaining = remaining.saturating_sub(buffer.retained_history_bytes());
        }
        evicted
    }

    pub(crate) fn retained_history_bytes(&self) -> usize {
        self.buffers
            .iter()
            .map(|b| b.editor.retained_history_bytes())
            .sum()
    }

    pub fn buffer_count(&self) -> usize {
        self.buffers.len()
    }

    pub fn active_index(&self) -> usize {
        self.active_index
    }

    pub fn buffer(&self, index: usize) -> Option<&EditorBuffer<SyntaxEffects<E>>> {
        self.buffers.get(index).map(|buffer| &buffer.editor)
    }

    pub fn active_buffer(&self) -> &EditorBuffer<SyntaxEffects<E>> {
        &self.active().editor
    }

    pub fn active_buffer_mut(&mut self) -> &mut EditorBuffer<SyntaxEffects<E>> {
        &mut self.active_mut().editor
    }

    pub fn set_active_text_byte_limit(&mut self, maximum: usize) -> Result<(), TransactionError> {
        self.active_buffer_mut().set_text_byte_limit(maximum)
    }

    pub fn buffer_by_document_id(
        &self,
        document_id: &DocumentId,
    ) -> Option<&EditorBuffer<SyntaxEffects<E>>> {
        self.buffers
            .iter()
            .find(|buffer| buffer.editor.document_id() == document_id)
            .map(|buffer| &buffer.editor)
    }

    pub fn set_document_selection(
        &mut self,
        document_id: &DocumentId,
        selection: SelectionState,
    ) -> Result<bool, TransactionError> {
        let Some(buffer) = self
            .buffers
            .iter_mut()
            .find(|buffer| buffer.editor.document_id() == document_id)
        else {
            return Ok(false);
        };
        buffer.editor.set_selection(selection)
    }

    pub fn active_document_id(&self) -> &DocumentId {
        self.active_buffer().document_id()
    }

    pub fn active_path(&self) -> &Path {
        &self.active().path
    }

    pub fn paths(&self) -> impl ExactSizeIterator<Item = &Path> {
        self.buffers.iter().map(|buffer| buffer.path.as_path())
    }

    /// Cached derived spans; borrowing them never parses or changes provenance.
    pub fn active_highlights(&self) -> Ref<'_, [HighlightSpan]> {
        Ref::map(self.active().syntax.borrow(), |syntax| {
            syntax.spans.as_slice()
        })
    }

    pub fn active_viewport(&self) -> &Viewport {
        &self.active().viewport
    }

    pub fn active_viewport_mut(&mut self) -> &mut Viewport {
        &mut self.active_mut().viewport
    }

    pub fn clipboard(&self) -> &str {
        &self.clipboard
    }

    pub fn is_active_dirty(&self) -> bool {
        self.active().is_dirty()
    }

    pub fn selected_text_for_find(&self) -> Option<String> {
        let selection = self.active_buffer().selection_state();
        if selection.is_caret() {
            return None;
        }
        let start = selection.anchor_byte.min(selection.active_byte) as usize;
        let end = selection.anchor_byte.max(selection.active_byte) as usize;
        let text = self.active_buffer().text();
        let selected = &text[start..end];
        (!selected.contains(['\r', '\n'])).then(|| selected.to_owned())
    }

    pub fn search_summary(&self, query: &str) -> SearchSummary {
        let matches = self.active_buffer().literal_matches(query);
        let total = matches.len();
        if total == 0 {
            return SearchSummary::default();
        }
        let selection = self.active_buffer().selection_state();
        let current = matches
            .iter()
            .position(|matched| *matched == selection)
            .or_else(|| {
                matches
                    .iter()
                    .position(|matched| matched.anchor_byte >= selection.active_byte)
            })
            .unwrap_or(0);
        SearchSummary {
            current: Some(current + 1),
            total,
        }
    }

    pub fn remember_search(&mut self, query: String) {
        if query.is_empty() {
            self.active_mut().search = None;
            return;
        }
        let selection = self.active_buffer().selection_state();
        let last_match = self
            .active_buffer()
            .literal_matches(&query)
            .into_iter()
            .find(|matched| *matched == selection)
            .map(|matched| (matched.anchor_byte as usize, matched.active_byte as usize));
        let match_version = self.active_buffer().version();
        self.active_mut().search = Some(SearchState {
            query,
            last_match,
            match_version,
        });
    }

    pub fn is_document_dirty(&self, document_id: &DocumentId) -> Option<bool> {
        self.buffers
            .iter()
            .find(|buffer| buffer.editor.document_id() == document_id)
            .map(OpenBuffer::is_dirty)
    }

    pub fn quit_confirmation_pending(&self) -> bool {
        matches!(self.pending_confirmation, Some(PendingConfirmation::Quit))
    }

    pub fn preview_document_replacement(
        &self,
        document_id: &DocumentId,
        replacement: &str,
        text_byte_limit: usize,
    ) -> Result<Vec<EditorTransaction>, TransactionError> {
        let Some(buffer) = self
            .buffers
            .iter()
            .find(|buffer| buffer.editor.document_id() == document_id)
        else {
            return Ok(Vec::new());
        };
        buffer
            .editor
            .preview_file_reload(replacement, text_byte_limit)
    }

    pub(crate) fn preview_formatter_replacement(
        &self,
        document_id: &DocumentId,
        replacement: &str,
    ) -> Result<Option<EditorTransaction>, TransactionError> {
        let Some(buffer) = self
            .buffers
            .iter()
            .find(|buffer| buffer.editor.document_id() == document_id)
        else {
            return Ok(None);
        };
        buffer.editor.preview_formatter_replacement(replacement)
    }

    pub(crate) fn preview_tool_replacement(
        &self,
        document_id: &DocumentId,
        origin: EditOrigin,
        replacement: &str,
    ) -> Result<Option<EditorTransaction>, TransactionError> {
        let Some(buffer) = self
            .buffers
            .iter()
            .find(|buffer| buffer.editor.document_id() == document_id)
        else {
            return Ok(None);
        };
        buffer.editor.preview_tool_replacement(origin, replacement)
    }

    pub fn set_document_text_byte_limit(
        &mut self,
        document_id: &DocumentId,
        maximum: usize,
    ) -> Result<bool, TransactionError> {
        let Some(buffer) = self
            .buffers
            .iter_mut()
            .find(|buffer| buffer.editor.document_id() == document_id)
        else {
            return Ok(false);
        };
        buffer.editor.set_text_byte_limit(maximum)?;
        Ok(true)
    }

    pub fn apply_document_transaction(
        &mut self,
        document_id: &DocumentId,
        transaction: EditorTransaction,
    ) -> Result<bool, TransactionError> {
        let Some(buffer) = self
            .buffers
            .iter_mut()
            .find(|buffer| buffer.editor.document_id() == document_id)
        else {
            return Ok(false);
        };
        buffer.editor.apply_transaction(transaction)
    }

    pub fn mark_document_saved(&mut self, document_id: &DocumentId) -> bool {
        let Some(buffer) = self
            .buffers
            .iter_mut()
            .find(|buffer| buffer.editor.document_id() == document_id)
        else {
            return false;
        };
        buffer.saved_hash = buffer.editor.hash();
        true
    }

    pub fn activate_document(&mut self, document_id: &DocumentId) -> EditorOutcome {
        let Some(index) = self
            .buffers
            .iter()
            .position(|buffer| buffer.editor.document_id() == document_id)
        else {
            return EditorOutcome::NoChange;
        };
        if index == self.active_index {
            EditorOutcome::NoChange
        } else {
            self.clear_transient_edit_state();
            self.active_index = index;
            EditorOutcome::BufferSwitched
        }
    }

    pub fn switched_document_id(&self, delta: isize) -> Option<&DocumentId> {
        if self.buffers.len() < 2 {
            return None;
        }
        let len = isize::try_from(self.buffers.len()).ok()?;
        let active = isize::try_from(self.active_index).ok()?;
        let index = usize::try_from((active + delta).rem_euclid(len)).ok()?;
        Some(self.buffers[index].editor.document_id())
    }

    pub fn rename_document(&mut self, document_id: &DocumentId, path: PathBuf) -> bool {
        let Some(buffer) = self
            .buffers
            .iter_mut()
            .find(|buffer| buffer.editor.document_id() == document_id)
        else {
            return false;
        };
        buffer.syntax.borrow_mut().rename(&path);
        buffer.path = path;
        true
    }

    pub fn remove_document(&mut self, document_id: &DocumentId) -> EditorOutcome {
        let Some(index) = self
            .buffers
            .iter()
            .position(|buffer| buffer.editor.document_id() == document_id)
        else {
            return EditorOutcome::NoChange;
        };
        self.close_buffer(index)
    }

    pub fn confirmation_pending(&self) -> bool {
        self.pending_confirmation.is_some()
    }

    pub fn follow_cursor(&mut self, width: usize, height: usize) {
        let active = self.active_mut();
        active.viewport.follow_cursor(&active.editor, width, height);
    }

    pub fn scroll_active_viewport(&mut self, delta: isize, height: usize) -> bool {
        let active = self.active_mut();
        let before = active.viewport.top_line();
        active
            .viewport
            .scroll_vertical(delta, active.editor.line_count(), height);
        active.viewport.top_line() != before
    }

    pub fn set_active_viewport_from_track(
        &mut self,
        row: usize,
        track_height: usize,
        height: usize,
    ) -> bool {
        let active = self.active_mut();
        let before = active.viewport.top_line();
        let maximum = active.editor.line_count().saturating_sub(height);
        let denominator = track_height.saturating_sub(1).max(1);
        let top = row.min(denominator).saturating_mul(maximum) / denominator;
        active
            .viewport
            .set_top_line(top, active.editor.line_count(), height);
        active.viewport.top_line() != before
    }

    pub fn save_active<S>(&mut self, storage: &mut S) -> io::Result<()>
    where
        S: EditorStorage,
    {
        let path = self.active().path.clone();
        let text = self.active().editor.text();
        let saved_hash = self.active().editor.hash();
        storage.write(&path, text.as_bytes())?;
        self.active_mut().saved_hash = saved_hash;
        Ok(())
    }

    pub fn mark_active_saved(&mut self) {
        let saved_hash = self.active().editor.hash();
        self.active_mut().saved_hash = saved_hash;
    }

    pub fn execute(&mut self, command: EditorCommand) -> Result<EditorOutcome, TransactionError> {
        match command {
            EditorCommand::Insert(character) => {
                let before = self.active_buffer().selection_state();
                if self.active_buffer_mut().insert_char(character)? {
                    Ok(EditorOutcome::Edited)
                } else {
                    Ok(selection_outcome(
                        before,
                        self.active_buffer().selection_state(),
                    ))
                }
            }
            EditorCommand::DeleteBackward => {
                edit_outcome(self.active_buffer_mut().delete_backward()?)
            }
            EditorCommand::DeleteForward => {
                edit_outcome(self.active_buffer_mut().delete_forward()?)
            }
            EditorCommand::DeletePreviousWord => {
                edit_outcome(self.active_buffer_mut().delete_previous_word()?)
            }
            EditorCommand::DeleteToLineStart => {
                edit_outcome(self.active_buffer_mut().delete_to_line_start()?)
            }
            EditorCommand::DeleteToLineEnd => {
                edit_outcome(self.active_buffer_mut().delete_to_line_end()?)
            }
            EditorCommand::Move {
                movement,
                selecting,
            } => Ok(self.move_cursor(movement, selecting)),
            EditorCommand::MoveTo {
                line,
                column,
                selecting,
            } => Ok(self.move_to(line, column, selecting)),
            EditorCommand::SelectWord => Ok(self.select_word()),
            EditorCommand::PageUp { lines, selecting } => {
                Ok(self.move_page(Movement::Up, lines, selecting))
            }
            EditorCommand::PageDown { lines, selecting } => {
                Ok(self.move_page(Movement::Down, lines, selecting))
            }
            EditorCommand::SelectAll => Ok(self.select_all()),
            EditorCommand::Copy => Ok(self.copy()),
            EditorCommand::Cut => self.cut(),
            EditorCommand::Paste => {
                let clipboard = self.clipboard.clone();
                edit_outcome(self.active_buffer_mut().paste(&clipboard)?)
            }
            EditorCommand::PasteExternal(text) => {
                edit_outcome(self.active_buffer_mut().paste(&text)?)
            }
            EditorCommand::Undo => edit_outcome(self.active_buffer_mut().undo()?),
            EditorCommand::Redo => edit_outcome(self.active_buffer_mut().redo()?),
            EditorCommand::Indent => self.indent(),
            EditorCommand::Outdent => self.outdent(),
            EditorCommand::ToggleComment => {
                edit_outcome(self.active_buffer_mut().toggle_line_comment()?)
            }
            EditorCommand::Search(query) => Ok(EditorOutcome::Search(self.search(query))),
            EditorCommand::SearchNext => Ok(EditorOutcome::Search(self.search_next())),
            EditorCommand::ReplaceCurrent { query, replacement } => {
                self.replace_current(query, replacement)
            }
            EditorCommand::ReplaceAll { query, replacement } => {
                self.replace_all(query, replacement)
            }
            EditorCommand::NextBuffer => Ok(self.switch_buffer(1)),
            EditorCommand::PreviousBuffer => Ok(self.switch_buffer(-1)),
            EditorCommand::CloseActive => Ok(self.request_close_active()),
            EditorCommand::RequestQuit => Ok(self.request_quit()),
            EditorCommand::ConfirmDiscard => Ok(self.confirm_discard()),
            EditorCommand::CancelDiscard => Ok(self.cancel_discard()),
        }
    }

    fn active(&self) -> &OpenBuffer<E> {
        &self.buffers[self.active_index]
    }

    fn active_mut(&mut self) -> &mut OpenBuffer<E> {
        &mut self.buffers[self.active_index]
    }

    fn move_cursor(&mut self, movement: Movement, selecting: bool) -> EditorOutcome {
        let before = self.active_buffer().selection_state();
        self.active_buffer_mut().move_cursor(movement, selecting);
        selection_outcome(before, self.active_buffer().selection_state())
    }

    fn move_to(&mut self, line: usize, column: usize, selecting: bool) -> EditorOutcome {
        let before = self.active_buffer().selection_state();
        self.active_buffer_mut()
            .move_to_visual(line, column, selecting);
        selection_outcome(before, self.active_buffer().selection_state())
    }

    fn select_word(&mut self) -> EditorOutcome {
        let before = self.active_buffer().selection_state();
        let cursor = self.active_buffer().cursor();
        let selection = self
            .active_buffer()
            .word_range_at(cursor.line, cursor.display_column);
        self.active_buffer_mut()
            .set_selection(selection)
            .expect("word ranges are valid UTF-8 selection offsets");
        selection_outcome(before, self.active_buffer().selection_state())
    }

    fn move_page(&mut self, movement: Movement, lines: usize, selecting: bool) -> EditorOutcome {
        let before = self.active_buffer().selection_state();
        for _ in 0..lines.max(1) {
            self.active_buffer_mut().move_cursor(movement, selecting);
        }
        selection_outcome(before, self.active_buffer().selection_state())
    }

    fn select_all(&mut self) -> EditorOutcome {
        let before = self.active_buffer().selection_state();
        self.active_buffer_mut()
            .move_cursor(Movement::DocumentStart, false);
        self.active_buffer_mut()
            .move_cursor(Movement::DocumentEnd, true);
        selection_outcome(before, self.active_buffer().selection_state())
    }

    fn selected_text(&self) -> Option<(String, SelectionState)> {
        let selection = self.active_buffer().selection_state();
        if selection.is_caret() {
            return None;
        }
        let text = self.active_buffer().text();
        let (start, end) = grapheme_byte_range(&text, selection);
        let normalized = if selection.anchor_byte <= selection.active_byte {
            SelectionState::new(start as u64, end as u64)
        } else {
            SelectionState::new(end as u64, start as u64)
        };
        Some((text[start..end].to_owned(), normalized))
    }

    fn copy(&mut self) -> EditorOutcome {
        let Some((selected, normalized)) = self.selected_text() else {
            return EditorOutcome::NoChange;
        };
        self.active_buffer_mut()
            .set_selection(normalized)
            .expect("grapheme boundaries are valid UTF-8 selection offsets");
        self.clipboard = selected;
        EditorOutcome::Copied
    }

    fn cut(&mut self) -> Result<EditorOutcome, TransactionError> {
        let Some((selected, normalized)) = self.selected_text() else {
            return Ok(EditorOutcome::NoChange);
        };
        self.active_buffer_mut().set_selection(normalized)?;
        if self.active_buffer_mut().delete_backward()? {
            self.clipboard = selected;
            Ok(EditorOutcome::Edited)
        } else {
            Ok(EditorOutcome::NoChange)
        }
    }

    fn indent(&mut self) -> Result<EditorOutcome, TransactionError> {
        if self.active_buffer().selection_state().is_caret() {
            return self.indent_caret();
        }

        let text = self.active_buffer().text();
        let selection = self.active_buffer().selection_state();
        let edits = selected_line_starts(&text, selection)
            .into_iter()
            .map(|start| TextEdit {
                start_byte: start as u64,
                end_byte: start as u64,
                inserted_text: " ".repeat(INDENT_WIDTH),
            })
            .collect::<Vec<_>>();
        self.apply_line_edits(edits, selection)
    }

    fn indent_caret(&mut self) -> Result<EditorOutcome, TransactionError> {
        let selection = self.active_buffer().selection_state();
        let spaces = INDENT_WIDTH - self.active_buffer().cursor().display_column % INDENT_WIDTH;
        let edit = TextEdit {
            start_byte: selection.active_byte,
            end_byte: selection.active_byte,
            inserted_text: " ".repeat(spaces),
        };
        let after = SelectionState::caret(selection.active_byte + spaces as u64);
        edit_outcome(self.active_buffer_mut().apply_edits(
            EditOrigin::Keyboard,
            vec![edit],
            after,
        )?)
    }

    fn outdent(&mut self) -> Result<EditorOutcome, TransactionError> {
        let text = self.active_buffer().text();
        let selection = self.active_buffer().selection_state();
        let edits = selected_line_starts(&text, selection)
            .into_iter()
            .filter_map(|start| {
                let suffix = &text[start..];
                let removed = if suffix.starts_with('\t') {
                    1
                } else {
                    suffix
                        .as_bytes()
                        .iter()
                        .take(INDENT_WIDTH)
                        .take_while(|byte| **byte == b' ')
                        .count()
                };
                (removed > 0).then(|| TextEdit {
                    start_byte: start as u64,
                    end_byte: (start + removed) as u64,
                    inserted_text: String::new(),
                })
            })
            .collect::<Vec<_>>();
        self.apply_line_edits(edits, selection)
    }

    fn apply_line_edits(
        &mut self,
        edits: Vec<TextEdit>,
        selection: SelectionState,
    ) -> Result<EditorOutcome, TransactionError> {
        if edits.is_empty() {
            return Ok(EditorOutcome::NoChange);
        }
        let selection_after = SelectionState::new(
            transform_offset(selection.anchor_byte, &edits),
            transform_offset(selection.active_byte, &edits),
        );
        edit_outcome(self.active_buffer_mut().apply_edits(
            EditOrigin::Keyboard,
            edits,
            selection_after,
        )?)
    }

    fn search(&mut self, query: String) -> SearchOutcome {
        if query.is_empty() {
            self.active_mut().search = None;
            return SearchOutcome::EmptyQuery;
        }
        let start = self.active_buffer().selection_state().active_byte as usize;
        let match_version = self.active_buffer().version();
        self.active_mut().search = Some(SearchState {
            query,
            last_match: None,
            match_version,
        });
        self.find_from(start)
    }

    fn search_next(&mut self) -> SearchOutcome {
        let Some(search) = &self.active().search else {
            return SearchOutcome::EmptyQuery;
        };
        let buffer = self.active_buffer();
        let start = search
            .last_match
            .filter(|_| search.match_version == buffer.version())
            .map_or(buffer.selection_state().active_byte as usize, |range| {
                range.1
            });
        self.find_from(start)
    }

    fn find_from(&mut self, start: usize) -> SearchOutcome {
        let Some(search) = &self.active().search else {
            return SearchOutcome::EmptyQuery;
        };
        let query = search.query.clone();
        let text = self.active_buffer().text();
        let mut start = start.min(text.len());
        while !text.is_char_boundary(start) {
            start -= 1;
        }
        let (matched, wrapped) = text[start..]
            .find(&query)
            .map(|offset| ((start + offset, start + offset + query.len()), false))
            .or_else(|| {
                text.find(&query)
                    .map(|offset| ((offset, offset + query.len()), true))
            })
            .map_or((None, false), |(range, wrapped)| (Some(range), wrapped));

        let Some((match_start, match_end)) = matched else {
            if let Some(search) = &mut self.active_mut().search {
                search.last_match = None;
            }
            return SearchOutcome::NoMatch;
        };
        let selection = SelectionState::new(match_start as u64, match_end as u64);
        self.active_buffer_mut()
            .set_selection(selection)
            .expect("string matches always use valid UTF-8 byte boundaries");
        let match_version = self.active_buffer().version();
        if let Some(search) = &mut self.active_mut().search {
            search.last_match = Some((match_start, match_end));
            search.match_version = match_version;
        }
        SearchOutcome::Match {
            start_byte: match_start as u64,
            end_byte: match_end as u64,
            wrapped,
        }
    }

    fn replace_current(
        &mut self,
        query: String,
        replacement: String,
    ) -> Result<EditorOutcome, TransactionError> {
        if query.is_empty() {
            return Ok(EditorOutcome::NoChange);
        }
        let selection = self.active_buffer().selection_state();
        let start = selection.anchor_byte.min(selection.active_byte) as usize;
        let end = selection.anchor_byte.max(selection.active_byte) as usize;
        let text = self.active_buffer().text();
        if selection.is_caret() || text.get(start..end) != Some(query.as_str()) {
            return Ok(EditorOutcome::NoChange);
        }
        let edit = TextEdit {
            start_byte: start as u64,
            end_byte: end as u64,
            inserted_text: replacement.clone(),
        };
        let mut after = text;
        after.replace_range(start..end, &replacement);
        let next_start = start + replacement.len();
        let next = next_literal_match(&after, &query, next_start);
        let selection_after = next
            .map(|(match_start, match_end, _)| {
                SelectionState::new(match_start as u64, match_end as u64)
            })
            .unwrap_or_else(|| SelectionState::caret(next_start as u64));
        if !self.active_buffer_mut().apply_edits(
            EditOrigin::Keyboard,
            vec![edit],
            selection_after,
        )? {
            return Ok(EditorOutcome::NoChange);
        }
        let match_version = self.active_buffer().version();
        self.active_mut().search = Some(SearchState {
            query,
            last_match: next.map(|(match_start, match_end, _)| (match_start, match_end)),
            match_version,
        });
        Ok(EditorOutcome::Replaced(1))
    }

    fn replace_all(
        &mut self,
        query: String,
        replacement: String,
    ) -> Result<EditorOutcome, TransactionError> {
        if query.is_empty() || query == replacement {
            return Ok(EditorOutcome::NoChange);
        }
        let matches = self.active_buffer().literal_matches(&query);
        if matches.is_empty() {
            return Ok(EditorOutcome::NoChange);
        }
        let count = matches.len();
        let selection = self.active_buffer().selection_state();
        let selection_after = SelectionState::new(
            transform_replacement_offset(selection.anchor_byte, &matches, replacement.len()),
            transform_replacement_offset(selection.active_byte, &matches, replacement.len()),
        );
        let text = self.active_buffer().text();
        let edits = bounded_replacement_edits(&text, &matches, &replacement);
        if !self
            .active_buffer_mut()
            .apply_edits(EditOrigin::Keyboard, edits, selection_after)?
        {
            return Ok(EditorOutcome::NoChange);
        }
        let match_version = self.active_buffer().version();
        self.active_mut().search = Some(SearchState {
            query,
            last_match: None,
            match_version,
        });
        Ok(EditorOutcome::Replaced(count))
    }

    fn switch_buffer(&mut self, delta: isize) -> EditorOutcome {
        if self.buffers.len() < 2 {
            return EditorOutcome::NoChange;
        }
        self.clear_transient_edit_state();
        if delta < 0 {
            self.active_index = if self.active_index == 0 {
                self.buffers.len() - 1
            } else {
                self.active_index - 1
            };
        } else {
            self.active_index = (self.active_index + 1) % self.buffers.len();
        }
        EditorOutcome::BufferSwitched
    }

    fn request_close_active(&mut self) -> EditorOutcome {
        if self.is_active_dirty() {
            self.pending_confirmation = Some(PendingConfirmation::CloseBuffer(
                self.active_buffer().document_id().clone(),
            ));
            EditorOutcome::ConfirmationRequired(DestructiveAction::CloseBuffer)
        } else {
            self.close_buffer(self.active_index)
        }
    }

    fn request_quit(&mut self) -> EditorOutcome {
        if self.buffers.iter().any(OpenBuffer::is_dirty) {
            self.pending_confirmation = Some(PendingConfirmation::Quit);
            EditorOutcome::ConfirmationRequired(DestructiveAction::Quit)
        } else {
            EditorOutcome::Quit
        }
    }

    fn confirm_discard(&mut self) -> EditorOutcome {
        match self.pending_confirmation.take() {
            Some(PendingConfirmation::CloseBuffer(document_id)) => {
                let Some(index) = self
                    .buffers
                    .iter()
                    .position(|buffer| buffer.editor.document_id() == &document_id)
                else {
                    return EditorOutcome::NoChange;
                };
                self.close_buffer(index)
            }
            Some(PendingConfirmation::Quit) => EditorOutcome::Quit,
            None => EditorOutcome::NoChange,
        }
    }

    fn cancel_discard(&mut self) -> EditorOutcome {
        if self.pending_confirmation.take().is_some() {
            EditorOutcome::Cancelled
        } else {
            EditorOutcome::NoChange
        }
    }

    fn close_buffer(&mut self, index: usize) -> EditorOutcome {
        if self.buffers.len() == 1 {
            return EditorOutcome::LastBuffer;
        }
        self.clear_transient_edit_state();
        self.buffers.remove(index);
        if self.active_index > index || self.active_index == self.buffers.len() {
            self.active_index -= 1;
        }
        EditorOutcome::BufferClosed
    }

    fn clear_transient_edit_state(&mut self) {
        for buffer in &mut self.buffers {
            buffer.editor.clear_transient_edit_state();
        }
    }
}

pub fn session_input_for_event(
    event: Event,
    page_lines: usize,
    confirmation_pending: bool,
    primary_modifier: PrimaryModifier,
) -> Option<SessionInput> {
    session_input_for_event_with_keyboard_enhancement(
        event,
        page_lines,
        confirmation_pending,
        primary_modifier,
        false,
        &GhosttyKeyBindings::passed(),
    )
}

pub(crate) fn session_input_for_event_with_keyboard_enhancement(
    event: Event,
    page_lines: usize,
    confirmation_pending: bool,
    primary_modifier: PrimaryModifier,
    keyboard_enhancement_active: bool,
    ghostty_key_bindings: &GhosttyKeyBindings,
) -> Option<SessionInput> {
    if confirmation_pending {
        return confirmation_input(event);
    }

    match event {
        Event::Paste(text) => Some(SessionInput::Command(EditorCommand::PasteExternal(text))),
        Event::Key(key) if key.kind != KeyEventKind::Release => input_for_key(
            key.code,
            key.modifiers,
            page_lines,
            primary_modifier,
            keyboard_enhancement_active,
            ghostty_key_bindings,
        ),
        _ => None,
    }
}

fn confirmation_input(event: Event) -> Option<SessionInput> {
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind == KeyEventKind::Release {
        return None;
    }
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            Some(SessionInput::Command(EditorCommand::ConfirmDiscard))
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            Some(SessionInput::Command(EditorCommand::CancelDiscard))
        }
        _ => None,
    }
}

fn input_for_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    page_lines: usize,
    primary_modifier: PrimaryModifier,
    keyboard_enhancement_active: bool,
    ghostty_key_bindings: &GhosttyKeyBindings,
) -> Option<SessionInput> {
    if let Some(command) = injected_text_command(
        code,
        modifiers,
        keyboard_enhancement_active,
        ghostty_key_bindings,
    ) {
        return Some(SessionInput::Command(command));
    }

    if let Some(command) = modified_navigation_command(code, modifiers, primary_modifier) {
        return Some(SessionInput::Command(command));
    }

    if matches!(code, KeyCode::Char('f' | 'F'))
        && has_exact_primary_modifier(modifiers, primary_modifier)
    {
        return Some(SessionInput::BeginFind);
    }

    if has_primary_modifier(modifiers, primary_modifier) {
        let command = match code {
            KeyCode::Char(' ') => return Some(SessionInput::Complete),
            KeyCode::Char('q' | 'Q') => EditorCommand::RequestQuit,
            KeyCode::Char('w' | 'W') => EditorCommand::CloseActive,
            KeyCode::Char('c' | 'C') => EditorCommand::Copy,
            KeyCode::Char('x' | 'X') => EditorCommand::Cut,
            KeyCode::Char('v' | 'V') => EditorCommand::Paste,
            KeyCode::Char('z' | 'Z') => EditorCommand::Undo,
            KeyCode::Char('y' | 'Y') => EditorCommand::Redo,
            KeyCode::Char('a' | 'A') => EditorCommand::SelectAll,
            KeyCode::Char('/') => EditorCommand::ToggleComment,
            KeyCode::Tab => EditorCommand::NextBuffer,
            KeyCode::BackTab => EditorCommand::PreviousBuffer,
            KeyCode::Char('s' | 'S') => return Some(SessionInput::Save),
            _ => return None,
        };
        return Some(SessionInput::Command(command));
    }

    if modifiers.intersects(
        KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
    ) {
        return None;
    }

    let selecting = modifiers.contains(KeyModifiers::SHIFT);
    let command = match code {
        KeyCode::Left => EditorCommand::Move {
            movement: Movement::Left,
            selecting,
        },
        KeyCode::Right => EditorCommand::Move {
            movement: Movement::Right,
            selecting,
        },
        KeyCode::Up => EditorCommand::Move {
            movement: Movement::Up,
            selecting,
        },
        KeyCode::Down => EditorCommand::Move {
            movement: Movement::Down,
            selecting,
        },
        KeyCode::Home => EditorCommand::Move {
            movement: Movement::LineStart,
            selecting,
        },
        KeyCode::End => EditorCommand::Move {
            movement: Movement::LineEnd,
            selecting,
        },
        KeyCode::PageUp => EditorCommand::PageUp {
            lines: page_lines,
            selecting,
        },
        KeyCode::PageDown => EditorCommand::PageDown {
            lines: page_lines,
            selecting,
        },
        KeyCode::Backspace => EditorCommand::DeleteBackward,
        KeyCode::Delete => EditorCommand::DeleteForward,
        KeyCode::Enter => EditorCommand::Insert('\n'),
        KeyCode::BackTab => EditorCommand::Outdent,
        KeyCode::Tab if selecting => EditorCommand::Outdent,
        KeyCode::Tab => EditorCommand::Indent,
        KeyCode::F(5) if modifiers == KeyModifiers::NONE => EditorCommand::PreviousBuffer,
        KeyCode::F(6) if modifiers == KeyModifiers::NONE => EditorCommand::NextBuffer,
        KeyCode::F(3) => EditorCommand::SearchNext,
        KeyCode::Char(character) => EditorCommand::Insert(character),
        _ => return None,
    };
    Some(SessionInput::Command(command))
}

fn injected_text_command(
    code: KeyCode,
    modifiers: KeyModifiers,
    keyboard_enhancement_active: bool,
    ghostty_key_bindings: &GhosttyKeyBindings,
) -> Option<EditorCommand> {
    if !keyboard_enhancement_active {
        return None;
    }

    // Crossterm normalizes a raw control byte and the corresponding CSI-u Ctrl
    // key to the same KeyEvent. Only an exact rewrite observed by the startup
    // Ghostty probe can therefore give Ctrl-A/E/U/K their injected editing
    // meanings. Escape-prefixed Option forms remain unambiguous while the
    // keyboard enhancement is active.
    match (code, modifiers) {
        (KeyCode::Char('a'), KeyModifiers::CONTROL)
            if ghostty_key_bindings.observed_exact_translation("super+arrow_left") =>
        {
            Some(EditorCommand::Move {
                movement: Movement::LineStart,
                selecting: false,
            })
        }
        (KeyCode::Char('e'), KeyModifiers::CONTROL)
            if ghostty_key_bindings.observed_exact_translation("super+arrow_right") =>
        {
            Some(EditorCommand::Move {
                movement: Movement::LineEnd,
                selecting: false,
            })
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL)
            if ghostty_key_bindings.observed_exact_translation("super+backspace") =>
        {
            Some(EditorCommand::DeleteToLineStart)
        }
        (KeyCode::Char('k'), KeyModifiers::CONTROL)
            if ghostty_key_bindings.observed_exact_translation("super+k") =>
        {
            Some(EditorCommand::DeleteToLineEnd)
        }
        (KeyCode::Char('b'), KeyModifiers::ALT) => Some(EditorCommand::Move {
            movement: Movement::PreviousWord,
            selecting: false,
        }),
        (KeyCode::Char('f'), KeyModifiers::ALT) => Some(EditorCommand::Move {
            movement: Movement::NextWord,
            selecting: false,
        }),
        (KeyCode::Backspace, KeyModifiers::ALT) => Some(EditorCommand::DeletePreviousWord),
        _ => None,
    }
}

fn modified_navigation_command(
    code: KeyCode,
    modifiers: KeyModifiers,
    primary_modifier: PrimaryModifier,
) -> Option<EditorCommand> {
    let selecting = modifiers.contains(KeyModifiers::SHIFT);
    let base_modifiers = modifiers & !KeyModifiers::SHIFT;
    let movement = match (primary_modifier, code, base_modifiers) {
        (PrimaryModifier::Command, KeyCode::Left, KeyModifiers::SUPER) => Movement::LineStart,
        (PrimaryModifier::Command, KeyCode::Right, KeyModifiers::SUPER) => Movement::LineEnd,
        (PrimaryModifier::Command, KeyCode::Up, KeyModifiers::SUPER) => Movement::DocumentStart,
        (PrimaryModifier::Command, KeyCode::Down, KeyModifiers::SUPER) => Movement::DocumentEnd,
        (PrimaryModifier::Command, KeyCode::Left, KeyModifiers::ALT)
        | (_, KeyCode::Left, KeyModifiers::CONTROL) => Movement::PreviousWord,
        (PrimaryModifier::Command, KeyCode::Right, KeyModifiers::ALT)
        | (_, KeyCode::Right, KeyModifiers::CONTROL) => Movement::NextWord,
        (_, KeyCode::Home, KeyModifiers::CONTROL) => Movement::DocumentStart,
        (_, KeyCode::End, KeyModifiers::CONTROL) => Movement::DocumentEnd,
        _ => {
            if selecting {
                return None;
            }
            return match (primary_modifier, code, modifiers) {
                (PrimaryModifier::Command, KeyCode::Backspace, KeyModifiers::SUPER) => {
                    Some(EditorCommand::DeleteToLineStart)
                }
                (PrimaryModifier::Command, KeyCode::Backspace, KeyModifiers::ALT)
                | (_, KeyCode::Backspace, KeyModifiers::CONTROL) => {
                    Some(EditorCommand::DeletePreviousWord)
                }
                _ => None,
            };
        }
    };
    Some(EditorCommand::Move {
        movement,
        selecting,
    })
}

pub fn has_primary_modifier(modifiers: KeyModifiers, primary_modifier: PrimaryModifier) -> bool {
    modifiers.contains(KeyModifiers::CONTROL)
        || (primary_modifier == PrimaryModifier::Command && modifiers.contains(KeyModifiers::SUPER))
}

pub fn has_exact_primary_modifier(
    modifiers: KeyModifiers,
    primary_modifier: PrimaryModifier,
) -> bool {
    modifiers == KeyModifiers::CONTROL
        || (primary_modifier == PrimaryModifier::Command && modifiers == KeyModifiers::SUPER)
}

fn edit_outcome(changed: bool) -> Result<EditorOutcome, TransactionError> {
    Ok(if changed {
        EditorOutcome::Edited
    } else {
        EditorOutcome::NoChange
    })
}

fn selection_outcome(before: SelectionState, after: SelectionState) -> EditorOutcome {
    if before == after {
        EditorOutcome::NoChange
    } else {
        EditorOutcome::SelectionChanged
    }
}

fn grapheme_byte_range(text: &str, selection: SelectionState) -> (usize, usize) {
    let start = selection.anchor_byte.min(selection.active_byte) as usize;
    let end = selection.anchor_byte.max(selection.active_byte) as usize;
    let grapheme_start = text
        .grapheme_indices(true)
        .map(|(byte, _)| byte)
        .take_while(|byte| *byte <= start)
        .last()
        .unwrap_or(0);
    let grapheme_end = if end == text.len() {
        end
    } else {
        text.grapheme_indices(true)
            .map(|(byte, _)| byte)
            .find(|byte| *byte >= end)
            .unwrap_or(text.len())
    };
    (grapheme_start, grapheme_end)
}

fn selected_line_starts(text: &str, selection: SelectionState) -> Vec<usize> {
    let start = selection.anchor_byte.min(selection.active_byte) as usize;
    let end = selection.anchor_byte.max(selection.active_byte) as usize;
    let first_line = line_start(text, start);
    let last_offset = if start != end && line_start(text, end) == end {
        end.saturating_sub(1)
    } else {
        end
    };
    let last_line = line_start(text, last_offset);

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

fn line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind('\n').map_or(0, |newline| newline + 1)
}

fn transform_offset(offset: u64, edits: &[TextEdit]) -> u64 {
    let original = offset as i128;
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

fn transform_replacement_offset(
    offset: u64,
    matches: &[SelectionState],
    replacement_len: usize,
) -> u64 {
    let original = offset as i128;
    let inserted = replacement_len as i128;
    let mut delta = 0_i128;
    for matched in matches {
        let start = i128::from(matched.anchor_byte);
        let end = i128::from(matched.active_byte);
        if original < start {
            break;
        }
        if original <= end {
            return u64::try_from(start + delta + inserted)
                .expect("literal match offsets remain non-negative");
        }
        delta += inserted - (end - start);
    }
    u64::try_from(original + delta).expect("literal match offsets remain non-negative")
}

fn bounded_replacement_edits(
    text: &str,
    matches: &[SelectionState],
    replacement: &str,
) -> Vec<TextEdit> {
    if matches.len() <= MAX_VECTOR_ITEMS {
        return matches
            .iter()
            .map(|matched| TextEdit {
                start_byte: matched.anchor_byte,
                end_byte: matched.active_byte,
                inserted_text: replacement.to_owned(),
            })
            .collect();
    }

    let first = matches.first().expect("replace-all has at least one match");
    let mut edits = Vec::new();
    let mut start = first.anchor_byte as usize;
    let mut end = first.active_byte as usize;
    let mut inserted_text = replacement.to_owned();
    for matched in &matches[1..] {
        let next_start = matched.anchor_byte as usize;
        let next_end = matched.active_byte as usize;
        let added = next_start
            .saturating_sub(end)
            .saturating_add(replacement.len());
        if inserted_text.len().saturating_add(added) > MAX_INSERTED_TEXT_BYTES {
            edits.push(TextEdit {
                start_byte: start as u64,
                end_byte: end as u64,
                inserted_text,
            });
            start = next_start;
            inserted_text = replacement.to_owned();
        } else {
            inserted_text.push_str(&text[end..next_start]);
            inserted_text.push_str(replacement);
        }
        end = next_end;
    }
    edits.push(TextEdit {
        start_byte: start as u64,
        end_byte: end as u64,
        inserted_text,
    });
    edits
}

fn next_literal_match(text: &str, query: &str, start: usize) -> Option<(usize, usize, bool)> {
    text[start..]
        .find(query)
        .map(|offset| (start + offset, start + offset + query.len(), false))
        .or_else(|| {
            text.find(query)
                .map(|offset| (offset, offset + query.len(), true))
        })
}

#[cfg(test)]
mod completion_input_tests {
    use super::*;
    use crate::ghostty::GhosttyKeyBindings;
    use crossterm::event::{KeyEvent, KeyEventState};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn ctrl_space_is_the_only_completion_trigger() {
        assert_eq!(
            session_input_for_event(
                key(KeyCode::Char(' '), KeyModifiers::CONTROL),
                10,
                false,
                PrimaryModifier::Control,
            ),
            Some(SessionInput::Complete)
        );
        for event in [
            key(KeyCode::Char('a'), KeyModifiers::NONE),
            key(KeyCode::Char(' '), KeyModifiers::NONE),
            key(KeyCode::Char(' '), KeyModifiers::ALT),
            key(KeyCode::Char('n'), KeyModifiers::CONTROL),
        ] {
            assert_ne!(
                session_input_for_event(event, 10, false, PrimaryModifier::Control),
                Some(SessionInput::Complete)
            );
        }
    }

    #[test]
    fn control_and_command_s_are_explicit_save_inputs() {
        for (modifiers, primary_modifier) in [
            (KeyModifiers::CONTROL, PrimaryModifier::Control),
            (KeyModifiers::CONTROL, PrimaryModifier::Command),
            (KeyModifiers::SUPER, PrimaryModifier::Command),
        ] {
            assert_eq!(
                session_input_for_event(
                    key(KeyCode::Char('s'), modifiers),
                    10,
                    false,
                    primary_modifier,
                ),
                Some(SessionInput::Save)
            );
        }
    }

    #[test]
    fn shifted_characters_are_inserted_exactly_as_delivered() {
        for character in ['(', 'A', '{', '"', 'É'] {
            assert_eq!(
                session_input_for_event(
                    key(KeyCode::Char(character), KeyModifiers::SHIFT),
                    10,
                    false,
                    PrimaryModifier::Control,
                ),
                Some(SessionInput::Command(EditorCommand::Insert(character))),
                "shifted character {character:?} was transformed or rejected",
            );
        }
    }

    #[test]
    fn shift_right_keeps_its_selection_meaning() {
        assert_eq!(
            session_input_for_event(
                key(KeyCode::Right, KeyModifiers::SHIFT),
                10,
                false,
                PrimaryModifier::Control,
            ),
            Some(SessionInput::Command(EditorCommand::Move {
                movement: Movement::Right,
                selecting: true,
            })),
        );
    }

    fn mapped_command(
        code: KeyCode,
        modifiers: KeyModifiers,
        primary_modifier: PrimaryModifier,
    ) -> Option<EditorCommand> {
        let Some(SessionInput::Command(command)) =
            session_input_for_event(key(code, modifiers), 10, false, primary_modifier)
        else {
            return None;
        };
        Some(command)
    }

    fn mapped_command_with_keyboard_enhancement(
        code: KeyCode,
        modifiers: KeyModifiers,
        primary_modifier: PrimaryModifier,
        keyboard_enhancement_active: bool,
        ghostty_key_bindings: &GhosttyKeyBindings,
    ) -> Option<EditorCommand> {
        let Some(SessionInput::Command(command)) =
            session_input_for_event_with_keyboard_enhancement(
                key(code, modifiers),
                10,
                false,
                primary_modifier,
                keyboard_enhancement_active,
                ghostty_key_bindings,
            )
        else {
            return None;
        };
        Some(command)
    }

    #[test]
    fn control_byte_translations_require_the_exact_observed_ghostty_rewrite() {
        let exact_rewrites = GhosttyKeyBindings::from_list_output(
            r#"keybind = super+arrow_left=text:\x01
keybind = super+arrow_right=text:\x05
keybind = super+backspace=text:\x15
keybind = super+k=text:\x0b
"#,
        );
        let mappings = [
            (
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
                "super+arrow_left",
                EditorCommand::Move {
                    movement: Movement::LineStart,
                    selecting: false,
                },
            ),
            (
                KeyCode::Char('e'),
                KeyModifiers::CONTROL,
                "super+arrow_right",
                EditorCommand::Move {
                    movement: Movement::LineEnd,
                    selecting: false,
                },
            ),
            (
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
                "super+backspace",
                EditorCommand::DeleteToLineStart,
            ),
            (
                KeyCode::Char('k'),
                KeyModifiers::CONTROL,
                "super+k",
                EditorCommand::DeleteToLineEnd,
            ),
        ];

        for primary_modifier in [PrimaryModifier::Control, PrimaryModifier::Command] {
            for (code, modifiers, key, expected) in &mappings {
                assert_eq!(
                    mapped_command_with_keyboard_enhancement(
                        *code,
                        *modifiers,
                        primary_modifier,
                        true,
                        &exact_rewrites,
                    ),
                    Some(expected.clone()),
                    "exact rewrite for {key} did not translate {modifiers:?}-{code:?} in {primary_modifier:?} mode",
                );
            }
        }
    }

    #[test]
    fn control_bytes_keep_legacy_meanings_without_an_exact_live_rewrite() {
        let exact_rewrites = GhosttyKeyBindings::from_list_output(
            r#"keybind = super+arrow_left=text:\x01
keybind = super+arrow_right=text:\x05
keybind = super+backspace=text:\x15
keybind = super+k=text:\x0b
"#,
        );
        let passed_after_snippet = GhosttyKeyBindings::from_list_output(
            r#"keybind = super+arrow_left=unbind
keybind = super+arrow_right=unbind
keybind = super+backspace=unbind
keybind = super+k=unbind
"#,
        );
        let other_rewrites = GhosttyKeyBindings::from_list_output(
            r#"keybind = super+arrow_left=text:x
keybind = super+arrow_right=text:x
keybind = super+backspace=text:x
keybind = super+k=text:x
"#,
        );
        let states = [
            ("passed after snippet", passed_after_snippet, true),
            ("non-Ghostty terminal", GhosttyKeyBindings::passed(), true),
            ("probe unavailable", GhosttyKeyBindings::defaults(), true),
            ("other rewrites", other_rewrites, true),
            ("enhancement inactive", exact_rewrites, false),
        ];
        for primary_modifier in [PrimaryModifier::Control, PrimaryModifier::Command] {
            for (state, bindings, enhancement_active) in &states {
                assert_eq!(
                    mapped_command_with_keyboard_enhancement(
                        KeyCode::Char('a'),
                        KeyModifiers::CONTROL,
                        primary_modifier,
                        *enhancement_active,
                        bindings,
                    ),
                    Some(EditorCommand::SelectAll),
                    "{state} changed Control-A",
                );
                for code in [KeyCode::Char('e'), KeyCode::Char('u'), KeyCode::Char('k')] {
                    assert_eq!(
                        mapped_command_with_keyboard_enhancement(
                            code,
                            KeyModifiers::CONTROL,
                            primary_modifier,
                            *enhancement_active,
                            bindings,
                        ),
                        None,
                        "{state} changed Control-{code:?}",
                    );
                }
            }
        }
    }

    #[test]
    fn option_injected_text_translations_stay_enhancement_gated_only() {
        let mappings = [
            (
                KeyCode::Char('b'),
                KeyModifiers::ALT,
                EditorCommand::Move {
                    movement: Movement::PreviousWord,
                    selecting: false,
                },
            ),
            (
                KeyCode::Char('f'),
                KeyModifiers::ALT,
                EditorCommand::Move {
                    movement: Movement::NextWord,
                    selecting: false,
                },
            ),
            (
                KeyCode::Backspace,
                KeyModifiers::ALT,
                EditorCommand::DeletePreviousWord,
            ),
        ];
        for primary_modifier in [PrimaryModifier::Control, PrimaryModifier::Command] {
            for bindings in [GhosttyKeyBindings::defaults(), GhosttyKeyBindings::passed()] {
                for (code, modifiers, expected) in &mappings {
                    assert_eq!(
                        mapped_command_with_keyboard_enhancement(
                            *code,
                            *modifiers,
                            primary_modifier,
                            true,
                            &bindings,
                        ),
                        Some(expected.clone()),
                    );
                }
            }
        }

        for primary_modifier in [PrimaryModifier::Control, PrimaryModifier::Command] {
            for (code, modifiers, expected) in &mappings {
                let inactive_expected = (*code == KeyCode::Backspace
                    && primary_modifier == PrimaryModifier::Command)
                    .then(|| expected.clone());
                assert_eq!(
                    mapped_command_with_keyboard_enhancement(
                        *code,
                        *modifiers,
                        primary_modifier,
                        false,
                        &GhosttyKeyBindings::defaults(),
                    ),
                    inactive_expected,
                    "inactive {primary_modifier:?} {modifiers:?}-{code:?}",
                );
            }
        }
    }

    #[test]
    fn command_mode_maps_super_option_and_control_alias_navigation_with_shift() {
        let movements = [
            (KeyCode::Left, KeyModifiers::SUPER, Movement::LineStart),
            (KeyCode::Right, KeyModifiers::SUPER, Movement::LineEnd),
            (KeyCode::Up, KeyModifiers::SUPER, Movement::DocumentStart),
            (KeyCode::Down, KeyModifiers::SUPER, Movement::DocumentEnd),
            (KeyCode::Left, KeyModifiers::ALT, Movement::PreviousWord),
            (KeyCode::Right, KeyModifiers::ALT, Movement::NextWord),
            (KeyCode::Left, KeyModifiers::CONTROL, Movement::PreviousWord),
            (KeyCode::Right, KeyModifiers::CONTROL, Movement::NextWord),
            (
                KeyCode::Home,
                KeyModifiers::CONTROL,
                Movement::DocumentStart,
            ),
            (KeyCode::End, KeyModifiers::CONTROL, Movement::DocumentEnd),
        ];

        for (code, modifiers, movement) in movements {
            for selecting in [false, true] {
                let modifiers = if selecting {
                    modifiers | KeyModifiers::SHIFT
                } else {
                    modifiers
                };
                assert_eq!(
                    mapped_command(code, modifiers, PrimaryModifier::Command),
                    Some(EditorCommand::Move {
                        movement,
                        selecting,
                    }),
                    "unexpected Command-mode mapping for {modifiers:?}-{code:?}",
                );
            }
        }

        assert_eq!(
            mapped_command(
                KeyCode::Backspace,
                KeyModifiers::ALT,
                PrimaryModifier::Command,
            ),
            Some(EditorCommand::DeletePreviousWord),
        );
        assert_eq!(
            mapped_command(
                KeyCode::Backspace,
                KeyModifiers::SUPER,
                PrimaryModifier::Command,
            ),
            Some(EditorCommand::DeleteToLineStart),
        );
        assert_eq!(
            mapped_command(
                KeyCode::Backspace,
                KeyModifiers::CONTROL,
                PrimaryModifier::Command,
            ),
            Some(EditorCommand::DeletePreviousWord),
        );
        for code in [KeyCode::Up, KeyCode::Down] {
            assert_eq!(
                mapped_command(code, KeyModifiers::ALT, PrimaryModifier::Command),
                None,
                "Option-{code:?} must remain reserved for diagnostics",
            );
        }
    }

    #[test]
    fn control_mode_maps_word_document_and_delete_chords_with_shift() {
        for (code, movement) in [
            (KeyCode::Left, Movement::PreviousWord),
            (KeyCode::Right, Movement::NextWord),
            (KeyCode::Home, Movement::DocumentStart),
            (KeyCode::End, Movement::DocumentEnd),
        ] {
            for selecting in [false, true] {
                let modifiers = if selecting {
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                } else {
                    KeyModifiers::CONTROL
                };
                assert_eq!(
                    mapped_command(code, modifiers, PrimaryModifier::Control),
                    Some(EditorCommand::Move {
                        movement,
                        selecting,
                    }),
                    "unexpected Control-mode mapping for {modifiers:?}-{code:?}",
                );
            }
        }

        assert_eq!(
            mapped_command(
                KeyCode::Backspace,
                KeyModifiers::CONTROL,
                PrimaryModifier::Control,
            ),
            Some(EditorCommand::DeletePreviousWord),
        );
        for (code, modifiers) in [
            (KeyCode::Left, KeyModifiers::SUPER),
            (KeyCode::Right, KeyModifiers::ALT),
            (KeyCode::Up, KeyModifiers::ALT),
            (KeyCode::Down, KeyModifiers::ALT),
        ] {
            assert_eq!(
                mapped_command(code, modifiers, PrimaryModifier::Control),
                None,
                "Control mode accepted reserved or unsupported {modifiers:?}-{code:?}",
            );
        }
    }

    #[test]
    fn unmodified_navigation_and_editing_keys_keep_their_existing_commands() {
        let mappings = [
            (
                KeyCode::Left,
                EditorCommand::Move {
                    movement: Movement::Left,
                    selecting: false,
                },
            ),
            (
                KeyCode::Right,
                EditorCommand::Move {
                    movement: Movement::Right,
                    selecting: false,
                },
            ),
            (
                KeyCode::Up,
                EditorCommand::Move {
                    movement: Movement::Up,
                    selecting: false,
                },
            ),
            (
                KeyCode::Down,
                EditorCommand::Move {
                    movement: Movement::Down,
                    selecting: false,
                },
            ),
            (
                KeyCode::Home,
                EditorCommand::Move {
                    movement: Movement::LineStart,
                    selecting: false,
                },
            ),
            (
                KeyCode::End,
                EditorCommand::Move {
                    movement: Movement::LineEnd,
                    selecting: false,
                },
            ),
            (
                KeyCode::PageUp,
                EditorCommand::PageUp {
                    lines: 10,
                    selecting: false,
                },
            ),
            (
                KeyCode::PageDown,
                EditorCommand::PageDown {
                    lines: 10,
                    selecting: false,
                },
            ),
            (KeyCode::Backspace, EditorCommand::DeleteBackward),
            (KeyCode::Delete, EditorCommand::DeleteForward),
            (KeyCode::Tab, EditorCommand::Indent),
        ];

        for primary_modifier in [PrimaryModifier::Control, PrimaryModifier::Command] {
            for (code, expected) in &mappings {
                assert_eq!(
                    mapped_command(*code, KeyModifiers::NONE, primary_modifier),
                    Some(expected.clone()),
                    "unmodified {code:?} changed in {primary_modifier:?} mode",
                );
            }
        }
    }
}
