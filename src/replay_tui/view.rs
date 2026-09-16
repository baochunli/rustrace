use std::{io, path::Path, time::Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::{
    Terminal, TerminalOptions, Viewport as TerminalViewport,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Widget},
};
use rustrace_editor::{EditorBuffer, NoopEditorEffects};
use rustrace_model::DocumentId;
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    display,
    editor::{EditorWidget, Viewport, highlight_path},
    process_indicators::{
        INDICATOR_EXPLANATION, ProcessRuleOutcome, display_factual_indicators,
        display_process_attempt_details,
    },
    review_flags::{
        EVIDENCE_LIMITATION_FIRST, EVIDENCE_LIMITATION_SECOND, EvidenceOutcome, display_advisory,
        display_flag, evidence_outcome, evidence_statement,
    },
    tui::{
        CrosstermTerminalOperations, TerminalSession,
        shell::{
            ReplayShellLayout, ReplayShellLayoutResult, display_width, put_right_text, put_text,
            replay_shell_layout, set_style,
        },
        theme::Palette,
    },
    verify::{AssignmentReferenceStatus, SubmittedSourceStatus, VerificationStatus},
};

use super::{
    ComparisonMode, DiffContent, DiffLine, DiffLineKind, EventPosition, EvidenceTarget,
    PasteMarker, PlaybackSpeed, ReplayController, diff::NO_NEWLINE_AT_END_MARKER,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DiffPaneState {
    offset: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplayTab {
    Source,
    Diff,
    Flags,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ReplayHitMap {
    area: Rect,
    files: Vec<(Rect, usize)>,
    events: Vec<(Rect, EventPosition)>,
    tabs: Vec<(Rect, ReplayTab)>,
    play_pause: Rect,
    events_pane: Rect,
    source: Rect,
    details: Rect,
    details_max_scroll: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplayMouseInput {
    SelectEvent(EventPosition),
    SelectFile(usize),
    ShowTab(ReplayTab),
    TogglePlay,
    ScrollEvents { delta: isize, admitted: bool },
    ScrollSource(isize),
    ScrollDetails(isize),
    ScrollDiff(isize),
    ScrollFlags(isize),
}

fn replay_mouse_input_for_event(
    event: &MouseEvent,
    hits: &ReplayHitMap,
    show_flags: bool,
    diff_open: bool,
) -> Option<ReplayMouseInput> {
    let position = Position::new(event.column, event.row);
    let wheel = match event.kind {
        MouseEventKind::ScrollUp => Some(-3),
        MouseEventKind::ScrollDown => Some(3),
        _ => None,
    };
    if let Some(delta) = wheel {
        if hits.events_pane.contains(position) {
            return Some(ReplayMouseInput::ScrollEvents {
                delta,
                admitted: true,
            });
        }
        if hits.source.contains(position) {
            return Some(if show_flags && !diff_open {
                ReplayMouseInput::ScrollFlags(delta)
            } else if diff_open {
                ReplayMouseInput::ScrollDiff(delta)
            } else {
                ReplayMouseInput::ScrollSource(delta)
            });
        }
        if hits.details.contains(position) {
            return Some(ReplayMouseInput::ScrollDetails(delta));
        }
        return None;
    }
    if event.kind != MouseEventKind::Down(MouseButton::Left)
        || event.modifiers != KeyModifiers::NONE
    {
        return None;
    }
    hits.events
        .iter()
        .find_map(|(rect, event_position)| {
            rect.contains(position)
                .then_some(ReplayMouseInput::SelectEvent(*event_position))
        })
        .or_else(|| {
            hits.files.iter().find_map(|(rect, index)| {
                rect.contains(position)
                    .then_some(ReplayMouseInput::SelectFile(*index))
            })
        })
        .or_else(|| {
            hits.tabs.iter().find_map(|(rect, tab)| {
                rect.contains(position)
                    .then_some(ReplayMouseInput::ShowTab(*tab))
            })
        })
        .or_else(|| {
            hits.play_pause
                .contains(position)
                .then_some(ReplayMouseInput::TogglePlay)
        })
}

fn replay_mouse_input_for_batch(
    batch: &ReplayInputBatch,
    hits: &ReplayHitMap,
    show_flags: bool,
    diff_open: bool,
) -> Option<ReplayMouseInput> {
    match batch.events_click {
        ReplayEventsClick::Event(position) => {
            return Some(ReplayMouseInput::SelectEvent(position));
        }
        ReplayEventsClick::Inert => return None,
        ReplayEventsClick::Uncaptured => {}
    }
    let Event::Mouse(mouse) = &batch.event else {
        return None;
    };
    let mut input = replay_mouse_input_for_event(mouse, hits, show_flags, diff_open)?;
    if let ReplayMouseInput::ScrollEvents { admitted, .. } = &mut input {
        *admitted = batch.events_wheel_admitted;
    }
    Some(input)
}

#[derive(Debug, Default)]
struct ReplayInputBatchState {
    pending: Option<ReplayPendingInput>,
    wheel_run: Option<ReplayWheelRun>,
}

#[derive(Debug)]
struct ReplayPendingInput {
    event: Event,
    events_click: ReplayEventsClick,
}

#[derive(Debug)]
struct ReplayInputBatch {
    event: Event,
    events_wheel_admitted: bool,
    events_click: ReplayEventsClick,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ReplayEventsClick {
    #[default]
    Uncaptured,
    Event(EventPosition),
    Inert,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplayWheelDirection {
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReplayWheelKey {
    direction: ReplayWheelDirection,
    modifiers: KeyModifiers,
    events_pane: Rect,
}

#[derive(Clone, Copy, Debug)]
struct ReplayWheelRun {
    key: ReplayWheelKey,
    last_admitted: Instant,
}

const REPLAY_INPUT_BATCH_REPORTS: usize = 128;
const REPLAY_INPUT_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(2);
const REPLAY_WHEEL_ADMISSION_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

fn events_wheel_key(event: &Event, hits: &ReplayHitMap) -> Option<ReplayWheelKey> {
    let Event::Mouse(mouse) = event else {
        return None;
    };
    let direction = match mouse.kind {
        MouseEventKind::ScrollUp => ReplayWheelDirection::Up,
        MouseEventKind::ScrollDown => ReplayWheelDirection::Down,
        _ => return None,
    };
    hits.events_pane
        .contains(Position::new(mouse.column, mouse.row))
        .then_some(ReplayWheelKey {
            direction,
            modifiers: mouse.modifiers,
            events_pane: hits.events_pane,
        })
}

fn is_mouse_moved(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            ..
        })
    )
}

fn pending_events_click(event: &Event, hits: &ReplayHitMap) -> ReplayEventsClick {
    let Event::Mouse(mouse) = event else {
        return ReplayEventsClick::Uncaptured;
    };
    if mouse.kind != MouseEventKind::Down(MouseButton::Left)
        || mouse.modifiers != KeyModifiers::NONE
    {
        return ReplayEventsClick::Uncaptured;
    }
    let position = Position::new(mouse.column, mouse.row);
    if !hits.events_pane.contains(position) {
        return ReplayEventsClick::Uncaptured;
    }
    hits.events
        .iter()
        .find_map(|(rect, event_position)| rect.contains(position).then_some(*event_position))
        .map_or(ReplayEventsClick::Inert, ReplayEventsClick::Event)
}

fn pending_replay_input(event: Event, hits: &ReplayHitMap) -> ReplayPendingInput {
    let events_click = pending_events_click(&event, hits);
    ReplayPendingInput {
        event,
        events_click,
    }
}

// Keep replay input draining behind its own seam: Events-wheel admission must
// not alter the shared student mouse coalescer.
fn drain_replay_input_batch<E>(
    mut event: Event,
    events_click: ReplayEventsClick,
    state: &mut ReplayInputBatchState,
    hits: &ReplayHitMap,
    mut now: impl FnMut() -> Instant,
    mut read_queued: impl FnMut() -> Result<Option<Event>, E>,
) -> Result<ReplayInputBatch, E> {
    let mut key = events_wheel_key(&event, hits);
    if key.is_none() && !is_mouse_moved(&event) {
        state.wheel_run = None;
        let batch = crate::tui::drain_mouse_event_batch(event, read_queued)?;
        state.pending = batch
            .pending
            .map(|pending| pending_replay_input(pending, hits));
        return Ok(ReplayInputBatch {
            event: batch.event,
            events_wheel_admitted: false,
            events_click,
        });
    };

    let started = now();
    let mut reports = 1;
    while reports < REPLAY_INPUT_BATCH_REPORTS {
        if now().saturating_duration_since(started) >= REPLAY_INPUT_DRAIN_BUDGET {
            break;
        }
        let Some(queued) = read_queued()? else {
            break;
        };
        reports += 1;
        if is_mouse_moved(&queued) {
            continue;
        }
        let queued_key = events_wheel_key(&queued, hits);
        match (key, queued_key) {
            (Some(current), Some(candidate)) if current == candidate => event = queued,
            (None, Some(candidate)) if state.wheel_run.is_some_and(|run| run.key == candidate) => {
                key = Some(candidate);
                event = queued;
            }
            _ => {
                state.pending = Some(pending_replay_input(queued, hits));
                break;
            }
        }
    }

    let Some(key) = key else {
        return Ok(ReplayInputBatch {
            event,
            events_wheel_admitted: false,
            events_click,
        });
    };
    let admitted = match state.wheel_run {
        Some(run) if run.key == key => {
            started.saturating_duration_since(run.last_admitted) >= REPLAY_WHEEL_ADMISSION_INTERVAL
        }
        _ => true,
    };
    if admitted {
        state.wheel_run = Some(ReplayWheelRun {
            key,
            last_admitted: started,
        });
    }
    Ok(ReplayInputBatch {
        event,
        events_wheel_admitted: admitted,
        events_click,
    })
}

fn advance_replay_after_input(
    controller: &mut ReplayController,
    blocks_autoplay: bool,
) -> Result<bool, String> {
    if blocks_autoplay {
        return Ok(false);
    }
    controller.advance_playback(std::time::Duration::ZERO)
}

fn advance_replay_before_wait(
    controller: &mut ReplayController,
    last_tick: &mut Instant,
    now: Instant,
    pending_input: bool,
    queued_input: bool,
) -> Result<bool, String> {
    if pending_input || queued_input {
        return Ok(false);
    }
    let elapsed = now.saturating_duration_since(*last_tick);
    *last_tick = now;
    controller.credit_playback(elapsed);
    advance_replay_after_input(controller, false)
}

fn replay_mouse_input_blocks_autoplay(input: &ReplayMouseInput) -> bool {
    matches!(
        input,
        ReplayMouseInput::SelectEvent(_)
            | ReplayMouseInput::TogglePlay
            | ReplayMouseInput::ScrollEvents { .. }
    )
}

fn replay_key_blocks_autoplay(controller: &ReplayController, key: KeyEvent) -> bool {
    if key.kind == KeyEventKind::Release {
        return false;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('q' | 'Q'))
        || key.modifiers.is_empty() && matches!(key.code, KeyCode::Char('q'))
    {
        return true;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }
    match key.code {
        KeyCode::Char(' ' | 's' | 'S' | 'i' | 'I' | 'f' | 'F' | 'g' | 'G')
        | KeyCode::Enter
        | KeyCode::Left
        | KeyCode::Right => true,
        KeyCode::Up | KeyCode::Down => controller.diff_view().is_none(),
        _ => false,
    }
}

impl DiffPaneState {
    fn open(diff: Option<&super::DiffView>) -> Self {
        let offset = diff
            .and_then(|diff| match &diff.content {
                DiffContent::Lines(lines) => lines
                    .iter()
                    .position(|line| line.kind != DiffLineKind::Context),
                DiffContent::Notice(_) => None,
            })
            .unwrap_or(0);
        Self { offset }
    }

    fn scroll_lines(&mut self, diff: &super::DiffView, lines: isize) {
        let last = match &diff.content {
            DiffContent::Lines(lines) => lines.len().saturating_sub(1),
            DiffContent::Notice(_) => 0,
        };
        self.offset = if lines.is_negative() {
            self.offset.saturating_sub(lines.unsigned_abs())
        } else {
            self.offset.saturating_add(lines as usize).min(last)
        };
    }
}

pub fn run_replay(args: &[String]) -> Result<(), String> {
    let [path] = args else {
        return Err("Usage: rustrace replay PATH".to_owned());
    };
    // Validation and semantic replay complete before terminal state changes or
    // any package content is presented.
    let mut controller = ReplayController::open(Path::new(path))?;
    let loaded = crate::config::load_config();
    let (theme_config, warning) = loaded.map_or_else(
        || (crate::tui::theme::ThemeConfig::default(), None),
        |loaded| (loaded.theme, loaded.warning),
    );
    let mut terminal_session =
        TerminalSession::enter(CrosstermTerminalOperations).map_err(|error| error.to_string())?;
    let colorterm = std::env::var("COLORTERM").ok();
    let theme = terminal_session.resolve_theme(&theme_config, colorterm.as_deref());
    run_loop(&mut controller, path, &theme.palette, warning.as_deref())
}

fn run_loop(
    controller: &mut ReplayController,
    _input: &str,
    palette: &Palette,
    startup_notice: Option<&str>,
) -> Result<(), String> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: TerminalViewport::Fullscreen,
        },
    )
    .map_err(|error| error.to_string())?;
    let mut source_view = build_source_view(controller, None);
    let mut status = startup_notice.map_or_else(|| initial_status(controller), str::to_owned);
    let mut show_flags = false;
    let mut flag_scroll = 0;
    let mut details_scroll = 0;
    let mut last_tick = Instant::now();
    let mut input_batch_state = ReplayInputBatchState::default();
    let mut draw_gate = crate::tui::DrawGate::requested();
    let mut hit_map = ReplayHitMap::default();
    'replay: loop {
        let selected_position = controller.selected_event().map(|event| event.position);
        if source_view.position != selected_position {
            source_view.refresh(controller);
        }
        if draw_gate.take_draw() {
            let size = terminal.size().map_err(|error| error.to_string())?;
            if let ReplayShellLayoutResult::Full(layout) =
                replay_shell_layout(Rect::new(0, 0, size.width, size.height))
            {
                source_view.prepare_for_render(layout.source);
            }
            terminal
                .draw(|frame| {
                    hit_map = ReplayView {
                        controller,
                        editor: &source_view.editor,
                        viewport: &source_view.viewport,
                        highlights: &source_view.highlights,
                        files: &source_view.files,
                        file_index: source_view.file_index,
                        following: source_view.following,
                        recorded_selection: source_view.recorded_selection,
                        diff_offset: source_view.diff_pane.offset,
                        details_offset: details_scroll,
                        status: &status,
                        show_flags,
                        flag_scroll,
                    }
                    .render_with_hit_map_palette(
                        frame.area(),
                        frame.buffer_mut(),
                        palette,
                    );
                })
                .map_err(|error| error.to_string())?;
        }
        details_scroll = details_scroll.min(hit_map.details_max_scroll);

        let pending_input = input_batch_state.pending.is_some();
        let queued_input = if pending_input {
            true
        } else {
            event::poll(std::time::Duration::ZERO).map_err(|error| error.to_string())?
        };
        if advance_replay_before_wait(
            controller,
            &mut last_tick,
            Instant::now(),
            pending_input,
            queued_input,
        )? {
            status = String::from("advanced using recorded segment-local time");
            draw_gate.request_change(true);
            continue 'replay;
        }
        let wait_started = Instant::now();
        let event = {
            let (next, events_click) = if let Some(input) = input_batch_state.pending.take() {
                (input.event, input.events_click)
            } else {
                let remaining =
                    std::time::Duration::from_millis(25).saturating_sub(wait_started.elapsed());
                if !event::poll(remaining).map_err(|error| error.to_string())? {
                    let now = Instant::now();
                    let elapsed = now.saturating_duration_since(last_tick);
                    last_tick = now;
                    controller.credit_playback(elapsed);
                    if advance_replay_after_input(controller, false)? {
                        status = String::from("advanced using recorded segment-local time");
                        draw_gate.request_change(true);
                    }
                    draw_gate.request_timer();
                    continue 'replay;
                }
                (
                    event::read().map_err(|error| error.to_string())?,
                    ReplayEventsClick::Uncaptured,
                )
            };
            drain_replay_input_batch(
                next,
                events_click,
                &mut input_batch_state,
                &hit_map,
                Instant::now,
                || {
                    if event::poll(std::time::Duration::ZERO).map_err(|error| error.to_string())? {
                        event::read().map(Some).map_err(|error| error.to_string())
                    } else {
                        Ok(None)
                    }
                },
            )?
        };
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(last_tick);
        last_tick = now;
        controller.credit_playback(elapsed);
        draw_gate.request_change(!matches!(&event.event, Event::Mouse(_)));
        if matches!(&event.event, Event::Mouse(_)) {
            let diff_open = controller.diff_view().is_some();
            let input = replay_mouse_input_for_batch(&event, &hit_map, show_flags, diff_open);
            let blocks_autoplay = input
                .as_ref()
                .is_some_and(replay_mouse_input_blocks_autoplay)
                || input_batch_state.pending.is_some();
            if let Some(input) = input {
                let result = apply_replay_mouse_input(
                    controller,
                    &mut source_view,
                    &mut show_flags,
                    &mut flag_scroll,
                    &mut details_scroll,
                    &hit_map,
                    input,
                );
                match result {
                    Ok(changed) => draw_gate.request_change(changed),
                    Err(error) => {
                        status = display::label(&error, 2048);
                        draw_gate.request_change(true);
                    }
                }
            }
            if advance_replay_after_input(controller, blocks_autoplay)? {
                status = String::from("advanced using recorded segment-local time");
                draw_gate.request_change(true);
            }
            continue;
        }
        let event = event.event;
        let Event::Key(key) = event else {
            if advance_replay_after_input(controller, input_batch_state.pending.is_some())? {
                status = String::from("advanced using recorded segment-local time");
                draw_gate.request_change(true);
            }
            continue;
        };
        let blocks_autoplay =
            replay_key_blocks_autoplay(controller, key) || input_batch_state.pending.is_some();
        if key.kind == KeyEventKind::Release {
            if advance_replay_after_input(controller, blocks_autoplay)? {
                status = String::from("advanced using recorded segment-local time");
                draw_gate.request_change(true);
            }
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('q' | 'Q'))
            || key.modifiers.is_empty() && matches!(key.code, KeyCode::Char('q'))
        {
            break;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if advance_replay_after_input(controller, blocks_autoplay)? {
                status = String::from("advanced using recorded segment-local time");
                draw_gate.request_change(true);
            }
            continue;
        }
        if matches!(key.code, KeyCode::Char('r' | 'R')) {
            source_view.restore_following(controller, hit_map.source);
            status = String::from("following recorded source and selection");
            draw_gate.request_change(true);
            if advance_replay_after_input(controller, blocks_autoplay)? {
                status = String::from("advanced using recorded segment-local time");
                draw_gate.request_change(true);
            }
            continue;
        }
        if let Some(result) = route_replay_binding(controller, key) {
            match result {
                Ok(Some(message)) => status = message,
                Ok(None) => {}
                Err(error) => status = display::label(&error, 2048),
            }
            continue;
        }
        let diff_scroll = match key.code {
            KeyCode::Up => Some(-1),
            KeyCode::Down => Some(1),
            KeyCode::PageUp => Some(-5),
            KeyCode::PageDown => Some(5),
            _ => None,
        };
        if let (Some(diff), Some(lines)) = (controller.diff_view(), diff_scroll) {
            source_view.diff_pane.scroll_lines(diff, lines);
            if advance_replay_after_input(controller, blocks_autoplay)? {
                status = String::from("advanced using recorded segment-local time");
                draw_gate.request_change(true);
            }
            continue;
        }
        let result = match key.code {
            KeyCode::Char(' ') => {
                controller.toggle_play();
                Ok(())
            }
            KeyCode::Left | KeyCode::Up => controller.previous_event(),
            KeyCode::Right | KeyCode::Down => controller.next_event(),
            KeyCode::PageUp if show_flags => {
                let size = terminal.size().map_err(|error| error.to_string())?;
                let (_, page_rows, max_start) = flags_view_scroll_metrics(
                    controller,
                    size.width as usize,
                    size.height as usize,
                );
                let previous = flag_scroll.min(max_start);
                flag_scroll = previous.saturating_sub(page_rows);
                status = if flag_scroll == previous {
                    String::from("no further content")
                } else {
                    String::from("scrolled validation flags up")
                };
                Ok(())
            }
            KeyCode::PageDown if show_flags => {
                let size = terminal.size().map_err(|error| error.to_string())?;
                let (_, page_rows, max_start) = flags_view_scroll_metrics(
                    controller,
                    size.width as usize,
                    size.height as usize,
                );
                let previous = flag_scroll.min(max_start);
                flag_scroll = previous.saturating_add(page_rows).min(max_start);
                status = if flag_scroll == previous {
                    String::from("no further content")
                } else {
                    String::from("scrolled validation flags down")
                };
                Ok(())
            }
            KeyCode::Tab | KeyCode::Char(']') => {
                if !source_view.files.is_empty() {
                    let next = source_view
                        .files
                        .iter()
                        .position(|path| Some(path.as_str()) == source_view.selected_path())
                        .map_or(0, |index| (index + 1) % source_view.files.len());
                    let path = source_view.files[next].clone();
                    controller.select_diff_file(&path)?;
                    source_view.browse(controller, &path);
                }
                Ok(())
            }
            KeyCode::BackTab | KeyCode::Char('[') => {
                if !source_view.files.is_empty() {
                    let previous = source_view
                        .files
                        .iter()
                        .position(|path| Some(path.as_str()) == source_view.selected_path())
                        .and_then(|index| index.checked_sub(1))
                        .unwrap_or(source_view.files.len() - 1);
                    let path = source_view.files[previous].clone();
                    controller.select_diff_file(&path)?;
                    source_view.browse(controller, &path);
                }
                Ok(())
            }
            KeyCode::Char('d' | 'D') => {
                if let Some(path) = source_view.selected_path().map(str::to_owned) {
                    let was_open = controller.diff_view().is_some();
                    show_flags = false;
                    flag_scroll = 0;
                    controller.toggle_diff(&path)?;
                    source_view.refresh(controller);
                    status = if was_open {
                        String::from("closed comparison; replay state unchanged")
                    } else {
                        String::from("opened bounded comparison; replay state unchanged")
                    };
                } else {
                    status = String::from("no file is available to compare");
                }
                Ok(())
            }
            KeyCode::Char('c' | 'C') => {
                if let Some(path) = source_view.selected_path().map(str::to_owned) {
                    controller.toggle_diff_mode(&path)?;
                    source_view.refresh(controller);
                    status = match controller.comparison_mode() {
                        ComparisonMode::Final => String::from("comparison target: final tree"),
                        ComparisonMode::PreviousCheckpoint => {
                            String::from("comparison target: preceding checkpoint")
                        }
                    };
                }
                Ok(())
            }
            KeyCode::Enter => match controller.follow_selected_evidence()? {
                Some(EvidenceTarget::Event(_)) => {
                    status = String::from("followed source event link");
                    Ok(())
                }
                Some(EvidenceTarget::Artifact(path)) => {
                    status = format!("evidence artifact: {}", display::label(&path, 1024));
                    Ok(())
                }
                None => {
                    status = String::from("selected event has no direct evidence link");
                    Ok(())
                }
            },
            KeyCode::Char('f' | 'F') => {
                match controller.follow_next_review_flag()? {
                    Some(flag) => {
                        let size = terminal.size().map_err(|error| error.to_string())?;
                        let (ranges, _, max_start) = flags_view_scroll_metrics(
                            controller,
                            size.width as usize,
                            size.height as usize,
                        );
                        let current = controller
                            .review_flags()
                            .iter()
                            .position(|candidate| candidate == &flag)
                            .unwrap_or(0);
                        flag_scroll = ranges
                            .get(current)
                            .map_or(0, |range| range.0.min(max_start));
                        status = format!("followed {}", flag.kind.name());
                    }
                    None => status = String::from("no validation flag is present"),
                }
                Ok(())
            }
            KeyCode::Char('v' | 'V') => {
                if !show_flags
                    && let Some(path) = controller.diff_view().map(|diff| diff.path.clone())
                {
                    controller.toggle_diff(&path)?;
                    source_view.refresh(controller);
                }
                show_flags = !show_flags;
                flag_scroll = 0;
                status = if show_flags {
                    String::from("showing validation flags and evidence limitations")
                } else {
                    String::from("showing replay timeline and reconstructed source")
                };
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            status = display::label(&error, 2048);
        }
        if advance_replay_after_input(controller, blocks_autoplay)? {
            status = String::from("advanced using recorded segment-local time");
            draw_gate.request_change(true);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_replay_mouse_input(
    controller: &mut ReplayController,
    source_view: &mut SourceView,
    show_flags: &mut bool,
    flag_scroll: &mut usize,
    details_scroll: &mut usize,
    hits: &ReplayHitMap,
    input: ReplayMouseInput,
) -> Result<bool, String> {
    match input {
        ReplayMouseInput::SelectEvent(position) => {
            let previous_position = controller.selected_event().map(|event| event.position);
            let was_playing = controller.is_playing();
            let previous_details = *details_scroll;
            *details_scroll = 0;
            controller.select(position)?;
            Ok(was_playing
                || previous_details != 0
                || controller.selected_event().map(|event| event.position) != previous_position)
        }
        ReplayMouseInput::SelectFile(index) => {
            let path = source_view
                .files
                .get(index)
                .cloned()
                .ok_or_else(|| "replay file index is out of bounds".to_owned())?;
            controller.select_diff_file(&path)?;
            source_view.browse(controller, &path);
            Ok(true)
        }
        ReplayMouseInput::ShowTab(tab) => {
            select_replay_tab(controller, source_view, show_flags, flag_scroll, tab)?;
            Ok(true)
        }
        ReplayMouseInput::TogglePlay => {
            let was_playing = controller.is_playing();
            controller.toggle_play();
            Ok(controller.is_playing() != was_playing)
        }
        ReplayMouseInput::ScrollEvents { delta, admitted } => {
            let was_playing = controller.is_playing();
            let previous_details = *details_scroll;
            *details_scroll = 0;
            let selected = controller.scroll_events(delta, admitted)?;
            Ok(selected || was_playing || previous_details != 0)
        }
        ReplayMouseInput::ScrollSource(delta) => {
            let was_following = source_view.following;
            source_view.following = false;
            let previous = source_view.viewport.top_line();
            source_view.viewport.scroll_vertical(
                delta,
                source_view.editor.line_count(),
                usize::from(hits.source.height),
            );
            Ok(was_following || source_view.viewport.top_line() != previous)
        }
        ReplayMouseInput::ScrollDetails(delta) => {
            let previous = *details_scroll;
            *details_scroll = details_scroll
                .saturating_add_signed(delta)
                .min(hits.details_max_scroll);
            Ok(*details_scroll != previous)
        }
        ReplayMouseInput::ScrollDiff(delta) => {
            let previous = source_view.diff_pane.offset;
            if let Some(diff) = controller.diff_view() {
                source_view.diff_pane.scroll_lines(diff, delta);
            }
            Ok(source_view.diff_pane.offset != previous)
        }
        ReplayMouseInput::ScrollFlags(delta) => {
            let previous = *flag_scroll;
            let (_, _, maximum) = flags_view_scroll_metrics(
                controller,
                usize::from(hits.area.width),
                usize::from(hits.area.height),
            );
            *flag_scroll = flag_scroll.saturating_add_signed(delta).min(maximum);
            Ok(*flag_scroll != previous)
        }
    }
}

fn select_replay_tab(
    controller: &mut ReplayController,
    source_view: &mut SourceView,
    show_flags: &mut bool,
    flag_scroll: &mut usize,
    tab: ReplayTab,
) -> Result<(), String> {
    let path = source_view.selected_path().map(str::to_owned);
    match tab {
        ReplayTab::Source => {
            if controller.diff_view().is_some() {
                controller.toggle_diff(path.as_deref().unwrap_or(""))?;
            }
            *show_flags = false;
        }
        ReplayTab::Diff => {
            let path = path
                .as_deref()
                .ok_or_else(|| "no file is available to compare".to_owned())?;
            if controller.diff_view().is_none() {
                controller.toggle_diff(path)?;
            }
            *show_flags = false;
        }
        ReplayTab::Flags => {
            if controller.diff_view().is_some() {
                controller.toggle_diff(path.as_deref().unwrap_or(""))?;
            }
            *show_flags = true;
            *flag_scroll = 0;
        }
    }
    source_view.refresh(controller);
    Ok(())
}

fn route_replay_binding(
    controller: &mut ReplayController,
    key: KeyEvent,
) -> Option<Result<Option<String>, String>> {
    match key.code {
        KeyCode::Char('s' | 'S') => {
            controller.toggle_speed();
            Some(Ok(None))
        }
        KeyCode::Char('i' | 'I') => {
            controller.toggle_skip_idle();
            Some(Ok(None))
        }
        KeyCode::Char('g' | 'G') => Some(controller.follow_next_indicator().map(|position| {
            Some(match position {
                Some(position) => format!(
                    "followed review indicator event {}:{}",
                    position.segment + 1,
                    position.sequence
                ),
                None => String::from("no review indicator event link is present"),
            })
        })),
        _ => None,
    }
}

struct SourceView {
    position: Option<EventPosition>,
    editor: EditorBuffer<NoopEditorEffects>,
    viewport: Viewport,
    highlights: Vec<crate::editor::HighlightSpan>,
    files: Vec<String>,
    file_index: usize,
    path: Option<String>,
    following: bool,
    recorded_selection: bool,
    diff_pane: DiffPaneState,
}

impl SourceView {
    fn selected_path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    fn refresh(&mut self, controller: &ReplayController) {
        let previous_path = self.path.clone();
        let previous_viewport = self.viewport;
        let following = self.following;
        let requested_path = (!following).then_some(previous_path.as_deref()).flatten();
        let mut refreshed = build_source_view_with_mode(controller, requested_path, following);
        if refreshed.path == previous_path {
            refreshed.viewport = previous_viewport;
        }
        *self = refreshed;
    }

    fn browse(&mut self, controller: &ReplayController, path: &str) {
        if self.path.as_deref() == Some(path) {
            self.following = false;
            return;
        }
        *self = build_source_view_with_mode(controller, Some(path), false);
    }

    fn restore_following(&mut self, controller: &ReplayController, source_area: Rect) {
        *self = build_source_view_with_mode(controller, None, true);
        self.prepare_for_render(source_area);
    }

    fn prepare_for_render(&mut self, source_area: Rect) {
        let width = usize::from(source_area.width);
        let height = usize::from(source_area.height);
        self.viewport
            .set_top_line(self.viewport.top_line(), self.editor.line_count(), height);
        if self.following && self.recorded_selection {
            self.viewport.follow_cursor(&self.editor, width, height);
        }
    }
}

fn build_source_view(controller: &ReplayController, requested_path: Option<&str>) -> SourceView {
    build_source_view_with_mode(controller, requested_path, requested_path.is_none())
}

fn build_source_view_with_mode(
    controller: &ReplayController,
    requested_path: Option<&str>,
    following: bool,
) -> SourceView {
    let selected = controller.selected_event();
    let files = controller.diff_view().map_or_else(
        || {
            selected
                .map(|selected| {
                    selected
                        .source
                        .iter()
                        .map(|(path, _)| path.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        },
        |diff| diff.files.clone(),
    );
    let preferred_path = if following {
        selected
            .and_then(|selected| selected.active_file.clone())
            .or_else(|| files.first().cloned())
    } else {
        requested_path
            .map(str::to_owned)
            .or_else(|| controller.diff_view().map(|diff| diff.path.clone()))
            .or_else(|| files.first().cloned())
    };
    let file_index = preferred_path
        .as_deref()
        .and_then(|requested| files.iter().position(|path| path == requested))
        .or_else(|| {
            controller
                .diff_view()
                .and_then(|diff| files.iter().position(|path| path == &diff.path))
        })
        .or_else(|| {
            selected
                .and_then(|selected| selected.active_file.as_ref())
                .and_then(|active| files.iter().position(|path| path == active))
        })
        .unwrap_or(files.len());
    let path = preferred_path
        .as_deref()
        .unwrap_or("(no reconstructed source)");
    let bytes = selected.and_then(|selected| {
        selected
            .source
            .iter()
            .find(|(candidate, _)| candidate == path)
            .map(|(_, bytes)| bytes.as_slice())
    });
    let text_source = bytes.and_then(|bytes| std::str::from_utf8(bytes).ok());
    let text = bytes.map_or_else(
        || String::from("(file absent at current replay position)"),
        |bytes| {
            text_source
                .map(str::to_owned)
                .unwrap_or_else(|| visible_binary(bytes))
        },
    );
    let mut editor = EditorBuffer::new(
        DocumentId::new("replay-read-only").expect("fixed document ID"),
        &text,
        NoopEditorEffects,
    );
    let recorded_selection = selected
        .and_then(|selected| {
            let active = selected.active_document.as_ref()?;
            (selected.active_file.as_deref() == Some(active.path.as_str())
                && active.path == path
                && text_source.is_some())
            .then_some(active.selection)
        })
        .is_some_and(|selection| editor.set_selection(selection).is_ok());
    let highlights = highlight_path(Path::new(path), &text).unwrap_or_default();
    SourceView {
        position: selected.map(|event| event.position),
        editor,
        viewport: Viewport::default(),
        highlights,
        files,
        file_index,
        path: preferred_path,
        following,
        recorded_selection,
        diff_pane: DiffPaneState::open(controller.diff_view()),
    }
}

fn visible_binary(bytes: &[u8]) -> String {
    let rendered = display::plain(bytes, display::Limits::default());
    rendered
        .text
        .lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct ReplayView<'a> {
    controller: &'a ReplayController,
    editor: &'a EditorBuffer<NoopEditorEffects>,
    viewport: &'a Viewport,
    highlights: &'a [crate::editor::HighlightSpan],
    files: &'a [String],
    file_index: usize,
    following: bool,
    recorded_selection: bool,
    diff_offset: usize,
    details_offset: usize,
    status: &'a str,
    show_flags: bool,
    flag_scroll: usize,
}

impl Widget for ReplayView<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let _ = self.render_with_hit_map(area, buffer);
    }
}

impl ReplayView<'_> {
    fn render_with_hit_map(self, area: Rect, buffer: &mut Buffer) -> ReplayHitMap {
        let palette = Palette::selected();
        self.render_with_hit_map_palette(area, buffer, &palette)
    }

    fn render_with_hit_map_palette(
        self,
        area: Rect,
        buffer: &mut Buffer,
        palette: &Palette,
    ) -> ReplayHitMap {
        Clear.render(area, buffer);
        let mut hits = ReplayHitMap {
            area,
            ..ReplayHitMap::default()
        };
        let ReplayShellLayoutResult::Full(layout) = replay_shell_layout(area) else {
            Paragraph::new(format!(
                "Replay needs {}x{}; got {}x{}",
                crate::tui::shell::REPLAY_MIN_WIDTH,
                crate::tui::shell::REPLAY_MIN_HEIGHT,
                area.width,
                area.height
            ))
            .style(Style::new().fg(palette.yellow).add_modifier(Modifier::BOLD))
            .render(area, buffer);
            return hits;
        };
        hits.events_pane = layout.events;
        hits.source = layout.source;
        hits.details = layout.details;
        self.render_sidebar(layout, buffer, palette, &mut hits);
        self.render_tabs(layout.tab_bar, buffer, palette, &mut hits);
        self.render_timing(layout.gap, buffer, palette);
        if self.show_flags && self.controller.diff_view().is_none() {
            self.render_flags_view(layout.source, buffer);
        } else if self.controller.diff_view().is_some() {
            self.render_source(layout.source, buffer, palette);
        } else if !self.controller.review_flags().is_empty()
            && self.controller.artifact_preview().is_some()
        {
            self.render_artifact_preview(layout.source, buffer, palette);
        } else {
            self.render_source(layout.source, buffer, palette);
        }
        hits.details_max_scroll = self.render_details(layout.details, buffer, palette);
        self.render_mode_bar(layout.mode_bar, buffer, palette, &mut hits);
        hits
    }
}

/// The status line shown before the first key press.
fn initial_status(controller: &ReplayController) -> String {
    match evidence_outcome(controller.verification_report()) {
        EvidenceOutcome::Consistent => String::from("validation complete; read-only replay"),
        EvidenceOutcome::NotEvaluated => String::from("validation incomplete; read-only replay"),
        EvidenceOutcome::Failed if controller.timeline_available() => {
            String::from("validation failed; reviewable timeline retained")
        }
        EvidenceOutcome::Failed => String::from("validation failed; timeline unavailable"),
    }
}

impl ReplayView<'_> {
    fn render_sidebar(
        &self,
        layout: ReplayShellLayout,
        buffer: &mut Buffer,
        palette: &Palette,
        hits: &mut ReplayHitMap,
    ) {
        set_style(
            buffer,
            layout.sidebar,
            Style::default().bg(palette.sidebar_bg),
        );
        for y in layout.sidebar_divider.y..layout.sidebar_divider.bottom() {
            put_text(
                buffer,
                layout.sidebar_divider.x,
                y,
                1,
                "│",
                Style::default().fg(palette.surface_dim),
            );
        }
        self.render_files(layout.files, buffer, palette, hits);
        self.render_events(layout.events, buffer, palette, hits);
    }

    fn render_tabs(
        &self,
        area: Rect,
        buffer: &mut Buffer,
        palette: &Palette,
        hits: &mut ReplayHitMap,
    ) {
        let panel = Style::default().fg(palette.overlay1).bg(palette.panel_bg);
        set_style(buffer, area, panel);
        let active = if self.controller.diff_view().is_some() {
            1
        } else if self.show_flags {
            2
        } else {
            0
        };
        let mut x = area.x;
        for (index, label) in [" source ", " diff ", " flags "].into_iter().enumerate() {
            let width = display_width(label) as u16;
            let rect = Rect::new(x, area.y, width.min(area.right().saturating_sub(x)), 1);
            if rect.is_empty() {
                break;
            }
            let style = if index == active {
                Style::default()
                    .fg(palette.tab_active_fg)
                    .bg(palette.tab_active_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(palette.overlay1)
                    .bg(palette.surface0)
                    .add_modifier(Modifier::DIM)
            };
            set_style(buffer, rect, style);
            put_text(buffer, rect.x, rect.y, rect.width, label, style);
            hits.tabs.push((
                rect,
                match index {
                    0 => ReplayTab::Source,
                    1 => ReplayTab::Diff,
                    _ => ReplayTab::Flags,
                },
            ));
            x = rect.right().saturating_add(1);
        }
        let speed = match self.controller.speed() {
            PlaybackSpeed::Normal => "1x",
            PlaybackSpeed::Accelerated => "4x",
        };
        let state = format!(
            "{} {speed} follow:{}",
            if self.controller.is_playing() {
                "PLAY"
            } else {
                "PAUSE"
            },
            if self.following { "on" } else { "off" }
        );
        put_right_text(buffer, area, area.y, &state, panel);
    }

    fn render_timing(&self, area: Rect, buffer: &mut Buffer, palette: &Palette) {
        let text = self.controller.selected_event().map_or_else(
            || String::from("no selected event"),
            |selected| match self.controller.timing {
                super::TimingAvailability::Recorded => {
                    let idle = if self.controller.skip_idle() {
                        "skip"
                    } else {
                        "recorded"
                    };
                    format!(
                        "A{} t={} idle:{idle}",
                        selected.position.segment + 1,
                        format_recorded_millis(selected.millis)
                    )
                }
                super::TimingAvailability::UnavailableSynthetic => {
                    format!("A{} t=unavailable", selected.position.segment + 1)
                }
            },
        );
        put_text(
            buffer,
            area.x,
            area.y,
            area.width,
            &text,
            Style::default().fg(palette.overlay0).bg(palette.panel_bg),
        );
    }

    fn render_files(
        &self,
        area: Rect,
        buffer: &mut Buffer,
        palette: &Palette,
        hits: &mut ReplayHitMap,
    ) {
        put_text(
            buffer,
            area.x,
            area.y,
            area.width,
            " files",
            Style::default()
                .fg(palette.overlay0)
                .add_modifier(Modifier::BOLD),
        );
        let content = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        );
        let visible = usize::from(content.height);
        let start = self.file_index.saturating_sub(visible.saturating_sub(1));
        for (row, (index, path)) in self
            .files
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .enumerate()
        {
            let selected = index == self.file_index;
            let row = Rect::new(content.x, content.y + row as u16, content.width, 1);
            let style = if selected {
                Style::default()
                    .fg(palette.text)
                    .bg(palette.active_row_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.subtext0)
            };
            if selected {
                set_style(buffer, row, style);
            }
            put_text(
                buffer,
                row.x,
                row.y,
                row.width,
                &format!(" {} {}", if selected { '●' } else { ' ' }, path),
                style,
            );
            hits.files.push((row, index));
        }
    }

    fn render_events(
        &self,
        area: Rect,
        buffer: &mut Buffer,
        palette: &Palette,
        hits: &mut ReplayHitMap,
    ) {
        put_text(
            buffer,
            area.x,
            area.y,
            area.width,
            " events",
            Style::default()
                .fg(palette.overlay0)
                .add_modifier(Modifier::BOLD),
        );
        let inner = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        );
        let visible_rows = inner.height as usize;
        if visible_rows == 0 {
            return;
        }
        let mut lines = Vec::new();
        let mut selected_line = None;
        for row in self.controller.timeline_rows(visible_rows) {
            if row.attempt_boundary {
                lines.push((
                    Line::from(format!(
                        "--- attempt {} | gap unknown ---",
                        row.position.segment + 1
                    )),
                    None,
                ));
            }
            if row.selected {
                selected_line = Some(lines.len());
            }
            lines.push((
                Line::from(format!(
                    "{} {}:{} {}",
                    if row.selected { '>' } else { ' ' },
                    row.position.segment + 1,
                    row.position.sequence,
                    row.event_name
                )),
                Some(row.position),
            ));
        }
        let max_start = lines.len().saturating_sub(visible_rows);
        let start = selected_line
            .unwrap_or(0)
            .saturating_sub(visible_rows / 2)
            .min(max_start);
        for (index, (line, position)) in
            lines.into_iter().skip(start).take(visible_rows).enumerate()
        {
            let row = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
            let selected = line.to_string().starts_with('>');
            let boundary = line.to_string().starts_with("---");
            let style = if selected {
                Style::default()
                    .fg(palette.text)
                    .bg(palette.active_row_bg)
                    .add_modifier(Modifier::BOLD)
            } else if boundary {
                Style::default().fg(palette.overlay0)
            } else {
                Style::default().fg(palette.subtext0)
            };
            if selected {
                set_style(buffer, row, style);
            }
            let text = if boundary {
                line.to_string().replace("---", "─")
            } else {
                line.to_string()
            };
            put_text(buffer, row.x, row.y, row.width, &text, style);
            if let Some(position) = position {
                hits.events.push((row, position));
            }
        }
    }

    fn render_source(&self, area: Rect, buffer: &mut Buffer, palette: &Palette) {
        if let Some(diff) = self.controller.diff_view() {
            self.render_diff(diff, area, buffer, palette);
            return;
        }
        EditorWidget::new(self.editor, self.viewport, self.highlights)
            .with_palette(palette)
            .with_cursor_visible(self.recorded_selection)
            .render(area, buffer);
    }

    fn render_artifact_preview(&self, area: Rect, buffer: &mut Buffer, palette: &Palette) {
        let Some((path, bytes)) = self.controller.artifact_preview() else {
            self.render_source(area, buffer, palette);
            return;
        };
        put_text(
            buffer,
            area.x,
            area.y,
            area.width,
            &format!(" evidence artifact · {}", display::label(path, 2048)),
            Style::default()
                .fg(palette.overlay0)
                .add_modifier(Modifier::BOLD),
        );
        let inner = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        );
        let rendered = display::plain(
            bytes,
            display::Limits {
                lines: inner.height as usize,
                ..display::Limits::default()
            },
        );
        Paragraph::new(rendered.text).render(inner, buffer);
    }

    fn render_flags_view(&self, area: Rect, buffer: &mut Buffer) {
        let limitation_lines = evidence_limitation_lines(self.controller, area.width as usize);
        let [flag_region, limitations] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(limitation_lines.len().min(usize::from(area.height)) as u16),
        ])
        .areas(area);
        let (lines, _) = wrapped_flag_lines(self.controller, flag_region.width as usize);
        Paragraph::new(limitation_lines).render(limitations, buffer);

        let visible = flag_region.height as usize;
        if lines.is_empty() {
            Paragraph::new("No validation flags.").render(flag_region, buffer);
            return;
        }
        if lines.len() <= visible {
            Paragraph::new(lines).render(flag_region, buffer);
            return;
        }

        let content_rows = visible.saturating_sub(1);
        if content_rows == 0 {
            return;
        }
        let total_lines = lines.len();
        let max_start = total_lines.saturating_sub(content_rows);
        let start = self.flag_scroll.min(max_start);
        let end = start.saturating_add(content_rows).min(lines.len());
        let mut visible_lines = lines
            .into_iter()
            .skip(start)
            .take(content_rows)
            .collect::<Vec<_>>();
        visible_lines.push(Line::from(format!(
            "lines {}-{} of {}",
            start + 1,
            end,
            total_lines
        )));
        Paragraph::new(visible_lines).render(flag_region, buffer);
    }

    fn render_diff(
        &self,
        diff: &super::DiffView,
        area: Rect,
        buffer: &mut Buffer,
        palette: &Palette,
    ) {
        let comparison = diff.comparison.map_or_else(
            || String::from("unavailable"),
            |position| {
                format!(
                    "segment {} sequence {}",
                    position.segment + 1,
                    position.sequence
                )
            },
        );
        let direction = match diff.mode {
            ComparisonMode::Final => format!("current -> final {comparison}"),
            ComparisonMode::PreviousCheckpoint => {
                format!("checkpoint {comparison} -> current")
            }
        };
        let mut lines = vec![Line::from(display::label(&direction, 4096))];
        match &diff.content {
            DiffContent::Lines(changes) => {
                lines.push(Line::from(format!(
                    "+{} -{}",
                    diff.insertions, diff.deletions
                )));
                lines.extend(
                    changes
                        .iter()
                        .skip(self.diff_offset)
                        .take(area.height.saturating_sub(lines.len() as u16) as usize)
                        .map(|line| render_diff_line(line, palette)),
                );
            }
            DiffContent::Notice(notice) => lines.push(Line::from(notice.text())),
        }
        Paragraph::new(lines).render(area, buffer);
    }

    fn render_details(&self, area: Rect, buffer: &mut Buffer, palette: &Palette) -> usize {
        let header_style = Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::BOLD);
        let status_width = if self.status.is_empty() {
            0
        } else {
            (display_width(self.status) as u16).min(area.width.saturating_sub(12))
        };
        let header_width = area
            .width
            .saturating_sub(status_width + u16::from(status_width > 0));
        put_text(
            buffer,
            area.x,
            area.y,
            header_width,
            " validation · selected event · recorded output",
            header_style,
        );
        if status_width > 0 {
            put_right_text(
                buffer,
                Rect::new(area.right() - status_width, area.y, status_width, 1),
                area.y,
                self.status,
                header_style,
            );
        }
        let inner = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        );
        let report = self.controller.verification_report();
        let validation = format!(
            "Verify package:{} chain:{} checkpoints:{} replay:{}",
            verification(report.package_structure),
            verification(report.event_chain),
            verification(report.checkpoint_hashes),
            verification(report.replay),
        );
        let mut lines = Vec::new();
        push_wrapped_lines(
            &mut lines,
            &display::label(&validation, 4096),
            usize::from(inner.width),
        );
        let source_summary = format!(
            "Source:{} assignment:{} | external:{} unknown-origin:{}",
            submitted(report.submitted_source_match),
            assignment(report.assignment_reference),
            report
                .external_changes
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
            report
                .unknown_edit_origins
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
        );
        push_wrapped_lines(&mut lines, &source_summary, usize::from(inner.width));
        let counts = self.controller.counts();
        let selected_attempt = self
            .controller
            .selected_event()
            .map(|selected| selected.position.segment as u32 + 1);
        let process = report
            .review_indicators
            .as_ref()
            .and_then(|indicators| {
                selected_attempt.and_then(|attempt| {
                    indicators
                        .attempts
                        .iter()
                        .find(|result| result.attempt == attempt)
                })
            })
            .map_or_else(
                || String::from("unavailable"),
                |attempt| {
                    let outcome = match &attempt.outcome {
                        ProcessRuleOutcome::Suggested => "suggested",
                        ProcessRuleOutcome::NotEligible { .. } => "not eligible",
                        ProcessRuleOutcome::Unavailable { .. } => "unavailable",
                    };
                    format!(
                        "{outcome} I:{} N:{} A:{} R:{} J:{} B:{} O:{} F:{}",
                        attempt.values.inserted_keyboard_scalars,
                        attempt.values.keyboard_transactions,
                        attempt.values.appended_keyboard_scalars,
                        attempt.values.removed_keyboard_scalars,
                        attempt.values.forward_inserted_scalars,
                        attempt.values.before_first_build_scalars,
                        attempt.values.feedback_opportunities,
                        attempt.values.error_edit_rebuild_sequences,
                    )
                },
            );
        push_wrapped_lines(
            &mut lines,
            &format!("Process {process}"),
            usize::from(inner.width),
        );
        let paste_summary = format!(
            "Paste: historical {} ({} chars, {} lines) | internal {} | blocked attempts {}",
            counts.historical_origin_unverified_pastes,
            counts.historical_paste_characters,
            counts.historical_paste_lines,
            counts.allowed_internal_pastes,
            counts.blocked_paste_attempts,
        );
        push_wrapped_lines(&mut lines, &paste_summary, usize::from(inner.width));
        let flags = self.controller.review_flags();
        if flags.is_empty()
            && let Some((path, bytes)) = self.controller.artifact_preview()
        {
            lines.push(Line::from(format!(
                "Evidence artifact: {}",
                display::label(path, 2048)
            )));
            let rendered = display::plain(
                bytes,
                display::Limits {
                    lines: display::MAX_LINES.saturating_sub(lines.len()),
                    ..display::Limits::default()
                },
            );
            lines.extend(rendered.text.lines);
            return render_details_lines(lines, self.details_offset, inner, buffer);
        }
        if let Some(selected) = self.controller.selected_event() {
            let marker = match &selected.paste_marker {
                Some(PasteMarker::HistoricalOriginUnverified { characters, lines }) => {
                    format!(
                        " | historical origin-unverified paste: {characters} chars, {lines} lines"
                    )
                }
                Some(PasteMarker::AllowedInternal { source }) => format!(
                    " | allowed internal paste; source {}:{}",
                    source.segment + 1,
                    source.sequence
                ),
                Some(PasteMarker::BlockedMetadataOnly) => {
                    String::from(" | blocked paste attempt; metadata only")
                }
                None => String::new(),
            };
            push_wrapped_lines(
                &mut lines,
                &format!(
                    "Attempt {} event {}{}{}",
                    selected.position.segment + 1,
                    selected.position.sequence,
                    if selected.inter_attempt_time_unknown {
                        " | inter-attempt time unknown"
                    } else {
                        ""
                    },
                    marker
                ),
                usize::from(inner.width),
            );
            if flags.is_empty() {
                if let Some(comparison) = &selected.test_case_comparison {
                    push_comparison_details(&mut lines, comparison, usize::from(inner.width));
                }
                let bytes = if selected.command_output.is_empty() {
                    selected.event_bytes.as_slice()
                } else {
                    selected.command_output.as_slice()
                };
                let remaining = display::MAX_LINES.saturating_sub(lines.len());
                let rendered = display::output(
                    bytes,
                    display::Limits {
                        lines: remaining,
                        ..display::Limits::default()
                    },
                );
                lines.extend(rendered.text.lines);
            } else {
                lines.push(Line::from(format!(
                    "Flags: {} (F next, V view)",
                    flags.len()
                )));
            }
        } else if flags.is_empty() {
            for issue in report
                .issues
                .iter()
                .take(display::MAX_LINES.saturating_sub(lines.len()))
            {
                lines.push(Line::from(display::label(&issue.detail, 4096)));
            }
        } else {
            lines.push(Line::from("Selected event: unavailable"));
            lines.push(Line::from(format!(
                "Flags: {} (F next, V view)",
                flags.len()
            )));
        }
        render_details_lines(lines, self.details_offset, inner, buffer)
    }

    fn render_mode_bar(
        &self,
        area: Rect,
        buffer: &mut Buffer,
        palette: &Palette,
        hits: &mut ReplayHitMap,
    ) {
        let base = Style::default().fg(palette.overlay1).bg(palette.panel_bg);
        set_style(buffer, area, base);
        let label = if self.controller.is_playing() {
            " PLAY "
        } else {
            " PAUSE "
        };
        let pill = Style::default()
            .fg(palette.panel_contrast_fg())
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD);
        hits.play_pause = Rect::new(
            area.x,
            area.y,
            (display_width(label) as u16).min(area.width),
            area.height,
        );
        put_text(
            buffer,
            hits.play_pause.x,
            hits.play_pause.y,
            hits.play_pause.width,
            label,
            pill,
        );
        let x = area.x + display_width(label) as u16;
        let hints = if area.width >= 90 {
            " space play  ←→ step  s speed  r follow  i idle  Enter evidence  f flag  g link  q quit"
        } else {
            " space play s speed r follow Enter evidence q"
        };
        put_text(
            buffer,
            x,
            area.y,
            area.right().saturating_sub(x),
            hints,
            base,
        );
    }
}

fn format_recorded_millis(millis: u64) -> String {
    let seconds = millis / 1_000;
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    let millis = millis % 1_000;
    format!("{hours}:{minutes:02}:{seconds:02}.{millis:03}")
}

fn push_comparison_details(
    lines: &mut Vec<Line<'static>>,
    comparison: &rustrace_model::TestCaseCompared,
    width: usize,
) {
    use rustrace_model::TestCaseComparisonOutcome;

    let (label, detail) = match &comparison.outcome {
        TestCaseComparisonOutcome::Pass => ("pass", None),
        TestCaseComparisonOutcome::Mismatch {
            line,
            expected_len,
            actual_len,
        } => (
            "mismatch",
            Some(format!(
                "line {line}; expected {expected_len} bytes, actual {actual_len} bytes"
            )),
        ),
        TestCaseComparisonOutcome::Error { reason } => {
            ("error", Some(comparison_error_label(*reason).to_owned()))
        }
    };
    push_wrapped_lines(
        lines,
        &format!(
            "Test case {}: {label}",
            display::label(&comparison.case, rustrace_model::MAX_TEST_CASE_NAME_BYTES)
        ),
        width,
    );
    if let Some(detail) = detail {
        push_wrapped_lines(lines, &detail, width);
    }
    push_wrapped_lines(
        lines,
        &format!("Expected BLAKE3: {}", comparison.expected_blake3),
        width,
    );
    push_wrapped_lines(
        lines,
        &format!(
            "Actual BLAKE3: {}",
            comparison
                .actual_blake3
                .map_or_else(|| "unavailable".to_owned(), |hash| hash.to_string())
        ),
        width,
    );
}

fn comparison_error_label(reason: rustrace_model::TestCaseComparisonError) -> &'static str {
    use rustrace_model::TestCaseComparisonError;

    match reason {
        TestCaseComparisonError::LaunchFailed => "launch failed",
        TestCaseComparisonError::NonzeroExit => "nonzero exit",
        TestCaseComparisonError::Terminated => "terminated",
        TestCaseComparisonError::CaptureTruncated => "capture truncated",
        TestCaseComparisonError::CaptureUnavailable => "capture unavailable",
        TestCaseComparisonError::CaptureReadFailed => "capture read failed",
        TestCaseComparisonError::ExpectedUnreadable => "expected unreadable",
        TestCaseComparisonError::ExpectedOversized => "expected oversized",
    }
}

fn render_details_lines(
    lines: Vec<Line<'static>>,
    offset: usize,
    area: Rect,
    buffer: &mut Buffer,
) -> usize {
    let maximum = lines.len().saturating_sub(usize::from(area.height));
    Paragraph::new(
        lines
            .into_iter()
            .skip(offset.min(maximum))
            .take(usize::from(area.height))
            .collect::<Vec<_>>(),
    )
    .render(area, buffer);
    maximum
}

fn wrapped_flag_lines(
    controller: &ReplayController,
    width: usize,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    let flags = controller.review_flags();
    let mut lines = Vec::new();
    let mut ranges = Vec::with_capacity(flags.len());
    if !controller.advisories().is_empty() {
        push_wrapped_lines(
            &mut lines,
            if flags.is_empty() {
                "Hard review flags: none"
            } else {
                "Hard review flags:"
            },
            width,
        );
    }
    for flag in &flags {
        let start = lines.len();
        push_wrapped_lines(&mut lines, &display_flag(flag), width);
        ranges.push((start, lines.len()));
    }
    if !controller.advisories().is_empty() {
        push_wrapped_lines(&mut lines, "Advisory flags:", width);
        for advisory in controller.advisories() {
            push_wrapped_lines(&mut lines, &display_advisory(advisory), width);
        }
    }
    if let Some(indicators) = &controller.verification_report().review_indicators {
        push_wrapped_lines(&mut lines, "Factual review indicators:", width);
        for factual in display_factual_indicators(&indicators.factual) {
            push_wrapped_lines(&mut lines, &factual, width);
        }
        for attempt in &indicators.attempts {
            push_wrapped_lines(&mut lines, &display_process_attempt_details(attempt), width);
        }
        push_wrapped_lines(&mut lines, INDICATOR_EXPLANATION, width);
    }
    (lines, ranges)
}

fn evidence_limitation_lines(controller: &ReplayController, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for statement in [
        evidence_statement(controller.verification_report()),
        EVIDENCE_LIMITATION_FIRST,
        EVIDENCE_LIMITATION_SECOND,
    ] {
        push_wrapped_lines(&mut lines, statement, width);
    }
    lines
}

fn flags_view_scroll_metrics(
    controller: &ReplayController,
    width: usize,
    height: usize,
) -> (Vec<(usize, usize)>, usize, usize) {
    let area = Rect::new(
        0,
        0,
        width.min(u16::MAX as usize) as u16,
        height.min(u16::MAX as usize) as u16,
    );
    let (content_width, flag_region_rows) = match replay_shell_layout(area) {
        ReplayShellLayoutResult::Full(layout) => {
            let content_width = usize::from(layout.source.width);
            let limitations = evidence_limitation_lines(controller, content_width).len();
            (
                content_width,
                usize::from(layout.source.height).saturating_sub(limitations),
            )
        }
        ReplayShellLayoutResult::TooSmall(_) => (width, 0),
    };
    let (lines, ranges) = wrapped_flag_lines(controller, content_width);
    let content_rows = if lines.len() > flag_region_rows {
        flag_region_rows.saturating_sub(1)
    } else {
        flag_region_rows
    };
    let max_start = lines.len().saturating_sub(content_rows);
    (ranges, content_rows.max(1), max_start)
}

fn push_wrapped_lines(lines: &mut Vec<Line<'static>>, value: &str, width: usize) {
    if width == 0 {
        return;
    }
    let mut remaining = value;
    while !remaining.is_empty() {
        let mut columns = 0;
        let mut fit_end = 0;
        let mut last_space = None;
        for (offset, grapheme) in remaining.grapheme_indices(true) {
            let grapheme_width = display::grapheme_width(grapheme, columns);
            if columns > 0 && columns.saturating_add(grapheme_width) > width {
                break;
            }
            columns = columns.saturating_add(grapheme_width);
            fit_end = offset + grapheme.len();
            if grapheme == " " {
                last_space = Some(offset);
            }
        }
        if fit_end == remaining.len() {
            lines.push(Line::from(remaining.to_owned()));
            return;
        }
        let split = last_space.filter(|offset| *offset > 0).unwrap_or(fit_end);
        lines.push(Line::from(remaining[..split].to_owned()));
        remaining = &remaining[split..];
    }
}

fn render_diff_line(line: &DiffLine, palette: &Palette) -> Line<'static> {
    let (marker, style) = match line.kind {
        DiffLineKind::Context => ("  ", Style::default()),
        DiffLineKind::Insertion => ("+ ", Style::new().fg(palette.green)),
        DiffLineKind::Deletion => ("- ", Style::new().fg(palette.red)),
    };
    let mut spans = vec![
        Span::styled(marker, style),
        Span::styled(display::label(&line.text, display::MAX_OUTPUT_BYTES), style),
    ];
    if line.unterminated {
        spans.push(Span::styled(NO_NEWLINE_AT_END_MARKER, style));
    }
    Line::from(spans)
}

fn verification(status: VerificationStatus) -> &'static str {
    match status {
        VerificationStatus::Ok => "OK",
        VerificationStatus::Failed => "FAILED",
        VerificationStatus::Unavailable => "unavailable",
    }
}

fn submitted(status: SubmittedSourceStatus) -> &'static str {
    match status {
        SubmittedSourceStatus::Ok => "OK",
        SubmittedSourceStatus::SourceMismatch => "MISMATCH",
        SubmittedSourceStatus::Unavailable => "unavailable",
    }
}

fn assignment(status: AssignmentReferenceStatus) -> &'static str {
    match status {
        AssignmentReferenceStatus::Ok => "OK",
        AssignmentReferenceStatus::Mismatch => "MISMATCH",
        AssignmentReferenceStatus::Unverified => "unverified",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review_flags::{EVIDENCE_CONSISTENCY, EVIDENCE_FAILURE};
    use crate::session::{ProductionSession, create_bundle};
    use crate::tui::EditorCommand;
    use crate::verify::{AssignmentReferenceStatus, SubmittedSourceStatus, VerificationStatus};
    use crate::verify::{VerificationIssue, VerificationIssueKind, VerificationIssueLocation};
    use rustrace_model::{
        Event as RecordedEvent, EventEnvelope, Hash, PasteInputChannel, PasteRejected,
        PasteRejectionReason, RPROV_CONTAINER_HEADER_BYTES, RPROV_RECORD_HEADER_BYTES,
        SelectionChanged, SelectionState, SessionId, decode_rprov_record_header,
    };
    use std::{
        collections::VecDeque,
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, Instant},
    };

    const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Replay render"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["**"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

    struct ProductionRenderFixture {
        base: PathBuf,
    }

    impl ProductionRenderFixture {
        fn controller(name: &str) -> (Self, ReplayController) {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let base = std::env::temp_dir().join(format!(
                "rustrace-replay-render-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let workspace = base.join("workspace");
            fs::create_dir_all(&workspace).unwrap();
            fs::write(workspace.join("main.rs"), "fn main() {}\n").unwrap();
            let mut session = ProductionSession::start(&workspace, MANIFEST).unwrap();
            session.execute(EditorCommand::Insert('x')).unwrap();
            let receipt = session.finalize("student-1").unwrap();
            let bundle = create_bundle(&receipt, &base.join("submission.zip"))
                .unwrap()
                .path;
            let controller = ReplayController::open(&bundle).unwrap();
            (Self { base }, controller)
        }

        fn corrupt_evidence_controller(
            name: &str,
            evidence_count: usize,
        ) -> (Self, ReplayController) {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let base = std::env::temp_dir().join(format!(
                "rustrace-replay-render-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let workspace = base.join("workspace");
            fs::create_dir_all(&workspace).unwrap();
            fs::write(workspace.join("main.rs"), "initial").unwrap();
            let mut session = ProductionSession::start(&workspace, MANIFEST).unwrap();
            for index in 0..evidence_count {
                fs::write(workspace.join("main.rs"), format!("external-{index}")).unwrap();
                session.save_all().unwrap();
            }
            let receipt = session.finalize("student-1").unwrap();
            let bundle = create_bundle(&receipt, &base.join("submission.zip"))
                .unwrap()
                .path;
            let archive = fs::read(bundle).unwrap();
            let rprov = stored_zip_entry(&archive, "session.rprov");
            let corrupt = corrupt_evidence_payloads(rprov, evidence_count);
            let path = base.join("corrupt-evidence.rprov");
            fs::write(&path, corrupt).unwrap();
            let controller = ReplayController::open(&path).unwrap();
            assert_eq!(controller.review_flags().len(), evidence_count);
            (Self { base }, controller)
        }

        fn final_only_controller(name: &str) -> (Self, ReplayController) {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let base = std::env::temp_dir().join(format!(
                "rustrace-replay-render-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let workspace = base.join("workspace");
            fs::create_dir_all(&workspace).unwrap();
            fs::write(workspace.join("main.rs"), "initial\n").unwrap();
            let mut session = ProductionSession::start(&workspace, MANIFEST).unwrap();
            session.capture_boundary().unwrap();
            session.create_file("final.rs").unwrap();
            session.execute(EditorCommand::Insert('x')).unwrap();
            let receipt = session.finalize("student-1").unwrap();
            let bundle = create_bundle(&receipt, &base.join("submission.zip"))
                .unwrap()
                .path;
            let mut controller = ReplayController::open(&bundle).unwrap();
            let current = controller
                .timeline_rows(controller.event_count())
                .into_iter()
                .filter(|row| row.event_name == "checkpoint")
                .nth(1)
                .map(|row| row.position)
                .expect("missing intermediate checkpoint");
            controller.select(current).unwrap();
            (Self { base }, controller)
        }
    }

    fn stored_zip_entry(archive: &[u8], wanted: &str) -> Vec<u8> {
        let mut offset = 0;
        while archive.get(offset..offset + 4) == Some(&0x0403_4b50_u32.to_le_bytes()) {
            let compressed =
                u32::from_le_bytes(archive[offset + 18..offset + 22].try_into().unwrap()) as usize;
            let name_len =
                u16::from_le_bytes(archive[offset + 26..offset + 28].try_into().unwrap()) as usize;
            let extra_len =
                u16::from_le_bytes(archive[offset + 28..offset + 30].try_into().unwrap()) as usize;
            let name_start = offset + 30;
            let data_start = name_start + name_len + extra_len;
            let name = std::str::from_utf8(&archive[name_start..name_start + name_len]).unwrap();
            if name == wanted {
                return archive[data_start..data_start + compressed].to_vec();
            }
            offset = data_start + compressed;
        }
        panic!("missing stored ZIP entry {wanted}");
    }

    fn corrupt_evidence_payloads(mut rprov: Vec<u8>, wanted: usize) -> Vec<u8> {
        let mut offset = RPROV_CONTAINER_HEADER_BYTES;
        let mut corrupted = 0;
        while offset < rprov.len() {
            let header =
                decode_rprov_record_header(&rprov[offset..offset + RPROV_RECORD_HEADER_BYTES])
                    .unwrap();
            let path_start = offset + RPROV_RECORD_HEADER_BYTES;
            let payload_start = path_start + usize::from(header.path_bytes);
            let payload_end = payload_start + header.payload_bytes as usize;
            let path = std::str::from_utf8(&rprov[path_start..payload_start]).unwrap();
            if path.contains("/evidence/") {
                rprov[payload_start] ^= 1;
                corrupted += 1;
            }
            offset = payload_end;
        }
        assert_eq!(corrupted, wanted);
        rprov
    }

    impl Drop for ProductionRenderFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn clean_report() -> crate::verify::VerificationReport {
        crate::verify::VerificationReport {
            package_structure: VerificationStatus::Ok,
            event_chain: VerificationStatus::Ok,
            checkpoint_hashes: VerificationStatus::Ok,
            replay: VerificationStatus::Ok,
            submitted_source_match: SubmittedSourceStatus::Unavailable,
            assignment_reference: AssignmentReferenceStatus::Unverified,
            test_case_runs: Some(0),
            test_case_passes: Some(0),
            test_case_mismatches: Some(0),
            test_case_errors: Some(0),
            test_case_evidence: Some(crate::verify::TestCaseEvidenceStatus::Recorded),
            first_failing_case: None,
            external_changes: Some(0),
            unknown_edit_origins: Some(0),
            student_id: None,
            assignment_id: None,
            allowed_internal_pastes: Some(0),
            allowed_internal_paste_characters: Some(0),
            historical_origin_unverified_pastes: Some(0),
            historical_origin_unverified_paste_characters: Some(0),
            rejected_paste_attempts: Some(0),
            first_external_change: None,
            first_unknown_edit_origin: None,
            first_allowed_internal_paste: None,
            first_historical_origin_unverified_paste: None,
            first_rejected_paste_attempt: None,
            review_indicators: None,
            advisories: vec![],
            issues: vec![],
        }
    }

    fn blocked(sequence: u64) -> EventEnvelope {
        blocked_at(sequence, sequence)
    }

    fn blocked_at(sequence: u64, monotonic_millis: u64) -> EventEnvelope {
        EventEnvelope {
            format_version: 1,
            session_id: SessionId::new("render-test").unwrap(),
            sequence,
            monotonic_millis,
            wall_clock_utc: None,
            previous_event_hash: Hash::zero(),
            event_hash: Hash::zero(),
            event: RecordedEvent::PasteRejected(PasteRejected {
                reason: PasteRejectionReason::ExternalInput,
                channel: PasteInputChannel::TerminalBracketed,
            }),
        }
    }

    fn replay_input_hits() -> ReplayHitMap {
        ReplayHitMap {
            area: Rect::new(0, 0, 80, 24),
            events_pane: Rect::new(0, 0, 26, 12),
            source: Rect::new(26, 0, 54, 12),
            details: Rect::new(26, 13, 54, 10),
            ..ReplayHitMap::default()
        }
    }

    fn mouse_event(kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers,
        })
    }

    fn events_wheel(kind: MouseEventKind) -> Event {
        mouse_event(kind, 1, 1, KeyModifiers::NONE)
    }

    fn drain_replay_batch_at(
        state: &mut ReplayInputBatchState,
        hits: &ReplayHitMap,
        event: Event,
        queued: &mut VecDeque<Event>,
        now: Instant,
    ) -> ReplayInputBatch {
        drain_replay_input_batch(
            event,
            ReplayEventsClick::Uncaptured,
            state,
            hits,
            || now,
            || Ok::<_, ()>(queued.pop_front()),
        )
        .unwrap()
    }

    fn synthetic_controller(event_count: u64, gap_millis: u64) -> ReplayController {
        ReplayController::from_test_events(
            clean_report(),
            super::super::TimingAvailability::Recorded,
            (1..=event_count)
                .map(|sequence| blocked_at(sequence, (sequence - 1) * gap_millis))
                .collect(),
        )
    }

    fn two_attempt_controller(events_per_attempt: usize) -> ReplayController {
        let mut controller = synthetic_controller((events_per_attempt * 2) as u64, 1_000);
        controller.by_position.clear();
        for (index, event) in controller.events.iter_mut().enumerate() {
            event.position = EventPosition {
                segment: index / events_per_attempt,
                sequence: (index % events_per_attempt + 1) as u64,
            };
            controller.by_position.insert(event.position, index);
        }
        controller
    }

    fn render_controller(controller: &ReplayController, width: u16, height: u16) -> String {
        render_controller_with(controller, width, height, false, "footer only")
    }

    fn render_controller_with(
        controller: &ReplayController,
        width: u16,
        height: u16,
        show_flags: bool,
        status: &str,
    ) -> String {
        render_controller_with_scroll(controller, width, height, show_flags, status, 0)
    }

    fn render_controller_with_scroll(
        controller: &ReplayController,
        width: u16,
        height: u16,
        show_flags: bool,
        status: &str,
        flag_scroll: usize,
    ) -> String {
        let source = build_source_view(controller, None);
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        ReplayView {
            controller,
            editor: &source.editor,
            viewport: &source.viewport,
            highlights: &source.highlights,
            files: &source.files,
            file_index: source.file_index,
            following: source.following,
            recorded_selection: source.recorded_selection,
            diff_offset: source.diff_pane.offset,
            details_offset: 0,
            status,
            show_flags,
            flag_scroll,
        }
        .render(area, &mut buffer);
        buffer
            .content()
            .chunks(area.width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rows_in_rect(rendered: &str, rect: Rect) -> Vec<String> {
        rendered
            .lines()
            .skip(usize::from(rect.y))
            .take(usize::from(rect.height))
            .map(|row| {
                row.chars()
                    .skip(usize::from(rect.x))
                    .take(usize::from(rect.width))
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn replay_uses_the_same_explicit_active_tab_palette_as_the_student_tui() {
        let controller = ReplayController::from_test_events(
            clean_report(),
            super::super::TimingAvailability::Recorded,
            vec![blocked(1)],
        );
        let source = build_source_view(&controller, None);
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        let mut palette = Palette::catppuccin();
        palette.tab_active_bg = ratatui::style::Color::Rgb(1, 2, 3);
        palette.tab_active_fg = ratatui::style::Color::Rgb(4, 5, 6);
        let hits = ReplayView {
            controller: &controller,
            editor: &source.editor,
            viewport: &source.viewport,
            highlights: &source.highlights,
            files: &source.files,
            file_index: source.file_index,
            following: source.following,
            recorded_selection: source.recorded_selection,
            diff_offset: source.diff_pane.offset,
            details_offset: 0,
            status: "",
            show_flags: false,
            flag_scroll: 0,
        }
        .render_with_hit_map_palette(area, &mut buffer, &palette);

        let active = hits.tabs[0].0;
        assert_eq!(buffer[(active.x, active.y)].bg, palette.tab_active_bg);
        assert_eq!(buffer[(active.x, active.y)].fg, palette.tab_active_fg);
    }

    fn main_surface(rendered: &str) -> String {
        rendered
            .lines()
            .map(|row| row.chars().skip(26).collect::<String>())
            .map(|row| row.trim_end().to_owned())
            .collect::<String>()
    }

    fn details_rows(rendered: &str, width: u16, height: u16) -> Vec<String> {
        let ReplayShellLayoutResult::Full(layout) =
            replay_shell_layout(Rect::new(0, 0, width, height))
        else {
            panic!("expected supported replay size {width}x{height}");
        };
        rows_in_rect(rendered, layout.details)
    }

    fn numbered_lines(count: usize, changed: &[(usize, &str)]) -> String {
        (1..=count)
            .map(|line| {
                changed
                    .iter()
                    .find(|(number, _)| *number == line)
                    .map_or_else(
                        || format!("line {line:03}\n"),
                        |(_, text)| format!("{text}\n"),
                    )
            })
            .collect()
    }

    fn diff_between(current: &str, final_text: &str) -> super::super::DiffView {
        let (insertions, deletions, content) = super::super::diff::compare_files(
            Some(current.as_bytes()),
            Some(final_text.as_bytes()),
        );
        super::super::DiffView {
            mode: ComparisonMode::Final,
            path: String::from("main.rs"),
            comparison: Some(super::super::ComparisonPoint {
                segment: 0,
                sequence: 1,
            }),
            files: vec![String::from("main.rs")],
            insertions,
            deletions,
            content,
        }
    }

    fn controller_with_diff(diff: super::super::DiffView) -> ReplayController {
        let mut controller = ReplayController::from_test_events(
            clean_report(),
            super::super::TimingAvailability::Recorded,
            vec![blocked(1)],
        );
        controller.diff_view = Some(diff);
        controller
    }

    fn render_at_80x24(controller: &ReplayController, source: &SourceView) -> String {
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        ReplayView {
            controller,
            editor: &source.editor,
            viewport: &source.viewport,
            highlights: &source.highlights,
            files: &source.files,
            file_index: source.file_index,
            following: source.following,
            recorded_selection: source.recorded_selection,
            diff_offset: source.diff_pane.offset,
            details_offset: 0,
            status: "",
            show_flags: false,
            flag_scroll: 0,
        }
        .render(area, &mut buffer);
        buffer
            .content()
            .chunks(area.width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn add_three_files(controller: &mut ReplayController) {
        let selected = controller.selected.as_mut().expect("selected test event");
        selected.source = std::sync::Arc::new(vec![
            ("a.rs".to_owned(), b"a\n1\n2\n3\n4\n5\n6\n7\n".to_vec()),
            ("b.rs".to_owned(), b"b".to_vec()),
            ("c.rs".to_owned(), b"c".to_vec()),
        ]);
        selected.active_file = Some("a.rs".to_owned());
    }

    fn set_selected_text(
        controller: &mut ReplayController,
        path: &str,
        text: String,
        selection: Option<SelectionState>,
    ) {
        let selected = controller.selected.as_mut().expect("selected test event");
        selected.source = std::sync::Arc::new(vec![(path.to_owned(), text.into_bytes())]);
        selected.active_file = Some(path.to_owned());
        selected.active_document = selection.map(|selection| super::super::SelectedDocument {
            document_id: DocumentId::new("test-document").unwrap(),
            path: path.to_owned(),
            selection,
        });
    }

    fn assert_replay_shell_golden(width: u16, height: u16) {
        let mut controller = ReplayController::from_test_events(
            clean_report(),
            super::super::TimingAvailability::Recorded,
            vec![blocked(1), blocked(2), blocked(3)],
        );
        add_three_files(&mut controller);
        let output = render_controller(&controller, width, height);
        let contract = [
            ("files section", output.contains(" files")),
            ("events section", output.contains(" events")),
            ("source tab", output.contains(" source ")),
            ("diff tab", output.contains(" diff ")),
            ("flags tab", output.contains(" flags ")),
            ("details header", output.contains(" validation")),
            ("pause mode", output.contains(" PAUSE ")),
            ("play hint", output.contains("space play")),
            ("borderless panes", !output.contains(['┌', '┐', '└', '┘'])),
        ];
        assert!(
            contract.iter().all(|(_, passed)| *passed),
            "replay shell golden {width}x{height} failed: {contract:?}\n{output}"
        );
    }

    #[test]
    fn replay_shell_golden_at_80x24() {
        assert_replay_shell_golden(80, 24);
    }

    #[test]
    fn source_view_uses_recorded_unicode_selection_near_line_81() {
        let source = numbered_lines(128, &[(81, "let café = \"東京\";")]);
        let target = source.find("café").unwrap();
        let document_id = DocumentId::new("main-document").unwrap();
        let mut controller = ReplayController::from_test_events(
            clean_report(),
            super::super::TimingAvailability::Recorded,
            vec![EventEnvelope {
                format_version: 1,
                session_id: SessionId::new("render-test").unwrap(),
                sequence: 1,
                monotonic_millis: 15_575,
                wall_clock_utc: None,
                previous_event_hash: Hash::zero(),
                event_hash: Hash::zero(),
                event: RecordedEvent::SelectionChanged(SelectionChanged {
                    document_id: document_id.clone(),
                    anchor_byte: target as u64,
                    active_byte: (target + "café".len()) as u64,
                }),
            }],
        );
        let selected = controller.selected.as_mut().unwrap();
        selected.source = std::sync::Arc::new(vec![("main.rs".to_owned(), source.into_bytes())]);
        selected.active_file = Some("main.rs".to_owned());
        selected.active_document = Some(super::super::SelectedDocument {
            document_id,
            path: "main.rs".to_owned(),
            selection: SelectionState::new(target as u64, (target + "café".len()) as u64),
        });

        let mut source_view = build_source_view(&controller, None);
        let ReplayShellLayoutResult::Full(layout) = replay_shell_layout(Rect::new(0, 0, 80, 24))
        else {
            panic!("80x24 replay layout unavailable");
        };
        source_view.prepare_for_render(layout.source);

        assert_eq!(
            source_view.editor.selection_state(),
            SelectionState::new(target as u64, (target + "café".len()) as u64)
        );
        assert!(source_view.recorded_selection);
        assert!(
            source_view.viewport.top_line() > 60,
            "recorded line-81 selection was outside the visible Source viewport"
        );
    }

    #[test]
    fn clamped_source_wheel_still_changes_follow_state() {
        let mut controller = synthetic_controller(1, 1_000);
        add_three_files(&mut controller);
        let mut source = build_source_view(&controller, None);
        let mut show_flags = false;
        let mut flag_scroll = 0;
        let mut details_scroll = 0;

        let changed = apply_replay_mouse_input(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            &mut details_scroll,
            &replay_input_hits(),
            ReplayMouseInput::ScrollSource(-3),
        )
        .unwrap();

        assert!(changed, "a clamped Source wheel must enter manual mode");
    }

    #[test]
    fn manual_source_scroll_survives_flags_tab_toggle() {
        let mut controller = synthetic_controller(1, 1_000);
        let selected = controller.selected.as_mut().unwrap();
        selected.source = std::sync::Arc::new(vec![(
            "main.rs".to_owned(),
            numbered_lines(80, &[]).into_bytes(),
        )]);
        selected.active_file = Some("main.rs".to_owned());
        let mut source = build_source_view(&controller, None);
        source
            .viewport
            .scroll_vertical(30, source.editor.line_count(), 11);
        assert_eq!(source.viewport.top_line(), 30);
        let mut show_flags = false;
        let mut flag_scroll = 0;

        select_replay_tab(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            ReplayTab::Flags,
        )
        .unwrap();
        select_replay_tab(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            ReplayTab::Source,
        )
        .unwrap();

        assert_eq!(source.viewport.top_line(), 30);
    }

    #[test]
    fn manual_source_reading_survives_event_play_and_tabs_until_r_restores_following() {
        let mut controller = synthetic_controller(2, 1_000);
        let source_text = numbered_lines(128, &[]);
        let first_target = source_text.find("line 081").unwrap() as u64;
        set_selected_text(
            &mut controller,
            "main.rs",
            source_text.clone(),
            Some(SelectionState::caret(first_target)),
        );
        let ReplayShellLayoutResult::Full(layout) = replay_shell_layout(Rect::new(0, 0, 80, 24))
        else {
            panic!("80x24 replay layout unavailable");
        };
        let mut source = build_source_view(&controller, None);
        source.prepare_for_render(layout.source);
        assert!(source.viewport.top_line() > 60);

        let mut show_flags = false;
        let mut flag_scroll = 0;
        let mut details_scroll = 0;
        apply_replay_mouse_input(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            &mut details_scroll,
            &replay_input_hits(),
            ReplayMouseInput::ScrollSource(-20),
        )
        .unwrap();
        let manual_top = source.viewport.top_line();
        assert!(!source.following);

        controller.toggle_play();
        controller.toggle_play();
        assert_eq!(source.viewport.top_line(), manual_top);
        controller
            .select(EventPosition {
                segment: 0,
                sequence: 2,
            })
            .unwrap();
        let second_target = source_text.find("line 111").unwrap() as u64;
        set_selected_text(
            &mut controller,
            "main.rs",
            source_text,
            Some(SelectionState::caret(second_target)),
        );
        source.refresh(&controller);
        source.prepare_for_render(layout.source);
        assert_eq!(source.viewport.top_line(), manual_top);

        select_replay_tab(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            ReplayTab::Flags,
        )
        .unwrap();
        assert_eq!(source.viewport.top_line(), manual_top);

        controller.toggle_play();
        source.restore_following(&controller, layout.source);
        assert!(source.following);
        assert!(controller.is_playing(), "R changed playback state");
        assert!(show_flags, "R changed the active replay tab");
        assert!(source.viewport.top_line() > manual_top);
    }

    #[test]
    fn follow_refresh_keeps_viewport_when_new_caret_remains_visible() {
        let mut controller = synthetic_controller(2, 1_000);
        let source_text = numbered_lines(100, &[]);
        let first_target = source_text.find("line 051").unwrap() as u64;
        set_selected_text(
            &mut controller,
            "main.rs",
            source_text.clone(),
            Some(SelectionState::caret(first_target)),
        );
        let mut source = build_source_view(&controller, None);
        source
            .viewport
            .set_top_line(45, source.editor.line_count(), 11);

        controller
            .select(EventPosition {
                segment: 0,
                sequence: 2,
            })
            .unwrap();
        let next_target = source_text.find("line 053").unwrap() as u64;
        set_selected_text(
            &mut controller,
            "main.rs",
            source_text,
            Some(SelectionState::caret(next_target)),
        );
        source.refresh(&controller);
        source.prepare_for_render(Rect::new(26, 1, 54, 11));

        assert_eq!(source.viewport.top_line(), 45);
    }

    #[test]
    fn manual_file_browse_pins_deleted_path_without_a_fake_selection() {
        let mut controller = synthetic_controller(2, 1_000);
        let selected = controller.selected.as_mut().unwrap();
        selected.source = std::sync::Arc::new(vec![
            ("a.rs".to_owned(), b"active".to_vec()),
            ("b.rs".to_owned(), b"reading".to_vec()),
        ]);
        selected.active_file = Some("a.rs".to_owned());
        selected.active_document = Some(super::super::SelectedDocument {
            document_id: DocumentId::new("a-document").unwrap(),
            path: "a.rs".to_owned(),
            selection: SelectionState::caret(3),
        });
        let mut source = build_source_view(&controller, None);

        source.browse(&controller, "b.rs");
        assert_eq!(source.selected_path(), Some("b.rs"));
        assert!(!source.following);
        assert_eq!(source.viewport.top_line(), 0);

        controller
            .select(EventPosition {
                segment: 0,
                sequence: 2,
            })
            .unwrap();
        set_selected_text(
            &mut controller,
            "a.rs",
            "active later".to_owned(),
            Some(SelectionState::caret(4)),
        );
        source.refresh(&controller);

        assert_eq!(source.selected_path(), Some("b.rs"));
        assert!(source.editor.text().contains("file absent"));
        assert!(!source.recorded_selection);
    }

    #[test]
    fn absent_invalid_and_binary_source_selection_never_shows_a_recorded_caret() {
        let mut controller = synthetic_controller(1, 1_000);
        set_selected_text(
            &mut controller,
            "main.rs",
            "text".to_owned(),
            Some(SelectionState::caret(u64::MAX)),
        );
        assert!(!build_source_view(&controller, None).recorded_selection);

        controller.selected.as_mut().unwrap().active_document = None;
        assert!(!build_source_view(&controller, None).recorded_selection);

        let selected = controller.selected.as_mut().unwrap();
        selected.source = std::sync::Arc::new(vec![("main.rs".to_owned(), vec![0xff, 0xfe])]);
        selected.active_document = Some(super::super::SelectedDocument {
            document_id: DocumentId::new("binary-document").unwrap(),
            path: "main.rs".to_owned(),
            selection: SelectionState::caret(0),
        });
        assert!(!build_source_view(&controller, None).recorded_selection);
    }

    #[test]
    fn recorded_time_format_keeps_unbounded_hours() {
        assert_eq!(format_recorded_millis(3_600_000), "1:00:00.000");
        assert_eq!(format_recorded_millis(u64::MAX), "5124095576030:25:51.615");
    }

    #[test]
    fn source_status_uses_gap_for_exact_time_and_follow_controls_at_80x24() {
        let controller = ReplayController::from_test_events(
            clean_report(),
            super::super::TimingAvailability::Recorded,
            vec![blocked_at(1, 15_575)],
        );
        let rendered = render_controller(&controller, 80, 24);
        let rows = rendered.lines().collect::<Vec<_>>();

        assert!(rows[0].contains("PAUSE 1x follow:on"), "{rendered}");
        assert!(
            rows[12].contains("A1 t=0:00:15.575 idle:recorded"),
            "{rendered}"
        );
        assert!(rows[23].contains("r follow"), "{rendered}");
    }

    #[test]
    fn source_timing_row_reports_enabled_idle_skip() {
        let mut controller = ReplayController::from_test_events(
            clean_report(),
            super::super::TimingAvailability::Recorded,
            vec![blocked_at(1, 15_575)],
        );
        controller.toggle_skip_idle();

        let rendered = render_controller(&controller, 80, 24);

        assert!(rendered.lines().nth(12).unwrap().contains("idle:skip"));
    }

    #[test]
    fn opening_diff_refreshes_and_reaches_final_only_files() {
        let (_fixture, mut controller) =
            ProductionRenderFixture::final_only_controller("final-only-source-list");
        let mut source = build_source_view(&controller, None);
        assert!(!source.files.iter().any(|path| path == "final.rs"));
        let mut show_flags = false;
        let mut flag_scroll = 0;

        select_replay_tab(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            ReplayTab::Diff,
        )
        .unwrap();

        let final_index = source
            .files
            .iter()
            .position(|path| path == "final.rs")
            .expect("Diff omitted its final-only file");
        let mut details_scroll = 0;
        apply_replay_mouse_input(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            &mut details_scroll,
            &ReplayHitMap::default(),
            ReplayMouseInput::SelectFile(final_index),
        )
        .unwrap();
        assert_eq!(controller.diff_view().unwrap().path, "final.rs");
        assert_eq!(source.selected_path(), Some("final.rs"));
    }

    #[test]
    fn cached_checkpoint_source_metadata_matches_uncached_reconstruction() {
        let (_fixture, mut controller) = ProductionRenderFixture::controller("source-cache");
        let position = controller.selected_event().unwrap().position;
        assert!(
            !controller.cache.is_empty(),
            "fixture did not warm Source cache"
        );
        let cached = controller.selected_event().unwrap().clone();
        assert!(controller.cache_bytes() <= super::super::SEEK_CACHE_LIMIT_BYTES);

        controller.cache.clear();
        controller.cache_lru.clear();
        controller.select(position).unwrap();
        let uncached = controller.selected_event().unwrap();

        assert_eq!(uncached.source, cached.source);
        assert_eq!(uncached.active_file, cached.active_file);
        assert_eq!(uncached.active_document, cached.active_document);
        assert_eq!(uncached.millis, cached.millis);
    }

    #[test]
    fn events_wheel_batch_of_eighty_reports_admits_one_step() {
        let hits = replay_input_hits();
        let mut state = ReplayInputBatchState::default();
        let mut queued = std::iter::repeat_with(|| events_wheel(MouseEventKind::ScrollDown))
            .take(79)
            .collect::<VecDeque<_>>();

        let batch = drain_replay_batch_at(
            &mut state,
            &hits,
            events_wheel(MouseEventKind::ScrollDown),
            &mut queued,
            Instant::now(),
        );

        assert!(
            queued.is_empty(),
            "matching reports were left in the terminal queue"
        );
        assert!(state.pending.is_none());
        assert!(batch.events_wheel_admitted);
        assert_eq!(
            replay_mouse_input_for_event(
                match &batch.event {
                    Event::Mouse(mouse) => mouse,
                    event => panic!("wheel batch returned {event:?}"),
                },
                &hits,
                false,
                false,
            ),
            Some(ReplayMouseInput::ScrollEvents {
                delta: 3,
                admitted: true,
            })
        );
    }

    #[test]
    fn pending_event_click_keeps_the_identity_from_the_pre_wheel_hit_map() {
        let area = Rect::new(0, 0, 80, 24);
        let mut controller = synthetic_controller(50, 1_000);
        controller
            .select(EventPosition {
                segment: 0,
                sequence: 20,
            })
            .unwrap();
        let mut source = build_source_view(&controller, None);
        let render_hits = |controller: &ReplayController, source: &SourceView| {
            let mut buffer = Buffer::empty(area);
            ReplayView {
                controller,
                editor: &source.editor,
                viewport: &source.viewport,
                highlights: &source.highlights,
                files: &source.files,
                file_index: source.file_index,
                following: source.following,
                recorded_selection: source.recorded_selection,
                diff_offset: source.diff_pane.offset,
                details_offset: 0,
                status: "",
                show_flags: false,
                flag_scroll: 0,
            }
            .render_with_hit_map(area, &mut buffer)
        };
        let old_hits = render_hits(&controller, &source);
        let clicked = old_hits
            .events
            .iter()
            .find(|(_, position)| position.sequence == 20)
            .map(|(rect, _)| Position::new(rect.x, rect.y))
            .expect("selected event row was not rendered at 80x24");
        let wheel = mouse_event(
            MouseEventKind::ScrollDown,
            old_hits.events_pane.x,
            old_hits.events_pane.y,
            KeyModifiers::NONE,
        );
        let click = mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            clicked.x,
            clicked.y,
            KeyModifiers::NONE,
        );
        let mut queued = VecDeque::from([click]);
        let mut batch_state = ReplayInputBatchState::default();

        let wheel_batch = drain_replay_batch_at(
            &mut batch_state,
            &old_hits,
            wheel,
            &mut queued,
            Instant::now(),
        );
        let mut show_flags = false;
        let mut flag_scroll = 0;
        let mut details_scroll = 0;
        let wheel_input =
            replay_mouse_input_for_batch(&wheel_batch, &old_hits, false, false).unwrap();
        apply_replay_mouse_input(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            &mut details_scroll,
            &old_hits,
            wheel_input,
        )
        .unwrap();
        assert_eq!(controller.selected_event().unwrap().position.sequence, 23);

        source = build_source_view(&controller, None);
        let new_hits = render_hits(&controller, &source);
        let pending = batch_state
            .pending
            .take()
            .expect("event click was not preserved as the wheel boundary");
        let click_batch = drain_replay_input_batch(
            pending.event,
            pending.events_click,
            &mut batch_state,
            &new_hits,
            Instant::now,
            || Ok::<_, ()>(queued.pop_front()),
        )
        .unwrap();
        assert_eq!(
            replay_mouse_input_for_event(
                match &click_batch.event {
                    Event::Mouse(mouse) => mouse,
                    event => panic!("pending click became {event:?}"),
                },
                &new_hits,
                false,
                false,
            ),
            Some(ReplayMouseInput::SelectEvent(EventPosition {
                segment: 0,
                sequence: 23,
            })),
            "fixture did not reproduce row rebinding after recentering"
        );
        let click_input =
            replay_mouse_input_for_batch(&click_batch, &new_hits, false, false).unwrap();
        apply_replay_mouse_input(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            &mut details_scroll,
            &new_hits,
            click_input,
        )
        .unwrap();

        assert_eq!(
            controller.selected_event().unwrap().position.sequence,
            20,
            "pending click rebound to the post-wheel event row"
        );
    }

    #[test]
    fn pending_attempt_separator_click_stays_inert_after_wheel_recenters_events() {
        let area = Rect::new(0, 0, 80, 24);
        let mut controller = two_attempt_controller(20);
        controller
            .select(EventPosition {
                segment: 0,
                sequence: 19,
            })
            .unwrap();
        let mut source = build_source_view(&controller, None);
        let render_hits = |controller: &ReplayController, source: &SourceView| {
            let mut buffer = Buffer::empty(area);
            let hits = ReplayView {
                controller,
                editor: &source.editor,
                viewport: &source.viewport,
                highlights: &source.highlights,
                files: &source.files,
                file_index: source.file_index,
                following: source.following,
                recorded_selection: source.recorded_selection,
                diff_offset: source.diff_pane.offset,
                details_offset: 0,
                status: "",
                show_flags: false,
                flag_scroll: 0,
            }
            .render_with_hit_map(area, &mut buffer);
            (hits, buffer)
        };
        let (old_hits, old_buffer) = render_hits(&controller, &source);
        let first_second_attempt = old_hits
            .events
            .iter()
            .find(|(_, position)| position.segment == 1 && position.sequence == 1)
            .map(|(rect, _)| *rect)
            .expect("attempt 2 first event was not rendered at 80x24");
        let separator = Position::new(
            first_second_attempt.x,
            first_second_attempt
                .y
                .checked_sub(1)
                .expect("attempt separator was above the terminal"),
        );
        assert!(old_hits.events_pane.contains(separator));
        assert!(
            !old_hits
                .events
                .iter()
                .any(|(rect, _)| rect.contains(separator)),
            "attempt separator unexpectedly had an event identity"
        );
        assert!(
            old_buffer[(separator.x, separator.y)]
                .symbol()
                .starts_with('─'),
            "derived row was not the rendered attempt separator"
        );

        let wheel = mouse_event(
            MouseEventKind::ScrollDown,
            old_hits.events_pane.x,
            old_hits.events_pane.y,
            KeyModifiers::NONE,
        );
        let click = mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            separator.x,
            separator.y,
            KeyModifiers::NONE,
        );
        let mut queued = VecDeque::from([click]);
        let mut batch_state = ReplayInputBatchState::default();
        let wheel_batch = drain_replay_batch_at(
            &mut batch_state,
            &old_hits,
            wheel,
            &mut queued,
            Instant::now(),
        );
        let mut show_flags = false;
        let mut flag_scroll = 0;
        let mut details_scroll = 0;
        let wheel_input =
            replay_mouse_input_for_batch(&wheel_batch, &old_hits, false, false).unwrap();
        apply_replay_mouse_input(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            &mut details_scroll,
            &old_hits,
            wheel_input,
        )
        .unwrap();
        let wheel_target = EventPosition {
            segment: 1,
            sequence: 2,
        };
        assert_eq!(controller.selected_event().unwrap().position, wheel_target);

        source = build_source_view(&controller, None);
        let (new_hits, _) = render_hits(&controller, &source);
        let pending = batch_state
            .pending
            .take()
            .expect("separator click was not preserved as the wheel boundary");
        let click_batch = drain_replay_input_batch(
            pending.event,
            pending.events_click,
            &mut batch_state,
            &new_hits,
            Instant::now,
            || Ok::<_, ()>(queued.pop_front()),
        )
        .unwrap();
        assert_eq!(
            replay_mouse_input_for_event(
                match &click_batch.event {
                    Event::Mouse(mouse) => mouse,
                    event => panic!("pending separator click became {event:?}"),
                },
                &new_hits,
                false,
                false,
            ),
            Some(ReplayMouseInput::SelectEvent(EventPosition {
                segment: 1,
                sequence: 4,
            })),
            "fixture did not reproduce separator-row rebinding"
        );
        if let Some(click_input) =
            replay_mouse_input_for_batch(&click_batch, &new_hits, false, false)
        {
            apply_replay_mouse_input(
                &mut controller,
                &mut source,
                &mut show_flags,
                &mut flag_scroll,
                &mut details_scroll,
                &new_hits,
                click_input,
            )
            .unwrap();
        }

        assert_eq!(
            controller.selected_event().unwrap().position,
            wheel_target,
            "pending separator click rebound to a post-wheel event row"
        );
    }

    #[test]
    fn events_wheel_queue_uses_eight_bounded_batches_and_discards_suppressed_deltas() {
        let hits = replay_input_hits();
        let started = Instant::now();
        let mut state = ReplayInputBatchState::default();
        let mut queued = std::iter::repeat_with(|| events_wheel(MouseEventKind::ScrollDown))
            .take(1_024)
            .collect::<VecDeque<_>>();
        let mut admitted = 0;

        for _ in 0..8 {
            let first = queued
                .pop_front()
                .expect("each bounded batch must have a first report");
            let batch = drain_replay_batch_at(&mut state, &hits, first, &mut queued, started);
            admitted += usize::from(batch.events_wheel_admitted);
        }

        assert!(
            queued.is_empty(),
            "1,024 reports did not drain in eight turns"
        );
        assert!(state.pending.is_none());
        assert_eq!(admitted, 1, "suppressed deltas became deferred seeks");
    }

    #[test]
    fn moved_reports_count_toward_the_replay_wheel_read_cap_without_splitting_the_run() {
        let hits = replay_input_hits();
        let mut state = ReplayInputBatchState::default();
        let mut queued =
            std::iter::repeat_with(|| mouse_event(MouseEventKind::Moved, 1, 1, KeyModifiers::NONE))
                .take(126)
                .chain(std::iter::once(events_wheel(MouseEventKind::ScrollDown)))
                .collect::<VecDeque<_>>();

        let batch = drain_replay_batch_at(
            &mut state,
            &hits,
            events_wheel(MouseEventKind::ScrollDown),
            &mut queued,
            Instant::now(),
        );

        assert!(queued.is_empty());
        assert!(state.pending.is_none());
        assert!(batch.events_wheel_admitted);
        assert!(matches!(
            batch.event,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                ..
            })
        ));
    }

    #[test]
    fn moved_reports_at_a_cap_yield_keep_the_uninterrupted_limiter_run() {
        let hits = replay_input_hits();
        let started = Instant::now();
        let mut state = ReplayInputBatchState::default();
        let mut empty = VecDeque::new();
        assert!(
            drain_replay_batch_at(
                &mut state,
                &hits,
                events_wheel(MouseEventKind::ScrollDown),
                &mut empty,
                started,
            )
            .events_wheel_admitted
        );

        let mut queued =
            std::iter::repeat_with(|| mouse_event(MouseEventKind::Moved, 1, 1, KeyModifiers::NONE))
                .take(126)
                .chain(std::iter::once(events_wheel(MouseEventKind::ScrollDown)))
                .collect::<VecDeque<_>>();
        let batch = drain_replay_batch_at(
            &mut state,
            &hits,
            mouse_event(MouseEventKind::Moved, 1, 1, KeyModifiers::NONE),
            &mut queued,
            started + Duration::from_millis(49),
        );

        assert!(queued.is_empty());
        assert!(!batch.events_wheel_admitted);
        assert!(matches!(
            batch.event,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                ..
            })
        ));
    }

    #[test]
    fn replay_wheel_batch_counts_the_first_report_toward_the_128_report_cap() {
        let hits = replay_input_hits();
        let started = Instant::now();
        let mut state = ReplayInputBatchState::default();
        let mut capped = std::iter::repeat_with(|| events_wheel(MouseEventKind::ScrollDown))
            .take(128)
            .collect::<VecDeque<_>>();

        let _ = drain_replay_batch_at(
            &mut state,
            &hits,
            events_wheel(MouseEventKind::ScrollDown),
            &mut capped,
            started,
        );
        assert_eq!(
            capped.len(),
            1,
            "the 128-report cap must include the first report"
        );
    }

    #[test]
    fn replay_wheel_batch_yields_at_the_cooperative_two_millisecond_budget() {
        let hits = replay_input_hits();
        let started = Instant::now();
        let mut state = ReplayInputBatchState::default();
        let mut queued = VecDeque::from([
            events_wheel(MouseEventKind::ScrollDown),
            events_wheel(MouseEventKind::ScrollDown),
        ]);
        let mut samples = VecDeque::from([
            started,
            started + Duration::from_millis(1),
            started + Duration::from_millis(2),
        ]);
        let _ = drain_replay_input_batch(
            events_wheel(MouseEventKind::ScrollDown),
            ReplayEventsClick::Uncaptured,
            &mut state,
            &hits,
            || {
                samples
                    .pop_front()
                    .unwrap_or(started + Duration::from_millis(2))
            },
            || Ok::<_, ()>(queued.pop_front()),
        )
        .unwrap();
        assert_eq!(queued.len(), 1, "the cooperative 2 ms budget was exceeded");
    }

    #[test]
    fn replay_wheel_limiter_uses_fifty_milliseconds_and_reversal_is_immediate() {
        let hits = replay_input_hits();
        let started = Instant::now();
        let mut state = ReplayInputBatchState::default();
        let mut empty = VecDeque::new();
        let reports = [
            (MouseEventKind::ScrollDown, 0),
            (MouseEventKind::ScrollDown, 49),
            (MouseEventKind::ScrollDown, 50),
            (MouseEventKind::ScrollUp, 51),
            (MouseEventKind::ScrollUp, 99),
        ];
        let admissions = reports.map(|(kind, millis)| {
            drain_replay_batch_at(
                &mut state,
                &hits,
                events_wheel(kind),
                &mut empty,
                started + Duration::from_millis(millis),
            )
            .events_wheel_admitted
        });

        assert_eq!(admissions, [true, false, true, true, false]);

        let key = Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        let _ = drain_replay_batch_at(
            &mut state,
            &hits,
            key,
            &mut empty,
            started + Duration::from_millis(100),
        );
        assert!(
            drain_replay_batch_at(
                &mut state,
                &hits,
                events_wheel(MouseEventKind::ScrollUp),
                &mut empty,
                started + Duration::from_millis(101),
            )
            .events_wheel_admitted,
            "a boundary did not start a fresh limiter run"
        );
    }

    #[test]
    fn replay_wheel_preserves_each_ordering_boundary_in_pending_input() {
        let hits = replay_input_hits();
        let boundaries = vec![
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                1,
                1,
                KeyModifiers::NONE,
            ),
            mouse_event(
                MouseEventKind::Up(MouseButton::Left),
                1,
                1,
                KeyModifiers::NONE,
            ),
            mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                1,
                1,
                KeyModifiers::NONE,
            ),
            Event::Resize(120, 40),
            mouse_event(MouseEventKind::ScrollDown, 1, 1, KeyModifiers::SHIFT),
            mouse_event(MouseEventKind::ScrollDown, 30, 1, KeyModifiers::NONE),
            events_wheel(MouseEventKind::ScrollUp),
        ];

        for boundary in boundaries {
            let mut state = ReplayInputBatchState::default();
            let mut queued = VecDeque::from([
                events_wheel(MouseEventKind::ScrollDown),
                boundary.clone(),
                events_wheel(MouseEventKind::ScrollDown),
            ]);
            let _ = drain_replay_batch_at(
                &mut state,
                &hits,
                events_wheel(MouseEventKind::ScrollDown),
                &mut queued,
                Instant::now(),
            );

            assert_eq!(
                state.pending.as_ref().map(|input| &input.event),
                Some(&boundary),
                "boundary was dropped or reordered: {boundary:?}"
            );
            assert_eq!(queued.len(), 1, "input after boundary was read early");
        }
    }

    #[test]
    fn events_wheel_clamps_then_selects_once_for_each_admitted_direction() {
        let mut controller = synthetic_controller(10, 1_000);
        let mut source = build_source_view(&controller, None);
        let mut show_flags = false;
        let mut flag_scroll = 0;
        let mut details_scroll = 0;
        let hits = replay_input_hits();
        controller.diff_view = Some(diff_between("one\n", "two\n"));
        controller.select_index_calls = 0;

        apply_replay_mouse_input(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            &mut details_scroll,
            &hits,
            ReplayMouseInput::ScrollEvents {
                delta: 3,
                admitted: true,
            },
        )
        .unwrap();
        assert_eq!(controller.selected_event().unwrap().position.sequence, 4);
        assert_eq!(controller.select_index_calls, 1);
        assert!(
            controller.diff_view().is_none(),
            "effective wheel kept Diff open"
        );

        apply_replay_mouse_input(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            &mut details_scroll,
            &hits,
            ReplayMouseInput::ScrollEvents {
                delta: -3,
                admitted: true,
            },
        )
        .unwrap();
        assert_eq!(controller.selected_event().unwrap().position.sequence, 1);
        assert_eq!(controller.select_index_calls, 2);
    }

    fn assert_inert_events_wheel_pauses_without_seeking(input: ReplayMouseInput) {
        let hits = replay_input_hits();
        let mut controller = synthetic_controller(5, 1_000);
        let mut source = build_source_view(&controller, None);
        let mut show_flags = false;
        let mut flag_scroll = 0;
        let mut details_scroll = 0;
        controller.diff_view = Some(diff_between("one\n", "two\n"));
        controller.toggle_play();
        assert!(
            !controller
                .advance_playback(Duration::from_millis(900))
                .unwrap()
        );
        controller.select_index_calls = 0;

        let changed = apply_replay_mouse_input(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            &mut details_scroll,
            &hits,
            input,
        )
        .unwrap();

        assert!(changed, "the visible pause did not request a redraw");
        assert!(!controller.is_playing(), "inert wheel did not pause");
        assert_eq!(controller.playback_budget_millis, 0);
        assert_eq!(controller.select_index_calls, 0);
        assert_eq!(controller.selected_event().unwrap().position.sequence, 1);
        assert!(controller.diff_view().is_some(), "no-op wheel closed Diff");

        let changed = apply_replay_mouse_input(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            &mut details_scroll,
            &hits,
            input,
        )
        .unwrap();
        assert!(!changed, "paused no-op wheel requested a change draw");
        assert_eq!(controller.select_index_calls, 0);
        assert!(controller.diff_view().is_some());
    }

    #[test]
    fn clamped_events_wheel_pauses_and_clears_budget_without_seeking() {
        assert_inert_events_wheel_pauses_without_seeking(ReplayMouseInput::ScrollEvents {
            delta: -3,
            admitted: true,
        });
    }

    #[test]
    fn suppressed_events_wheel_pauses_and_clears_budget_without_seeking() {
        assert_inert_events_wheel_pauses_without_seeking(ReplayMouseInput::ScrollEvents {
            delta: 3,
            admitted: false,
        });
    }

    #[test]
    fn event_row_click_pauses_while_local_scrolls_preserve_playback() {
        let hits = replay_input_hits();
        let mut controller = synthetic_controller(5, 1_000);
        add_three_files(&mut controller);
        let mut source = build_source_view(&controller, None);
        let mut show_flags = false;
        let mut flag_scroll = 0;
        let mut details_scroll = 0;
        controller.toggle_play();
        assert!(
            !controller
                .advance_playback(Duration::from_millis(900))
                .unwrap()
        );
        controller.select_index_calls = 0;

        for local in [
            ReplayMouseInput::ScrollSource(3),
            ReplayMouseInput::ScrollDiff(3),
            ReplayMouseInput::ScrollFlags(3),
        ] {
            apply_replay_mouse_input(
                &mut controller,
                &mut source,
                &mut show_flags,
                &mut flag_scroll,
                &mut details_scroll,
                &hits,
                local,
            )
            .unwrap();
            assert!(controller.is_playing(), "local scroll paused playback");
            assert_eq!(controller.playback_budget_millis, 900);
            assert_eq!(controller.select_index_calls, 0);
        }

        apply_replay_mouse_input(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            &mut details_scroll,
            &hits,
            ReplayMouseInput::SelectEvent(EventPosition {
                segment: 0,
                sequence: 3,
            }),
        )
        .unwrap();
        assert!(!controller.is_playing(), "event-row click did not pause");
        assert_eq!(controller.playback_budget_millis, 0);
        assert_eq!(controller.select_index_calls, 1);
    }

    #[test]
    fn repeated_source_scroll_turns_do_not_discard_elapsed_playback() {
        let hits = replay_input_hits();
        let mut controller = synthetic_controller(3, 1_000);
        let mut source = build_source_view(&controller, None);
        let mut show_flags = false;
        let mut flag_scroll = 0;
        let mut details_scroll = 0;
        controller.toggle_play();
        controller.select_index_calls = 0;

        for turn in 0..4 {
            let input = ReplayMouseInput::ScrollSource(3);
            let blocks_autoplay = replay_mouse_input_blocks_autoplay(&input);
            controller.credit_playback(Duration::from_millis(250));
            apply_replay_mouse_input(
                &mut controller,
                &mut source,
                &mut show_flags,
                &mut flag_scroll,
                &mut details_scroll,
                &hits,
                input,
            )
            .unwrap();
            let advanced = advance_replay_after_input(&mut controller, blocks_autoplay).unwrap();
            assert_eq!(advanced, turn == 3, "unexpected advance on turn {turn}");
        }

        assert_eq!(controller.selected_event().unwrap().position.sequence, 2);
        assert_eq!(controller.select_index_calls, 1);
        assert!(controller.is_playing(), "Source scrolling paused playback");
        assert!(
            !source.following,
            "Source scrolling did not enter manual mode"
        );
    }

    #[test]
    fn quiet_due_playback_repeats_before_wait_without_overtaking_input() {
        let started = Instant::now();
        let mut quiet = synthetic_controller(4, 1_000);
        quiet.toggle_play();
        let mut quiet_tick = started;

        for expected_sequence in 2..=4 {
            assert!(
                advance_replay_before_wait(
                    &mut quiet,
                    &mut quiet_tick,
                    started + Duration::from_secs(3),
                    false,
                    false,
                )
                .unwrap(),
                "due event {expected_sequence} waited for another 25 ms poll"
            );
            assert_eq!(
                quiet.selected_event().unwrap().position.sequence,
                expected_sequence
            );
        }

        for (pending_input, queued_input) in [(true, false), (false, true)] {
            let mut blocked = synthetic_controller(2, 1_000);
            blocked.toggle_play();
            blocked.credit_playback(Duration::from_secs(1));
            let mut blocked_tick = started;
            assert!(
                !advance_replay_before_wait(
                    &mut blocked,
                    &mut blocked_tick,
                    started,
                    pending_input,
                    queued_input,
                )
                .unwrap(),
                "pre-wait autoplay overtook available input"
            );
            assert_eq!(blocked.selected_event().unwrap().position.sequence, 1);
            assert_eq!(blocked_tick, started);
        }
    }

    #[test]
    fn speed_transition_credits_old_rate_before_local_input_playback() {
        let mut controller = synthetic_controller(3, 1_000);
        controller.toggle_play();
        controller.select_index_calls = 0;
        let speed_key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
        let speed_blocks_autoplay = replay_key_blocks_autoplay(&controller, speed_key);
        controller.credit_playback(Duration::from_millis(300));

        assert!(
            !advance_replay_after_input(&mut controller, speed_blocks_autoplay).unwrap(),
            "S turn advanced before the speed control"
        );
        route_replay_binding(&mut controller, speed_key)
            .unwrap()
            .unwrap();
        assert_eq!(controller.speed(), PlaybackSpeed::Accelerated);

        controller.credit_playback(Duration::from_millis(100));
        assert!(
            !advance_replay_after_input(&mut controller, false).unwrap(),
            "the pre-S interval was retroactively accelerated"
        );
        controller.credit_playback(Duration::from_millis(75));
        assert!(
            advance_replay_after_input(&mut controller, false).unwrap(),
            "elapsed time around S and local input was lost"
        );
        assert_eq!(controller.selected_event().unwrap().position.sequence, 2);
        assert_eq!(controller.select_index_calls, 1);
    }

    #[test]
    fn pause_and_manual_control_win_the_turn_without_delayed_autoplay() {
        let mut controller = synthetic_controller(5, 1_000);
        controller.toggle_play();
        assert!(
            !controller
                .advance_playback(Duration::from_millis(900))
                .unwrap()
        );
        let pause_key = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        let pause_blocks_autoplay = replay_key_blocks_autoplay(&controller, pause_key);
        controller.credit_playback(Duration::from_millis(100));

        assert!(
            !advance_replay_after_input(&mut controller, pause_blocks_autoplay).unwrap(),
            "autoplay ran ahead of Pause"
        );
        controller.toggle_play();
        assert!(!controller.is_playing());
        assert_eq!(controller.selected_event().unwrap().position.sequence, 1);

        controller.credit_playback(Duration::from_secs(5));
        assert!(
            !advance_replay_after_input(&mut controller, false).unwrap(),
            "paused Source input advanced playback"
        );
        controller.toggle_play();
        assert!(controller.is_playing());
        controller.credit_playback(Duration::from_millis(999));
        assert!(
            !advance_replay_after_input(&mut controller, false).unwrap(),
            "paused time leaked across Resume"
        );
        controller.credit_playback(Duration::from_millis(1));
        assert!(
            advance_replay_after_input(&mut controller, false).unwrap(),
            "playing elapsed time was lost after Resume"
        );

        controller.select_index_calls = 0;
        let manual_key = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        let manual_blocks_autoplay = replay_key_blocks_autoplay(&controller, manual_key);
        controller.credit_playback(Duration::from_millis(1_000));
        assert!(
            !advance_replay_after_input(&mut controller, manual_blocks_autoplay).unwrap(),
            "autoplay ran ahead of manual navigation"
        );
        controller.next_event().unwrap();
        assert_eq!(controller.selected_event().unwrap().position.sequence, 3);
        assert_eq!(controller.select_index_calls, 1);
        assert!(!controller.is_playing());
        assert!(
            !advance_replay_after_input(&mut controller, false).unwrap(),
            "manual navigation left a delayed autoplay selection"
        );
        assert_eq!(controller.selected_event().unwrap().position.sequence, 3);
        assert_eq!(controller.select_index_calls, 1);
    }

    #[test]
    fn manual_selection_is_serviced_before_autoplay_with_one_selection_per_turn() {
        let mut controller = synthetic_controller(3, 1_000);
        controller.toggle_play();
        controller.select_index_calls = 0;

        controller.next_event().unwrap();
        controller.credit_playback(Duration::from_millis(1_000));
        let advanced = advance_replay_after_input(&mut controller, true).unwrap();

        assert!(!advanced, "autoplay ran after a manual selection");
        assert_eq!(controller.selected_event().unwrap().position.sequence, 2);
        assert_eq!(controller.select_index_calls, 1);
        assert!(!controller.is_playing());
    }

    #[test]
    fn comparison_details_render_before_the_reconstructed_output() {
        let comparison = rustrace_model::TestCaseCompared {
            command_id: rustrace_model::CommandId::new("command-1").unwrap(),
            case: "sample".to_owned(),
            expected_blake3: Hash::from_bytes([1; 32]),
            actual_blake3: Some(Hash::from_bytes([2; 32])),
            outcome: rustrace_model::TestCaseComparisonOutcome::Mismatch {
                line: 3,
                expected_len: 4,
                actual_len: 5,
            },
        };
        let controller = ReplayController::from_test_events(
            clean_report(),
            super::super::TimingAvailability::Recorded,
            vec![EventEnvelope {
                format_version: 1,
                session_id: SessionId::new("render-test").unwrap(),
                sequence: 1,
                monotonic_millis: 0,
                wall_clock_utc: None,
                previous_event_hash: Hash::zero(),
                event_hash: Hash::zero(),
                event: RecordedEvent::TestCaseCompared(comparison),
            }],
        );

        let rendered = render_controller(&controller, 120, 40);

        assert!(
            rendered.contains("Test case sample: mismatch"),
            "{rendered}"
        );
        assert!(
            rendered.contains("line 3; expected 4 bytes, actual 5 bytes"),
            "{rendered}"
        );
        assert!(rendered.contains("Expected BLAKE3:"), "{rendered}");
        assert!(rendered.contains("Actual BLAKE3:"), "{rendered}");
    }

    #[test]
    fn replay_mouse_hit_map_covers_events_files_tabs_play_and_each_scrollable_pane() {
        let mut controller = ReplayController::from_test_events(
            clean_report(),
            super::super::TimingAvailability::Recorded,
            vec![blocked(1), blocked(2), blocked(3)],
        );
        add_three_files(&mut controller);
        let source = build_source_view(&controller, None);
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        let hits = ReplayView {
            controller: &controller,
            editor: &source.editor,
            viewport: &source.viewport,
            highlights: &source.highlights,
            files: &source.files,
            file_index: source.file_index,
            following: source.following,
            recorded_selection: source.recorded_selection,
            diff_offset: source.diff_pane.offset,
            details_offset: 0,
            status: "",
            show_flags: false,
            flag_scroll: 0,
        }
        .render_with_hit_map(area, &mut buffer);

        assert_eq!(hits.events.len(), 3);
        assert_eq!(hits.files.len(), 3);
        assert_eq!(hits.tabs.len(), 3);
        assert!(!hits.play_pause.is_empty());
        for pane in [hits.events_pane, hits.source, hits.details] {
            assert!(!pane.is_empty());
        }

        let (event_rect, position) = hits.events[2];
        assert_eq!(
            replay_mouse_input_for_event(
                &MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: event_rect.x,
                    row: event_rect.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hits,
                false,
                false,
            ),
            Some(ReplayMouseInput::SelectEvent(position))
        );

        let (file_rect, file_index) = hits.files[1];
        assert_eq!(
            replay_mouse_input_for_event(
                &MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: file_rect.x,
                    row: file_rect.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hits,
                false,
                false,
            ),
            Some(ReplayMouseInput::SelectFile(file_index))
        );
        for (rect, tab) in &hits.tabs {
            assert_eq!(
                replay_mouse_input_for_event(
                    &MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: rect.x,
                        row: rect.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hits,
                    false,
                    false,
                ),
                Some(ReplayMouseInput::ShowTab(*tab))
            );
        }
        assert_eq!(
            replay_mouse_input_for_event(
                &MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: hits.play_pause.x,
                    row: hits.play_pause.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hits,
                false,
                false,
            ),
            Some(ReplayMouseInput::TogglePlay)
        );
        for (rect, show_flags, diff_open, expected) in [
            (
                hits.events_pane,
                false,
                false,
                ReplayMouseInput::ScrollEvents {
                    delta: 3,
                    admitted: true,
                },
            ),
            (hits.source, false, false, ReplayMouseInput::ScrollSource(3)),
            (hits.source, false, true, ReplayMouseInput::ScrollDiff(3)),
            (hits.source, true, false, ReplayMouseInput::ScrollFlags(3)),
            (
                hits.details,
                false,
                false,
                ReplayMouseInput::ScrollDetails(3),
            ),
        ] {
            assert_eq!(
                replay_mouse_input_for_event(
                    &MouseEvent {
                        kind: MouseEventKind::ScrollDown,
                        column: rect.x,
                        row: rect.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hits,
                    show_flags,
                    diff_open,
                ),
                Some(expected)
            );
        }

        let mut source = source;
        let mut show_flags = false;
        let mut flag_scroll = 0;
        let mut details_scroll = 0;
        apply_replay_mouse_input(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            &mut details_scroll,
            &hits,
            ReplayMouseInput::SelectFile(file_index),
        )
        .unwrap();
        assert_eq!(source.file_index, file_index);
        apply_replay_mouse_input(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            &mut details_scroll,
            &hits,
            ReplayMouseInput::SelectEvent(position),
        )
        .unwrap();
        assert_eq!(controller.selected_event().unwrap().position, position);
        apply_replay_mouse_input(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            &mut details_scroll,
            &hits,
            ReplayMouseInput::TogglePlay,
        )
        .unwrap();
        assert!(controller.is_playing());
    }

    #[test]
    fn replay_mouse_flags_scroll_uses_terminal_geometry() {
        let mut controller = two_flag_controller();
        let mut source = build_source_view(&controller, None);
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        let hits = ReplayView {
            controller: &controller,
            editor: &source.editor,
            viewport: &source.viewport,
            highlights: &source.highlights,
            files: &source.files,
            file_index: source.file_index,
            following: source.following,
            recorded_selection: source.recorded_selection,
            diff_offset: source.diff_pane.offset,
            details_offset: 0,
            status: "",
            show_flags: true,
            flag_scroll: 0,
        }
        .render_with_hit_map(area, &mut buffer);
        let (_, _, maximum) = flags_view_scroll_metrics(&controller, 80, 24);
        assert!(maximum >= 3, "fixture must have scrollable flag content");

        let mut show_flags = true;
        let mut flag_scroll = 0;
        let mut details_scroll = 0;
        apply_replay_mouse_input(
            &mut controller,
            &mut source,
            &mut show_flags,
            &mut flag_scroll,
            &mut details_scroll,
            &hits,
            ReplayMouseInput::ScrollFlags(isize::MAX),
        )
        .unwrap();

        assert_eq!(flag_scroll, maximum);
    }

    #[test]
    fn replay_shell_golden_at_120x40() {
        assert_replay_shell_golden(120, 40);
        let controller = ReplayController::from_test_events(
            clean_report(),
            super::super::TimingAvailability::Recorded,
            vec![blocked(1)],
        );
        let output = render_controller(&controller, 120, 40);
        assert!(
            output.contains(
                "space play  ←→ step  s speed  r follow  i idle  Enter evidence  f flag  g link  q quit"
            ),
            "replay mode bar omitted a reachable binding:\n{output}"
        );
    }

    #[test]
    fn replay_loop_routing_exercises_speed_idle_skip_and_indicator_navigation() {
        let events = vec![blocked_at(1, 0), blocked_at(2, 10_000)];
        let mut report = clean_report();
        let mut indicators = crate::process_indicators::ReviewIndicators::default();
        indicators.factual.rejected_paste.attempts = 1;
        indicators.factual.rejected_paste.links.push(
            crate::process_indicators::IndicatorEventLink {
                segment: 1,
                session_id: SessionId::new("render-test").unwrap(),
                sequence: 2,
            },
        );
        report.review_indicators = Some(indicators);
        let mut controller = ReplayController::from_test_events(
            report,
            super::super::TimingAvailability::Recorded,
            events,
        );

        route_replay_binding(
            &mut controller,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
        )
        .unwrap()
        .unwrap();
        assert_eq!(controller.speed(), PlaybackSpeed::Accelerated);
        assert!(render_controller(&controller, 120, 40).contains("4x"));

        route_replay_binding(
            &mut controller,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
        )
        .unwrap()
        .unwrap();
        assert!(controller.skip_idle());
        controller.toggle_play();
        assert!(
            controller
                .advance_playback(std::time::Duration::ZERO)
                .unwrap(),
            "I did not skip the recorded idle gap"
        );
        assert_eq!(controller.selected_event().unwrap().position.sequence, 2);

        controller.previous_event().unwrap();
        let status = route_replay_binding(
            &mut controller,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
        )
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(status, "followed review indicator event 1:2");
        assert_eq!(controller.selected_event().unwrap().position.sequence, 2);
    }

    fn two_flag_controller() -> ReplayController {
        let mut report = clean_report();
        report.package_structure = VerificationStatus::Failed;
        report.submitted_source_match = SubmittedSourceStatus::SourceMismatch;
        report.issues.push(VerificationIssue {
            kind: VerificationIssueKind::UnprovenancedExternalChange,
            location: VerificationIssueLocation::Artifact(format!(
                "segments/0001/evidence/{}.bin",
                "a".repeat(64)
            )),
            detail: "evidence digest mismatch".to_owned(),
        });
        report.issues.push(VerificationIssue {
            kind: VerificationIssueKind::SubmittedSource,
            location: VerificationIssueLocation::Event(crate::verify::VerificationEventLocation {
                segment: 1,
                sequence: 3,
            }),
            detail: "submitted source mismatch".to_owned(),
        });
        ReplayController::from_test_events(
            report,
            super::super::TimingAvailability::Recorded,
            vec![blocked(1), blocked(2), blocked(3)],
        )
    }

    fn assert_accepted_clean_details(height: u16) {
        let (_fixture, controller) = ProductionRenderFixture::controller("accepted-details");
        assert!(controller.verification_report().is_clean());
        let rendered = render_controller(&controller, 80, height);
        let details = details_rows(&rendered, 80, height);
        let normalized = details
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for expected in [
            "Verify package:OK chain:OK checkpoints:OK replay:OK",
            "Source:OK assignment:unverified | external:0 unknown-origin:0",
            "Process not eligible I:1 N:1 A:0 R:0 J:1 B:1 O:0 F:0",
            "Paste: historical 0 (0 chars, 0 lines) | internal 0 | blocked attempts 0",
            "Attempt 1 event 1",
            r#"{"format_version":1"#,
        ] {
            assert!(
                normalized.contains(expected),
                "missing accepted details row {expected:?} at 80x{height}:\n{rendered}"
            );
        }
        let ordered_rows = [
            " validation",
            "Verify package:",
            "Source:",
            "Process ",
            "Paste:",
            "Attempt ",
            r#"{"format_version":1"#,
        ]
        .map(|prefix| {
            details
                .iter()
                .position(|row| row.contains(prefix))
                .unwrap_or_else(|| panic!("missing ordered details row {prefix:?}:\n{rendered}"))
        });
        assert!(
            ordered_rows.windows(2).all(|rows| rows[0] < rows[1]),
            "details rows are out of order: {ordered_rows:?}\n{rendered}"
        );
    }

    #[test]
    fn clean_production_details_match_the_accepted_rows_at_80x24() {
        assert_accepted_clean_details(24);
    }

    #[test]
    fn clean_production_details_match_the_accepted_rows_at_80x40() {
        assert_accepted_clean_details(40);
    }

    #[test]
    fn details_pane_names_each_paste_count_with_readable_spacing() {
        let (_fixture, controller) = ProductionRenderFixture::controller("paste-details");
        let rendered = render_controller(&controller, 160, 24);
        let details = details_rows(&rendered, 160, 24).join("\n");
        assert!(
            details.contains(
                "Paste: historical 0 (0 chars, 0 lines) | internal 0 | blocked attempts 0"
            ),
            "{rendered}"
        );
    }

    #[test]
    fn flagged_footer_retains_enter_feedback_at_80x24() {
        let mut controller = two_flag_controller();
        assert_eq!(controller.follow_selected_evidence().unwrap(), None);
        let rendered = render_controller_with(
            &controller,
            80,
            24,
            false,
            "selected event has no direct evidence link",
        );
        assert!(
            rendered.contains("selected event has no direct evidence link"),
            "flagged footer clipped Enter feedback:\n{rendered}"
        );
    }

    #[test]
    fn clean_80x24_layout_preserves_base_source_file_and_event_heights() {
        let mut controller = ReplayController::from_test_events(
            clean_report(),
            super::super::TimingAvailability::Recorded,
            vec![blocked(1), blocked(2), blocked(3)],
        );
        add_three_files(&mut controller);

        let rendered = render_controller(&controller, 80, 24);
        let rows = rendered.lines().collect::<Vec<_>>();
        assert!(rows[0].contains(" source "), "{rendered}");
        assert!(rows[1].contains("│a"), "{rendered}");
        assert!(rows[8].contains("│7"), "{rendered}");
        assert!(rows[12].contains("A1 t="), "{rendered}");
        assert!(rows[13].contains(" validation"), "{rendered}");
        for path in ["a.rs", "b.rs", "c.rs"] {
            assert!(rendered.contains(path), "missing {path}:\n{rendered}");
        }
        assert!(rendered.contains("> 1:1 paste blocked"), "{rendered}");
        assert!(rendered.contains("  1:2 paste blocked"), "{rendered}");
    }

    #[test]
    fn first_change_is_visible_when_diff_opens_at_80x24() {
        let current = numbered_lines(20, &[]);
        let final_text = numbered_lines(20, &[(15, "line 015 edited")]);
        let controller = controller_with_diff(diff_between(&current, &final_text));
        let source = build_source_view(&controller, None);
        let rendered = render_at_80x24(&controller, &source);

        assert!(rendered.contains("- line 015"), "{rendered}");
        assert!(rendered.contains("+ line 015 edited"), "{rendered}");
    }

    #[test]
    fn scrolling_diff_reveals_a_later_change_at_80x24() {
        let current = numbered_lines(200, &[]);
        let final_text = numbered_lines(200, &[(15, "line 015 edited"), (150, "line 150 edited")]);
        let controller = controller_with_diff(diff_between(&current, &final_text));
        let mut source = build_source_view(&controller, None);
        let first = render_at_80x24(&controller, &source);
        assert!(first.contains("line 015 edited"), "{first}");
        assert!(!first.contains("line 150 edited"), "{first}");

        source
            .diff_pane
            .scroll_lines(controller.diff_view().unwrap(), 136);
        let scrolled = render_at_80x24(&controller, &source);
        assert!(scrolled.contains("- line 150"), "{scrolled}");
        assert!(scrolled.contains("+ line 150 edited"), "{scrolled}");
    }

    #[test]
    fn bounded_notice_omits_uncomputed_counts_at_80x24() {
        let mut diff = diff_between("", "");
        diff.content = DiffContent::Notice(super::super::DiffNotice::TooLarge);
        let controller = controller_with_diff(diff);
        let source = build_source_view(&controller, None);
        let rendered = render_at_80x24(&controller, &source);

        assert!(rendered.contains("diff too large to display"), "{rendered}");
        assert!(!rendered.contains("+0 -0"), "{rendered}");
    }

    #[test]
    fn trailing_newline_difference_is_visible_at_80x24() {
        let controller = controller_with_diff(diff_between("a\nb", "a\nb\n"));
        let source = build_source_view(&controller, None);
        let rendered = render_at_80x24(&controller, &source);

        assert!(rendered.contains("- b [no newline at end]"), "{rendered}");
        assert!(rendered.contains("+ b"), "{rendered}");
    }

    #[test]
    fn selected_event_marker_remains_visible_after_down_at_80x24() {
        let events = (1..=10).map(blocked).collect();
        let mut controller = ReplayController::from_test_events(
            clean_report(),
            super::super::TimingAvailability::Recorded,
            events,
        );
        for _ in 0..3 {
            controller.next_event().unwrap();
        }
        let rendered = render_controller(&controller, 80, 24);
        assert!(
            rendered.contains("> 1:4 paste blocked"),
            "selected event is absent at 80x24:\n{rendered}"
        );
    }

    #[test]
    fn clean_evidence_limitations_are_complete_at_80x24() {
        let controller = ReplayController::from_test_events(
            clean_report(),
            super::super::TimingAvailability::Recorded,
            vec![blocked(1)],
        );
        let replay = render_controller(&controller, 80, 24);
        assert!(
            !replay.contains("This provenance is internally replayable and consistent."),
            "limitations consumed accepted replay details rows:\n{replay}"
        );
        let rendered = render_controller_with(&controller, 80, 24, true, "footer only");
        let surface = main_surface(&rendered);
        assert!(surface.contains(EVIDENCE_CONSISTENCY), "{rendered}");
        assert!(surface.contains(EVIDENCE_LIMITATION_FIRST), "{rendered}");
        assert!(surface.contains(EVIDENCE_LIMITATION_SECOND), "{rendered}");
        assert!(rendered.contains("Attempt 1 event 1"), "{rendered}");
        assert!(rendered.contains(" source "), "{rendered}");
        assert!(rendered.contains(" flags "), "{rendered}");
        assert!(rendered.contains(" PAUSE "), "{rendered}");
    }

    #[test]
    fn review_view_presents_the_shared_factual_and_process_report() {
        let (_fixture, controller) = ProductionRenderFixture::controller("indicator-details");
        let rendered = wrapped_flag_lines(&controller, 160)
            .0
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        for expected in [
            "Factual review indicators:",
            "allowed internal paste: 0 / 0 chars (Unicode scalars)",
            "Cargo Check: started 0 complete 0",
            "process-review-v1 attempt 1: not eligible:",
            "I=1 N=1 A=0 R=0 J=1 B=1 O=0 F=0",
            "Unknown means a missing origin explanation, not established external input.",
            "Internal paste is allowed and not inherently suspicious.",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}:\n{rendered}"
            );
        }
    }

    #[test]
    fn failed_evidence_limitations_and_selected_event_are_complete_at_80x24() {
        let mut report = clean_report();
        report.submitted_source_match = SubmittedSourceStatus::SourceMismatch;
        report.issues.push(VerificationIssue {
            kind: VerificationIssueKind::SubmittedSource,
            location: VerificationIssueLocation::Decoder("outer submitted source tree".to_owned()),
            detail: "outer submitted source differs from replayed final tree".to_owned(),
        });
        let controller = ReplayController::from_test_events(
            report,
            super::super::TimingAvailability::Recorded,
            vec![blocked(1), blocked(2), blocked(3)],
        );
        let rendered = render_controller_with(&controller, 80, 24, true, "footer only");
        let surface = main_surface(&rendered);

        assert!(
            surface.contains("SOURCE_MISMATCH [location:outer submitted source tree]"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("This provenance is internally replayable and consistent."),
            "{rendered}"
        );
        assert!(
            surface.contains("This package did not validate."),
            "{rendered}"
        );
        assert!(surface.contains(EVIDENCE_LIMITATION_FIRST), "{rendered}");
        assert!(surface.contains(EVIDENCE_LIMITATION_SECOND), "{rendered}");

        let replay = render_controller(&controller, 80, 24);
        assert!(replay.contains(" PAUSE "), "{replay}");
        assert!(replay.contains("space play"), "{replay}");
        assert!(replay.contains(" diff "), "{replay}");
        assert!(replay.contains(" flags "), "{replay}");
        assert!(replay.contains("Attempt 1 event 1"), "{replay}");
        assert!(replay.contains("Flags: 1 (F next, V view)"), "{replay}");
        assert!(replay.contains("  1:2 paste blocked"), "{replay}");
    }

    #[test]
    fn multi_flag_view_reserves_limitations_and_exposes_scrolling_at_80x24() {
        let controller = two_flag_controller();

        let rendered = render_controller(&controller, 80, 24);
        assert!(rendered.contains("Flags: 2 (F next, V view)"), "{rendered}");
        let first_page = render_controller_with_scroll(&controller, 80, 24, true, "footer only", 0);
        let first_surface = main_surface(&first_page);
        assert!(first_surface.contains(EVIDENCE_FAILURE), "{first_page}");
        assert!(
            first_surface.contains(EVIDENCE_LIMITATION_FIRST),
            "{first_page}"
        );
        assert!(
            first_surface.contains(EVIDENCE_LIMITATION_SECOND),
            "{first_page}"
        );
        let (ranges, page_rows, _) = flags_view_scroll_metrics(&controller, 80, 24);
        let total = wrapped_flag_lines(&controller, 54).0.len();
        assert!(
            first_page.contains(&format!("lines 1-{page_rows} of {total}")),
            "{first_page}"
        );

        let second_start = ranges[1].0;
        let second_page =
            render_controller_with_scroll(&controller, 80, 24, true, "footer only", second_start);
        let second_unwrapped = main_surface(&second_page);
        let second = crate::review_flags::display_flag(&controller.review_flags()[1]);
        assert!(
            second_unwrapped.contains(&second),
            "scrolling did not reveal the complete second flag {second:?}:\n{second_page}"
        );

        let rendered = render_controller_with(&controller, 80, 40, true, "footer only");
        let unwrapped = main_surface(&rendered);
        for flag in controller.review_flags() {
            let expected = crate::review_flags::display_flag(&flag);
            assert!(
                unwrapped.contains(&expected),
                "complete flag is not reachable: {expected:?}\n{rendered}"
            );
        }
    }

    #[test]
    fn long_single_flag_tail_is_reachable_by_line_offset_at_80x24() {
        let (_fixture, controller) =
            ProductionRenderFixture::corrupt_evidence_controller("one-long-flag", 1);
        let flag = crate::review_flags::display_flag(&controller.review_flags()[0]);
        let mut wrapped = Vec::new();
        push_wrapped_lines(&mut wrapped, &flag, 54);
        assert!(wrapped.len() > 7, "fixture no longer has a wrapped tail");

        let first = render_controller_with_scroll(&controller, 80, 24, true, "footer only", 0);
        let (_, page_rows, max_start) = flags_view_scroll_metrics(&controller, 80, 24);
        let total = wrapped_flag_lines(&controller, 54).0.len();
        assert!(
            first.contains(&format!("lines 1-{page_rows} of {total}")),
            "{first}"
        );
        let tail_start = wrapped.len().saturating_sub(page_rows).min(max_start);
        let last =
            render_controller_with_scroll(&controller, 80, 24, true, "footer only", tail_start);
        for expected in &wrapped[tail_start..] {
            let expected = expected.spans[0].content.as_ref();
            assert!(
                last.contains(expected),
                "missing tail line {expected:?}:\n{last}"
            );
        }
    }

    #[test]
    fn every_line_of_two_long_flags_is_reachable_at_80x24() {
        let (_fixture, controller) =
            ProductionRenderFixture::corrupt_evidence_controller("two-long-flags", 2);
        let mut wrapped = Vec::new();
        for flag in controller.review_flags() {
            push_wrapped_lines(&mut wrapped, &display_flag(&flag), 54);
        }
        assert_eq!(
            wrapped.len(),
            16,
            "fixture no longer wraps to sixteen shell-width lines"
        );

        let (_, page_rows, max_start) = flags_view_scroll_metrics(&controller, 80, 24);
        let mut offsets = (0..=max_start).step_by(page_rows).collect::<Vec<_>>();
        if offsets.last().copied() != Some(max_start) {
            offsets.push(max_start);
        }
        let pages = offsets
            .into_iter()
            .map(|offset| {
                render_controller_with_scroll(&controller, 80, 24, true, "footer only", offset)
            })
            .collect::<Vec<_>>();
        for expected in wrapped {
            let expected = expected.spans[0].content.as_ref();
            assert!(
                pages.iter().any(|page| page.contains(expected)),
                "unreachable wrapped line {expected:?}:\n{}",
                pages.join("\n--- page ---\n")
            );
        }
    }

    #[test]
    fn followed_no_timeline_flag_detail_is_rendered_in_the_details_pane() {
        let report = crate::verify::VerificationReport::input_failure("invalid package bytes");
        let mut controller = ReplayController::from_test_events(
            report,
            super::super::TimingAvailability::Recorded,
            vec![],
        );
        let flag = controller.review_flags().remove(0);
        controller.follow_review_flag(&flag).unwrap();

        let rendered = render_controller_with(&controller, 80, 24, true, "footer only");
        assert!(
            rendered.contains("PACKAGE_INVALID [location:package input]"),
            "followed flag detail is absent from flags view:\n{rendered}"
        );
    }

    #[test]
    fn flags_view_wording_is_conditional_and_toggling_preserves_replay_state() {
        let clean = ReplayController::from_test_events(
            clean_report(),
            super::super::TimingAvailability::Recorded,
            vec![blocked(1), blocked(2), blocked(3)],
        );
        let clean_view = render_controller_with(&clean, 80, 40, true, "footer only");
        let clean_surface = main_surface(&clean_view);
        assert!(clean_surface.contains(EVIDENCE_CONSISTENCY), "{clean_view}");
        assert!(!clean_view.contains("This package did not validate."));

        let failed = two_flag_controller();
        let replay_state = |controller: &ReplayController| {
            (
                controller.selected_event().map(|event| event.position),
                controller.is_playing(),
                controller.speed(),
                controller.skip_idle(),
                controller.cache_bytes(),
                controller.last_seek_applied_events(),
                controller.counts(),
            )
        };
        let before = replay_state(&failed);
        let replay = render_controller_with(&failed, 80, 40, false, "footer only");
        let flags = render_controller_with(&failed, 80, 40, true, "footer only");
        let after = replay_state(&failed);
        assert_ne!(replay, flags, "V did not toggle to a distinct flags view");
        assert_eq!(after, before, "view toggling changed replay selection");
        let flag_surface = main_surface(&flags);
        assert!(
            flag_surface.contains("This package did not validate."),
            "{flags}"
        );
        assert!(!flag_surface.contains(EVIDENCE_CONSISTENCY), "{flags}");
        assert!(flag_surface.contains(EVIDENCE_LIMITATION_FIRST), "{flags}");
        assert!(flag_surface.contains(EVIDENCE_LIMITATION_SECOND), "{flags}");
    }

    #[test]
    fn advisory_flags_view_golden_has_a_separate_section() {
        let mut report = clean_report();
        report.advisories.push(crate::review_flags::AdvisoryFlag {
            kind: crate::review_flags::AdvisoryFlagKind::LargeSingleInsertion,
            link: crate::verify::VerificationEventLocation {
                segment: 1,
                sequence: 7,
            },
            measured_value: "200 bytes".to_owned(),
        });
        let controller = ReplayController::from_test_events(
            report,
            super::super::TimingAvailability::Recorded,
            vec![blocked(1)],
        );
        let rendered = wrapped_flag_lines(&controller, 160)
            .0
            .iter()
            .map(|line| line.spans[0].content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            rendered,
            concat!(
                "Hard review flags: none\n",
                "Advisory flags:\n",
                "LARGE_SINGLE_INSERTION [segment:1 seq:7]: ",
                "the record contains one Keyboard transaction inserting at least 200 bytes; ",
                "measured value: 200 bytes; advisory:\n",
                " heuristic; expect false positives"
            )
        );
    }

    #[test]
    fn rendered_ta_surfaces_quote_exact_limitations_and_avoid_verdict_vocabulary() {
        // T8.6: the exact Task 8.6 sentences, spelled literally so a drift in
        // the shared constants or the flags view fails here.
        let clean = ReplayController::from_test_events(
            clean_report(),
            super::super::TimingAvailability::Recorded,
            vec![blocked(1)],
        );
        let clean_flags = render_controller_with(&clean, 100, 40, true, "footer only");
        let clean_surface = main_surface(&clean_flags);
        for expected in [
            "This provenance is internally replayable and consistent.",
            "It does not prove that the client was unmodified or that the recorded code",
            "originated from the student.",
        ] {
            assert!(
                clean_surface.contains(expected),
                "missing {expected:?}:\n{clean_flags}"
            );
        }

        // A package whose checks passed but whose submitted-source comparison
        // was unavailable with a recorded issue is neither consistent nor
        // failed; it gets the neutral third form.
        let mut partial_report = clean_report();
        partial_report.issues.push(VerificationIssue {
            kind: VerificationIssueKind::SubmittedSource,
            location: VerificationIssueLocation::Decoder("outer submitted source tree".to_owned()),
            detail: "outer source became unreadable".to_owned(),
        });
        let partial = ReplayController::from_test_events(
            partial_report,
            super::super::TimingAvailability::Recorded,
            vec![blocked(1)],
        );
        let partial_flags = render_controller_with(&partial, 100, 40, true, "footer only");
        let partial_surface = main_surface(&partial_flags);
        assert!(
            partial_surface
                .contains("Package checks passed; reference or submitted source not evaluated."),
            "{partial_flags}"
        );
        assert!(
            !partial_surface.contains("This package did not validate."),
            "{partial_flags}"
        );
        assert!(
            !partial_surface.contains("This provenance is internally replayable and consistent."),
            "{partial_flags}"
        );

        // The details and initial status follow the same three-way outcome as
        // the flags view, so one screen never contradicts itself.
        let partial_replay = render_controller_with(&partial, 100, 40, false, "footer only");
        assert!(
            partial_replay.contains("Source:unavailable assignment:unverified"),
            "{partial_replay}"
        );
        assert!(
            !partial_replay.contains("Verify package:FAILED"),
            "{partial_replay}"
        );
        assert_eq!(
            initial_status(&partial),
            "validation incomplete; read-only replay"
        );
        assert_eq!(
            initial_status(&clean),
            "validation complete; read-only replay"
        );
        assert!(initial_status(&two_flag_controller()).starts_with("validation failed;"),);

        let failed = two_flag_controller();
        let diff_controller = controller_with_diff(diff_between("line 001\n", "line 002\n"));
        let surfaces = [
            ("clean flags view", clean_flags),
            (
                "clean replay view",
                render_controller_with(&clean, 100, 40, false, "followed flag"),
            ),
            (
                "failed flags view",
                render_controller_with(&failed, 100, 40, true, "footer only"),
            ),
            (
                "failed replay view",
                render_controller_with(&failed, 100, 40, false, "footer only"),
            ),
            (
                "diff pane",
                render_controller_with(&diff_controller, 100, 40, false, "footer only"),
            ),
            ("partial flags view", partial_flags),
        ];
        for (surface, rendered) in surfaces {
            let lower = rendered.to_ascii_lowercase();
            for forbidden in [
                "misconduct",
                "cheating",
                "plagiarism",
                "ai probability",
                "authorship proof",
                "detected",
            ] {
                assert!(
                    !lower.contains(forbidden),
                    "{surface} contains {forbidden:?}:\n{rendered}"
                );
            }
        }
    }

    #[test]
    fn diff_lines_escape_terminal_controls_before_rendering() {
        let rendered = render_diff_line(
            &DiffLine {
                kind: DiffLineKind::Insertion,
                text: String::from("safe\u{1b}]52;unsafe\u{7}\nnext"),
                unterminated: false,
            },
            &Palette::terminal(),
        )
        .spans
        .into_iter()
        .map(|span| span.content.into_owned())
        .collect::<String>();

        assert!(rendered.starts_with("+ safe"));
        assert!(rendered.contains("\\u{1b}]52;unsafe\\u{7}\\nnext"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{7}'));
        assert!(!rendered.contains('\n'));
    }
}
