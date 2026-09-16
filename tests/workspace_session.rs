use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use crossterm::event::{
    Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    layout::Rect,
    style::{Color, Modifier},
};
use rustrace::config::PrimaryModifier;
use rustrace::diagnostics::DiagnosticSpan;
use rustrace::editor::{EditorEffectError, EditorEffects, EditorTransaction, Movement, Viewport};
use rustrace::tui::theme::Palette;
use rustrace::tui::{
    BufferTabViewEntry, CTRL_W_DELETE_SELECTED_HELP_ENTRY, DiagnosticLineMarker,
    DiagnosticMarkerKind, DrawGate, EditorCommand, EditorOutcome, JournalHealth, MainLayout,
    MainView, MainViewState, MouseState, OutputRow, RecordingState, ShellInput, ShellState,
    WorkspaceEffectError, WorkspaceEffects, WorkspaceError, WorkspaceFocus, WorkspaceInput,
    WorkspaceOutcome, WorkspaceSession, diagnostic_delta_for_event, main_layout,
    mouse_input_for_event, workspace_input_for_event as workspace_input_for_event_with_modifier,
};
use rustrace_journal::{CheckpointFile, CheckpointSnapshot, OpenDocument, StoredCheckpoint};
use rustrace_model::assignment::AssignmentManifest;
use rustrace_model::{
    EditOrigin, Event, EventEnvelope, FORMAT_VERSION_V1, FileFocused, Hash, SessionId,
    WorkspacePath, compute_event_hash, document_hash,
};
use rustrace_replay::ReplayEngine;
use rustrace_workspace::hash::{
    MAX_WORKSPACE_FILE_BYTES, MAX_WORKSPACE_TOTAL_BYTES, hash_workspace, read_workspace,
};

#[derive(Clone, Default)]
struct RecordingEffects(Rc<RefCell<Vec<Event>>>);

impl RecordingEffects {
    fn events(&self) -> Vec<Event> {
        self.0.borrow().clone()
    }
}

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
        assert!(matches!(
            event,
            Event::SelectionChanged(_) | Event::FileFocused(_)
        ));
        self.0.borrow_mut().push(event);
        Ok(())
    }

    fn record_lifecycle(&mut self, event: Event) -> Result<(), WorkspaceEffectError> {
        assert!(matches!(
            event,
            Event::FileCreated(_) | Event::FileDeleted(_) | Event::FileRenamed(_)
        ));
        self.0.borrow_mut().push(event);
        Ok(())
    }
}

#[derive(Clone, Default)]
struct RejectingLifecycleEffects {
    events: Rc<RefCell<Vec<Event>>>,
    reject: Rc<RefCell<bool>>,
    provenance_attempts: Rc<RefCell<usize>>,
    reject_provenance_at: Rc<RefCell<Option<usize>>>,
    sabotage_create: Rc<RefCell<Option<PathBuf>>>,
}

impl RejectingLifecycleEffects {
    fn reject_next(&self) {
        *self.reject.borrow_mut() = true;
    }

    fn reject_provenance_in(&self, attempts: usize) {
        let current = *self.provenance_attempts.borrow();
        *self.reject_provenance_at.borrow_mut() = Some(current + attempts);
    }

    fn sabotage_next_create(&self, path: PathBuf) {
        *self.sabotage_create.borrow_mut() = Some(path);
    }

    fn events(&self) -> Vec<Event> {
        self.events.borrow().clone()
    }
}

impl EditorEffects for RejectingLifecycleEffects {
    fn record_provenance(
        &mut self,
        transaction: &EditorTransaction,
    ) -> Result<(), EditorEffectError> {
        let attempt = {
            let mut attempts = self.provenance_attempts.borrow_mut();
            *attempts += 1;
            *attempts
        };
        if *self.reject_provenance_at.borrow() == Some(attempt) {
            return Err(EditorEffectError::new("injected provenance failure"));
        }
        self.events
            .borrow_mut()
            .push(Event::FileEdited(transaction.clone()));
        Ok(())
    }
}

impl WorkspaceEffects for RejectingLifecycleEffects {
    fn record_workspace_event(&mut self, event: Event) -> Result<(), WorkspaceEffectError> {
        self.events.borrow_mut().push(event);
        Ok(())
    }

    fn record_lifecycle(&mut self, event: Event) -> Result<(), WorkspaceEffectError> {
        if std::mem::take(&mut *self.reject.borrow_mut()) {
            return Err(WorkspaceEffectError::new("injected lifecycle failure"));
        }
        self.events.borrow_mut().push(event);
        if let Some(path) = self.sabotage_create.borrow_mut().take() {
            fs::write(path, b"interloper").unwrap();
        }
        Ok(())
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rustrace-workspace-session-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn path(value: &str) -> WorkspacePath {
    WorkspacePath::new(value).unwrap()
}

fn manifest(allowed_paths: &[&str]) -> AssignmentManifest {
    let allowed_paths = allowed_paths
        .iter()
        .map(|path| format!("\"{path}\""))
        .collect::<Vec<_>>()
        .join(", ");
    AssignmentManifest::parse(
        format!(
            r#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Workspace management"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = [{allowed_paths}]

[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#
        )
        .as_bytes(),
    )
    .unwrap()
}

fn workspace_fixture() -> TempDir {
    let temp = TempDir::new();
    fs::create_dir(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("Cargo.lock"), "version = 4\n").unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\n[workspace]\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn answer() -> u8 { 42 }\n",
    )
    .unwrap();
    temp
}

fn write_padding_files(root: &Path, mut byte_count: usize) {
    let per_file = usize::try_from(MAX_WORKSPACE_FILE_BYTES).unwrap();
    let mut index = 0;
    while byte_count > 0 {
        let chunk = byte_count.min(per_file);
        fs::write(root.join(format!("padding-{index}.bin")), vec![b'p'; chunk]).unwrap();
        byte_count -= chunk;
        index += 1;
    }
}

fn open_workspace(
    root: &Path,
    active: &str,
) -> (WorkspaceSession<RecordingEffects>, RecordingEffects) {
    let effects = RecordingEffects::default();
    let workspace = WorkspaceSession::open(
        root,
        &manifest(&["Cargo.lock", "Cargo.toml", "src/**/*.rs"]),
        Some(path(active)),
        effects.clone(),
    )
    .unwrap();
    (workspace, effects)
}

fn diagnostic_span(
    byte_start: u64,
    byte_end: u64,
    line_start: u64,
    column_start: u64,
    line_end: u64,
    column_end: u64,
) -> DiagnosticSpan {
    DiagnosticSpan {
        file_name: "main.rs".to_owned(),
        byte_start,
        byte_end,
        line_start,
        line_end,
        column_start,
        column_end,
        is_primary: true,
        label: None,
        suggested_replacement: None,
        suggestion_applicability: None,
    }
}

#[test]
fn diagnostic_coordinates_must_exactly_match_utf8_bytes_lines_columns_and_crlf() {
    let temp = TempDir::new();
    let source = "aé🦀\r\nβ\n";
    fs::write(temp.path().join("main.rs"), source).unwrap();
    let workspace = WorkspaceSession::open(
        temp.path(),
        &manifest(&["*.rs"]),
        Some(path("main.rs")),
        RecordingEffects::default(),
    )
    .unwrap();
    let main = path("main.rs");

    for span in [
        diagnostic_span(0, 1, 1, 1, 1, 2),
        diagnostic_span(1, 7, 1, 2, 1, 4),
        diagnostic_span(7, 7, 1, 4, 1, 4),
        diagnostic_span(7, 9, 1, 4, 2, 1),
        diagnostic_span(9, 11, 2, 1, 2, 2),
        diagnostic_span(11, 11, 2, 2, 2, 2),
        diagnostic_span(source.len() as u64, source.len() as u64, 3, 1, 3, 1),
    ] {
        assert!(
            workspace.diagnostic_span_is_valid(&main, &span),
            "valid span rejected: {span:?}"
        );
    }

    for span in [
        // Both columns are in range, but they contradict bytes 0..1.
        diagnostic_span(0, 1, 1, 3, 1, 4),
        // Byte 2 is inside the multibyte encoding of é.
        diagnostic_span(2, 3, 1, 2, 1, 3),
        // Byte 8 is the non-source boundary between CR and LF.
        diagnostic_span(8, 8, 1, 4, 1, 4),
        diagnostic_span(9, 11, 1, 1, 1, 2),
    ] {
        assert!(
            !workspace.diagnostic_span_is_valid(&main, &span),
            "invalid span accepted: {span:?}"
        );
    }
}

#[test]
fn actual_diagnostic_navigation_renders_far_right_selection_at_80_by_24() {
    let area = Rect::new(0, 0, 80, 24);
    let MainLayout::Full(panes) = main_layout(area) else {
        unreachable!();
    };
    let editor_inner_width = usize::from(panes.editor.width);
    let source = "x".repeat(editor_inner_width.saturating_sub(1));
    let temp = TempDir::new();
    fs::write(temp.path().join("main.rs"), &source).unwrap();
    let mut workspace = WorkspaceSession::open(
        temp.path(),
        &manifest(&["*.rs"]),
        Some(path("main.rs")),
        RecordingEffects::default(),
    )
    .unwrap();
    let source_len = u64::try_from(source.len()).unwrap();
    let span = diagnostic_span(source_len - 1, source_len, 1, source_len, 1, source_len + 1);

    workspace
        .navigate_to_diagnostic_span(&path("main.rs"), &span)
        .unwrap();
    workspace.follow_cursor(editor_inner_width, usize::from(panes.editor.height));
    let state = MainViewState::new(
        "diagnostic navigation",
        vec![BufferTabViewEntry::new("main.rs", true, false)],
        vec!["diagnostics".to_owned()],
        RecordingState::Active,
        JournalHealth::Healthy,
        "saved",
    )
    .with_diagnostic_markers(vec![DiagnosticLineMarker::new(
        0,
        DiagnosticMarkerKind::Error,
        true,
    )]);
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            frame.render_widget(
                MainView::new(
                    &state,
                    workspace.active_buffer(),
                    workspace.active_viewport(),
                    &[],
                )
                .with_palette(Palette::terminal()),
                frame.area(),
            );
        })
        .unwrap();

    let row = panes.editor.y;
    let visible = (panes.editor.x..panes.editor.right())
        .filter_map(|x| terminal.backend().buffer().cell((x, row)))
        .collect::<Vec<_>>();
    assert!(
        visible
            .iter()
            .any(|cell| cell.modifier.contains(Modifier::REVERSED)),
        "navigated caret was not rendered"
    );
    assert!(
        visible.iter().any(|cell| cell.bg == Color::DarkGray),
        "navigated selection was not rendered"
    );
}

#[test]
fn production_cursor_follow_uses_the_full_width_for_a_short_document() {
    let area = Rect::new(0, 0, 80, 24);
    let MainLayout::Full(panes) = main_layout(area) else {
        unreachable!();
    };
    let source = "x".repeat(usize::from(panes.editor.width).saturating_sub(1));
    let temp = TempDir::new();
    fs::write(temp.path().join("main.rs"), source).unwrap();
    let mut workspace = WorkspaceSession::open(
        temp.path(),
        &manifest(&["*.rs"]),
        Some(path("main.rs")),
        RecordingEffects::default(),
    )
    .unwrap();

    workspace
        .execute_editor(EditorCommand::Move {
            movement: Movement::LineEnd,
            selecting: false,
        })
        .unwrap();
    workspace.follow_cursor_in_editor_area(panes.editor);
    assert_eq!(
        workspace.active_viewport().left_column(),
        0,
        "a short document scrolled before the caret crossed the true right edge"
    );

    workspace
        .execute_editor(EditorCommand::Insert('x'))
        .unwrap();
    workspace.follow_cursor_in_editor_area(panes.editor);
    assert_eq!(workspace.active_viewport().left_column(), 1);
}

fn snapshot(workspace: &WorkspaceSession<RecordingEffects>) -> CheckpointSnapshot {
    let files = workspace
        .disk_files()
        .unwrap()
        .into_iter()
        .map(|(path, contents)| CheckpointFile { path, contents })
        .collect();
    let documents = workspace
        .file_tree()
        .iter()
        .filter_map(|entry| {
            entry.document_id().map(|document_id| OpenDocument {
                document_id: document_id.clone(),
                path: entry.path().clone(),
                selection: Default::default(),
                version: 0,
            })
        })
        .collect();
    CheckpointSnapshot::new(
        SessionId::new("workspace-session").unwrap(),
        1,
        files,
        Some(workspace.active_document_id().clone()),
        documents,
    )
    .unwrap()
}

fn envelope(
    session_id: &SessionId,
    sequence: u64,
    previous_event_hash: Hash,
    event: Event,
) -> EventEnvelope {
    let mut envelope = EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: session_id.clone(),
        sequence,
        monotonic_millis: sequence,
        wall_clock_utc: None,
        previous_event_hash,
        event_hash: Hash::zero(),
        event,
    };
    envelope.event_hash = compute_event_hash(previous_event_hash, &envelope).unwrap();
    envelope
}

#[test]
fn tree_navigation_skips_the_hidden_lockfile_but_active_file_changes_are_recorded() {
    let temp = workspace_fixture();
    let (mut workspace, effects) = open_workspace(temp.path(), "src/lib.rs");

    assert_eq!(
        workspace
            .file_tree()
            .iter()
            .map(|entry| (entry.path().as_str(), entry.is_editable()))
            .collect::<Vec<_>>(),
        [
            ("Cargo.lock", false),
            ("Cargo.toml", true),
            ("src/lib.rs", true),
        ]
    );
    assert_eq!(workspace.selected_path(), &path("src/lib.rs"));
    assert_eq!(
        workspace.select_next(),
        WorkspaceOutcome::TreeSelectionChanged
    );
    assert_eq!(workspace.selected_path(), &path("Cargo.toml"));
    assert_eq!(
        workspace.activate_selected().unwrap(),
        WorkspaceOutcome::FileActivated
    );
    assert_eq!(workspace.active_path(), &path("Cargo.toml"));
    assert_eq!(
        workspace.select_previous(),
        WorkspaceOutcome::TreeSelectionChanged
    );
    assert_eq!(workspace.selected_path(), &path("src/lib.rs"));
    assert!(matches!(
        effects.events().as_slice(),
        [Event::FileFocused(FileFocused { document_id })]
            if document_id == workspace.active_document_id()
    ));
}

#[test]
fn dirty_markers_follow_each_buffer_and_switching_emits_focus_events() {
    let temp = workspace_fixture();
    let (mut workspace, effects) = open_workspace(temp.path(), "src/lib.rs");

    workspace
        .execute_editor(EditorCommand::Move {
            movement: Movement::DocumentEnd,
            selecting: false,
        })
        .unwrap();
    assert_eq!(
        workspace
            .execute_editor(EditorCommand::Insert('!'))
            .unwrap(),
        EditorOutcome::Edited
    );
    workspace.select_path(&path("Cargo.toml")).unwrap();
    workspace.activate_selected().unwrap();
    workspace
        .execute_editor(EditorCommand::Move {
            movement: Movement::DocumentEnd,
            selecting: false,
        })
        .unwrap();
    workspace
        .execute_editor(EditorCommand::Insert('#'))
        .unwrap();

    let dirty = workspace
        .file_tree()
        .iter()
        .filter(|entry| entry.is_dirty())
        .map(|entry| entry.path().as_str())
        .collect::<Vec<_>>();
    assert_eq!(dirty, ["Cargo.toml", "src/lib.rs"]);
    assert_eq!(
        effects
            .events()
            .iter()
            .filter(|event| matches!(event, Event::FileEdited(_)))
            .count(),
        2
    );
    let event_count = effects.events().len();

    workspace
        .execute_editor(EditorCommand::PreviousBuffer)
        .unwrap();
    workspace.execute_editor(EditorCommand::NextBuffer).unwrap();
    workspace.select_next();
    assert_eq!(effects.events().len(), event_count + 2);
    assert!(
        effects.events()[event_count..]
            .iter()
            .all(|event| matches!(event, Event::FileFocused(_)))
    );
}

#[test]
fn deleting_active_manifest_skips_the_controller_managed_lockfile() {
    let temp = workspace_fixture();
    let lock_before = fs::read(temp.path().join("Cargo.lock")).unwrap();
    let source_before = fs::read(temp.path().join("src/lib.rs")).unwrap();
    let (mut workspace, effects) = open_workspace(temp.path(), "Cargo.toml");
    let lock_document = workspace
        .file_tree()
        .iter()
        .find(|file| file.path() == &path("Cargo.lock"))
        .and_then(|file| file.document_id())
        .cloned()
        .unwrap();
    let source_document = workspace
        .file_tree()
        .iter()
        .find(|file| file.path() == &path("src/lib.rs"))
        .and_then(|file| file.document_id())
        .cloned()
        .unwrap();

    assert_eq!(
        workspace.request_delete_selected().unwrap(),
        WorkspaceOutcome::FileDeleted
    );
    assert_eq!(workspace.active_path(), &path("src/lib.rs"));
    assert_eq!(workspace.selected_path(), &path("src/lib.rs"));
    assert_eq!(workspace.active_document_id(), &source_document);
    assert_eq!(
        workspace.execute_editor(EditorCommand::NextBuffer).unwrap(),
        EditorOutcome::NoChange
    );
    assert_eq!(
        workspace
            .execute_editor(EditorCommand::PreviousBuffer)
            .unwrap(),
        EditorOutcome::NoChange
    );
    assert!(workspace.activate_path(&path("Cargo.lock")).is_err());
    assert_eq!(workspace.active_path(), &path("src/lib.rs"));

    workspace
        .execute_editor(EditorCommand::Move {
            movement: Movement::DocumentEnd,
            selecting: false,
        })
        .unwrap();
    assert_eq!(
        workspace
            .execute_editor(EditorCommand::Insert('!'))
            .unwrap(),
        EditorOutcome::Edited
    );
    workspace.save_active().unwrap();

    assert_eq!(
        fs::read(temp.path().join("Cargo.lock")).unwrap(),
        lock_before
    );
    assert_ne!(
        fs::read(temp.path().join("src/lib.rs")).unwrap(),
        source_before
    );
    assert!(effects.events().iter().all(|event| {
        !matches!(event, Event::FileFocused(focused) if focused.document_id == lock_document)
            && !matches!(event, Event::FileEdited(edit) if edit.document_id == lock_document)
    }));
    assert!(effects.events().iter().any(
        |event| matches!(event, Event::FileFocused(focused) if focused.document_id == source_document)
    ));
}

#[test]
fn controller_managed_lockfile_does_not_make_the_manifest_deletable() {
    let temp = TempDir::new();
    fs::write(temp.path().join("Cargo.lock"), "version = 4\n").unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\n[workspace]\n",
    )
    .unwrap();
    let effects = RecordingEffects::default();
    let mut workspace = WorkspaceSession::open(
        temp.path(),
        &manifest(&["Cargo.lock", "Cargo.toml"]),
        Some(path("Cargo.toml")),
        effects.clone(),
    )
    .unwrap();

    let error = workspace.request_delete_selected().unwrap_err();
    assert!(matches!(error, WorkspaceError::LastEditableFile));
    assert_eq!(workspace.active_path(), &path("Cargo.toml"));
    assert_eq!(workspace.selected_path(), &path("Cargo.toml"));
    assert!(temp.path().join("Cargo.toml").exists());
    assert_eq!(
        workspace.execute_editor(EditorCommand::NextBuffer).unwrap(),
        EditorOutcome::NoChange
    );
    assert_eq!(workspace.active_path(), &path("Cargo.toml"));
    assert!(effects.events().is_empty());
}

#[test]
fn recovery_replaces_a_controller_managed_active_document_with_an_editable_one() {
    let temp = workspace_fixture();
    let (workspace, _) = open_workspace(temp.path(), "src/lib.rs");
    let baseline = workspace.disk_files().unwrap();
    let files = baseline
        .iter()
        .map(|(path, contents)| CheckpointFile {
            path: path.clone(),
            contents: contents.clone(),
        })
        .collect();
    let documents = workspace
        .file_tree()
        .iter()
        .filter_map(|file| {
            file.document_id().map(|document_id| OpenDocument {
                document_id: document_id.clone(),
                path: file.path().clone(),
                selection: Default::default(),
                version: 0,
            })
        })
        .collect::<Vec<_>>();
    let lock_document = documents
        .iter()
        .find(|document| document.path == path("Cargo.lock"))
        .map(|document| document.document_id.clone())
        .unwrap();
    let snapshot = CheckpointSnapshot::new(
        SessionId::new("workspace-session").unwrap(),
        1,
        files,
        Some(lock_document),
        documents,
    )
    .unwrap();
    let effects = RecordingEffects::default();

    let recovered = WorkspaceSession::from_recovered(
        temp.path(),
        &manifest(&["Cargo.lock", "Cargo.toml", "src/**/*.rs"]),
        &snapshot,
        &baseline,
        effects.clone(),
    )
    .unwrap();

    assert_eq!(recovered.active_path(), &path("Cargo.toml"));
    assert!(
        recovered
            .file_tree()
            .iter()
            .find(|file| file.path() == &path("Cargo.lock"))
            .is_some_and(|file| !file.is_editable())
    );
    assert!(effects.events().is_empty());
}

#[test]
fn indexed_mouse_targets_share_file_and_tab_activation_authority() {
    let temp = workspace_fixture();
    let (mut clicked, clicked_effects) = open_workspace(temp.path(), "src/lib.rs");
    let (mut keyboard, keyboard_effects) = open_workspace(temp.path(), "src/lib.rs");

    assert_eq!(
        clicked.select_index(1).unwrap(),
        WorkspaceOutcome::TreeSelectionChanged
    );
    assert_eq!(
        clicked.activate_selected().unwrap(),
        WorkspaceOutcome::FileActivated
    );
    assert_eq!(
        keyboard.select_next(),
        WorkspaceOutcome::TreeSelectionChanged
    );
    assert_eq!(keyboard.selected_path(), &path("Cargo.toml"));
    assert_eq!(
        keyboard.activate_selected().unwrap(),
        WorkspaceOutcome::FileActivated
    );
    assert_eq!(clicked.active_path(), keyboard.active_path());
    assert_eq!(clicked_effects.events(), keyboard_effects.events());

    assert_eq!(
        clicked.activate_path(&path("src/lib.rs")).unwrap(),
        WorkspaceOutcome::FileActivated
    );
    keyboard.select_path(&path("src/lib.rs")).unwrap();
    assert_eq!(
        keyboard.activate_selected().unwrap(),
        WorkspaceOutcome::FileActivated
    );
    assert_eq!(clicked.active_path(), keyboard.active_path());
    assert_eq!(clicked_effects.events(), keyboard_effects.events());

    assert!(clicked.select_index(usize::MAX).is_err());
    assert!(clicked.activate_path(&path("missing.rs")).is_err());
}

fn tab_click<E>(workspace: &mut WorkspaceSession<E>, displayed_index: usize) -> WorkspaceOutcome
where
    E: WorkspaceEffects + Clone,
{
    let active_path = workspace.active_path().clone();
    let buffers = workspace
        .file_tree()
        .iter()
        .filter(|file| file.is_editable())
        .map(|file| {
            BufferTabViewEntry::new(
                file.path().as_str(),
                file.path() == &active_path,
                file.is_dirty(),
            )
        })
        .collect();
    let state = MainViewState::new(
        "tab identity",
        buffers,
        Vec::new(),
        RecordingState::Active,
        JournalHealth::Healthy,
        "",
    );
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = rustrace::tui::shell::HitMap::default();
    terminal
        .draw(|frame| {
            hits = MainView::new(&state, workspace.active_buffer(), &Viewport::default(), &[])
                .render_with_hit_map(frame.area(), frame.buffer_mut());
        })
        .unwrap();
    let (rect, _) = hits.tab_pills[displayed_index];
    let input = mouse_input_for_event(
        &MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        },
        &hits,
        &ShellState::default(),
        &Default::default(),
    );
    let Some(ShellInput::ActivateTab(path)) = input else {
        panic!("displayed tab did not map to tab activation: {input:?}");
    };
    workspace
        .activate_path(&WorkspacePath::new(path).unwrap())
        .unwrap()
}

fn editable_fixture(paths: &[&str]) -> TempDir {
    let temp = TempDir::new();
    for path in paths {
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(temp.path().join(parent)).unwrap();
        }
        fs::write(temp.path().join(path), format!("// {path}\n")).unwrap();
    }
    temp
}

#[test]
fn mouse_tab_identity_survives_create_before_existing_buffer_order() {
    let clicked_root = editable_fixture(&["src/b.rs"]);
    let keyboard_root = editable_fixture(&["src/b.rs"]);
    let (mut clicked, clicked_effects) = open_workspace(clicked_root.path(), "src/b.rs");
    let (mut keyboard, keyboard_effects) = open_workspace(keyboard_root.path(), "src/b.rs");
    clicked.create_file("src/a.rs").unwrap();
    keyboard.create_file("src/a.rs").unwrap();

    assert_eq!(tab_click(&mut clicked, 0), WorkspaceOutcome::NoChange);
    keyboard.select_path(&path("src/a.rs")).unwrap();
    assert_eq!(
        keyboard.activate_selected().unwrap(),
        WorkspaceOutcome::NoChange
    );
    assert_eq!(clicked.active_path(), keyboard.active_path());
    assert_eq!(clicked.active_document_id(), keyboard.active_document_id());
    assert_eq!(clicked_effects.events(), keyboard_effects.events());
}

#[test]
fn mouse_tab_identity_survives_rename_across_sort_order() {
    let clicked_root = editable_fixture(&["src/a.rs", "src/b.rs"]);
    let keyboard_root = editable_fixture(&["src/a.rs", "src/b.rs"]);
    let (mut clicked, clicked_effects) = open_workspace(clicked_root.path(), "src/b.rs");
    let (mut keyboard, keyboard_effects) = open_workspace(keyboard_root.path(), "src/b.rs");
    clicked.select_path(&path("src/b.rs")).unwrap();
    keyboard.select_path(&path("src/b.rs")).unwrap();
    clicked.rename_selected("src/0.rs").unwrap();
    keyboard.rename_selected("src/0.rs").unwrap();

    assert_eq!(tab_click(&mut clicked, 0), WorkspaceOutcome::NoChange);
    keyboard.select_path(&path("src/0.rs")).unwrap();
    assert_eq!(
        keyboard.activate_selected().unwrap(),
        WorkspaceOutcome::NoChange
    );
    assert_eq!(clicked.active_path(), keyboard.active_path());
    assert_eq!(clicked.active_document_id(), keyboard.active_document_id());
    assert_eq!(clicked_effects.events(), keyboard_effects.events());
}

fn confirmation_inputs<E>(workspace: &WorkspaceSession<E>) -> (WorkspaceInput, WorkspaceInput)
where
    E: WorkspaceEffects + Clone,
{
    let active = workspace.active_path();
    let state = MainViewState::new(
        "confirmation parity",
        workspace
            .file_tree()
            .iter()
            .filter(|file| file.document_id().is_some())
            .map(|file| {
                BufferTabViewEntry::new(
                    file.path().as_str(),
                    file.path() == active,
                    file.is_dirty(),
                )
            })
            .collect(),
        Vec::new(),
        RecordingState::Active,
        JournalHealth::Healthy,
        "",
    )
    .with_confirmation(rustrace::tui::ConfirmationState::new("Confirm?"));
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = rustrace::tui::shell::HitMap::default();
    terminal
        .draw(|frame| {
            hits = MainView::new(&state, workspace.active_buffer(), &Viewport::default(), &[])
                .render_with_hit_map(frame.area(), frame.buffer_mut());
        })
        .unwrap();
    let mouse = mouse_input_for_event(
        &MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hits.overlay_confirm.x,
            row: hits.overlay_confirm.y,
            modifiers: KeyModifiers::NONE,
        },
        &hits,
        &ShellState {
            modal: rustrace::tui::ShellModal::Confirmation,
        },
        &Default::default(),
    );
    let Some(ShellInput::Workspace(mouse)) = mouse else {
        panic!("confirmation pill did not map to workspace input: {mouse:?}");
    };
    let keyboard = workspace_input_for_event(
        TerminalEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        10,
        WorkspaceFocus::Editor,
        true,
    )
    .expect("confirmation Enter did not map");
    (mouse, keyboard)
}

#[test]
fn mouse_delete_confirmation_emits_the_keyboard_events() {
    let clicked_root = editable_fixture(&["src/a.rs", "src/b.rs"]);
    let keyboard_root = editable_fixture(&["src/a.rs", "src/b.rs"]);
    let (mut clicked, clicked_effects) = open_workspace(clicked_root.path(), "src/b.rs");
    let (mut keyboard, keyboard_effects) = open_workspace(keyboard_root.path(), "src/b.rs");
    clicked.execute_editor(EditorCommand::Insert('!')).unwrap();
    keyboard.execute_editor(EditorCommand::Insert('!')).unwrap();
    assert_eq!(
        clicked.request_delete_selected().unwrap(),
        WorkspaceOutcome::ConfirmationRequired
    );
    assert_eq!(
        keyboard.request_delete_selected().unwrap(),
        WorkspaceOutcome::ConfirmationRequired
    );
    let (mouse, key) = confirmation_inputs(&clicked);
    assert_eq!(mouse, key);
    assert_eq!(mouse, WorkspaceInput::ConfirmDestructive);

    assert_eq!(
        clicked.confirm_delete().unwrap(),
        WorkspaceOutcome::FileDeleted
    );
    assert_eq!(
        keyboard.confirm_delete().unwrap(),
        WorkspaceOutcome::FileDeleted
    );
    assert_eq!(clicked_effects.events(), keyboard_effects.events());
}

#[test]
fn mouse_discard_confirmation_emits_the_keyboard_events() {
    let clicked_root = editable_fixture(&["src/a.rs", "src/b.rs"]);
    let keyboard_root = editable_fixture(&["src/a.rs", "src/b.rs"]);
    let (mut clicked, clicked_effects) = open_workspace(clicked_root.path(), "src/b.rs");
    let (mut keyboard, keyboard_effects) = open_workspace(keyboard_root.path(), "src/b.rs");
    for workspace in [&mut clicked, &mut keyboard] {
        workspace
            .execute_editor(EditorCommand::Insert('!'))
            .unwrap();
        assert!(matches!(
            workspace
                .execute_editor(EditorCommand::RequestQuit)
                .unwrap(),
            EditorOutcome::ConfirmationRequired(_)
        ));
    }
    let (mouse, key) = confirmation_inputs(&clicked);
    assert_eq!(mouse, key);
    assert_eq!(mouse, WorkspaceInput::ConfirmDestructive);

    assert_eq!(
        clicked
            .execute_editor(EditorCommand::ConfirmDiscard)
            .unwrap(),
        EditorOutcome::Quit
    );
    assert_eq!(
        keyboard
            .execute_editor(EditorCommand::ConfirmDiscard)
            .unwrap(),
        EditorOutcome::Quit
    );
    assert_eq!(clicked_effects.events(), keyboard_effects.events());
}

#[test]
fn mouse_diagnostic_row_emits_the_keyboard_navigation_events() {
    let clicked_root = editable_fixture(&["src/main.rs"]);
    let keyboard_root = editable_fixture(&["src/main.rs"]);
    let (mut clicked, clicked_effects) = open_workspace(clicked_root.path(), "src/main.rs");
    let (mut keyboard, keyboard_effects) = open_workspace(keyboard_root.path(), "src/main.rs");
    let state = MainViewState::new(
        "diagnostic parity",
        vec![BufferTabViewEntry::new("src/main.rs", true, false)],
        Vec::new(),
        RecordingState::Active,
        JournalHealth::Healthy,
        "",
    )
    .with_output_rows(vec![OutputRow::diagnostic("error", 0)]);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = rustrace::tui::shell::HitMap::default();
    terminal
        .draw(|frame| {
            hits = MainView::new(&state, clicked.active_buffer(), &Viewport::default(), &[])
                .render_with_hit_map(frame.area(), frame.buffer_mut());
        })
        .unwrap();
    let row = hits.output_rows[0].0;
    assert_eq!(
        mouse_input_for_event(
            &MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: row.x,
                row: row.y,
                modifiers: KeyModifiers::NONE,
            },
            &hits,
            &ShellState::default(),
            &Default::default(),
        ),
        Some(ShellInput::SelectDiagnostic(0))
    );
    let keyboard_event = TerminalEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT));
    assert_eq!(diagnostic_delta_for_event(&keyboard_event), Some(1));
    let span = DiagnosticSpan {
        file_name: "src/main.rs".to_owned(),
        byte_start: 0,
        byte_end: 2,
        line_start: 1,
        line_end: 1,
        column_start: 1,
        column_end: 3,
        is_primary: true,
        label: None,
        suggested_replacement: None,
        suggestion_applicability: None,
    };
    clicked
        .navigate_to_diagnostic_span(&path("src/main.rs"), &span)
        .unwrap();
    keyboard
        .navigate_to_diagnostic_span(&path("src/main.rs"), &span)
        .unwrap();
    assert_eq!(clicked_effects.events(), keyboard_effects.events());
    assert_eq!(
        clicked.active_buffer().selection_state(),
        keyboard.active_buffer().selection_state()
    );
}

#[test]
fn mouse_wheel_draws_once_only_when_the_viewport_changes() {
    let temp = editable_fixture(&["src/main.rs"]);
    fs::write(
        temp.path().join("src/main.rs"),
        (0..40)
            .map(|line| format!("line {line:02}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let (mut workspace, effects) = open_workspace(temp.path(), "src/main.rs");
    let state = MainViewState::new(
        "wheel redraw",
        vec![BufferTabViewEntry::new("src/main.rs", true, false)],
        Vec::new(),
        RecordingState::Active,
        JournalHealth::Healthy,
        "",
    );
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = rustrace::tui::shell::HitMap::default();
    terminal
        .draw(|frame| {
            hits = MainView::new(
                &state,
                workspace.active_buffer(),
                workspace.active_viewport(),
                &[],
            )
            .render_with_hit_map(frame.area(), frame.buffer_mut());
        })
        .unwrap();
    let cell = (hits.editor.rect.x, hits.editor.rect.y);
    let caret = workspace.active_buffer().selection_state();
    let provenance = effects.events();
    let editor_height = usize::from(hits.editor.rect.height);
    let mut reducer = MouseState::default();
    let mut draws = DrawGate::default();

    let wheel_down = MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: cell.0,
        row: cell.1,
        modifiers: KeyModifiers::NONE,
    };
    reducer.reduce(&wheel_down, 1, Default::default());
    let Some(ShellInput::ScrollEditor(delta)) =
        mouse_input_for_event(&wheel_down, &hits, &ShellState::default(), &reducer)
    else {
        panic!("editor wheel-down did not map to scrolling")
    };
    let changed = workspace.scroll_active_viewport(delta, editor_height);
    draws.request_change(changed);
    assert_eq!(workspace.active_viewport().top_line(), 3);
    assert_eq!(workspace.active_buffer().selection_state(), caret);
    assert!(draws.take_draw(), "effective wheel did not request a draw");
    assert!(
        !draws.take_draw(),
        "effective wheel requested more than one draw"
    );

    assert!(workspace.scroll_active_viewport(-3, editor_height));
    let wheel_up = MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: cell.0,
        row: cell.1,
        modifiers: KeyModifiers::NONE,
    };
    reducer.reduce(&wheel_up, 2, Default::default());
    let Some(ShellInput::ScrollEditor(delta)) =
        mouse_input_for_event(&wheel_up, &hits, &ShellState::default(), &reducer)
    else {
        panic!("editor wheel-up did not map to scrolling")
    };
    let changed = workspace.scroll_active_viewport(delta, editor_height);
    draws.request_change(changed);
    assert!(!changed, "wheel at the top boundary changed the viewport");
    assert!(!draws.take_draw(), "no-op wheel requested a draw");
    assert_eq!(workspace.active_buffer().selection_state(), caret);
    assert_eq!(effects.events(), provenance);
}

#[test]
fn lifecycle_events_replay_to_exact_live_filesystem_and_hash() {
    let temp = workspace_fixture();
    let (mut workspace, effects) = open_workspace(temp.path(), "src/lib.rs");
    let initial = snapshot(&workspace);
    let initial_owner = envelope(
        initial.session_id(),
        1,
        Hash::zero(),
        Event::WorkspaceCheckpoint(initial.event_payload()),
    );
    let mut replay = ReplayEngine::from_initial_checkpoint(StoredCheckpoint {
        owning_event: initial_owner,
        snapshot: initial,
    })
    .unwrap();

    workspace
        .execute_editor(EditorCommand::Insert('!'))
        .unwrap();
    workspace.save_active().unwrap();

    workspace.create_file("src/new.rs").unwrap();
    workspace
        .execute_editor(EditorCommand::Insert('x'))
        .unwrap();
    workspace.save_active().unwrap();
    workspace.rename_selected("src/renamed.rs").unwrap();
    assert_eq!(
        workspace.request_delete_selected().unwrap(),
        WorkspaceOutcome::FileDeleted
    );

    let events = effects.events();
    assert_eq!(
        events
            .iter()
            .map(|event| match event {
                Event::FileCreated(_) => "created",
                Event::FileDeleted(_) => "deleted",
                Event::FileRenamed(_) => "renamed",
                Event::FileEdited(_) => "edited",
                Event::FileFocused(_) => "focused",
                _ => "unexpected",
            })
            .collect::<Vec<_>>(),
        [
            "edited", "created", "focused", "edited", "renamed", "deleted", "focused"
        ]
    );

    if let Event::FileCreated(created) = &events[1] {
        assert_eq!(created.path, path("src/new.rs"));
        assert_eq!(created.contents, "");
        assert_eq!(created.content_hash, document_hash(""));
    } else {
        panic!("expected FileCreated");
    }
    if let Event::FileRenamed(renamed) = &events[4] {
        assert_eq!(renamed.old_path, path("src/new.rs"));
        assert_eq!(renamed.new_path, path("src/renamed.rs"));
    } else {
        panic!("expected FileRenamed");
    }
    if let Event::FileDeleted(deleted) = &events[5] {
        assert_eq!(deleted.path, path("src/renamed.rs"));
        assert_eq!(deleted.previous_hash, document_hash("x"));
    } else {
        panic!("expected FileDeleted");
    }

    let mut previous = replay.last_event_hash();
    for (index, event) in events.into_iter().enumerate() {
        let event = envelope(replay.session_id(), index as u64 + 2, previous, event);
        replay.apply(&event).unwrap();
        previous = event.event_hash;
    }

    let live = read_workspace(temp.path()).unwrap();
    assert_eq!(replay.workspace_state().files(), &live);
    assert_eq!(
        replay.current_workspace_hash(),
        hash_workspace(temp.path()).unwrap()
    );
}

#[test]
fn selection_changes_before_edits_replay_through_production_contracts() {
    let temp = workspace_fixture();
    let (mut workspace, effects) = open_workspace(temp.path(), "src/lib.rs");
    let initial = snapshot(&workspace);
    let initial_owner = envelope(
        initial.session_id(),
        1,
        Hash::zero(),
        Event::WorkspaceCheckpoint(initial.event_payload()),
    );
    let mut replay = ReplayEngine::from_initial_checkpoint(StoredCheckpoint {
        owning_event: initial_owner,
        snapshot: initial,
    })
    .unwrap();

    assert_eq!(
        workspace
            .execute_editor(EditorCommand::Move {
                movement: Movement::DocumentEnd,
                selecting: false,
            })
            .unwrap(),
        EditorOutcome::SelectionChanged
    );
    assert_eq!(
        workspace
            .execute_editor(EditorCommand::Insert('!'))
            .unwrap(),
        EditorOutcome::Edited
    );
    workspace.save_active().unwrap();

    let events = effects.events();
    assert!(matches!(
        events.as_slice(),
        [Event::SelectionChanged(_), Event::FileEdited(_)]
    ));

    let mut previous = replay.last_event_hash();
    for (index, event) in events.into_iter().enumerate() {
        let event = envelope(replay.session_id(), index as u64 + 2, previous, event);
        replay.apply(&event).unwrap();
        previous = event.event_hash;
    }

    let live = read_workspace(temp.path()).unwrap();
    assert_eq!(replay.workspace_state().files(), &live);
    assert_eq!(
        replay.current_workspace_hash(),
        hash_workspace(temp.path()).unwrap()
    );
}

#[test]
fn aggregate_limit_rejects_an_edit_before_buffer_or_provenance_mutation() {
    let temp = TempDir::new();
    fs::create_dir(temp.path().join("src")).unwrap();
    let full = vec![b'x'; MAX_WORKSPACE_FILE_BYTES as usize];
    for index in 0..9 {
        fs::write(temp.path().join(format!("src/{index}.rs")), &full).unwrap();
    }
    let half = vec![b'y'; MAX_WORKSPACE_FILE_BYTES as usize / 2];
    fs::write(temp.path().join("src/9.rs"), &half).unwrap();
    fs::write(temp.path().join("src/active.rs"), &half).unwrap();
    let effects = RecordingEffects::default();
    let mut workspace = WorkspaceSession::open(
        temp.path(),
        &manifest(&["src/**/*.rs"]),
        Some(path("src/active.rs")),
        effects.clone(),
    )
    .unwrap();
    let before_text = workspace.active_buffer().text();

    assert!(
        workspace
            .execute_editor(EditorCommand::Insert('!'))
            .is_err()
    );
    assert_eq!(workspace.active_buffer().text(), before_text);
    assert!(effects.events().is_empty());
    assert_eq!(
        workspace.logical_files().unwrap(),
        read_workspace(temp.path()).unwrap()
    );
}

#[test]
fn aggregate_limit_counts_allowed_read_only_files_before_editing() {
    let temp = TempDir::new();
    fs::create_dir(temp.path().join("data")).unwrap();
    fs::create_dir(temp.path().join("src")).unwrap();
    let full = vec![b'x'; MAX_WORKSPACE_FILE_BYTES as usize];
    for index in 0..9 {
        fs::write(temp.path().join(format!("data/{index}.lock")), &full).unwrap();
    }
    let half = vec![b'y'; MAX_WORKSPACE_FILE_BYTES as usize / 2];
    fs::write(temp.path().join("data/9.lock"), &half).unwrap();
    fs::write(temp.path().join("src/active.rs"), &half).unwrap();
    let effects = RecordingEffects::default();
    let mut workspace = WorkspaceSession::open(
        temp.path(),
        &manifest(&["data/*.lock", "src/**/*.rs"]),
        Some(path("src/active.rs")),
        effects.clone(),
    )
    .unwrap();
    let before_text = workspace.active_buffer().text();

    assert!(
        workspace
            .execute_editor(EditorCommand::Insert('!'))
            .is_err()
    );
    assert_eq!(workspace.active_buffer().text(), before_text);
    assert!(effects.events().is_empty());
}

#[test]
fn editor_close_command_cannot_bypass_workspace_lifecycle_management() {
    let temp = workspace_fixture();
    let (mut workspace, effects) = open_workspace(temp.path(), "src/lib.rs");
    let before_tree = workspace.file_tree().to_vec();

    assert!(
        workspace
            .execute_editor(EditorCommand::CloseActive)
            .is_err()
    );
    assert_eq!(workspace.file_tree(), before_tree);
    assert!(effects.events().is_empty());
}

#[test]
fn external_changes_and_missing_known_files_fail_closed() {
    let temp = workspace_fixture();
    let (mut workspace, effects) = open_workspace(temp.path(), "src/lib.rs");
    let before_tree = workspace.file_tree().to_vec();

    fs::write(temp.path().join("Cargo.lock"), "externally changed\n").unwrap();
    assert!(workspace.logical_files().is_err());
    fs::write(temp.path().join("Cargo.lock"), "version = 4\n").unwrap();
    fs::remove_file(temp.path().join("Cargo.lock")).unwrap();
    assert!(workspace.logical_files().is_err());
    fs::write(temp.path().join("Cargo.lock"), "version = 4\n").unwrap();

    fs::write(temp.path().join("src/lib.rs"), "external replacement\n").unwrap();
    assert!(workspace.rename_selected("src/renamed.rs").is_err());
    assert_eq!(workspace.file_tree(), before_tree);
    assert!(!temp.path().join("src/renamed.rs").exists());
    assert!(effects.events().is_empty());
}

#[test]
fn confirmed_dirty_quit_records_discard_and_matches_disk_after_replay() {
    let temp = workspace_fixture();
    let (mut workspace, effects) = open_workspace(temp.path(), "src/lib.rs");
    let initial = snapshot(&workspace);
    let initial_owner = envelope(
        initial.session_id(),
        1,
        Hash::zero(),
        Event::WorkspaceCheckpoint(initial.event_payload()),
    );
    let mut replay = ReplayEngine::from_initial_checkpoint(StoredCheckpoint {
        owning_event: initial_owner,
        snapshot: initial,
    })
    .unwrap();

    workspace
        .execute_editor(EditorCommand::Insert('!'))
        .unwrap();
    assert!(matches!(
        workspace
            .execute_editor(EditorCommand::RequestQuit)
            .unwrap(),
        EditorOutcome::ConfirmationRequired(_)
    ));
    assert_eq!(
        workspace
            .execute_editor(EditorCommand::ConfirmDiscard)
            .unwrap(),
        EditorOutcome::Quit
    );

    let events = effects.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::FileEdited(_)))
            .count(),
        2
    );
    assert!(matches!(
        events.last(),
        Some(Event::FileEdited(transaction)) if transaction.origin == EditOrigin::FileReload
    ));

    let mut previous = replay.last_event_hash();
    for (index, event) in events.into_iter().enumerate() {
        let event = envelope(replay.session_id(), index as u64 + 2, previous, event);
        replay.apply(&event).unwrap();
        previous = event.event_hash;
    }
    assert_eq!(
        replay.workspace_state().files(),
        &read_workspace(temp.path()).unwrap()
    );
    assert_eq!(
        replay.current_workspace_hash(),
        hash_workspace(temp.path()).unwrap()
    );
}

#[test]
fn confirmed_dirty_quit_restores_files_larger_than_one_edit_payload() {
    let temp = TempDir::new();
    fs::create_dir(temp.path().join("src")).unwrap();
    let baseline = "x".repeat(rustrace_model::MAX_INSERTED_TEXT_BYTES + 1);
    fs::write(temp.path().join("src/lib.rs"), &baseline).unwrap();
    let effects = RecordingEffects::default();
    let mut workspace = WorkspaceSession::open(
        temp.path(),
        &manifest(&["src/**/*.rs"]),
        Some(path("src/lib.rs")),
        effects.clone(),
    )
    .unwrap();
    let initial = snapshot(&workspace);
    let initial_owner = envelope(
        initial.session_id(),
        1,
        Hash::zero(),
        Event::WorkspaceCheckpoint(initial.event_payload()),
    );
    let mut replay = ReplayEngine::from_initial_checkpoint(StoredCheckpoint {
        owning_event: initial_owner,
        snapshot: initial,
    })
    .unwrap();

    workspace.execute_editor(EditorCommand::SelectAll).unwrap();
    workspace
        .execute_editor(EditorCommand::DeleteBackward)
        .unwrap();
    let replacement = "y".repeat(baseline.len());
    for chunk in replacement.as_bytes().chunks(128 * 1024) {
        workspace
            .execute_editor(EditorCommand::PasteExternal(
                std::str::from_utf8(chunk).unwrap().to_owned(),
            ))
            .unwrap();
    }
    workspace
        .execute_editor(EditorCommand::RequestQuit)
        .unwrap();
    assert_eq!(
        workspace
            .execute_editor(EditorCommand::ConfirmDiscard)
            .unwrap(),
        EditorOutcome::Quit
    );

    let mut previous = replay.last_event_hash();
    for (index, event) in effects.events().into_iter().enumerate() {
        let event = envelope(replay.session_id(), index as u64 + 2, previous, event);
        replay.apply(&event).unwrap();
        previous = event.event_hash;
    }
    assert_eq!(
        replay.workspace_state().files(),
        &read_workspace(temp.path()).unwrap()
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("src/lib.rs")).unwrap(),
        baseline
    );
}

#[test]
fn confirmed_dirty_quit_keeps_cross_buffer_replay_within_workspace_limit() {
    let temp = TempDir::new();
    let moved_bytes = usize::try_from(MAX_WORKSPACE_FILE_BYTES / 4).unwrap();
    fs::write(temp.path().join("a.rs"), vec![b'a'; moved_bytes]).unwrap();
    fs::write(temp.path().join("b.rs"), []).unwrap();
    write_padding_files(
        temp.path(),
        usize::try_from(MAX_WORKSPACE_TOTAL_BYTES).unwrap() - moved_bytes,
    );
    let effects = RecordingEffects::default();
    let mut workspace = WorkspaceSession::open(
        temp.path(),
        &manifest(&["*.rs", "*.bin"]),
        Some(path("a.rs")),
        effects.clone(),
    )
    .unwrap();
    let initial = snapshot(&workspace);
    let initial_owner = envelope(
        initial.session_id(),
        1,
        Hash::zero(),
        Event::WorkspaceCheckpoint(initial.event_payload()),
    );
    let mut replay = ReplayEngine::from_initial_checkpoint(StoredCheckpoint {
        owning_event: initial_owner,
        snapshot: initial,
    })
    .unwrap();

    workspace.execute_editor(EditorCommand::SelectAll).unwrap();
    workspace
        .execute_editor(EditorCommand::DeleteBackward)
        .unwrap();
    workspace.select_path(&path("b.rs")).unwrap();
    workspace.activate_selected().unwrap();
    workspace
        .execute_editor(EditorCommand::PasteExternal("b".repeat(moved_bytes)))
        .unwrap();
    workspace
        .execute_editor(EditorCommand::RequestQuit)
        .unwrap();
    assert_eq!(
        workspace
            .execute_editor(EditorCommand::ConfirmDiscard)
            .unwrap(),
        EditorOutcome::Quit
    );

    let mut previous = replay.last_event_hash();
    for (index, event) in effects.events().into_iter().enumerate() {
        let event = envelope(replay.session_id(), index as u64 + 2, previous, event);
        replay.apply(&event).unwrap();
        previous = event.event_hash;
    }
    assert_eq!(
        replay.workspace_state().files(),
        &read_workspace(temp.path()).unwrap()
    );
}

#[test]
fn confirmed_dirty_quit_ignores_stale_per_buffer_edit_headroom() {
    let temp = TempDir::new();
    let half_file = usize::try_from(MAX_WORKSPACE_FILE_BYTES / 2).unwrap();
    let quarter_file = half_file / 2;
    fs::write(temp.path().join("a.rs"), vec![b'a'; half_file]).unwrap();
    fs::write(temp.path().join("b.rs"), []).unwrap();
    write_padding_files(
        temp.path(),
        usize::try_from(MAX_WORKSPACE_TOTAL_BYTES).unwrap() - half_file,
    );
    let effects = RecordingEffects::default();
    let mut workspace = WorkspaceSession::open(
        temp.path(),
        &manifest(&["*.rs", "*.bin"]),
        Some(path("a.rs")),
        effects.clone(),
    )
    .unwrap();

    workspace.execute_editor(EditorCommand::SelectAll).unwrap();
    workspace
        .execute_editor(EditorCommand::DeleteBackward)
        .unwrap();
    workspace
        .execute_editor(EditorCommand::PasteExternal("a".repeat(quarter_file)))
        .unwrap();
    workspace.select_path(&path("b.rs")).unwrap();
    workspace.activate_selected().unwrap();
    workspace
        .execute_editor(EditorCommand::PasteExternal("b".repeat(quarter_file)))
        .unwrap();
    workspace.select_path(&path("a.rs")).unwrap();
    workspace.activate_selected().unwrap();
    workspace.execute_editor(EditorCommand::SelectAll).unwrap();
    workspace
        .execute_editor(EditorCommand::DeleteBackward)
        .unwrap();
    workspace.select_path(&path("b.rs")).unwrap();
    workspace.activate_selected().unwrap();
    workspace
        .execute_editor(EditorCommand::PasteExternal("b".repeat(quarter_file)))
        .unwrap();

    workspace
        .execute_editor(EditorCommand::RequestQuit)
        .unwrap();
    assert_eq!(
        workspace
            .execute_editor(EditorCommand::ConfirmDiscard)
            .unwrap(),
        EditorOutcome::Quit
    );
    assert_eq!(
        workspace.logical_files().unwrap(),
        read_workspace(temp.path()).unwrap()
    );
}

#[test]
fn create_and_rename_reject_invalid_policy_type_parent_and_collisions_atomically() {
    let temp = workspace_fixture();
    let (mut workspace, effects) = open_workspace(temp.path(), "src/lib.rs");
    let before_tree = workspace.file_tree().to_vec();
    let before_files = read_workspace(temp.path()).unwrap();

    for candidate in [
        "../escape.rs",
        "/tmp/escape.rs",
        "tests/denied.rs",
        "src/data.json",
        "src/UPPER.RS",
        "missing/parent.rs",
        "src/lib.rs",
    ] {
        assert!(
            workspace.create_file(candidate).is_err(),
            "created {candidate}"
        );
        assert_eq!(workspace.file_tree(), before_tree);
        assert_eq!(read_workspace(temp.path()).unwrap(), before_files);
        assert!(effects.events().is_empty());
    }

    workspace.select_path(&path("src/lib.rs")).unwrap();
    for candidate in [
        "../escape.rs",
        "tests/denied.rs",
        "src/data.json",
        "Cargo.toml",
    ] {
        assert!(
            workspace.rename_selected(candidate).is_err(),
            "renamed to {candidate}"
        );
        assert_eq!(workspace.file_tree(), before_tree);
        assert_eq!(read_workspace(temp.path()).unwrap(), before_files);
        assert!(effects.events().is_empty());
    }
}

#[test]
fn editable_paths_with_terminal_control_characters_are_rejected() {
    let temp = workspace_fixture();
    let (mut workspace, effects) = open_workspace(temp.path(), "src/lib.rs");
    let before_tree = workspace.file_tree().to_vec();
    let before_files = read_workspace(temp.path()).unwrap();

    for candidate in ["src/evil\n.rs", "src/evil\u{1b}[2J.rs"] {
        assert!(workspace.create_file(candidate).is_err());
        assert!(workspace.rename_selected(candidate).is_err());
        assert_eq!(workspace.file_tree(), before_tree);
        assert_eq!(read_workspace(temp.path()).unwrap(), before_files);
        assert!(effects.events().is_empty());
    }
}

#[test]
fn editable_paths_cannot_enter_hash_excluded_roots() {
    for candidate in [
        ".rustrace/note.rs",
        "target/note.rs",
        ".RUSTRACE/note.rs",
        "Target/note.rs",
    ] {
        for operation in ["create", "rename"] {
            let temp = workspace_fixture();
            fs::create_dir(temp.path().join(".rustrace")).unwrap();
            fs::create_dir(temp.path().join("target")).unwrap();
            let effects = RecordingEffects::default();
            let mut workspace = WorkspaceSession::open(
                temp.path(),
                &manifest(&[
                    "Cargo.lock",
                    "Cargo.toml",
                    "src/**/*.rs",
                    ".rustrace/**/*.rs",
                    "target/**/*.rs",
                    ".RUSTRACE/**/*.rs",
                    "Target/**/*.rs",
                ]),
                Some(path("src/lib.rs")),
                effects.clone(),
            )
            .unwrap();
            let before_tree = workspace.file_tree().to_vec();
            let before_files = read_workspace(temp.path()).unwrap();

            let result = match operation {
                "create" => workspace.create_file(candidate),
                "rename" => workspace.rename_selected(candidate),
                _ => unreachable!(),
            };

            assert!(
                matches!(&result, Err(WorkspaceError::HashExcludedPath { .. })),
                "{operation} returned {result:?} for {candidate}"
            );
            assert_eq!(workspace.file_tree(), before_tree);
            assert_eq!(read_workspace(temp.path()).unwrap(), before_files);
            assert!(effects.events().is_empty());
        }
    }
}

#[test]
fn startup_rejects_editable_files_in_hash_excluded_root_aliases() {
    for (directory, candidate) in [
        (".RUSTRACE", ".RUSTRACE/note.rs"),
        ("Target", "Target/note.rs"),
    ] {
        let temp = TempDir::new();
        fs::create_dir(temp.path().join(directory)).unwrap();
        fs::write(temp.path().join(candidate), "fn hidden() {}\n").unwrap();
        let effects = RecordingEffects::default();

        let result = WorkspaceSession::open(
            temp.path(),
            &manifest(&[candidate]),
            Some(path(candidate)),
            effects,
        );

        assert!(
            matches!(result, Err(WorkspaceError::HashExcludedPath { .. })),
            "startup accepted {candidate}"
        );
    }
}

#[test]
fn lifecycle_recording_failures_poison_create_rename_and_delete_before_mutation() {
    for operation in ["create", "rename", "delete"] {
        let temp = workspace_fixture();
        let effects = RejectingLifecycleEffects::default();
        let mut workspace = WorkspaceSession::open(
            temp.path(),
            &manifest(&["Cargo.lock", "Cargo.toml", "src/**/*.rs"]),
            Some(path("src/lib.rs")),
            effects.clone(),
        )
        .unwrap();
        let before_tree = workspace.file_tree().to_vec();
        let before_files = read_workspace(temp.path()).unwrap();
        effects.reject_next();

        let result = match operation {
            "create" => workspace.create_file("src/new.rs"),
            "rename" => workspace.rename_selected("src/renamed.rs"),
            "delete" => workspace.request_delete_selected(),
            _ => unreachable!(),
        };

        assert!(result.is_err(), "{operation} unexpectedly succeeded");
        assert_eq!(workspace.file_tree(), before_tree, "{operation} tree");
        assert_eq!(
            read_workspace(temp.path()).unwrap(),
            before_files,
            "{operation} filesystem"
        );
        assert!(effects.events.borrow().is_empty(), "{operation} events");
        assert!(
            workspace.create_file("src/after-failure.rs").is_err(),
            "{operation} accepted a later mutation after provenance failure"
        );
        assert!(
            workspace.logical_files().is_err(),
            "{operation} allowed final verification after provenance failure"
        );
    }
}

#[test]
fn recorded_lifecycle_with_failed_filesystem_mutation_requires_recovery() {
    let temp = workspace_fixture();
    let effects = RejectingLifecycleEffects::default();
    let mut workspace = WorkspaceSession::open(
        temp.path(),
        &manifest(&["Cargo.lock", "Cargo.toml", "src/**/*.rs"]),
        Some(path("src/lib.rs")),
        effects.clone(),
    )
    .unwrap();
    let before_tree = workspace.file_tree().to_vec();
    let target = temp.path().join("src/reserved.rs");
    effects.sabotage_next_create(target.clone());

    let result = workspace.create_file("src/reserved.rs");

    assert!(result.is_err());
    assert_eq!(workspace.file_tree(), before_tree);
    assert_eq!(fs::read(&target).unwrap(), b"interloper");
    assert!(matches!(
        effects.events().as_slice(),
        [Event::FileCreated(_), Event::FileFocused(_)]
    ));
    assert!(workspace.create_file("src/after-failure.rs").is_err());
    assert!(workspace.logical_files().is_err());
}

#[test]
fn editor_provenance_failure_poison_rejects_later_operations_and_verification() {
    let temp = workspace_fixture();
    let effects = RejectingLifecycleEffects::default();
    let mut workspace = WorkspaceSession::open(
        temp.path(),
        &manifest(&["Cargo.lock", "Cargo.toml", "src/**/*.rs"]),
        Some(path("src/lib.rs")),
        effects.clone(),
    )
    .unwrap();
    let before_text = workspace.active_buffer().text();
    effects.reject_provenance_in(1);

    assert!(
        workspace
            .execute_editor(EditorCommand::Insert('!'))
            .is_err()
    );
    assert_eq!(workspace.active_buffer().text(), before_text);
    assert!(effects.events().is_empty());
    assert!(workspace.create_file("src/after-failure.rs").is_err());
    assert!(workspace.logical_files().is_err());
}

#[test]
fn workspace_exposes_the_first_poison_reason_read_only() {
    let temp = workspace_fixture();
    let effects = RejectingLifecycleEffects::default();
    let mut workspace = WorkspaceSession::open(
        temp.path(),
        &manifest(&["Cargo.lock", "Cargo.toml", "src/**/*.rs"]),
        Some(path("src/lib.rs")),
        effects.clone(),
    )
    .unwrap();
    assert!(workspace.recovery_reason().is_none());
    effects.reject_provenance_in(1);
    assert!(
        workspace
            .execute_editor(EditorCommand::Insert('!'))
            .is_err()
    );
    let reason = workspace
        .recovery_reason()
        .expect("poison must be inspectable");
    assert!(reason.contains("provenance"));
    assert!(workspace.save_active().is_err());
    assert_eq!(
        workspace.recovery_reason().as_deref(),
        Some(reason.as_str())
    );
    assert!(workspace.logical_files().is_err());
    assert!(workspace.disk_files().is_err());
}

#[test]
fn discard_provenance_failure_keeps_an_authoritative_prefix_and_poison_state() {
    for failure_index in [1_usize, 5, 10] {
        let temp = TempDir::new();
        fs::create_dir(temp.path().join("src")).unwrap();
        let baseline = "x".repeat(rustrace_model::MAX_INSERTED_TEXT_BYTES + 1);
        fs::write(temp.path().join("src/lib.rs"), &baseline).unwrap();
        let effects = RejectingLifecycleEffects::default();
        let mut workspace = WorkspaceSession::open(
            temp.path(),
            &manifest(&["src/**/*.rs"]),
            Some(path("src/lib.rs")),
            effects.clone(),
        )
        .unwrap();
        let initial = snapshot_with_effects(&workspace);
        let initial_owner = envelope(
            initial.session_id(),
            1,
            Hash::zero(),
            Event::WorkspaceCheckpoint(initial.event_payload()),
        );
        let mut replay = ReplayEngine::from_initial_checkpoint(StoredCheckpoint {
            owning_event: initial_owner,
            snapshot: initial,
        })
        .unwrap();

        workspace.execute_editor(EditorCommand::SelectAll).unwrap();
        workspace
            .execute_editor(EditorCommand::DeleteBackward)
            .unwrap();
        let replacement = "y".repeat(baseline.len());
        for chunk in replacement.as_bytes().chunks(128 * 1024) {
            workspace
                .execute_editor(EditorCommand::PasteExternal(
                    std::str::from_utf8(chunk).unwrap().to_owned(),
                ))
                .unwrap();
        }
        workspace
            .execute_editor(EditorCommand::RequestQuit)
            .unwrap();
        let event_count_before_discard = effects.events().len();
        effects.reject_provenance_in(failure_index);

        assert!(
            workspace
                .execute_editor(EditorCommand::ConfirmDiscard)
                .is_err(),
            "discard failure {failure_index} was acknowledged"
        );
        assert!(workspace.confirmation_pending());
        let events = effects.events();
        assert_eq!(
            events[event_count_before_discard..]
                .iter()
                .filter(|event| matches!(event, Event::FileEdited(transaction) if transaction.origin == EditOrigin::FileReload))
                .count(),
            failure_index - 1
        );

        let mut previous = replay.last_event_hash();
        for (index, event) in events.into_iter().enumerate() {
            let event = envelope(replay.session_id(), index as u64 + 2, previous, event);
            replay.apply(&event).unwrap();
            previous = event.event_hash;
        }
        assert_eq!(
            replay
                .workspace_state()
                .files()
                .get(&path("src/lib.rs"))
                .unwrap(),
            workspace.active_buffer().text().as_bytes()
        );
        assert!(workspace.create_file("src/after-failure.rs").is_err());
        assert!(workspace.logical_files().is_err());
    }
}

fn snapshot_with_effects(
    workspace: &WorkspaceSession<RejectingLifecycleEffects>,
) -> CheckpointSnapshot {
    let files = workspace
        .disk_files()
        .unwrap()
        .into_iter()
        .map(|(path, contents)| CheckpointFile { path, contents })
        .collect();
    let documents = workspace
        .file_tree()
        .iter()
        .filter_map(|entry| {
            entry.document_id().map(|document_id| OpenDocument {
                document_id: document_id.clone(),
                path: entry.path().clone(),
                selection: Default::default(),
                version: 0,
            })
        })
        .collect();
    CheckpointSnapshot::new(
        SessionId::new("workspace-session").unwrap(),
        1,
        files,
        Some(workspace.active_document_id().clone()),
        documents,
    )
    .unwrap()
}

#[test]
fn missing_sources_and_symlinks_fail_closed_without_state_or_event_changes() {
    let temp = workspace_fixture();
    let outside_dir = TempDir::new();
    let outside = outside_dir.path().join("outside.rs");
    fs::write(&outside, "outside").unwrap();
    let (mut workspace, effects) = open_workspace(temp.path(), "src/lib.rs");

    let before_tree = workspace.file_tree().to_vec();
    fs::remove_file(temp.path().join("src/lib.rs")).unwrap();
    assert!(workspace.rename_selected("src/renamed.rs").is_err());
    assert!(workspace.request_delete_selected().is_err());
    assert_eq!(workspace.file_tree(), before_tree);
    assert!(effects.events().is_empty());

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        symlink(&outside, temp.path().join("src/lib.rs")).unwrap();
        workspace
            .execute_editor(EditorCommand::Move {
                movement: Movement::DocumentEnd,
                selecting: false,
            })
            .unwrap();
        workspace
            .execute_editor(EditorCommand::Insert('!'))
            .unwrap();
        assert!(workspace.save_active().is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
        assert!(workspace.active_is_dirty());
    }
}

#[test]
fn dirty_delete_uses_confirmation_and_never_loses_content_silently() {
    let temp = workspace_fixture();
    let (mut workspace, effects) = open_workspace(temp.path(), "src/lib.rs");
    workspace
        .execute_editor(EditorCommand::Insert('!'))
        .unwrap();

    assert_eq!(
        workspace.request_delete_selected().unwrap(),
        WorkspaceOutcome::ConfirmationRequired
    );
    assert!(temp.path().join("src/lib.rs").exists());
    assert!(workspace.confirmation_pending());
    assert_eq!(
        workspace_input_for_event(
            key(KeyCode::Esc, KeyModifiers::NONE),
            10,
            WorkspaceFocus::Console,
            true,
        ),
        Some(WorkspaceInput::CancelDestructive),
    );
    assert_eq!(workspace.cancel_delete(), WorkspaceOutcome::Cancelled);
    assert!(temp.path().join("src/lib.rs").exists());
    assert_eq!(effects.events().len(), 1);

    assert_eq!(
        workspace.request_delete_selected().unwrap(),
        WorkspaceOutcome::ConfirmationRequired
    );
    assert_eq!(
        workspace_input_for_event(
            key(KeyCode::Enter, KeyModifiers::NONE),
            10,
            WorkspaceFocus::Console,
            true,
        ),
        Some(WorkspaceInput::ConfirmDestructive),
    );
    assert_eq!(
        workspace.confirm_delete().unwrap(),
        WorkspaceOutcome::FileDeleted
    );
    assert!(!temp.path().join("src/lib.rs").exists());
    assert_eq!(
        effects
            .events()
            .iter()
            .filter(|event| matches!(event, Event::FileDeleted(_)))
            .count(),
        1
    );
}

#[test]
fn startup_rejects_disallowed_paths_and_symlinks_and_marks_allowed_noneditable_files() {
    let disallowed = workspace_fixture();
    fs::write(disallowed.path().join("secret.rs"), "secret").unwrap();
    let effects = RecordingEffects::default();
    let error = WorkspaceSession::open(
        disallowed.path(),
        &manifest(&["Cargo.lock", "Cargo.toml", "src/**/*.rs"]),
        None,
        effects.clone(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("not allowed"));
    assert!(effects.events().is_empty());

    let unsafe_name = workspace_fixture();
    fs::rename(
        unsafe_name.path().join("src/lib.rs"),
        unsafe_name.path().join("src/evil\n.rs"),
    )
    .unwrap();
    let error = WorkspaceSession::open(
        unsafe_name.path(),
        &manifest(&["Cargo.lock", "Cargo.toml", "src/**/*.rs"]),
        None,
        RecordingEffects::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("terminal control"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let linked = workspace_fixture();
        let outside = linked.path().join("outside-source.rs");
        fs::write(&outside, "outside").unwrap();
        symlink(&outside, linked.path().join("src/linked.rs")).unwrap();
        let error = WorkspaceSession::open(
            linked.path(),
            &manifest(&["Cargo.lock", "Cargo.toml", "src/**/*.rs"]),
            None,
            effects,
        )
        .unwrap_err();
        assert!(error.to_string().contains("symlink"));
    }
}

#[test]
fn startup_rejects_terminal_unsafe_read_only_paths_without_rendering_them() {
    for unsafe_component in ["evil\n.bin", "evil\u{1b}[2J.bin"] {
        let temp = workspace_fixture();
        fs::create_dir(temp.path().join("notes")).unwrap();
        fs::write(temp.path().join("notes").join(unsafe_component), b"notes").unwrap();

        let error = WorkspaceSession::open(
            temp.path(),
            &manifest(&["Cargo.lock", "Cargo.toml", "src/**/*.rs", "notes/*.bin"]),
            None,
            RecordingEffects::default(),
        )
        .unwrap_err();
        let displayed = error.to_string();

        assert!(matches!(error, WorkspaceError::UnsafeDisplayPath { .. }));
        assert!(!displayed.contains('\n'));
        assert!(!displayed.contains('\u{1b}'));
    }
}

#[cfg(unix)]
#[test]
fn root_rebinding_cannot_redirect_session_reads_or_mutations() {
    use std::os::unix::fs::symlink;

    for operation in ["save", "create", "rename", "delete", "snapshot"] {
        let outer = TempDir::new();
        let root = outer.path().join("workspace");
        let outside = outer.path().join("outside");
        let retired = outer.path().join("retired");
        for directory in [&root, &outside] {
            fs::create_dir(directory).unwrap();
            fs::create_dir(directory.join("src")).unwrap();
            fs::write(
                directory.join("Cargo.toml"),
                "[package]\nname='demo'\n[workspace]\n",
            )
            .unwrap();
            fs::write(directory.join("src/lib.rs"), "fn answer() {}\n").unwrap();
        }
        let effects = RecordingEffects::default();
        let mut workspace = WorkspaceSession::open(
            &root,
            &manifest(&["Cargo.toml", "src/**/*.rs"]),
            Some(path("src/lib.rs")),
            effects.clone(),
        )
        .unwrap();
        if operation == "save" {
            workspace
                .execute_editor(EditorCommand::Insert('!'))
                .unwrap();
        }
        let event_count = effects.events().len();
        let outside_before = read_workspace(&outside).unwrap();

        fs::rename(&root, &retired).unwrap();
        symlink(&outside, &root).unwrap();
        let result = match operation {
            "save" => workspace.save_active(),
            "create" => workspace.create_file("src/new.rs").map(|_| ()),
            "rename" => workspace.rename_selected("src/renamed.rs").map(|_| ()),
            "delete" => workspace.request_delete_selected().map(|_| ()),
            "snapshot" => workspace.logical_files().map(|_| ()),
            _ => unreachable!(),
        };

        assert!(result.is_err(), "{operation} accepted a rebound root");
        assert_eq!(
            read_workspace(&outside).unwrap(),
            outside_before,
            "{operation} changed the rebound target"
        );
        assert_eq!(effects.events().len(), event_count, "{operation} events");

        fs::remove_file(&root).unwrap();
        fs::rename(&retired, &root).unwrap();
    }
}

#[test]
fn logical_files_are_bounded_and_match_saved_disk_bytes() {
    let temp = workspace_fixture();
    let (workspace, _) = open_workspace(temp.path(), "src/lib.rs");
    let expected: BTreeMap<_, _> = read_workspace(temp.path()).unwrap();

    assert_eq!(workspace.logical_files().unwrap(), expected);
}

#[test]
fn allowed_missing_or_nondirectory_parent_rejects_before_lifecycle_intent() {
    for destination in ["src/missing/new.rs", "src/lib.rs/new.rs"] {
        for rename in [false, true] {
            let temp = workspace_fixture();
            let (mut workspace, effects) = open_workspace(temp.path(), "src/lib.rs");
            let tree = workspace.file_tree().to_vec();
            let disk = workspace.disk_files().unwrap();
            let active = workspace.active_document_id().clone();
            let result = if rename {
                workspace.rename_selected(destination)
            } else {
                workspace.create_file(destination)
            };
            assert!(result.is_err());
            assert!(
                effects.events().is_empty(),
                "{destination}, rename={rename}"
            );
            assert!(workspace.recovery_reason().is_none());
            assert_eq!(workspace.file_tree(), tree);
            assert_eq!(workspace.active_document_id(), &active);
            assert_eq!(workspace.disk_files().unwrap(), disk);
            workspace.create_file("src/after.rs").unwrap();
        }
    }
}

#[test]
fn host_equivalent_destinations_reject_before_lifecycle_intent() {
    for (original, alias) in [("lib.rs", "LIB.rs"), ("caf\u{e9}.rs", "cafe\u{301}.rs")] {
        for rename in [false, true] {
            let temp = workspace_fixture();
            fs::write(temp.path().join("src").join(original), b"original").unwrap();
            if !temp.path().join("src").join(alias).exists() {
                continue; // This spelling is distinct on this filesystem.
            }
            let (mut workspace, effects) = open_workspace(temp.path(), "src/lib.rs");
            let tree = workspace.file_tree().to_vec();
            let disk = workspace.disk_files().unwrap();
            let active = workspace.active_document_id().clone();
            let destination = format!("src/{alias}");
            let result = if rename {
                workspace.rename_selected(&destination)
            } else {
                workspace.create_file(&destination)
            };
            assert!(result.is_err());
            assert!(
                effects.events().is_empty(),
                "{destination}, rename={rename}"
            );
            assert!(workspace.recovery_reason().is_none());
            assert_eq!(workspace.file_tree(), tree);
            assert_eq!(workspace.active_document_id(), &active);
            assert_eq!(workspace.disk_files().unwrap(), disk);
        }
    }
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> TerminalEvent {
    TerminalEvent::Key(KeyEvent::new(code, modifiers))
}

fn workspace_input_for_event(
    event: TerminalEvent,
    page_lines: usize,
    focus: WorkspaceFocus,
    confirmation_pending: bool,
) -> Option<WorkspaceInput> {
    workspace_input_for_event_with_modifier(
        event,
        page_lines,
        focus,
        confirmation_pending,
        PrimaryModifier::Control,
    )
}

#[test]
fn file_management_is_mouse_only_while_primary_w_stays_global() {
    assert_eq!(
        workspace_input_for_event(
            key(KeyCode::F(2), KeyModifiers::NONE),
            10,
            WorkspaceFocus::Editor,
            false,
        ),
        None,
        "F2 must not create a file-tree focus mode"
    );
    for code in [
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Enter,
        KeyCode::Char('n'),
        KeyCode::Char('r'),
        KeyCode::Char('d'),
        KeyCode::Delete,
    ] {
        assert!(
            !matches!(
                workspace_input_for_event(
                    key(code, KeyModifiers::NONE),
                    10,
                    WorkspaceFocus::Editor,
                    false,
                ),
                Some(
                    WorkspaceInput::BeginCreate
                        | WorkspaceInput::BeginRename
                        | WorkspaceInput::DeleteSelected
                )
            ),
            "{code:?} retained a file-panel management route"
        );
    }
    assert_eq!(
        workspace_input_for_event(
            key(KeyCode::Char('w'), KeyModifiers::CONTROL),
            10,
            WorkspaceFocus::Editor,
            false,
        ),
        Some(WorkspaceInput::DeleteSelected)
    );
    assert!(
        rustrace::tui::KEYBIND_ROWS.contains(&CTRL_W_DELETE_SELECTED_HELP_ENTRY),
        "Ctrl-W's destructive routing must use its selected-file confirmation help entry"
    );
    assert!(CTRL_W_DELETE_SELECTED_HELP_ENTRY.contains("delete selected file"));
    assert!(CTRL_W_DELETE_SELECTED_HELP_ENTRY.contains("confirm"));
    assert!(rustrace::tui::KEYBIND_ROWS.contains(&"Right-click file        file menu"));
    for removed in [
        "F2 / Esc               focus files / return",
        "Up / Down              select file",
        "Enter                  open file",
        "N / R                  new / rename",
        "D / Delete             delete",
    ] {
        assert!(!rustrace::tui::KEYBIND_ROWS.contains(&removed));
    }
}

#[test]
fn console_focus_keeps_global_quit_and_delete_but_does_not_edit_the_workspace() {
    assert_eq!(
        workspace_input_for_event(
            key(KeyCode::Char('x'), KeyModifiers::NONE),
            10,
            WorkspaceFocus::Console,
            false,
        ),
        None
    );
    assert_eq!(
        workspace_input_for_event(
            key(KeyCode::Char('q'), KeyModifiers::CONTROL),
            10,
            WorkspaceFocus::Console,
            false,
        ),
        Some(WorkspaceInput::Editor(
            rustrace::tui::SessionInput::Command(EditorCommand::RequestQuit,)
        ))
    );
    assert_eq!(
        workspace_input_for_event(
            key(KeyCode::Char('w'), KeyModifiers::CONTROL),
            10,
            WorkspaceFocus::Console,
            false,
        ),
        Some(WorkspaceInput::DeleteSelected),
    );
    assert_eq!(
        workspace_input_for_event(
            key(
                KeyCode::Char('w'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            10,
            WorkspaceFocus::Console,
            false,
        ),
        None,
    );
}

#[test]
fn workspace_command_mode_keeps_control_aliases_and_routes_super_globally() {
    for focus in [WorkspaceFocus::Editor, WorkspaceFocus::Console] {
        let control = workspace_input_for_event_with_modifier(
            key(KeyCode::Char('q'), KeyModifiers::CONTROL),
            10,
            focus,
            true,
            PrimaryModifier::Command,
        );
        assert_eq!(
            workspace_input_for_event_with_modifier(
                key(KeyCode::Char('q'), KeyModifiers::SUPER),
                10,
                focus,
                true,
                PrimaryModifier::Command,
            ),
            control,
            "Super-Q did not follow the global Control-Q path for {focus:?}"
        );
        assert!(control.is_some(), "Control-Q alias was lost for {focus:?}");
        assert_eq!(
            workspace_input_for_event_with_modifier(
                key(KeyCode::Char('q'), KeyModifiers::SUPER),
                10,
                focus,
                true,
                PrimaryModifier::Control,
            ),
            None,
            "Control mode accepted Super-Q for {focus:?}"
        );
    }

    for focus in [WorkspaceFocus::Editor, WorkspaceFocus::Console] {
        assert_eq!(
            workspace_input_for_event_with_modifier(
                key(KeyCode::Char('w'), KeyModifiers::SUPER),
                10,
                focus,
                false,
                PrimaryModifier::Command,
            ),
            Some(WorkspaceInput::DeleteSelected),
            "exact Super-W was not global for {focus:?}",
        );
    }
}

#[test]
fn mixed_modifiers_preserve_legacy_global_and_workspace_routes() {
    let quit = Some(WorkspaceInput::Editor(
        rustrace::tui::SessionInput::Command(EditorCommand::RequestQuit),
    ));
    for primary_modifier in [PrimaryModifier::Control, PrimaryModifier::Command] {
        let focus = WorkspaceFocus::Console;
        assert_eq!(
            workspace_input_for_event_with_modifier(
                key(KeyCode::Char('q'), KeyModifiers::CONTROL),
                10,
                focus,
                false,
                primary_modifier,
            ),
            quit,
        );
        for modifiers in [
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ] {
            assert_eq!(
                workspace_input_for_event_with_modifier(
                    key(KeyCode::Char('q'), modifiers),
                    10,
                    focus,
                    false,
                    primary_modifier,
                ),
                None,
                "mixed Control-Q changed the legacy {focus:?} route in {primary_modifier:?} mode",
            );
        }
    }

    for focus in [WorkspaceFocus::Editor, WorkspaceFocus::Console] {
        assert_eq!(
            workspace_input_for_event_with_modifier(
                key(KeyCode::Char('q'), KeyModifiers::SUPER),
                10,
                focus,
                false,
                PrimaryModifier::Command,
            ),
            quit,
            "exact Super-Q must match exact Control-Q for {focus:?}",
        );
        assert_eq!(
            workspace_input_for_event_with_modifier(
                key(
                    KeyCode::Char('q'),
                    KeyModifiers::SUPER | KeyModifiers::SHIFT,
                ),
                10,
                focus,
                false,
                PrimaryModifier::Command,
            ),
            if focus == WorkspaceFocus::Editor {
                quit.clone()
            } else {
                None
            },
            "mixed Super-Q must reach only the containment-based editor route",
        );
    }

    let delete = Some(WorkspaceInput::DeleteSelected);
    let close = Some(WorkspaceInput::Editor(
        rustrace::tui::SessionInput::Command(EditorCommand::CloseActive),
    ));
    for primary_modifier in [PrimaryModifier::Control, PrimaryModifier::Command] {
        assert_eq!(
            workspace_input_for_event_with_modifier(
                key(KeyCode::Char('w'), KeyModifiers::CONTROL),
                10,
                WorkspaceFocus::Editor,
                false,
                primary_modifier,
            ),
            delete,
        );
        for modifiers in [
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ] {
            assert_eq!(
                workspace_input_for_event_with_modifier(
                    key(KeyCode::Char('w'), modifiers),
                    10,
                    WorkspaceFocus::Editor,
                    false,
                    primary_modifier,
                ),
                close,
                "mixed Control-W must retain the base editor CloseActive route",
            );
        }
    }
    assert_eq!(
        workspace_input_for_event_with_modifier(
            key(KeyCode::Char('w'), KeyModifiers::SUPER),
            10,
            WorkspaceFocus::Editor,
            false,
            PrimaryModifier::Command,
        ),
        delete,
        "exact Super-W must match exact Control-W",
    );
}

#[test]
fn poisoned_quit_preserves_clean_dirty_and_pending_confirmation_state() {
    for state in ["clean", "dirty", "quit", "delete"] {
        let temp = workspace_fixture();
        let effects = RejectingLifecycleEffects::default();
        let mut workspace = WorkspaceSession::open(
            temp.path(),
            &manifest(&["Cargo.lock", "Cargo.toml", "src/**/*.rs"]),
            Some(path("src/lib.rs")),
            effects.clone(),
        )
        .unwrap();
        if state != "clean" {
            workspace
                .execute_editor(EditorCommand::Insert('!'))
                .unwrap();
        }
        if state == "quit" {
            workspace
                .execute_editor(EditorCommand::RequestQuit)
                .unwrap();
        } else if state == "delete" {
            workspace.request_delete_selected().unwrap();
        }
        let text = workspace.active_buffer().text();
        let disk = read_workspace(temp.path()).unwrap();
        let events = effects.events();
        effects.reject_next();
        assert!(workspace.create_file("src/rejected.rs").is_err());
        let reason = workspace.recovery_reason().unwrap();

        assert_eq!(
            workspace
                .execute_editor(EditorCommand::RequestQuit)
                .unwrap(),
            EditorOutcome::Quit,
            "{state}"
        );
        assert_eq!(workspace.active_buffer().text(), text);
        assert_eq!(read_workspace(temp.path()).unwrap(), disk);
        assert_eq!(effects.events(), events);
        assert_eq!(
            workspace.recovery_reason().as_deref(),
            Some(reason.as_str())
        );
        assert!(workspace.logical_files().is_err());
        assert!(workspace.save_active().is_err());
    }
}

#[test]
fn quit_key_is_routed_even_during_pending_confirmation() {
    for focus in [WorkspaceFocus::Editor, WorkspaceFocus::Console] {
        assert_eq!(
            workspace_input_for_event(
                key(KeyCode::Char('q'), KeyModifiers::CONTROL),
                10,
                focus,
                true
            ),
            Some(WorkspaceInput::Editor(
                rustrace::tui::SessionInput::Command(EditorCommand::RequestQuit)
            ))
        );
    }
}
