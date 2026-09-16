use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, BorderType, Borders, Clear, Widget},
};
use rustrace_editor::EditorEffects;

use crate::{
    display,
    editor::{EditorBuffer, Viewport},
    tui::{EditorCommand, EditorOutcome},
};

use super::{
    shell::{HitMap, display_width, put_right_text, put_text, set_style},
    theme::Palette,
};

pub const COMPLETION_DELAY_MS: u64 = 200;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionTriggerInput {
    KeyboardInsertion(char),
    Deletion,
    Paste,
    Undo,
    Redo,
    Formatter,
    CompletionAcceptance,
    CursorMovement,
    BufferSwitch,
    Other,
    ModalEntry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ArmedCompletion {
    version: u64,
    deadline_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CompletionTrigger {
    armed: Option<ArmedCompletion>,
    previous_colon_version: Option<u64>,
}

impl CompletionTrigger {
    pub fn observe(&mut self, input: CompletionTriggerInput, version: u64, now_ms: u64) {
        let should_arm = match input {
            CompletionTriggerInput::KeyboardInsertion(character) => {
                character.is_alphanumeric()
                    || character == '_'
                    || character == '.'
                    || (character == ':'
                        && self
                            .previous_colon_version
                            .is_some_and(|previous| previous.checked_add(1) == Some(version)))
            }
            _ => false,
        };
        self.armed = should_arm.then_some(ArmedCompletion {
            version,
            deadline_ms: now_ms.saturating_add(COMPLETION_DELAY_MS),
        });
        self.previous_colon_version = match input {
            CompletionTriggerInput::KeyboardInsertion(':') => Some(version),
            _ => None,
        };
    }

    pub fn take_expired(&mut self, version: u64, now_ms: u64) -> bool {
        let Some(armed) = self.armed else {
            return false;
        };
        if version != armed.version {
            self.armed = None;
            self.previous_colon_version = None;
            return false;
        }
        if now_ms < armed.deadline_ms {
            return false;
        }
        self.armed = None;
        true
    }

    pub fn is_armed(&self) -> bool {
        self.armed.is_some()
    }
}

pub fn completion_trigger_input(
    command: &EditorCommand,
    outcome: &EditorOutcome,
) -> CompletionTriggerInput {
    match outcome {
        EditorOutcome::BufferSwitched | EditorOutcome::BufferClosed => {
            CompletionTriggerInput::BufferSwitch
        }
        EditorOutcome::SelectionChanged => CompletionTriggerInput::CursorMovement,
        EditorOutcome::ConfirmationRequired(_) => CompletionTriggerInput::ModalEntry,
        EditorOutcome::Edited => match command {
            EditorCommand::Insert(character) => {
                CompletionTriggerInput::KeyboardInsertion(*character)
            }
            EditorCommand::DeleteBackward
            | EditorCommand::DeleteForward
            | EditorCommand::DeletePreviousWord
            | EditorCommand::DeleteToLineStart
            | EditorCommand::DeleteToLineEnd
            | EditorCommand::Cut => CompletionTriggerInput::Deletion,
            EditorCommand::Paste | EditorCommand::PasteExternal(_) => CompletionTriggerInput::Paste,
            EditorCommand::Undo => CompletionTriggerInput::Undo,
            EditorCommand::Redo => CompletionTriggerInput::Redo,
            _ => CompletionTriggerInput::Other,
        },
        _ => CompletionTriggerInput::Other,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionKeyAction {
    Move(i32),
    Accept,
    Close,
    CloseAndPassThrough,
    PassThrough,
}

pub fn completion_key_action(popup_open: bool, key: KeyEvent) -> CompletionKeyAction {
    if !popup_open || key.kind == KeyEventKind::Release {
        return CompletionKeyAction::PassThrough;
    }
    match key.code {
        KeyCode::Up => CompletionKeyAction::Move(-1),
        KeyCode::Down => CompletionKeyAction::Move(1),
        KeyCode::Tab | KeyCode::Enter => CompletionKeyAction::Accept,
        KeyCode::Esc => CompletionKeyAction::Close,
        _ => CompletionKeyAction::CloseAndPassThrough,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionViewItem {
    pub label: String,
    pub kind: Option<u8>,
}

impl CompletionViewItem {
    pub fn new(label: impl Into<String>, kind: Option<u8>) -> Self {
        Self {
            label: label.into(),
            kind,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompletionPopupState {
    pub(crate) items: Vec<CompletionViewItem>,
    pub(crate) selected: usize,
}

const MAX_VISIBLE_ITEMS: usize = 8;
const MIN_POPUP_WIDTH: u16 = 20;
const MAX_POPUP_WIDTH: u16 = 48;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CompletionPopupAnchor {
    pub(crate) editor_area: Rect,
}

fn visible_window(item_count: usize, selected: usize, visible: usize) -> Option<usize> {
    if visible == 0 || selected >= item_count {
        return None;
    }
    Some(
        selected
            .saturating_sub(visible.saturating_sub(1))
            .min(item_count.saturating_sub(visible)),
    )
}

pub(crate) fn render_completion_popup<S>(
    state: &CompletionPopupState,
    editor: &EditorBuffer<S>,
    viewport: &Viewport,
    anchor: CompletionPopupAnchor,
    palette: &Palette,
    buffer: &mut Buffer,
    hits: &mut HitMap,
) where
    S: EditorEffects,
{
    let CompletionPopupAnchor { editor_area } = anchor;
    if state.items.is_empty() || editor_area.width < 3 || editor_area.height < 3 {
        return;
    }
    let cursor = editor.cursor();
    let Some(screen_line) = cursor.line.checked_sub(viewport.top_line()) else {
        return;
    };
    let Some(screen_column) = cursor.display_column.checked_sub(viewport.left_column()) else {
        return;
    };
    let caret_y = editor_area.y.saturating_add(screen_line as u16);
    let caret_x = editor_area.x.saturating_add(screen_column as u16);
    if caret_y >= editor_area.bottom() || caret_x >= editor_area.right() {
        return;
    }
    let below_y = caret_y.saturating_add(1);
    let available_below = editor_area.bottom().saturating_sub(below_y);
    let available_above = caret_y.saturating_sub(editor_area.y);
    let desired_height = u16::try_from(state.items.len().min(MAX_VISIBLE_ITEMS))
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let place_below = if available_below >= desired_height {
        true
    } else if available_above >= desired_height {
        false
    } else {
        available_below >= available_above
    };
    let available_height = if place_below {
        available_below
    } else {
        available_above
    };
    let visible = state
        .items
        .len()
        .min(MAX_VISIBLE_ITEMS)
        .min(usize::from(available_height.saturating_sub(2)));
    let Some(start) = visible_window(state.items.len(), state.selected, visible) else {
        return;
    };
    let items = state
        .items
        .iter()
        .skip(start)
        .take(visible)
        .map(|item| {
            let label = display::label(&item.label, rustrace_model::MAX_STRING_BYTES);
            let kind = item.kind.map(|kind| format!("kind {kind}"));
            (label, kind)
        })
        .collect::<Vec<_>>();
    let has_scrollbar = state.items.len() > visible;
    let content_width = items
        .iter()
        .map(|(label, kind)| {
            display_width(label)
                + kind.as_deref().map_or(0, |kind| 1 + display_width(kind))
                + usize::from(has_scrollbar)
        })
        .max()
        .unwrap_or_default();
    let width = u16::try_from(content_width.saturating_add(2))
        .unwrap_or(u16::MAX)
        .clamp(MIN_POPUP_WIDTH, MAX_POPUP_WIDTH)
        .min(editor_area.width);
    let height = visible as u16 + 2;
    let y = if place_below {
        below_y
    } else {
        caret_y.saturating_sub(height)
    };
    let x = caret_x.min(editor_area.right().saturating_sub(width));
    let area = Rect::new(x, y, width, height);
    hits.completion_popup = area;
    Clear.render(area, buffer);
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(palette.accent))
        .style(Style::default().bg(palette.panel_bg))
        .render(area, buffer);

    let thumb_length = if has_scrollbar {
        ((visible * visible) / state.items.len()).clamp(1, visible)
    } else {
        0
    };
    let thumb_top = if has_scrollbar {
        start
            .saturating_mul(visible.saturating_sub(thumb_length))
            .checked_div(state.items.len().saturating_sub(visible))
            .unwrap_or_default()
    } else {
        0
    };
    for (offset, (label, kind)) in items.iter().enumerate() {
        let index = start + offset;
        let row = Rect::new(area.x + 1, area.y + 1 + offset as u16, area.width - 2, 1);
        let selected = index == state.selected;
        let row_style = if selected {
            Style::default()
                .fg(palette.panel_contrast_fg())
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.text).bg(palette.panel_bg)
        };
        set_style(buffer, row, row_style);
        let scrollbar_width = u16::from(has_scrollbar);
        let content = Rect::new(row.x, row.y, row.width.saturating_sub(scrollbar_width), 1);
        let kind_width = kind.as_deref().map_or(0, display_width) as u16;
        let label_width = content
            .width
            .saturating_sub(kind_width)
            .saturating_sub(u16::from(kind.is_some()));
        put_text(buffer, content.x, content.y, label_width, label, row_style);
        if let Some(kind) = kind {
            let kind_style = if selected {
                row_style
            } else {
                Style::default().fg(palette.overlay0).bg(palette.panel_bg)
            };
            put_right_text(buffer, content, content.y, kind, kind_style);
        }
        if has_scrollbar {
            let thumb = (thumb_top..thumb_top + thumb_length).contains(&offset);
            put_text(
                buffer,
                row.right() - 1,
                row.y,
                1,
                if thumb { "▐" } else { "▕" },
                if selected {
                    row_style
                } else {
                    Style::default().fg(palette.overlay1).bg(palette.panel_bg)
                },
            );
        }
        hits.completion_rows.push((row, index));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn completion_anchor_uses_editor_coordinates_without_a_gutter_field() {
        let editor_area = Rect::new(3, 4, 20, 8);
        assert_eq!(
            CompletionPopupAnchor { editor_area }.editor_area,
            editor_area
        );
    }

    #[test]
    fn reducer_arms_identifier_dot_and_double_colon_with_injected_time() {
        for character in ['a', '7', '_', '東'] {
            let mut trigger = CompletionTrigger::default();
            trigger.observe(CompletionTriggerInput::KeyboardInsertion(character), 4, 10);
            assert!(trigger.is_armed(), "identifier {character:?}");
            assert!(!trigger.take_expired(4, 209));
            assert!(trigger.take_expired(4, 210));
        }

        let mut dot = CompletionTrigger::default();
        dot.observe(CompletionTriggerInput::KeyboardInsertion('.'), 8, 20);
        assert!(dot.is_armed());
        assert!(dot.take_expired(8, 220));

        let mut colon = CompletionTrigger::default();
        colon.observe(CompletionTriggerInput::KeyboardInsertion(':'), 9, 30);
        assert!(!colon.is_armed(), "one colon is not a trigger sequence");
        colon.observe(CompletionTriggerInput::KeyboardInsertion(':'), 10, 31);
        assert!(
            colon.is_armed(),
            "the second adjacent colon arms completion"
        );
        assert!(colon.take_expired(10, 231));
    }

    #[test]
    fn reducer_disarms_on_every_non_trigger_outcome_and_modal_entry() {
        let disarming = [
            CompletionTriggerInput::Deletion,
            CompletionTriggerInput::Paste,
            CompletionTriggerInput::Undo,
            CompletionTriggerInput::Redo,
            CompletionTriggerInput::Formatter,
            CompletionTriggerInput::CompletionAcceptance,
            CompletionTriggerInput::CursorMovement,
            CompletionTriggerInput::BufferSwitch,
            CompletionTriggerInput::Other,
            CompletionTriggerInput::ModalEntry,
        ];
        for input in disarming {
            let mut trigger = CompletionTrigger::default();
            trigger.observe(CompletionTriggerInput::KeyboardInsertion('x'), 2, 100);
            assert!(trigger.is_armed());
            trigger.observe(input, 2, 101);
            assert!(!trigger.is_armed(), "{input:?}");
            assert!(!trigger.take_expired(2, 1_000), "{input:?}");
        }

        let mut punctuation = CompletionTrigger::default();
        punctuation.observe(CompletionTriggerInput::KeyboardInsertion('x'), 2, 100);
        punctuation.observe(CompletionTriggerInput::KeyboardInsertion(' '), 3, 101);
        assert!(!punctuation.is_armed());
    }

    #[test]
    fn reducer_expires_only_for_the_unchanged_document_version() {
        let mut early = CompletionTrigger::default();
        early.observe(CompletionTriggerInput::KeyboardInsertion('x'), 12, 500);
        assert!(!early.take_expired(12, 699));
        assert!(early.is_armed());
        assert!(early.take_expired(12, 700));
        assert!(!early.is_armed());
        assert!(!early.take_expired(12, 900), "expiry is consumed once");

        let mut stale = CompletionTrigger::default();
        stale.observe(CompletionTriggerInput::KeyboardInsertion('x'), 12, 500);
        assert!(!stale.take_expired(13, 700));
        assert!(!stale.is_armed(), "a version mismatch retires the timer");
    }

    #[test]
    fn popup_key_actions_consume_only_navigation_acceptance_and_escape() {
        for code in [KeyCode::Tab, KeyCode::Enter] {
            assert_eq!(
                completion_key_action(false, press(code)),
                CompletionKeyAction::PassThrough,
                "{code:?} keeps its editor meaning before a response"
            );
            assert_eq!(
                completion_key_action(true, press(code)),
                CompletionKeyAction::Accept,
                "{code:?} accepts only while a list is open"
            );
        }
        assert_eq!(
            completion_key_action(true, press(KeyCode::Up)),
            CompletionKeyAction::Move(-1)
        );
        assert_eq!(
            completion_key_action(true, press(KeyCode::Down)),
            CompletionKeyAction::Move(1)
        );
        assert_eq!(
            completion_key_action(true, press(KeyCode::Esc)),
            CompletionKeyAction::Close
        );
        for code in [KeyCode::Char('x'), KeyCode::Char(' '), KeyCode::Left] {
            assert_eq!(
                completion_key_action(true, press(code)),
                CompletionKeyAction::CloseAndPassThrough,
                "{code:?} closes the popup without swallowing the key"
            );
        }

        let release =
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release);
        assert_eq!(
            completion_key_action(true, release),
            CompletionKeyAction::PassThrough
        );
    }

    #[test]
    fn every_bounded_selection_stays_inside_the_eight_row_window() {
        for selected in 0..crate::language_service::MAX_COMPLETION_ITEMS {
            let start = visible_window(
                crate::language_service::MAX_COMPLETION_ITEMS,
                selected,
                MAX_VISIBLE_ITEMS,
            )
            .unwrap();
            assert!((start..start + MAX_VISIBLE_ITEMS).contains(&selected));
        }
    }

    #[test]
    fn twenty_identifier_insertions_inside_the_delay_debounce_to_one_expiry() {
        let mut trigger = CompletionTrigger::default();
        let mut version = 0;
        for (index, character) in "abcdefghijklmnopqrst".chars().enumerate() {
            version += 1;
            let now_ms = index as u64 * 9;
            trigger.observe(
                CompletionTriggerInput::KeyboardInsertion(character),
                version,
                now_ms,
            );
            assert!(!trigger.take_expired(version, now_ms));
        }
        assert!(!trigger.take_expired(version, 370));
        assert!(trigger.take_expired(version, 371));
        assert!(!trigger.take_expired(version, 1_000));
    }
}
