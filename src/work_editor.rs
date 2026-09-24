use crate::cargo_policy::CargoAction;
use crate::command_process::LiveSnapshot;
use crate::console::{ConsoleLine, TestCaseDirectory};
use crate::diagnostics::DiagnosticNavigation;
use crate::session::ConsoleStart;
use crate::tui::*;
use crate::tui::{ConsoleAnchor, ConsoleRows};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{Terminal, TerminalOptions, Viewport as TerminalViewport, backend::CrosstermBackend};
use rustrace_model::{
    MAX_WORKSPACE_PATH_BYTES, PasteInputChannel, PasteRejectionReason, WorkspacePath,
};
use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    io,
    ops::Deref,
    rc::Rc,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkView {
    Workspace,
    Console,
}

struct ConsoleBody {
    output: Vec<u8>,
    prompt: Vec<u8>,
    cursor: Option<usize>,
}

enum WorkBody {
    Console(ConsoleBody),
}

#[derive(Default)]
struct TestCasePicker {
    open: bool,
    cases: Vec<crate::session::TestCase>,
    selected: usize,
    results: BTreeMap<String, crate::session::TestCaseComparison>,
    queue: VecDeque<crate::session::TestCase>,
    visible_results: VecDeque<crate::session::TestCaseComparison>,
    visible_output_case: Option<crate::session::TestCase>,
    notice: Option<String>,
    origin: Option<(WorkView, WorkspaceFocus)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestCasePickerKeyAction {
    None,
    Run,
    Refresh,
    Close,
}

impl TestCasePicker {
    fn refresh(
        &mut self,
        workspace_root: &std::path::Path,
        packaged_suite_expected: bool,
    ) -> Result<(), Box<dyn Error>> {
        let selected = self
            .cases
            .get(self.selected)
            .map(|case| case.name().to_owned());
        match TestCaseDirectory::open(workspace_root).and_then(|directory| directory.list_cases()) {
            Ok(cases) => {
                self.replace_cases(cases);
                self.notice = None;
            }
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                self.replace_cases(Vec::new());
                self.notice = Some(Self::missing_notice(packaged_suite_expected).to_owned());
            }
            Err(error) => {
                self.replace_cases(Vec::new());
                self.notice = Some("Packaged test cases are unavailable".to_owned());
                return Err(error);
            }
        }
        self.selected = selected
            .and_then(|name| self.cases.iter().position(|case| case.name() == name))
            .unwrap_or(0)
            .min(self.cases.len());
        Ok(())
    }

    fn replace_cases(&mut self, cases: Vec<crate::session::TestCase>) {
        self.results
            .retain(|_, result| cases.contains(&result.case));
        self.visible_results
            .retain(|result| cases.contains(&result.case));
        if self
            .visible_output_case
            .as_ref()
            .is_some_and(|output_case| !cases.contains(output_case))
        {
            self.visible_output_case = None;
        }
        self.cases = cases;
        self.selected = self.selected.min(self.cases.len());
        self.queue.clear();
    }

    fn open(&mut self) {
        self.open = true;
    }

    fn open_from(&mut self, view: WorkView, focus: WorkspaceFocus) {
        self.origin.get_or_insert((view, focus));
        self.open();
    }

    fn close(&mut self) {
        self.open = false;
    }

    fn close_and_restore(&mut self, view: &mut WorkView, focus: &mut WorkspaceFocus) {
        self.close();
        if let Some((origin_view, origin_focus)) = self.origin.take() {
            *view = origin_view;
            *focus = origin_focus;
        }
    }

    fn is_open(&self) -> bool {
        self.open
    }

    fn select(&mut self, index: usize) -> bool {
        let selected = index.min(self.cases.len());
        let changed = selected != self.selected;
        self.selected = selected;
        changed
    }

    fn move_selection(&mut self, delta: isize) -> bool {
        let count = self.cases.len() + 1;
        let selected = if delta < 0 {
            (self.selected + count - 1) % count
        } else if delta > 0 {
            (self.selected + 1) % count
        } else {
            self.selected
        };
        self.select(selected)
    }

    fn begin_selected(&mut self) -> Option<crate::session::TestCase> {
        self.queue.clear();
        self.visible_results.clear();
        self.visible_output_case = None;
        if self.selected == self.cases.len() {
            self.queue.extend(self.cases.iter().cloned());
        } else if let Some(case) = self.cases.get(self.selected) {
            self.queue.push_back(case.clone());
        }
        let first = self.queue.pop_front()?;
        self.close();
        Some(first)
    }

    fn record_completed(
        &mut self,
        result: crate::session::TestCaseComparison,
    ) -> Option<crate::session::TestCase> {
        self.results
            .insert(result.case.name().to_owned(), result.clone());
        self.visible_results.push_back(result);
        if self.visible_results.len() > rustrace_workspace::assignment_package::MAX_TEST_CASES {
            self.visible_results.pop_front();
        }
        if let Some(next) = self.queue.pop_front() {
            Some(next)
        } else {
            self.open();
            None
        }
    }

    fn cancel_queue(&mut self) {
        self.queue.clear();
    }

    fn has_queued_cases(&self) -> bool {
        !self.queue.is_empty()
    }

    fn has_visible_results(&self) -> bool {
        !self.visible_results.is_empty()
    }

    fn retire_visible_results(&mut self) {
        self.visible_results.clear();
        self.visible_output_case = None;
    }

    fn note_started(&mut self, case: crate::session::TestCase) {
        self.visible_output_case = Some(case);
    }

    const fn missing_notice(packaged_suite_expected: bool) -> &'static str {
        if packaged_suite_expected {
            "Packaged test cases are unavailable"
        } else {
            "No packaged test cases (assignment format v1)"
        }
    }

    fn rows(&self) -> Vec<TestCasePickerRow> {
        self.cases
            .iter()
            .map(|case| {
                let status = match self.results.get(case.name()).map(|result| &result.outcome) {
                    None => "—".to_owned(),
                    Some(crate::session::TestCaseOutcome::Pass) => "PASS".to_owned(),
                    Some(crate::session::TestCaseOutcome::Fail(mismatch)) => {
                        format!("FAIL line {}", mismatch.line)
                    }
                    Some(crate::session::TestCaseOutcome::Error(_)) => "ERROR".to_owned(),
                };
                TestCasePickerRow::new(case.name(), status)
            })
            .collect()
    }

    fn view_state(&self) -> TestCasePickerState {
        TestCasePickerState::new(self.rows(), self.selected, self.notice.clone())
    }
}

fn test_case_view_switch_available(active_test_case: bool, picker: &TestCasePicker) -> bool {
    !active_test_case && !picker.has_queued_cases()
}

fn non_test_command_owned_tick(
    was_command_active: bool,
    was_test_case_active: bool,
    command_active: bool,
    test_case_active: bool,
) -> bool {
    (was_command_active && !was_test_case_active) || (command_active && !test_case_active)
}

fn test_case_picker_key_action(
    picker: &mut TestCasePicker,
    event: &Event,
) -> TestCasePickerKeyAction {
    let Event::Key(key) = event else {
        return TestCasePickerKeyAction::None;
    };
    if key.kind == KeyEventKind::Release {
        return TestCasePickerKeyAction::None;
    }
    match key.code {
        KeyCode::Up => {
            picker.move_selection(-1);
            TestCasePickerKeyAction::None
        }
        KeyCode::Down => {
            picker.move_selection(1);
            TestCasePickerKeyAction::None
        }
        KeyCode::Enter => TestCasePickerKeyAction::Run,
        KeyCode::Char('r' | 'R') if key.modifiers.is_empty() => TestCasePickerKeyAction::Refresh,
        KeyCode::Esc => TestCasePickerKeyAction::Close,
        _ => TestCasePickerKeyAction::None,
    }
}

fn test_case_result_summary(result: &crate::session::TestCaseComparison) -> String {
    let summary = match &result.outcome {
        crate::session::TestCaseOutcome::Pass => {
            format!("Test case {}: PASS", result.case.name())
        }
        crate::session::TestCaseOutcome::Fail(mismatch) => format!(
            "Test case {}: FAIL at line {} — expected ({} bytes) \"{}\" got ({} bytes) \"{}\"",
            result.case.name(),
            mismatch.line,
            mismatch.expected_len,
            mismatch.expected_preview,
            mismatch.actual_len,
            mismatch.actual_preview,
        ),
        crate::session::TestCaseOutcome::Error(reason) => {
            format!("Test case {}: ERROR ({reason})", result.case.name())
        }
    };
    crate::display::label(&summary, 512)
}

fn start_test_case_sequence(
    session: &mut crate::session::ProductionSession,
    picker: &mut TestCasePicker,
    mut case: crate::session::TestCase,
) {
    loop {
        match session.start_test_case(case.clone()) {
            Ok(()) => {
                picker.note_started(case);
                return;
            }
            Err(error) => {
                let reason =
                    crate::display::label_fmt(format_args!("could not start: {error}"), 128);
                let result = crate::session::TestCaseComparison {
                    case,
                    outcome: crate::session::TestCaseOutcome::Error(reason),
                    expected_blake3: None,
                    actual_blake3: None,
                };
                let Some(next) = picker.record_completed(result) else {
                    return;
                };
                case = next;
            }
        }
    }
}

fn test_case_output(
    output: &[u8],
    results: &VecDeque<crate::session::TestCaseComparison>,
    output_case: Option<&crate::session::TestCase>,
    active: bool,
    maximum_rows: usize,
) -> Vec<OutputRow> {
    if maximum_rows == 0 {
        return Vec::new();
    }
    let display = crate::display::plain(output, crate::display::Limits::default());
    let output_rows = display
        .text
        .lines
        .into_iter()
        .map(|line| OutputRow::plain(line.to_string()))
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    if active {
        rows.extend(
            results
                .iter()
                .map(|result| OutputRow::plain(test_case_result_summary(result))),
        );
        rows.extend(output_rows);
        return rows;
    }
    for result in results {
        if output_case == Some(&result.case) {
            rows.extend(output_rows.iter().cloned());
        }
        rows.push(OutputRow::plain(test_case_result_summary(result)));
    }
    rows
}

fn console_body(
    output: &[u8],
    line: &ConsoleLine,
    active: bool,
    accepts_stdin: bool,
    overwrite: bool,
) -> ConsoleBody {
    let prompt = if overwrite {
        "Overwrite existing test-case output? Y/Enter confirms, N/Esc cancels"
    } else if accepts_stdin {
        "stdin> "
    } else if active {
        "[console input disabled for this command]"
    } else {
        "> "
    };
    let mut prompt = prompt.as_bytes().to_vec();
    let cursor = (!active || accepts_stdin).then(|| prompt.len() + line.cursor());
    if cursor.is_some() {
        prompt.extend_from_slice(line.text().as_bytes());
    }
    ConsoleBody {
        output: output.to_vec(),
        prompt,
        cursor,
    }
}

fn recording_indicators(
    health: Option<&crate::session::SessionHealth>,
) -> (RecordingState, JournalHealth) {
    match health {
        Some(health) if health.recovery_reason.is_none() => {
            (RecordingState::Active, JournalHealth::Healthy)
        }
        _ => (RecordingState::Inactive, JournalHealth::Degraded),
    }
}

fn journal_warning_message(health: Option<&crate::session::SessionHealth>) -> Option<String> {
    let health = health.filter(|health| health.recovery_reason.is_none())?;
    match (health.warning, health.undo_limited) {
        (true, true) => Some("80% budget warning | undo limit reached".to_owned()),
        (true, false) => Some("80% budget warning".to_owned()),
        (false, true) => Some("undo limit reached".to_owned()),
        (false, false) => None,
    }
}

fn completion_input_now_ms(_loop_start_ms: u64, event_read_ms: u64) -> u64 {
    event_read_ms
}

fn observe_completion_trigger(
    session: &mut crate::session::ProductionSession,
    trigger: &mut CompletionTrigger,
    input: CompletionTriggerInput,
    now_ms: u64,
) {
    trigger.observe(input, session.workspace().active_buffer().version(), now_ms);
    if !trigger.is_armed() {
        session.cancel_deferred_automatic_completion();
    }
}

/// Console scrollback position. `pin` anchors the first visible row to a
/// source line of the current command's output while the student reads older
/// output; `None` follows the newest output. `rows` caches the wrapped output
/// for the pane width so each frame decodes it at most once.
#[derive(Debug, Default)]
struct ConsoleScroll {
    pin: Option<ConsoleAnchor>,
    rows: Rc<ConsoleRows>,
    // (live buffer id, absolute start, length, width) of the cached rows.
    key: (u64, u64, usize, u16),
    visible: usize,
}

impl ConsoleScroll {
    fn measure(&mut self, snapshot: &LiveSnapshot, width: u16, visible: usize) {
        let key = (snapshot.id, snapshot.start, snapshot.bytes.len(), width);
        // Each new command's output starts at its newest line.
        if key.0 != self.key.0 {
            self.pin = None;
        }
        if key != self.key {
            self.rows = Rc::new(ConsoleRows::new(&snapshot.bytes, snapshot.start, width));
            self.key = key;
            self.pin = self.pin.map(|pin| self.rows.retain(pin));
        }
        self.visible = visible;
        if self.top().is_some_and(|top| top >= self.last_top()) {
            self.pin = None;
        }
    }

    fn top(&self) -> Option<usize> {
        self.pin.map(|pin| self.rows.resolve(pin))
    }

    fn last_top(&self) -> usize {
        self.rows.len().saturating_sub(self.visible)
    }

    fn scroll_by(&mut self, delta: isize) -> bool {
        let previous = self.top();
        let top = previous
            .unwrap_or_else(|| self.last_top())
            .saturating_add_signed(delta);
        self.pin = (top < self.last_top()).then(|| self.rows.anchor(top));
        self.top() != previous
    }

    fn page(&self) -> isize {
        isize::try_from(self.visible.saturating_sub(1).max(1)).unwrap_or(isize::MAX)
    }
}

fn handle_console_key(
    session: &mut crate::session::ProductionSession,
    key: &crossterm::event::KeyEvent,
    console_line: &mut ConsoleLine,
    console_scroll: &mut ConsoleScroll,
    view: &mut WorkView,
    focus: &mut WorkspaceFocus,
    status: &mut StatusMessage,
) {
    if key.kind == KeyEventKind::Release {
        return;
    }
    let accepts_stdin = session.console_accepts_stdin();
    let editable = !session.command_active() || accepts_stdin;
    match key.code {
        KeyCode::PageUp => {
            console_scroll.scroll_by(-console_scroll.page());
        }
        KeyCode::PageDown => {
            console_scroll.scroll_by(console_scroll.page());
        }
        KeyCode::Esc => {
            if session.console_command_active() {
                session.cancel_command();
            }
            *view = WorkView::Workspace;
            *focus = WorkspaceFocus::Editor;
            status.clear();
        }
        KeyCode::Enter if accepts_stdin => {
            *status = match session.submit_console_line(console_line.text()) {
                Ok(true) => {
                    console_line.take();
                    "console stdin line sent".into()
                }
                Ok(false) => "console stdin queue full; line preserved".into(),
                Err(error) => format!("console stdin rejected: {error}").into(),
            };
        }
        KeyCode::Enter if !session.command_active() => {
            *status = match session.start_console_command(console_line.text()) {
                Ok(ConsoleStart::Started) => {
                    console_line.take();
                    "console command preparation started".into()
                }
                Ok(ConsoleStart::OverwriteConfirmation { .. }) => {
                    console_line.take();
                    "confirm overwrite; output remains untouched".into()
                }
                Err(error) => format!("console command rejected: {error}").into(),
            };
        }
        KeyCode::Left if editable => console_line.left(),
        KeyCode::Right if editable => console_line.right(),
        KeyCode::Home if editable => console_line.home(),
        KeyCode::End if editable => console_line.end(),
        KeyCode::Backspace if editable => {
            console_line.backspace();
        }
        KeyCode::Delete if editable => {
            console_line.delete();
        }
        KeyCode::Char(character)
            if editable
                && !key.modifiers.intersects(
                    KeyModifiers::CONTROL
                        | KeyModifiers::ALT
                        | KeyModifiers::SUPER
                        | KeyModifiers::HYPER
                        | KeyModifiers::META,
                )
                && !console_line.insert(character) =>
        {
            *status = "console line rejected: control or 4096-byte limit".into();
        }
        _ => {}
    }
}

fn keybinds_scroll_delta(event: &Event) -> Option<isize> {
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind == KeyEventKind::Release {
        return None;
    }
    match key.code {
        KeyCode::Up => Some(-1),
        KeyCode::Down => Some(1),
        KeyCode::PageUp => Some(-10),
        KeyCode::PageDown => Some(10),
        _ => None,
    }
}

fn reduce_keybinds_scroll(scroll: &mut usize, delta: isize, area: ratatui::layout::Rect) -> bool {
    let previous = *scroll;
    *scroll = scroll
        .saturating_add_signed(delta)
        .min(maximum_keybinds_scroll(area, KEYBIND_ROWS.len()));
    *scroll != previous
}

fn should_follow_cursor_for_frame(
    follow_cursor: bool,
    resize_follow_cursor: bool,
    _view: WorkView,
    focus: WorkspaceFocus,
) -> bool {
    resize_follow_cursor || (follow_cursor && focus == WorkspaceFocus::Editor)
}

#[derive(Clone, Copy)]
struct EditorKeyConfig {
    primary_modifier: crate::config::PrimaryModifier,
    keyboard_enhancement_active: bool,
    ghostty_key_bindings: crate::ghostty::GhosttyKeyBindings,
}

struct UpdateMenu {
    state: crate::update::UpdateState,
    managed: bool,
    open: bool,
}

impl UpdateMenu {
    fn new(state: crate::update::UpdateState) -> Self {
        Self {
            state,
            managed: false,
            open: false,
        }
    }

    fn toggle_with(&mut self, persist: impl FnOnce(bool) -> io::Result<()>) {
        let enabled = !self.state.checks_enabled;
        if persist(enabled).is_ok() {
            self.state.checks_enabled = enabled;
        }
    }

    fn refresh(&mut self) {
        self.state = crate::update::cached_state();
        self.managed = crate::update::installer_managed();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_editor_loop<O>(
    session: &mut crate::session::ProductionSession,
    assignment_title: &str,
    quit_pending: bool,
    key_config: EditorKeyConfig,
    startup_notice: Option<&str>,
    palette: &crate::tui::theme::Palette,
    operations: &mut O,
    initial_update: crate::update::UpdateState,
) -> Result<(), Box<dyn Error>>
where
    O: TerminalOperations,
{
    let mut update_menu = UpdateMenu::new(initial_update);
    let EditorKeyConfig {
        primary_modifier,
        keyboard_enhancement_active,
        ghostty_key_bindings,
    } = key_config;
    let hint_ghostty_key_bindings = if keyboard_enhancement_active {
        ghostty_key_bindings
    } else {
        crate::ghostty::GhosttyKeyBindings::defaults()
    };
    let backend = CrosstermBackend::new(io::stdout());
    let options = TerminalOptions {
        viewport: TerminalViewport::Fullscreen,
    };
    let mut terminal = Terminal::with_options(backend, options)?;
    let mut status: StatusMessage = startup_notice.unwrap_or("ready").into();
    let mut find_panel: Option<FindPanel> = None;
    let mut path_prompt: Option<PathPrompt> = None;
    let mut command_picker: Option<usize> = None;
    let mut editor_context_menu: Option<EditorContextMenuState> = None;
    let mut files_context_menu: Option<FilesContextMenuState> = None;
    let mut keybinds_scroll: Option<usize> = None;
    let mut dismissed_health_toast: Option<String> = None;
    let mut focus = WorkspaceFocus::Editor;
    let mut follow_cursor = true;
    let mut resize_follow_cursor = false;
    let mut output_scroll = 0;
    let mut console_scroll = ConsoleScroll::default();
    let mut mouse_state = MouseState::default();
    let mut pane_resize = PaneResizeState::default();
    let mouse_clock = Instant::now();
    let mut pending_event = None;
    let mut draw_gate = DrawGate::requested();
    let mut hit_map = crate::tui::shell::HitMap::default();
    let mut view = WorkView::Workspace;
    let mut console_line = ConsoleLine::default();
    let mut test_cases = TestCasePicker::default();
    let completion_clock = Instant::now();
    let mut completion_trigger = CompletionTrigger::default();
    let mut toast_timer = ToastTimer::default();
    let mut save_triggered_check = false;
    'editor: loop {
        let command_modal = find_panel.is_some()
            || path_prompt.is_some()
            || command_picker.is_some()
            || editor_context_menu.is_some()
            || files_context_menu.is_some()
            || update_menu.open
            || keybinds_scroll.is_some()
            || test_cases.is_open();
        session.set_command_modal(command_modal);
        let now_ms = u64::try_from(completion_clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        if command_modal
            || focus != WorkspaceFocus::Editor
            || session.workspace().confirmation_pending()
            || session.console_overwrite_pending()
        {
            observe_completion_trigger(
                session,
                &mut completion_trigger,
                CompletionTriggerInput::ModalEntry,
                now_ms,
            );
        }
        let was_command_active = session.command_active();
        let was_test_case_active = session.test_case_active();
        let tick = session.tick();
        let completed_test_case = if let Some(result) = session.take_test_case_result() {
            let next = test_cases.record_completed(result);
            output_scroll = usize::MAX;
            status.clear();
            if let Some(next) = next {
                start_test_case_sequence(session, &mut test_cases, next);
            }
            session.set_command_modal(test_cases.is_open());
            true
        } else {
            false
        };
        if let Some(next_status) = command_tick_status(
            tick.as_ref().err().map(|error| error.as_ref()),
            completed_test_case,
            was_command_active,
            session.command_active(),
            session.completed_format_rejection(),
            &session.command_status(),
            &mut save_triggered_check,
        ) {
            status = next_status.into();
        }
        if non_test_command_owned_tick(
            was_command_active,
            was_test_case_active,
            session.command_active(),
            session.test_case_active(),
        ) {
            test_cases.retire_visible_results();
        }
        if completion_trigger.take_expired(session.workspace().active_buffer().version(), now_ms) {
            match session.trigger_automatic_completion() {
                Ok(_) => {}
                Err(error) => status = format!("recovery required: {error}").into(),
            }
        }
        if quit_pending && session.command_terminal_restoration_safe() {
            break;
        }
        if quit_pending {
            status = "Quit pending: waiting for owned process cleanup; input is disabled".into();
        }
        let command_active = session.command_active();
        let command_status = session.command_status();
        let command_error = session.command_error_status();
        let health = session.health();
        let external_notice = session.external_notice().unwrap_or("").to_owned();
        let (recording, journal_health) = recording_indicators(health.as_ref().ok());
        let recovery_reason = match &health {
            Ok(health) => health.recovery_reason.clone(),
            Err(error) => Some(error.to_string()),
        };
        let journal_warning = journal_warning_message(health.as_ref().ok());
        let selected_completion = session.selected_completion();
        let completion_can_render = selected_completion.is_some()
            && find_panel.is_none()
            && path_prompt.is_none()
            && command_picker.is_none()
            && editor_context_menu.is_none()
            && files_context_menu.is_none()
            && !update_menu.open
            && keybinds_scroll.is_none()
            && !command_active
            && focus == WorkspaceFocus::Editor
            && !session.workspace().confirmation_pending()
            && external_notice.is_empty()
            && recovery_reason.is_none()
            && !status.starts_with(crate::session::PASTE_BLOCKED_WARNING);
        let completion_popup = completion_can_render.then(|| {
            (
                session
                    .completion_items()
                    .iter()
                    .map(|item| CompletionViewItem::new(&item.label, item.kind))
                    .collect(),
                selected_completion.expect("renderable completion has a selection"),
            )
        });
        let diagnostics_visible = find_panel.is_none()
            && view == WorkView::Workspace
            && path_prompt.is_none()
            && command_picker.is_none()
            && editor_context_menu.is_none()
            && files_context_menu.is_none()
            && !update_menu.open
            && keybinds_scroll.is_none()
            && !command_active
            && external_notice.is_empty()
            && recovery_reason.is_none()
            && !status.starts_with(crate::session::PASTE_BLOCKED_WARNING);
        let area = terminal.size()?;
        if let Some(scroll) = &mut keybinds_scroll {
            reduce_keybinds_scroll(scroll, 0, area.into());
        }
        let mode_bar_visible = find_panel.is_some()
            || command_picker.is_some()
            || editor_context_menu.is_some()
            || files_context_menu.is_some()
            || selected_completion.is_some()
            || test_cases.is_open()
            || focus == WorkspaceFocus::Console
            || session.workspace().confirmation_pending()
            || session.console_overwrite_pending()
            || quit_pending
            || recovery_reason.is_some()
            || recording == RecordingState::Inactive
            || journal_health == JournalHealth::Degraded
            || !external_notice.is_empty()
            || status.contains('\n');
        let bottom_pane = if view == WorkView::Console {
            crate::tui::shell::BottomPane::Console
        } else {
            crate::tui::shell::BottomPane::Output
        };
        let layout = crate::tui::shell::shell_layout_with_sizes(
            area.into(),
            bottom_pane,
            mode_bar_visible,
            pane_resize.bottom_height(),
            pane_resize.sidebar_width(),
        );
        let output_rows = match layout {
            MainLayout::Full(panes) => panes.bottom.height.saturating_sub(1) as usize,
            MainLayout::TooSmall(_) => 0,
        };
        let console_output = (view == WorkView::Console).then(|| session.console_snapshot());
        if let (Some(snapshot), MainLayout::Full(panes)) = (&console_output, layout) {
            let area = crate::tui::console_output_area(panes.bottom);
            console_scroll.measure(snapshot, area.width, usize::from(area.height));
        }
        let mut diagnostic_rows = if diagnostics_visible {
            session.diagnostic_display_rows(output_rows.saturating_add(1))
        } else {
            Vec::new()
        };
        let output_header_message = session.live_diagnostic_header_for_caret();
        if diagnostic_rows
            .first()
            .is_some_and(|row| row.text().starts_with("Diagnostics "))
        {
            diagnostic_rows.remove(0);
        }
        let diagnostic_markers = if recovery_reason.is_none() && view == WorkView::Workspace {
            session.active_diagnostic_markers()
        } else {
            Vec::new()
        };
        let live_diagnostics = if recovery_reason.is_none() && view == WorkView::Workspace {
            session.active_live_diagnostic_spans()
        } else {
            Vec::new()
        };
        let console_overwrite = session.console_overwrite_pending();
        let body = match view {
            WorkView::Workspace => None,
            WorkView::Console => Some(WorkBody::Console(console_body(
                console_output
                    .as_ref()
                    .map_or(&[][..], |snapshot| snapshot.bytes.as_slice()),
                &console_line,
                session.console_command_active(),
                session.console_accepts_stdin(),
                console_overwrite,
            ))),
        };
        let test_case_active = session.test_case_active();
        let test_case_console_output = (test_case_active || test_cases.has_visible_results())
            .then(|| session.console_output());
        let workspace = session.workspace_mut();
        if should_follow_cursor_for_frame(follow_cursor, resize_follow_cursor, view, focus) {
            match layout {
                MainLayout::Full(panes) => workspace.follow_cursor_in_editor_area(panes.editor),
                MainLayout::TooSmall(_) => workspace.follow_cursor(0, 0),
            }
        }
        let editor_height = match layout {
            MainLayout::Full(panes) => usize::from(panes.editor.height),
            MainLayout::TooSmall(_) => 0,
        };
        follow_cursor = false;
        resize_follow_cursor = false;

        let dirty = workspace.active_is_dirty();
        let active_file = workspace.active_path().as_str().to_owned();
        let selected_file = workspace.selected_path().clone();
        let files = workspace
            .file_tree()
            .iter()
            .map(|file| {
                FileTreeViewEntry::new(
                    file.path().as_str(),
                    file.path().as_str() == active_file,
                    file.path() == &selected_file,
                    file.is_dirty(),
                    file.is_editable(),
                )
            })
            .collect::<Vec<_>>();
        let buffers = workspace
            .file_tree()
            .iter()
            .filter(|file| file.is_editable())
            .map(|file| {
                BufferTabViewEntry::new(
                    file.path().as_str(),
                    file.path().as_str() == active_file,
                    file.is_dirty(),
                )
            })
            .collect::<Vec<_>>();
        let message = match (command_picker, editor_context_menu, files_context_menu) {
            (Some(_), _, _) | (_, Some(_), _) | (_, _, Some(_)) => String::new(),
            (None, None, None) if command_active => {
                command_status_for_output(&command_status, primary_modifier)
            }
            (None, None, None) if console_overwrite => {
                "Console output exists: Y/Enter overwrites, N/Esc cancels".into()
            }
            (None, None, None) => String::new(),
        };
        let multiline_status = (!external_notice.is_empty())
            .then_some(external_notice.as_str())
            .or_else(|| {
                recovery_reason
                    .as_deref()
                    .filter(|reason| reason.contains('\n'))
            })
            .or_else(|| status.contains('\n').then_some(status.as_str()));
        let output = if let Some(console_output) = test_case_console_output {
            test_case_output(
                &console_output,
                &test_cases.visible_results,
                test_cases.visible_output_case.as_ref(),
                test_case_active,
                output_rows,
            )
        } else {
            multiline_status.map_or_else(
                || diagnostic_output(&message, &diagnostic_rows, output_rows),
                |notice| vec![OutputRow::plain(notice)],
            )
        };
        let maximum_output_scroll = crate::tui::maximum_output_scroll(&output, output_rows);
        output_scroll = output_scroll.min(maximum_output_scroll);
        let mut state = MainViewState::new(
            assignment_title,
            buffers,
            vec![],
            recording,
            journal_health,
            if dirty { "modified" } else { "saved" },
        )
        .with_primary_modifier(primary_modifier)
        .with_ghostty_key_bindings(hint_ghostty_key_bindings)
        .with_output_rows(output)
        .with_output_scroll(output_scroll)
        .with_file_tree(files)
        .with_diagnostic_markers(diagnostic_markers)
        .with_live_diagnostics(live_diagnostics)
        .with_recovery_reason(recovery_reason.as_deref());
        if let Some(message) = output_header_message {
            state = state.with_output_header_message(message);
        }
        if let Some(height) = pane_resize.bottom_height() {
            state = state.with_bottom_pane_height(height);
        }
        if let Some(width) = pane_resize.sidebar_width() {
            state = state.with_sidebar_width(width);
        }
        state = state.with_sidebar_dragging(pane_resize.sidebar_dragging());
        if let Some((items, selected)) = completion_popup {
            state = state.with_completion_popup(items, selected);
        }
        let persistent_error = persistent_error_message(
            recovery_reason.as_deref(),
            (!external_notice.is_empty()).then_some(external_notice.as_str()),
            recording,
            journal_health,
        );
        if let Some(error) = &persistent_error {
            state = state.with_error_condition(error);
        } else if let Some(error) = &command_error {
            state = state.with_error_condition(error);
        } else if quit_pending || status.contains('\n') {
            state = state.with_error_condition(status.as_str());
        }
        let mode_bar = if command_picker.is_some() {
            Some(ModeBarState::new(
                ModeBarKind::Menu,
                "esc close  ↵ run  ↑↓ select",
            ))
        } else if editor_context_menu.is_some() || files_context_menu.is_some() {
            Some(ModeBarState::new(
                ModeBarKind::Menu,
                "esc close  ↵ apply  ↑↓ select",
            ))
        } else if console_overwrite || workspace.confirmation_pending() {
            Some(ModeBarState::new(
                ModeBarKind::Confirm,
                "↵ confirm  esc cancel",
            ))
        } else if selected_completion.is_some() {
            Some(ModeBarState::new(
                ModeBarKind::Complete,
                "esc close  tab/↵ accept  ↑↓ select",
            ))
        } else {
            None
        };
        if let Some(mode_bar) = mode_bar {
            state = state.with_mode_bar(mode_bar);
        }
        state = state.with_update_state(update_menu.state.clone());
        if update_menu.open {
            state = state.with_update_panel(update_menu.managed, crate::update::unix_seconds());
        }
        if let Some(selected) = command_picker {
            state = state.with_command_menu(selected);
        }
        if let Some(menu) = editor_context_menu {
            state = state.with_editor_context_menu(menu);
        }
        if let Some(menu) = files_context_menu {
            state = state.with_files_context_menu(menu);
        }
        if let Some(scroll) = keybinds_scroll {
            state = state.with_keybinds_overlay(scroll);
        }
        if test_cases.is_open() {
            state = state.with_test_case_picker(test_cases.view_state());
        }
        if let Some(prompt) = path_prompt.as_ref().filter(|prompt| {
            matches!(
                prompt.operation,
                PathOperation::Create | PathOperation::Rename
            )
        }) {
            state = state.with_file_prompt(FilePromptState::new(
                match prompt.operation {
                    PathOperation::Create => FilePromptKind::Create,
                    PathOperation::Rename => FilePromptKind::Rename,
                },
                &prompt.input,
            ));
        }
        if let Some(panel) = &find_panel {
            let counter = if panel.find.is_empty() {
                String::new()
            } else {
                let summary = workspace.search_summary(&panel.find);
                summary.current.map_or_else(
                    || String::from("no matches"),
                    |current| format!("{current} of {}", summary.total),
                )
            };
            state = state.with_find_panel(FindPanelState::new(
                &panel.find,
                &panel.replace,
                panel.active,
                counter,
            ));
        }
        if console_overwrite {
            state = state.with_confirmation(ConfirmationState::new(
                "Console output exists. Replace it with the next command?",
            ));
        } else if workspace.confirmation_pending() {
            let message = if workspace.delete_confirmation_pending() {
                format!("Delete {}?", selected_file.as_str())
            } else {
                "Discard unsaved buffer changes?".to_owned()
            };
            state = state.with_confirmation(ConfirmationState::new(message));
        }
        let health_message = persistent_error.clone().or_else(|| journal_warning.clone());
        if health_message.is_none() {
            dismissed_health_toast = None;
        }
        if let Some(toast) = toast_for_frame(
            &mut status,
            health_message,
            persistent_error.is_some(),
            &mut dismissed_health_toast,
            &mut toast_timer,
            now_ms,
        ) {
            state = state.with_toast(toast);
        }
        match body {
            Some(WorkBody::Console(body)) => {
                state = state
                    .with_console_body_view(
                        "Embedded Cargo console",
                        body.output,
                        body.prompt,
                        body.cursor,
                        focus == WorkspaceFocus::Console,
                    )
                    .with_console_scroll(console_scroll.top())
                    .with_console_rows(Rc::clone(&console_scroll.rows));
            }
            None => {}
        }
        let shell_state = ShellState {
            modal: if find_panel.is_some() {
                ShellModal::FindPanel
            } else if path_prompt.is_some() {
                ShellModal::FilePrompt
            } else if update_menu.open {
                ShellModal::UpdateNotice
            } else if command_active {
                ShellModal::Prompt
            } else if console_overwrite {
                ShellModal::ConsoleOverwrite
            } else if workspace.confirmation_pending() {
                ShellModal::Confirmation
            } else if command_picker.is_some() {
                ShellModal::CommandMenu
            } else if editor_context_menu.is_some() {
                ShellModal::EditorContextMenu
            } else if files_context_menu.is_some() {
                ShellModal::FilesContextMenu
            } else if selected_completion.is_some() {
                ShellModal::Completion
            } else if keybinds_scroll.is_some() {
                ShellModal::Keybinds
            } else if test_cases.is_open() {
                ShellModal::TestCases
            } else {
                ShellModal::None
            },
        };
        if draw_gate.take_draw() {
            terminal.draw(|frame| {
                hit_map = MainView::new(
                    &state,
                    workspace.active_buffer(),
                    workspace.active_viewport(),
                    &workspace.active_highlights(),
                )
                .with_palette(palette.clone())
                .render_with_hit_map(frame.area(), frame.buffer_mut());
            })?;
        }
        let wait_started = Instant::now();
        let event = loop {
            let mut next = if let Some(event) = pending_event.take() {
                event
            } else {
                let remaining = Duration::from_millis(100).saturating_sub(wait_started.elapsed());
                if remaining.is_zero() || !event::poll(remaining)? {
                    draw_gate.request_timer();
                    continue 'editor;
                }
                event::read()?
            };
            if let Event::Mouse(mouse) = &next
                && mouse.kind == crossterm::event::MouseEventKind::Moved
                && shell_state.modal != ShellModal::TestCases
            {
                let now_ms = mouse_clock.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                mouse_state.reduce(mouse, now_ms, shell_state.modal);
                continue;
            }
            let batch = drain_mouse_event_batch(next, || {
                if event::poll(Duration::ZERO)? {
                    event::read().map(Some)
                } else {
                    Ok(None)
                }
            })?;
            for mouse in &batch.moved {
                let now_ms = mouse_clock.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                mouse_state.reduce(mouse, now_ms, shell_state.modal);
            }
            pending_event = batch.pending;
            next = batch.event;
            break next;
        };
        draw_gate.request_change(!matches!(&event, Event::Mouse(_)));
        let event_read_ms =
            u64::try_from(completion_clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        let input_now_ms = completion_input_now_ms(now_ms, event_read_ms);
        dismiss_toast_on_key(
            &event,
            &mut status,
            persistent_error_message(
                recovery_reason.as_deref(),
                (!external_notice.is_empty()).then_some(external_notice.as_str()),
                recording,
                journal_health,
            )
            .or_else(|| journal_warning.clone()),
            &mut dismissed_health_toast,
            &mut toast_timer,
        );
        if quit_pending {
            continue;
        }
        let outside_editor = find_panel.is_some()
            || path_prompt.is_some()
            || command_picker.is_some()
            || editor_context_menu.is_some()
            || files_context_menu.is_some()
            || update_menu.open
            || keybinds_scroll.is_some()
            || test_cases.is_open()
            || focus != WorkspaceFocus::Editor
            || session.workspace().confirmation_pending()
            || session.console_overwrite_pending();
        // Consume/discard clipboard input before any prompt, focus, path or
        // confirmation router can retain it or silently ignore the attempt.
        let matching_internal = match &event {
            Event::Paste(text) if !outside_editor => session.terminal_paste_matches(text)?,
            _ => false,
        };
        match paste_event_route(&event, outside_editor, matching_internal, primary_modifier) {
            PasteEventRoute::Continue => {}
            PasteEventRoute::MatchingInternal => {
                let Event::Paste(text) = event else {
                    unreachable!("matching paste route requires a bracketed paste event")
                };
                let outcome = match session.paste_terminal(&text) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        status = error.to_string().into();
                        EditorOutcome::NoChange
                    }
                };
                observe_completion_trigger(
                    session,
                    &mut completion_trigger,
                    CompletionTriggerInput::Paste,
                    input_now_ms,
                );
                let (follow, quit) = apply_outcome(outcome, &mut status);
                follow_cursor = follow;
                if quit {
                    break;
                }
                continue;
            }
            PasteEventRoute::Reject(channel, reason) => {
                drop(event);
                observe_completion_trigger(
                    session,
                    &mut completion_trigger,
                    CompletionTriggerInput::Paste,
                    input_now_ms,
                );
                status = match session.reject_paste(channel, reason) {
                    Ok(()) => crate::session::PASTE_BLOCKED_WARNING.into(),
                    Err(error) => error.to_string().into(),
                };
                continue;
            }
        }
        if matches!(&event, Event::Key(key) if has_exact_primary_modifier(key.modifiers, primary_modifier) && matches!(key.code, KeyCode::Char('q' | 'Q')))
        {
            if request_quit(session, &mut status) {
                break;
            }
            continue;
        }
        if let Event::Mouse(mouse) = &event {
            let now_ms = mouse_clock.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            mouse_state.reduce(mouse, now_ms, shell_state.modal);
            let was_sidebar_dragging = pane_resize.sidebar_dragging();
            let resize = match layout {
                MainLayout::Full(panes) => pane_resize.reduce(mouse, &hit_map, shell_state, panes),
                MainLayout::TooSmall(_) => PaneResizeOutcome::default(),
            };
            if resize.consumed {
                draw_gate.request_change(
                    resize.changed || was_sidebar_dragging != pane_resize.sidebar_dragging(),
                );
                if resize.changed {
                    resize_follow_cursor = true;
                }
                continue;
            }
            let mut input = mouse_input_for_event(mouse, &hit_map, &shell_state, &mouse_state);
            if input == Some(ShellInput::DismissCompletion) {
                session.clear_completion();
                observe_completion_trigger(
                    session,
                    &mut completion_trigger,
                    CompletionTriggerInput::Other,
                    input_now_ms,
                );
                draw_gate.request_change(true);
                input =
                    mouse_input_for_event(mouse, &hit_map, &ShellState::default(), &mouse_state);
            }
            match input {
                Some(ShellInput::SelectEditorDiagnostic {
                    line,
                    column,
                    diagnostic_index,
                }) => {
                    focus = WorkspaceFocus::Editor;
                    let command = EditorCommand::MoveTo {
                        line,
                        column,
                        selecting: false,
                    };
                    let outcome = match execute_editor_command(session, command.clone(), operations)
                    {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            status = format!("edit rejected: {error}").into();
                            draw_gate.request_change(true);
                            EditorOutcome::NoChange
                        }
                    };
                    observe_completion_trigger(
                        session,
                        &mut completion_trigger,
                        completion_trigger_input(&command, &outcome),
                        input_now_ms,
                    );
                    let selected = session.select_diagnostic_for_output(diagnostic_index);
                    draw_gate.request_change(outcome != EditorOutcome::NoChange || selected);
                    let (follow, quit) = apply_outcome(outcome, &mut status);
                    follow_cursor = follow;
                    if selected {
                        output_scroll = 0;
                    }
                    if quit {
                        break;
                    }
                }
                Some(ShellInput::Workspace(WorkspaceInput::Editor(SessionInput::Command(
                    command,
                )))) => {
                    focus = WorkspaceFocus::Editor;
                    let observed_command = command.clone();
                    let outcome = match execute_editor_command(session, command, operations) {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            status = format!("edit rejected: {error}").into();
                            draw_gate.request_change(true);
                            EditorOutcome::NoChange
                        }
                    };
                    observe_completion_trigger(
                        session,
                        &mut completion_trigger,
                        completion_trigger_input(&observed_command, &outcome),
                        input_now_ms,
                    );
                    draw_gate.request_change(outcome != EditorOutcome::NoChange);
                    let (follow, quit) = apply_outcome(outcome, &mut status);
                    follow_cursor = follow;
                    if quit {
                        break;
                    }
                }
                Some(ShellInput::ScrollEditor(delta)) => {
                    let changed = session
                        .workspace_mut()
                        .scroll_active_viewport(delta, editor_height);
                    draw_gate.request_change(changed);
                    if changed {
                        follow_cursor = false;
                    }
                }
                Some(ShellInput::ScrollEditorPage(pages)) => {
                    let delta = pages.saturating_mul(editor_height.max(1) as isize);
                    let changed = session
                        .workspace_mut()
                        .scroll_active_viewport(delta, editor_height);
                    draw_gate.request_change(changed);
                    if changed {
                        follow_cursor = false;
                    }
                }
                Some(ShellInput::SetEditorScroll(row)) => {
                    let changed = session.workspace_mut().set_active_viewport_from_track(
                        usize::from(row.saturating_sub(hit_map.editor_scrollbar_track.y)),
                        usize::from(hit_map.editor_scrollbar_track.height),
                        editor_height,
                    );
                    draw_gate.request_change(changed);
                    if changed {
                        follow_cursor = false;
                    }
                }
                Some(ShellInput::ScrollConsole(delta)) => {
                    draw_gate.request_change(console_scroll.scroll_by(delta));
                }
                Some(ShellInput::ScrollOutput(delta)) => {
                    let previous = output_scroll;
                    output_scroll = output_scroll
                        .saturating_add_signed(delta)
                        .min(maximum_output_scroll);
                    draw_gate.request_change(output_scroll != previous);
                }
                Some(ShellInput::ScrollOverlay(delta)) => {
                    if let Some(scroll) = &mut keybinds_scroll {
                        let changed = reduce_keybinds_scroll(scroll, delta, area.into());
                        draw_gate.request_change(changed);
                    } else if let Some(index) = &mut command_picker {
                        let previous = *index;
                        *index = index
                            .saturating_add_signed(delta.signum())
                            .min(COMMAND_MENU_ENTRIES.len().saturating_sub(1));
                        draw_gate.request_change(*index != previous);
                    }
                }
                Some(ShellInput::SelectTestCase(index)) => {
                    draw_gate.request_change(test_cases.select(index));
                }
                Some(ShellInput::RunTestCase(index)) => {
                    test_cases.select(index);
                    session.set_command_modal(false);
                    if let Some(case) = test_cases.begin_selected() {
                        view = WorkView::Workspace;
                        focus = WorkspaceFocus::Editor;
                        start_test_case_sequence(session, &mut test_cases, case);
                    }
                    draw_gate.request_change(true);
                }
                Some(ShellInput::ScrollTestCases(delta)) => {
                    draw_gate.request_change(test_cases.move_selection(delta.signum()));
                }
                Some(ShellInput::AcceptCompletion(index)) => {
                    let selected = session.select_completion(index);
                    status = if selected {
                        match session.accept_completion() {
                            Ok(true) => "completion accepted".into(),
                            Ok(false) => "completion became stale".into(),
                            Err(error) => format!("completion rejected: {error}").into(),
                        }
                    } else {
                        "completion became stale".into()
                    };
                    observe_completion_trigger(
                        session,
                        &mut completion_trigger,
                        CompletionTriggerInput::CompletionAcceptance,
                        input_now_ms,
                    );
                    follow_cursor = true;
                    draw_gate.request_change(true);
                }
                Some(ShellInput::ScrollCompletion(delta)) => {
                    let changed = session.scroll_completion(delta);
                    draw_gate.request_change(changed);
                }
                Some(ShellInput::DismissCompletion) => {
                    unreachable!("completion dismissal was processed before dispatch")
                }
                Some(ShellInput::CloseUpdateNotice) => {
                    update_menu.open = false;
                    draw_gate.request_change(true);
                }
                Some(ShellInput::CloseKeybinds) => {
                    keybinds_scroll = None;
                    draw_gate.request_change(true);
                }
                Some(ShellInput::SubmitFilePrompt) => {
                    apply_path_prompt_action(
                        session,
                        &mut path_prompt,
                        PathPromptAction::Submit,
                        &mut status,
                        &mut focus,
                        &mut follow_cursor,
                    );
                    draw_gate.request_change(true);
                }
                Some(ShellInput::CancelFilePrompt) => {
                    apply_path_prompt_action(
                        session,
                        &mut path_prompt,
                        PathPromptAction::Cancel,
                        &mut status,
                        &mut focus,
                        &mut follow_cursor,
                    );
                    draw_gate.request_change(true);
                }
                Some(ShellInput::OpenFind) => {
                    session.clear_completion();
                    find_panel = Some(begin_find_panel(session));
                    retire_paste_warning(&mut status);
                    draw_gate.request_change(true);
                }
                Some(ShellInput::FindNext) => {
                    apply_find_panel_action(
                        session,
                        &mut find_panel,
                        FindPanelAction::Next,
                        &mut status,
                        &mut follow_cursor,
                    );
                    draw_gate.request_change(true);
                }
                Some(ShellInput::Replace) => {
                    apply_find_panel_action(
                        session,
                        &mut find_panel,
                        FindPanelAction::Replace,
                        &mut status,
                        &mut follow_cursor,
                    );
                    draw_gate.request_change(true);
                }
                Some(ShellInput::ReplaceAll) => {
                    apply_find_panel_action(
                        session,
                        &mut find_panel,
                        FindPanelAction::ReplaceAll,
                        &mut status,
                        &mut follow_cursor,
                    );
                    draw_gate.request_change(true);
                }
                Some(ShellInput::CloseFind) => {
                    apply_find_panel_action(
                        session,
                        &mut find_panel,
                        FindPanelAction::Close,
                        &mut status,
                        &mut follow_cursor,
                    );
                    draw_gate.request_change(true);
                }
                Some(ShellInput::Workspace(input)) => {
                    draw_gate.request_change(true);
                    if apply_workspace_input(
                        session,
                        input,
                        &mut status,
                        &mut follow_cursor,
                        &mut find_panel,
                        &mut path_prompt,
                        &mut save_triggered_check,
                        operations,
                    ) {
                        break;
                    }
                }
                Some(ShellInput::ActivateFile(index)) => {
                    draw_gate.request_change(true);
                    focus = WorkspaceFocus::Editor;
                    observe_completion_trigger(
                        session,
                        &mut completion_trigger,
                        CompletionTriggerInput::BufferSwitch,
                        input_now_ms,
                    );
                    match session.workspace_mut().select_index(index) {
                        Ok(outcome) => apply_workspace_outcome(outcome, &mut status),
                        Err(error) => {
                            status = format!("file selection rejected: {error}").into();
                            continue;
                        }
                    }
                    match session.workspace_mut().activate_selected() {
                        Ok(outcome) => {
                            apply_workspace_outcome(outcome, &mut status);
                            focus = WorkspaceFocus::Editor;
                            follow_cursor = true;
                        }
                        Err(error) => status = format!("open rejected: {error}").into(),
                    }
                }
                Some(ShellInput::ActivateTab(path)) => {
                    draw_gate.request_change(true);
                    observe_completion_trigger(
                        session,
                        &mut completion_trigger,
                        CompletionTriggerInput::BufferSwitch,
                        input_now_ms,
                    );
                    let path = WorkspacePath::new(path)
                        .expect("rendered tab paths are validated workspace paths");
                    match session.workspace_mut().activate_path(&path) {
                        Ok(outcome) => {
                            apply_workspace_outcome(outcome, &mut status);
                            focus = WorkspaceFocus::Editor;
                            follow_cursor = true;
                        }
                        Err(error) => status = format!("tab activation rejected: {error}").into(),
                    }
                }
                Some(ShellInput::OpenMenu) => {
                    draw_gate.request_change(true);
                    update_menu.refresh();
                    command_picker = Some(0);
                }
                Some(ShellInput::ActivateMenu(index)) => {
                    draw_gate.request_change(true);
                    command_picker = None;
                    session.set_command_modal(false);
                    if activate_command_menu_entry(
                        session,
                        index,
                        &mut status,
                        &mut view,
                        &mut focus,
                        &mut test_cases,
                        &mut keybinds_scroll,
                        &mut update_menu,
                    ) {
                        break;
                    }
                }
                Some(ShellInput::CancelMenu) => {
                    draw_gate.request_change(true);
                    command_picker = None;
                    status = "command selection closed".into();
                }
                Some(ShellInput::OpenEditorContextMenu(anchor)) => {
                    session.clear_completion();
                    observe_completion_trigger(
                        session,
                        &mut completion_trigger,
                        CompletionTriggerInput::ModalEntry,
                        input_now_ms,
                    );
                    let has_selection = !session
                        .workspace()
                        .active_buffer()
                        .selection_state()
                        .is_caret();
                    editor_context_menu = Some(EditorContextMenuState::new(
                        anchor,
                        has_selection,
                        session.has_internal_clipboard(),
                    ));
                    session.set_command_modal(true);
                    status.clear();
                    draw_gate.request_change(true);
                }
                Some(ShellInput::ActivateEditorContextMenu(index)) => {
                    if let Some((command, outcome)) = activate_editor_context_menu_entry(
                        session,
                        &mut editor_context_menu,
                        index,
                        &mut status,
                        operations,
                    ) {
                        observe_completion_trigger(
                            session,
                            &mut completion_trigger,
                            completion_trigger_input(&command, &outcome),
                            input_now_ms,
                        );
                        let (follow, _) = apply_outcome(outcome, &mut status);
                        follow_cursor = follow;
                        draw_gate.request_change(true);
                    }
                }
                Some(ShellInput::CancelEditorContextMenu) => {
                    editor_context_menu = None;
                    status = "clipboard menu closed".into();
                    draw_gate.request_change(true);
                }
                Some(ShellInput::ScrollEditorContextMenu(delta)) => {
                    let changed = editor_context_menu
                        .as_mut()
                        .is_some_and(|menu| menu.move_selection(delta));
                    draw_gate.request_change(changed);
                }
                Some(ShellInput::OpenFilesContextMenu(anchor, target)) => {
                    session.clear_completion();
                    observe_completion_trigger(
                        session,
                        &mut completion_trigger,
                        CompletionTriggerInput::ModalEntry,
                        input_now_ms,
                    );
                    open_files_context_menu(
                        session,
                        anchor,
                        target,
                        &mut files_context_menu,
                        &mut status,
                    );
                    draw_gate.request_change(true);
                }
                Some(ShellInput::ActivateFilesContextMenu(index)) => {
                    if let Some((action, _)) = activate_files_context_menu_entry(
                        session,
                        &mut files_context_menu,
                        index,
                        &mut status,
                        &mut focus,
                        &mut follow_cursor,
                        &mut path_prompt,
                    ) {
                        observe_completion_trigger(
                            session,
                            &mut completion_trigger,
                            if action == FilesContextMenuAction::Open {
                                CompletionTriggerInput::BufferSwitch
                            } else {
                                CompletionTriggerInput::ModalEntry
                            },
                            input_now_ms,
                        );
                        draw_gate.request_change(true);
                    }
                }
                Some(ShellInput::CancelFilesContextMenu) => {
                    files_context_menu = None;
                    session.set_command_modal(false);
                    status = "files menu closed".into();
                    draw_gate.request_change(true);
                }
                Some(ShellInput::ScrollFilesContextMenu(delta)) => {
                    let changed = files_context_menu
                        .as_mut()
                        .is_some_and(|menu| menu.move_selection(delta));
                    draw_gate.request_change(changed);
                }
                Some(ShellInput::FocusConsole) => {
                    draw_gate.request_change(true);
                    session.clear_completion();
                    view = WorkView::Console;
                    focus = WorkspaceFocus::Console;
                    status.clear();
                }
                Some(ShellInput::SelectDiagnostic(index)) => {
                    draw_gate.request_change(true);
                    observe_completion_trigger(
                        session,
                        &mut completion_trigger,
                        CompletionTriggerInput::CursorMovement,
                        input_now_ms,
                    );
                    let navigation = session.select_diagnostic_index(index);
                    let navigated =
                        matches!(&navigation, Ok(DiagnosticNavigation::Navigated { .. }));
                    status = diagnostic_navigation_status(navigation).into();
                    if navigated {
                        focus = WorkspaceFocus::Editor;
                        follow_cursor = true;
                        output_scroll = 0;
                    }
                }
                Some(ShellInput::ConfirmModal) => {
                    draw_gate.request_change(true);
                    status = match session.confirm_console_overwrite() {
                        Ok(()) => "console command preparation started".into(),
                        Err(error) => format!("console command rejected: {error}").into(),
                    };
                }
                Some(ShellInput::CancelModal) => {
                    draw_gate.request_change(true);
                    session.cancel_console_overwrite();
                    status = "console overwrite cancelled; output preserved".into();
                }
                None => {}
            }
            continue;
        }
        if update_menu.open {
            if matches!(&event, Event::Key(key) if key.kind != KeyEventKind::Release && matches!(key.code, KeyCode::Esc | KeyCode::Enter))
            {
                update_menu.open = false;
                draw_gate.request_change(true);
            }
            continue;
        }
        if files_context_menu.is_some() {
            let action = match &event {
                Event::Key(key) => files_context_menu_key_action(
                    files_context_menu
                        .as_mut()
                        .expect("files context menu checked above"),
                    *key,
                ),
                _ => FilesContextMenuKeyAction::None,
            };
            match action {
                FilesContextMenuKeyAction::None => {}
                FilesContextMenuKeyAction::SelectionChanged => {
                    draw_gate.request_change(true);
                }
                FilesContextMenuKeyAction::Activate(index) => {
                    if let Some((action, _)) = activate_files_context_menu_entry(
                        session,
                        &mut files_context_menu,
                        index,
                        &mut status,
                        &mut focus,
                        &mut follow_cursor,
                        &mut path_prompt,
                    ) {
                        observe_completion_trigger(
                            session,
                            &mut completion_trigger,
                            if action == FilesContextMenuAction::Open {
                                CompletionTriggerInput::BufferSwitch
                            } else {
                                CompletionTriggerInput::ModalEntry
                            },
                            input_now_ms,
                        );
                        draw_gate.request_change(true);
                    }
                }
                FilesContextMenuKeyAction::Cancel => {
                    files_context_menu = None;
                    session.set_command_modal(false);
                    status = "files menu closed".into();
                    draw_gate.request_change(true);
                }
            }
            continue;
        }
        if editor_context_menu.is_some() {
            let action = match &event {
                Event::Key(key) => editor_context_menu_key_action(
                    editor_context_menu
                        .as_mut()
                        .expect("context menu checked above"),
                    *key,
                ),
                _ => EditorContextMenuKeyAction::None,
            };
            match action {
                EditorContextMenuKeyAction::None => {}
                EditorContextMenuKeyAction::SelectionChanged => {
                    draw_gate.request_change(true);
                }
                EditorContextMenuKeyAction::Activate(index) => {
                    if let Some((command, outcome)) = activate_editor_context_menu_entry(
                        session,
                        &mut editor_context_menu,
                        index,
                        &mut status,
                        operations,
                    ) {
                        observe_completion_trigger(
                            session,
                            &mut completion_trigger,
                            completion_trigger_input(&command, &outcome),
                            input_now_ms,
                        );
                        let (follow, _) = apply_outcome(outcome, &mut status);
                        follow_cursor = follow;
                        draw_gate.request_change(true);
                    }
                }
                EditorContextMenuKeyAction::Cancel => {
                    editor_context_menu = None;
                    status = "clipboard menu closed".into();
                    draw_gate.request_change(true);
                }
            }
            continue;
        }
        if let Some(scroll) = &mut keybinds_scroll {
            if let Some(delta) = keybinds_scroll_delta(&event) {
                let changed = reduce_keybinds_scroll(scroll, delta, area.into());
                draw_gate.request_change(changed);
            } else if matches!(&event, Event::Key(key) if key.kind != KeyEventKind::Release && matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::F(1)))
            {
                keybinds_scroll = None;
            }
            continue;
        }
        let view_switch_available = find_panel.is_none()
            && path_prompt.is_none()
            && command_picker.is_none()
            && editor_context_menu.is_none()
            && files_context_menu.is_none()
            && !update_menu.open
            && keybinds_scroll.is_none()
            && !session.workspace().confirmation_pending()
            && !session.console_overwrite_pending();
        let test_case_views_available =
            test_case_view_switch_available(session.test_case_active(), &test_cases);
        if test_cases.is_open() {
            match test_case_picker_key_action(&mut test_cases, &event) {
                TestCasePickerKeyAction::None => {}
                TestCasePickerKeyAction::Close => {
                    test_cases.close_and_restore(&mut view, &mut focus);
                    status.clear();
                }
                TestCasePickerKeyAction::Run => {
                    session.set_command_modal(false);
                    if let Some(case) = test_cases.begin_selected() {
                        view = WorkView::Workspace;
                        focus = WorkspaceFocus::Editor;
                        start_test_case_sequence(session, &mut test_cases, case);
                    }
                }
                TestCasePickerKeyAction::Refresh => {
                    let _ = test_cases.refresh(
                        session.workspace().root(),
                        session.metadata().test_case_suite_hash.is_some(),
                    );
                }
            }
            continue;
        }
        if view_switch_available
            && test_case_views_available
            && matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Press && key.modifiers.is_empty() && key.code == KeyCode::F(1))
        {
            session.clear_completion();
            keybinds_scroll = Some(0);
            continue;
        }
        if view_switch_available
            && test_case_views_available
            && matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Press && key.modifiers.is_empty() && key.code == KeyCode::F(7))
        {
            session.clear_completion();
            update_menu.refresh();
            command_picker = Some(0);
            continue;
        }
        if view_switch_available
            && test_case_views_available
            && matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Press && key.modifiers.is_empty() && key.code == KeyCode::F(9))
        {
            session.clear_completion();
            view = WorkView::Console;
            focus = WorkspaceFocus::Console;
            status.clear();
            continue;
        }
        if view_switch_available
            && !command_active
            && matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Press && key.modifiers.is_empty() && key.code == KeyCode::F(4))
        {
            session.clear_completion();
            let _ = test_cases.refresh(
                session.workspace().root(),
                session.metadata().test_case_suite_hash.is_some(),
            );
            test_cases.open_from(view, focus);
            status.clear();
            continue;
        }
        if view_switch_available
            && !command_active
            && matches!(&event, Event::Key(key)
                if key.kind != KeyEventKind::Release
                    && has_exact_primary_modifier(key.modifiers, primary_modifier)
                    && matches!(key.code, KeyCode::Char('f' | 'F')))
        {
            session.clear_completion();
            find_panel = Some(begin_find_panel(session));
            retire_paste_warning(&mut status);
            continue;
        }
        if session.console_overwrite_pending() {
            if let Event::Key(key) = &event
                && key.kind != KeyEventKind::Release
            {
                match key.code {
                    KeyCode::Enter | KeyCode::Char('y' | 'Y') => {
                        status = match session.confirm_console_overwrite() {
                            Ok(()) => "console command preparation started".into(),
                            Err(error) => format!("console command rejected: {error}").into(),
                        };
                    }
                    KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                        session.cancel_console_overwrite();
                        status = "console overwrite cancelled; output preserved".into();
                    }
                    _ => {}
                }
            }
            continue;
        }
        let modal_keyboard_active = find_panel.is_some()
            || path_prompt.is_some()
            || command_picker.is_some()
            || editor_context_menu.is_some()
            || files_context_menu.is_some()
            || update_menu.open
            || keybinds_scroll.is_some()
            || test_cases.is_open()
            || selected_completion.is_some()
            || session.workspace().confirmation_pending();
        let global_delete_shortcut = matches!(
            &event,
            Event::Key(key)
                if key.kind != KeyEventKind::Release
                    && has_exact_primary_modifier(key.modifiers, primary_modifier)
                    && matches!(key.code, KeyCode::Char('w' | 'W'))
        );
        if focus == WorkspaceFocus::Console && !modal_keyboard_active && !global_delete_shortcut {
            if let Event::Key(key) = &event {
                handle_console_key(
                    session,
                    key,
                    &mut console_line,
                    &mut console_scroll,
                    &mut view,
                    &mut focus,
                    &mut status,
                );
            }
            continue;
        }
        if command_active && !modal_keyboard_active {
            if matches!(
                workspace_input_for_event_with_keyboard_enhancement(
                    event.clone(),
                    editor_height,
                    focus,
                    session.workspace().confirmation_pending(),
                    primary_modifier,
                    keyboard_enhancement_active,
                    &ghostty_key_bindings,
                ),
                Some(WorkspaceInput::Editor(SessionInput::Save))
            ) {
                status.replace(crate::session::SAVE_CHECK_BUSY_WARNING);
            }
            if !session.console_command_active()
                && matches!(&event, Event::Key(key) if key.kind != KeyEventKind::Release && key.code == KeyCode::Esc)
            {
                if session.test_case_active() {
                    test_cases.cancel_queue();
                }
                session.cancel_command();
            }
            continue;
        }
        if let Event::Key(key) = &event {
            match completion_key_action(session.selected_completion().is_some(), *key) {
                CompletionKeyAction::Move(delta) => {
                    session.move_completion(delta);
                    continue;
                }
                CompletionKeyAction::Accept => {
                    status = match session.accept_completion() {
                        Ok(true) => "completion accepted".into(),
                        Ok(false) => "completion became stale".into(),
                        Err(error) => format!("completion rejected: {error}").into(),
                    };
                    observe_completion_trigger(
                        session,
                        &mut completion_trigger,
                        CompletionTriggerInput::CompletionAcceptance,
                        input_now_ms,
                    );
                    follow_cursor = true;
                    continue;
                }
                CompletionKeyAction::Close => {
                    session.clear_completion();
                    observe_completion_trigger(
                        session,
                        &mut completion_trigger,
                        CompletionTriggerInput::Other,
                        input_now_ms,
                    );
                    status = "completion closed".into();
                    continue;
                }
                CompletionKeyAction::CloseAndPassThrough => session.clear_completion(),
                CompletionKeyAction::PassThrough => {}
            }
        }
        if matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Press && key.code == KeyCode::F(8) && key.modifiers.is_empty())
        {
            observe_completion_trigger(
                session,
                &mut completion_trigger,
                CompletionTriggerInput::Other,
                input_now_ms,
            );
            if find_panel.is_none()
                && path_prompt.is_none()
                && command_picker.is_none()
                && files_context_menu.is_none()
            {
                match session.reload_language_service() {
                    Ok(true) => status = "language service reload/restart requested".into(),
                    Ok(false) => {}
                    Err(_) => status = "language service reload unavailable".into(),
                }
            }
            continue;
        }
        let workspace = session.workspace_mut();
        if find_panel.is_none()
            && path_prompt.is_none()
            && command_picker.is_none()
            && files_context_menu.is_none()
            && !workspace.confirmation_pending()
            && let Some(delta) = diagnostic_delta_for_event(&event)
        {
            observe_completion_trigger(
                session,
                &mut completion_trigger,
                CompletionTriggerInput::CursorMovement,
                input_now_ms,
            );
            let navigation = session.select_diagnostic(delta);
            output_scroll = diagnostic_output_scroll(output_scroll, &navigation);
            if matches!(&navigation, Ok(DiagnosticNavigation::Navigated { .. })) {
                focus = WorkspaceFocus::Editor;
                follow_cursor = true;
            }
            status = diagnostic_navigation_status(navigation).into();
            continue;
        }
        if let Some(index) = &mut command_picker {
            if let Event::Key(key) = &event
                && key.kind != KeyEventKind::Release
            {
                match key.code {
                    KeyCode::Left | KeyCode::Up => {
                        *index =
                            (*index + COMMAND_MENU_ENTRIES.len() - 1) % COMMAND_MENU_ENTRIES.len()
                    }
                    KeyCode::Right | KeyCode::Down => {
                        *index = (*index + 1) % COMMAND_MENU_ENTRIES.len()
                    }
                    KeyCode::Esc => {
                        command_picker = None;
                        status = "command selection closed".into();
                    }
                    KeyCode::Enter => {
                        let index = *index;
                        command_picker = None;
                        session.set_command_modal(false);
                        if activate_command_menu_entry(
                            session,
                            index,
                            &mut status,
                            &mut view,
                            &mut focus,
                            &mut test_cases,
                            &mut keybinds_scroll,
                            &mut update_menu,
                        ) {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            continue;
        }
        if find_panel.is_some() {
            let action =
                route_find_panel_event(&mut find_panel, event).expect("find panel checked above");
            apply_find_panel_action(
                session,
                &mut find_panel,
                action,
                &mut status,
                &mut follow_cursor,
            );
            continue;
        }

        if path_prompt.is_some() {
            let action = path_prompt
                .as_mut()
                .expect("path prompt is active")
                .update(event);
            apply_path_prompt_action(
                session,
                &mut path_prompt,
                action,
                &mut status,
                &mut focus,
                &mut follow_cursor,
            );
            continue;
        }

        let Some(input) = workspace_input_for_event_with_keyboard_enhancement(
            event,
            editor_height,
            focus,
            workspace.confirmation_pending(),
            primary_modifier,
            keyboard_enhancement_active,
            &ghostty_key_bindings,
        ) else {
            continue;
        };
        match input {
            WorkspaceInput::Editor(SessionInput::BeginFind) => {
                observe_completion_trigger(
                    session,
                    &mut completion_trigger,
                    CompletionTriggerInput::ModalEntry,
                    input_now_ms,
                );
                find_panel = Some(begin_find_panel(session));
                retire_paste_warning(&mut status);
            }
            WorkspaceInput::Editor(SessionInput::Save) => {
                observe_completion_trigger(
                    session,
                    &mut completion_trigger,
                    CompletionTriggerInput::Other,
                    input_now_ms,
                );
                status.replace(explicit_save_status(session, &mut save_triggered_check));
            }
            WorkspaceInput::Editor(SessionInput::Complete) => {
                observe_completion_trigger(
                    session,
                    &mut completion_trigger,
                    CompletionTriggerInput::Other,
                    input_now_ms,
                );
                status = match session.trigger_completion() {
                    Ok(true) => "completion requested".into(),
                    Ok(false) => "completion unavailable".into(),
                    Err(error) => format!("completion unavailable: {error}").into(),
                };
            }
            WorkspaceInput::Editor(SessionInput::Command(command)) => {
                let observed_command = command.clone();
                let outcome = match execute_editor_command(session, command, operations) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        status = format!("edit rejected: {error}").into();
                        EditorOutcome::NoChange
                    }
                };
                observe_completion_trigger(
                    session,
                    &mut completion_trigger,
                    completion_trigger_input(&observed_command, &outcome),
                    input_now_ms,
                );
                let (follow, quit) = apply_outcome(outcome, &mut status);
                follow_cursor = follow;
                if quit {
                    break;
                }
            }
            WorkspaceInput::BeginCreate => {
                observe_completion_trigger(
                    session,
                    &mut completion_trigger,
                    CompletionTriggerInput::ModalEntry,
                    input_now_ms,
                );
                path_prompt = Some(begin_path_prompt(session, PathOperation::Create));
                retire_paste_warning(&mut status);
            }
            WorkspaceInput::BeginRename => {
                observe_completion_trigger(
                    session,
                    &mut completion_trigger,
                    CompletionTriggerInput::ModalEntry,
                    input_now_ms,
                );
                path_prompt = Some(begin_path_prompt(session, PathOperation::Rename));
                retire_paste_warning(&mut status);
            }
            WorkspaceInput::DeleteSelected => {
                observe_completion_trigger(
                    session,
                    &mut completion_trigger,
                    CompletionTriggerInput::ModalEntry,
                    input_now_ms,
                );
                match session.delete_selected() {
                    Ok(outcome) => apply_workspace_outcome(outcome, &mut status),
                    Err(error) => status = format!("delete rejected: {error}").into(),
                }
            }
            WorkspaceInput::ConfirmDestructive => {
                observe_completion_trigger(
                    session,
                    &mut completion_trigger,
                    CompletionTriggerInput::Other,
                    input_now_ms,
                );
                if session.workspace().delete_confirmation_pending() {
                    match session.confirm_delete() {
                        Ok(outcome) => {
                            apply_workspace_outcome(outcome, &mut status);
                            follow_cursor = true;
                        }
                        Err(error) => status = format!("delete failed: {error}").into(),
                    }
                } else {
                    let outcome = execute_workspace_with_status(
                        session.workspace_mut(),
                        EditorCommand::ConfirmDiscard,
                        &mut status,
                    );
                    let (follow, quit) = apply_outcome(outcome, &mut status);
                    follow_cursor = follow;
                    if quit {
                        break;
                    }
                }
            }
            WorkspaceInput::CancelDestructive => {
                observe_completion_trigger(
                    session,
                    &mut completion_trigger,
                    CompletionTriggerInput::Other,
                    input_now_ms,
                );
                if session.workspace().delete_confirmation_pending() {
                    let outcome = session.workspace_mut().cancel_delete();
                    apply_workspace_outcome(outcome, &mut status);
                } else {
                    let outcome = execute_workspace_with_status(
                        session.workspace_mut(),
                        EditorCommand::CancelDiscard,
                        &mut status,
                    );
                    let (follow, _) = apply_outcome(outcome, &mut status);
                    follow_cursor = follow;
                }
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_workspace_input(
    session: &mut crate::session::ProductionSession,
    input: WorkspaceInput,
    status: &mut StatusMessage,
    follow_cursor: &mut bool,
    find_panel: &mut Option<FindPanel>,
    path_prompt: &mut Option<PathPrompt>,
    save_triggered_check: &mut bool,
    operations: &mut impl TerminalOperations,
) -> bool {
    match input {
        WorkspaceInput::Editor(SessionInput::BeginFind) => {
            *find_panel = Some(begin_find_panel(session));
            retire_paste_warning(status);
        }
        WorkspaceInput::Editor(SessionInput::Save) => {
            status.replace(explicit_save_status(session, save_triggered_check));
        }
        WorkspaceInput::Editor(SessionInput::Complete) => {
            status.replace(match session.trigger_completion() {
                Ok(true) => "completion requested".into(),
                Ok(false) => "completion unavailable".into(),
                Err(error) => format!("completion unavailable: {error}"),
            });
        }
        WorkspaceInput::Editor(SessionInput::Command(command)) => {
            let outcome = match execute_editor_command(session, command, operations) {
                Ok(outcome) => outcome,
                Err(error) => {
                    status.replace(format!("edit rejected: {error}"));
                    EditorOutcome::NoChange
                }
            };
            let (follow, quit) = apply_outcome(outcome, status);
            *follow_cursor = follow;
            return quit;
        }
        WorkspaceInput::BeginCreate => {
            *path_prompt = Some(begin_path_prompt(session, PathOperation::Create));
            retire_paste_warning(status);
        }
        WorkspaceInput::BeginRename => {
            *path_prompt = Some(begin_path_prompt(session, PathOperation::Rename));
            retire_paste_warning(status);
        }
        WorkspaceInput::DeleteSelected => match session.delete_selected() {
            Ok(outcome) => apply_workspace_outcome(outcome, status),
            Err(error) => status.replace(format!("delete rejected: {error}")),
        },
        WorkspaceInput::ConfirmDestructive => {
            if session.workspace().delete_confirmation_pending() {
                match session.confirm_delete() {
                    Ok(outcome) => {
                        apply_workspace_outcome(outcome, status);
                        *follow_cursor = true;
                    }
                    Err(error) => status.replace(format!("delete failed: {error}")),
                }
            } else {
                let outcome = execute_workspace_with_status(
                    session.workspace_mut(),
                    EditorCommand::ConfirmDiscard,
                    status,
                );
                let (follow, quit) = apply_outcome(outcome, status);
                *follow_cursor = follow;
                return quit;
            }
        }
        WorkspaceInput::CancelDestructive => {
            if session.workspace().delete_confirmation_pending() {
                let outcome = session.workspace_mut().cancel_delete();
                apply_workspace_outcome(outcome, status);
            } else {
                let outcome = execute_workspace_with_status(
                    session.workspace_mut(),
                    EditorCommand::CancelDiscard,
                    status,
                );
                let (follow, _) = apply_outcome(outcome, status);
                *follow_cursor = follow;
            }
        }
    }
    false
}

fn begin_path_prompt(
    session: &crate::session::ProductionSession,
    operation: PathOperation,
) -> PathPrompt {
    let initial = match operation {
        PathOperation::Create => "",
        PathOperation::Rename => session.workspace().selected_path().as_str(),
    };
    PathPrompt::new(operation, initial)
}

fn apply_path_prompt_action(
    session: &mut crate::session::ProductionSession,
    path_prompt: &mut Option<PathPrompt>,
    action: PathPromptAction,
    status: &mut StatusMessage,
    focus: &mut WorkspaceFocus,
    follow_cursor: &mut bool,
) {
    match action {
        PathPromptAction::Continue => {}
        PathPromptAction::Edited => retire_paste_warning(status),
        PathPromptAction::Cancel => {
            *path_prompt = None;
            status.replace("file operation cancelled");
        }
        PathPromptAction::Submit => {
            let prompt = path_prompt.take().expect("path prompt is active");
            let path = submitted_path(&prompt);
            let result = match prompt.operation {
                PathOperation::Create => session.create_file(&path),
                PathOperation::Rename => session.rename_selected(&path),
            };
            match result {
                Ok(outcome) => {
                    apply_workspace_outcome(outcome, status);
                    *follow_cursor = *focus == WorkspaceFocus::Editor;
                }
                Err(error) => status.replace(format!("file operation rejected: {error}")),
            }
        }
        PathPromptAction::RejectedTooLong => {
            *path_prompt = None;
            status.replace(format!(
                "file operation rejected: path exceeds {MAX_WORKSPACE_PATH_BYTES} bytes"
            ));
        }
        PathPromptAction::RejectedUnsafe => {
            *path_prompt = None;
            status.replace("file operation rejected: path contains a terminal control character");
        }
    }
}

fn submitted_path(prompt: &PathPrompt) -> String {
    match prompt.operation {
        PathOperation::Create => format!("src/{}", prompt.input),
        PathOperation::Rename => prompt.input.clone(),
    }
}

fn activate_editor_context_menu_entry(
    session: &mut crate::session::ProductionSession,
    menu: &mut Option<EditorContextMenuState>,
    index: usize,
    status: &mut StatusMessage,
    operations: &mut impl TerminalOperations,
) -> Option<(EditorCommand, EditorOutcome)> {
    let command = menu.as_ref()?.command(index)?;
    *menu = None;
    session.set_command_modal(false);
    let outcome = match execute_editor_command(session, command.clone(), operations) {
        Ok(outcome) => outcome,
        Err(error) => {
            status.replace(format!("edit rejected: {error}"));
            EditorOutcome::NoChange
        }
    };
    Some((command, outcome))
}

fn open_files_context_menu(
    session: &mut crate::session::ProductionSession,
    anchor: ratatui::layout::Position,
    target: Option<usize>,
    menu: &mut Option<FilesContextMenuState>,
    status: &mut StatusMessage,
) -> WorkspaceOutcome {
    let outcome = if let Some(index) = target {
        match session.workspace_mut().select_index(index) {
            Ok(outcome) => outcome,
            Err(error) => {
                status.replace(format!("file selection rejected: {error}"));
                return WorkspaceOutcome::NoChange;
            }
        }
    } else {
        WorkspaceOutcome::NoChange
    };
    *menu = Some(FilesContextMenuState::new(anchor, target.is_some()));
    session.set_command_modal(true);
    status.clear();
    outcome
}

#[allow(clippy::too_many_arguments)]
fn activate_files_context_menu_entry(
    session: &mut crate::session::ProductionSession,
    menu: &mut Option<FilesContextMenuState>,
    index: usize,
    status: &mut StatusMessage,
    focus: &mut WorkspaceFocus,
    follow_cursor: &mut bool,
    path_prompt: &mut Option<PathPrompt>,
) -> Option<(FilesContextMenuAction, WorkspaceOutcome)> {
    let action = menu.as_ref()?.action(index)?;
    *menu = None;
    session.set_command_modal(false);
    let outcome = match action {
        FilesContextMenuAction::Open => match session.workspace_mut().activate_selected() {
            Ok(outcome) => {
                apply_workspace_outcome(outcome, status);
                *focus = WorkspaceFocus::Editor;
                *follow_cursor = true;
                outcome
            }
            Err(error) => {
                status.replace(format!("open rejected: {error}"));
                WorkspaceOutcome::NoChange
            }
        },
        FilesContextMenuAction::Rename => {
            *path_prompt = Some(begin_path_prompt(session, PathOperation::Rename));
            retire_paste_warning(status);
            WorkspaceOutcome::NoChange
        }
        FilesContextMenuAction::Delete => match session.delete_selected() {
            Ok(outcome) => {
                apply_workspace_outcome(outcome, status);
                outcome
            }
            Err(error) => {
                status.replace(format!("delete rejected: {error}"));
                WorkspaceOutcome::NoChange
            }
        },
        FilesContextMenuAction::NewFile => {
            *path_prompt = Some(begin_path_prompt(session, PathOperation::Create));
            retire_paste_warning(status);
            WorkspaceOutcome::NoChange
        }
    };
    Some((action, outcome))
}

fn execute_editor_command(
    session: &mut crate::session::ProductionSession,
    command: EditorCommand,
    operations: &mut impl TerminalOperations,
) -> Result<EditorOutcome, Box<dyn Error>> {
    let mirrors_clipboard = matches!(command, EditorCommand::Copy | EditorCommand::Cut);
    let outcome = session.execute(command)?;
    if mirrors_clipboard
        && outcome != EditorOutcome::NoChange
        && let Ok(Some(text)) = session.live_clipboard_text()
    {
        let _ = operations.write_system_clipboard(&text);
    }
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
fn activate_command_menu_entry(
    session: &mut crate::session::ProductionSession,
    index: usize,
    status: &mut StatusMessage,
    view: &mut WorkView,
    focus: &mut WorkspaceFocus,
    test_cases: &mut TestCasePicker,
    keybinds_scroll: &mut Option<usize>,
    update_menu: &mut UpdateMenu,
) -> bool {
    let Some(action) = command_menu_action(index) else {
        status.replace("command selection is no longer available");
        return false;
    };
    match action {
        CommandMenuAction::Check
        | CommandMenuAction::Run
        | CommandMenuAction::Clippy
        | CommandMenuAction::Format
        | CommandMenuAction::Doc => {
            *view = WorkView::Workspace;
            *focus = WorkspaceFocus::Editor;
            let action = match action {
                CommandMenuAction::Check => CargoAction::Check,
                CommandMenuAction::Run => CargoAction::Run,
                CommandMenuAction::Clippy => CargoAction::Clippy,
                CommandMenuAction::Format => CargoAction::Format,
                CommandMenuAction::Doc => CargoAction::Doc,
                _ => unreachable!("matched only Cargo menu actions"),
            };
            status.replace(match session.start_command(action) {
                Ok(()) => String::from("command preparation started"),
                Err(_) => String::from(
                    "Command unavailable; finish recovery/modal or check configured tools",
                ),
            });
        }
        CommandMenuAction::Update => {
            *view = WorkView::Workspace;
            *focus = WorkspaceFocus::Editor;
            status.replace(match session.start_dependency_command(CargoAction::Update, None) {
                Ok(()) => String::from("dependency update preparation started"),
                Err(_) => {
                    String::from(
                        "Dependency update unavailable; finish recovery/modal or check configured tools",
                    )
                }
            });
        }
        CommandMenuAction::UpdateRustrace => {
            update_menu.open = true;
        }
        CommandMenuAction::AutomaticChecks => {
            update_menu.toggle_with(crate::update::set_checks_enabled);
        }
        CommandMenuAction::Console => {
            session.clear_completion();
            *view = WorkView::Console;
            *focus = WorkspaceFocus::Console;
            status.clear();
        }
        CommandMenuAction::TestCases if !session.command_active() => {
            session.clear_completion();
            let _ = test_cases.refresh(
                session.workspace().root(),
                session.metadata().test_case_suite_hash.is_some(),
            );
            test_cases.open_from(*view, *focus);
            status.clear();
        }
        CommandMenuAction::TestCases => {}
        CommandMenuAction::Keybinds => {
            *keybinds_scroll = Some(0);
            status.clear();
        }
        CommandMenuAction::Quit => {
            return request_quit(session, status);
        }
    }
    false
}

fn request_quit(
    session: &mut crate::session::ProductionSession,
    status: &mut StatusMessage,
) -> bool {
    let outcome = match session.execute(EditorCommand::RequestQuit) {
        Ok(outcome) => outcome,
        Err(error) => {
            status.replace(format!("edit rejected: {error}"));
            EditorOutcome::NoChange
        }
    };
    apply_outcome(outcome, status).1
}

fn diagnostic_navigation_status(result: Result<DiagnosticNavigation, Box<dyn Error>>) -> String {
    match result {
        Ok(DiagnosticNavigation::Navigated { .. }) => String::new(),
        Ok(DiagnosticNavigation::NoDiagnostics) => "no diagnostics to navigate".into(),
        Ok(DiagnosticNavigation::MissingTarget) => {
            "diagnostic has no source target; no navigation performed".into()
        }
        Ok(DiagnosticNavigation::UnmanagedTarget) => {
            "diagnostic target is not an editable managed file; no navigation performed".into()
        }
        Ok(DiagnosticNavigation::OutsideWorkspace) => {
            "diagnostic target is outside this workspace; no navigation performed".into()
        }
        Ok(DiagnosticNavigation::InvalidCoordinates) => {
            "diagnostic coordinates are invalid for current source; no navigation performed".into()
        }
        Ok(DiagnosticNavigation::Stale) => {
            "diagnostic is stale after an edit; no navigation performed".into()
        }
        Err(error) => format!("diagnostic navigation failed: {error}"),
    }
}

fn diagnostic_output_scroll(
    current: usize,
    result: &Result<DiagnosticNavigation, Box<dyn Error>>,
) -> usize {
    if matches!(result, Ok(DiagnosticNavigation::Navigated { .. })) {
        0
    } else {
        current
    }
}

const EXPLICIT_SAVE_SUCCESS: &str = "File saved";

fn explicit_save_status(
    session: &mut crate::session::ProductionSession,
    save_triggered_check: &mut bool,
) -> String {
    match session.save_all_and_check() {
        Ok(crate::session::ExplicitSaveOutcome::CheckStarted) => {
            *save_triggered_check = true;
            EXPLICIT_SAVE_SUCCESS.into()
        }
        Ok(crate::session::ExplicitSaveOutcome::RunnerBusy) => {
            crate::session::SAVE_CHECK_BUSY_WARNING.into()
        }
        Ok(crate::session::ExplicitSaveOutcome::CheckFailedToStart(error)) => {
            format!("saved exact buffer bytes; Check failed to start: {error}")
        }
        Err(error) => format!("save failed; buffer remains dirty: {error}"),
    }
}

fn command_completion_status(status: &str, save_triggered_check: &mut bool) -> String {
    let successful = matches!(
        status,
        "Command: exit 0; capture recorded"
            | "Console: exit 0; evidence recorded"
            | "Format: changes recorded and saved"
            | "Format: no changes"
    ) || status.starts_with("Doc: generated ");
    if std::mem::take(save_triggered_check) || successful {
        String::new()
    } else {
        status.to_owned()
    }
}

#[allow(clippy::too_many_arguments)]
fn command_tick_status(
    tick_error: Option<&dyn Error>,
    completed_test_case: bool,
    was_command_active: bool,
    command_active: bool,
    completed_format_rejection: bool,
    command_status: &str,
    save_triggered_check: &mut bool,
) -> Option<String> {
    if let Some(error) = tick_error {
        return Some(if was_command_active && *save_triggered_check {
            *save_triggered_check = false;
            String::new()
        } else if was_command_active && completed_format_rejection {
            command_status.to_owned()
        } else if was_command_active {
            "Command stopped; inspect evidence or tool installation; Ctrl-Q quits".to_owned()
        } else {
            format!("recovery required: {error}; Ctrl-Q preserves session")
        });
    }
    (!completed_test_case && was_command_active && !command_active)
        .then(|| command_completion_status(command_status, save_triggered_check))
}

struct EditorStartupConfig {
    modifier: crate::config::ModifierPreference,
    theme: crate::tui::theme::ThemeConfig,
    warning: Option<String>,
}

fn run_editor_with<O, F>(
    mut session: crate::session::ProductionSession,
    assignment_title: &str,
    operations: O,
    startup: EditorStartupConfig,
    ghostty_key_bindings: crate::ghostty::GhosttyKeyBindings,
    mut editor_loop: F,
) -> Result<(), Box<dyn Error>>
where
    O: TerminalOperations,
    F: FnMut(
        &mut crate::session::ProductionSession,
        &str,
        bool,
        EditorKeyConfig,
        Option<&str>,
        &crate::tui::theme::Palette,
        &mut O,
    ) -> Result<(), Box<dyn Error>>,
{
    let EditorStartupConfig {
        modifier,
        theme,
        warning,
    } = startup;
    let mut terminal_session = match TerminalSession::enter(operations) {
        Ok(terminal_session) => terminal_session,
        Err(error) => {
            session.quit()?;
            return Err(error.into());
        }
    };
    let modifier = crate::config::resolve_primary_modifier(
        modifier,
        cfg!(target_os = "macos"),
        terminal_session.keyboard_enhancement_supported(),
    );
    let colorterm = std::env::var("COLORTERM").ok();
    let theme = terminal_session.resolve_theme(&theme, colorterm.as_deref());
    let startup_notice = warning.or(modifier.warning);
    let keyboard_enhancement_active = terminal_session.keyboard_enhancement_active();
    let key_config = EditorKeyConfig {
        primary_modifier: modifier.effective,
        keyboard_enhancement_active,
        ghostty_key_bindings,
    };
    let initial = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        editor_loop(
            &mut session,
            assignment_title,
            false,
            key_config,
            startup_notice.as_deref(),
            &theme.palette,
            terminal_session.operations_mut(),
        )
    }));
    let (editor_result, mut pending_unwind) = match initial {
        Ok(result) => (result, None),
        Err(payload) => (Ok(()), Some(payload)),
    };
    let mut cleanup_ui_error = None;
    let mut cleanup_ui_available = true;
    let terminal_shutdown = loop {
        match session.prepare_terminal_quit() {
            crate::session::TerminalQuit::Ready(result) => break result,
            crate::session::TerminalQuit::CleanupPending if cleanup_ui_available => {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    editor_loop(
                        &mut session,
                        assignment_title,
                        true,
                        key_config,
                        None,
                        &theme.palette,
                        terminal_session.operations_mut(),
                    )
                })) {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        cleanup_ui_error.get_or_insert(error);
                        cleanup_ui_available = false;
                    }
                    Err(payload) => {
                        if pending_unwind.is_none() {
                            pending_unwind = Some(payload);
                        }
                        cleanup_ui_available = false;
                    }
                }
            }
            crate::session::TerminalQuit::CleanupPending => {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
    let quit_result = session.quit();
    drop(terminal_session);
    if let Some(payload) = pending_unwind {
        std::panic::resume_unwind(payload);
    }
    editor_result?;
    if let Some(error) = cleanup_ui_error {
        return Err(error);
    }
    terminal_shutdown?;
    quit_result
}

pub(crate) fn run_editor(
    session: crate::session::ProductionSession,
    assignment_title: &str,
    update_state: crate::update::UpdateState,
) -> Result<(), Box<dyn Error>> {
    let startup = crate::config::load_config().map_or_else(
        || EditorStartupConfig {
            modifier: crate::config::ModifierPreference::Auto,
            theme: crate::tui::theme::ThemeConfig::default(),
            warning: None,
        },
        |loaded| EditorStartupConfig {
            modifier: loaded.modifier,
            theme: loaded.theme,
            warning: loaded.warning,
        },
    );
    let ghostty_key_bindings = crate::ghostty::startup_key_bindings();
    run_editor_with(
        session,
        assignment_title,
        CrosstermTerminalOperations,
        startup,
        ghostty_key_bindings,
        move |session, title, quit_pending, key_config, notice, palette, operations| {
            run_editor_loop(
                session,
                title,
                quit_pending,
                key_config,
                notice,
                palette,
                operations,
                update_state.clone(),
            )
        },
    )
}

#[cfg(test)]
pub(crate) fn run_editor_with_for_test<O, F>(
    session: crate::session::ProductionSession,
    assignment_title: &str,
    operations: O,
    mut editor_loop: F,
) -> Result<(), Box<dyn Error>>
where
    O: TerminalOperations,
    F: FnMut(&mut crate::session::ProductionSession, &str, bool) -> Result<(), Box<dyn Error>>,
{
    run_editor_with(
        session,
        assignment_title,
        operations,
        EditorStartupConfig {
            modifier: crate::config::ModifierPreference::Auto,
            theme: crate::tui::theme::ThemeConfig::default(),
            warning: None,
        },
        crate::ghostty::GhosttyKeyBindings::passed(),
        move |session, title, quit_pending, _key_config, _notice, _palette, _operations| {
            editor_loop(session, title, quit_pending)
        },
    )
}

fn retire_paste_warning(status: &mut StatusMessage) {
    if status.as_str() == crate::session::PASTE_BLOCKED_WARNING {
        status.clear();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PasteEventRoute {
    Continue,
    MatchingInternal,
    Reject(PasteInputChannel, PasteRejectionReason),
}

fn paste_event_route(
    event: &Event,
    outside_editor: bool,
    matching_internal: bool,
    primary_modifier: crate::config::PrimaryModifier,
) -> PasteEventRoute {
    match event {
        Event::Paste(_) if !outside_editor && matching_internal => {
            PasteEventRoute::MatchingInternal
        }
        Event::Paste(_) => PasteEventRoute::Reject(
            PasteInputChannel::TerminalBracketed,
            PasteRejectionReason::ExternalInput,
        ),
        Event::Key(key)
            if outside_editor
                && key.kind != KeyEventKind::Release
                && has_exact_primary_modifier(key.modifiers, primary_modifier)
                && matches!(key.code, KeyCode::Char('v' | 'V')) =>
        {
            PasteEventRoute::Reject(
                PasteInputChannel::InternalShortcut,
                PasteRejectionReason::OutsideEditor,
            )
        }
        _ => PasteEventRoute::Continue,
    }
}

#[cfg(test)]
fn paste_rejection_for_event(
    event: &Event,
    outside_editor: bool,
    primary_modifier: crate::config::PrimaryModifier,
) -> Option<(PasteInputChannel, PasteRejectionReason)> {
    match paste_event_route(event, outside_editor, false, primary_modifier) {
        PasteEventRoute::Reject(channel, reason) => Some((channel, reason)),
        PasteEventRoute::Continue | PasteEventRoute::MatchingInternal => None,
    }
}

fn persistent_error_message(
    recovery_reason: Option<&str>,
    external_notice: Option<&str>,
    recording: RecordingState,
    journal: JournalHealth,
) -> Option<String> {
    recovery_reason
        .or(external_notice)
        .map(ToOwned::to_owned)
        .or_else(|| {
            if recording == RecordingState::Inactive {
                Some("recording stopped".to_owned())
            } else if journal == JournalHealth::Degraded {
                Some("journal degraded".to_owned())
            } else {
                None
            }
        })
}

fn toast_for_status(status: &str) -> Option<ToastState> {
    let status = status.trim();
    let command_state = matches!(
        status,
        "command preparation started"
            | "dependency update preparation started"
            | "console command preparation started"
            | "console command cancellation requested"
            | "Command: resolving tools (Esc cancel)"
            | "Command: running (Esc cancel)"
            | "Command: saving evidence"
            | "Command: waiting for owned process cleanup"
            | "Console: resolving tools (Esc cancels and closes)"
            | "Console: running; Enter sends line, Esc cancels and closes"
            | "Console: running; input disabled, Esc cancels and closes"
            | "Console: saving evidence"
            | "Console: waiting for owned process cleanup"
            | "Command: nonzero exit; capture recorded"
            | "Command: launch failed; evidence recorded"
            | "Command: cancelled; capture recorded"
            | "Command: deadline; capture recorded"
            | "Command: terminated; capture recorded"
            | "Console: nonzero exit; evidence recorded"
            | "Console: stopped; evidence recorded"
            | "Format: failed/cancelled; returned changes discarded"
            | "console stdin line sent"
            | "console stdin queue full; line preserved"
    );
    if status.is_empty() || status == "ready" || command_state {
        return None;
    }
    if status.starts_with(crate::session::PASTE_BLOCKED_WARNING) {
        return Some(ToastState::new(
            ToastKind::Warning,
            "paste blocked",
            crate::session::PASTE_BLOCKED_WARNING,
        ));
    }
    let lower = status.to_ascii_lowercase();
    let (kind, title) = if lower.contains("failed")
        || lower.contains("error")
        || lower.contains("rejected")
        || lower.contains("unavailable")
        || lower.contains("stopped")
        || lower.contains("recovery required")
        || lower.starts_with("quit pending")
    {
        (ToastKind::Error, "action failed")
    } else if lower.contains("saved")
        || lower.contains("accepted")
        || lower.contains("finished")
        || lower.contains("opened")
    {
        (ToastKind::Success, "complete")
    } else if lower.contains("warning") || lower.contains("undo limit") {
        (ToastKind::Warning, "warning")
    } else {
        (ToastKind::Info, "notice")
    };
    Some(ToastState::new(kind, title, status))
}

const TOAST_DURATION_MILLIS: u64 = 10_000;

struct StatusMessage {
    text: String,
    occurrence: Rc<()>,
}

impl From<&str> for StatusMessage {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            occurrence: Rc::new(()),
        }
    }
}

impl From<String> for StatusMessage {
    fn from(text: String) -> Self {
        Self {
            text,
            occurrence: Rc::new(()),
        }
    }
}

impl StatusMessage {
    fn replace(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.occurrence = Rc::new(());
    }

    fn clear(&mut self) {
        self.replace("");
    }
}

impl Deref for StatusMessage {
    type Target = String;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

struct TimedStatusToast {
    occurrence: Rc<()>,
    toast: ToastState,
    started_at_ms: u64,
}

struct TimedHealthToast {
    message: String,
    persistent_error: bool,
    toast: ToastState,
    started_at_ms: u64,
}

#[derive(Default)]
struct ToastTimer {
    status: Option<TimedStatusToast>,
    health: Option<TimedHealthToast>,
}

impl ToastTimer {
    fn observe_status(&mut self, status: &StatusMessage, now_ms: u64) {
        let Some(toast) = toast_for_status(status) else {
            self.status = None;
            return;
        };
        let same_occurrence = self
            .status
            .as_ref()
            .is_some_and(|current| Rc::ptr_eq(&current.occurrence, &status.occurrence));
        if !same_occurrence {
            self.status = Some(TimedStatusToast {
                occurrence: Rc::clone(&status.occurrence),
                toast,
                started_at_ms: now_ms,
            });
        }
    }

    fn observe_health(&mut self, message: Option<&str>, persistent_error: bool, now_ms: u64) {
        let Some(message) = message else {
            self.health = None;
            return;
        };
        let same_occurrence = self.health.as_ref().is_some_and(|current| {
            current.message == message && current.persistent_error == persistent_error
        });
        if same_occurrence {
            return;
        }
        let toast = if persistent_error {
            ToastState::new(ToastKind::Error, "recovery required", message)
        } else {
            toast_for_status(message).expect("journal warning is a visible toast")
        };
        self.health = Some(TimedHealthToast {
            message: message.to_owned(),
            persistent_error,
            toast,
            started_at_ms: now_ms,
        });
    }

    fn expire(
        &mut self,
        status: &mut StatusMessage,
        dismissed_health: &mut Option<String>,
        now_ms: u64,
    ) {
        if self.health.as_ref().is_some_and(|toast| {
            now_ms.saturating_sub(toast.started_at_ms) >= TOAST_DURATION_MILLIS
        }) {
            *dismissed_health = self.health.take().map(|toast| toast.message);
        }
        if self.status.as_ref().is_some_and(|toast| {
            now_ms.saturating_sub(toast.started_at_ms) >= TOAST_DURATION_MILLIS
        }) {
            let expired = self.status.take().expect("expired status toast exists");
            if Rc::ptr_eq(&expired.occurrence, &status.occurrence) {
                status.clear();
            }
        }
    }

    fn visible(&self) -> Option<ToastState> {
        self.health
            .as_ref()
            .map(|toast| toast.toast.clone())
            .or_else(|| self.status.as_ref().map(|toast| toast.toast.clone()))
    }

    fn reset(&mut self) {
        self.status = None;
        self.health = None;
    }
}

fn toast_for_frame(
    status: &mut StatusMessage,
    health_message: Option<String>,
    persistent_error: bool,
    dismissed_health: &mut Option<String>,
    timer: &mut ToastTimer,
    now_ms: u64,
) -> Option<ToastState> {
    let health_message =
        health_message.filter(|message| dismissed_health.as_deref() != Some(message.as_str()));
    timer.observe_status(status, now_ms);
    timer.observe_health(health_message.as_deref(), persistent_error, now_ms);
    timer.expire(status, dismissed_health, now_ms);
    timer.visible()
}

fn dismiss_toast_on_key(
    event: &Event,
    status: &mut StatusMessage,
    health_message: Option<String>,
    dismissed_health: &mut Option<String>,
    timer: &mut ToastTimer,
) {
    if !matches!(event, Event::Key(key) if key.kind == KeyEventKind::Press) {
        return;
    }
    if let Some(message) = health_message {
        *dismissed_health = Some(message);
    }
    if toast_for_status(status).is_some() {
        status.clear();
    }
    timer.reset();
}

fn diagnostic_output(
    message: &str,
    diagnostics: &[crate::tui::OutputRow],
    maximum_rows: usize,
) -> Vec<crate::tui::OutputRow> {
    if maximum_rows == 0 {
        Vec::new()
    } else if !diagnostics.is_empty() {
        diagnostics.iter().take(maximum_rows).cloned().collect()
    } else if message.is_empty() {
        Vec::new()
    } else {
        vec![crate::tui::OutputRow::plain(message)]
    }
}

fn command_status_for_output(
    status: &str,
    primary_modifier: crate::config::PrimaryModifier,
) -> String {
    primary_modifier_text(status, primary_modifier)
}

#[cfg(test)]
fn diagnostic_output_text(rows: &[crate::tui::OutputRow]) -> String {
    rows.iter()
        .map(crate::tui::OutputRow::text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::{
        ConsoleScroll, EXPLICIT_SAVE_SUCCESS, FindPanel, FindPanelAction, FindPanelField,
        LiveSnapshot, PasteEventRoute, PathOperation, PathPrompt, PathPromptAction, StatusMessage,
        TestCasePicker, TestCasePickerKeyAction, ToastTimer, WorkView, activate_command_menu_entry,
        activate_editor_context_menu_entry, activate_files_context_menu_entry,
        apply_path_prompt_action, apply_workspace_outcome, begin_path_prompt,
        command_completion_status, command_status_for_output, command_tick_status,
        completion_input_now_ms, console_body, diagnostic_navigation_status, diagnostic_output,
        diagnostic_output_scroll, diagnostic_output_text, dismiss_toast_on_key,
        execute_editor_command, handle_console_key, journal_warning_message, keybinds_scroll_delta,
        maximum_keybinds_scroll, non_test_command_owned_tick, open_files_context_menu,
        paste_event_route, paste_rejection_for_event, persistent_error_message,
        reduce_keybinds_scroll, retire_paste_warning, route_find_panel_event,
        should_follow_cursor_for_frame, start_test_case_sequence, submitted_path, test_case_output,
        test_case_picker_key_action, test_case_result_summary, test_case_view_switch_available,
        toast_for_frame, toast_for_status,
    };
    use crate::config::PrimaryModifier;
    use crate::console::{ConsoleLine, TestCaseDirectory};
    use crate::diagnostics::DiagnosticNavigation;
    use crate::editor::{EditorBuffer, Movement, NoopEditorEffects, Viewport};
    use crate::session::{PASTE_BLOCKED_WARNING, ProductionSession, SessionHealth};
    use crate::tui::{
        BufferTabViewEntry, CompletionTrigger, CompletionTriggerInput, FilesContextMenuAction,
        FilesContextMenuState, JournalHealth, MainView, MainViewState, MouseState, OutputRow,
        RecordingState, ShellInput, ShellModal, ShellState, TerminalOperations, ToastKind,
        ToastState, WorkspaceFocus, WorkspaceOutcome, mouse_input_for_event,
        osc52_clipboard_sequence,
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    };
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, layout::Rect};
    use rustrace_model::{
        DocumentId, Event as ModelEvent, OutputStream, PasteInputChannel, PasteRejectionReason,
        WorkspacePath,
    };
    use std::{
        fs, io,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    struct ClipboardFixture(PathBuf);

    impl ClipboardFixture {
        fn new(source: &str) -> (Self, ProductionSession) {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "rustrace-osc52-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&root).unwrap();
            fs::write(root.join("main.rs"), source).unwrap();
            let root = fs::canonicalize(root).unwrap();
            let manifest = br#"format_version = 1
course_id = "course"
assignment_id = "osc52"
assignment_version = "v1"
title = "OSC 52"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;
            let session = ProductionSession::start(&root, manifest).unwrap();
            (Self(root), session)
        }

        fn recorded_events(&self) -> Vec<ModelEvent> {
            let metadata = ProductionSession::read_metadata(&self.0).unwrap();
            let path = self
                .0
                .join(".rustrace")
                .join(format!("{}.sqlite", metadata.session_id));
            let mut journal = rustrace_journal::Journal::open_no_follow(&path).unwrap();
            journal
                .read_events(
                    &metadata.session_id,
                    1,
                    rustrace_journal::MAX_EVENTS_PER_READ,
                )
                .unwrap()
                .into_iter()
                .map(|envelope| envelope.event)
                .collect()
        }
    }

    impl Drop for ClipboardFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct ClipboardProbe {
        writes: Vec<Vec<u8>>,
    }

    impl TerminalOperations for ClipboardProbe {
        fn enable_raw_mode(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn enter_alternate_screen(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn enable_bracketed_paste(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn enable_mouse_capture(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn write_system_clipboard(&mut self, text: &str) -> io::Result<()> {
            self.writes.push(osc52_clipboard_sequence(text));
            Ok(())
        }
        fn disable_mouse_capture(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn disable_bracketed_paste(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn leave_alternate_screen(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn disable_raw_mode(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    enum ClipboardRoute {
        Shortcut,
        ContextMenu,
    }

    #[test]
    fn successful_copy_and_cut_routes_write_exact_osc52_payloads_without_queries() {
        for (text, encoded) in [
            ("copy me", "Y29weSBtZQ=="),
            ("é🦀", "w6nwn6aA"),
            ("first\nsecond\n", "Zmlyc3QKc2Vjb25kCg=="),
        ] {
            for command in [
                crate::tui::EditorCommand::Copy,
                crate::tui::EditorCommand::Cut,
            ] {
                for route in [ClipboardRoute::Shortcut, ClipboardRoute::ContextMenu] {
                    let (_fixture, mut session) = ClipboardFixture::new(text);
                    session
                        .execute(crate::tui::EditorCommand::SelectAll)
                        .unwrap();
                    let mut probe = ClipboardProbe::default();
                    match route {
                        ClipboardRoute::Shortcut => {
                            execute_editor_command(&mut session, command.clone(), &mut probe)
                                .unwrap();
                        }
                        ClipboardRoute::ContextMenu => {
                            let index = usize::from(command == crate::tui::EditorCommand::Copy);
                            let mut menu = Some(crate::tui::EditorContextMenuState::new(
                                ratatui::layout::Position::new(5, 5),
                                true,
                                false,
                            ));
                            let mut status: StatusMessage = "".into();
                            activate_editor_context_menu_entry(
                                &mut session,
                                &mut menu,
                                index,
                                &mut status,
                                &mut probe,
                            )
                            .unwrap();
                        }
                    }
                    assert_eq!(
                        probe.writes,
                        [format!("\x1b]52;c;{encoded}\x07").into_bytes()],
                        "{command:?}",
                    );
                    assert!(
                        probe
                            .writes
                            .iter()
                            .all(|write| !write.windows(2).any(|w| w == b";?"))
                    );
                }
            }
        }
    }

    #[test]
    fn context_menu_select_all_records_only_the_resulting_selection() {
        let (fixture, mut session) = ClipboardFixture::new("alpha\nbeta");
        let mut menu = Some(crate::tui::EditorContextMenuState::new(
            ratatui::layout::Position::new(5, 5),
            false,
            false,
        ));
        let mut status: StatusMessage = "".into();
        let mut probe = ClipboardProbe::default();

        let (command, outcome) =
            activate_editor_context_menu_entry(&mut session, &mut menu, 3, &mut status, &mut probe)
                .expect("Select all is always enabled");

        assert_eq!(command, crate::tui::EditorCommand::SelectAll);
        assert_eq!(outcome, crate::tui::EditorOutcome::SelectionChanged);
        assert!(menu.is_none());
        assert!(probe.writes.is_empty());
        session.quit().unwrap();
        let editor_events = fixture
            .recorded_events()
            .into_iter()
            .filter(|event| {
                matches!(
                    event,
                    ModelEvent::SelectionChanged(_) | ModelEvent::FileEdited(_)
                )
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            editor_events.as_slice(),
            [ModelEvent::SelectionChanged(_)]
        ));
    }

    #[test]
    fn paste_event_routing_accepts_only_a_matching_editor_bracketed_paste() {
        assert_eq!(
            paste_event_route(
                &Event::Paste("same".into()),
                false,
                true,
                PrimaryModifier::Control
            ),
            PasteEventRoute::MatchingInternal,
        );
        for (outside_editor, matches) in [(false, false), (true, false), (true, true)] {
            assert_eq!(
                paste_event_route(
                    &Event::Paste("same".into()),
                    outside_editor,
                    matches,
                    PrimaryModifier::Control,
                ),
                PasteEventRoute::Reject(
                    PasteInputChannel::TerminalBracketed,
                    PasteRejectionReason::ExternalInput,
                ),
            );
        }
    }

    struct FilesMenuFixture(std::path::PathBuf);

    impl FilesMenuFixture {
        fn new(paths: &[&str]) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "rustrace-files-menu-unit-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            for relative in paths {
                let path = root.join(relative);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, format!("// {relative}\n")).unwrap();
            }
            Self(std::fs::canonicalize(root).unwrap())
        }

        fn start(&self) -> crate::session::ProductionSession {
            crate::session::ProductionSession::start(&self.0, files_menu_manifest()).unwrap()
        }

        fn lifecycle_events(&self) -> Vec<ModelEvent> {
            let metadata = crate::session::ProductionSession::read_metadata(&self.0).unwrap();
            let path = self
                .0
                .join(".rustrace")
                .join(format!("{}.sqlite", metadata.session_id));
            let mut journal = rustrace_journal::Journal::open_no_follow(&path).unwrap();
            journal
                .read_events(
                    &metadata.session_id,
                    1,
                    rustrace_journal::MAX_EVENTS_PER_READ,
                )
                .unwrap()
                .into_iter()
                .map(|envelope| envelope.event)
                .filter(|event| {
                    matches!(
                        event,
                        ModelEvent::FileCreated(_)
                            | ModelEvent::FileRenamed(_)
                            | ModelEvent::FileDeleted(_)
                            | ModelEvent::FileFocused(_)
                    )
                })
                .collect()
        }
    }

    impl Drop for FilesMenuFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn files_menu_manifest() -> &'static [u8] {
        br#"format_version = 1
course_id = "course"
assignment_id = "files-menu"
assignment_version = "v1"
title = "Files menu"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["src/**/*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#
    }

    fn files_menu(
        session: &mut crate::session::ProductionSession,
        index: usize,
        status: &mut StatusMessage,
        focus: &mut WorkspaceFocus,
        follow_cursor: &mut bool,
        prompt: &mut Option<PathPrompt>,
    ) -> Option<(FilesContextMenuAction, WorkspaceOutcome)> {
        let mut menu = Some(FilesContextMenuState::new(
            ratatui::layout::Position::new(4, 3),
            true,
        ));
        session.set_command_modal(true);
        let result = activate_files_context_menu_entry(
            session,
            &mut menu,
            index,
            status,
            focus,
            follow_cursor,
            prompt,
        );
        assert!(menu.is_none(), "enabled menu action did not close the menu");
        result
    }

    #[test]
    fn pane_resize_refollows_the_editor_while_the_console_has_focus() {
        assert!(should_follow_cursor_for_frame(
            false,
            true,
            WorkView::Console,
            WorkspaceFocus::Console,
        ));
        assert!(!should_follow_cursor_for_frame(
            false,
            false,
            WorkView::Console,
            WorkspaceFocus::Console,
        ));
    }

    #[test]
    fn successful_file_and_diagnostic_activation_are_silent_without_a_toast() {
        let mut status: StatusMessage = "previous notice".into();
        apply_workspace_outcome(WorkspaceOutcome::FileActivated, &mut status);
        assert_eq!(status.as_str(), "");
        assert_eq!(toast_for_status(&status), None);

        let status = diagnostic_navigation_status(Ok(DiagnosticNavigation::Navigated {
            path: WorkspacePath::new("src/other.rs").unwrap(),
            start_byte: 3,
            end_byte: 7,
        }));
        assert_eq!(status, "");
        assert_eq!(toast_for_status(&status), None);

        let failure = diagnostic_navigation_status(Ok(DiagnosticNavigation::MissingTarget));
        assert!(!failure.is_empty());
        assert!(toast_for_status(&failure).is_some());
    }

    #[test]
    fn successful_alt_navigation_reveals_the_selected_output_row() {
        let navigated = Ok(DiagnosticNavigation::Navigated {
            path: WorkspacePath::new("src/other.rs").unwrap(),
            start_byte: 3,
            end_byte: 7,
        });
        assert_eq!(diagnostic_output_scroll(9, &navigated), 0);

        let unavailable = Ok(DiagnosticNavigation::MissingTarget);
        assert_eq!(diagnostic_output_scroll(9, &unavailable), 9);
    }

    #[test]
    fn files_menu_open_matches_the_existing_silent_activation_route() {
        let menu_fixture = FilesMenuFixture::new(&["src/a.rs", "src/b.rs"]);
        let direct_fixture = FilesMenuFixture::new(&["src/a.rs", "src/b.rs"]);
        let mut menu_session = menu_fixture.start();
        let mut direct_session = direct_fixture.start();
        for session in [&mut menu_session, &mut direct_session] {
            session
                .workspace_mut()
                .select_path(&WorkspacePath::new("src/b.rs").unwrap())
                .unwrap();
        }

        let mut menu_status: StatusMessage = "previous notice".into();
        let mut menu_focus = WorkspaceFocus::Console;
        let mut menu_follow = false;
        let mut menu_prompt = None;
        assert_eq!(
            files_menu(
                &mut menu_session,
                0,
                &mut menu_status,
                &mut menu_focus,
                &mut menu_follow,
                &mut menu_prompt,
            ),
            Some((
                FilesContextMenuAction::Open,
                WorkspaceOutcome::FileActivated,
            ))
        );

        let mut direct_status: StatusMessage = "previous notice".into();
        let direct_outcome = direct_session.workspace_mut().activate_selected().unwrap();
        apply_workspace_outcome(direct_outcome, &mut direct_status);
        assert_eq!(direct_outcome, WorkspaceOutcome::FileActivated);
        assert_eq!(
            menu_session.workspace().active_path(),
            direct_session.workspace().active_path()
        );
        assert_eq!(menu_status.as_str(), direct_status.as_str());
        assert_eq!(menu_status.as_str(), "", "open must not raise a toast");
        assert_eq!(menu_focus, WorkspaceFocus::Editor);
        assert!(menu_follow);
        assert!(menu_prompt.is_none());
        assert_eq!(
            menu_fixture.lifecycle_events(),
            direct_fixture.lifecycle_events()
        );
    }

    #[test]
    fn opening_files_menu_on_a_row_selects_it_while_header_keeps_reduced_actions() {
        let row_fixture = FilesMenuFixture::new(&["src/a.rs", "src/b.rs"]);
        let mut row_session = row_fixture.start();
        assert_eq!(row_session.workspace().selected_path().as_str(), "src/a.rs");
        let mut row_menu = None;
        let mut row_status: StatusMessage = "previous notice".into();
        assert_eq!(
            open_files_context_menu(
                &mut row_session,
                ratatui::layout::Position::new(4, 2),
                Some(1),
                &mut row_menu,
                &mut row_status,
            ),
            WorkspaceOutcome::TreeSelectionChanged
        );
        assert_eq!(row_session.workspace().selected_path().as_str(), "src/b.rs");
        let row_menu = row_menu.unwrap();
        assert!(
            (0..4).all(|index| row_menu.action(index).is_some()),
            "a file-row menu must enable all entries"
        );

        let header_fixture = FilesMenuFixture::new(&["src/a.rs", "src/b.rs"]);
        let mut header_session = header_fixture.start();
        let selected = header_session.workspace().selected_path().clone();
        let mut header_menu = None;
        let mut header_status: StatusMessage = "previous notice".into();
        assert_eq!(
            open_files_context_menu(
                &mut header_session,
                ratatui::layout::Position::new(2, 0),
                None,
                &mut header_menu,
                &mut header_status,
            ),
            WorkspaceOutcome::NoChange
        );
        assert_eq!(header_session.workspace().selected_path(), &selected);
        let header_menu = header_menu.unwrap();
        assert_eq!(header_menu.action(0), None);
        assert_eq!(header_menu.action(1), None);
        assert_eq!(header_menu.action(2), None);
        assert_eq!(header_menu.action(3), Some(FilesContextMenuAction::NewFile));
        assert_eq!(row_status.as_str(), "");
        assert_eq!(header_status.as_str(), "");
        assert_eq!(
            row_fixture.lifecycle_events(),
            header_fixture.lifecycle_events()
        );
    }

    #[test]
    fn files_menu_rename_matches_the_prefilled_panel_and_recorded_route() {
        let menu_fixture = FilesMenuFixture::new(&["src/a.rs", "src/b.rs"]);
        let direct_fixture = FilesMenuFixture::new(&["src/a.rs", "src/b.rs"]);
        let mut menu_session = menu_fixture.start();
        let mut direct_session = direct_fixture.start();
        for session in [&mut menu_session, &mut direct_session] {
            session
                .workspace_mut()
                .select_path(&WorkspacePath::new("src/b.rs").unwrap())
                .unwrap();
        }

        let mut menu_status: StatusMessage = "previous notice".into();
        let mut menu_focus = WorkspaceFocus::Editor;
        let mut menu_follow = false;
        let mut menu_prompt = None;
        assert_eq!(
            files_menu(
                &mut menu_session,
                1,
                &mut menu_status,
                &mut menu_focus,
                &mut menu_follow,
                &mut menu_prompt,
            ),
            Some((FilesContextMenuAction::Rename, WorkspaceOutcome::NoChange,))
        );
        let mut direct_prompt = Some(begin_path_prompt(&direct_session, PathOperation::Rename));
        assert_eq!(
            menu_prompt.as_ref().unwrap().operation,
            PathOperation::Rename
        );
        assert_eq!(
            menu_prompt.as_ref().unwrap().input,
            direct_prompt.as_ref().unwrap().input
        );
        assert_eq!(menu_prompt.as_ref().unwrap().input, "src/b.rs");
        assert_eq!(
            menu_fixture.lifecycle_events(),
            direct_fixture.lifecycle_events()
        );

        menu_prompt.as_mut().unwrap().input = "src/c.rs".into();
        direct_prompt.as_mut().unwrap().input = "src/c.rs".into();
        let mut direct_status: StatusMessage = "previous notice".into();
        let mut direct_focus = WorkspaceFocus::Editor;
        let mut direct_follow = false;
        apply_path_prompt_action(
            &mut menu_session,
            &mut menu_prompt,
            PathPromptAction::Submit,
            &mut menu_status,
            &mut menu_focus,
            &mut menu_follow,
        );
        apply_path_prompt_action(
            &mut direct_session,
            &mut direct_prompt,
            PathPromptAction::Submit,
            &mut direct_status,
            &mut direct_focus,
            &mut direct_follow,
        );
        assert_eq!(menu_status.as_str(), direct_status.as_str());
        assert_eq!(
            menu_fixture.lifecycle_events(),
            direct_fixture.lifecycle_events()
        );
        assert!(!menu_fixture.0.join("src/b.rs").exists());
        assert!(menu_fixture.0.join("src/c.rs").exists());
    }

    #[test]
    fn files_menu_new_file_matches_the_fixed_src_prefix_panel_and_recorded_route() {
        let menu_fixture = FilesMenuFixture::new(&["src/a.rs", "src/b.rs"]);
        let direct_fixture = FilesMenuFixture::new(&["src/a.rs", "src/b.rs"]);
        let mut menu_session = menu_fixture.start();
        let mut direct_session = direct_fixture.start();
        let mut menu_status: StatusMessage = "previous notice".into();
        let mut menu_focus = WorkspaceFocus::Editor;
        let mut menu_follow = false;
        let mut menu_prompt = None;
        assert_eq!(
            files_menu(
                &mut menu_session,
                3,
                &mut menu_status,
                &mut menu_focus,
                &mut menu_follow,
                &mut menu_prompt,
            ),
            Some((FilesContextMenuAction::NewFile, WorkspaceOutcome::NoChange,))
        );
        let mut direct_prompt = Some(begin_path_prompt(&direct_session, PathOperation::Create));
        assert_eq!(
            menu_prompt.as_ref().unwrap().operation,
            PathOperation::Create
        );
        assert_eq!(menu_prompt.as_ref().unwrap().input, "");
        assert_eq!(submitted_path(menu_prompt.as_ref().unwrap()), "src/");
        assert_eq!(
            menu_fixture.lifecycle_events(),
            direct_fixture.lifecycle_events()
        );

        menu_prompt.as_mut().unwrap().input = "new.rs".into();
        direct_prompt.as_mut().unwrap().input = "new.rs".into();
        let mut direct_status: StatusMessage = "previous notice".into();
        let mut direct_focus = WorkspaceFocus::Editor;
        let mut direct_follow = false;
        apply_path_prompt_action(
            &mut menu_session,
            &mut menu_prompt,
            PathPromptAction::Submit,
            &mut menu_status,
            &mut menu_focus,
            &mut menu_follow,
        );
        apply_path_prompt_action(
            &mut direct_session,
            &mut direct_prompt,
            PathPromptAction::Submit,
            &mut direct_status,
            &mut direct_focus,
            &mut direct_follow,
        );
        assert_eq!(menu_status.as_str(), direct_status.as_str());
        assert_eq!(
            menu_fixture.lifecycle_events(),
            direct_fixture.lifecycle_events()
        );
        assert!(menu_fixture.0.join("src/new.rs").exists());
    }

    #[test]
    fn files_menu_delete_matches_confirmation_events_and_last_file_refusal() {
        let menu_fixture = FilesMenuFixture::new(&["src/a.rs", "src/b.rs"]);
        let direct_fixture = FilesMenuFixture::new(&["src/a.rs", "src/b.rs"]);
        let mut menu_session = menu_fixture.start();
        let mut direct_session = direct_fixture.start();
        for session in [&mut menu_session, &mut direct_session] {
            session
                .workspace_mut()
                .select_path(&WorkspacePath::new("src/b.rs").unwrap())
                .unwrap();
            session.workspace_mut().activate_selected().unwrap();
            session
                .execute(crate::tui::EditorCommand::Insert('!'))
                .unwrap();
        }

        let mut menu_status: StatusMessage = "previous notice".into();
        let mut menu_focus = WorkspaceFocus::Editor;
        let mut menu_follow = false;
        let mut menu_prompt = None;
        assert_eq!(
            files_menu(
                &mut menu_session,
                2,
                &mut menu_status,
                &mut menu_focus,
                &mut menu_follow,
                &mut menu_prompt,
            ),
            Some((
                FilesContextMenuAction::Delete,
                WorkspaceOutcome::ConfirmationRequired,
            ))
        );
        let mut direct_status: StatusMessage = "previous notice".into();
        let direct_outcome = direct_session.delete_selected().unwrap();
        apply_workspace_outcome(direct_outcome, &mut direct_status);
        assert_eq!(menu_status.as_str(), direct_status.as_str());
        assert!(menu_session.workspace().delete_confirmation_pending());

        let menu_outcome = menu_session.confirm_delete().unwrap();
        let direct_outcome = direct_session.confirm_delete().unwrap();
        apply_workspace_outcome(menu_outcome, &mut menu_status);
        apply_workspace_outcome(direct_outcome, &mut direct_status);
        assert_eq!(menu_outcome, WorkspaceOutcome::FileDeleted);
        assert_eq!(menu_status.as_str(), direct_status.as_str());
        assert_eq!(
            menu_fixture.lifecycle_events(),
            direct_fixture.lifecycle_events()
        );
        assert_eq!(
            menu_fixture
                .lifecycle_events()
                .iter()
                .filter(|event| matches!(event, ModelEvent::FileDeleted(_)))
                .count(),
            1
        );
        assert!(!menu_fixture.0.join("src/b.rs").exists());

        let refusal_fixture = FilesMenuFixture::new(&["src/only.rs"]);
        let mut refusal_session = refusal_fixture.start();
        let before = refusal_fixture.lifecycle_events();
        let mut refusal_status: StatusMessage = "previous notice".into();
        let mut refusal_focus = WorkspaceFocus::Editor;
        let mut refusal_follow = false;
        let mut refusal_prompt = None;
        assert_eq!(
            files_menu(
                &mut refusal_session,
                2,
                &mut refusal_status,
                &mut refusal_focus,
                &mut refusal_follow,
                &mut refusal_prompt,
            ),
            Some((FilesContextMenuAction::Delete, WorkspaceOutcome::NoChange))
        );
        assert!(
            refusal_status.contains("last editable workspace file cannot be deleted"),
            "{}",
            refusal_status.as_str()
        );
        assert_eq!(refusal_fixture.lifecycle_events(), before);
        assert!(refusal_fixture.0.join("src/only.rs").exists());
    }

    #[test]
    fn explicit_save_has_one_toast_and_successful_command_completions_are_silent() {
        let save_toast = toast_for_status(EXPLICIT_SAVE_SUCCESS);
        assert_eq!(
            save_toast,
            Some(ToastState::new(
                ToastKind::Success,
                "complete",
                "File saved",
            ))
        );

        let completion = "Command: exit 0; capture recorded";
        let mut save_triggered_check = true;
        let save_completion = command_completion_status(completion, &mut save_triggered_check);
        assert_eq!(save_completion, "");
        assert!(!save_triggered_check);
        assert_eq!(toast_for_status(&save_completion), None);
        assert_eq!(
            [save_toast, toast_for_status(&save_completion)]
                .into_iter()
                .flatten()
                .count(),
            1
        );

        let mut menu_command = false;
        let menu_completion = command_completion_status(completion, &mut menu_command);
        assert_eq!(menu_completion, "");
        assert!(toast_for_status(&menu_completion).is_none());

        for successful in [
            "Console: exit 0; evidence recorded",
            "Format: changes recorded and saved",
            "Format: no changes",
            "Doc: generated /workspace/target/doc",
        ] {
            let mut menu_command = false;
            let status = command_completion_status(successful, &mut menu_command);
            assert_eq!(
                status, "",
                "successful completion was not silent: {successful}"
            );
            assert_eq!(toast_for_status(&status), None);
        }
    }

    #[test]
    fn command_states_are_silent_and_retained_failures_keep_their_toasts() {
        for state in [
            "command preparation started",
            "dependency update preparation started",
            "console command preparation started",
            "console command cancellation requested",
            "Command: resolving tools (Esc cancel)",
            "Command: running (Esc cancel)",
            "Command: saving evidence",
            "Command: waiting for owned process cleanup",
            "Console: resolving tools (Esc cancels and closes)",
            "Console: running; Enter sends line, Esc cancels and closes",
            "Console: running; input disabled, Esc cancels and closes",
            "Console: saving evidence",
            "Console: waiting for owned process cleanup",
            "console stdin line sent",
            "console stdin queue full; line preserved",
        ] {
            assert_eq!(
                toast_for_status(state),
                None,
                "command state raised a notice: {state}"
            );
        }

        for outcome in [
            "Command: nonzero exit; capture recorded",
            "Command: launch failed; evidence recorded",
            "Command: cancelled; capture recorded",
            "Command: deadline; capture recorded",
            "Command: terminated; capture recorded",
            "Console: nonzero exit; evidence recorded",
            "Console: stopped; evidence recorded",
            "Format: failed/cancelled; returned changes discarded",
        ] {
            let mut menu_command = false;
            let status = command_completion_status(outcome, &mut menu_command);
            assert_eq!(status, outcome);
            assert_eq!(
                toast_for_status(&status),
                None,
                "command outcome raised a notice: {outcome}"
            );
        }

        for retained in [
            "Command: owned process cleanup cannot be confirmed",
            "Command: recovery required",
            "Console: owned process cleanup cannot be confirmed",
            "Console: recovery required",
            "Quit pending: waiting for owned process cleanup; input is disabled",
            "Format: unsafe or invalid result rejected; inspect evidence",
            "save failed; buffer remains dirty: writer failed",
            "saved exact buffer bytes; Check failed to start: missing cargo",
            PASTE_BLOCKED_WARNING,
            "80% budget warning | undo limit reached",
            "recording stopped",
            "journal degraded",
        ] {
            assert!(
                toast_for_status(retained).is_some(),
                "missing retained notice: {retained}"
            );
        }
    }

    #[test]
    fn discarded_format_outcome_is_silent_but_safety_rejection_stays_a_notice() {
        let discarded = "Format: failed/cancelled; returned changes discarded";
        let mut menu_command = false;
        let status = command_completion_status(discarded, &mut menu_command);
        assert_eq!(status, discarded);
        assert_eq!(toast_for_status(&status), None);

        let rejected = "Format: unsafe or invalid result rejected; inspect evidence";
        let mut menu_command = false;
        let status = command_completion_status(rejected, &mut menu_command);
        assert_eq!(status, rejected);
        assert!(toast_for_status(&status).is_some());
    }

    fn rendered_production_idle_output(health: &SessionHealth, navigate: bool) -> String {
        let output = diagnostic_output("", &[], 4);
        let mut state = MainViewState::new(
            "Idle journal regression",
            vec![BufferTabViewEntry::new("main.rs", true, false)],
            vec![],
            RecordingState::Active,
            JournalHealth::Healthy,
            "saved",
        )
        .with_output_rows(output);
        if let Some(message) = journal_warning_message(Some(health)) {
            state = state.with_toast(toast_for_status(&message).unwrap());
        }
        let mut editor = EditorBuffer::new(
            DocumentId::new("idle-journal-regression").unwrap(),
            "fn main() {}\n",
            NoopEditorEffects,
        );
        if navigate {
            editor.move_cursor(Movement::Right, false);
        }
        let viewport = Viewport::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(MainView::new(&state, &editor, &viewport, &[]), frame.area());
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(80)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered_keybinds_position(scroll: usize) -> (String, Rect, crate::tui::shell::HitMap) {
        let area = Rect::new(0, 0, 80, 24);
        let state = MainViewState::new(
            "Keybind scroll regression",
            vec![BufferTabViewEntry::new("main.rs", true, false)],
            vec![],
            RecordingState::Active,
            JournalHealth::Healthy,
            "saved",
        )
        .with_keybinds_overlay(scroll);
        let editor = EditorBuffer::new(
            DocumentId::new("keybind-scroll-regression").unwrap(),
            "fn main() {}\n",
            NoopEditorEffects,
        );
        let viewport = Viewport::default();
        let mut buffer = Buffer::empty(area);
        let hits =
            MainView::new(&state, &editor, &viewport, &[]).render_with_hit_map(area, &mut buffer);
        let first_row_y = hits.overlay.y + 3;
        let first_row = (hits.overlay.x + 1..hits.keybinds_scrollbar_track.x)
            .filter_map(|x| buffer.cell((x, first_row_y)))
            .map(|cell| cell.symbol())
            .collect::<String>()
            .trim_end()
            .to_owned();
        (first_row, hits.keybinds_scrollbar_thumb, hits)
    }

    fn rendered_active_console_status(primary_modifier: PrimaryModifier) -> String {
        let status = command_status_for_output(
            "Console: running; Enter sends line, Esc cancels and closes",
            primary_modifier,
        );
        let state = MainViewState::new(
            "Command status modifier regression",
            vec![BufferTabViewEntry::new("main.rs", true, false)],
            vec![],
            RecordingState::Active,
            JournalHealth::Healthy,
            "saved",
        )
        .with_primary_modifier(primary_modifier)
        .with_output_rows(diagnostic_output(&status, &[], 4));
        let editor = EditorBuffer::new(
            DocumentId::new("command-status-modifier-regression").unwrap(),
            "fn main() {}\n",
            NoopEditorEffects,
        );
        let viewport = Viewport::default();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(MainView::new(&state, &editor, &viewport, &[]), frame.area());
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(120)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn active_console_status_uses_escape_in_both_modifier_modes() {
        for primary in [PrimaryModifier::Control, PrimaryModifier::Command] {
            let output = rendered_active_console_status(primary);
            assert!(output.contains("Esc cancels and closes"), "{output}");
            for chord in ["Ctrl-D", "⌘D", "Ctrl-C", "⌘C", "EOF"] {
                assert!(!output.contains(chord), "{output}");
            }
        }
    }

    #[test]
    fn captured_program_output_that_names_ctrl_c_is_never_rewritten() {
        let state = MainViewState::new(
            "Captured output modifier regression",
            vec![BufferTabViewEntry::new("main.rs", true, false)],
            vec![],
            RecordingState::Active,
            JournalHealth::Healthy,
            "saved",
        )
        .with_primary_modifier(PrimaryModifier::Command)
        .with_console_body_view(
            "Embedded Cargo console",
            b"student program says Ctrl-C literally\n".to_vec(),
            b"> ".to_vec(),
            None,
            true,
        );
        let editor = EditorBuffer::new(
            DocumentId::new("captured-output-modifier-regression").unwrap(),
            "fn main() {}\n",
            NoopEditorEffects,
        );
        let viewport = Viewport::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(MainView::new(&state, &editor, &viewport, &[]), frame.area());
            })
            .unwrap();
        let output = terminal
            .backend()
            .buffer()
            .content()
            .chunks(80)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            output.contains("student program says Ctrl-C literally"),
            "{output}"
        );
        assert!(
            !output.contains("student program says ⌘C literally"),
            "{output}"
        );
    }

    fn overlay_wheel_delta(kind: MouseEventKind, hits: &crate::tui::shell::HitMap) -> isize {
        let event = MouseEvent {
            kind,
            column: hits.overlay.x + 1,
            row: hits.overlay.y + 3,
            modifiers: KeyModifiers::NONE,
        };
        match mouse_input_for_event(
            &event,
            hits,
            &ShellState {
                modal: ShellModal::Keybinds,
            },
            &MouseState::default(),
        ) {
            Some(ShellInput::ScrollOverlay(delta)) => delta,
            input => panic!("expected overlay wheel input, got {input:?}"),
        }
    }

    #[test]
    fn keybinds_keyboard_overscroll_reverses_on_the_next_key() {
        let area = Rect::new(0, 0, 80, 24);
        let down = Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let up = Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        let mut scroll = 0;

        for _ in 0..100 {
            reduce_keybinds_scroll(&mut scroll, keybinds_scroll_delta(&down).unwrap(), area);
        }

        // The clamp tracks the inventory size and the overlay geometry.
        let maximum = maximum_keybinds_scroll(area, crate::tui::KEYBIND_ROWS.len());
        assert!(maximum > 10, "the inventory must overflow an 80x24 overlay");
        assert_eq!(scroll, maximum);
        let (last_row, last_thumb, _) = rendered_keybinds_position(scroll);
        assert!(reduce_keybinds_scroll(
            &mut scroll,
            keybinds_scroll_delta(&up).unwrap(),
            area,
        ));
        assert_eq!(scroll, maximum - 1);
        let (reversed_row, reversed_thumb, _) = rendered_keybinds_position(scroll);
        assert_ne!(reversed_row, last_row);
        assert!(reversed_thumb.y < last_thumb.y);

        assert_eq!(
            keybinds_scroll_delta(&Event::Key(KeyEvent::new(
                KeyCode::PageDown,
                KeyModifiers::NONE,
            ))),
            Some(10)
        );
        assert_eq!(
            keybinds_scroll_delta(&Event::Key(KeyEvent::new(
                KeyCode::PageUp,
                KeyModifiers::NONE,
            ))),
            Some(-10)
        );

        let compact_area = Rect::new(0, 0, 80, 18);
        for _ in 0..100 {
            reduce_keybinds_scroll(&mut scroll, 1, compact_area);
        }
        let compact_maximum = maximum_keybinds_scroll(compact_area, crate::tui::KEYBIND_ROWS.len());
        assert!(
            compact_maximum > maximum,
            "a shorter overlay scrolls further"
        );
        assert_eq!(scroll, compact_maximum);
        assert!(reduce_keybinds_scroll(&mut scroll, 0, area));
        assert_eq!(scroll, maximum, "resize must re-clamp the stored offset");
    }

    #[test]
    fn keybinds_wheel_overscroll_reverses_on_the_next_wheel_event() {
        let area = Rect::new(0, 0, 80, 24);
        let (_, _, initial_hits) = rendered_keybinds_position(0);
        let down = overlay_wheel_delta(MouseEventKind::ScrollDown, &initial_hits);
        let mut scroll = 0;

        for _ in 0..100 {
            reduce_keybinds_scroll(&mut scroll, down, area);
        }

        let maximum = maximum_keybinds_scroll(area, crate::tui::KEYBIND_ROWS.len());
        assert!(maximum > 10, "the inventory must overflow an 80x24 overlay");
        assert_eq!(scroll, maximum);
        let (last_row, last_thumb, last_hits) = rendered_keybinds_position(scroll);
        let up = overlay_wheel_delta(MouseEventKind::ScrollUp, &last_hits);
        assert!(reduce_keybinds_scroll(&mut scroll, up, area));
        assert_eq!(scroll, maximum - 3, "one wheel step scrolls three rows");
        let (reversed_row, reversed_thumb, _) = rendered_keybinds_position(scroll);
        assert_ne!(reversed_row, last_row);
        assert!(reversed_thumb.y < last_thumb.y);
    }

    #[test]
    fn runner_busy_save_check_is_a_key_dismissed_warning_toast() {
        let mut status: StatusMessage = crate::session::SAVE_CHECK_BUSY_WARNING.into();
        let toast = toast_for_status(&status).expect("busy save Check warning");
        assert_eq!(
            toast,
            ToastState::new(
                ToastKind::Warning,
                "warning",
                crate::session::SAVE_CHECK_BUSY_WARNING,
            )
        );
        dismiss_toast_on_key(
            &Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            &mut status,
            None,
            &mut None,
            &mut ToastTimer::default(),
        );
        assert!(status.is_empty());
    }

    #[test]
    fn completion_debounce_starts_when_a_polled_key_is_applied() {
        let mut trigger = CompletionTrigger::default();
        let loop_start_ms = 1_000;
        let key_applied_ms = 1_099;
        trigger.observe(
            CompletionTriggerInput::KeyboardInsertion('x'),
            2,
            completion_input_now_ms(loop_start_ms, key_applied_ms),
        );

        assert!(
            !trigger.take_expired(2, key_applied_ms + 199),
            "a key near the end of the poll interval must retain the full 200 ms pause"
        );
        assert!(trigger.take_expired(2, key_applied_ms + 200));
    }

    #[test]
    fn test_case_picker_wraps_and_runs_all_serially_before_reopening() {
        let cases = (0..10)
            .map(|index| crate::session::TestCase::new(format!("case-{index:02}")).unwrap())
            .collect::<Vec<_>>();
        let mut picker = TestCasePicker::default();
        picker.replace_cases(cases);
        picker.open();

        assert_eq!(picker.selected, 0);
        assert!(picker.move_selection(-1));
        assert_eq!(picker.selected, 10, "Up wraps onto Run all");
        assert!(picker.move_selection(1));
        assert_eq!(picker.selected, 0, "Down wraps onto the first case");
        picker.select(10);

        let first = picker.begin_selected().expect("Run all has a first case");
        assert_eq!(first.name(), "case-00");
        assert!(!picker.is_open());
        for index in 0..10 {
            let case = crate::session::TestCase::new(format!("case-{index:02}")).unwrap();
            let next = picker.record_completed(crate::session::TestCaseComparison {
                case,
                outcome: crate::session::TestCaseOutcome::Pass,
                expected_blake3: None,
                actual_blake3: None,
            });
            if index < 9 {
                assert_eq!(next.unwrap().name(), format!("case-{:02}", index + 1));
                assert!(!picker.is_open());
            } else {
                assert!(next.is_none());
                assert!(picker.is_open());
            }
        }
        assert!(picker.rows().iter().all(|row| row.status() == "PASS"));
    }

    #[test]
    fn test_case_picker_cancellation_stops_the_queue_and_reopens() {
        let mut picker = TestCasePicker::default();
        picker.replace_cases(
            ["alpha", "beta", "gamma"]
                .into_iter()
                .map(|name| crate::session::TestCase::new(name).unwrap())
                .collect(),
        );
        picker.open();
        picker.select(3);
        assert_eq!(picker.begin_selected().unwrap().name(), "alpha");
        picker.cancel_queue();
        assert!(
            picker
                .record_completed(crate::session::TestCaseComparison {
                    case: crate::session::TestCase::new("alpha").unwrap(),
                    outcome: crate::session::TestCaseOutcome::Error("cancelled".into()),
                    expected_blake3: None,
                    actual_blake3: None,
                })
                .is_none()
        );
        assert!(picker.is_open());
        assert_eq!(picker.rows()[0].status(), "ERROR");
        assert_eq!(picker.rows()[1].status(), "—");
    }

    #[test]
    fn test_case_picker_blocks_view_switches_until_the_sequence_finishes() {
        let mut picker = TestCasePicker::default();
        picker.replace_cases(
            ["alpha", "beta"]
                .into_iter()
                .map(|name| crate::session::TestCase::new(name).unwrap())
                .collect(),
        );
        picker.select(2);
        assert_eq!(picker.begin_selected().unwrap().name(), "alpha");
        assert!(picker.has_queued_cases());
        assert!(!test_case_view_switch_available(true, &picker));

        let next = picker
            .record_completed(crate::session::TestCaseComparison {
                case: crate::session::TestCase::new("alpha").unwrap(),
                outcome: crate::session::TestCaseOutcome::Pass,
                expected_blake3: None,
                actual_blake3: None,
            })
            .unwrap();
        assert_eq!(next.name(), "beta");
        assert!(!picker.has_queued_cases());
        assert!(!test_case_view_switch_available(true, &picker));
        assert!(test_case_view_switch_available(false, &picker));
    }

    #[test]
    fn test_case_picker_restores_the_view_and_focus_it_overlaid() {
        let mut picker = TestCasePicker::default();
        let mut view = WorkView::Console;
        let mut focus = WorkspaceFocus::Console;
        picker.open_from(view, focus);
        view = WorkView::Workspace;
        focus = WorkspaceFocus::Editor;

        picker.close_and_restore(&mut view, &mut focus);

        assert_eq!(view, WorkView::Console);
        assert_eq!(focus, WorkspaceFocus::Console);
    }

    #[test]
    fn refreshed_picker_prunes_removed_results_and_distinguishes_missing_v2_data() {
        let mut picker = TestCasePicker::default();
        picker.replace_cases(
            ["alpha", "beta"]
                .into_iter()
                .map(|name| crate::session::TestCase::new(name).unwrap())
                .collect(),
        );
        for name in ["alpha", "beta"] {
            picker.record_completed(crate::session::TestCaseComparison {
                case: crate::session::TestCase::new(name).unwrap(),
                outcome: crate::session::TestCaseOutcome::Pass,
                expected_blake3: None,
                actual_blake3: None,
            });
        }
        picker.replace_cases(vec![crate::session::TestCase::new("beta").unwrap()]);
        assert_eq!(picker.results.len(), 1);
        assert!(picker.results.contains_key("beta"));
        assert_eq!(
            TestCasePicker::missing_notice(false),
            "No packaged test cases (assignment format v1)"
        );
        assert_eq!(
            TestCasePicker::missing_notice(true),
            "Packaged test cases are unavailable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn absent_v1_sibling_directory_reaches_the_live_picker_test_backend() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = std::env::temp_dir().join(format!(
            "rustrace-picker-absent-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        let root = parent.join("assignment.work");
        fs::create_dir_all(&root).unwrap();
        let mut picker = TestCasePicker::default();

        picker.refresh(&root, false).unwrap();
        picker.open();

        let state = MainViewState::new(
            "Absent v1 cases",
            vec![BufferTabViewEntry::new("main.rs", true, false)],
            vec![],
            RecordingState::Active,
            JournalHealth::Healthy,
            "saved",
        )
        .with_test_case_picker(picker.view_state());
        let editor = EditorBuffer::new(
            DocumentId::new("absent-v1-cases").unwrap(),
            "fn main() {}\n",
            NoopEditorEffects,
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    MainView::new(&state, &editor, &Viewport::default(), &[]),
                    frame.area(),
                );
            })
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .chunks(80)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.contains("No packaged test cases (assignment format v1)"),
            "{screen}"
        );
        assert!(screen.contains("Run all"), "{screen}");
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn completed_test_case_does_not_hide_a_same_tick_error() {
        let error = io::Error::other("same tick failure");
        let mut save_triggered_check = false;
        assert_eq!(
            command_tick_status(
                Some(&error),
                true,
                true,
                false,
                false,
                "test case finished",
                &mut save_triggered_check,
            )
            .as_deref(),
            Some("Command stopped; inspect evidence or tool installation; Ctrl-Q quits")
        );
        assert_eq!(
            command_tick_status(
                None,
                true,
                true,
                false,
                false,
                "test case finished",
                &mut save_triggered_check,
            ),
            None
        );
    }

    #[test]
    fn update_menu_information_action_leaves_assignment_and_runner_unchanged() {
        let (fixture, mut session) = ClipboardFixture::new("A");
        let before = fixture.recorded_events();
        let mut menu = super::UpdateMenu::new(crate::update::UpdateState::default());
        let mut status: StatusMessage = "existing status".into();
        let mut view = WorkView::Console;
        let mut focus = WorkspaceFocus::Console;
        let mut picker = TestCasePicker::default();
        let mut keybinds = None;
        assert!(!activate_command_menu_entry(
            &mut session,
            6,
            &mut status,
            &mut view,
            &mut focus,
            &mut picker,
            &mut keybinds,
            &mut menu
        ));
        assert!(menu.open);
        assert!(!session.command_active());
        assert_eq!(view, WorkView::Console);
        assert_eq!(focus, WorkspaceFocus::Console);
        assert_eq!(status.as_str(), "existing status");
        assert_eq!(fixture.recorded_events(), before);
    }

    #[test]
    fn automatic_checks_toggle_changes_only_after_successful_persistence() {
        let mut menu = super::UpdateMenu::new(crate::update::UpdateState::default());
        menu.toggle_with(|enabled| {
            assert!(!enabled);
            Err(io::Error::other("read only"))
        });
        assert!(menu.state.checks_enabled);
        assert!(!menu.open);
        menu.toggle_with(|enabled| {
            assert!(!enabled);
            Ok(())
        });
        assert!(!menu.state.checks_enabled);
        menu.toggle_with(|enabled| {
            assert!(enabled);
            Ok(())
        });
        assert!(menu.state.checks_enabled);
    }

    #[test]
    fn command_menu_cannot_open_test_cases_while_a_command_is_active() {
        let (_fixture, mut session) = ClipboardFixture::new("A");
        session
            .install_stalled_preparing_for_test(std::time::Duration::ZERO)
            .unwrap();
        let mut status: StatusMessage = "active command status".into();
        let mut view = WorkView::Console;
        let mut focus = WorkspaceFocus::Console;
        let mut picker = TestCasePicker::default();
        let mut keybinds_scroll = None;
        let index = crate::tui::COMMAND_MENU_ENTRIES
            .iter()
            .position(|entry| *entry == "Test cases")
            .unwrap();

        assert!(!activate_command_menu_entry(
            &mut session,
            index,
            &mut status,
            &mut view,
            &mut focus,
            &mut picker,
            &mut keybinds_scroll,
            &mut super::UpdateMenu::new(crate::update::UpdateState::default()),
        ));

        assert!(!picker.is_open());
        assert_eq!(status.as_str(), "active command status");
        assert_eq!(view, WorkView::Console);
        assert_eq!(focus, WorkspaceFocus::Console);
    }

    #[cfg(unix)]
    #[test]
    fn disappeared_case_records_a_safe_prelaunch_error_row() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = std::env::temp_dir().join(format!(
            "rustrace-picker-prelaunch-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        let root = parent.join("assignment.work");
        fs::create_dir_all(parent.join("test-cases")).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(parent.join("test-cases/sample.in"), b"input\n").unwrap();
        fs::write(parent.join("test-cases/sample.expected"), b"output\n").unwrap();
        let manifest = br#"format_version = 1
course_id = "course"
assignment_id = "prelaunch"
assignment_version = "v1"
title = "Prelaunch"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;
        let mut session = ProductionSession::start(&root, manifest).unwrap();
        let mut picker = TestCasePicker::default();
        picker.refresh(&root, false).unwrap();
        let case = picker.begin_selected().unwrap();
        fs::remove_file(parent.join("test-cases/sample.in")).unwrap();

        start_test_case_sequence(&mut session, &mut picker, case);

        assert!(!session.command_active());
        assert!(picker.is_open());
        assert_eq!(picker.rows()[0].status(), "ERROR");
        let rows = test_case_output(
            b"stale output",
            &picker.visible_results,
            picker.visible_output_case.as_ref(),
            false,
            24,
        );
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0]
                .text()
                .starts_with("Test case sample: ERROR (could not start:"),
            "{}",
            rows[0].text()
        );
        session.quit().unwrap();
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refreshed_picker_prunes_results_when_pair_bytes_change() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = std::env::temp_dir().join(format!(
            "rustrace-picker-refresh-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(parent.join("test-cases")).unwrap();
        fs::create_dir(parent.join("assignment.work")).unwrap();
        fs::write(parent.join("test-cases/sample.in"), b"input").unwrap();
        fs::write(parent.join("test-cases/sample.expected"), b"old").unwrap();
        let directory = TestCaseDirectory::open(&parent.join("assignment.work")).unwrap();
        let original = directory.list_cases().unwrap();
        let mut picker = TestCasePicker::default();
        picker.replace_cases(original.clone());
        picker.record_completed(crate::session::TestCaseComparison {
            case: original[0].clone(),
            outcome: crate::session::TestCaseOutcome::Pass,
            expected_blake3: None,
            actual_blake3: None,
        });

        fs::write(parent.join("test-cases/sample.expected"), b"new").unwrap();
        picker.replace_cases(directory.list_cases().unwrap());

        assert!(picker.results.is_empty());
        assert_eq!(picker.rows()[0].status(), "—");
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn test_case_output_keeps_each_serial_summary_and_yields_to_later_commands() {
        let alpha = crate::session::TestCaseComparison {
            case: crate::session::TestCase::new("alpha").unwrap(),
            outcome: crate::session::TestCaseOutcome::Pass,
            expected_blake3: None,
            actual_blake3: None,
        };
        let beta = crate::session::TestCaseComparison {
            case: crate::session::TestCase::new("beta").unwrap(),
            outcome: crate::session::TestCaseOutcome::Error("cancelled".into()),
            expected_blake3: None,
            actual_blake3: None,
        };
        let mut picker = TestCasePicker::default();
        picker.note_started(alpha.case.clone());
        picker.record_completed(alpha);
        picker.record_completed(beta);

        let rows = test_case_output(
            b"alpha output\n",
            &picker.visible_results,
            picker.visible_output_case.as_ref(),
            false,
            24,
        )
        .into_iter()
        .map(|row| row.text().to_owned())
        .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                "alpha output",
                "",
                "Test case alpha: PASS",
                "Test case beta: ERROR (cancelled)",
            ]
        );
        picker.retire_visible_results();
        assert!(!picker.has_visible_results());
    }

    #[test]
    fn prelaunch_error_never_borrows_stale_command_output() {
        let mut picker = TestCasePicker::default();
        picker.record_completed(crate::session::TestCaseComparison {
            case: crate::session::TestCase::new("missing").unwrap(),
            outcome: crate::session::TestCaseOutcome::Error("could not start".into()),
            expected_blake3: None,
            actual_blake3: None,
        });
        let rows = test_case_output(
            b"stale output from another command",
            &picker.visible_results,
            picker.visible_output_case.as_ref(),
            false,
            24,
        );
        assert_eq!(
            rows.iter().map(OutputRow::text).collect::<Vec<_>>(),
            ["Test case missing: ERROR (could not start)"]
        );
    }

    #[test]
    fn non_test_output_ownership_covers_same_tick_preparation_failure() {
        assert!(non_test_command_owned_tick(true, false, false, false));
        assert!(non_test_command_owned_tick(false, false, true, false));
        assert!(!non_test_command_owned_tick(true, true, false, false));
    }

    #[test]
    fn test_case_picker_keys_wrap_run_refresh_close_and_ignore_releases() {
        let mut picker = TestCasePicker::default();
        picker.replace_cases(
            ["alpha", "beta"]
                .into_iter()
                .map(|name| crate::session::TestCase::new(name).unwrap())
                .collect(),
        );
        picker.open();
        assert_eq!(
            test_case_picker_key_action(
                &mut picker,
                &Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            ),
            TestCasePickerKeyAction::None
        );
        assert_eq!(picker.selected, 2);
        assert_eq!(
            test_case_picker_key_action(
                &mut picker,
                &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ),
            TestCasePickerKeyAction::Run
        );
        assert_eq!(
            test_case_picker_key_action(
                &mut picker,
                &Event::Key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE)),
            ),
            TestCasePickerKeyAction::Refresh
        );
        assert_eq!(
            test_case_picker_key_action(
                &mut picker,
                &Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ),
            TestCasePickerKeyAction::Close
        );
        assert_eq!(
            test_case_picker_key_action(
                &mut picker,
                &Event::Key(KeyEvent::new_with_kind(
                    KeyCode::Down,
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                )),
            ),
            TestCasePickerKeyAction::None
        );
        assert_eq!(picker.selected, 2);
    }

    #[test]
    fn test_case_result_summary_is_safe_complete_and_bounded() {
        let summary = test_case_result_summary(&crate::session::TestCaseComparison {
            case: crate::session::TestCase::new("bytes").unwrap(),
            outcome: crate::session::TestCaseOutcome::Fail(crate::session::TestCaseMismatch {
                line: 12,
                expected_len: 513,
                actual_len: 258,
                expected_preview: "expected \\u{1b}".into(),
                actual_preview: "actual \\xff".into(),
            }),
            expected_blake3: None,
            actual_blake3: None,
        });
        assert!(summary.starts_with("Test case bytes: FAIL at line 12"));
        assert!(summary.contains("expected (513 bytes) \"expected \\u{1b}\""));
        assert!(summary.contains("got (258 bytes) \"actual \\xff\""));
        assert!(summary.len() <= 512, "unbounded result summary: {summary}");
    }

    #[test]
    fn prompt_activity_retires_only_the_exact_transient_paste_warning() {
        let mut status: StatusMessage = PASTE_BLOCKED_WARNING.into();
        retire_paste_warning(&mut status);
        assert!(status.is_empty());
        for preserved in [
            "recovery required: writer failed; Ctrl-Q preserves session".to_owned(),
            "save failed; buffer remains dirty: writer failed".to_owned(),
            format!("{PASTE_BLOCKED_WARNING}; recovery required"),
        ] {
            let mut status: StatusMessage = preserved.clone().into();
            retire_paste_warning(&mut status);
            assert_eq!(status.as_str(), preserved);
        }
    }

    #[test]
    fn toast_is_dismissed_by_a_key_press_but_not_a_release_or_paste_event() {
        let mut timer = ToastTimer::default();
        let mut status: StatusMessage = EXPLICIT_SAVE_SUCCESS.into();
        let mut dismissed_health = None;
        dismiss_toast_on_key(
            &Event::Paste("ignored".to_owned()),
            &mut status,
            Some("journal degraded".to_owned()),
            &mut dismissed_health,
            &mut timer,
        );
        assert_eq!(status.as_str(), EXPLICIT_SAVE_SUCCESS);
        assert_eq!(dismissed_health, None);

        let mut release = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        dismiss_toast_on_key(
            &Event::Key(release),
            &mut status,
            Some("journal degraded".to_owned()),
            &mut dismissed_health,
            &mut timer,
        );
        assert_eq!(status.as_str(), EXPLICIT_SAVE_SUCCESS);
        assert_eq!(dismissed_health, None);

        dismiss_toast_on_key(
            &Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            &mut status,
            Some("journal degraded".to_owned()),
            &mut dismissed_health,
            &mut timer,
        );
        assert!(status.is_empty());
        assert_eq!(dismissed_health.as_deref(), Some("journal degraded"));
    }

    #[test]
    fn toast_clock_expires_at_ten_seconds_and_restarts_for_each_message() {
        let mut timer = ToastTimer::default();
        let mut dismissed_health = None;
        let mut status: StatusMessage = "first notice".into();

        assert!(
            toast_for_frame(
                &mut status,
                None,
                false,
                &mut dismissed_health,
                &mut timer,
                1_000,
            )
            .is_some()
        );
        assert!(
            toast_for_frame(
                &mut status,
                None,
                false,
                &mut dismissed_health,
                &mut timer,
                10_999,
            )
            .is_some()
        );
        assert!(
            toast_for_frame(
                &mut status,
                None,
                false,
                &mut dismissed_health,
                &mut timer,
                11_000,
            )
            .is_none()
        );
        assert!(status.is_empty());

        status = "second notice".into();
        assert!(
            toast_for_frame(
                &mut status,
                None,
                false,
                &mut dismissed_health,
                &mut timer,
                25_000,
            )
            .is_some()
        );
        assert!(
            toast_for_frame(
                &mut status,
                None,
                false,
                &mut dismissed_health,
                &mut timer,
                34_999,
            )
            .is_some()
        );
        assert!(
            toast_for_frame(
                &mut status,
                None,
                false,
                &mut dismissed_health,
                &mut timer,
                35_000,
            )
            .is_none()
        );
    }

    #[test]
    fn gated_status_toast_expires_without_resurrection_and_error_pill_persists() {
        let error = "External changes are not accepted; restored canonical bytes";
        let mut timer = ToastTimer::default();
        let mut dismissed_health = None;
        let mut status: StatusMessage = "unrelated status".into();

        assert!(
            toast_for_frame(
                &mut status,
                Some(error.to_owned()),
                true,
                &mut dismissed_health,
                &mut timer,
                0,
            )
            .is_some()
        );
        assert!(
            toast_for_frame(
                &mut status,
                Some(error.to_owned()),
                true,
                &mut dismissed_health,
                &mut timer,
                10_000,
            )
            .is_none()
        );
        assert_eq!(dismissed_health.as_deref(), Some(error));
        assert!(
            toast_for_frame(
                &mut status,
                Some(error.to_owned()),
                true,
                &mut dismissed_health,
                &mut timer,
                20_000,
            )
            .is_none(),
            "the gated status occurrence must age while the health toast is visible"
        );
        assert!(status.is_empty());
        assert_eq!(
            persistent_error_message(
                None,
                Some(error),
                RecordingState::Active,
                JournalHealth::Healthy,
            )
            .as_deref(),
            Some(error),
            "the persistent ERROR pill is independent of toast lifetime"
        );
    }

    #[test]
    fn consecutive_identical_status_toasts_each_get_ten_seconds() {
        let mut timer = ToastTimer::default();
        let mut dismissed_health = None;
        let mut status: StatusMessage = "same notice".into();

        assert!(
            toast_for_frame(
                &mut status,
                None,
                false,
                &mut dismissed_health,
                &mut timer,
                0,
            )
            .is_some()
        );

        status = "same notice".into();
        assert!(
            toast_for_frame(
                &mut status,
                None,
                false,
                &mut dismissed_health,
                &mut timer,
                9_000,
            )
            .is_some(),
            "the second occurrence must receive a fresh lifetime"
        );
        assert!(
            toast_for_frame(
                &mut status,
                None,
                false,
                &mut dismissed_health,
                &mut timer,
                18_999,
            )
            .is_some()
        );
        assert!(
            toast_for_frame(
                &mut status,
                None,
                false,
                &mut dismissed_health,
                &mut timer,
                19_000,
            )
            .is_none()
        );
    }

    #[test]
    fn blocked_paste_toast_expires_without_changing_the_recorded_rejection() {
        let mut timer = ToastTimer::default();
        let mut dismissed_health = None;
        let mut status: StatusMessage = PASTE_BLOCKED_WARNING.into();
        let recorded_rejection = (
            rustrace_model::PasteInputChannel::TerminalBracketed,
            rustrace_model::PasteRejectionReason::ExternalInput,
        );

        assert!(
            toast_for_frame(
                &mut status,
                None,
                false,
                &mut dismissed_health,
                &mut timer,
                0,
            )
            .is_some()
        );
        assert!(
            toast_for_frame(
                &mut status,
                None,
                false,
                &mut dismissed_health,
                &mut timer,
                10_000,
            )
            .is_none()
        );
        assert!(status.is_empty());
        assert_eq!(
            recorded_rejection,
            (
                rustrace_model::PasteInputChannel::TerminalBracketed,
                rustrace_model::PasteRejectionReason::ExternalInput,
            ),
            "toast retirement must not mutate the accepted rejection event"
        );
    }

    #[test]
    fn ordinary_output_does_not_prepend_the_idle_toolchain_placeholder() {
        let output = diagnostic_output("ordinary output", &[], 4);
        let output = diagnostic_output_text(&output);
        assert_eq!(output, "ordinary output");
        assert!(!output.contains(PASTE_BLOCKED_WARNING));
        assert!(!output.contains("Tools: observed"));
        assert!(!output.contains("item-2"));
    }

    #[test]
    fn idle_output_pane_is_empty_at_both_supported_golden_sizes() {
        for (width, height) in [(80, 24), (120, 40)] {
            let rows = diagnostic_output("", &[], 8);
            assert!(rows.is_empty(), "idle composition retained rows: {rows:?}");
            let state = MainViewState::new(
                "Idle output regression",
                vec![BufferTabViewEntry::new("main.rs", true, false)],
                vec![],
                RecordingState::Active,
                JournalHealth::Healthy,
                "saved",
            )
            .with_output_rows(rows);
            let editor = EditorBuffer::new(
                DocumentId::new("idle-output-regression").unwrap(),
                "fn main() {}\n",
                NoopEditorEffects,
            );
            let viewport = Viewport::default();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        MainView::new(&state, &editor, &viewport, &[]),
                        frame.area(),
                    );
                })
                .unwrap();
            let crate::tui::MainLayout::Full(layout) =
                crate::tui::main_layout(Rect::new(0, 0, width, height))
            else {
                unreachable!();
            };
            let buffer = terminal.backend().buffer();
            for y in layout.bottom.y + 1..layout.bottom.bottom() {
                let row = (layout.bottom.x..layout.bottom.right())
                    .filter_map(|x| buffer.cell((x, y)))
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(
                    row.trim().is_empty(),
                    "{width}x{height} idle row {y}: {row:?}"
                );
            }
        }
    }

    #[test]
    fn production_external_notice_keeps_source_text_and_renders_by_context() {
        let (fixture, mut session) = ClipboardFixture::new("A");
        fs::write(fixture.0.join("main.rs"), "external").unwrap();
        assert!(session.recheck_external().unwrap());
        let notice = session.external_notice().unwrap().to_owned();
        assert_eq!(
            notice,
            "External changes are not accepted.\nRustrace preserves recovery evidence.\nRestoring current contents, including unsaved edits."
        );

        let state = MainViewState::new(
            "External notice regression",
            vec![BufferTabViewEntry::new("main.rs", true, false)],
            vec![],
            RecordingState::Active,
            JournalHealth::Healthy,
            "saved",
        )
        .with_output_rows(vec![OutputRow::plain(&notice)])
        .with_error_condition(&notice);
        let editor = EditorBuffer::new(
            DocumentId::new("external-notice-regression").unwrap(),
            "A\n",
            NoopEditorEffects,
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    MainView::new(&state, &editor, &Viewport::default(), &[]),
                    frame.area(),
                );
            })
            .unwrap();
        let crate::tui::MainLayout::Full(layout) =
            crate::tui::main_layout(Rect::new(0, 0, 120, 40))
        else {
            unreachable!();
        };
        let row_text = |y| {
            (0..120)
                .filter_map(|x| terminal.backend().buffer().cell((x, y)))
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        let mode_bar = row_text(layout.mode_bar.y);
        assert!(
            mode_bar.contains(
                "External changes are not accepted. · Rustrace preserves recovery evidence. · "
            ),
            "{mode_bar:?}"
        );
        assert!(!mode_bar.contains(r"\n"));
        for (offset, expected) in [
            (1, "External changes are not accepted."),
            (2, "Rustrace preserves recovery evidence."),
            (3, "Restoring current contents, including unsaved edits."),
        ] {
            let row = row_text(layout.bottom.y + offset);
            assert!(row.trim_end().ends_with(expected), "{row:?}",);
        }
        assert!(!row_text(layout.bottom.y + 1).contains(r"\n"));
        session.quit().unwrap();
    }

    #[test]
    fn production_idle_screen_omits_healthy_journal_counters_across_navigation() {
        let mut health = SessionHealth {
            events: 1,
            checkpoint_pending: false,
            storage_bytes: 64 * 1024 * 1024,
            storage_headroom: 1_919 * 1024 * 1024,
            warning: false,
            undo_bytes: 0,
            undo_limited: false,
            recovery_reason: None,
        };
        for (events, navigate, position) in [(1, false, "Ln 1, Col 1"), (2, true, "Ln 1, Col 2")] {
            health.events = events;
            let output = rendered_production_idle_output(&health, navigate);
            assert!(output.contains(" files"), "missing idle chrome:\n{output}");
            assert!(output.contains("output"), "missing output label:\n{output}");
            assert!(
                !output.contains("Tools:"),
                "idle toolchain row remained:\n{output}"
            );
            assert!(
                output.contains(position),
                "navigation was not rendered:\n{output}"
            );
            assert!(
                !["events ", "storage ", "headroom "]
                    .iter()
                    .any(|counter| output.contains(counter)),
                "healthy journal counters occupied the idle output:\n{output}"
            );
        }
    }

    #[test]
    fn budget_and_undo_warnings_use_the_transient_warning_toast() {
        let health = SessionHealth {
            events: 800_000,
            checkpoint_pending: false,
            storage_bytes: 64 * 1024 * 1024,
            storage_headroom: 1_919 * 1024 * 1024,
            warning: true,
            undo_bytes: 0,
            undo_limited: true,
            recovery_reason: None,
        };
        let message = "80% budget warning | undo limit reached";
        assert_eq!(
            journal_warning_message(Some(&health)).as_deref(),
            Some(message)
        );
        assert_eq!(
            toast_for_status(message),
            Some(ToastState::new(ToastKind::Warning, "warning", message))
        );
        let output = rendered_production_idle_output(&health, false);
        assert!(output.contains("80% budget warning"), "{output}");
        assert!(output.contains("undo limit reached"), "{output}");
        assert!(!output.contains(" ERROR "), "{output}");
        assert!(!output.contains("events 800000"), "{output}");
        assert!(!output.contains("storage 64 MiB"), "{output}");
        assert!(!output.contains("headroom 1919 MiB"), "{output}");
    }

    #[test]
    fn external_reconciliation_keeps_error_after_key_only_toast_dismissal() {
        let external = "External changes are not accepted; restored canonical bytes";
        let persistent = persistent_error_message(
            None,
            Some(external),
            RecordingState::Active,
            JournalHealth::Healthy,
        );
        assert_eq!(persistent.as_deref(), Some(external));

        let mut status: StatusMessage = "".into();
        let mut dismissed = None;
        let mut timer = ToastTimer::default();
        dismiss_toast_on_key(
            &Event::Paste("ignored".to_owned()),
            &mut status,
            persistent.clone(),
            &mut dismissed,
            &mut timer,
        );
        assert_eq!(dismissed, None);

        let mut release = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        dismiss_toast_on_key(
            &Event::Key(release),
            &mut status,
            persistent.clone(),
            &mut dismissed,
            &mut timer,
        );
        assert_eq!(dismissed, None);

        dismiss_toast_on_key(
            &Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            &mut status,
            persistent.clone(),
            &mut dismissed,
            &mut timer,
        );
        assert_eq!(dismissed.as_deref(), Some(external));
        assert_eq!(
            persistent_error_message(
                None,
                Some(external),
                RecordingState::Active,
                JournalHealth::Healthy,
            ),
            persistent,
            "dismissing the toast must not clear the persistent ERROR condition"
        );
    }

    #[test]
    fn file_path_prompt_starts_empty_for_create_and_prefilled_at_end_for_rename() {
        let mut create = PathPrompt::new(PathOperation::Create, "");
        assert_eq!(create.input, "");
        assert_eq!(submitted_path(&create), "src/");
        assert_eq!(
            create.update(Event::Key(KeyEvent::new(
                KeyCode::Backspace,
                KeyModifiers::NONE,
            ))),
            PathPromptAction::Continue
        );
        assert_eq!(create.input, "");
        for character in "foo.rs".chars() {
            assert_eq!(
                create.update(Event::Key(KeyEvent::new(
                    KeyCode::Char(character),
                    KeyModifiers::NONE,
                ))),
                PathPromptAction::Edited
            );
        }
        assert_eq!(submitted_path(&create), "src/foo.rs");

        let mut nested = PathPrompt::new(PathOperation::Create, "");
        for character in "util/mod.rs".chars() {
            nested.update(Event::Key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::NONE,
            )));
        }
        assert_eq!(submitted_path(&nested), "src/util/mod.rs");

        let mut rename = PathPrompt::new(PathOperation::Rename, "src/main.rs");
        assert_eq!(submitted_path(&rename), "src/main.rs");
        assert_eq!(
            rename.update(Event::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            ))),
            PathPromptAction::Edited
        );
        assert_eq!(rename.input, "src/main.rsx");
        assert_eq!(
            rename.update(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            ))),
            PathPromptAction::Submit
        );
        assert_eq!(
            rename.update(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))),
            PathPromptAction::Cancel
        );
    }

    #[test]
    fn shifted_characters_are_preserved_in_file_name_prompts() {
        for operation in [PathOperation::Create, PathOperation::Rename] {
            let mut prompt = PathPrompt::new(operation, "");
            for character in ['(', 'A', '{', '"', 'É'] {
                assert_eq!(
                    prompt.update(Event::Key(KeyEvent::new(
                        KeyCode::Char(character),
                        KeyModifiers::SHIFT,
                    ))),
                    PathPromptAction::Edited,
                );
            }
            assert_eq!(prompt.input, "(A{\"É");
        }
    }

    #[test]
    fn find_panel_fields_preserve_shifted_input_and_tab_enter_escape_parity() {
        let mut panel = Some(FindPanel::new("selected"));
        for character in ['(', 'A', '{', '"', 'É'] {
            assert_eq!(
                route_find_panel_event(
                    &mut panel,
                    Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::SHIFT,)),
                ),
                Some(FindPanelAction::Edited),
            );
        }
        let panel = panel.as_mut().unwrap();
        assert_eq!(panel.find, "selected(A{\"É");
        assert_eq!(panel.active, FindPanelField::Find);
        assert_eq!(
            panel.update(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE,))),
            FindPanelAction::Edited
        );
        assert_eq!(panel.active, FindPanelField::Replace);
        assert_eq!(
            panel.update(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            ))),
            FindPanelAction::Next
        );
        assert_eq!(
            panel.update(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE,))),
            FindPanelAction::Close
        );
    }

    #[test]
    fn find_panel_rejects_external_paste_in_both_fields_without_insertion() {
        for active in [FindPanelField::Find, FindPanelField::Replace] {
            let mut panel = FindPanel::new("needle");
            panel.active = active;
            let before = panel.clone();
            let event = Event::Paste("blocked".to_owned());
            assert_eq!(
                paste_rejection_for_event(&event, true, PrimaryModifier::Control),
                Some((
                    PasteInputChannel::TerminalBracketed,
                    PasteRejectionReason::ExternalInput,
                ))
            );
            assert_eq!(panel.update(event), FindPanelAction::Continue);
            assert_eq!(panel, before);
        }
    }

    #[test]
    fn open_find_panel_consumes_keyboard_input_before_the_console_line() {
        let mut panel = Some(FindPanel::new(""));
        let mut console = ConsoleLine::default();
        assert!(console.insert('Q'));

        assert_eq!(
            route_find_panel_event(
                &mut panel,
                Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE,)),
            ),
            Some(FindPanelAction::Edited)
        );
        assert_eq!(panel.unwrap().find, "x");
        assert_eq!(console.text(), "Q");
    }

    #[test]
    fn console_line_preserves_shifted_characters_delivered_by_crossterm() {
        let mut line = ConsoleLine::default();
        for key in ['(', 'A', '{', '"', 'É']
            .map(|character| KeyEvent::new(KeyCode::Char(character), KeyModifiers::SHIFT))
        {
            let KeyCode::Char(character) = key.code else {
                unreachable!("fixture uses character keys");
            };
            assert!(
                !key.modifiers.intersects(
                    KeyModifiers::CONTROL
                        | KeyModifiers::ALT
                        | KeyModifiers::SUPER
                        | KeyModifiers::HYPER
                        | KeyModifiers::META,
                ) && line.insert(character),
                "shifted character {character:?} was transformed or rejected",
            );
        }
        assert_eq!(line.text(), "(A{\"É");
    }

    #[test]
    fn file_path_panel_preserves_exact_external_paste_rejections() {
        assert_eq!(
            paste_rejection_for_event(
                &Event::Paste("blocked".to_owned()),
                true,
                crate::config::PrimaryModifier::Control,
            ),
            Some((
                PasteInputChannel::TerminalBracketed,
                PasteRejectionReason::ExternalInput,
            ))
        );
        assert_eq!(
            paste_rejection_for_event(
                &Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL,)),
                true,
                crate::config::PrimaryModifier::Control,
            ),
            Some((
                PasteInputChannel::InternalShortcut,
                PasteRejectionReason::OutsideEditor,
            ))
        );
        assert_eq!(
            paste_rejection_for_event(
                &Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::SUPER)),
                true,
                crate::config::PrimaryModifier::Command,
            ),
            Some((
                PasteInputChannel::InternalShortcut,
                PasteRejectionReason::OutsideEditor,
            ))
        );

        let mut recorded_rejections = Vec::new();
        for primary_modifier in [
            crate::config::PrimaryModifier::Control,
            crate::config::PrimaryModifier::Command,
        ] {
            for modifiers in [
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ] {
                if let Some(rejection) = paste_rejection_for_event(
                    &Event::Key(KeyEvent::new(KeyCode::Char('v'), modifiers)),
                    true,
                    primary_modifier,
                ) {
                    recorded_rejections.push(rejection);
                }
            }
        }
        assert!(
            recorded_rejections.is_empty(),
            "mixed Control-V must not record a new paste rejection: {recorded_rejections:?}"
        );
        assert_eq!(
            paste_rejection_for_event(
                &Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::SUPER)),
                true,
                crate::config::PrimaryModifier::Control,
            ),
            None,
        );
    }

    #[test]
    fn escape_closes_console_and_removed_chords_are_inert() {
        let (_fixture, mut session) = ClipboardFixture::new("A");
        let (cancel, _) = session
            .install_stalled_console_preparing_for_test(std::time::Duration::ZERO)
            .unwrap();
        let mut line = ConsoleLine::default();
        let mut view = WorkView::Console;
        let mut focus = WorkspaceFocus::Console;
        let mut status: StatusMessage = "active console command".into();
        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::SUPER] {
            for code in ['c', 'C', 'd', 'D'] {
                handle_console_key(
                    &mut session,
                    &KeyEvent::new(KeyCode::Char(code), modifiers),
                    &mut line,
                    &mut ConsoleScroll::default(),
                    &mut view,
                    &mut focus,
                    &mut status,
                );
                assert!(session.command_active(), "{modifiers:?}-{code}");
                assert!(session.console_command_active(), "{modifiers:?}-{code}");
                assert_eq!(cancel.load(Ordering::Acquire), 0, "{modifiers:?}-{code}");
                assert_eq!(view, WorkView::Console);
                assert_eq!(focus, WorkspaceFocus::Console);
                assert_eq!(line.text(), "");
                assert_eq!(&*status, "active console command");
            }
        }
        let mut release = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        handle_console_key(
            &mut session,
            &release,
            &mut line,
            &mut ConsoleScroll::default(),
            &mut view,
            &mut focus,
            &mut status,
        );
        assert!(session.console_command_active());
        assert_eq!(cancel.load(Ordering::Acquire), 0);
        assert_eq!(view, WorkView::Console);
        assert_eq!(focus, WorkspaceFocus::Console);
        handle_console_key(
            &mut session,
            &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut line,
            &mut ConsoleScroll::default(),
            &mut view,
            &mut focus,
            &mut status,
        );
        assert_eq!(
            cancel.load(Ordering::Acquire),
            1,
            "Esc uses the cancel path"
        );
        assert_eq!(view, WorkView::Workspace);
        assert_eq!(focus, WorkspaceFocus::Editor);
        assert!(status.is_empty());
    }

    #[test]
    fn console_cargo_lines_render_flush_left_and_program_lines_keep_spaces() {
        for line in [
            "Compiling",
            "Checking",
            "Finished",
            "Generated",
            "Running",
            "Documenting",
            "Downloading",
            "Downloaded",
            "Updating",
            "Adding",
            "Removing",
            "Locking",
            "Blocking",
            "Fresh",
            "Dirty",
            "Building",
            "Installing",
            "Installed",
            "Ignored",
            "Packaging",
            "Uploading",
            "Warning",
            "Error",
            "warning:",
            "error:",
            "error[E0308]:",
            "note:",
            "help:",
            "-->",
            "|",
            "12 | source",
            "| ^^^",
        ] {
            let original = format!(
                "    {line} fixture\n   Generated index.html\n    board row\n    arbitrary stderr\n"
            );
            let rows = rendered_console_rows(&original);
            assert!(
                rows.iter()
                    .any(|row| row.starts_with(&format!("{line} fixture"))),
                "{line}: {rows:?}"
            );
            for expected in [
                "Generated index.html",
                "    board row",
                "    arbitrary stderr",
            ] {
                assert!(rows.iter().any(|row| row.starts_with(expected)), "{rows:?}");
            }
        }
    }

    #[test]
    fn console_diagnostic_blocks_preserve_source_and_caret_columns() {
        let diagnostic = "error[E0308]: mismatched types\n    --> src/main.rs:12:5\n   |\n12 |     let x = 1;\n   |     ^^^ expected string\n";
        for indentation in ["", "    "] {
            let original = diagnostic
                .lines()
                .map(|line| format!("{indentation}{line}\n"))
                .collect::<String>()
                + "\n    board row\n    arbitrary stderr\n";
            let rows = rendered_console_rows(&original);
            let source = rows.iter().find(|row| row.contains("let x = 1;")).unwrap();
            let caret = rows
                .iter()
                .find(|row| row.contains("^^^ expected"))
                .unwrap();
            assert_eq!(source.find("let"), caret.find("^^^"), "{rows:?}");
            for expected in diagnostic.lines() {
                assert!(rows.iter().any(|row| row.starts_with(expected)), "{rows:?}");
            }
            for expected in ["    board row", "    arbitrary stderr"] {
                assert!(rows.iter().any(|row| row.starts_with(expected)), "{rows:?}");
            }
        }
    }

    fn numbered_output(id: u64, lines: std::ops::Range<usize>) -> LiveSnapshot {
        // Each "line-NNN\n" is nine bytes, so line N starts at offset 9 * N.
        LiveSnapshot {
            id,
            start: u64::try_from(lines.start * 9).unwrap(),
            bytes: lines
                .flat_map(|index| format!("line-{index:03}\n").into_bytes())
                .collect(),
        }
    }

    #[test]
    fn console_scroll_pins_a_source_line_and_resumes_following_at_the_bottom() {
        let mut scroll = ConsoleScroll::default();
        scroll.measure(&numbered_output(1, 0..100), 80, 10);
        assert_eq!(scroll.top(), None);
        assert_eq!(scroll.page(), 9);
        assert!(scroll.scroll_by(-5));
        assert_eq!(scroll.top(), Some(85));

        // New output arrives and the oldest lines leave the live buffer: the
        // pinned line stays at the top of the window.
        scroll.measure(&numbered_output(1, 20..130), 80, 10);
        assert_eq!(scroll.top(), Some(65));
        // A narrower pane reflows each line onto two rows around the same line.
        scroll.measure(&numbered_output(1, 20..130), 4, 10);
        assert_eq!(scroll.top(), Some(130));
        scroll.measure(&numbered_output(1, 20..130), 80, 10);
        assert_eq!(scroll.top(), Some(65));

        // Once the pinned line leaves the scrollback, the oldest remaining line
        // is pinned instead of the view drifting with new output.
        scroll.measure(&numbered_output(1, 90..200), 80, 10);
        assert_eq!(scroll.top(), Some(0));
        scroll.measure(&numbered_output(1, 90..210), 80, 10);
        assert_eq!(scroll.top(), Some(0));
        scroll.measure(&numbered_output(1, 20..130), 80, 10);
        assert!(scroll.scroll_by(-1000));
        assert_eq!(scroll.top(), Some(0));
        assert!(!scroll.scroll_by(-1));
        // Reaching the newest rows follows again.
        assert!(scroll.scroll_by(1000));
        assert_eq!(scroll.top(), None);
        assert!(!scroll.scroll_by(3));

        // A new command's output starts at its newest line.
        assert!(scroll.scroll_by(-3));
        scroll.measure(&numbered_output(2, 0..100), 80, 10);
        assert_eq!(scroll.top(), None);
        // Short output never scrolls.
        scroll.measure(&numbered_output(3, 0..4), 80, 10);
        assert!(!scroll.scroll_by(-3));
        assert_eq!(scroll.top(), None);
    }

    fn rendered_console_rows(output: &str) -> Vec<String> {
        let state = MainViewState::new(
            "Cargo display",
            vec![],
            vec![],
            RecordingState::Active,
            JournalHealth::Healthy,
            "saved",
        )
        .with_bottom_pane_height(16)
        .with_console_body_view(
            "Console",
            output.as_bytes().to_vec(),
            b"> ".to_vec(),
            None,
            true,
        );
        let editor = EditorBuffer::new(
            DocumentId::new("cargo-display").unwrap(),
            "",
            NoopEditorEffects,
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
        let hits = MainView::new(&state, &editor, &Viewport::default(), &[])
            .render_with_hit_map(Rect::new(0, 0, 120, 40), &mut buffer);
        (hits.console.y + 1..hits.console.bottom() - 1)
            .map(|y| {
                (hits.console.x..hits.console.right())
                    .filter_map(|x| buffer.cell((x, y)))
                    .map(|cell| cell.symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
    }

    #[test]
    fn inactive_console_uses_the_shell_neutral_prompt() {
        let body = console_body(&[], &ConsoleLine::default(), false, false, false);
        assert_eq!(body.prompt, b"> ");
        assert_eq!(body.cursor, Some(2));
    }

    #[test]
    fn final_diagnostic_composition_uses_the_full_row_budget_without_status() {
        let output = diagnostic_output(
            "unused normal message",
            &[
                OutputRow::plain("Diagnostics 3/5"),
                OutputRow::diagnostic("> error third selected", 2),
                OutputRow::captured("program result", OutputStream::Stdout),
            ],
            4,
        );
        let output = diagnostic_output_text(&output);

        assert_eq!(output.lines().count(), 3);
        assert!(output.contains("> error third selected"));
        assert!(output.contains("program result"));
        assert!(!output.contains("stdout:"));
        assert!(!output.contains("ready"));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PathOperation {
    Create,
    Rename,
}

struct PathPrompt {
    operation: PathOperation,
    input: String,
}

impl PathPrompt {
    fn new(operation: PathOperation, initial: &str) -> Self {
        Self {
            operation,
            input: initial.to_owned(),
        }
    }

    fn update(&mut self, event: Event) -> PathPromptAction {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Enter => return PathPromptAction::Submit,
                KeyCode::Esc => return PathPromptAction::Cancel,
                KeyCode::Backspace => {
                    return if self.input.pop().is_some() {
                        PathPromptAction::Edited
                    } else {
                        PathPromptAction::Continue
                    };
                }
                KeyCode::Char(character)
                    if !key.modifiers.intersects(
                        KeyModifiers::CONTROL
                            | KeyModifiers::ALT
                            | KeyModifiers::SUPER
                            | KeyModifiers::HYPER
                            | KeyModifiers::META,
                    ) =>
                {
                    if character.is_control() {
                        return PathPromptAction::RejectedUnsafe;
                    }
                    if !self.can_append(character.len_utf8()) {
                        return PathPromptAction::RejectedTooLong;
                    }
                    self.input.push(character);
                    return PathPromptAction::Edited;
                }
                _ => {}
            },
            _ => {}
        }
        PathPromptAction::Continue
    }

    fn can_append(&self, additional_bytes: usize) -> bool {
        self.input
            .len()
            .checked_add(additional_bytes)
            .is_some_and(|length| length <= MAX_WORKSPACE_PATH_BYTES)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PathPromptAction {
    Continue,
    Edited,
    Cancel,
    Submit,
    RejectedTooLong,
    RejectedUnsafe,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FindPanel {
    find: String,
    replace: String,
    active: FindPanelField,
}

impl FindPanel {
    fn new(find: &str) -> Self {
        Self {
            find: find.to_owned(),
            replace: String::new(),
            active: FindPanelField::Find,
        }
    }

    fn update(&mut self, event: Event) -> FindPanelAction {
        let Event::Key(key) = event else {
            return FindPanelAction::Continue;
        };
        if key.kind == KeyEventKind::Release {
            return FindPanelAction::Continue;
        }
        match key.code {
            KeyCode::Enter => FindPanelAction::Next,
            KeyCode::Esc => FindPanelAction::Close,
            KeyCode::Tab | KeyCode::BackTab => {
                self.active = match self.active {
                    FindPanelField::Find => FindPanelField::Replace,
                    FindPanelField::Replace => FindPanelField::Find,
                };
                FindPanelAction::Edited
            }
            KeyCode::Backspace => {
                let input = self.active_input_mut();
                if input.pop().is_some() {
                    FindPanelAction::Edited
                } else {
                    FindPanelAction::Continue
                }
            }
            KeyCode::Char(character)
                if !character.is_control()
                    && !key.modifiers.intersects(
                        KeyModifiers::CONTROL
                            | KeyModifiers::ALT
                            | KeyModifiers::SUPER
                            | KeyModifiers::HYPER
                            | KeyModifiers::META,
                    ) =>
            {
                let input = self.active_input_mut();
                if input
                    .len()
                    .checked_add(character.len_utf8())
                    .is_some_and(|length| length <= 4096)
                {
                    input.push(character);
                    FindPanelAction::Edited
                } else {
                    FindPanelAction::Continue
                }
            }
            _ => FindPanelAction::Continue,
        }
    }

    fn active_input_mut(&mut self) -> &mut String {
        match self.active {
            FindPanelField::Find => &mut self.find,
            FindPanelField::Replace => &mut self.replace,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FindPanelAction {
    Continue,
    Edited,
    Next,
    Replace,
    ReplaceAll,
    Close,
}

fn route_find_panel_event(panel: &mut Option<FindPanel>, event: Event) -> Option<FindPanelAction> {
    panel.as_mut().map(|panel| panel.update(event))
}

fn begin_find_panel(session: &crate::session::ProductionSession) -> FindPanel {
    FindPanel::new(
        session
            .workspace()
            .selected_text_for_find()
            .as_deref()
            .unwrap_or(""),
    )
}

fn apply_find_panel_action(
    session: &mut crate::session::ProductionSession,
    panel: &mut Option<FindPanel>,
    action: FindPanelAction,
    status: &mut StatusMessage,
    follow_cursor: &mut bool,
) {
    match action {
        FindPanelAction::Continue => {}
        FindPanelAction::Edited => {
            let query = panel
                .as_ref()
                .map(|panel| panel.find.clone())
                .unwrap_or_default();
            session.workspace_mut().remember_search(query);
            retire_paste_warning(status);
        }
        FindPanelAction::Next => {
            let query = panel
                .as_ref()
                .map(|panel| panel.find.clone())
                .unwrap_or_default();
            let outcome = match session.execute(EditorCommand::Search(query)) {
                Ok(outcome) => outcome,
                Err(error) => {
                    status.replace(format!("edit rejected: {error}"));
                    EditorOutcome::NoChange
                }
            };
            let (follow, _) = apply_outcome(outcome, status);
            *follow_cursor = follow;
        }
        FindPanelAction::Replace => {
            let Some(panel) = panel.as_mut() else {
                return;
            };
            let command = EditorCommand::ReplaceCurrent {
                query: panel.find.clone(),
                replacement: panel.replace.clone(),
            };
            let outcome = match session.execute(command) {
                Ok(outcome) => outcome,
                Err(error) => {
                    status.replace(format!("edit rejected: {error}"));
                    EditorOutcome::NoChange
                }
            };
            let (follow, _) = apply_outcome(outcome, status);
            *follow_cursor = follow;
        }
        FindPanelAction::ReplaceAll => {
            let Some(current) = panel.as_ref() else {
                return;
            };
            let command = EditorCommand::ReplaceAll {
                query: current.find.clone(),
                replacement: current.replace.clone(),
            };
            let outcome = match session.execute(command) {
                Ok(outcome) => outcome,
                Err(error) => {
                    status.replace(format!("edit rejected: {error}"));
                    EditorOutcome::NoChange
                }
            };
            let (follow, _) = apply_outcome(outcome, status);
            *follow_cursor = follow;
        }
        FindPanelAction::Close => {
            if let Some(panel) = panel.take() {
                session.workspace_mut().remember_search(panel.find);
            }
            status.replace("find closed");
        }
    }
}

fn execute_workspace_with_status<S>(
    workspace: &mut WorkspaceSession<S>,
    command: EditorCommand,
    status: &mut StatusMessage,
) -> EditorOutcome
where
    S: WorkspaceEffects,
{
    match workspace.execute_editor(command) {
        Ok(outcome) => outcome,
        Err(error) => {
            status.replace(format!("edit rejected: {error}"));
            EditorOutcome::NoChange
        }
    }
}

fn apply_workspace_outcome(outcome: WorkspaceOutcome, status: &mut StatusMessage) {
    let message = match outcome {
        WorkspaceOutcome::NoChange => return,
        WorkspaceOutcome::TreeSelectionChanged => String::from("selected workspace file"),
        WorkspaceOutcome::FileActivated => {
            status.clear();
            return;
        }
        WorkspaceOutcome::FileCreated => String::from("created and opened file"),
        WorkspaceOutcome::FileRenamed => String::from("renamed file"),
        WorkspaceOutcome::FileDeleted => String::from("deleted file"),
        WorkspaceOutcome::ConfirmationRequired => {
            String::from("dirty file: Y/Enter deletes, N/Esc cancels")
        }
        WorkspaceOutcome::Cancelled => String::from("file operation cancelled"),
    };
    status.replace(message);
}

fn apply_outcome(outcome: EditorOutcome, status: &mut StatusMessage) -> (bool, bool) {
    match outcome {
        EditorOutcome::NoChange => (false, false),
        EditorOutcome::Edited | EditorOutcome::SelectionChanged => (true, false),
        EditorOutcome::Replaced(count) => {
            status.replace(format!("replaced {count}"));
            (true, false)
        }
        EditorOutcome::Copied => {
            status.replace("selection copied to the editor clipboard");
            (false, false)
        }
        EditorOutcome::Search(SearchOutcome::Match { wrapped, .. }) => {
            status.replace(if wrapped {
                String::from("match found after wrapping to the start")
            } else {
                String::from("match found")
            });
            (true, false)
        }
        EditorOutcome::Search(SearchOutcome::NoMatch) => {
            status.replace("no match");
            (false, false)
        }
        EditorOutcome::Search(SearchOutcome::EmptyQuery) => {
            status.replace("search query is empty");
            (false, false)
        }
        EditorOutcome::BufferSwitched => {
            status.replace("switched active buffer");
            (true, false)
        }
        EditorOutcome::ConfirmationRequired(action) => {
            status.replace(format!(
                "confirm {action:?}: Y/Enter confirms, N/Esc cancels"
            ));
            (false, false)
        }
        EditorOutcome::BufferClosed => {
            status.replace("buffer closed");
            (true, false)
        }
        EditorOutcome::LastBuffer => {
            status.replace("the last buffer stays open; use Ctrl-Q to quit");
            (false, false)
        }
        EditorOutcome::Quit => (false, true),
        EditorOutcome::Cancelled => {
            status.replace("discard cancelled; buffer preserved");
            (false, false)
        }
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;

    #[test]
    fn failed_or_unreadable_health_degrades_but_budget_warning_does_not() {
        let mut health = crate::session::SessionHealth {
            events: 10,
            checkpoint_pending: false,
            storage_bytes: 100,
            storage_headroom: 0,
            warning: true,
            undo_bytes: 0,
            undo_limited: false,
            recovery_reason: Some("event budget exhausted".to_owned()),
        };
        assert_eq!(
            recording_indicators(Some(&health)),
            (RecordingState::Inactive, JournalHealth::Degraded)
        );
        assert_eq!(
            recording_indicators(None),
            (RecordingState::Inactive, JournalHealth::Degraded)
        );
        health.recovery_reason = None;
        assert_eq!(
            recording_indicators(Some(&health)),
            (RecordingState::Active, JournalHealth::Healthy)
        );
    }
}
