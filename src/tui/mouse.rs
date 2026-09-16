use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use super::{
    EditorCommand, SessionInput, WorkspaceInput,
    shell::{HitMap, ShellLayout, bottom_height_for_divider_row, sidebar_width_for_divider_column},
};

const DOUBLE_CLICK_MILLIS: u64 = 350;
const MAX_DRAINED_MOUSE_EVENTS: usize = 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MouseEventBatch {
    pub event: Event,
    pub moved: Vec<MouseEvent>,
    pub pending: Option<Event>,
}

pub fn drain_mouse_event_batch<E>(
    event: Event,
    mut read_queued: impl FnMut() -> Result<Option<Event>, E>,
) -> Result<MouseEventBatch, E> {
    let mut batch = MouseEventBatch {
        event,
        moved: Vec::new(),
        pending: None,
    };
    if !matches!(
        batch.event,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(_),
            ..
        })
    ) {
        return Ok(batch);
    }
    for _ in 0..MAX_DRAINED_MOUSE_EVENTS {
        let Some(queued) = read_queued()? else {
            break;
        };
        match queued {
            Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Drag(_)) => {
                batch.event = Event::Mouse(mouse);
            }
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved => {
                batch.moved.push(mouse);
            }
            pending => {
                batch.pending = Some(pending);
                break;
            }
        }
    }
    Ok(batch)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DrawGate {
    requested: bool,
}

impl DrawGate {
    pub const fn requested() -> Self {
        Self { requested: true }
    }

    pub fn request_timer(&mut self) {
        self.requested = true;
    }

    pub fn request_change(&mut self, changed: bool) {
        self.requested |= changed;
    }

    pub fn take_draw(&mut self) -> bool {
        std::mem::take(&mut self.requested)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ShellModal {
    #[default]
    None,
    Prompt,
    FilePrompt,
    FindPanel,
    Confirmation,
    CommandMenu,
    EditorContextMenu,
    FilesContextMenu,
    Completion,
    Keybinds,
    UpdateNotice,
    ConsoleOverwrite,
    TestCases,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShellState {
    pub modal: ShellModal,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PaneResizeOutcome {
    pub consumed: bool,
    pub changed: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PaneResizeState {
    bottom_height: Option<u16>,
    sidebar_width: Option<u16>,
    dragging: Option<ResizeTarget>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResizeTarget {
    Bottom,
    Sidebar,
}

impl PaneResizeState {
    pub const fn bottom_height(&self) -> Option<u16> {
        self.bottom_height
    }

    pub const fn sidebar_width(&self) -> Option<u16> {
        self.sidebar_width
    }

    pub const fn sidebar_dragging(&self) -> bool {
        matches!(self.dragging, Some(ResizeTarget::Sidebar))
    }

    pub fn reduce(
        &mut self,
        event: &MouseEvent,
        hits: &HitMap,
        shell: ShellState,
        layout: ShellLayout,
    ) -> PaneResizeOutcome {
        if shell.modal != ShellModal::None {
            self.dragging = None;
            return PaneResizeOutcome::default();
        }

        let position = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left)
                if event.modifiers == KeyModifiers::NONE && contains(hits.pane_split, position) =>
            {
                self.dragging = Some(ResizeTarget::Bottom);
                PaneResizeOutcome {
                    consumed: true,
                    changed: false,
                }
            }
            MouseEventKind::Down(MouseButton::Left)
                if event.modifiers == KeyModifiers::NONE
                    && contains(hits.sidebar_split, position) =>
            {
                self.dragging = Some(ResizeTarget::Sidebar);
                PaneResizeOutcome {
                    consumed: true,
                    changed: false,
                }
            }
            MouseEventKind::Drag(MouseButton::Left)
                if self.dragging == Some(ResizeTarget::Bottom)
                    && event.modifiers == KeyModifiers::NONE =>
            {
                let bottom_height = bottom_height_for_divider_row(layout, event.row);
                let changed = self.bottom_height != Some(bottom_height);
                self.bottom_height = Some(bottom_height);
                PaneResizeOutcome {
                    consumed: true,
                    changed,
                }
            }
            MouseEventKind::Drag(MouseButton::Left)
                if self.dragging == Some(ResizeTarget::Sidebar)
                    && event.modifiers == KeyModifiers::NONE =>
            {
                let sidebar_width = sidebar_width_for_divider_column(layout, event.column);
                let changed = self.sidebar_width != Some(sidebar_width);
                self.sidebar_width = Some(sidebar_width);
                PaneResizeOutcome {
                    consumed: true,
                    changed,
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging.is_some() => {
                self.dragging = None;
                PaneResizeOutcome {
                    consumed: true,
                    changed: false,
                }
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {
                self.dragging = None;
                PaneResizeOutcome::default()
            }
            MouseEventKind::Up(_) | MouseEventKind::Moved => PaneResizeOutcome::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellInput {
    Workspace(WorkspaceInput),
    ActivateFile(usize),
    ActivateTab(String),
    OpenMenu,
    ActivateMenu(usize),
    CancelMenu,
    OpenEditorContextMenu(Position),
    ActivateEditorContextMenu(usize),
    CancelEditorContextMenu,
    ScrollEditorContextMenu(isize),
    OpenFilesContextMenu(Position, Option<usize>),
    ActivateFilesContextMenu(usize),
    CancelFilesContextMenu,
    ScrollFilesContextMenu(isize),
    FocusConsole,
    SelectDiagnostic(usize),
    SelectEditorDiagnostic {
        line: usize,
        column: usize,
        diagnostic_index: usize,
    },
    ScrollEditor(isize),
    ScrollEditorPage(isize),
    SetEditorScroll(u16),
    ScrollOutput(isize),
    ScrollOverlay(isize),
    AcceptCompletion(usize),
    ScrollCompletion(isize),
    DismissCompletion,
    CloseKeybinds,
    CloseUpdateNotice,
    SelectTestCase(usize),
    RunTestCase(usize),
    ScrollTestCases(isize),
    SubmitFilePrompt,
    CancelFilePrompt,
    OpenFind,
    FindNext,
    Replace,
    ReplaceAll,
    CloseFind,
    ConfirmModal,
    CancelModal,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MouseState {
    modal: ShellModal,
    last_left_down: Option<(Position, u64)>,
    active_left_down: Option<Position>,
    double_click: bool,
    last_now_ms: u64,
}

impl MouseState {
    pub fn reduce(&mut self, event: &MouseEvent, now_ms: u64, modal: ShellModal) {
        self.double_click = false;
        if modal != self.modal || now_ms < self.last_now_ms {
            self.reset_gesture();
            self.modal = modal;
        }
        self.last_now_ms = now_ms;
        let position = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if event.modifiers == KeyModifiers::NONE => {
                self.double_click = self.last_left_down.is_some_and(|(previous, previous_ms)| {
                    previous == position
                        && now_ms.saturating_sub(previous_ms) <= DOUBLE_CLICK_MILLIS
                });
                self.last_left_down = (!self.double_click).then_some((position, now_ms));
                self.active_left_down = Some(position);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.reset_gesture();
                self.active_left_down = Some(position);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.active_left_down != Some(position) {
                    self.last_left_down = None;
                }
                self.active_left_down = None;
            }
            MouseEventKind::Moved => {
                if self
                    .last_left_down
                    .is_some_and(|(previous, _)| previous != position)
                {
                    self.last_left_down = None;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.last_left_down = None;
            }
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
            | MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_) => self.reset_gesture(),
        }
    }

    fn reset_gesture(&mut self) {
        self.last_left_down = None;
        self.active_left_down = None;
        self.double_click = false;
    }
}

pub fn mouse_input_for_event(
    event: &MouseEvent,
    hits: &HitMap,
    shell: &ShellState,
    state: &MouseState,
) -> Option<ShellInput> {
    let position = Position::new(event.column, event.row);
    let wheel = wheel_delta(event.kind);
    match shell.modal {
        ShellModal::Prompt => return None,
        ShellModal::FindPanel => {
            if !left_down(event) {
                return None;
            }
            return if contains(hits.find_next, position) {
                Some(ShellInput::FindNext)
            } else if contains(hits.find_replace, position) {
                Some(ShellInput::Replace)
            } else if contains(hits.find_replace_all, position) {
                Some(ShellInput::ReplaceAll)
            } else if contains(hits.find_close, position) || !contains(hits.overlay, position) {
                Some(ShellInput::CloseFind)
            } else {
                None
            };
        }
        ShellModal::FilePrompt => {
            return left_down(event)
                .then(|| {
                    if contains(hits.overlay_confirm, position) {
                        Some(ShellInput::SubmitFilePrompt)
                    } else if contains(hits.overlay_cancel, position) {
                        Some(ShellInput::CancelFilePrompt)
                    } else {
                        None
                    }
                })
                .flatten()
                .or_else(|| overlay_wheel(wheel, hits.overlay, position));
        }
        ShellModal::Completion => {
            if contains(hits.completion_popup, position) {
                if left_down(event) {
                    return hits.completion_rows.iter().find_map(|(rect, index)| {
                        contains(*rect, position).then_some(ShellInput::AcceptCompletion(*index))
                    });
                }
                return wheel.map(ShellInput::ScrollCompletion);
            }
            return matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
                .then_some(ShellInput::DismissCompletion);
        }
        ShellModal::Confirmation => {
            return left_down(event)
                .then(|| {
                    if contains(hits.overlay_confirm, position) {
                        Some(ShellInput::Workspace(WorkspaceInput::ConfirmDestructive))
                    } else if contains(hits.overlay_cancel, position) {
                        Some(ShellInput::Workspace(WorkspaceInput::CancelDestructive))
                    } else {
                        None
                    }
                })
                .flatten()
                .or_else(|| overlay_wheel(wheel, hits.overlay, position));
        }
        ShellModal::ConsoleOverwrite => {
            return left_down(event)
                .then(|| {
                    if contains(hits.overlay_confirm, position) {
                        Some(ShellInput::ConfirmModal)
                    } else if contains(hits.overlay_cancel, position) {
                        Some(ShellInput::CancelModal)
                    } else {
                        None
                    }
                })
                .flatten()
                .or_else(|| overlay_wheel(wheel, hits.overlay, position));
        }
        ShellModal::CommandMenu => {
            if !left_down(event) {
                return overlay_wheel(wheel, hits.overlay, position);
            }
            return Some(
                hits.context_menu_rows
                    .iter()
                    .find_map(|(rect, index)| {
                        contains(*rect, position).then_some(ShellInput::ActivateMenu(*index))
                    })
                    .unwrap_or(ShellInput::CancelMenu),
            );
        }
        ShellModal::EditorContextMenu => {
            if let Some(delta) = wheel.filter(|_| contains(hits.overlay, position)) {
                return Some(ShellInput::ScrollEditorContextMenu(delta));
            }
            if !left_down(event) {
                return None;
            }
            if let Some((_, index, enabled)) = hits
                .editor_context_menu_rows
                .iter()
                .find(|(rect, _, _)| contains(*rect, position))
            {
                return enabled.then_some(ShellInput::ActivateEditorContextMenu(*index));
            }
            return (!contains(hits.overlay, position))
                .then_some(ShellInput::CancelEditorContextMenu);
        }
        ShellModal::FilesContextMenu => {
            if event.kind == MouseEventKind::Down(MouseButton::Right)
                && event.modifiers == KeyModifiers::NONE
            {
                return Some(ShellInput::CancelFilesContextMenu);
            }
            if let Some(delta) = wheel.filter(|_| contains(hits.overlay, position)) {
                return Some(ShellInput::ScrollFilesContextMenu(delta));
            }
            if !left_down(event) {
                return None;
            }
            if let Some((_, index, enabled)) = hits
                .files_context_menu_rows
                .iter()
                .find(|(rect, _, _)| contains(*rect, position))
            {
                return enabled.then_some(ShellInput::ActivateFilesContextMenu(*index));
            }
            return (!contains(hits.overlay, position))
                .then_some(ShellInput::CancelFilesContextMenu);
        }
        ShellModal::UpdateNotice => {
            return (left_down(event) && contains(hits.overlay_cancel, position))
                .then_some(ShellInput::CloseUpdateNotice);
        }
        ShellModal::Keybinds => {
            if left_down(event) && contains(hits.overlay_cancel, position) {
                return Some(ShellInput::CloseKeybinds);
            }
            return overlay_wheel(wheel, hits.overlay, position);
        }
        ShellModal::TestCases => {
            if let Some(delta) = wheel.filter(|_| contains(hits.overlay, position)) {
                return Some(ShellInput::ScrollTestCases(delta));
            }
            let selected = hits
                .test_case_rows
                .iter()
                .find_map(|(rect, index)| contains(*rect, position).then_some(*index));
            if event.kind == MouseEventKind::Moved {
                return selected.map(ShellInput::SelectTestCase);
            }
            if left_down(event) {
                return selected.map(|index| {
                    if state.double_click {
                        ShellInput::RunTestCase(index)
                    } else {
                        ShellInput::SelectTestCase(index)
                    }
                });
            }
            return None;
        }
        ShellModal::None => {}
    }

    if let Some(delta) = wheel {
        if contains(hits.editor.rect, position) || contains(hits.editor_scrollbar_track, position) {
            return Some(ShellInput::ScrollEditor(delta));
        }
        if contains(hits.output, position) {
            return Some(ShellInput::ScrollOutput(delta));
        }
        return None;
    }

    match event.kind {
        MouseEventKind::Down(MouseButton::Right) => {
            if event.modifiers != KeyModifiers::NONE {
                return None;
            }
            if let Some(index) = hits.sidebar_rows.iter().find_map(|(rect, target)| {
                (contains(*rect, position))
                    .then_some(target)
                    .and_then(|target| match target {
                        super::shell::SidebarTarget::File(index) => Some(*index),
                        super::shell::SidebarTarget::New | super::shell::SidebarTarget::Menu => {
                            None
                        }
                    })
            }) {
                return Some(ShellInput::OpenFilesContextMenu(position, Some(index)));
            }
            if contains(hits.sidebar_files, position) {
                return Some(ShellInput::OpenFilesContextMenu(position, None));
            }
            contains(hits.editor.rect, position)
                .then_some(ShellInput::OpenEditorContextMenu(position))
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if event.modifiers == KeyModifiers::SHIFT {
                return editor_move(event, hits, false);
            }
            if event.modifiers != KeyModifiers::NONE {
                return None;
            }
            if contains(hits.editor_scrollbar_track, position) {
                if contains(hits.editor_scrollbar_thumb, position) {
                    return None;
                }
                return Some(ShellInput::ScrollEditorPage(
                    if event.row < hits.editor_scrollbar_thumb.y {
                        -1
                    } else {
                        1
                    },
                ));
            }
            if contains(hits.editor.rect, position) {
                if state.double_click {
                    return Some(ShellInput::Workspace(WorkspaceInput::Editor(
                        SessionInput::Command(EditorCommand::SelectWord),
                    )));
                }
                if let Some(diagnostic_index) = hits
                    .editor_diagnostic_rows
                    .iter()
                    .find_map(|(rect, index)| contains(*rect, position).then_some(*index))
                {
                    let (line, column) = editor_line_column(event, hits)?;
                    return Some(ShellInput::SelectEditorDiagnostic {
                        line,
                        column,
                        diagnostic_index,
                    });
                }
                return editor_move(event, hits, false);
            }
            if let Some(input) = hits.sidebar_rows.iter().find_map(|(rect, target)| {
                contains(*rect, position).then_some(match target {
                    super::shell::SidebarTarget::File(index) => ShellInput::ActivateFile(*index),
                    super::shell::SidebarTarget::New => {
                        ShellInput::Workspace(WorkspaceInput::BeginCreate)
                    }
                    super::shell::SidebarTarget::Menu => ShellInput::OpenMenu,
                })
            }) {
                return Some(input);
            }
            if contains(hits.sidebar_new, position) {
                return Some(ShellInput::Workspace(WorkspaceInput::BeginCreate));
            }
            if contains(hits.sidebar_menu, position) {
                return Some(ShellInput::OpenMenu);
            }
            if contains(hits.sidebar_find, position) {
                return Some(ShellInput::OpenFind);
            }
            if let Some(path) = hits
                .tab_pills
                .iter()
                .find_map(|(rect, path)| contains(*rect, position).then(|| path.clone()))
            {
                return Some(ShellInput::ActivateTab(path));
            }
            if contains(hits.new_tab, position) {
                return Some(ShellInput::Workspace(WorkspaceInput::BeginCreate));
            }
            if contains(hits.tab_scroll_left, position) {
                return Some(ShellInput::Workspace(WorkspaceInput::Editor(
                    SessionInput::Command(EditorCommand::PreviousBuffer),
                )));
            }
            if contains(hits.tab_scroll_right, position) {
                return Some(ShellInput::Workspace(WorkspaceInput::Editor(
                    SessionInput::Command(EditorCommand::NextBuffer),
                )));
            }
            if let Some(index) = hits
                .output_rows
                .iter()
                .find_map(|(rect, index)| contains(*rect, position).then_some(*index))
            {
                return Some(ShellInput::SelectDiagnostic(index));
            }
            if contains(hits.console, position) {
                return Some(ShellInput::FocusConsole);
            }
            None
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if event.modifiers == KeyModifiers::NONE
                && state.active_left_down.is_some_and(|down| {
                    contains(hits.editor_scrollbar_thumb, down)
                        && contains(hits.editor_scrollbar_track, position)
                })
            {
                return Some(ShellInput::SetEditorScroll(event.row));
            }
            if event.modifiers != KeyModifiers::NONE && event.modifiers != KeyModifiers::SHIFT {
                return None;
            }
            editor_move(event, hits, true)
        }
        _ => None,
    }
}

fn editor_move(event: &MouseEvent, hits: &HitMap, drag: bool) -> Option<ShellInput> {
    let (line, column) = editor_line_column(event, hits)?;
    Some(ShellInput::Workspace(WorkspaceInput::Editor(
        SessionInput::Command(EditorCommand::MoveTo {
            line,
            column,
            selecting: drag || event.modifiers.contains(KeyModifiers::SHIFT),
        }),
    )))
}

fn editor_line_column(event: &MouseEvent, hits: &HitMap) -> Option<(usize, usize)> {
    let position = Position::new(event.column, event.row);
    if !contains(hits.editor.rect, position) {
        return None;
    }
    let screen_column = usize::from(event.column.saturating_sub(hits.editor.rect.x));
    let column = hits.editor.left_column + screen_column;
    let line = hits.editor.top_line + usize::from(event.row.saturating_sub(hits.editor.rect.y));
    Some((line, column))
}

fn wheel_delta(kind: MouseEventKind) -> Option<isize> {
    match kind {
        MouseEventKind::ScrollUp => Some(-3),
        MouseEventKind::ScrollDown => Some(3),
        _ => None,
    }
}

fn overlay_wheel(delta: Option<isize>, overlay: Rect, position: Position) -> Option<ShellInput> {
    delta
        .filter(|_| contains(overlay, position))
        .map(ShellInput::ScrollOverlay)
}

fn left_down(event: &MouseEvent) -> bool {
    event.kind == MouseEventKind::Down(MouseButton::Left) && event.modifiers == KeyModifiers::NONE
}

fn contains(rect: Rect, position: Position) -> bool {
    !rect.is_empty() && rect.contains(position)
}
