//! Main student terminal UI and its terminal-lifecycle boundary.

pub mod completion;
mod editor;
mod mouse;
pub mod shell;
mod terminal;
pub mod theme;
mod workspace;

use std::path::Path;
use std::rc::Rc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};
use rustrace_model::OutputStream;
use unicode_segmentation::UnicodeSegmentation;

use crate::display;
pub use crate::editor::{DiagnosticLineMarker, DiagnosticMarkerKind};
use crate::editor::{
    EditorBuffer, EditorEffects, EditorWidget, HighlightSpan, LiveDiagnosticSpan, Viewport,
};
use crate::{config::PrimaryModifier, ghostty::GhosttyKeyBindings};
use shell::{
    BottomPane, HitMap, ShellLayout, ShellLayoutResult, SidebarTarget, display_width,
    put_right_text, put_text, set_style, shell_layout, shell_layout_with_sizes, truncate_to_width,
};
use theme::Palette;

pub use completion::{
    CompletionKeyAction, CompletionTrigger, CompletionTriggerInput, CompletionViewItem,
    completion_key_action, completion_trigger_input,
};
pub(crate) use editor::session_input_for_event_with_keyboard_enhancement;
pub use editor::{
    DestructiveAction, EDITOR_KEY_HINTS, EditorCommand, EditorOutcome, EditorSession,
    EditorStorage, FileSystemStorage, SearchOutcome, SearchSummary, SessionInput,
    has_exact_primary_modifier, has_primary_modifier, session_input_for_event,
};
pub use mouse::{
    DrawGate, MouseEventBatch, MouseState, PaneResizeOutcome, PaneResizeState, ShellInput,
    ShellModal, ShellState, drain_mouse_event_batch, mouse_input_for_event,
};
#[cfg(test)]
pub(crate) use terminal::osc52_clipboard_sequence;
pub use terminal::{CrosstermTerminalOperations, TerminalOperations, TerminalSession};
pub(crate) use workspace::FormatterDocumentState;
pub(crate) use workspace::workspace_input_for_event_with_keyboard_enhancement;
pub use workspace::{
    CTRL_W_DELETE_SELECTED_HELP_ENTRY, WorkspaceEffectError, WorkspaceEffects, WorkspaceError,
    WorkspaceFile, WorkspaceFocus, WorkspaceInput, WorkspaceOutcome, WorkspaceSession,
    diagnostic_delta_for_event, workspace_input_for_event,
};

pub const MIN_TERMINAL_WIDTH: u16 = 60;
pub const MIN_TERMINAL_HEIGHT: u16 = 15;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingState {
    Active,
    Inactive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalHealth {
    Healthy,
    Degraded,
}

pub const COMMAND_MENU_ENTRIES: [&str; 12] = [
    "Check",
    "Run",
    "Clippy",
    "Format",
    "Doc",
    "Update dependencies",
    "Update Rustrace",
    "Automatic checks: On",
    "Console",
    "Test cases",
    "Keybinds",
    "Quit",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandMenuAction {
    Check,
    Run,
    Clippy,
    Format,
    Doc,
    Update,
    UpdateRustrace,
    AutomaticChecks,
    Console,
    TestCases,
    Keybinds,
    Quit,
}

const COMMAND_MENU_ACTIONS: [CommandMenuAction; 12] = [
    CommandMenuAction::Check,
    CommandMenuAction::Run,
    CommandMenuAction::Clippy,
    CommandMenuAction::Format,
    CommandMenuAction::Doc,
    CommandMenuAction::Update,
    CommandMenuAction::UpdateRustrace,
    CommandMenuAction::AutomaticChecks,
    CommandMenuAction::Console,
    CommandMenuAction::TestCases,
    CommandMenuAction::Keybinds,
    CommandMenuAction::Quit,
];

pub const fn command_menu_action(index: usize) -> Option<CommandMenuAction> {
    if index < COMMAND_MENU_ACTIONS.len() {
        Some(COMMAND_MENU_ACTIONS[index])
    } else {
        None
    }
}

pub const EDITOR_CONTEXT_MENU_ENTRIES: [&str; 4] = ["Cut", "Copy", "Paste", "Select all"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EditorContextMenuState {
    anchor: Position,
    selected: usize,
    enabled: [bool; 4],
}

impl EditorContextMenuState {
    pub fn new(anchor: Position, has_selection: bool, has_clipboard: bool) -> Self {
        let enabled = [has_selection, has_selection, has_clipboard, true];
        let selected = enabled.iter().position(|enabled| *enabled).unwrap_or(0);
        Self {
            anchor,
            selected,
            enabled,
        }
    }

    pub const fn anchor(self) -> Position {
        self.anchor
    }

    pub const fn selected(self) -> usize {
        self.selected
    }

    pub const fn enabled(self, index: usize) -> bool {
        index < self.enabled.len() && self.enabled[index]
    }

    pub fn command(self, index: usize) -> Option<EditorCommand> {
        if !self.enabled(index) {
            return None;
        }
        match index {
            0 => Some(EditorCommand::Cut),
            1 => Some(EditorCommand::Copy),
            2 => Some(EditorCommand::Paste),
            3 => Some(EditorCommand::SelectAll),
            _ => None,
        }
    }

    pub(crate) fn move_selection(&mut self, delta: isize) -> bool {
        if self.enabled.iter().all(|enabled| !enabled) {
            return false;
        }
        let previous = self.selected;
        for distance in 1..=self.enabled.len() {
            let index = (self.selected as isize + delta.signum() * distance as isize)
                .rem_euclid(self.enabled.len() as isize) as usize;
            if self.enabled[index] {
                self.selected = index;
                break;
            }
        }
        self.selected != previous
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditorContextMenuKeyAction {
    None,
    SelectionChanged,
    Activate(usize),
    Cancel,
}

pub fn editor_context_menu_key_action(
    menu: &mut EditorContextMenuState,
    key: KeyEvent,
) -> EditorContextMenuKeyAction {
    if key.kind == KeyEventKind::Release {
        return EditorContextMenuKeyAction::None;
    }
    match key.code {
        KeyCode::Left | KeyCode::Up => {
            if menu.move_selection(-1) {
                EditorContextMenuKeyAction::SelectionChanged
            } else {
                EditorContextMenuKeyAction::None
            }
        }
        KeyCode::Right | KeyCode::Down => {
            if menu.move_selection(1) {
                EditorContextMenuKeyAction::SelectionChanged
            } else {
                EditorContextMenuKeyAction::None
            }
        }
        KeyCode::Enter if menu.enabled(menu.selected) => {
            EditorContextMenuKeyAction::Activate(menu.selected)
        }
        KeyCode::Esc => EditorContextMenuKeyAction::Cancel,
        _ => EditorContextMenuKeyAction::None,
    }
}

pub const FILES_CONTEXT_MENU_ENTRIES: [&str; 4] = ["open", "rename…", "delete…", "new file…"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilesContextMenuAction {
    Open,
    Rename,
    Delete,
    NewFile,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilesContextMenuState {
    anchor: Position,
    selected: usize,
    enabled: [bool; 4],
}

impl FilesContextMenuState {
    pub fn new(anchor: Position, has_file: bool) -> Self {
        let enabled = [has_file, has_file, has_file, true];
        let selected = enabled
            .iter()
            .position(|enabled| *enabled)
            .expect("new file is always enabled");
        Self {
            anchor,
            selected,
            enabled,
        }
    }

    pub const fn anchor(self) -> Position {
        self.anchor
    }

    pub const fn selected(self) -> usize {
        self.selected
    }

    pub const fn enabled(self, index: usize) -> bool {
        index < self.enabled.len() && self.enabled[index]
    }

    pub const fn action(self, index: usize) -> Option<FilesContextMenuAction> {
        if !self.enabled(index) {
            return None;
        }
        match index {
            0 => Some(FilesContextMenuAction::Open),
            1 => Some(FilesContextMenuAction::Rename),
            2 => Some(FilesContextMenuAction::Delete),
            3 => Some(FilesContextMenuAction::NewFile),
            _ => None,
        }
    }

    pub(crate) fn move_selection(&mut self, delta: isize) -> bool {
        let previous = self.selected;
        for distance in 1..=self.enabled.len() {
            let index = (self.selected as isize + delta.signum() * distance as isize)
                .rem_euclid(self.enabled.len() as isize) as usize;
            if self.enabled[index] {
                self.selected = index;
                break;
            }
        }
        self.selected != previous
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilesContextMenuKeyAction {
    None,
    SelectionChanged,
    Activate(usize),
    Cancel,
}

pub fn files_context_menu_key_action(
    menu: &mut FilesContextMenuState,
    key: KeyEvent,
) -> FilesContextMenuKeyAction {
    if key.kind == KeyEventKind::Release {
        return FilesContextMenuKeyAction::None;
    }
    match key.code {
        KeyCode::Left | KeyCode::Up => {
            if menu.move_selection(-1) {
                FilesContextMenuKeyAction::SelectionChanged
            } else {
                FilesContextMenuKeyAction::None
            }
        }
        KeyCode::Right | KeyCode::Down => {
            if menu.move_selection(1) {
                FilesContextMenuKeyAction::SelectionChanged
            } else {
                FilesContextMenuKeyAction::None
            }
        }
        KeyCode::Enter if menu.enabled(menu.selected) => {
            FilesContextMenuKeyAction::Activate(menu.selected)
        }
        KeyCode::Esc => FilesContextMenuKeyAction::Cancel,
        _ => FilesContextMenuKeyAction::None,
    }
}

pub const OBVIOUS_EDITOR_KEYBIND_ROWS: [&str; 7] = [
    "Typing / Enter         insert text / newline",
    "Arrows                 move caret",
    "Shift+movement         select",
    "Home / End             line start / end",
    "PageUp / PageDown      move one page",
    "Backspace / Delete     delete",
    "Tab / Shift-Tab        indent / outdent",
];

pub const KEYBIND_ROWS: [&str; 66] = [
    "EDITOR",
    "{line-navigation}",
    "{document-navigation}",
    "{word-navigation}",
    "{navigation-deletions}",
    "{navigation-selection}",
    "Ctrl-/                 toggle line comments",
    "{select-all}",
    "Ctrl-C / Ctrl-X        copy / cut",
    "Ctrl-V                 internal paste",
    "Ctrl-Z / Ctrl-Y        undo / redo",
    "Typing pause / Ctrl-Space  open completion",
    "Ctrl-F / F3            find panel / next match",
    "Ctrl-S                 save all, then Check",
    "Alt-Up / Alt-Down      Cargo or live diagnostic previous / next",
    "FILES",
    "Right-click file        file menu",
    "TABS",
    "Ctrl-Tab / Ctrl-BackTab  next / previous buffer",
    CTRL_W_DELETE_SELECTED_HELP_ENTRY,
    "F5 / F6                previous / next buffer",
    "COMMANDS",
    "F1                     keybinds",
    "F4                     test cases",
    "F7                     menu",
    "F8                     reload language service",
    "F9                     console",
    "Esc                    stop active Cargo command",
    "MENU",
    "Update dependencies    update Cargo dependencies",
    "Update Rustrace        cached version information",
    "Automatic checks: On/Off  persist preference for next launch",
    "Left / Up              previous entry",
    "Right / Down           next entry",
    "Enter                  run or open entry",
    "Esc                    close menu",
    "KEYBINDS",
    "Up / Down              scroll one row",
    "PageUp / PageDown      scroll ten rows",
    "Esc / Enter / F1       dismiss",
    "COMPLETION",
    "Up / Down              select completion",
    "Tab / Enter            accept completion",
    "Esc                    close completion",
    "PROMPTS",
    "Typing / Backspace     edit prompt",
    "Enter / Esc            submit / cancel",
    "CONFIRMATIONS",
    "Y / Enter              confirm",
    "N / Esc                cancel",
    "CONSOLE",
    "Typing                 edit command or stdin",
    "Left / Right           move line cursor",
    "Home / End             line start / end",
    "Backspace / Delete     edit line",
    "Enter                  run command / send stdin",
    "PgUp / PgDn / wheel    scroll output",
    "Ctrl-C                 stop running command",
    "Esc                    cancel command / close console",
    "TEST CASES",
    "Up / Down              select case",
    "Enter                  run selected case",
    "R                      refresh cases",
    "Esc                    close test cases",
    "SESSION",
    "Ctrl-Q                 quit",
];

pub(crate) fn maximum_keybinds_scroll(area: Rect, row_count: usize) -> usize {
    let panel_width = area.width.saturating_sub(4).min(76);
    let panel_height = area.height.saturating_sub(2).min(22);
    let inner_width = panel_width.saturating_sub(2);
    let inner_height = panel_height.saturating_sub(2);
    if inner_width < 20 || inner_height < 6 {
        return 0;
    }
    let visible_rows = usize::from(inner_height.saturating_sub(3));
    row_count.saturating_sub(visible_rows)
}

pub fn primary_modifier_text(text: &str, primary_modifier: PrimaryModifier) -> String {
    primary_modifier_text_with_ghostty(text, primary_modifier, &GhosttyKeyBindings::passed())
}

pub fn primary_modifier_text_with_ghostty(
    text: &str,
    primary_modifier: PrimaryModifier,
    ghostty_keys: &GhosttyKeyBindings,
) -> String {
    let text = if primary_modifier == PrimaryModifier::Command
        && !ghostty_keys.command_hint_available("super+k")
    {
        text.lines()
            .filter(|line| !line.contains("Ctrl-K") && !line.contains("ctrl-K"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text.to_owned()
    };
    let ctrl_a_is_rewritten = ghostty_keys.observed_exact_translation("super+arrow_left");
    let text = text.replace(
        "{select-all}",
        if ctrl_a_is_rewritten {
            "right-click → Select all"
        } else {
            "Ctrl-A                 select all"
        },
    );
    let text = if ctrl_a_is_rewritten {
        text.replace("Ctrl-A select all", "right-click → Select all")
            .replace("Ctrl-A all", "right-click → Select all")
    } else {
        text
    };
    let rendered = match primary_modifier {
        PrimaryModifier::Control => text
            .replace(
                "{line-navigation}",
                "Home / End             line start / end",
            )
            .replace(
                "{document-navigation}",
                "Ctrl-Home / Ctrl-End   document start / end",
            )
            .replace(
                "{word-navigation}",
                "Ctrl-Left / Ctrl-Right  previous / next word",
            )
            .replace(
                "{navigation-deletions}",
                "Ctrl-Backspace         delete previous word",
            ),
        PrimaryModifier::Command => text,
    }
    .replace(
        "{navigation-selection}",
        "Shift + navigation     extend selection",
    );

    match primary_modifier {
        PrimaryModifier::Control => rendered.replace("Alt-", "alt-"),
        PrimaryModifier::Command => {
            let mut rendered = rendered
                .replace("Ctrl-", "⌘")
                .replace("ctrl-", "⌘")
                .replace("Alt-", "Option-")
                .replace("alt-", "Option-")
                .replace("⌘Q", "Ctrl-Q")
                .replace("⌘q", "Ctrl-Q")
                .replace("⌘W", "Ctrl-W")
                .replace("⌘w", "Ctrl-W")
                .replace("⌘A", "Ctrl-A")
                .replace("⌘a", "Ctrl-A")
                .replace("⌘C", "Ctrl-C")
                .replace("⌘c", "Ctrl-C")
                .replace("⌘X", "Ctrl-X")
                .replace("⌘x", "Ctrl-X")
                .replace("⌘V", "Ctrl-V")
                .replace("⌘v", "Ctrl-V")
                .replace("⌘Space", "Ctrl-Space")
                .replace("⌘BackTab", "Ctrl-BackTab")
                .replace("⌘Tab", "Ctrl-Tab");
            for (key, upper, lower, control) in [
                ("super+f", "⌘F", "⌘f", "Ctrl-F"),
                ("super+z", "⌘Z", "⌘z", "Ctrl-Z"),
                ("super+k", "⌘K", "⌘k", "Ctrl-K"),
                ("super+home", "⌘Home", "⌘home", "Ctrl-Home"),
                ("super+end", "⌘End", "⌘end", "Ctrl-End"),
            ] {
                if !ghostty_keys.command_hint_available(key) {
                    rendered = rendered.replace(upper, control).replace(lower, control);
                }
            }
            let document_navigation = match (
                ghostty_keys.command_hint_available("super+arrow_up"),
                ghostty_keys.command_hint_available("super+arrow_down"),
            ) {
                (false, false) => "Ctrl-Home / Ctrl-End   document start / end",
                (false, true) => "Ctrl-Home / ⌘Down      document start / end",
                (true, false) => "⌘Up / Ctrl-End        document start / end",
                (true, true) => "⌘Up / ⌘Down           document start / end",
            };
            let line_navigation = match (
                ghostty_keys.command_hint_available("super+arrow_left"),
                ghostty_keys.command_hint_available("super+arrow_right"),
            ) {
                (false, false) => "Home / End             line start / end",
                (false, true) => "Home / ⌘Right          line start / end",
                (true, false) => "⌘Left / End            line start / end",
                (true, true) => "⌘Left / ⌘Right       line start / end",
            };
            let word_navigation = match (
                ghostty_keys.command_hint_available("alt+arrow_left"),
                ghostty_keys.command_hint_available("alt+arrow_right"),
            ) {
                (false, false) => "Ctrl-Left / Ctrl-Right  previous / next word",
                (false, true) => "Ctrl-Left / Option-Right  previous / next word",
                (true, false) => "Option-Left / Ctrl-Right  previous / next word",
                (true, true) => "Option-Left / Option-Right  previous / next word",
            };
            let navigation_deletions = if ghostty_keys.command_hint_available("super+backspace") {
                "Option-Backspace / ⌘Backspace  word / line-start delete"
            } else {
                "Option-Backspace       delete previous word"
            };
            let rendered = rendered
                .replace("{line-navigation}", line_navigation)
                .replace("{document-navigation}", document_navigation)
                .replace("{word-navigation}", word_navigation)
                .replace("{navigation-deletions}", navigation_deletions);
            if ctrl_a_is_rewritten {
                rendered
                    .replace("Ctrl-A", "right-click → Select all")
                    .replace("⌘A", "right-click → Select all")
            } else {
                rendered
            }
        }
    }
}

fn keybinds_modifier_text_with_ghostty(
    text: &str,
    primary_modifier: PrimaryModifier,
    ghostty_keys: &GhosttyKeyBindings,
) -> String {
    primary_modifier_text_with_ghostty(text, primary_modifier, ghostty_keys)
        .replace("Ctrl-", "Control-")
}

fn single_line_text(text: &str) -> String {
    text.lines().collect::<Vec<_>>().join(" · ")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToastState {
    kind: ToastKind,
    title: String,
    body: String,
}

impl ToastState {
    pub fn new(kind: ToastKind, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            body: body.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfirmationState {
    message: String,
}

impl ConfirmationState {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileTreeViewEntry {
    path: String,
    workspace_index: usize,
    active: bool,
    selected: bool,
    dirty: bool,
    editable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BufferTabViewEntry {
    path: String,
    active: bool,
    dirty: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputRow {
    text: String,
    diagnostic_index: Option<usize>,
    stream: Option<OutputStream>,
    selected: bool,
}

impl OutputRow {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            diagnostic_index: None,
            stream: None,
            selected: false,
        }
    }

    pub fn diagnostic(text: impl Into<String>, diagnostic_index: usize) -> Self {
        Self {
            text: text.into(),
            diagnostic_index: Some(diagnostic_index),
            stream: None,
            selected: false,
        }
    }

    pub fn captured(text: impl Into<String>, stream: OutputStream) -> Self {
        Self {
            text: text.into(),
            diagnostic_index: None,
            stream: Some(stream),
            selected: false,
        }
    }

    pub fn with_selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn diagnostic_index(&self) -> Option<usize> {
        self.diagnostic_index
    }

    pub const fn selected(&self) -> bool {
        self.selected
    }
}

impl BufferTabViewEntry {
    pub fn new(path: impl Into<String>, active: bool, dirty: bool) -> Self {
        Self {
            path: path.into(),
            active,
            dirty,
        }
    }
}

impl FileTreeViewEntry {
    pub fn new(
        path: impl Into<String>,
        active: bool,
        selected: bool,
        dirty: bool,
        editable: bool,
    ) -> Self {
        Self {
            path: path.into(),
            workspace_index: 0,
            active,
            selected,
            dirty,
            editable,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModeBarKind {
    Confirm,
    Menu,
    Complete,
    Console,
    TestCases,
    Running,
    Keybinds,
    Error,
}

impl ModeBarKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Confirm => " CONFIRM ",
            Self::Menu => " MENU ",
            Self::Complete => " COMPLETE ",
            Self::Console => " CONSOLE ",
            Self::TestCases => " TEST CASES ",
            Self::Running => " RUNNING ",
            Self::Keybinds => " KEYBINDS ",
            Self::Error => " ERROR ",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilePromptKind {
    Create,
    Rename,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilePromptState {
    kind: FilePromptKind,
    input: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FindPanelField {
    Find,
    Replace,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FindPanelState {
    find: String,
    replace: String,
    active: FindPanelField,
    counter: String,
}

impl FindPanelState {
    pub fn new(
        find: impl Into<String>,
        replace: impl Into<String>,
        active: FindPanelField,
        counter: impl Into<String>,
    ) -> Self {
        Self {
            find: find.into(),
            replace: replace.into(),
            active,
            counter: counter.into(),
        }
    }
}

impl FilePromptState {
    pub fn new(kind: FilePromptKind, input: impl Into<String>) -> Self {
        Self {
            kind,
            input: input.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModeBarState {
    kind: ModeBarKind,
    hints: String,
    prompt: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestCasePickerRow {
    name: String,
    status: String,
}

impl TestCasePickerRow {
    pub fn new(name: impl Into<String>, status: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: status.into(),
        }
    }

    pub fn status(&self) -> &str {
        &self.status
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestCasePickerState {
    rows: Vec<TestCasePickerRow>,
    selected: usize,
    notice: Option<String>,
}

impl TestCasePickerState {
    pub fn new(rows: Vec<TestCasePickerRow>, selected: usize, notice: Option<String>) -> Self {
        let selected = selected.min(rows.len());
        Self {
            rows,
            selected,
            notice,
        }
    }
}

impl ModeBarState {
    pub fn new(kind: ModeBarKind, hints: impl Into<String>) -> Self {
        Self {
            kind,
            hints: single_line_text(&hints.into()),
            prompt: String::new(),
        }
    }

    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = single_line_text(&prompt.into());
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MainViewState {
    buffers: Vec<BufferTabViewEntry>,
    file_tree: Vec<FileTreeViewEntry>,
    editor_focused: bool,
    bottom_pane_focused: bool,
    bottom_pane_height: Option<u16>,
    sidebar_width: Option<u16>,
    sidebar_dragging: bool,
    output: Vec<OutputRow>,
    output_header_message: Option<String>,
    output_scroll: usize,
    recording: RecordingState,
    journal: JournalHealth,
    recovery_reason: Option<String>,
    error_condition: Option<String>,
    diagnostic_markers: Vec<DiagnosticLineMarker>,
    live_diagnostics: Vec<LiveDiagnosticSpan>,
    console: Option<ConsoleViewState>,
    mode_bar: Option<ModeBarState>,
    command_menu: Option<usize>,
    update_state: crate::update::UpdateState,
    update_panel: Option<(bool, u64)>,
    editor_context_menu: Option<EditorContextMenuState>,
    files_context_menu: Option<FilesContextMenuState>,
    keybinds_scroll: Option<usize>,
    test_case_picker: Option<TestCasePickerState>,
    keybind_rows: Vec<&'static str>,
    file_prompt: Option<FilePromptState>,
    find_panel: Option<FindPanelState>,
    confirmation: Option<ConfirmationState>,
    toast: Option<ToastState>,
    completion: Option<completion::CompletionPopupState>,
    primary_modifier: PrimaryModifier,
    ghostty_key_bindings: GhosttyKeyBindings,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConsoleViewState {
    output: Vec<u8>,
    prompt: Vec<u8>,
    cursor: Option<usize>,
    // First visible output row; None follows the newest output.
    scroll: Option<usize>,
    // Rows the caller already wrapped for this output and pane width.
    rows: Option<Rc<ConsoleRows>>,
}

impl MainViewState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        _assignment_title: impl Into<String>,
        buffers: Vec<BufferTabViewEntry>,
        output: Vec<String>,
        recording: RecordingState,
        journal: JournalHealth,
        _session_state: impl Into<String>,
    ) -> Self {
        let buffers = buffers
            .into_iter()
            .filter(|buffer| buffer.path != "Cargo.lock")
            .take(rustrace_workspace::hash::MAX_WORKSPACE_FILES)
            .collect::<Vec<_>>();
        let file_tree = buffers
            .iter()
            .take(rustrace_workspace::hash::MAX_WORKSPACE_FILES)
            .enumerate()
            .map(|(index, buffer)| {
                let mut entry = FileTreeViewEntry::new(
                    &buffer.path,
                    buffer.active,
                    buffer.active,
                    buffer.dirty,
                    true,
                );
                entry.workspace_index = index;
                entry
            })
            .collect();

        Self {
            buffers,
            file_tree,
            editor_focused: true,
            bottom_pane_focused: false,
            bottom_pane_height: None,
            sidebar_width: None,
            sidebar_dragging: false,
            output: output.into_iter().map(OutputRow::plain).collect(),
            output_header_message: None,
            output_scroll: 0,
            recording,
            journal,
            recovery_reason: None,
            error_condition: None,
            diagnostic_markers: Vec::new(),
            live_diagnostics: Vec::new(),
            console: None,
            mode_bar: None,
            command_menu: None,
            update_state: crate::update::UpdateState::default(),
            update_panel: None,
            editor_context_menu: None,
            files_context_menu: None,
            keybinds_scroll: None,
            test_case_picker: None,
            keybind_rows: KEYBIND_ROWS.to_vec(),
            file_prompt: None,
            find_panel: None,
            confirmation: None,
            toast: None,
            completion: None,
            primary_modifier: PrimaryModifier::Control,
            ghostty_key_bindings: GhosttyKeyBindings::passed(),
        }
    }

    pub fn with_file_tree(mut self, entries: Vec<FileTreeViewEntry>) -> Self {
        self.file_tree = entries
            .into_iter()
            .enumerate()
            .filter_map(|(workspace_index, mut entry)| {
                (entry.path != "Cargo.lock").then(|| {
                    entry.workspace_index = workspace_index;
                    entry
                })
            })
            .collect();
        self
    }

    pub fn with_diagnostic_markers(mut self, markers: Vec<DiagnosticLineMarker>) -> Self {
        self.diagnostic_markers = markers;
        self
    }

    pub fn with_live_diagnostics(mut self, diagnostics: Vec<LiveDiagnosticSpan>) -> Self {
        self.live_diagnostics = diagnostics;
        self
    }

    pub fn with_output_header_message(mut self, message: impl Into<String>) -> Self {
        self.output_header_message = Some(message.into());
        self
    }

    pub fn with_output_rows(mut self, rows: Vec<OutputRow>) -> Self {
        self.output = rows;
        self
    }

    pub fn with_output_scroll(mut self, offset: usize) -> Self {
        self.output_scroll = offset;
        self
    }

    pub fn with_bottom_pane_height(mut self, height: u16) -> Self {
        self.bottom_pane_height = Some(height);
        self
    }

    pub fn with_sidebar_width(mut self, width: u16) -> Self {
        self.sidebar_width = Some(width);
        self
    }

    pub fn with_sidebar_dragging(mut self, dragging: bool) -> Self {
        self.sidebar_dragging = dragging;
        self
    }

    pub fn with_mode_bar(mut self, mode_bar: ModeBarState) -> Self {
        self.mode_bar = Some(mode_bar);
        self
    }

    pub fn with_update_panel(mut self, managed: bool, now: u64) -> Self {
        self.update_panel = Some((managed, now));
        self
    }

    pub fn with_update_state(mut self, state: crate::update::UpdateState) -> Self {
        self.update_state = state;
        self
    }

    fn update_available(&self) -> bool {
        self.update_state
            .latest
            .as_ref()
            .is_some_and(|latest| latest.is_newer_than(env!("CARGO_PKG_VERSION")))
    }

    pub fn with_command_menu(mut self, selected: usize) -> Self {
        self.command_menu = Some(selected.min(COMMAND_MENU_ENTRIES.len() - 1));
        self
    }

    pub fn with_editor_context_menu(mut self, menu: EditorContextMenuState) -> Self {
        self.editor_context_menu = Some(menu);
        self
    }

    pub fn with_files_context_menu(mut self, menu: FilesContextMenuState) -> Self {
        self.files_context_menu = Some(menu);
        self
    }

    pub fn with_keybinds_overlay(mut self, scroll: usize) -> Self {
        self.keybinds_scroll = Some(scroll);
        self.mode_bar = Some(ModeBarState::new(
            ModeBarKind::Keybinds,
            "scroll ↑↓/pgup/pgdn · close esc/enter",
        ));
        self
    }

    pub fn with_test_case_picker(mut self, picker: TestCasePickerState) -> Self {
        self.test_case_picker = Some(picker);
        self.mode_bar = Some(ModeBarState::new(
            ModeBarKind::TestCases,
            "esc close  ↵ run  ↑↓ select  r refresh",
        ));
        self
    }

    pub fn with_file_prompt(mut self, prompt: FilePromptState) -> Self {
        self.file_prompt = Some(prompt);
        self
    }

    pub fn with_find_panel(mut self, panel: FindPanelState) -> Self {
        self.find_panel = Some(panel);
        self
    }

    #[doc(hidden)]
    pub fn with_keybind_rows(mut self, rows: Vec<&'static str>) -> Self {
        self.keybind_rows = rows;
        self
    }

    pub fn with_primary_modifier(mut self, primary_modifier: PrimaryModifier) -> Self {
        self.primary_modifier = primary_modifier;
        self
    }

    #[doc(hidden)]
    pub fn with_ghostty_key_bindings(mut self, bindings: GhosttyKeyBindings) -> Self {
        self.ghostty_key_bindings = bindings;
        self
    }

    pub fn with_confirmation(mut self, confirmation: ConfirmationState) -> Self {
        self.confirmation = Some(confirmation);
        self
    }

    pub fn with_toast(mut self, toast: ToastState) -> Self {
        self.toast = Some(toast);
        self
    }

    pub fn with_completion_popup(
        mut self,
        items: Vec<CompletionViewItem>,
        selected: usize,
    ) -> Self {
        self.completion = Some(completion::CompletionPopupState {
            items: items
                .into_iter()
                .take(crate::language_service::MAX_COMPLETION_ITEMS)
                .collect(),
            selected,
        });
        self
    }

    pub fn with_console_scroll(mut self, scroll: Option<usize>) -> Self {
        if let Some(console) = &mut self.console {
            console.scroll = scroll;
        }
        self
    }

    pub(crate) fn with_console_rows(mut self, rows: Rc<ConsoleRows>) -> Self {
        if let Some(console) = &mut self.console {
            console.rows = Some(rows);
        }
        self
    }

    pub fn with_console_body_view(
        mut self,
        _title: impl Into<String>,
        output: Vec<u8>,
        prompt: Vec<u8>,
        cursor: Option<usize>,
        focused: bool,
    ) -> Self {
        self.console = Some(ConsoleViewState {
            output,
            prompt,
            cursor: cursor.filter(|_| focused),
            scroll: None,
            rows: None,
        });
        if focused {
            self.editor_focused = false;
        }
        self.bottom_pane_focused = focused;
        if focused && self.mode_bar.is_none() {
            self.mode_bar = Some(ModeBarState::new(
                ModeBarKind::Console,
                "esc close  ↵ run/send",
            ));
        }
        self
    }

    pub fn with_recovery_reason(mut self, reason: Option<&str>) -> Self {
        if self.recovery_reason.is_none()
            && let Some(reason) = reason
        {
            let reason = display::plain(
                reason.as_bytes(),
                display::Limits {
                    output_bytes: 1024,
                    ..display::Limits::default()
                },
            )
            .text
            .to_string();
            self.recording = RecordingState::Inactive;
            self.journal = JournalHealth::Degraded;
            self.recovery_reason = Some(reason);
        }
        self
    }

    pub fn with_error_condition(mut self, condition: impl Into<String>) -> Self {
        self.error_condition = Some(condition.into());
        self
    }

    fn error_text(&self) -> Option<String> {
        self.error_condition
            .clone()
            .or_else(|| self.recovery_reason.clone())
            .or_else(|| {
                if self.recording == RecordingState::Inactive {
                    Some("recording stopped".to_owned())
                } else if self.journal == JournalHealth::Degraded {
                    Some("journal degraded".to_owned())
                } else {
                    None
                }
            })
    }
}

pub type MainLayout = ShellLayoutResult;

pub fn main_layout(area: Rect) -> MainLayout {
    shell_layout(area, BottomPane::Output, false)
}

pub struct MainView<'a, S>
where
    S: EditorEffects,
{
    state: &'a MainViewState,
    editor: &'a EditorBuffer<S>,
    viewport: &'a Viewport,
    highlights: &'a [HighlightSpan],
    palette: Palette,
}

impl<'a, S> MainView<'a, S>
where
    S: EditorEffects,
{
    pub fn new(
        state: &'a MainViewState,
        editor: &'a EditorBuffer<S>,
        viewport: &'a Viewport,
        highlights: &'a [HighlightSpan],
    ) -> Self {
        Self {
            state,
            editor,
            viewport,
            highlights,
            palette: Palette::selected(),
        }
    }

    pub fn with_palette(mut self, palette: Palette) -> Self {
        self.palette = palette;
        self
    }

    pub fn render_with_hit_map(self, area: Rect, buffer: &mut Buffer) -> HitMap {
        Clear.render(area, buffer);
        let error = self.state.error_text();
        let mode = self.state.mode_bar.as_ref();
        let show_mode_bar = error.is_some() || mode.is_some();
        let bottom = if self.state.console.is_some() {
            BottomPane::Console
        } else {
            BottomPane::Output
        };
        match shell_layout_with_sizes(
            area,
            bottom,
            show_mode_bar,
            self.state.bottom_pane_height,
            self.state.sidebar_width,
        ) {
            ShellLayoutResult::TooSmall(area) => {
                self.render_too_small(area, buffer, error.as_deref());
                HitMap::default()
            }
            ShellLayoutResult::Full(layout) => {
                let mut hits = HitMap::default();
                self.render_sidebar(layout, buffer, &mut hits);
                self.render_pane_divider(layout, buffer, &mut hits);
                self.render_tab_bar(layout.tab_bar, buffer, &mut hits);
                self.render_editor(layout, buffer, &mut hits);
                if let Some(console) = &self.state.console {
                    self.render_console(console, layout.bottom, buffer, &mut hits);
                } else {
                    self.render_output(layout.bottom, buffer, &mut hits);
                }
                if let Some(completion) = &self.state.completion {
                    let source =
                        shell::editor_source_layout(layout.editor, self.editor.line_count()).area;
                    completion::render_completion_popup(
                        completion,
                        self.editor,
                        self.viewport,
                        completion::CompletionPopupAnchor {
                            editor_area: source,
                        },
                        &self.palette,
                        buffer,
                        &mut hits,
                    );
                }
                match (mode, error.as_deref()) {
                    (Some(mode), Some(error)) => {
                        let total_width = layout.mode_bar.width;
                        let mode_label_width = display_width(mode.kind.label()) as u16;
                        let mut desired_mode_width = mode_label_width;
                        if !mode.prompt.is_empty() {
                            desired_mode_width = desired_mode_width
                                .saturating_add(display_width(&mode.prompt) as u16 + 3);
                        }
                        if !mode.hints.is_empty() {
                            desired_mode_width = desired_mode_width
                                .saturating_add(display_width(&mode.hints) as u16 + 1);
                        }
                        let error_label_width = display_width(ModeBarKind::Error.label()) as u16;
                        let minimum_error_width = error_label_width
                            .saturating_add(12)
                            .min(total_width.saturating_sub(mode_label_width));
                        let mode_width =
                            desired_mode_width.min(total_width.saturating_sub(minimum_error_width));
                        let mode_area = Rect::new(
                            layout.mode_bar.x,
                            layout.mode_bar.y,
                            mode_width,
                            layout.mode_bar.height,
                        );
                        let error_area = Rect::new(
                            mode_area.right(),
                            layout.mode_bar.y,
                            total_width.saturating_sub(mode_width),
                            layout.mode_bar.height,
                        );
                        self.render_mode_bar(mode_area, mode, buffer);
                        self.render_mode_bar(
                            error_area,
                            &ModeBarState::new(ModeBarKind::Error, error),
                            buffer,
                        );
                    }
                    (None, Some(error)) => self.render_mode_bar(
                        layout.mode_bar,
                        &ModeBarState::new(ModeBarKind::Error, error),
                        buffer,
                    ),
                    (Some(mode), None) => self.render_mode_bar(layout.mode_bar, mode, buffer),
                    (None, None) => self.render_idle_mode_bar(layout.mode_bar, buffer),
                }
                if let Some(selected) = self.state.command_menu {
                    self.render_command_menu(layout, selected, buffer, &mut hits);
                }
                if let Some(menu) = self.state.editor_context_menu {
                    self.render_editor_context_menu(layout, menu, buffer, &mut hits);
                }
                if let Some(menu) = self.state.files_context_menu {
                    self.render_files_context_menu(layout, menu, buffer, &mut hits);
                }
                if let Some(scroll) = self.state.keybinds_scroll {
                    self.render_keybinds_overlay(area, scroll, buffer, &mut hits);
                }
                if let Some((managed, now)) = self.state.update_panel {
                    self.render_update_panel(area, managed, now, buffer, &mut hits);
                }
                if let Some(picker) = &self.state.test_case_picker {
                    self.render_test_case_picker(area, picker, buffer, &mut hits);
                }
                if let Some(prompt) = &self.state.file_prompt {
                    self.render_file_prompt(area, prompt, buffer, &mut hits);
                }
                if let Some(panel) = &self.state.find_panel {
                    self.render_find_panel(area, panel, buffer, &mut hits);
                }
                if let Some(confirmation) = &self.state.confirmation {
                    self.render_confirmation(area, confirmation, buffer, &mut hits);
                }
                if let Some(toast) = &self.state.toast {
                    let toast_area = if layout.bottom.is_empty() {
                        layout.editor
                    } else {
                        layout.bottom
                    };
                    self.render_toast(toast_area, toast, buffer);
                }
                hits
            }
        }
    }

    fn render_sidebar(&self, layout: ShellLayout, buffer: &mut Buffer, hits: &mut HitMap) {
        hits.sidebar_split = layout.sidebar_divider;
        let content = Rect::new(
            layout.sidebar.x,
            layout.sidebar.y,
            layout.sidebar.width.saturating_sub(1),
            layout.sidebar.height,
        );
        hits.sidebar_files = Rect::new(
            content.x,
            content.y,
            content.width,
            content.height.saturating_sub(1),
        );
        set_style(
            buffer,
            content,
            Style::default().bg(self.palette.sidebar_bg),
        );
        for y in layout.sidebar_divider.y..layout.sidebar_divider.bottom() {
            put_text(
                buffer,
                layout.sidebar_divider.x,
                y,
                1,
                "│",
                Style::default().fg(if self.state.sidebar_dragging {
                    self.palette.accent
                } else {
                    self.palette.surface_dim
                }),
            );
        }
        put_text(
            buffer,
            content.x,
            content.y,
            content.width,
            " files",
            Style::default()
                .fg(self.palette.overlay0)
                .add_modifier(Modifier::BOLD),
        );
        hits.sidebar_find = Rect::new(
            content.right().saturating_sub(4.min(content.width)),
            content.y,
            4.min(content.width),
            1,
        );
        put_right_text(
            buffer,
            content,
            content.y,
            "find",
            Style::default()
                .fg(self.palette.overlay0)
                .add_modifier(Modifier::DIM),
        );
        if content.height < 2 {
            return;
        }
        let footer_y = content.bottom() - 1;
        hits.sidebar_new = Rect::new(content.x, footer_y, 5.min(content.width), 1);
        let available = self.state.update_available();
        let menu_width = if available { 7 } else { 5 };
        hits.sidebar_menu = Rect::new(
            content
                .right()
                .saturating_sub(menu_width.min(content.width)),
            footer_y,
            menu_width.min(content.width),
            1,
        );
        put_text(
            buffer,
            hits.sidebar_new.x,
            footer_y,
            hits.sidebar_new.width,
            " new",
            Style::default().fg(self.palette.overlay0),
        );
        put_right_text(
            buffer,
            content,
            footer_y,
            if available { "● menu" } else { "menu" },
            Style::default().fg(if available {
                self.palette.accent
            } else {
                self.palette.overlay0
            }),
        );

        let visible_rows = usize::from(content.height.saturating_sub(2));
        let selected = self
            .state
            .file_tree
            .iter()
            .position(|entry| entry.selected)
            .unwrap_or(0);
        let scroll = selected.saturating_sub(visible_rows.saturating_sub(1));
        for (visible_index, entry) in self
            .state
            .file_tree
            .iter()
            .skip(scroll)
            .take(visible_rows)
            .enumerate()
        {
            let row = Rect::new(
                content.x,
                content.y + 1 + visible_index as u16,
                content.width,
                1,
            );
            hits.sidebar_rows
                .push((row, SidebarTarget::File(entry.workspace_index)));
            let background = self.palette.sidebar_bg;
            let base = if !entry.editable {
                Style::default()
                    .fg(self.palette.overlay0)
                    .bg(background)
                    .add_modifier(Modifier::DIM)
            } else {
                Style::default().fg(self.palette.subtext0).bg(background)
            };
            put_text(buffer, row.x, row.y, 1, " ", base);
            put_text(
                buffer,
                row.x + 1,
                row.y,
                1,
                if entry.active { "●" } else { " " },
                Style::default()
                    .fg(if entry.active {
                        self.palette.accent
                    } else {
                        self.palette.subtext0
                    })
                    .bg(background),
            );
            let suffix = if entry.dirty { "*" } else { "" };
            let available = usize::from(row.width.saturating_sub(3 + suffix.len() as u16));
            let name = truncate_to_width(&display::label(&entry.path, 4096), available);
            put_text(
                buffer,
                row.x + 3,
                row.y,
                row.width.saturating_sub(3),
                &format!("{name}{suffix}"),
                base,
            );
        }
    }

    fn render_pane_divider(&self, layout: ShellLayout, buffer: &mut Buffer, hits: &mut HitMap) {
        if layout.gap.is_empty() {
            return;
        }
        hits.pane_split = Rect::new(
            layout.sidebar_divider.x,
            layout.gap.y,
            layout.gap.width.saturating_add(1),
            1,
        );
        let style = Style::default().fg(if self.state.bottom_pane_focused {
            self.palette.accent
        } else {
            self.palette.surface_dim
        });
        put_text(
            buffer,
            layout.sidebar_divider.x,
            layout.gap.y,
            1,
            "├",
            style,
        );
        for x in layout.gap.x..layout.gap.right() {
            put_text(buffer, x, layout.gap.y, 1, "─", style);
        }
    }

    fn render_tab_bar(&self, area: Rect, buffer: &mut Buffer, hits: &mut HitMap) {
        let panel = Style::default()
            .fg(self.palette.overlay1)
            .bg(self.palette.panel_bg);
        set_style(buffer, area, panel);
        let cursor = self.editor.cursor();
        let position = format!("Ln {}, Col {}", cursor.line + 1, cursor.display_column + 1);
        let position = display::label(&position, 128);
        let new_tab_width: u16 = 3;
        let desired_tabs_width = self
            .state
            .buffers
            .iter()
            .fold(new_tab_width, |total, entry| {
                let label = tab_label(&entry.path, entry.dirty);
                total.saturating_add(display_width(&label).max(4) as u16 + 5)
            });
        let position_width = (display_width(&position) as u16).min(
            area.width
                .saturating_sub(desired_tabs_width)
                .saturating_sub(1),
        );
        let position = truncate_to_width(&position, usize::from(position_width));
        let status_width = display_width(&position) as u16;
        let status_area = Rect::new(
            area.right().saturating_sub(status_width),
            area.y,
            status_width,
            1,
        );
        put_text(
            buffer,
            status_area.x,
            area.y,
            status_area.width,
            &position,
            panel,
        );
        let tabs_right = status_area.x.saturating_sub(u16::from(status_width > 0));
        let tab_widths = self
            .state
            .buffers
            .iter()
            .map(|entry| {
                let label = tab_label(&entry.path, entry.dirty);
                (display_width(&label).max(4) as u16 + 4).max(8)
            })
            .collect::<Vec<_>>();
        let available = tabs_right
            .saturating_sub(area.x)
            .saturating_sub(new_tab_width);
        let all_tabs_width = tab_widths
            .iter()
            .copied()
            .fold(0_u16, u16::saturating_add)
            .saturating_add(tab_widths.len().saturating_sub(1) as u16);
        let active = self
            .state
            .buffers
            .iter()
            .position(|entry| entry.active)
            .unwrap_or_default()
            .min(self.state.buffers.len().saturating_sub(1));
        let (mut start, mut end, mut window_width) = if tab_widths.is_empty() {
            (0, 0, 0)
        } else if all_tabs_width <= available {
            (0, tab_widths.len(), all_tabs_width)
        } else {
            (active, active + 1, tab_widths[active])
        };
        if !tab_widths.is_empty() && all_tabs_width > available {
            let fits = |candidate_width: u16, candidate_start: usize, candidate_end: usize| {
                let left_marker = u16::from(candidate_start > 0) * 3;
                let right_marker = u16::from(candidate_end < tab_widths.len()) * 4;
                candidate_width
                    <= available
                        .saturating_sub(left_marker)
                        .saturating_sub(right_marker)
            };
            let mut try_right = true;
            loop {
                let mut added = false;
                for right in [try_right, !try_right] {
                    if right && end < tab_widths.len() {
                        let candidate = window_width
                            .saturating_add(1)
                            .saturating_add(tab_widths[end]);
                        if fits(candidate, start, end + 1) {
                            end += 1;
                            window_width = candidate;
                            added = true;
                            break;
                        }
                    } else if !right && start > 0 {
                        let candidate = window_width
                            .saturating_add(1)
                            .saturating_add(tab_widths[start - 1]);
                        if fits(candidate, start - 1, end) {
                            start -= 1;
                            window_width = candidate;
                            added = true;
                            break;
                        }
                    }
                }
                if !added {
                    break;
                }
                try_right = !try_right;
            }
        }
        let hides_left = start > 0;
        let hides_right = end < self.state.buffers.len();
        let mut x = area.x;
        if hides_left {
            hits.tab_scroll_left = Rect::new(x, area.y, 3, 1);
            put_text(buffer, x, area.y, 3, " < ", panel);
            x += 3;
        }
        let right_adornment_width = u16::from(hides_right) * 4;
        let tabs_end = tabs_right
            .saturating_sub(new_tab_width)
            .saturating_sub(right_adornment_width);
        for (index, entry) in self.state.buffers.iter().enumerate().take(end).skip(start) {
            let label = tab_label(&entry.path, entry.dirty);
            let desired = tab_widths[index];
            let width = desired.min(tabs_end.saturating_sub(x));
            if width < 4 {
                break;
            }
            let rect = Rect::new(x, area.y, width, 1);
            let style = if entry.active {
                Style::default()
                    .fg(self.palette.tab_active_fg)
                    .bg(self.palette.tab_active_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(self.palette.overlay1)
                    .bg(self.palette.surface0)
                    .add_modifier(Modifier::DIM)
            };
            set_style(buffer, rect, style);
            let clipped = truncate_to_width(&label, usize::from(width.saturating_sub(2)));
            let clipped_width = display_width(&clipped) as u16;
            put_text(
                buffer,
                rect.x + rect.width.saturating_sub(clipped_width) / 2,
                rect.y,
                clipped_width.min(rect.width),
                &clipped,
                style,
            );
            hits.tab_pills.push((rect, entry.path.clone()));
            x = rect.right().saturating_add(1);
            if width < desired {
                break;
            }
        }
        if hides_right {
            let right = tabs_right.saturating_sub(new_tab_width + 3);
            hits.tab_scroll_right = Rect::new(right, area.y, 3, 1);
            put_text(buffer, right, area.y, 3, " > ", panel);
            put_text(
                buffer,
                right.saturating_sub(1),
                area.y,
                1,
                "…",
                Style::default().fg(self.palette.overlay0),
            );
        }
        hits.new_tab = Rect::new(tabs_right.saturating_sub(new_tab_width), area.y, 3, 1);
        put_text(buffer, hits.new_tab.x, area.y, 3, " + ", panel);
    }

    fn render_editor(&self, layout: ShellLayout, buffer: &mut Buffer, hits: &mut HitMap) {
        let source_layout = shell::editor_source_layout(layout.editor, self.editor.line_count());
        let source = source_layout.area;
        let source_clickable =
            self.editor.len_bytes() <= rustrace_workspace::hash::MAX_WORKSPACE_FILE_BYTES as usize;
        if source_clickable {
            hits.editor = shell::EditorHit {
                rect: source,
                top_line: self.viewport.top_line(),
                left_column: self.viewport.left_column(),
            };
            hits.editor_diagnostic_rows
                .extend(self.state.diagnostic_markers.iter().filter_map(|marker| {
                    if !matches!(
                        marker.kind,
                        DiagnosticMarkerKind::Error | DiagnosticMarkerKind::Warning
                    ) {
                        return None;
                    }
                    let diagnostic_index = marker.diagnostic_index()?;
                    let row = marker.line.checked_sub(self.viewport.top_line())?;
                    (row < usize::from(source.height)).then_some((
                        Rect::new(source.x, source.y + row as u16, source.width, 1),
                        diagnostic_index,
                    ))
                }));
        }
        EditorWidget::new(self.editor, self.viewport, self.highlights)
            .with_diagnostic_markers(&self.state.diagnostic_markers)
            .with_live_diagnostics(&self.state.live_diagnostics)
            .with_palette(&self.palette)
            .with_cursor_visible(self.state.editor_focused)
            .render(source, buffer);
        if source_clickable && source_layout.show_scrollbar {
            self.render_scrollbar(layout.editor_scrollbar, buffer, hits);
        }
    }

    fn render_scrollbar(&self, track: Rect, buffer: &mut Buffer, hits: &mut HitMap) {
        hits.editor_scrollbar_track = track;
        let total = self.editor.line_count().max(1);
        let visible = usize::from(track.height).min(total);
        let thumb_len = ((visible * usize::from(track.height)) / total)
            .max(1)
            .min(usize::from(track.height));
        let max_line = total.saturating_sub(visible);
        let max_top = usize::from(track.height).saturating_sub(thumb_len);
        let thumb_top = self
            .viewport
            .top_line()
            .min(max_line)
            .saturating_mul(max_top)
            .checked_div(max_line)
            .unwrap_or_default();
        hits.editor_scrollbar_thumb =
            Rect::new(track.x, track.y + thumb_top as u16, 1, thumb_len as u16);
        for y in track.y..track.bottom() {
            put_text(
                buffer,
                track.x,
                y,
                1,
                "▕",
                Style::default().fg(self.palette.surface_dim),
            );
        }
        for y in hits.editor_scrollbar_thumb.y..hits.editor_scrollbar_thumb.bottom() {
            put_text(
                buffer,
                track.x,
                y,
                1,
                "▐",
                Style::default().fg(self.palette.overlay1),
            );
        }
    }

    fn render_output(&self, area: Rect, buffer: &mut Buffer, hits: &mut HitMap) {
        if area.is_empty() {
            return;
        }
        hits.output = area;
        put_text(
            buffer,
            area.x,
            area.y,
            area.width,
            "output",
            Style::default()
                .fg(self.palette.overlay0)
                .add_modifier(Modifier::BOLD),
        );
        if let Some(message) = &self.state.output_header_message {
            put_text(
                buffer,
                area.x.saturating_add(6),
                area.y,
                area.width.saturating_sub(6),
                " · ",
                Style::default().fg(self.palette.overlay0),
            );
            put_text(
                buffer,
                area.x.saturating_add(9),
                area.y,
                area.width.saturating_sub(9),
                message,
                Style::default()
                    .fg(self.palette.overlay0)
                    .add_modifier(Modifier::DIM),
            );
        }
        let inner = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        );
        let lines = output_lines(
            &self.state.output,
            usize::from(inner.height),
            self.state.output_scroll,
        );
        for (index, (line, diagnostic_index, stream, selected)) in lines.iter().enumerate() {
            let row = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
            if let Some(diagnostic_index) = diagnostic_index {
                hits.output_rows.push((row, *diagnostic_index));
            }
            let mut style = match stream {
                Some(OutputStream::Stdout) => Style::default().fg(self.palette.text),
                Some(OutputStream::Stderr) => Style::default().fg(self.palette.red),
                None => Style::default(),
            };
            if *selected {
                style = style.fg(self.palette.accent).add_modifier(Modifier::BOLD);
            }
            Paragraph::new(line.clone())
                .style(style)
                .render(row, buffer);
        }
    }

    fn render_console(
        &self,
        console: &ConsoleViewState,
        area: Rect,
        buffer: &mut Buffer,
        hits: &mut HitMap,
    ) {
        hits.console = area;
        put_text(
            buffer,
            area.x,
            area.y,
            area.width,
            "console",
            Style::default()
                .fg(self.palette.overlay0)
                .add_modifier(Modifier::BOLD),
        );
        let inner = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        );
        let below = render_console_body(console, inner, buffer);
        if below > 0 {
            let header = Rect::new(area.x, area.y, area.width, 1);
            put_right_text(
                buffer,
                header,
                area.y,
                &format!(
                    "↓ {below} more {} · PgDn",
                    if below == 1 { "line" } else { "lines" }
                ),
                Style::default().fg(self.palette.accent),
            );
        }
    }

    fn render_mode_bar(&self, area: Rect, mode: &ModeBarState, buffer: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let base = Style::default()
            .fg(self.palette.overlay0)
            .bg(self.palette.panel_bg);
        set_style(buffer, area, base);
        let accent = if mode.kind == ModeBarKind::Error {
            self.palette.red
        } else {
            self.palette.accent
        };
        let pill = Style::default()
            .fg(self.palette.panel_contrast_fg())
            .bg(accent)
            .add_modifier(Modifier::BOLD);
        put_text(buffer, area.x, area.y, area.width, mode.kind.label(), pill);
        let mut x = area.x + display_width(mode.kind.label()) as u16;
        if !mode.prompt.is_empty() && x < area.right() {
            put_text(
                buffer,
                x,
                area.y,
                area.right() - x,
                &format!(" {}  ", mode.prompt),
                Style::default()
                    .fg(self.palette.text)
                    .bg(self.palette.panel_bg),
            );
            x = x
                .saturating_add(display_width(&mode.prompt) as u16 + 3)
                .min(area.right());
        }
        if x < area.right() {
            put_text(
                buffer,
                x,
                area.y,
                area.right() - x,
                &format!(
                    " {}",
                    primary_modifier_text_with_ghostty(
                        &mode.hints,
                        self.state.primary_modifier,
                        &self.state.ghostty_key_bindings,
                    )
                ),
                base,
            );
        }
    }

    fn render_idle_mode_bar(&self, area: Rect, buffer: &mut Buffer) {
        let style = Style::default()
            .fg(self.palette.overlay0)
            .bg(self.palette.panel_bg)
            .add_modifier(Modifier::DIM);
        set_style(buffer, area, style);
        put_text(
            buffer,
            area.x,
            area.y,
            area.width,
            &primary_modifier_text_with_ghostty(
                "F1 keybinds · F7 menu · F9 console",
                self.state.primary_modifier,
                &self.state.ghostty_key_bindings,
            ),
            style,
        );
    }

    fn render_command_menu(
        &self,
        layout: ShellLayout,
        selected: usize,
        buffer: &mut Buffer,
        hits: &mut HitMap,
    ) {
        let entries = COMMAND_MENU_ENTRIES
            .iter()
            .enumerate()
            .map(|(index, entry)| match index {
                6 if self.state.update_available() => "Update Rustrace  NEW",
                7 if !self.state.update_state.checks_enabled => "Automatic checks: Off",
                _ => entry,
            })
            .collect::<Vec<_>>();
        let longest = entries
            .iter()
            .map(|entry| display_width(entry))
            .max()
            .unwrap_or(0) as u16;
        let width = longest
            .saturating_add(2)
            .min(layout.editor.right().saturating_sub(layout.sidebar.x));
        let height = COMMAND_MENU_ENTRIES.len() as u16 + 2;
        let anchor = hits.sidebar_menu;
        let area = Rect::new(
            anchor.right().saturating_sub(width).max(layout.sidebar.x),
            anchor.y.saturating_sub(height),
            width,
            height,
        );
        hits.overlay = area;
        Clear.render(area, buffer);
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(self.palette.accent))
            .style(Style::default().bg(self.palette.panel_bg))
            .render(area, buffer);
        for (index, entry) in entries.iter().enumerate() {
            let row = Rect::new(area.x + 1, area.y + 1 + index as u16, area.width - 2, 1);
            let style = if index == selected {
                Style::default()
                    .fg(self.palette.panel_contrast_fg())
                    .bg(self.palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(self.palette.text)
                    .bg(self.palette.panel_bg)
            };
            set_style(buffer, row, style);
            put_text(buffer, row.x, row.y, row.width, entry, style);
            hits.context_menu_rows.push((row, index));
        }
    }

    fn render_editor_context_menu(
        &self,
        layout: ShellLayout,
        menu: EditorContextMenuState,
        buffer: &mut Buffer,
        hits: &mut HitMap,
    ) {
        let longest = EDITOR_CONTEXT_MENU_ENTRIES
            .iter()
            .map(|entry| display_width(entry))
            .max()
            .unwrap_or(0) as u16;
        let width = longest.saturating_add(2).min(layout.editor.width);
        let height = (EDITOR_CONTEXT_MENU_ENTRIES.len() as u16 + 2).min(layout.editor.height);
        let anchor = menu.anchor();
        let area = Rect::new(
            anchor
                .x
                .min(layout.editor.right().saturating_sub(width))
                .max(layout.editor.x),
            anchor
                .y
                .min(layout.editor.bottom().saturating_sub(height))
                .max(layout.editor.y),
            width,
            height,
        );
        hits.overlay = area;
        Clear.render(area, buffer);
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(self.palette.accent))
            .style(Style::default().bg(self.palette.panel_bg))
            .render(area, buffer);
        for (index, entry) in EDITOR_CONTEXT_MENU_ENTRIES.iter().enumerate() {
            let row = Rect::new(area.x + 1, area.y + 1 + index as u16, area.width - 2, 1);
            let enabled = menu.enabled(index);
            let style = if enabled && index == menu.selected() {
                Style::default()
                    .fg(self.palette.panel_contrast_fg())
                    .bg(self.palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else if enabled {
                Style::default()
                    .fg(self.palette.text)
                    .bg(self.palette.panel_bg)
            } else {
                Style::default()
                    .fg(self.palette.overlay0)
                    .bg(self.palette.panel_bg)
                    .add_modifier(Modifier::DIM)
            };
            set_style(buffer, row, style);
            put_text(buffer, row.x, row.y, row.width, entry, style);
            hits.editor_context_menu_rows.push((row, index, enabled));
        }
    }

    fn render_files_context_menu(
        &self,
        layout: ShellLayout,
        menu: FilesContextMenuState,
        buffer: &mut Buffer,
        hits: &mut HitMap,
    ) {
        let longest = FILES_CONTEXT_MENU_ENTRIES
            .iter()
            .map(|entry| display_width(entry))
            .max()
            .unwrap_or(0) as u16;
        let bounds = Rect::new(
            layout.sidebar.x,
            layout.sidebar.y,
            layout.sidebar.width.saturating_sub(1),
            layout.sidebar.height,
        );
        let width = longest.saturating_add(2).min(bounds.width);
        let height = (FILES_CONTEXT_MENU_ENTRIES.len() as u16 + 2).min(bounds.height);
        let anchor = menu.anchor();
        let area = Rect::new(
            anchor
                .x
                .min(bounds.right().saturating_sub(width))
                .max(bounds.x),
            anchor
                .y
                .min(bounds.bottom().saturating_sub(height))
                .max(bounds.y),
            width,
            height,
        );
        hits.overlay = area;
        Clear.render(area, buffer);
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(self.palette.accent))
            .style(Style::default().bg(self.palette.panel_bg))
            .render(area, buffer);
        for (index, entry) in FILES_CONTEXT_MENU_ENTRIES.iter().enumerate() {
            let row = Rect::new(area.x + 1, area.y + 1 + index as u16, area.width - 2, 1);
            let enabled = menu.enabled(index);
            let style = if enabled && index == menu.selected() {
                Style::default()
                    .fg(self.palette.panel_contrast_fg())
                    .bg(self.palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else if enabled {
                Style::default()
                    .fg(self.palette.text)
                    .bg(self.palette.panel_bg)
            } else {
                Style::default()
                    .fg(self.palette.overlay0)
                    .bg(self.palette.panel_bg)
                    .add_modifier(Modifier::DIM)
            };
            set_style(buffer, row, style);
            put_text(buffer, row.x, row.y, row.width, entry, style);
            hits.files_context_menu_rows.push((row, index, enabled));
        }
    }

    fn render_update_panel(
        &self,
        area: Rect,
        managed: bool,
        now: u64,
        buffer: &mut Buffer,
        hits: &mut HitMap,
    ) {
        self.dim(area, buffer);
        let panel = centered(
            area,
            area.width.saturating_sub(4).min(76),
            12.min(area.height.saturating_sub(2)),
        );
        hits.overlay = panel;
        self.render_overlay_frame(panel, "", buffer);
        let inner = Rect::new(
            panel.x + 2,
            panel.y + 1,
            panel.width.saturating_sub(4),
            panel.height.saturating_sub(2),
        );
        let style = Style::default()
            .fg(self.palette.text)
            .bg(self.palette.panel_bg);
        put_text(
            buffer,
            inner.x,
            inner.y,
            inner.width,
            "Update Rustrace",
            style.add_modifier(Modifier::BOLD),
        );
        let body = Rect::new(
            inner.x,
            inner.y + 2,
            inner.width,
            inner.height.saturating_sub(3),
        );
        Paragraph::new(
            self.state
                .update_state
                .panel_lines(env!("CARGO_PKG_VERSION"), managed, now)
                .join("\n"),
        )
        .style(style)
        .wrap(Wrap { trim: false })
        .render(body, buffer);
        hits.overlay_cancel = Rect::new(
            inner.right().saturating_sub(19),
            inner.bottom().saturating_sub(1),
            19.min(inner.width),
            1,
        );
        let close_style = style
            .fg(self.palette.panel_contrast_fg())
            .bg(self.palette.accent)
            .add_modifier(Modifier::BOLD);
        set_style(buffer, hits.overlay_cancel, close_style);
        put_text(
            buffer,
            hits.overlay_cancel.x,
            hits.overlay_cancel.y,
            hits.overlay_cancel.width,
            " esc / enter close ",
            close_style,
        );
    }

    fn render_keybinds_overlay(
        &self,
        area: Rect,
        scroll: usize,
        buffer: &mut Buffer,
        hits: &mut HitMap,
    ) {
        self.dim(area, buffer);
        let width = area.width.saturating_sub(4).min(76);
        let height = area.height.saturating_sub(2).min(22);
        let panel = centered(area, width, height);
        hits.overlay = panel;
        self.render_overlay_frame(panel, "", buffer);
        let inner = Rect::new(
            panel.x + 1,
            panel.y + 1,
            panel.width.saturating_sub(2),
            panel.height.saturating_sub(2),
        );
        if inner.width < 20 || inner.height < 6 {
            return;
        }
        put_text(
            buffer,
            inner.x,
            inner.y,
            inner.width,
            "keybinds",
            Style::default()
                .fg(self.palette.text)
                .bg(self.palette.panel_bg)
                .add_modifier(Modifier::BOLD),
        );
        hits.overlay_cancel = Rect::new(inner.right().saturating_sub(13), inner.y, 13, 1);
        let close_style = Style::default()
            .fg(self.palette.panel_contrast_fg())
            .bg(self.palette.accent)
            .add_modifier(Modifier::BOLD);
        set_style(buffer, hits.overlay_cancel, close_style);
        put_text(
            buffer,
            hits.overlay_cancel.x,
            hits.overlay_cancel.y,
            hits.overlay_cancel.width,
            " esc close ",
            close_style,
        );
        let body = Rect::new(
            inner.x,
            inner.y + 2,
            inner.width,
            inner.height.saturating_sub(3),
        );
        let visible = usize::from(body.height);
        let rows = &self.state.keybind_rows;
        let max_scroll = maximum_keybinds_scroll(area, rows.len());
        let scroll = scroll.min(max_scroll);
        let show_scrollbar = rows.len() > visible;
        let text_width = body.width.saturating_sub(u16::from(show_scrollbar));
        for (index, row) in rows.iter().skip(scroll).take(visible).enumerate() {
            let row = keybinds_modifier_text_with_ghostty(
                row,
                self.state.primary_modifier,
                &self.state.ghostty_key_bindings,
            );
            let heading = !row.contains(' ');
            if heading {
                put_text(
                    buffer,
                    body.x,
                    body.y + index as u16,
                    text_width,
                    &row,
                    Style::default()
                        .fg(self.palette.accent)
                        .bg(self.palette.panel_bg)
                        .add_modifier(Modifier::BOLD),
                );
            } else {
                let split = row.find("  ").unwrap_or(row.len());
                let key = row[..split].trim_end();
                let description = row[split..].trim_start();
                let key_width = 30.min(text_width);
                put_text(
                    buffer,
                    body.x,
                    body.y + index as u16,
                    key_width,
                    key,
                    Style::default()
                        .fg(self.palette.mauve)
                        .bg(self.palette.panel_bg)
                        .add_modifier(Modifier::BOLD),
                );
                put_text(
                    buffer,
                    body.x + key_width,
                    body.y + index as u16,
                    text_width.saturating_sub(key_width),
                    description,
                    Style::default()
                        .fg(self.palette.text)
                        .bg(self.palette.panel_bg),
                );
            }
        }
        if show_scrollbar {
            let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
            let thumb_len = ((visible * visible) / rows.len()).max(1).min(visible);
            let max_top = visible.saturating_sub(thumb_len);
            let thumb_top = scroll
                .saturating_mul(max_top)
                .checked_div(max_scroll)
                .unwrap_or_default();
            hits.keybinds_scrollbar_track = track;
            hits.keybinds_scrollbar_thumb =
                Rect::new(track.x, track.y + thumb_top as u16, 1, thumb_len as u16);
            for y in track.y..track.bottom() {
                put_text(
                    buffer,
                    track.x,
                    y,
                    1,
                    "▐",
                    Style::default()
                        .fg(self.palette.overlay0)
                        .bg(self.palette.panel_bg)
                        .add_modifier(Modifier::DIM),
                );
            }
            for y in track.y + thumb_top as u16..track.y + (thumb_top + thumb_len) as u16 {
                put_text(
                    buffer,
                    track.x,
                    y,
                    1,
                    "▐",
                    Style::default()
                        .fg(self.palette.overlay1)
                        .bg(self.palette.panel_bg),
                );
            }
        }
        put_text(
            buffer,
            inner.x,
            inner.bottom().saturating_sub(1),
            inner.width,
            " scroll ↑↓/pgup/pgdn · close esc/enter ",
            Style::default()
                .fg(self.palette.overlay0)
                .bg(self.palette.panel_bg)
                .add_modifier(Modifier::DIM),
        );
    }

    fn render_test_case_picker(
        &self,
        area: Rect,
        picker: &TestCasePickerState,
        buffer: &mut Buffer,
        hits: &mut HitMap,
    ) {
        self.dim(area, buffer);
        let width = area.width.saturating_sub(4).min(68);
        let height = area.height.saturating_sub(2).min(22);
        let panel = centered(area, width, height);
        hits.overlay = panel;
        self.render_overlay_frame(panel, "", buffer);
        let inner = Rect::new(
            panel.x + 1,
            panel.y + 1,
            panel.width.saturating_sub(2),
            panel.height.saturating_sub(2),
        );
        if inner.width < 24 || inner.height < 6 {
            return;
        }
        let title_style = Style::default()
            .fg(self.palette.text)
            .bg(self.palette.panel_bg)
            .add_modifier(Modifier::BOLD);
        put_text(
            buffer,
            inner.x,
            inner.y,
            inner.width,
            "Test cases",
            title_style,
        );

        let body = Rect::new(
            inner.x,
            inner.y + 2,
            inner.width,
            inner.height.saturating_sub(3),
        );
        if let Some(notice) = &picker.notice {
            put_text(
                buffer,
                body.x,
                body.y,
                body.width,
                notice,
                Style::default()
                    .fg(self.palette.yellow)
                    .bg(self.palette.panel_bg),
            );
        }
        let list = if picker.notice.is_some() {
            Rect::new(
                body.x,
                body.y.saturating_add(1),
                body.width,
                body.height.saturating_sub(1),
            )
        } else {
            body
        };
        let total = picker.rows.len() + 1;
        let visible = usize::from(list.height).max(1);
        let start = picker
            .selected
            .saturating_add(2)
            .saturating_sub(visible)
            .min(total.saturating_sub(visible));
        let show_scrollbar = total > visible;
        let row_width = list.width.saturating_sub(u16::from(show_scrollbar));
        for (visible_index, index) in (start..total).take(visible).enumerate() {
            let row = Rect::new(list.x, list.y + visible_index as u16, row_width, 1);
            hits.test_case_rows.push((row, index));
            let style = if index == picker.selected {
                Style::default()
                    .fg(self.palette.panel_contrast_fg())
                    .bg(self.palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(self.palette.text)
                    .bg(self.palette.panel_bg)
            };
            set_style(buffer, row, style);
            if let Some(case) = picker.rows.get(index) {
                let status_width = (display_width(&display::label(&case.status, 512)) as u16)
                    .min(row.width.saturating_sub(2));
                let name_width = row.width.saturating_sub(status_width.saturating_add(1));
                put_text(buffer, row.x, row.y, name_width, &case.name, style);
                put_right_text(buffer, row, row.y, &case.status, style);
            } else {
                put_text(buffer, row.x, row.y, row.width, "Run all", style);
            }
        }
        if show_scrollbar {
            let track = Rect::new(list.right().saturating_sub(1), list.y, 1, list.height);
            let thumb_len = ((visible * visible) / total).max(1).min(visible);
            let max_top = visible.saturating_sub(thumb_len);
            let max_scroll = total.saturating_sub(visible);
            let thumb_top = start
                .saturating_mul(max_top)
                .checked_div(max_scroll)
                .unwrap_or_default();
            hits.test_case_scrollbar_track = track;
            hits.test_case_scrollbar_thumb =
                Rect::new(track.x, track.y + thumb_top as u16, 1, thumb_len as u16);
            for y in track.y..track.bottom() {
                put_text(
                    buffer,
                    track.x,
                    y,
                    1,
                    "▐",
                    Style::default()
                        .fg(self.palette.overlay0)
                        .bg(self.palette.panel_bg)
                        .add_modifier(Modifier::DIM),
                );
            }
            for y in hits.test_case_scrollbar_thumb.y..hits.test_case_scrollbar_thumb.bottom() {
                put_text(
                    buffer,
                    track.x,
                    y,
                    1,
                    "▐",
                    Style::default()
                        .fg(self.palette.overlay1)
                        .bg(self.palette.panel_bg),
                );
            }
        }
        put_text(
            buffer,
            inner.x,
            inner.bottom().saturating_sub(1),
            inner.width,
            " ↑↓ select · ↵/double-click run · r refresh · esc close ",
            Style::default()
                .fg(self.palette.overlay0)
                .bg(self.palette.panel_bg)
                .add_modifier(Modifier::DIM),
        );
    }

    fn render_file_prompt(
        &self,
        area: Rect,
        prompt: &FilePromptState,
        buffer: &mut Buffer,
        hits: &mut HitMap,
    ) {
        self.dim(area, buffer);
        let panel = centered(area, area.width.min(56), area.height.min(7));
        hits.overlay = panel;
        self.render_overlay_frame(panel, "", buffer);
        let inner = Rect::new(
            panel.x + 1,
            panel.y + 1,
            panel.width.saturating_sub(2),
            panel.height.saturating_sub(2),
        );
        if inner.width < 24 || inner.height < 5 {
            return;
        }

        let (title, submit) = match prompt.kind {
            FilePromptKind::Create => ("new file", " ↵ create "),
            FilePromptKind::Rename => ("rename file", " ↵ rename "),
        };
        put_text(
            buffer,
            inner.x,
            inner.y,
            inner.width,
            title,
            Style::default()
                .fg(self.palette.text)
                .bg(self.palette.panel_bg)
                .add_modifier(Modifier::BOLD),
        );

        let input_area = Rect::new(inner.x, inner.y + 2, inner.width, 1);
        let input_style = Style::default()
            .fg(self.palette.text)
            .bg(self.palette.surface0);
        set_style(buffer, input_area, input_style);
        match prompt.kind {
            FilePromptKind::Create => {
                let prefix_width = 4.min(input_area.width);
                put_text(
                    buffer,
                    input_area.x,
                    input_area.y,
                    prefix_width,
                    "src/",
                    input_style.add_modifier(Modifier::DIM),
                );
                let field = Rect::new(
                    input_area.x.saturating_add(prefix_width),
                    input_area.y,
                    input_area.width.saturating_sub(prefix_width),
                    1,
                );
                let input = format!("{}▏", display::label(&prompt.input, 4096));
                let horizontal = display_width(&input)
                    .saturating_sub(usize::from(field.width))
                    .min(u16::MAX as usize) as u16;
                Paragraph::new(input)
                    .style(input_style)
                    .scroll((0, horizontal))
                    .render(field, buffer);
            }
            FilePromptKind::Rename => {
                let input = format!(" {}▏", display::label(&prompt.input, 4096));
                let horizontal = display_width(&input)
                    .saturating_sub(usize::from(input_area.width))
                    .min(u16::MAX as usize) as u16;
                Paragraph::new(input)
                    .style(input_style)
                    .scroll((0, horizontal))
                    .render(input_area, buffer);
            }
        }

        let actions_width = 24;
        let actions_x = inner.x + inner.width.saturating_sub(actions_width) / 2;
        let actions_y = inner.y + 4;
        hits.overlay_confirm = Rect::new(actions_x, actions_y, 10, 1);
        hits.overlay_cancel = Rect::new(actions_x + 12, actions_y, 12, 1);
        let submit_style = Style::default()
            .fg(self.palette.panel_contrast_fg())
            .bg(self.palette.accent)
            .add_modifier(Modifier::BOLD);
        let cancel_style = Style::default()
            .fg(self.palette.text)
            .bg(self.palette.surface0);
        put_text(
            buffer,
            hits.overlay_confirm.x,
            hits.overlay_confirm.y,
            hits.overlay_confirm.width,
            submit,
            submit_style,
        );
        put_text(
            buffer,
            hits.overlay_cancel.x,
            hits.overlay_cancel.y,
            hits.overlay_cancel.width,
            " esc cancel ",
            cancel_style,
        );
    }

    fn render_find_panel(
        &self,
        area: Rect,
        state: &FindPanelState,
        buffer: &mut Buffer,
        hits: &mut HitMap,
    ) {
        self.dim(area, buffer);
        let panel = centered(area, area.width.min(56), area.height.min(9));
        hits.overlay = panel;
        self.render_overlay_frame(panel, "", buffer);
        let inner = Rect::new(
            panel.x + 1,
            panel.y + 1,
            panel.width.saturating_sub(2),
            panel.height.saturating_sub(2),
        );
        if inner.width < 44 || inner.height < 7 {
            return;
        }

        put_text(
            buffer,
            inner.x,
            inner.y,
            inner.width,
            "find and replace",
            Style::default()
                .fg(self.palette.text)
                .bg(self.palette.panel_bg)
                .add_modifier(Modifier::BOLD),
        );
        put_right_text(
            buffer,
            inner,
            inner.y,
            &state.counter,
            Style::default()
                .fg(self.palette.overlay0)
                .bg(self.palette.panel_bg)
                .add_modifier(Modifier::DIM),
        );
        self.render_find_field(
            Rect::new(inner.x, inner.y + 2, inner.width, 1),
            " find ",
            &state.find,
            state.active == FindPanelField::Find,
            buffer,
        );
        self.render_find_field(
            Rect::new(inner.x, inner.y + 4, inner.width, 1),
            " replace ",
            &state.replace,
            state.active == FindPanelField::Replace,
            buffer,
        );

        let actions_x = inner.x + inner.width.saturating_sub(44) / 2;
        let actions_y = inner.y + 6;
        hits.find_next = Rect::new(actions_x, actions_y, 8, 1);
        hits.find_replace = Rect::new(actions_x + 9, actions_y, 9, 1);
        hits.find_replace_all = Rect::new(actions_x + 19, actions_y, 13, 1);
        hits.find_close = Rect::new(actions_x + 33, actions_y, 11, 1);
        let primary = Style::default()
            .fg(self.palette.panel_contrast_fg())
            .bg(self.palette.accent)
            .add_modifier(Modifier::BOLD);
        let secondary = Style::default()
            .fg(self.palette.text)
            .bg(self.palette.surface0);
        for (rect, label, style) in [
            (hits.find_next, " ↵ next ", primary),
            (hits.find_replace, " replace ", secondary),
            (hits.find_replace_all, " replace all ", secondary),
            (hits.find_close, " esc close ", secondary),
        ] {
            put_text(buffer, rect.x, rect.y, rect.width, label, style);
        }
    }

    fn render_find_field(
        &self,
        area: Rect,
        label: &str,
        value: &str,
        active: bool,
        buffer: &mut Buffer,
    ) {
        let style = Style::default()
            .fg(self.palette.text)
            .bg(self.palette.surface0);
        set_style(buffer, area, style);
        let label_width = display_width(label).min(usize::from(area.width)) as u16;
        put_text(
            buffer,
            area.x,
            area.y,
            label_width,
            label,
            style.add_modifier(Modifier::DIM),
        );
        let field = Rect::new(
            area.x + label_width,
            area.y,
            area.width.saturating_sub(label_width),
            1,
        );
        let input = format!(
            "{}{}",
            display::label(value, 4096),
            if active { "▏" } else { "" }
        );
        let horizontal = display_width(&input)
            .saturating_sub(usize::from(field.width))
            .min(u16::MAX as usize) as u16;
        Paragraph::new(input)
            .style(style)
            .scroll((0, horizontal))
            .render(field, buffer);
    }

    fn render_confirmation(
        &self,
        area: Rect,
        confirmation: &ConfirmationState,
        buffer: &mut Buffer,
        hits: &mut HitMap,
    ) {
        self.dim(area, buffer);
        let panel = centered(area, area.width.min(52), area.height.min(7));
        hits.overlay = panel;
        self.render_overlay_frame(panel, " confirm ", buffer);
        let message_area = Rect::new(panel.x + 2, panel.y + 2, panel.width.saturating_sub(4), 1);
        put_text(
            buffer,
            message_area.x,
            message_area.y,
            message_area.width,
            &confirmation.message,
            Style::default()
                .fg(self.palette.text)
                .bg(self.palette.panel_bg),
        );
        let actions_y = panel.bottom().saturating_sub(2);
        hits.overlay_confirm = Rect::new(panel.x + 2, actions_y, 11, 1);
        hits.overlay_cancel = Rect::new(panel.right().saturating_sub(14), actions_y, 12, 1);
        let confirm_style = Style::default()
            .fg(self.palette.panel_contrast_fg())
            .bg(self.palette.accent)
            .add_modifier(Modifier::BOLD);
        let cancel_style = Style::default()
            .fg(self.palette.text)
            .bg(self.palette.surface0);
        put_text(
            buffer,
            hits.overlay_confirm.x,
            actions_y,
            hits.overlay_confirm.width,
            " ↵ confirm ",
            confirm_style,
        );
        put_text(
            buffer,
            hits.overlay_cancel.x,
            actions_y,
            hits.overlay_cancel.width,
            " esc cancel ",
            cancel_style,
        );
    }

    fn render_toast(&self, area: Rect, toast: &ToastState, buffer: &mut Buffer) {
        let body = if toast
            .body
            .starts_with(crate::session::PASTE_BLOCKED_WARNING)
        {
            toast.body.clone()
        } else {
            primary_modifier_text_with_ghostty(
                &toast.body,
                self.state.primary_modifier,
                &self.state.ghostty_key_bindings,
            )
        };
        let lines = display::output(
            body.as_bytes(),
            display::Limits {
                lines: 4,
                ..display::Limits::default()
            },
        );
        let title = display::label(&toast.title, 128);
        let content_width = lines
            .text
            .lines
            .iter()
            .map(Line::width)
            .chain(std::iter::once(display_width(&title)))
            .max()
            .unwrap_or(0)
            .min(usize::from(area.width.saturating_sub(4))) as u16;
        let width = content_width.saturating_add(4).max(24).min(area.width);
        let body_height =
            wrapped_line_count(&lines.text.lines, usize::from(width.saturating_sub(4)));
        let paragraph = Paragraph::new(lines.text)
            .style(
                Style::default()
                    .fg(self.palette.text)
                    .bg(self.palette.panel_bg),
            )
            .wrap(Wrap { trim: true });
        let height = u16::try_from(body_height)
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .max(3)
            .min(area.height);
        let toast_area = Rect::new(
            area.right().saturating_sub(width),
            area.bottom().saturating_sub(height),
            width,
            height,
        );
        Clear.render(toast_area, buffer);
        let color = match toast.kind {
            ToastKind::Info => self.palette.blue,
            ToastKind::Success => self.palette.green,
            ToastKind::Warning => self.palette.yellow,
            ToastKind::Error => self.palette.red,
        };
        Block::default()
            .title(Line::from(vec![
                Span::raw(" "),
                Span::styled("●", Style::default().fg(color)),
                Span::raw(" "),
                Span::styled(
                    title,
                    Style::default()
                        .fg(self.palette.text)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ]))
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(self.palette.overlay0))
            .style(Style::default().bg(self.palette.panel_bg))
            .render(toast_area, buffer);
        paragraph.render(
            Rect::new(
                toast_area.x + 2,
                toast_area.y + 1,
                toast_area.width.saturating_sub(4),
                toast_area.height.saturating_sub(2),
            ),
            buffer,
        );
    }

    fn dim(&self, area: Rect, buffer: &mut Buffer) {
        set_style(buffer, area, Style::default().add_modifier(Modifier::DIM));
    }

    fn render_overlay_frame(&self, area: Rect, title: &str, buffer: &mut Buffer) {
        Clear.render(area, buffer);
        Block::default()
            .title(display::label(title, 128))
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(self.palette.accent))
            .style(Style::default().bg(self.palette.panel_bg))
            .render(area, buffer);
    }

    fn render_too_small(&self, area: Rect, buffer: &mut Buffer, error: Option<&str>) {
        let message = if let Some(error) = error {
            format!(
                "ERROR\nRecovery required\n{}\nTerminal too small\nNeed {MIN_TERMINAL_WIDTH}x{MIN_TERMINAL_HEIGHT}",
                display::label(error, 1024)
            )
        } else {
            format!(
                "Terminal too small\nNeed {MIN_TERMINAL_WIDTH}x{MIN_TERMINAL_HEIGHT}; got {}x{}",
                area.width, area.height
            )
        };
        Paragraph::new(message)
            .style(
                Style::default()
                    .fg(if error.is_some() {
                        self.palette.red
                    } else {
                        self.palette.yellow
                    })
                    .add_modifier(Modifier::BOLD),
            )
            .render(area, buffer);
    }
}

fn wrapped_line_count(lines: &[Line<'_>], width: usize) -> usize {
    if width == 0 {
        return 0;
    }
    lines
        .iter()
        .map(|line| {
            let text = line.to_string();
            let mut words = text.split_whitespace();
            let Some(first) = words.next() else {
                return 1;
            };
            let first_width = display_width(first);
            let mut rows = first_width.div_ceil(width).max(1);
            let mut used = first_width % width;
            if used == 0 {
                used = width;
            }
            for word in words {
                let word_width = display_width(word);
                if used.saturating_add(1).saturating_add(word_width) <= width {
                    used += 1 + word_width;
                } else {
                    rows += word_width.div_ceil(width).max(1);
                    used = word_width % width;
                    if used == 0 {
                        used = width;
                    }
                }
            }
            rows
        })
        .sum()
}

fn tab_label(path: &str, dirty: bool) -> String {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    format!(
        "{}{}",
        display::label(name, 4096),
        if dirty { "*" } else { "" }
    )
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width.min(area.width),
        height.min(area.height),
    )
}

fn output_lines(
    output: &[OutputRow],
    maximum_rows: usize,
    scroll: usize,
) -> Vec<(Line<'static>, Option<usize>, Option<OutputStream>, bool)> {
    let mut lines = Vec::new();
    let mut limits = display::Limits {
        lines: display::MAX_LINES,
        ..display::Limits::default()
    };
    for output in output.iter().take(display::MAX_LINES) {
        if lines.len() >= display::MAX_LINES {
            break;
        }
        let rendered = display::output(output.text.as_bytes(), limits);
        limits.input_bytes = limits.input_bytes.saturating_sub(rendered.input_bytes);
        limits.output_bytes = limits.output_bytes.saturating_sub(rendered.output_bytes);
        limits.spans = limits.spans.saturating_sub(rendered.spans);
        lines.extend(
            rendered
                .text
                .lines
                .into_iter()
                .map(trim_line_start)
                .map(|line| {
                    (
                        line,
                        output.diagnostic_index,
                        output.stream,
                        output.selected,
                    )
                }),
        );
        limits.lines = display::MAX_LINES.saturating_sub(lines.len());
        if rendered.truncated {
            break;
        }
    }
    let start = scroll.min(lines.len().saturating_sub(maximum_rows));
    lines.into_iter().skip(start).take(maximum_rows).collect()
}

fn trim_line_start(mut line: Line<'static>) -> Line<'static> {
    let mut trimming = true;
    for span in &mut line.spans {
        if !trimming {
            break;
        }
        let trimmed = span.content.trim_start_matches(char::is_whitespace);
        if trimmed.len() != span.content.len() {
            span.content = trimmed.to_owned().into();
        }
        trimming = span.content.is_empty();
    }
    line.spans.retain(|span| !span.content.is_empty());
    line
}

pub(crate) fn maximum_output_scroll(output: &[OutputRow], visible_rows: usize) -> usize {
    output_lines(output, display::MAX_LINES, 0)
        .len()
        .saturating_sub(visible_rows)
}

/// The console's output rows inside the console pane: below the header row
/// and above the prompt row.
pub(crate) fn console_output_area(console: Rect) -> Rect {
    Rect::new(
        console.x,
        console.y.saturating_add(1),
        console.width,
        console.height.saturating_sub(2),
    )
}

// Returns the number of source lines not fully shown below the window.
fn render_console_body(console: &ConsoleViewState, area: Rect, buffer: &mut Buffer) -> usize {
    if area.is_empty() {
        return 0;
    }
    let prompt = console.prompt.as_slice();
    let prompt_area = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    let output_area = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
    let rows = console
        .rows
        .clone()
        .filter(|rows| rows.width == output_area.width)
        .unwrap_or_else(|| Rc::new(ConsoleRows::new(&console.output, 0, output_area.width)));
    let visible = usize::from(output_area.height);
    let last_top = rows.len().saturating_sub(visible);
    let top = console.scroll.map_or(last_top, |top| top.min(last_top));
    let end = rows.len().min(top + visible);
    let below = if end < rows.len() {
        rows.lines_below(end)
    } else {
        0
    };
    Paragraph::new(rows.rows[top..end].to_vec()).render(output_area, buffer);

    let cursor = console.cursor.filter(|cursor| *cursor <= prompt.len());
    let cursor_column = cursor.map(|cursor| {
        display::output(
            &prompt[..cursor],
            display::Limits {
                lines: 1,
                ..display::Limits::default()
            },
        )
        .text
        .lines
        .first()
        .map_or(0, Line::width)
    });
    let mut prompt = prompt.to_vec();
    if let Some(cursor) = cursor {
        prompt.splice(cursor..cursor, "▏".bytes());
    }
    let rendered = display::output(
        &prompt,
        display::Limits {
            lines: 1,
            ..display::Limits::default()
        },
    );
    let horizontal = cursor_column
        .unwrap_or(0)
        .saturating_sub(usize::from(prompt_area.width).saturating_sub(1))
        .min(u16::MAX as usize) as u16;
    Paragraph::new(rendered.text)
        .scroll((0, horizontal))
        .render(prompt_area, buffer);
    below
}

// Scrollback is bounded below the live capture so a rolling command keeps a
// stable top marker and each rebuild decodes a fixed amount of text.
const CONSOLE_SCROLLBACK_BYTES: usize = 128 * 1024;
const CONSOLE_OMITTED: &str = "[older console output omitted]";
// A longer line shows its newest bytes, so a newline-free stream stays live.
const CONSOLE_LINE_TAIL_BYTES: usize = 4 * 1024;
const CONSOLE_LINE_OMITTED: &str = "[line start omitted] ";

/// Console output wrapped at the pane width, with each source line's absolute
/// output offset so a scroll position survives new output and width changes.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct ConsoleRows {
    width: u16,
    rows: Vec<Line<'static>>,
    // (absolute offset of a source line, its first row), in output order.
    lines: Vec<(u64, usize)>,
}

/// A scroll position: a source line's absolute offset and a row within it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ConsoleAnchor {
    line: u64,
    row: usize,
}

impl ConsoleRows {
    /// `start` is the absolute output offset of `output[0]`.
    pub(crate) fn new(output: &[u8], start: u64, width: u16) -> Self {
        let mut result = Self {
            width,
            ..Self::default()
        };
        if width == 0 {
            return result;
        }
        let mut offset = 0;
        let mut lines = Vec::new();
        let mut starts = Vec::new();
        if output.len() > CONSOLE_SCROLLBACK_BYTES {
            let cut = output.len() - CONSOLE_SCROLLBACK_BYTES;
            offset = output[cut..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(cut, |index| cut + index + 1);
            lines.push(Line::from(CONSOLE_OMITTED));
            starts.push(None);
        }
        let rest = &output[offset..];
        let rest = rest.strip_suffix(b"\n").unwrap_or(rest);
        if !rest.is_empty() {
            for line in rest.split(|byte| *byte == b'\n') {
                starts.push(Some(
                    start.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX)),
                ));
                offset += line.len() + 1;
                lines.push(console_line(line));
            }
        }
        dedent_cargo_console_blocks(&mut lines);
        for (line, start) in lines.into_iter().zip(starts) {
            if let Some(start) = start {
                result.lines.push((start, result.rows.len()));
            }
            result
                .rows
                .extend(wrap_console_line(line, usize::from(width)));
        }
        result
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn anchor(&self, top: usize) -> ConsoleAnchor {
        let index = self.lines.partition_point(|(_, first)| *first <= top);
        index
            .checked_sub(1)
            .map_or_else(ConsoleAnchor::default, |index| {
                let (line, first) = self.lines[index];
                ConsoleAnchor {
                    line,
                    row: top - first,
                }
            })
    }

    /// The anchor's row; a line that left the scrollback resolves to the top.
    pub(crate) fn resolve(&self, anchor: ConsoleAnchor) -> usize {
        match self
            .lines
            .binary_search_by_key(&anchor.line, |(line, _)| *line)
        {
            Ok(index) => {
                let first = self.lines[index].1;
                let end = self
                    .lines
                    .get(index + 1)
                    .map_or(self.rows.len(), |(_, next)| *next);
                first + anchor.row.min(end.saturating_sub(first + 1))
            }
            Err(_) => 0,
        }
    }

    /// Keeps a pin whose line is still in the scrollback; otherwise pins the
    /// oldest remaining line, so the window never parks on the omitted marker.
    pub(crate) fn retain(&self, anchor: ConsoleAnchor) -> ConsoleAnchor {
        if self
            .lines
            .binary_search_by_key(&anchor.line, |(line, _)| *line)
            .is_ok()
        {
            return anchor;
        }
        self.lines
            .first()
            .map_or(anchor, |(line, _)| ConsoleAnchor {
                line: *line,
                row: 0,
            })
    }

    // Source lines with a row at or after `row`.
    fn lines_below(&self, row: usize) -> usize {
        let starting_after = self.lines.partition_point(|(_, first)| *first < row);
        let previous_end = self
            .lines
            .get(starting_after)
            .map_or(self.rows.len(), |(_, first)| *first);
        let continued = starting_after > 0 && previous_end > row;
        self.lines.len() - starting_after + usize::from(continued)
    }
}

fn console_line(line: &[u8]) -> Line<'static> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let (prefix, line) = if line.len() > CONSOLE_LINE_TAIL_BYTES {
        let mut cut = line.len() - CONSOLE_LINE_TAIL_BYTES;
        // Begin on a UTF-8 lead byte: skip at most three continuation bytes.
        for _ in 0..3 {
            if line.get(cut).is_some_and(|byte| byte & 0xc0 == 0x80) {
                cut += 1;
            }
        }
        (Some(CONSOLE_LINE_OMITTED), &line[cut..])
    } else {
        (None, line)
    };
    let mut rendered = display::output(
        line,
        display::Limits {
            lines: 1,
            ..display::Limits::default()
        },
    )
    .text
    .lines
    .into_iter()
    .next()
    .unwrap_or_default();
    if let Some(prefix) = prefix {
        rendered.spans.insert(0, Span::raw(prefix));
    }
    rendered
}

fn wrap_console_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if line.width() <= width {
        return vec![line];
    }
    let style = line.style;
    let mut rows = Vec::new();
    let mut row = Line::default().style(style);
    let mut column = 0;
    for span in line.spans {
        let mut content = String::new();
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = display_width(grapheme);
            if column > 0 && column + grapheme_width > width {
                if !content.is_empty() {
                    row.spans
                        .push(Span::styled(std::mem::take(&mut content), span.style));
                }
                rows.push(std::mem::replace(&mut row, Line::default().style(style)));
                column = 0;
            }
            content.push_str(grapheme);
            column += grapheme_width;
        }
        if !content.is_empty() {
            row.spans.push(Span::styled(content, span.style));
        }
    }
    rows.push(row);
    rows
}

// Status lines are standalone blocks. Diagnostic blocks end before a status
// or after a blank separator, which does not lower the block's minimum.
// Keep relative source/gutter/caret columns and each safe-display span's style.
fn dedent_cargo_console_blocks(lines: &mut [Line<'_>]) {
    let mut start = 0;
    while start < lines.len() {
        if !is_cargo_console_line(&lines[start].to_string()) {
            start += 1;
            continue;
        }
        let mut end = start;
        let mut indentation = usize::MAX;
        while end < lines.len() {
            let content = lines[end].to_string();
            if content.trim().is_empty() {
                end += 1;
                break;
            }
            if !is_cargo_console_line(&content) {
                break;
            }
            let status_line = is_cargo_console_status_line(&content);
            if end > start && status_line {
                break;
            }
            let leading = &content[..content.len() - content.trim_start().len()];
            indentation = indentation.min(display_width(leading));
            end += 1;
            if status_line {
                break;
            }
        }
        for line in &mut lines[start..end] {
            let mut remaining = indentation;
            for span in &mut line.spans {
                if remaining == 0 {
                    break;
                }
                let mut offset = 0;
                let mut padding = 0;
                for (index, character) in span.content.char_indices() {
                    if !character.is_whitespace() || remaining == 0 {
                        break;
                    }
                    let width = display_width(&character.to_string());
                    offset = index + character.len_utf8();
                    // A wide whitespace character can straddle the dedent edge.
                    padding = width.saturating_sub(remaining);
                    remaining = remaining.saturating_sub(width);
                }
                span.content = (" ".repeat(padding) + &span.content[offset..]).into();
                if !span.content.is_empty() {
                    break;
                }
            }
        }
        start = end;
    }
}

// LiveOutput merges stdout and stderr. Match Cargo-shaped lines on both streams
// only at render time, after safe display decoding; raw evidence stays intact.
fn is_cargo_console_status_line(line: &str) -> bool {
    let token = line.split_whitespace().next().unwrap_or_default();
    matches!(
        token,
        "Compiling"
            | "Checking"
            | "Finished"
            | "Generated"
            | "Running"
            | "Documenting"
            | "Downloading"
            | "Downloaded"
            | "Updating"
            | "Adding"
            | "Removing"
            | "Locking"
            | "Blocking"
            | "Fresh"
            | "Dirty"
            | "Building"
            | "Installing"
            | "Installed"
            | "Ignored"
            | "Packaging"
            | "Uploading"
            | "Warning"
            | "Error"
    )
}

fn is_cargo_console_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if is_cargo_console_status_line(trimmed) {
        return true;
    }
    if ["warning:", "error:", "note:", "help:", "-->"]
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        return true;
    }
    if let Some(code) = trimmed.strip_prefix("error[")
        && code
            .split_once("]:")
            .is_some_and(|(code, _)| !code.is_empty())
    {
        return true;
    }
    // Rust's source frames have a blank or numeric gutter before the pipe.
    trimmed.split_once('|').is_some_and(|(gutter, _)| {
        gutter.trim().bytes().all(|byte| byte.is_ascii_digit())
            && (gutter.is_empty() || gutter.ends_with(' '))
    })
}

impl<S> Widget for MainView<'_, S>
where
    S: EditorEffects,
{
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let _ = self.render_with_hit_map(area, buffer);
    }
}
