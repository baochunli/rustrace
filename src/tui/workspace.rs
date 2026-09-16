use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use crossterm::event::{Event as TerminalEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use rustrace_model::assignment::AssignmentManifest;
use rustrace_model::{
    DocumentId, Event, FileCreated, FileDeleted, FileFocused, FileRenamed, IdentifierError,
    SelectionChanged, ValidationError, WorkspacePath, WorkspacePathError, document_hash,
};
use rustrace_workspace::hash::{
    MAX_WORKSPACE_FILE_BYTES, MAX_WORKSPACE_FILES, MAX_WORKSPACE_TOTAL_BYTES, PinnedWorkspaceRoot,
    WorkspaceHashError, hash_entries, read_pinned_workspace,
};
use rustrace_workspace::{
    AllowedPathSet, AllowedPathSetError, WorkspaceMutationError, create_workspace_file_in,
    preflight_workspace_destination_in, remove_workspace_file_in, rename_workspace_file_in,
    write_workspace_file_in,
};

use crate::config::PrimaryModifier;
use crate::editor::{EditOrigin, EditorBuffer, EditorEffects, TransactionError, Viewport};

use super::{
    EditorCommand, EditorOutcome, EditorSession, SessionInput, has_exact_primary_modifier,
    session_input_for_event_with_keyboard_enhancement, shell::editor_source_layout,
};

pub trait WorkspaceEffects: EditorEffects + Clone {
    /// Temporary command authority is distinct from permanent recorder poison.
    fn command_active(&self) -> bool {
        false
    }

    /// Production input policy runs before health, selection, limits or editor
    /// dispatch, including commands that would otherwise be empty/no-op.
    /// Generic historical editor fixtures retain their original semantics.
    fn check_editor_command(
        &mut self,
        _command: &EditorCommand,
    ) -> Result<(), WorkspaceEffectError> {
        Ok(())
    }
    /// Read-only access to permanent recording failure, including authority checks.
    fn recovery_reason(&self) -> Option<String> {
        None
    }
    /// Records non-lifecycle workspace provenance such as selection changes.
    fn record_workspace_event(&mut self, event: Event) -> Result<(), WorkspaceEffectError>;

    /// Atomically accepts one lifecycle event.
    ///
    /// The accepted event establishes logical intent before filesystem
    /// publication. It is not evidence that the saved bytes already changed.
    /// A subsequent publication failure poisons this controller; restart
    /// reconciliation must compare the durable prefix with saved disk state
    /// before enabling further writes (owned by T3.6).
    /// Returning an error means the event was not accepted into provenance.
    fn record_lifecycle(&mut self, event: Event) -> Result<(), WorkspaceEffectError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceEffectError {
    message: String,
}

impl WorkspaceEffectError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for WorkspaceEffectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for WorkspaceEffectError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceFile {
    path: WorkspacePath,
    document_id: Option<DocumentId>,
    user_editable: bool,
    dirty: bool,
    persisted_contents: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FormatterDocumentState {
    pub path: WorkspacePath,
    pub document_id: DocumentId,
    pub version: u64,
    pub content_hash: rustrace_model::Hash,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FormatterChange {
    pub path: WorkspacePath,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
    pub transaction: rustrace_model::EditorTransaction,
}

pub(crate) type FormatterPrestate = (
    BTreeMap<WorkspacePath, Vec<u8>>,
    Vec<FormatterDocumentState>,
);

impl WorkspaceFile {
    pub const fn path(&self) -> &WorkspacePath {
        &self.path
    }

    pub const fn document_id(&self) -> Option<&DocumentId> {
        self.document_id.as_ref()
    }

    pub const fn is_editable(&self) -> bool {
        self.user_editable
    }

    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceOutcome {
    NoChange,
    TreeSelectionChanged,
    FileActivated,
    FileCreated,
    FileRenamed,
    FileDeleted,
    ConfirmationRequired,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceFocus {
    Editor,
    Console,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceInput {
    Editor(SessionInput),
    BeginCreate,
    BeginRename,
    DeleteSelected,
    ConfirmDestructive,
    CancelDestructive,
}

pub const CTRL_W_DELETE_SELECTED_HELP_ENTRY: &str =
    "Ctrl-W                 delete selected file (confirm if dirty)";

pub fn diagnostic_delta_for_event(event: &TerminalEvent) -> Option<isize> {
    let TerminalEvent::Key(key) = event else {
        return None;
    };
    if key.kind == KeyEventKind::Release || key.modifiers != KeyModifiers::ALT {
        return None;
    }
    match key.code {
        KeyCode::Up => Some(-1),
        KeyCode::Down => Some(1),
        _ => None,
    }
}

pub fn workspace_input_for_event(
    event: TerminalEvent,
    page_lines: usize,
    focus: WorkspaceFocus,
    confirmation_pending: bool,
    primary_modifier: PrimaryModifier,
) -> Option<WorkspaceInput> {
    workspace_input_for_event_with_keyboard_enhancement(
        event,
        page_lines,
        focus,
        confirmation_pending,
        primary_modifier,
        false,
        &crate::ghostty::GhosttyKeyBindings::passed(),
    )
}

pub(crate) fn workspace_input_for_event_with_keyboard_enhancement(
    event: TerminalEvent,
    page_lines: usize,
    focus: WorkspaceFocus,
    confirmation_pending: bool,
    primary_modifier: PrimaryModifier,
    keyboard_enhancement_active: bool,
    ghostty_key_bindings: &crate::ghostty::GhosttyKeyBindings,
) -> Option<WorkspaceInput> {
    let TerminalEvent::Key(key) = &event else {
        return if focus == WorkspaceFocus::Editor && !confirmation_pending {
            session_input_for_event_with_keyboard_enhancement(
                event,
                page_lines,
                false,
                primary_modifier,
                keyboard_enhancement_active,
                ghostty_key_bindings,
            )
            .map(WorkspaceInput::Editor)
        } else {
            None
        };
    };
    if key.kind == KeyEventKind::Release {
        return None;
    }
    if has_exact_primary_modifier(key.modifiers, primary_modifier)
        && matches!(key.code, KeyCode::Char('q' | 'Q'))
    {
        return Some(WorkspaceInput::Editor(SessionInput::Command(
            EditorCommand::RequestQuit,
        )));
    }
    if confirmation_pending {
        return match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => Some(WorkspaceInput::ConfirmDestructive),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(WorkspaceInput::CancelDestructive),
            _ => None,
        };
    }
    if has_exact_primary_modifier(key.modifiers, primary_modifier)
        && matches!(key.code, KeyCode::Char('w' | 'W'))
    {
        return Some(WorkspaceInput::DeleteSelected);
    }

    if focus == WorkspaceFocus::Console {
        return None;
    }

    session_input_for_event_with_keyboard_enhancement(
        event,
        page_lines,
        false,
        primary_modifier,
        keyboard_enhancement_active,
        ghostty_key_bindings,
    )
    .map(WorkspaceInput::Editor)
}

#[derive(Clone, Debug)]
struct PendingDelete {
    path: WorkspacePath,
    document_id: DocumentId,
}

pub struct WorkspaceSession<E>
where
    E: WorkspaceEffects,
{
    root: PinnedWorkspaceRoot,
    allowed_paths: AllowedPathSet,
    files: Vec<WorkspaceFile>,
    selected_index: usize,
    next_document_id: u64,
    editor: EditorSession<E>,
    effects: E,
    pending_delete: Option<PendingDelete>,
    poison: Option<String>,
}

impl<E> fmt::Debug for WorkspaceSession<E>
where
    E: WorkspaceEffects,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceSession")
            .field("root", &self.root)
            .field("files", &self.files)
            .field("selected_index", &self.selected_index)
            .field("next_document_id", &self.next_document_id)
            .field("active_document_id", &self.editor.active_document_id())
            .field("pending_delete", &self.pending_delete)
            .field("poison", &self.poison)
            .finish_non_exhaustive()
    }
}

impl<E> WorkspaceSession<E>
where
    E: WorkspaceEffects,
{
    pub fn open(
        root: impl AsRef<Path>,
        manifest: &AssignmentManifest,
        active_path: Option<WorkspacePath>,
        effects: E,
    ) -> Result<Self, WorkspaceError> {
        let root = PinnedWorkspaceRoot::open(root.as_ref()).map_err(WorkspaceError::Workspace)?;
        let contents = read_pinned_workspace(&root).map_err(WorkspaceError::Workspace)?;
        Self::open_contents(root, manifest, active_path, effects, contents)
    }

    fn open_contents(
        root: PinnedWorkspaceRoot,
        manifest: &AssignmentManifest,
        active_path: Option<WorkspacePath>,
        effects: E,
        contents: BTreeMap<WorkspacePath, Vec<u8>>,
    ) -> Result<Self, WorkspaceError> {
        let allowed_paths =
            AllowedPathSet::from_manifest(manifest).map_err(WorkspaceError::Policy)?;
        let dependency_documents = ["Cargo.toml", "Cargo.lock"].into_iter().all(|name| {
            WorkspacePath::new(name)
                .ok()
                .is_some_and(|path| contents.contains_key(&path))
        });
        let mut files = Vec::with_capacity(contents.len());
        let mut editable = Vec::new();

        for (path, bytes) in &contents {
            if path.as_str().chars().any(char::is_control) {
                return Err(WorkspaceError::UnsafeDisplayPath { path: path.clone() });
            }
            if is_hash_excluded_root(path) {
                return Err(WorkspaceError::HashExcludedPath { path: path.clone() });
            }
            allowed_paths
                .validate(path)
                .map_err(WorkspaceError::Policy)?;
            let user_editable = editable_file_type(path);
            let document_id = if controller_document_type(path, dependency_documents) {
                let text = std::str::from_utf8(bytes)
                    .map_err(|_| WorkspaceError::InvalidUtf8 { path: path.clone() })?;
                let document_id = document_id(editable.len() as u64)?;
                editable.push((document_id.clone(), path.clone(), text.to_owned()));
                Some(document_id)
            } else {
                None
            };
            files.push(WorkspaceFile {
                path: path.clone(),
                document_id,
                user_editable,
                dirty: false,
                persisted_contents: bytes.clone(),
            });
        }

        let Some((first_document_id, first_path, first_text)) = editable
            .iter()
            .find(|(_, path, _)| editable_file_type(path))
            .cloned()
        else {
            return Err(WorkspaceError::NoEditableFiles);
        };
        let mut editor = EditorSession::new(
            first_document_id.clone(),
            root.path().join(first_path.as_str()),
            &first_text,
            effects.clone(),
        );
        for (document_id, path, text) in editable
            .iter()
            .filter(|(document_id, _, _)| document_id != &first_document_id)
        {
            editor.open_buffer_inactive(
                document_id.clone(),
                root.path().join(path.as_str()),
                text,
                effects.clone(),
            );
        }

        let requested_active = active_path.unwrap_or(first_path);
        let selected_index = files
            .iter()
            .position(|file| file.path == requested_active)
            .ok_or_else(|| WorkspaceError::MissingPath {
                path: requested_active.clone(),
            })?;
        let active_document = files[selected_index]
            .user_editable
            .then_some(files[selected_index].document_id.as_ref())
            .flatten()
            .ok_or_else(|| WorkspaceError::NotEditable {
                path: requested_active.clone(),
            })?;
        let _ = editor.activate_document(active_document);

        Ok(Self {
            root,
            allowed_paths,
            files,
            selected_index,
            next_document_id: editable.len() as u64,
            editor,
            effects,
            pending_delete: None,
            poison: None,
        })
    }

    /// Restore authoritative buffers without publishing source files or emitting edits.
    pub fn from_recovered(
        root: impl AsRef<Path>,
        manifest: &AssignmentManifest,
        snapshot: &rustrace_journal::CheckpointSnapshot,
        baseline: &BTreeMap<WorkspacePath, Vec<u8>>,
        effects: E,
    ) -> Result<Self, WorkspaceError> {
        let root = PinnedWorkspaceRoot::open(root.as_ref()).map_err(WorkspaceError::Workspace)?;
        let contents = snapshot
            .files()
            .iter()
            .map(|f| (f.path.clone(), f.contents.clone()))
            .collect();
        let mut workspace = Self::open_contents(root, manifest, None, effects.clone(), contents)?;
        let first = snapshot
            .documents()
            .iter()
            .find(|document| {
                workspace
                    .files
                    .iter()
                    .any(|file| file.path == document.path && file.user_editable)
            })
            .ok_or(WorkspaceError::NoEditableFiles)?;
        let text_for = |path: &WorkspacePath| -> &str {
            std::str::from_utf8(
                &snapshot
                    .files()
                    .iter()
                    .find(|f| &f.path == path)
                    .expect("validated snapshot file")
                    .contents,
            )
            .expect("validated snapshot text")
        };
        let mut editor = EditorSession::new(
            first.document_id.clone(),
            workspace.root.path().join(first.path.as_str()),
            text_for(&first.path),
            effects.clone(),
        );
        for doc in snapshot
            .documents()
            .iter()
            .filter(|document| document.document_id != first.document_id)
        {
            editor.open_buffer_inactive(
                doc.document_id.clone(),
                workspace.root.path().join(doc.path.as_str()),
                text_for(&doc.path),
                effects.clone(),
            );
        }
        for doc in snapshot.documents() {
            let saved = baseline
                .get(&doc.path)
                .and_then(|b| std::str::from_utf8(b).ok())
                .map(document_hash)
                .unwrap_or(rustrace_model::Hash::zero());
            editor
                .recover_buffer(
                    &doc.document_id,
                    text_for(&doc.path),
                    doc.version,
                    doc.selection,
                    saved,
                    effects.clone(),
                )
                .map_err(WorkspaceError::Editor)?;
        }
        let active = snapshot
            .active_document()
            .filter(|document_id| {
                snapshot.documents().iter().any(|document| {
                    &document.document_id == *document_id
                        && workspace
                            .files
                            .iter()
                            .any(|file| file.path == document.path && file.user_editable)
                })
            })
            .unwrap_or(&first.document_id);
        editor.activate_document(active);
        for file in &mut workspace.files {
            file.document_id = snapshot
                .documents()
                .iter()
                .find(|d| d.path == file.path)
                .map(|d| d.document_id.clone());
            file.persisted_contents = baseline.get(&file.path).cloned().unwrap_or_default();
        }
        workspace.next_document_id = snapshot
            .documents()
            .iter()
            .filter_map(|d| {
                d.document_id
                    .as_str()
                    .strip_prefix("workspace-file-")
                    .and_then(|s| u64::from_str_radix(s, 16).ok())
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        // Every allocation requires a lifecycle event. This prefix-derived floor
        // exceeds all IDs previously allocated, including subsequently deleted files.
        workspace.next_document_id = workspace.next_document_id.max(
            snapshot
                .event_sequence()
                .saturating_add(MAX_WORKSPACE_FILES as u64),
        );
        workspace.editor = editor;
        workspace.selected_index = workspace
            .files
            .iter()
            .position(|f| f.document_id.as_ref() == Some(workspace.editor.active_document_id()))
            .expect("active document is in recovered files");
        workspace.refresh_dirty();
        Ok(workspace)
    }

    pub fn checkpoint_input(
        &self,
        session_id: rustrace_model::SessionId,
    ) -> Result<rustrace_journal::CheckpointInput, WorkspaceError> {
        Ok(rustrace_journal::CheckpointInput {
            session_id,
            files: self
                .logical_files()?
                .into_iter()
                .map(|(path, contents)| rustrace_journal::CheckpointFile { path, contents })
                .collect(),
            active_document: Some(self.active_document_id().clone()),
            documents: self
                .files
                .iter()
                .filter_map(|file| {
                    file.document_id.as_ref().map(|id| {
                        let buffer = self
                            .editor
                            .buffer_by_document_id(id)
                            .expect("workspace buffer");
                        rustrace_journal::OpenDocument {
                            document_id: id.clone(),
                            path: file.path.clone(),
                            version: buffer.version(),
                            selection: buffer.selection_state(),
                        }
                    })
                })
                .collect(),
        })
    }

    /// Publish all managed buffers, then require complete disk/baseline/logical agreement.
    pub fn save_all(&mut self) -> Result<(), WorkspaceError> {
        self.ensure_mutable()?;
        self.ensure_active_user_editable()?;
        self.save_all_with_hook(|_| {})
    }

    pub(crate) fn save_all_with_hook(
        &mut self,
        mut published: impl FnMut(&WorkspacePath),
    ) -> Result<(), WorkspaceError> {
        self.ensure_healthy()?;
        self.ensure_active_user_editable()?;
        let mut expected = self.verified_disk_files()?;
        let logical = self.logical_files()?;
        for file in &mut self.files {
            let contents = &logical[&file.path];
            if contents != &file.persisted_contents {
                // Only already-verified own publications advance this view.
                // A later ordinary replacement is not owned by our save intent.
                match read_pinned_workspace(&self.root) {
                    Ok(current) if current == expected => {}
                    Ok(_) => return Err(self.poison(
                        "external change during save-all; newer disk contents preserved; recovery required",
                    )),
                    Err(error) => return Err(self.poison(format!(
                        "save-all recheck failed; disk preserved: {error}",
                    ))),
                }
                if let Err(error) = write_workspace_file_in(&self.root, &file.path, contents) {
                    return Err(self.poison(format!("save-all publication failed: {error}")));
                }
                expected.insert(file.path.clone(), contents.clone());
                crate::session::process_probe("own-save-file");
                published(&file.path);
                file.persisted_contents = contents.clone();
                if let Some(id) = &file.document_id {
                    self.editor.mark_document_saved(id);
                }
            }
        }
        self.refresh_dirty();
        if self.verified_disk_files()? != logical {
            return Err(self.poison("save-all equality failed"));
        }
        Ok(())
    }

    pub fn trim_undo_to(&mut self, maximum: usize) -> bool {
        if self.effects.command_active() {
            return false;
        }
        self.editor.trim_history_to(maximum)
    }
    pub fn retained_undo_bytes(&self) -> usize {
        self.editor.retained_history_bytes()
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn root_authority(&self) -> &PinnedWorkspaceRoot {
        &self.root
    }

    pub fn file_tree(&self) -> &[WorkspaceFile] {
        &self.files
    }

    pub(crate) fn language_documents(
        &self,
    ) -> Result<Vec<crate::language_service::DocumentState>, WorkspaceError> {
        self.ensure_healthy()?;
        self.files
            .iter()
            .filter_map(|file| {
                let document_id = file.document_id.as_ref()?;
                let buffer = self
                    .editor
                    .buffer_by_document_id(document_id)
                    .expect("every editable workspace file has a buffer");
                Some(Ok(crate::language_service::DocumentState {
                    document_id: document_id.clone(),
                    path: file.path.clone(),
                    version: buffer.version(),
                    text: buffer.text(),
                }))
            })
            .collect()
    }

    pub fn selected_path(&self) -> &WorkspacePath {
        &self.files[self.selected_index].path
    }

    pub fn active_path(&self) -> &WorkspacePath {
        let document_id = self.editor.active_document_id();
        &self
            .files
            .iter()
            .find(|file| file.document_id.as_ref() == Some(document_id))
            .expect("every editor buffer belongs to one workspace file")
            .path
    }

    pub fn active_document_id(&self) -> &DocumentId {
        self.editor.active_document_id()
    }

    pub fn active_buffer(&self) -> &EditorBuffer<crate::editor::SyntaxEffects<E>> {
        self.editor.active_buffer()
    }

    pub fn selected_text_for_find(&self) -> Option<String> {
        self.editor.selected_text_for_find()
    }

    pub fn search_summary(&self, query: &str) -> super::SearchSummary {
        self.editor.search_summary(query)
    }

    pub fn remember_search(&mut self, query: String) {
        self.editor.remember_search(query);
    }

    pub(crate) fn buffer_by_document_id(
        &self,
        document_id: &DocumentId,
    ) -> Option<&EditorBuffer<crate::editor::SyntaxEffects<E>>> {
        self.editor.buffer_by_document_id(document_id)
    }

    pub(crate) fn preview_completion(
        &mut self,
        edit: rustrace_model::TextEdit,
    ) -> Result<Option<rustrace_model::EditorTransaction>, WorkspaceError> {
        self.ensure_mutable()?;
        self.ensure_active_user_editable()?;
        self.set_active_text_limit()?;
        let cursor = edit
            .start_byte
            .saturating_add(edit.inserted_text.len() as u64);
        self.editor
            .active_buffer()
            .preview_edits(
                EditOrigin::Completion,
                vec![edit],
                rustrace_model::SelectionState::caret(cursor),
            )
            .map_err(WorkspaceError::Editor)
    }

    pub(crate) fn apply_completion(
        &mut self,
        transaction: rustrace_model::EditorTransaction,
    ) -> Result<bool, WorkspaceError> {
        self.ensure_mutable()?;
        self.ensure_active_user_editable()?;
        if transaction.origin != EditOrigin::Completion
            || transaction.document_id != *self.editor.active_document_id()
        {
            return Err(WorkspaceError::ControllerManagedCommand);
        }
        self.set_active_text_limit()?;
        let document_id = transaction.document_id.clone();
        let changed = match self
            .editor
            .apply_document_transaction(&document_id, transaction)
        {
            Ok(changed) => changed,
            Err(error @ TransactionError::Provenance(_)) => {
                return Err(self.poison(format!("editor provenance failed: {error}")));
            }
            Err(error) => return Err(WorkspaceError::Editor(error)),
        };
        self.refresh_dirty();
        Ok(changed)
    }

    pub fn active_highlights(&self) -> std::cell::Ref<'_, [crate::editor::HighlightSpan]> {
        self.editor.active_highlights()
    }

    pub fn active_viewport(&self) -> &Viewport {
        self.editor.active_viewport()
    }

    pub fn follow_cursor(&mut self, width: usize, height: usize) {
        if self.effects.command_active() {
            return;
        }
        self.editor.follow_cursor(width, height);
    }

    pub fn follow_cursor_in_editor_area(&mut self, editor: Rect) {
        let source = editor_source_layout(editor, self.active_buffer().line_count()).area;
        self.follow_cursor(usize::from(source.width), usize::from(source.height));
    }

    pub fn scroll_active_viewport(&mut self, delta: isize, height: usize) -> bool {
        self.editor.scroll_active_viewport(delta, height)
    }

    pub fn set_active_viewport_from_track(
        &mut self,
        row: usize,
        track_height: usize,
        height: usize,
    ) -> bool {
        self.editor
            .set_active_viewport_from_track(row, track_height, height)
    }

    pub fn active_is_dirty(&self) -> bool {
        self.editor.is_active_dirty()
    }

    pub fn confirmation_pending(&self) -> bool {
        self.pending_delete.is_some() || self.editor.confirmation_pending()
    }

    pub fn delete_confirmation_pending(&self) -> bool {
        self.pending_delete.is_some()
    }

    pub fn select_next(&mut self) -> WorkspaceOutcome {
        if self.effects.command_active() {
            return WorkspaceOutcome::NoChange;
        }
        loop {
            self.selected_index = (self.selected_index + 1) % self.files.len();
            if self.files[self.selected_index].path.as_str() != "Cargo.lock" {
                break;
            }
        }
        WorkspaceOutcome::TreeSelectionChanged
    }

    pub fn select_previous(&mut self) -> WorkspaceOutcome {
        if self.effects.command_active() {
            return WorkspaceOutcome::NoChange;
        }
        loop {
            self.selected_index = if self.selected_index == 0 {
                self.files.len() - 1
            } else {
                self.selected_index - 1
            };
            if self.files[self.selected_index].path.as_str() != "Cargo.lock" {
                break;
            }
        }
        WorkspaceOutcome::TreeSelectionChanged
    }

    pub fn select_index(&mut self, index: usize) -> Result<WorkspaceOutcome, WorkspaceError> {
        self.ensure_mutable()?;
        if index >= self.files.len() {
            return Err(WorkspaceError::InvalidIndex {
                target: "file",
                index,
            });
        }
        if self.selected_index == index {
            return Ok(WorkspaceOutcome::NoChange);
        }
        self.selected_index = index;
        Ok(WorkspaceOutcome::TreeSelectionChanged)
    }

    pub fn activate_path(
        &mut self,
        path: &WorkspacePath,
    ) -> Result<WorkspaceOutcome, WorkspaceError> {
        self.select_path(path)?;
        self.activate_selected()
    }

    pub fn select_path(&mut self, path: &WorkspacePath) -> Result<(), WorkspaceError> {
        self.ensure_mutable()?;
        self.selected_index = self
            .files
            .iter()
            .position(|file| &file.path == path)
            .ok_or_else(|| WorkspaceError::MissingPath { path: path.clone() })?;
        Ok(())
    }

    pub fn activate_selected(&mut self) -> Result<WorkspaceOutcome, WorkspaceError> {
        self.ensure_mutable()?;
        let selected = &self.files[self.selected_index];
        if !selected.user_editable {
            return Err(WorkspaceError::NotEditable {
                path: selected.path.clone(),
            });
        }
        let document_id =
            selected
                .document_id
                .clone()
                .ok_or_else(|| WorkspaceError::NotEditable {
                    path: selected.path.clone(),
                })?;
        if self.editor.active_document_id() == &document_id {
            return Ok(WorkspaceOutcome::NoChange);
        }
        self.record_focus(&document_id)?;
        let outcome = self.editor.activate_document(&document_id);
        debug_assert_eq!(outcome, EditorOutcome::BufferSwitched);
        Ok(if outcome == EditorOutcome::NoChange {
            WorkspaceOutcome::NoChange
        } else {
            WorkspaceOutcome::FileActivated
        })
    }

    /// Navigate to a compiler span only after validating it against the current
    /// managed UTF-8 buffer. The caller separately establishes command-version
    /// freshness and workspace containment.
    pub fn navigate_to_diagnostic_span(
        &mut self,
        path: &WorkspacePath,
        span: &crate::diagnostics::DiagnosticSpan,
    ) -> Result<(), WorkspaceError> {
        self.ensure_mutable()?;
        let index = self
            .files
            .iter()
            .position(|file| &file.path == path)
            .ok_or_else(|| WorkspaceError::MissingPath { path: path.clone() })?;
        let document_id = self.files[index]
            .user_editable
            .then_some(self.files[index].document_id.clone())
            .flatten()
            .ok_or_else(|| WorkspaceError::NotEditable { path: path.clone() })?;
        if !self.diagnostic_span_is_valid(path, span) {
            return Err(WorkspaceError::InvalidDiagnosticSpan);
        }
        let selection = rustrace_model::SelectionState::new(span.byte_start, span.byte_end);
        if self.editor.active_document_id() != &document_id {
            self.record_focus(&document_id)?;
            let outcome = self.editor.activate_document(&document_id);
            debug_assert_eq!(outcome, EditorOutcome::BufferSwitched);
        }
        if self.editor.active_buffer().selection_state() != selection {
            self.record_selection(document_id.clone(), selection)?;
            let found = self
                .editor
                .set_document_selection(&document_id, selection)
                .map_err(WorkspaceError::Editor)?;
            debug_assert!(found, "validated diagnostic document must still exist");
        }
        self.selected_index = index;
        Ok(())
    }

    pub(crate) fn navigate_to_live_diagnostic(
        &mut self,
        path: &WorkspacePath,
        start_byte: u64,
        end_byte: u64,
    ) -> Result<(), WorkspaceError> {
        self.ensure_mutable()?;
        let index = self
            .files
            .iter()
            .position(|file| &file.path == path)
            .ok_or_else(|| WorkspaceError::MissingPath { path: path.clone() })?;
        let document_id = self.files[index]
            .user_editable
            .then_some(self.files[index].document_id.clone())
            .flatten()
            .ok_or_else(|| WorkspaceError::NotEditable { path: path.clone() })?;
        let selection = rustrace_model::SelectionState::new(start_byte, end_byte);
        let buffer = self
            .editor
            .buffer_by_document_id(&document_id)
            .expect("every editable workspace file has a buffer");
        let positions = buffer
            .positions(buffer.version())
            .expect("the current buffer version is exact");
        if start_byte > end_byte
            || positions
                .byte_to_scalar(rustrace_editor::position::ByteOffset(start_byte))
                .is_err()
            || positions
                .byte_to_scalar(rustrace_editor::position::ByteOffset(end_byte))
                .is_err()
        {
            return Err(WorkspaceError::InvalidDiagnosticSpan);
        }
        if self.editor.active_document_id() != &document_id {
            self.record_focus(&document_id)?;
            let outcome = self.editor.activate_document(&document_id);
            debug_assert_eq!(outcome, EditorOutcome::BufferSwitched);
        }
        if self.editor.active_buffer().selection_state() != selection {
            self.record_selection(document_id.clone(), selection)?;
            let found = self
                .editor
                .set_document_selection(&document_id, selection)
                .map_err(WorkspaceError::Editor)?;
            debug_assert!(found, "validated diagnostic document must still exist");
        }
        self.selected_index = index;
        Ok(())
    }

    pub fn diagnostic_span_is_valid(
        &self,
        path: &WorkspacePath,
        span: &crate::diagnostics::DiagnosticSpan,
    ) -> bool {
        self.diagnostic_spans_are_valid(path, &[span])
            .into_iter()
            .next()
            .unwrap_or(false)
    }

    pub(crate) fn diagnostic_spans_are_valid(
        &self,
        path: &WorkspacePath,
        spans: &[&crate::diagnostics::DiagnosticSpan],
    ) -> Vec<bool> {
        let Some(document_id) = self
            .files
            .iter()
            .find(|file| &file.path == path)
            .filter(|file| file.user_editable)
            .and_then(|file| file.document_id.as_ref())
        else {
            return vec![false; spans.len()];
        };
        let text = self
            .editor
            .buffer_by_document_id(document_id)
            .expect("every editable workspace file has a buffer")
            .text();
        valid_diagnostic_spans(&text, spans)
    }

    pub fn execute_editor(
        &mut self,
        command: EditorCommand,
    ) -> Result<EditorOutcome, WorkspaceError> {
        self.effects
            .check_editor_command(&command)
            .map_err(WorkspaceError::InputRejected)?;
        // An uncertain controller may exit for inspection/recovery, never discard
        // or claim clean finalization. Leave buffers, confirmations and poison intact.
        if command == EditorCommand::RequestQuit
            && (self.recovery_reason().is_some() || self.effects.command_active())
        {
            return Ok(EditorOutcome::Quit);
        }
        self.ensure_mutable()?;
        self.ensure_active_user_editable()?;
        if command == EditorCommand::CloseActive {
            return Err(WorkspaceError::ControllerManagedCommand);
        }
        if command == EditorCommand::ConfirmDiscard && self.editor.quit_confirmation_pending() {
            self.discard_dirty_buffers()?;
        }
        self.set_active_text_limit()?;
        let document_before = self.editor.active_document_id().clone();
        let selection_before = self.editor.active_buffer().selection_state();
        let switching_buffers = matches!(
            &command,
            EditorCommand::NextBuffer | EditorCommand::PreviousBuffer
        );
        let switch_target = match command {
            EditorCommand::NextBuffer => self.switched_editable_document_id(1),
            EditorCommand::PreviousBuffer => self.switched_editable_document_id(-1),
            _ => None,
        };
        if let Some(document_id) = &switch_target {
            self.record_focus(document_id)?;
        }
        let editor_result = match switch_target {
            Some(document_id) => Ok(self.editor.activate_document(&document_id)),
            None if switching_buffers => Ok(EditorOutcome::NoChange),
            None => self.editor.execute(command),
        };
        let outcome = match editor_result {
            Ok(outcome) => outcome,
            Err(error @ TransactionError::Provenance(_)) => {
                return Err(self.poison(format!("editor provenance failed: {error}")));
            }
            Err(error) => return Err(WorkspaceError::Editor(error)),
        };
        let document_after = self.editor.active_document_id().clone();
        let selection_after = self.editor.active_buffer().selection_state();
        if !matches!(outcome, EditorOutcome::Edited | EditorOutcome::Replaced(_))
            && document_before == document_after
            && selection_before != selection_after
        {
            self.record_selection(document_after.clone(), selection_after)?;
        }
        if outcome == EditorOutcome::BufferSwitched {
            let active = self.editor.active_document_id();
            self.selected_index = self
                .files
                .iter()
                .position(|file| file.document_id.as_ref() == Some(active))
                .expect("every editor buffer belongs to one workspace file");
        }
        self.refresh_dirty();
        Ok(outcome)
    }

    fn switched_editable_document_id(&self, direction: isize) -> Option<DocumentId> {
        (1..self.editor.buffer_count()).find_map(|distance| {
            let offset = direction.saturating_mul(isize::try_from(distance).unwrap_or(isize::MAX));
            let document_id = self.editor.switched_document_id(offset)?;
            self.files
                .iter()
                .any(|file| file.user_editable && file.document_id.as_ref() == Some(document_id))
                .then(|| document_id.clone())
        })
    }

    pub fn save_active(&mut self) -> Result<(), WorkspaceError> {
        self.ensure_mutable()?;
        self.ensure_active_user_editable()?;
        let path = self.active_path().clone();
        let text = self.editor.active_buffer().text();
        let mut files = self.verified_disk_files()?;
        files.insert(path.clone(), text.as_bytes().to_vec());
        hash_entries(
            files
                .iter()
                .map(|(path, contents)| (path, contents.as_slice())),
        )
        .map_err(WorkspaceError::Workspace)?;
        if let Err(error) = write_workspace_file_in(&self.root, &path, text.as_bytes()) {
            return Err(self.poison(format!("save failed: {error}")));
        }
        self.files
            .iter_mut()
            .find(|file| file.path == path)
            .expect("active file belongs to the workspace tree")
            .persisted_contents = text.into_bytes();
        self.editor.mark_active_saved();
        self.refresh_dirty();
        Ok(())
    }

    pub fn create_file(&mut self, raw_path: &str) -> Result<WorkspaceOutcome, WorkspaceError> {
        self.ensure_mutable()?;
        self.verified_disk_files()?;
        if self.files.len() >= MAX_WORKSPACE_FILES {
            return Err(WorkspaceError::FileLimit);
        }
        let path = self.validate_editable_path(raw_path)?;
        if self.files.iter().any(|file| file.path == path) {
            return Err(WorkspaceError::Collision { path });
        }
        preflight_workspace_destination_in(&self.root, &path).map_err(WorkspaceError::Mutation)?;
        let document_id = document_id(self.next_document_id)?;
        let event = Event::FileCreated(FileCreated {
            document_id: document_id.clone(),
            path: path.clone(),
            contents: String::new(),
            content_hash: document_hash(""),
        });
        event.validate().map_err(WorkspaceError::Event)?;

        self.record_lifecycle(event)?;
        self.record_focus(&document_id)?;
        if let Err(error) = create_workspace_file_in(&self.root, &path) {
            return Err(self.poison(format!(
                "recorded create for `{path}` but filesystem mutation failed: {error}"
            )));
        }
        self.editor.open_buffer(
            document_id.clone(),
            self.root.path().join(path.as_str()),
            "",
            self.effects.clone(),
        );
        self.files.push(WorkspaceFile {
            path: path.clone(),
            document_id: Some(document_id.clone()),
            user_editable: true,
            dirty: false,
            persisted_contents: Vec::new(),
        });
        self.files
            .sort_unstable_by(|left, right| left.path.cmp(&right.path));
        self.selected_index = self
            .files
            .iter()
            .position(|file| file.path == path)
            .expect("created file was inserted");
        self.next_document_id += 1;
        Ok(WorkspaceOutcome::FileCreated)
    }

    pub fn rename_selected(&mut self, raw_path: &str) -> Result<WorkspaceOutcome, WorkspaceError> {
        self.ensure_mutable()?;
        self.verified_disk_files()?;
        let new_path = self.validate_editable_path(raw_path)?;
        if self.files.iter().any(|file| file.path == new_path) {
            return Err(WorkspaceError::Collision { path: new_path });
        }
        preflight_workspace_destination_in(&self.root, &new_path)
            .map_err(WorkspaceError::Mutation)?;
        let old_path = self.files[self.selected_index].path.clone();
        if !self.files[self.selected_index].user_editable {
            return Err(WorkspaceError::NotEditable { path: old_path });
        }
        let document_id = self.files[self.selected_index]
            .document_id
            .clone()
            .ok_or_else(|| WorkspaceError::NotEditable {
                path: old_path.clone(),
            })?;
        let event = Event::FileRenamed(FileRenamed {
            document_id: document_id.clone(),
            old_path: old_path.clone(),
            new_path: new_path.clone(),
        });
        event.validate().map_err(WorkspaceError::Event)?;

        self.record_lifecycle(event)?;
        if let Err(error) = rename_workspace_file_in(&self.root, &old_path, &new_path) {
            return Err(self.poison(format!(
                "recorded rename from `{old_path}` to `{new_path}` but filesystem mutation failed: {error}"
            )));
        }
        let renamed = self
            .editor
            .rename_document(&document_id, self.root.path().join(new_path.as_str()));
        debug_assert!(renamed, "selected editable file has an editor buffer");
        self.files[self.selected_index].path = new_path.clone();
        self.files
            .sort_unstable_by(|left, right| left.path.cmp(&right.path));
        self.selected_index = self
            .files
            .iter()
            .position(|file| file.path == new_path)
            .expect("renamed file remains in the tree");
        Ok(WorkspaceOutcome::FileRenamed)
    }

    pub fn request_delete_selected(&mut self) -> Result<WorkspaceOutcome, WorkspaceError> {
        self.ensure_mutable()?;
        self.verified_disk_files()?;
        let selected = &self.files[self.selected_index];
        if !selected.user_editable {
            return Err(WorkspaceError::NotEditable {
                path: selected.path.clone(),
            });
        }
        let document_id =
            selected
                .document_id
                .clone()
                .ok_or_else(|| WorkspaceError::NotEditable {
                    path: selected.path.clone(),
                })?;
        if self
            .files
            .iter()
            .filter(|file| file.user_editable && file.document_id.is_some())
            .count()
            == 1
        {
            return Err(WorkspaceError::LastEditableFile);
        }
        let pending = PendingDelete {
            path: selected.path.clone(),
            document_id,
        };
        if selected.dirty {
            self.pending_delete = Some(pending);
            Ok(WorkspaceOutcome::ConfirmationRequired)
        } else {
            self.delete(pending)
        }
    }

    pub fn confirm_delete(&mut self) -> Result<WorkspaceOutcome, WorkspaceError> {
        self.ensure_mutable()?;
        let Some(pending) = self.pending_delete.clone() else {
            return Ok(WorkspaceOutcome::NoChange);
        };
        let outcome = self.delete(pending)?;
        self.pending_delete = None;
        Ok(outcome)
    }

    pub fn cancel_delete(&mut self) -> WorkspaceOutcome {
        if self.effects.command_active() {
            return WorkspaceOutcome::NoChange;
        }
        if self.pending_delete.take().is_some() {
            WorkspaceOutcome::Cancelled
        } else {
            WorkspaceOutcome::NoChange
        }
    }

    pub fn logical_files(&self) -> Result<BTreeMap<WorkspacePath, Vec<u8>>, WorkspaceError> {
        self.ensure_healthy()?;
        self.verified_disk_files()?;
        self.buffered_files()
    }

    /// Logical authority independent of external disk changes. Production
    /// reconciliation uses this; ordinary workspace snapshots still verify disk.
    pub(crate) fn buffered_files(
        &self,
    ) -> Result<BTreeMap<WorkspacePath, Vec<u8>>, WorkspaceError> {
        self.ensure_healthy()?;
        let mut files = self.saved_files();
        for entry in &self.files {
            self.allowed_paths
                .validate(&entry.path)
                .map_err(WorkspaceError::Policy)?;
            if let Some(document_id) = &entry.document_id {
                let editor = self
                    .editor
                    .buffer_by_document_id(document_id)
                    .expect("every editable workspace file has a buffer");
                files.insert(entry.path.clone(), editor.text().into_bytes());
            }
        }
        hash_entries(
            files
                .iter()
                .map(|(path, contents)| (path, contents.as_slice())),
        )
        .map_err(WorkspaceError::Workspace)?;
        Ok(files)
    }

    pub(crate) fn saved_files(&self) -> BTreeMap<WorkspacePath, Vec<u8>> {
        self.files
            .iter()
            .map(|f| (f.path.clone(), f.persisted_contents.clone()))
            .collect()
    }

    /// Capture the exact existing Rust-document authority used by one Format.
    pub(crate) fn formatter_prestate(&self) -> Result<FormatterPrestate, WorkspaceError> {
        let files = self.logical_files()?;
        if self.saved_files() != files {
            return Err(WorkspaceError::Effects(WorkspaceEffectError::new(
                "formatter pre-state requires saved, disk and logical agreement",
            )));
        }
        let mut documents = Vec::new();
        for file in self.files.iter().filter(|file| is_rust_path(&file.path)) {
            let document_id =
                file.document_id
                    .clone()
                    .ok_or_else(|| WorkspaceError::NotEditable {
                        path: file.path.clone(),
                    })?;
            let buffer = self
                .editor
                .buffer_by_document_id(&document_id)
                .expect("every managed Rust document has one open buffer");
            documents.push(FormatterDocumentState {
                path: file.path.clone(),
                document_id,
                version: buffer.version(),
                content_hash: buffer.hash(),
            });
        }
        Ok((files, documents))
    }

    /// Capture the exact manifest and lockfile documents owned by one
    /// dependency command. Both files must already be present and allowed.
    pub(crate) fn dependency_prestate(&self) -> Result<FormatterPrestate, WorkspaceError> {
        let files = self.logical_files()?;
        if self.saved_files() != files {
            return Err(WorkspaceError::Effects(WorkspaceEffectError::new(
                "dependency pre-state requires saved, disk and logical agreement",
            )));
        }
        let dependency_paths = ["Cargo.toml", "Cargo.lock"]
            .into_iter()
            .map(|raw| WorkspacePath::new(raw).map_err(WorkspaceError::InvalidPath))
            .collect::<Result<Vec<_>, _>>()?;
        let mut documents = Vec::with_capacity(dependency_paths.len());
        for path in dependency_paths {
            self.allowed_paths
                .validate(&path)
                .map_err(WorkspaceError::Policy)?;
            let file = self
                .files
                .iter()
                .find(|file| file.path == path)
                .ok_or_else(|| WorkspaceError::MissingPath { path: path.clone() })?;
            let document_id = file
                .document_id
                .clone()
                .ok_or_else(|| WorkspaceError::NotEditable { path: path.clone() })?;
            let buffer = self
                .editor
                .buffer_by_document_id(&document_id)
                .expect("every managed dependency document has one open buffer");
            documents.push(FormatterDocumentState {
                path,
                document_id,
                version: buffer.version(),
                content_hash: buffer.hash(),
            });
        }
        Ok((files, documents))
    }

    /// Validate every returned file and materialize all formatter transactions
    /// before any one of them is durably accepted.
    pub(crate) fn preflight_formatter_changes(
        &self,
        before: &BTreeMap<WorkspacePath, Vec<u8>>,
        returned: &BTreeMap<WorkspacePath, Vec<u8>>,
        documents: &[FormatterDocumentState],
    ) -> Result<Vec<FormatterChange>, WorkspaceError> {
        self.ensure_healthy()?;
        if !self.effects.command_active() {
            return Err(WorkspaceError::Effects(WorkspaceEffectError::new(
                "formatter preflight requires active Format ownership",
            )));
        }
        if before.keys().ne(returned.keys()) {
            return Err(WorkspaceError::Effects(WorkspaceEffectError::new(
                "formatter returned file creation, deletion or rename",
            )));
        }
        if self.saved_files() != *before || self.buffered_files()? != *before {
            return Err(WorkspaceError::Effects(WorkspaceEffectError::new(
                "formatter document identity or canonical pre-state changed",
            )));
        }

        let mut changes = Vec::new();
        for (path, after) in returned {
            let old = &before[path];
            if old == after {
                continue;
            }
            if !is_rust_path(path) {
                return Err(WorkspaceError::Effects(WorkspaceEffectError::new(format!(
                    "formatter changed non-Rust managed file `{path}`"
                ))));
            }
            let state = documents
                .iter()
                .find(|document| &document.path == path)
                .ok_or_else(|| WorkspaceError::NotEditable { path: path.clone() })?;
            let file = self
                .files
                .iter()
                .find(|file| &file.path == path)
                .ok_or_else(|| WorkspaceError::MissingPath { path: path.clone() })?;
            let buffer = self
                .editor
                .buffer_by_document_id(&state.document_id)
                .ok_or_else(|| WorkspaceError::MissingPath { path: path.clone() })?;
            if file.document_id.as_ref() != Some(&state.document_id)
                || buffer.version() != state.version
                || buffer.hash() != state.content_hash
                || buffer.text().as_bytes() != old
            {
                return Err(WorkspaceError::Effects(WorkspaceEffectError::new(format!(
                    "formatter document identity/version changed for `{path}`"
                ))));
            }
            let replacement = std::str::from_utf8(after)
                .map_err(|_| WorkspaceError::InvalidUtf8 { path: path.clone() })?;
            let transaction = self
                .editor
                .preview_formatter_replacement(&state.document_id, replacement)
                .map_err(WorkspaceError::Editor)?
                .ok_or_else(|| {
                    WorkspaceError::Effects(WorkspaceEffectError::new(
                        "formatter change disappeared during preflight",
                    ))
                })?;
            changes.push(FormatterChange {
                path: path.clone(),
                before: old.clone(),
                after: after.clone(),
                transaction,
            });
        }
        Ok(changes)
    }

    /// Validate the returned dependency tree and materialize one transaction
    /// for each changed manifest/lockfile before publishing either change.
    pub(crate) fn preflight_dependency_changes(
        &self,
        before: &BTreeMap<WorkspacePath, Vec<u8>>,
        returned: &BTreeMap<WorkspacePath, Vec<u8>>,
        documents: &[FormatterDocumentState],
    ) -> Result<Vec<FormatterChange>, WorkspaceError> {
        self.ensure_healthy()?;
        if !self.effects.command_active() {
            return Err(WorkspaceError::Effects(WorkspaceEffectError::new(
                "dependency preflight requires active command ownership",
            )));
        }
        if before.keys().ne(returned.keys()) {
            return Err(WorkspaceError::Effects(WorkspaceEffectError::new(
                "dependency command returned file creation, deletion or rename",
            )));
        }
        if self.saved_files() != *before || self.buffered_files()? != *before {
            return Err(WorkspaceError::Effects(WorkspaceEffectError::new(
                "dependency document identity or canonical pre-state changed",
            )));
        }

        let mut changes = Vec::new();
        for (path, after) in returned {
            let old = &before[path];
            if old == after {
                continue;
            }
            if !matches!(path.as_str(), "Cargo.toml" | "Cargo.lock") {
                return Err(WorkspaceError::Effects(WorkspaceEffectError::new(format!(
                    "dependency command changed unauthorized managed file `{path}`"
                ))));
            }
            let state = documents
                .iter()
                .find(|document| &document.path == path)
                .ok_or_else(|| WorkspaceError::NotEditable { path: path.clone() })?;
            let file = self
                .files
                .iter()
                .find(|file| &file.path == path)
                .ok_or_else(|| WorkspaceError::MissingPath { path: path.clone() })?;
            let buffer = self
                .editor
                .buffer_by_document_id(&state.document_id)
                .ok_or_else(|| WorkspaceError::MissingPath { path: path.clone() })?;
            if file.document_id.as_ref() != Some(&state.document_id)
                || buffer.version() != state.version
                || buffer.hash() != state.content_hash
                || buffer.text().as_bytes() != old
            {
                return Err(WorkspaceError::Effects(WorkspaceEffectError::new(format!(
                    "dependency document identity/version changed for `{path}`"
                ))));
            }
            let replacement = std::str::from_utf8(after)
                .map_err(|_| WorkspaceError::InvalidUtf8 { path: path.clone() })?;
            let transaction = self
                .editor
                .preview_tool_replacement(
                    &state.document_id,
                    rustrace_model::EditOrigin::DependencyTool,
                    replacement,
                )
                .map_err(WorkspaceError::Editor)?
                .ok_or_else(|| {
                    WorkspaceError::Effects(WorkspaceEffectError::new(
                        "dependency change disappeared during preflight",
                    ))
                })?;
            changes.push(FormatterChange {
                path: path.clone(),
                before: old.clone(),
                after: after.clone(),
                transaction,
            });
        }
        Ok(changes)
    }

    /// Commit one already-preflighted formatter document through the existing
    /// editor/writer gateway, then publish and verify its exact disk bytes.
    pub(crate) fn apply_formatter_change(
        &mut self,
        change: FormatterChange,
    ) -> Result<(), WorkspaceError> {
        self.ensure_healthy()?;
        if !self.effects.command_active()
            || change.transaction.origin != rustrace_model::EditOrigin::Formatter
        {
            return Err(WorkspaceError::Effects(WorkspaceEffectError::new(
                "formatter publication lacks active Format ownership",
            )));
        }
        let file = self
            .files
            .iter()
            .find(|file| file.path == change.path)
            .ok_or_else(|| WorkspaceError::MissingPath {
                path: change.path.clone(),
            })?;
        if file.persisted_contents != change.before
            || file.document_id.as_ref() != Some(&change.transaction.document_id)
        {
            return Err(WorkspaceError::Effects(WorkspaceEffectError::new(
                "formatter publication pre-state changed",
            )));
        }
        let changed = self
            .editor
            .apply_document_transaction(&change.transaction.document_id, change.transaction.clone())
            .map_err(WorkspaceError::Editor)?;
        if !changed {
            return Err(self.poison("durably accepted formatter transaction became a no-op"));
        }
        crate::session::process_probe("format-intent");
        if let Err(error) = write_workspace_file_in(&self.root, &change.path, &change.after) {
            return Err(self.poison(format!("formatter publication failed: {error}")));
        }
        crate::session::process_probe("format-disk");
        let file = self
            .files
            .iter_mut()
            .find(|file| file.path == change.path)
            .expect("preflighted formatter file remains managed");
        file.persisted_contents = change.after;
        self.editor
            .mark_document_saved(&change.transaction.document_id);
        self.refresh_dirty();
        Ok(())
    }

    /// Publish one preflighted dependency document through the editor/writer
    /// gateway, then advance the saved baseline to the exact returned bytes.
    pub(crate) fn apply_dependency_change(
        &mut self,
        change: FormatterChange,
    ) -> Result<(), WorkspaceError> {
        self.ensure_healthy()?;
        if !self.effects.command_active()
            || change.transaction.origin != rustrace_model::EditOrigin::DependencyTool
        {
            return Err(WorkspaceError::Effects(WorkspaceEffectError::new(
                "dependency publication lacks active command ownership",
            )));
        }
        let file = self
            .files
            .iter()
            .find(|file| file.path == change.path)
            .ok_or_else(|| WorkspaceError::MissingPath {
                path: change.path.clone(),
            })?;
        if file.persisted_contents != change.before
            || file.document_id.as_ref() != Some(&change.transaction.document_id)
        {
            return Err(WorkspaceError::Effects(WorkspaceEffectError::new(
                "dependency publication pre-state changed",
            )));
        }
        let changed = self
            .editor
            .apply_document_transaction(&change.transaction.document_id, change.transaction.clone())
            .map_err(WorkspaceError::Editor)?;
        if !changed {
            return Err(self.poison("durably accepted dependency transaction became a no-op"));
        }
        crate::session::process_probe("dependency-intent");
        if let Err(error) = write_workspace_file_in(&self.root, &change.path, &change.after) {
            return Err(self.poison(format!("dependency publication failed: {error}")));
        }
        crate::session::process_probe("dependency-disk");
        let file = self
            .files
            .iter_mut()
            .find(|file| file.path == change.path)
            .expect("preflighted dependency file remains managed");
        file.persisted_contents = change.after;
        self.editor
            .mark_document_saved(&change.transaction.document_id);
        self.refresh_dirty();
        Ok(())
    }

    pub(crate) fn adopt_verified_baseline(
        &mut self,
        disk: &BTreeMap<WorkspacePath, Vec<u8>>,
    ) -> Result<(), WorkspaceError> {
        if &read_pinned_workspace(&self.root).map_err(WorkspaceError::Workspace)? != disk {
            return Err(WorkspaceError::ExternalFileChange {
                path: self.active_path().clone(),
            });
        }
        for file in &mut self.files {
            file.persisted_contents = disk
                .get(&file.path)
                .ok_or_else(|| WorkspaceError::MissingPath {
                    path: file.path.clone(),
                })?
                .clone();
            if let Some(id) = &file.document_id {
                let buffer = self
                    .editor
                    .buffer_by_document_id(id)
                    .expect("open document");
                if buffer.text().as_bytes() == file.persisted_contents {
                    self.editor.mark_document_saved(id);
                }
            }
        }
        // Canonical restoration changes only the persisted baseline. The tree
        // and document identities have not changed; preserve the selected file
        // and any pending operation even when another document is active.
        self.refresh_dirty();
        Ok(())
    }

    /// Reads and verifies disk state through the retained workspace authority.
    pub fn disk_files(&self) -> Result<BTreeMap<WorkspacePath, Vec<u8>>, WorkspaceError> {
        self.ensure_healthy()?;
        self.verified_disk_files()
    }

    fn validate_editable_path(&self, raw_path: &str) -> Result<WorkspacePath, WorkspaceError> {
        let path = WorkspacePath::new(raw_path).map_err(WorkspaceError::InvalidPath)?;
        if path.as_str().chars().any(char::is_control) {
            return Err(WorkspaceError::UnsafeDisplayPath { path });
        }
        if is_hash_excluded_root(&path) {
            return Err(WorkspaceError::HashExcludedPath { path });
        }
        if !editable_file_type(&path) {
            return Err(WorkspaceError::UnsupportedFileType { path });
        }
        self.allowed_paths
            .validate(&path)
            .map_err(WorkspaceError::Policy)?;
        Ok(path)
    }

    fn delete(&mut self, pending: PendingDelete) -> Result<WorkspaceOutcome, WorkspaceError> {
        self.ensure_healthy()?;
        self.verified_disk_files()?;
        let editor = self
            .editor
            .buffer_by_document_id(&pending.document_id)
            .ok_or_else(|| WorkspaceError::MissingPath {
                path: pending.path.clone(),
            })?;
        let event = Event::FileDeleted(FileDeleted {
            document_id: pending.document_id.clone(),
            path: pending.path.clone(),
            previous_hash: editor.hash(),
        });
        event.validate().map_err(WorkspaceError::Event)?;
        let fallback = if self.editor.active_document_id() == &pending.document_id {
            Some(
                self.switched_editable_document_id(1)
                    .ok_or(WorkspaceError::LastEditableFile)?,
            )
        } else {
            None
        };
        self.record_lifecycle(event)?;
        if let Some(document_id) = &fallback {
            self.record_focus(document_id)?;
        }
        if let Err(error) = remove_workspace_file_in(&self.root, &pending.path) {
            return Err(self.poison(format!(
                "recorded delete for `{}` but filesystem mutation failed: {error}",
                pending.path
            )));
        }
        if let Some(document_id) = &fallback {
            let activated = self.editor.activate_document(document_id);
            debug_assert!(matches!(
                activated,
                EditorOutcome::BufferSwitched | EditorOutcome::NoChange
            ));
        }
        let removed = self.editor.remove_document(&pending.document_id);
        debug_assert_eq!(removed, EditorOutcome::BufferClosed);
        let removed_index = self
            .files
            .iter()
            .position(|file| file.path == pending.path)
            .expect("pending file remains in the tree until deletion succeeds");
        self.files.remove(removed_index);
        let active = self.editor.active_document_id();
        self.selected_index = self
            .files
            .iter()
            .position(|file| file.document_id.as_ref() == Some(active))
            .expect("one remaining active editor buffer belongs to the tree");
        Ok(WorkspaceOutcome::FileDeleted)
    }

    fn record_focus(&mut self, document_id: &DocumentId) -> Result<(), WorkspaceError> {
        let event = Event::FileFocused(FileFocused {
            document_id: document_id.clone(),
        });
        event.validate().map_err(WorkspaceError::Event)?;
        if let Err(error) = self.effects.record_workspace_event(event) {
            return Err(self.poison(format!("focus provenance failed: {error}")));
        }
        Ok(())
    }

    pub(crate) fn ensure_active_user_editable(&self) -> Result<(), WorkspaceError> {
        let path = self.active_path();
        if self.files.iter().any(|file| {
            &file.path == path
                && file.user_editable
                && file.document_id.as_ref() == Some(self.editor.active_document_id())
        }) {
            Ok(())
        } else {
            Err(WorkspaceError::ActiveFileNotEditable { path: path.clone() })
        }
    }

    fn record_selection(
        &mut self,
        document_id: DocumentId,
        selection: rustrace_model::SelectionState,
    ) -> Result<(), WorkspaceError> {
        let event = Event::SelectionChanged(SelectionChanged {
            document_id,
            anchor_byte: selection.anchor_byte,
            active_byte: selection.active_byte,
        });
        if let Err(error) = self.effects.record_workspace_event(event) {
            return Err(self.poison(format!("selection provenance failed: {error}")));
        }
        Ok(())
    }

    fn record_lifecycle(&mut self, event: Event) -> Result<(), WorkspaceError> {
        if let Err(error) = self.effects.record_lifecycle(event) {
            return Err(self.poison(format!("lifecycle provenance failed: {error}")));
        }
        Ok(())
    }

    /// The first local or shared recorder failure, available for inspection.
    pub fn recovery_reason(&self) -> Option<String> {
        self.poison
            .clone()
            .or_else(|| self.effects.recovery_reason())
    }

    fn ensure_healthy(&self) -> Result<(), WorkspaceError> {
        match self.recovery_reason() {
            Some(reason) => Err(WorkspaceError::Poisoned { reason }),
            None => Ok(()),
        }
    }

    fn ensure_mutable(&self) -> Result<(), WorkspaceError> {
        self.ensure_healthy()?;
        if self.effects.command_active() {
            Err(WorkspaceError::Effects(WorkspaceEffectError::new(
                "command owns workspace; cancel or wait for its durable post-boundary",
            )))
        } else {
            Ok(())
        }
    }

    fn poison(&mut self, reason: impl Into<String>) -> WorkspaceError {
        if self.poison.is_none() {
            self.poison = Some(reason.into());
        }
        WorkspaceError::Poisoned {
            reason: self.poison.clone().expect("poison reason was initialized"),
        }
    }

    fn refresh_dirty(&mut self) {
        for file in &mut self.files {
            file.dirty = file.document_id.as_ref().is_some_and(|document_id| {
                self.editor
                    .is_document_dirty(document_id)
                    .expect("every editable workspace file has a buffer")
            });
        }
    }

    fn set_active_text_limit(&mut self) -> Result<(), WorkspaceError> {
        let retained_bytes = self
            .files
            .iter()
            .map(|file| match &file.document_id {
                Some(document_id) if document_id == self.editor.active_document_id() => 0,
                Some(document_id) => self
                    .editor
                    .buffer_by_document_id(document_id)
                    .expect("every editable workspace file has a buffer")
                    .len_bytes(),
                None => file.persisted_contents.len(),
            })
            .try_fold(0_usize, usize::checked_add)
            .unwrap_or(usize::MAX);
        let aggregate_headroom = usize::try_from(MAX_WORKSPACE_TOTAL_BYTES)
            .unwrap_or(usize::MAX)
            .saturating_sub(retained_bytes);
        let per_file_limit = usize::try_from(MAX_WORKSPACE_FILE_BYTES).unwrap_or(usize::MAX);
        self.editor
            .set_active_text_byte_limit(aggregate_headroom.min(per_file_limit))
            .map_err(WorkspaceError::Editor)?;
        Ok(())
    }

    fn verified_disk_files(&self) -> Result<BTreeMap<WorkspacePath, Vec<u8>>, WorkspaceError> {
        self.ensure_healthy()?;
        let files = read_pinned_workspace(&self.root).map_err(WorkspaceError::Workspace)?;
        for path in files.keys() {
            self.allowed_paths
                .validate(path)
                .map_err(WorkspaceError::Policy)?;
            if !self.files.iter().any(|file| &file.path == path) {
                return Err(WorkspaceError::UnmanagedPath { path: path.clone() });
            }
        }
        for entry in &self.files {
            let Some(contents) = files.get(&entry.path) else {
                return Err(WorkspaceError::ExternalFileChange {
                    path: entry.path.clone(),
                });
            };
            if contents != &entry.persisted_contents {
                return Err(WorkspaceError::ExternalFileChange {
                    path: entry.path.clone(),
                });
            }
        }
        Ok(files)
    }

    fn discard_dirty_buffers(&mut self) -> Result<(), WorkspaceError> {
        self.verified_disk_files()?;
        let per_file_limit = usize::try_from(MAX_WORKSPACE_FILE_BYTES).unwrap_or(usize::MAX);
        let mut replacements = Vec::new();
        for file in self.files.iter().filter(|file| file.dirty) {
            let document_id = file
                .document_id
                .as_ref()
                .expect("only editable workspace files can be dirty");
            let current_len = self
                .editor
                .buffer_by_document_id(document_id)
                .expect("every editable workspace file has a buffer")
                .len_bytes();
            let persisted = std::str::from_utf8(&file.persisted_contents)
                .expect("editable file baseline was validated as UTF-8 at open/save");
            let transactions = self
                .editor
                .preview_document_replacement(document_id, persisted, per_file_limit)
                .map_err(WorkspaceError::Editor)?;
            replacements.push((
                file.persisted_contents.len() > current_len,
                document_id.clone(),
                transactions,
            ));
        }
        replacements.sort_by_key(|(grows, _, _)| *grows);
        for (_, document_id, _) in &replacements {
            let found = self
                .editor
                .set_document_text_byte_limit(document_id, per_file_limit)
                .map_err(WorkspaceError::Editor)?;
            debug_assert!(found, "preflighted discard document must still exist");
        }
        for (_, document_id, transactions) in replacements {
            for transaction in transactions {
                let changed = match self
                    .editor
                    .apply_document_transaction(&document_id, transaction)
                {
                    Ok(changed) => changed,
                    Err(error @ TransactionError::Provenance(_)) => {
                        self.refresh_dirty();
                        return Err(self.poison(format!(
                            "discard provenance failed after an authoritative transaction prefix: {error}"
                        )));
                    }
                    Err(error) => return Err(WorkspaceError::Editor(error)),
                };
                debug_assert!(
                    changed,
                    "preflighted discard transaction must change content"
                );
            }
            let marked = self.editor.mark_document_saved(&document_id);
            debug_assert!(marked, "preflighted discard document must still exist");
        }
        self.refresh_dirty();
        Ok(())
    }
}

fn editable_file_type(path: &WorkspacePath) -> bool {
    matches!(
        Path::new(path.as_str())
            .extension()
            .and_then(|extension| extension.to_str()),
        Some("rs" | "toml")
    )
}

fn controller_document_type(path: &WorkspacePath, dependency_documents: bool) -> bool {
    editable_file_type(path) || dependency_documents && path.as_str() == "Cargo.lock"
}

fn valid_diagnostic_spans(text: &str, spans: &[&crate::diagnostics::DiagnosticSpan]) -> Vec<bool> {
    valid_diagnostic_spans_with_observer(text, spans, || {})
}

fn valid_diagnostic_spans_with_observer<F>(
    text: &str,
    spans: &[&crate::diagnostics::DiagnosticSpan],
    mut source_scan_started: F,
) -> Vec<bool>
where
    F: FnMut(),
{
    let mut endpoints = Vec::with_capacity(spans.len().saturating_mul(2));
    let candidates = spans
        .iter()
        .map(|span| {
            let (Ok(start), Ok(end)) = (
                usize::try_from(span.byte_start),
                usize::try_from(span.byte_end),
            ) else {
                return None;
            };
            if start > end
                || end > text.len()
                || !text.is_char_boundary(start)
                || !text.is_char_boundary(end)
                || span.line_start == 0
                || span.line_end == 0
                || span.column_start == 0
                || span.column_end == 0
            {
                return None;
            }
            endpoints.extend([start, end]);
            Some((start, end))
        })
        .collect::<Vec<_>>();
    endpoints.sort_unstable();
    endpoints.dedup();

    if endpoints.is_empty() {
        return vec![false; spans.len()];
    }

    let mut positions = vec![None; endpoints.len()];
    let mut endpoint_index = 0;
    let mut line = 1_u64;
    let mut column = 1_u64;
    let bytes = text.as_bytes();
    source_scan_started();
    for (offset, character) in text.char_indices() {
        while endpoints.get(endpoint_index) == Some(&offset) {
            positions[endpoint_index] =
                if character == '\n' && offset > 0 && bytes[offset - 1] == b'\r' {
                    None
                } else {
                    Some((line, column))
                };
            endpoint_index += 1;
        }
        if endpoint_index == endpoints.len() {
            break;
        }
        if character == '\r' && bytes.get(offset + 1) == Some(&b'\n') {
            continue;
        }
        if character == '\n' {
            line = line.saturating_add(1);
            column = 1;
        } else {
            column = column.saturating_add(1);
        }
    }
    while endpoints.get(endpoint_index) == Some(&text.len()) {
        positions[endpoint_index] = Some((line, column));
        endpoint_index += 1;
    }

    spans
        .iter()
        .zip(candidates)
        .map(|(span, candidate)| {
            let Some((start, end)) = candidate else {
                return false;
            };
            let position = |offset| {
                endpoints
                    .binary_search(&offset)
                    .ok()
                    .and_then(|index| positions[index])
            };
            position(start) == Some((span.line_start, span.column_start))
                && position(end) == Some((span.line_end, span.column_end))
        })
        .collect()
}

fn is_rust_path(path: &WorkspacePath) -> bool {
    Path::new(path.as_str())
        .extension()
        .and_then(|value| value.to_str())
        == Some("rs")
}

fn is_hash_excluded_root(path: &WorkspacePath) -> bool {
    path.as_str().split('/').next().is_some_and(|component| {
        component.eq_ignore_ascii_case(".rustrace") || component.eq_ignore_ascii_case("target")
    })
}

fn document_id(index: u64) -> Result<DocumentId, WorkspaceError> {
    DocumentId::new(format!("workspace-file-{index:016x}")).map_err(WorkspaceError::Identifier)
}

#[derive(Debug)]
pub enum WorkspaceError {
    InputRejected(WorkspaceEffectError),
    InvalidPath(WorkspacePathError),
    Identifier(IdentifierError),
    Policy(AllowedPathSetError),
    Workspace(WorkspaceHashError),
    Mutation(WorkspaceMutationError),
    Event(ValidationError),
    Editor(TransactionError),
    Effects(WorkspaceEffectError),
    Filesystem {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    InvalidUtf8 {
        path: WorkspacePath,
    },
    NoEditableFiles,
    NotEditable {
        path: WorkspacePath,
    },
    UnsupportedFileType {
        path: WorkspacePath,
    },
    UnsafeDisplayPath {
        path: WorkspacePath,
    },
    HashExcludedPath {
        path: WorkspacePath,
    },
    MissingPath {
        path: WorkspacePath,
    },
    InvalidIndex {
        target: &'static str,
        index: usize,
    },
    Collision {
        path: WorkspacePath,
    },
    UnmanagedPath {
        path: WorkspacePath,
    },
    ExternalFileChange {
        path: WorkspacePath,
    },
    InvalidDiagnosticSpan,
    ControllerManagedCommand,
    ActiveFileNotEditable {
        path: WorkspacePath,
    },
    FileLimit,
    LastEditableFile,
    Poisoned {
        reason: String,
    },
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputRejected(source) => source.fmt(formatter),
            Self::InvalidPath(source) => write!(formatter, "invalid workspace path: {source}"),
            Self::Identifier(source) => write!(formatter, "invalid document identifier: {source}"),
            Self::Policy(source) => source.fmt(formatter),
            Self::Workspace(source) => write!(formatter, "cannot read workspace: {source}"),
            Self::Mutation(source) => source.fmt(formatter),
            Self::Event(source) => write!(formatter, "invalid lifecycle event: {source}"),
            Self::Editor(source) => write!(formatter, "editor operation rejected: {source}"),
            Self::Effects(source) => {
                write!(formatter, "lifecycle event was not recorded: {source}")
            }
            Self::Filesystem {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "failed to {operation} `{}`: {source}",
                path.display()
            ),
            Self::InvalidUtf8 { path } => {
                write!(formatter, "editable file `{path}` is not valid UTF-8")
            }
            Self::NoEditableFiles => formatter.write_str("workspace has no editable files"),
            Self::NotEditable { path } => write!(
                formatter,
                "workspace file `{path}` is visible but not an editable .rs or .toml file"
            ),
            Self::UnsupportedFileType { path } => {
                write!(
                    formatter,
                    "workspace file `{path}` must end in .rs or .toml"
                )
            }
            Self::UnsafeDisplayPath { .. } => {
                formatter.write_str("workspace path contains a terminal control character")
            }
            Self::HashExcludedPath { path } => write!(
                formatter,
                "workspace file `{path}` is excluded from submission and workspace hashing"
            ),
            Self::MissingPath { path } => write!(formatter, "workspace path `{path}` is missing"),
            Self::InvalidIndex { target, index } => {
                write!(
                    formatter,
                    "workspace {target} index {index} is out of bounds"
                )
            }
            Self::Collision { path } => {
                write!(formatter, "workspace path `{path}` already exists")
            }
            Self::UnmanagedPath { path } => write!(
                formatter,
                "workspace path `{path}` appeared outside the managed file tree"
            ),
            Self::ExternalFileChange { path } => write!(
                formatter,
                "workspace path `{path}` changed outside the editor session"
            ),
            Self::InvalidDiagnosticSpan => formatter.write_str(
                "compiler span does not identify valid current UTF-8 source coordinates",
            ),
            Self::ControllerManagedCommand => formatter.write_str(
                "buffer close is managed by workspace delete so lifecycle provenance is preserved",
            ),
            Self::ActiveFileNotEditable { path } => write!(
                formatter,
                "workspace file `{path}` is managed automatically and cannot be edited or saved directly"
            ),
            Self::FileLimit => write!(
                formatter,
                "workspace already contains the maximum {MAX_WORKSPACE_FILES} files"
            ),
            Self::LastEditableFile => {
                formatter.write_str("the last editable workspace file cannot be deleted")
            }
            Self::Poisoned { reason } => {
                write!(formatter, "workspace session requires recovery: {reason}")
            }
        }
    }
}

impl Error for WorkspaceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InputRejected(source) => Some(source),
            Self::InvalidPath(source) => Some(source),
            Self::Identifier(source) => Some(source),
            Self::Policy(source) => Some(source),
            Self::Workspace(source) => Some(source),
            Self::Mutation(source) => Some(source),
            Self::Event(source) => Some(source),
            Self::Editor(source) => Some(source),
            Self::Effects(source) => Some(source),
            Self::Filesystem { source, .. } => Some(source),
            Self::InvalidUtf8 { .. }
            | Self::NoEditableFiles
            | Self::NotEditable { .. }
            | Self::UnsupportedFileType { .. }
            | Self::UnsafeDisplayPath { .. }
            | Self::HashExcludedPath { .. }
            | Self::MissingPath { .. }
            | Self::InvalidIndex { .. }
            | Self::Collision { .. }
            | Self::UnmanagedPath { .. }
            | Self::ExternalFileChange { .. }
            | Self::InvalidDiagnosticSpan
            | Self::ControllerManagedCommand
            | Self::ActiveFileNotEditable { .. }
            | Self::FileLimit
            | Self::LastEditableFile
            | Self::Poisoned { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        EditorCommand, EditorEffects, EditorOutcome, Event, WorkspaceEffectError, WorkspaceEffects,
        WorkspaceSession, valid_diagnostic_spans_with_observer,
    };
    use crate::diagnostics::{DiagnosticSpan, MAX_DIAGNOSTICS};
    use crate::editor::{EditorEffectError, EditorTransaction};
    use rustrace_model::assignment::AssignmentManifest;
    use rustrace_model::{FileFocused, WorkspacePath};

    #[derive(Clone, Default)]
    struct RecordingEffects(Rc<RefCell<Vec<Event>>>);

    impl EditorEffects for RecordingEffects {
        fn record_provenance(
            &mut self,
            transaction: &EditorTransaction,
        ) -> Result<(), EditorEffectError> {
            self.0
                .borrow_mut()
                .push(Event::FileEdited(transaction.clone()));
            Ok(())
        }
    }

    impl WorkspaceEffects for RecordingEffects {
        fn record_workspace_event(&mut self, event: Event) -> Result<(), WorkspaceEffectError> {
            self.0.borrow_mut().push(event);
            Ok(())
        }

        fn record_lifecycle(&mut self, event: Event) -> Result<(), WorkspaceEffectError> {
            self.0.borrow_mut().push(event);
            Ok(())
        }
    }

    fn manifest() -> AssignmentManifest {
        AssignmentManifest::parse(
            br#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Controller document guard"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["Cargo.lock", "Cargo.toml", "src/**/*.rs"]

[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#,
        )
        .unwrap()
    }

    fn controller_document_workspace() -> (std::path::PathBuf, WorkspaceSession<RecordingEffects>) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rustrace-controller-document-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("Cargo.lock"), "version = 4\n").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\n[workspace]\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() {}\n").unwrap();
        let workspace = WorkspaceSession::open(
            &root,
            &manifest(),
            Some(WorkspacePath::new("Cargo.toml").unwrap()),
            RecordingEffects::default(),
        )
        .unwrap();
        (root, workspace)
    }

    #[test]
    fn direct_editor_and_save_operations_reject_a_controller_managed_active_document() {
        let (root, mut workspace) = controller_document_workspace();
        let lock_path = WorkspacePath::new("Cargo.lock").unwrap();
        let lock_document = workspace
            .files
            .iter()
            .find(|file| file.path == lock_path)
            .and_then(|file| file.document_id.clone())
            .unwrap();
        assert_eq!(
            workspace.editor.activate_document(&lock_document),
            EditorOutcome::BufferSwitched
        );
        let lock_before = fs::read(root.join("Cargo.lock")).unwrap();

        let edit = workspace.execute_editor(EditorCommand::Insert('!'));
        let save_active = workspace.save_active();
        let save_all = workspace.save_all();

        for error in [
            edit.unwrap_err(),
            save_active.unwrap_err(),
            save_all.unwrap_err(),
        ] {
            assert_eq!(
                error.to_string(),
                "workspace file `Cargo.lock` is managed automatically and cannot be edited or saved directly"
            );
        }
        assert_eq!(workspace.active_buffer().text().as_bytes(), lock_before);
        assert_eq!(fs::read(root.join("Cargo.lock")).unwrap(), lock_before);
        assert!(workspace.effects.0.borrow().iter().all(
            |event| !matches!(event, Event::FileFocused(FileFocused { document_id }) if document_id == &lock_document)
                && !matches!(event, Event::FileEdited(edit) if edit.document_id == lock_document)
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn marker_validation_prepares_one_maximum_file_once_for_maximum_diagnostics() {
        let text = "x"
            .repeat(usize::try_from(rustrace_workspace::hash::MAX_WORKSPACE_FILE_BYTES).unwrap());
        let spans = (0..MAX_DIAGNOSTICS)
            .map(|index| {
                let start = index * (text.len() / MAX_DIAGNOSTICS);
                DiagnosticSpan {
                    file_name: "main.rs".to_owned(),
                    byte_start: u64::try_from(start).unwrap(),
                    byte_end: u64::try_from(start + 1).unwrap(),
                    line_start: 1,
                    line_end: 1,
                    column_start: u64::try_from(start).unwrap() + 1,
                    column_end: u64::try_from(start).unwrap() + 2,
                    is_primary: true,
                    label: None,
                    suggested_replacement: None,
                    suggestion_applicability: None,
                }
            })
            .collect::<Vec<_>>();
        let span_refs = spans.iter().collect::<Vec<_>>();
        let mut source_scans = 0;

        let valid = valid_diagnostic_spans_with_observer(&text, &span_refs, || {
            source_scans += 1;
        });

        assert_eq!(valid, vec![true; MAX_DIAGNOSTICS]);
        assert_eq!(source_scans, 1);
    }
}
