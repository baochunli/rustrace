use crossterm::event::{
    Event as TerminalEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{Terminal, backend::TestBackend};
use rustrace::{
    session::{PASTE_BLOCKED_WARNING, ProductionSession, ResumeChoice, create_bundle},
    tui::{
        BufferTabViewEntry, EditorCommand, EditorContextMenuState, EditorOutcome, JournalHealth,
        MainView, MainViewState, MouseState, RecordingState, ShellInput, ShellState,
        WorkspaceInput, drain_mouse_event_batch, mouse_input_for_event,
    },
};
use rustrace_journal::{Journal, MAX_EVENTS_PER_READ, StoredCheckpoint};
use rustrace_model::{EventEnvelope, WorkspacePath};
use rustrace_replay::ReplayEngine;
use std::{
    collections::VecDeque,
    convert::Infallible,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

const WARNING: &str = "Paste blocked: only text copied or cut inside this recorded workspace is allowed. Use Ctrl-C, Ctrl-X and Ctrl-V. ⌘C, ⌘X and ⌘V work only when the terminal delivers them; macOS terminals capture ⌘C and ⌘V for the system clipboard.";
const SOURCE: &str = "é\r\n🦀e\u{301}\n";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rustrace-clipboard-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.rs"), SOURCE).unwrap();
        fs::write(root.join("b.rs"), "destination").unwrap();
        Self(fs::canonicalize(root).unwrap())
    }

    fn start(&self) -> ProductionSession {
        ProductionSession::start(&self.0, manifest()).unwrap()
    }

    fn reopen(&self) -> (StoredCheckpoint, Vec<EventEnvelope>) {
        let metadata = ProductionSession::read_metadata(&self.0).unwrap();
        let path = self
            .0
            .join(".rustrace")
            .join(format!("{}.sqlite", metadata.session_id));
        let mut journal = Journal::open_no_follow(&path).unwrap();
        let initial = journal
            .load_checkpoint(&metadata.session_id, 1)
            .unwrap()
            .unwrap();
        let events = journal
            .read_events(&metadata.session_id, 2, MAX_EVENTS_PER_READ)
            .unwrap();
        (initial, events)
    }

    fn journal_path(&self) -> PathBuf {
        let metadata = ProductionSession::read_metadata(&self.0).unwrap();
        self.0
            .join(".rustrace")
            .join(format!("{}.sqlite", metadata.session_id))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn blocked_paste_warning_names_working_keys_and_the_macos_terminal_boundary() {
    assert_eq!(PASTE_BLOCKED_WARNING, WARNING);
    for required in [
        "Ctrl-C, Ctrl-X and Ctrl-V",
        "⌘C, ⌘X and ⌘V work only when the terminal delivers them",
        "macOS terminals capture ⌘C and ⌘V for the system clipboard",
    ] {
        assert!(
            WARNING.contains(required),
            "missing clipboard hint: {required}"
        );
    }
}

fn manifest() -> &'static [u8] {
    br#"format_version = 1
course_id = "course"
assignment_id = "clipboard"
assignment_version = "v1"
title = "Internal clipboard"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#
}

#[derive(Debug, Eq, PartialEq)]
struct EditorState {
    text: String,
    version: u64,
    selection: rustrace_model::SelectionState,
    undo_bytes: usize,
    undo: bool,
    redo: bool,
}

fn state(session: &ProductionSession) -> EditorState {
    let buffer = session.workspace().active_buffer();
    EditorState {
        text: buffer.text(),
        version: buffer.version(),
        selection: buffer.selection_state(),
        undo_bytes: session.workspace().retained_undo_bytes(),
        undo: buffer.can_undo(),
        redo: buffer.can_redo(),
    }
}

fn mouse_hits(session: &ProductionSession) -> rustrace::tui::shell::HitMap {
    let workspace = session.workspace();
    let active = workspace.active_path();
    let buffers = workspace
        .file_tree()
        .iter()
        .filter(|file| file.document_id().is_some())
        .map(|file| {
            BufferTabViewEntry::new(file.path().as_str(), file.path() == active, file.is_dirty())
        })
        .collect();
    let state = MainViewState::new(
        "mouse provenance",
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
            hits = MainView::new(
                &state,
                workspace.active_buffer(),
                workspace.active_viewport(),
                &workspace.active_highlights(),
            )
            .render_with_hit_map(frame.area(), frame.buffer_mut());
        })
        .unwrap();
    hits
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn mapped_editor_command(
    event: &MouseEvent,
    hits: &rustrace::tui::shell::HitMap,
    reducer: &mut MouseState,
    now_ms: u64,
) -> EditorCommand {
    reducer.reduce(event, now_ms, Default::default());
    let input = mouse_input_for_event(event, hits, &ShellState::default(), reducer);
    let Some(ShellInput::Workspace(WorkspaceInput::Editor(rustrace::tui::SessionInput::Command(
        command,
    )))) = input
    else {
        panic!("mouse event did not map to an editor command: {input:?}");
    };
    command
}

fn assert_review2_confirmation_blocks_clipboard(
    command: EditorCommand,
    selection: bool,
    old_slot: bool,
) {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    if old_slot {
        session.execute(EditorCommand::SelectAll).unwrap();
        session.execute(EditorCommand::Copy).unwrap();
    }
    activate(&mut session, "b.rs");
    session.execute(EditorCommand::Insert('!')).unwrap();
    if selection {
        session.execute(EditorCommand::SelectAll).unwrap();
    }
    session.delete_selected().unwrap();
    assert!(session.workspace().delete_confirmation_pending());
    let before = state(&session);
    let selected = session.workspace().selected_path().clone();
    let events = fixture.reopen().1;
    let health = session.health().unwrap();
    let outcome = session.execute(command.clone());
    assert_eq!(state(&session), before, "{command:?} changed editor state");
    assert!(session.workspace().delete_confirmation_pending());
    assert_eq!(session.workspace().selected_path(), &selected);
    assert_eq!(session.health().unwrap().events, health.events);
    assert_eq!(
        session.health().unwrap().checkpoint_pending,
        health.checkpoint_pending
    );
    assert_eq!(fixture.reopen().1, events, "{command:?} changed journal");
    assert!(
        outcome.is_err(),
        "{command:?} bypassed pending confirmation"
    );
    assert_eq!(fs::read(fixture.0.join("b.rs")).unwrap(), b"destination");

    // The same pending deletion still requires an explicit confirmation. Its
    // completion must not have replaced or cleared the previous clipboard slot.
    session.confirm_delete().unwrap();
    assert!(!session.workspace().confirmation_pending());
    assert!(!fixture.0.join("b.rs").exists());
    session
        .execute(EditorCommand::Move {
            movement: rustrace_editor::Movement::DocumentEnd,
            selecting: false,
        })
        .unwrap();
    let pasted = session.execute(EditorCommand::Paste);
    if old_slot {
        assert!(pasted.is_ok());
        assert_eq!(session.workspace().active_buffer().text(), SOURCE.repeat(2));
    } else {
        assert!(pasted.is_err());
        assert_eq!(session.workspace().active_buffer().text(), SOURCE);
    }
    session.quit().unwrap();
}

#[test]
fn review2_copy_selection_preserves_pending_confirmation() {
    assert_review2_confirmation_blocks_clipboard(EditorCommand::Copy, true, false);
}

#[test]
fn review2_cut_selection_preserves_pending_confirmation() {
    assert_review2_confirmation_blocks_clipboard(EditorCommand::Cut, true, false);
}

#[test]
fn review2_copy_caret_respects_pending_confirmation_before_noop() {
    assert_review2_confirmation_blocks_clipboard(EditorCommand::Copy, false, false);
}

#[test]
fn review2_cut_caret_respects_pending_confirmation_before_noop() {
    assert_review2_confirmation_blocks_clipboard(EditorCommand::Cut, false, false);
}

#[test]
fn review2_copy_selection_preserves_old_slot_behind_confirmation() {
    assert_review2_confirmation_blocks_clipboard(EditorCommand::Copy, true, true);
}

#[test]
fn review2_cut_selection_preserves_old_slot_behind_confirmation() {
    assert_review2_confirmation_blocks_clipboard(EditorCommand::Cut, true, true);
}

#[test]
fn review2_copy_caret_preserves_old_slot_behind_confirmation() {
    assert_review2_confirmation_blocks_clipboard(EditorCommand::Copy, false, true);
}

#[test]
fn review2_cut_caret_preserves_old_slot_behind_confirmation() {
    assert_review2_confirmation_blocks_clipboard(EditorCommand::Cut, false, true);
}

#[cfg(unix)]
#[test]
fn review1_copy_and_cut_respect_workspace_recovery_latch() {
    use std::os::unix::fs::PermissionsExt;
    for command in [EditorCommand::Copy, EditorCommand::Cut] {
        let fixture = Fixture::new();
        let mut session = fixture.start();
        session.execute(EditorCommand::SelectAll).unwrap();
        fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o500)).unwrap();
        let created = session.create_file("new.rs");
        fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(created.is_err());
        assert!(session.recovery_reason().is_some());
        let before = state(&session);
        let count = session.health().unwrap().events;
        assert!(session.execute(command).is_err());
        assert_eq!(state(&session), before);
        assert_eq!(session.health().unwrap().events, count);
        assert!(!fixture.0.join("new.rs").exists());
        assert_eq!(fs::read(fixture.0.join("a.rs")).unwrap(), SOURCE.as_bytes());
    }
}

#[test]
fn review1_failed_replacement_preflight_preserves_previous_slot() {
    use rustrace_model::MAX_INTERNAL_CLIPBOARD_BYTES;
    for command in [EditorCommand::Copy, EditorCommand::Cut] {
        for text in [
            "x".repeat(MAX_INTERNAL_CLIPBOARD_BYTES + 1),
            "\0".repeat(MAX_INTERNAL_CLIPBOARD_BYTES),
        ] {
            let fixture = Fixture::new();
            fs::write(fixture.0.join("b.rs"), text).unwrap();
            let mut session = fixture.start();
            session.execute(EditorCommand::SelectAll).unwrap();
            session.execute(EditorCommand::Copy).unwrap();
            activate(&mut session, "b.rs");
            session.execute(EditorCommand::SelectAll).unwrap();
            let before = state(&session);
            let count = session.health().unwrap().events;
            assert!(session.execute(command.clone()).is_err());
            assert_eq!(state(&session), before);
            assert_eq!(session.health().unwrap().events, count);
            assert!(session.recovery_reason().is_none());
            // Paste into the original source at a caret: the rejected source
            // may itself exceed the normal inverse-transaction encoding cap.
            activate(&mut session, "a.rs");
            session
                .execute(EditorCommand::Move {
                    movement: rustrace_editor::Movement::DocumentEnd,
                    selecting: false,
                })
                .unwrap();
            session.execute(EditorCommand::Paste).unwrap();
            assert_eq!(session.workspace().active_buffer().text(), SOURCE.repeat(2));
        }
    }
}

#[test]
fn review1_direct_paste_rejects_pending_confirmation_without_mutation() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Copy).unwrap();
    activate(&mut session, "b.rs");
    session.execute(EditorCommand::Insert('!')).unwrap();
    session.delete_selected().unwrap();
    assert!(session.workspace().confirmation_pending());
    let before = state(&session);
    let count = session.health().unwrap().events;
    assert!(
        session
            .execute(EditorCommand::Paste)
            .unwrap_err()
            .to_string()
            .contains(WARNING)
    );
    assert_eq!(state(&session), before);
    assert!(session.workspace().confirmation_pending());
    assert_eq!(session.health().unwrap().events, count + 1);
    session.quit().unwrap();
    let (_, events) = fixture.reopen();
    let rejected = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                rustrace_model::Event::PasteRejected(rustrace_model::PasteRejected {
                    reason: rustrace_model::PasteRejectionReason::OutsideEditor,
                    channel: rustrace_model::PasteInputChannel::ProductionCommand,
                })
            )
        })
        .count();
    assert_eq!(rejected, 1);
}

#[test]
fn external_paste_is_warned_recorded_without_payload_and_changes_nothing() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session.execute(EditorCommand::Insert('!')).unwrap();
    session.execute(EditorCommand::Undo).unwrap();
    session.execute(EditorCommand::SelectAll).unwrap();
    let before = state(&session);
    let event_count = session.health().unwrap().events;
    let sentinel = "REJECTED_CLIPBOARD_SENTINEL_é\r\n🦀";
    let result = session.execute(EditorCommand::PasteExternal(sentinel.to_owned()));
    assert!(result.is_err(), "external clipboard must be rejected");
    let warning = result.unwrap_err().to_string();
    assert!(warning.contains(WARNING));
    assert!(!warning.contains(sentinel), "warning leaked rejected input");
    assert!(state(&session) == before, "rejection changed editor state");
    assert_eq!(session.health().unwrap().events, event_count + 1);
    assert_eq!(fs::read(fixture.0.join("a.rs")).unwrap(), SOURCE.as_bytes());
    session.capture_boundary().unwrap();
    session.quit().unwrap();
    let (initial, events) = fixture.reopen();
    let mut replay = ReplayEngine::from_initial_checkpoint(initial).unwrap();
    let mut attempts = 0;
    for event in &events {
        let bytes = rustrace_model::encode_envelope(event).unwrap();
        assert!(
            !bytes
                .windows(sentinel.len())
                .any(|part| part == sentinel.as_bytes()),
            "journal leaked rejected input"
        );
        let value = serde_json::to_value(event).unwrap();
        if value["event"]["type"] == "paste_rejected" {
            attempts += 1;
            assert!(bytes.len() <= 1024);
            assert_eq!(value["event"]["payload"].as_object().unwrap().len(), 2);
        }
        replay.apply(event).unwrap();
    }
    assert_eq!(attempts, 1);
}

#[test]
fn combined_external_blocker_precedes_clipboard_dispatch() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Copy).unwrap();
    fs::write(fixture.0.join("outside-policy.txt"), b"external").unwrap();
    assert!(session.recheck_external().is_err());
    assert!(session.recovery_reason().is_some());
    let before = state(&session);
    let count = session.health().unwrap().events;
    for command in [
        EditorCommand::Copy,
        EditorCommand::Cut,
        EditorCommand::Paste,
    ] {
        assert!(session.execute(command).is_err());
        assert_eq!(state(&session), before);
        assert_eq!(session.health().unwrap().events, count);
    }
    assert_eq!(
        fs::read(fixture.0.join("outside-policy.txt")).unwrap(),
        b"external"
    );
}

#[test]
fn mouse_selection_copy_and_paste_matches_keyboard_and_verifies_and_replays() {
    fn run(fixture: &Fixture, mouse: bool) -> Vec<rustrace_model::Event> {
        let mut session = fixture.start();
        if mouse {
            session
                .execute(EditorCommand::MoveTo {
                    line: 0,
                    column: 0,
                    selecting: false,
                })
                .unwrap();
            session
                .execute(EditorCommand::MoveTo {
                    line: 0,
                    column: 1,
                    selecting: true,
                })
                .unwrap();
        } else {
            session
                .execute(EditorCommand::Move {
                    movement: rustrace_editor::Movement::DocumentStart,
                    selecting: false,
                })
                .unwrap();
            session
                .execute(EditorCommand::Move {
                    movement: rustrace_editor::Movement::Right,
                    selecting: true,
                })
                .unwrap();
        }
        session.execute(EditorCommand::Copy).unwrap();
        session.execute(EditorCommand::NextBuffer).unwrap();
        session.execute(EditorCommand::SelectAll).unwrap();
        session.execute(EditorCommand::Paste).unwrap();
        session.save_all().unwrap();
        let receipt = session.finalize("student-1").unwrap();
        let bundle = create_bundle(&receipt, &fixture.0.join("submission.zip"))
            .unwrap()
            .path;
        let mut output = Vec::new();
        assert_eq!(
            rustrace::verify::run_verify(&[bundle.display().to_string()], &mut output).unwrap(),
            0,
            "{}",
            String::from_utf8_lossy(&output)
        );

        let (initial, envelopes) = fixture.reopen();
        let mut replay = ReplayEngine::from_initial_checkpoint(initial).unwrap();
        for envelope in &envelopes {
            replay.apply(envelope).unwrap();
        }
        assert_eq!(
            replay
                .workspace_state()
                .file(&WorkspacePath::new("b.rs").unwrap())
                .unwrap(),
            "é".as_bytes()
        );
        envelopes
            .into_iter()
            .map(|envelope| envelope.event)
            .collect()
    }

    let clicked = Fixture::new();
    let keyboard = Fixture::new();
    let mouse_events = run(&clicked, true);
    let keyboard_events = run(&keyboard, false);
    assert_eq!(mouse_events.len(), keyboard_events.len());
    for (mouse, keyboard) in mouse_events.iter().zip(&keyboard_events) {
        assert_eq!(
            std::mem::discriminant(mouse),
            std::mem::discriminant(keyboard),
            "mouse and keyboard controls emitted different event kinds"
        );
    }
    assert_eq!(
        mouse_events
            .iter()
            .filter(|event| matches!(event, rustrace_model::Event::SelectionChanged(_)))
            .count(),
        2,
        "mouse selection must record only its resulting selection states"
    );
    let encoded = serde_json::to_string(&mouse_events).unwrap();
    assert!(!encoded.contains("mouse"));
    assert!(!encoded.contains("coordinate"));
}

#[test]
fn keyboard_and_context_menu_clipboard_commands_record_identical_events() {
    fn scrub_live_references(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(values) => {
                for (key, value) in values {
                    if matches!(key.as_str(), "session_id" | "event_hash") {
                        *value = serde_json::Value::String("normalized".into());
                    } else {
                        scrub_live_references(value);
                    }
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    scrub_live_references(value);
                }
            }
            _ => {}
        }
    }

    fn run(fixture: &Fixture, menu: bool) -> (String, Vec<serde_json::Value>) {
        let mut session = fixture.start();
        let command = |index, direct| {
            if menu {
                EditorContextMenuState::new(
                    ratatui::layout::Position::new(30, 4),
                    index != 2,
                    index == 2,
                )
                .command(index)
                .expect("enabled context-menu entry")
            } else {
                direct
            }
        };

        session.execute(EditorCommand::SelectAll).unwrap();
        session.execute(command(1, EditorCommand::Copy)).unwrap();
        activate(&mut session, "b.rs");
        session.execute(EditorCommand::SelectAll).unwrap();
        session.execute(command(2, EditorCommand::Paste)).unwrap();
        session.execute(EditorCommand::SelectAll).unwrap();
        session.execute(command(0, EditorCommand::Cut)).unwrap();
        let before_external = state(&session);
        assert!(
            session
                .execute(EditorCommand::PasteExternal(
                    "EXTERNAL_CONTEXT_PAYLOAD".to_owned(),
                ))
                .is_err()
        );
        assert_eq!(state(&session), before_external);

        let final_text = session.workspace().active_buffer().text();
        let (_, events) = fixture.reopen();
        let mut events = events
            .into_iter()
            .filter_map(|envelope| {
                matches!(
                    envelope.event,
                    rustrace_model::Event::ClipboardCopied(_)
                        | rustrace_model::Event::InternalPaste(_)
                        | rustrace_model::Event::FileEdited(_)
                        | rustrace_model::Event::PasteRejected(_)
                )
                .then(|| serde_json::to_value(envelope.event).unwrap())
            })
            .collect::<Vec<_>>();
        for event in &mut events {
            scrub_live_references(event);
        }
        (final_text, events)
    }

    let keyboard = Fixture::new();
    let context_menu = Fixture::new();
    let keyboard_result = run(&keyboard, false);
    let menu_result = run(&context_menu, true);
    assert_eq!(menu_result, keyboard_result);
    let encoded = serde_json::to_string(&menu_result.1).unwrap();
    for event_type in [
        "clipboard_copied",
        "internal_paste",
        "file_edited",
        "paste_rejected",
    ] {
        assert!(
            encoded.contains(event_type),
            "missing {event_type}: {encoded}"
        );
    }
    assert!(!encoded.contains("EXTERNAL_CONTEXT_PAYLOAD"));
    assert!(!encoded.contains("mouse"));
}

#[test]
fn mouse_caret_drag_and_word_selection_record_only_bounded_selection_states() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("a.rs"), "alpha beta").unwrap();
    let mut session = fixture.start();
    let hits = mouse_hits(&session);
    let row = hits.editor.rect.y;
    let start = hits.editor.rect.x + 1;
    let mut reducer = MouseState::default();
    let down = mouse(MouseEventKind::Down(MouseButton::Left), start, row);
    session
        .execute(mapped_editor_command(&down, &hits, &mut reducer, 1))
        .unwrap();

    let drag_cells = 5_u64;
    let first_drag = TerminalEvent::Mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        start + 1,
        row,
    ));
    let mut queued = (2..=drag_cells)
        .map(|offset| {
            TerminalEvent::Mouse(mouse(
                MouseEventKind::Drag(MouseButton::Left),
                start + u16::try_from(offset).unwrap(),
                row,
            ))
        })
        .collect::<VecDeque<_>>();
    let batch =
        drain_mouse_event_batch(first_drag, || Ok::<_, Infallible>(queued.pop_front())).unwrap();
    let TerminalEvent::Mouse(final_drag) = &batch.event else {
        panic!("coalesced batch lost its drag event")
    };
    assert_eq!(final_drag.column, start + drag_cells as u16);
    assert!(batch.pending.is_none());
    let before_drag = session.health().unwrap().events;
    session
        .execute(mapped_editor_command(
            final_drag,
            &hits,
            &mut reducer,
            drag_cells + 1,
        ))
        .unwrap();
    let drag_events = session.health().unwrap().events - before_drag;
    assert!(
        drag_events <= drag_cells,
        "an N-cell drag recorded more than N selection states"
    );
    assert_eq!(
        session.workspace().active_buffer().selection_state(),
        rustrace_model::SelectionState::new(1, 6)
    );
    let events = fixture.reopen().1;
    assert!(
        events
            .iter()
            .skip(before_drag as usize - 1)
            .all(|event| matches!(event.event, rustrace_model::Event::SelectionChanged(_)))
    );
    assert!(events.len() <= before_drag as usize - 1 + drag_cells as usize);
}

#[test]
fn combined_canonical_restoration_preserves_exact_internal_source() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session.execute(EditorCommand::Insert('!')).unwrap();
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Copy).unwrap();
    fs::write(fixture.0.join("a.rs"), b"external disk bytes").unwrap();
    assert!(session.recheck_external().unwrap());
    let expected = format!("!{SOURCE}");
    assert_eq!(
        fs::read(fixture.0.join("a.rs")).unwrap(),
        expected.as_bytes()
    );
    activate(&mut session, "b.rs");
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Paste).unwrap();
    assert_eq!(session.workspace().active_buffer().text(), expected);
    session.save_all().unwrap();
    session.quit().unwrap();
    let views = ProductionSession::inspect(&fixture.0).unwrap();
    assert_eq!(views.logical, views.saved);
    assert_eq!(views.logical, views.disk);
    assert_eq!(
        views.logical[&WorkspacePath::new("b.rs").unwrap()],
        expected.as_bytes()
    );
}

#[test]
fn equal_external_text_and_empty_external_input_never_gain_internal_authority() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Copy).unwrap();
    for payload in [SOURCE, ""] {
        let before = state(&session);
        let count = session.health().unwrap().events;
        assert!(
            session
                .execute(EditorCommand::PasteExternal(payload.to_owned()))
                .is_err(),
            "content equality or an empty payload must not prove origin"
        );
        assert!(state(&session) == before, "rejection changed editor state");
        assert_eq!(session.health().unwrap().events, count + 1);
    }
}

#[test]
fn g3_m1_production_search_survives_typing_internal_paste_and_history() {
    use rustrace::tui::SearchOutcome;
    for paste in [false, true] {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("a.rs"), "abc").unwrap();
        fs::write(fixture.0.join("b.rs"), "é").unwrap();
        let mut session = fixture.start();
        if paste {
            activate(&mut session, "b.rs");
            session.execute(EditorCommand::SelectAll).unwrap();
            session.execute(EditorCommand::Copy).unwrap();
            activate(&mut session, "a.rs");
        }
        session.execute(EditorCommand::Search("a".into())).unwrap();
        session
            .execute(if paste {
                EditorCommand::Paste
            } else {
                EditorCommand::Insert('é')
            })
            .unwrap();
        assert_eq!(session.workspace().active_buffer().text(), "ébc");
        assert_eq!(
            session.execute(EditorCommand::SearchNext).unwrap(),
            EditorOutcome::Search(SearchOutcome::NoMatch)
        );
        session.execute(EditorCommand::Undo).unwrap();
        assert!(matches!(
            session.execute(EditorCommand::SearchNext).unwrap(),
            EditorOutcome::Search(SearchOutcome::Match {
                start_byte: 0,
                end_byte: 1,
                ..
            })
        ));
        session.execute(EditorCommand::Redo).unwrap();
        assert_eq!(
            session.execute(EditorCommand::SearchNext).unwrap(),
            EditorOutcome::Search(SearchOutcome::NoMatch)
        );
        session.save_all().unwrap();
        session.quit().unwrap();
        let views = ProductionSession::inspect(&fixture.0).unwrap();
        assert_eq!(views.logical, views.saved);
        assert_eq!(views.logical, views.disk);
        assert_eq!(
            views.logical[&WorkspacePath::new("a.rs").unwrap()],
            "ébc".as_bytes()
        );
    }
}

#[test]
fn internal_cut_has_exact_pre_cut_source_link_and_one_replayable_paste() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session.execute(EditorCommand::SelectAll).unwrap();
    let source_id = session.workspace().active_document_id().clone();
    let version = session.workspace().active_buffer().version();
    let hash = session.workspace().active_buffer().hash();
    session.execute(EditorCommand::Cut).unwrap();
    session.execute(EditorCommand::NextBuffer).unwrap();
    session.execute(EditorCommand::SelectAll).unwrap();
    let before = session.health().unwrap().events;
    assert_eq!(
        session.execute(EditorCommand::Paste).unwrap(),
        EditorOutcome::Edited
    );
    assert_eq!(session.health().unwrap().events, before + 1);
    assert_eq!(session.workspace().active_buffer().text(), SOURCE);
    session.execute(EditorCommand::Undo).unwrap();
    assert_eq!(session.workspace().active_buffer().text(), "destination");
    session.execute(EditorCommand::Redo).unwrap();
    session.quit().unwrap();
    let (initial, events) = fixture.reopen();
    let mut replay = ReplayEngine::from_initial_checkpoint(initial).unwrap();
    let values: Vec<_> = events
        .iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect();
    let proof = values
        .iter()
        .find(|event| event["event"]["type"] == "clipboard_copied");
    assert!(
        proof.is_some(),
        "internal cut must record exact pre-cut source proof"
    );
    let proof = proof.unwrap();
    let source = &proof["event"]["payload"];
    assert_eq!(source["document_id"], source_id.as_str());
    assert_eq!(source["path"], "a.rs");
    assert_eq!(source["version"], version);
    assert_eq!(source["content_hash"], hash.to_string());
    assert_eq!(source["start_byte"], 0);
    assert_eq!(source["end_byte"], SOURCE.len());
    let pasted: Vec<_> = values
        .iter()
        .filter(|event| event["event"]["type"] == "internal_paste")
        .collect();
    assert_eq!(
        pasted.len(),
        1,
        "one required-link paste event must be recorded"
    );
    assert_eq!(
        pasted[0]["event"]["payload"]["source"]["sequence"],
        proof["sequence"]
    );
    assert_eq!(
        pasted[0]["event"]["payload"]["source"]["event_hash"],
        proof["event_hash"]
    );
    assert_eq!(
        pasted[0]["event"]["payload"]["source"]["session_id"],
        proof["session_id"]
    );
    for event in &events {
        replay.apply(event).unwrap();
    }
    assert_eq!(
        replay
            .workspace_state()
            .file(&WorkspacePath::new("a.rs").unwrap())
            .unwrap(),
        b""
    );
    assert_eq!(
        replay
            .workspace_state()
            .file(&WorkspacePath::new("b.rs").unwrap())
            .unwrap(),
        SOURCE.as_bytes()
    );
}

#[test]
fn restart_clears_live_authority_without_deleting_the_destination_selection() {
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Copy).unwrap();
    session.quit().unwrap();
    let mut resumed =
        ProductionSession::resume(&fixture.0, manifest(), ResumeChoice::Resume).unwrap();
    let before = state(&resumed);
    assert!(
        resumed.execute(EditorCommand::Paste).is_err(),
        "restart cannot restore clipboard authority"
    );
    assert!(
        state(&resumed) == before,
        "empty clipboard deleted the selection"
    );
}

fn activate(session: &mut ProductionSession, path: &str) {
    for _ in 0..session.workspace().file_tree().len() {
        if session.workspace().active_path().as_str() == path {
            return;
        }
        session.execute(EditorCommand::NextBuffer).unwrap();
    }
    assert_eq!(session.workspace().active_path().as_str(), path);
}

#[test]
fn copied_snapshot_survives_source_edit_rename_delete_and_path_recreation() {
    for cut in [false, true] {
        for lifecycle in ["edit", "rename", "delete", "recreate"] {
            let fixture = Fixture::new();
            let mut session = fixture.start();
            session.execute(EditorCommand::SelectAll).unwrap();
            let original_id = session.workspace().active_document_id().clone();
            session
                .execute(if cut {
                    EditorCommand::Cut
                } else {
                    EditorCommand::Copy
                })
                .unwrap();
            match lifecycle {
                "edit" => {
                    session.execute(EditorCommand::Insert('!')).unwrap();
                }
                "rename" => {
                    session.rename_selected("renamed.rs").unwrap();
                }
                "delete" | "recreate" => {
                    session.delete_selected().unwrap();
                    if session.workspace().delete_confirmation_pending() {
                        session.confirm_delete().unwrap();
                    }
                    if lifecycle == "recreate" {
                        session.create_file("a.rs").unwrap();
                        assert_ne!(session.workspace().active_document_id(), &original_id);
                        session.execute(EditorCommand::Insert('N')).unwrap();
                    }
                }
                _ => unreachable!(),
            }
            activate(&mut session, "b.rs");
            session.execute(EditorCommand::SelectAll).unwrap();
            session.execute(EditorCommand::Paste).unwrap();
            assert_eq!(
                session.workspace().active_buffer().text(),
                SOURCE,
                "{cut}/{lifecycle}"
            );
            session.execute(EditorCommand::Undo).unwrap();
            assert_eq!(session.workspace().active_buffer().text(), "destination");
            session.execute(EditorCommand::Redo).unwrap();
            session.save_all().unwrap();
            session.quit().unwrap();
            let (initial, events) = fixture.reopen();
            let mut replay = ReplayEngine::from_initial_checkpoint(initial).unwrap();
            for event in &events {
                replay.apply(event).unwrap();
            }
            assert_eq!(
                replay
                    .workspace_state()
                    .file(&WorkspacePath::new("b.rs").unwrap())
                    .unwrap(),
                SOURCE.as_bytes()
            );
            let resumed =
                ProductionSession::resume(&fixture.0, manifest(), ResumeChoice::Resume).unwrap();
            assert_eq!(resumed.workspace().active_buffer().text(), SOURCE);
        }
    }
}

#[test]
fn nonzero_unsaved_version_and_reverse_unicode_range_bind_exact_source_bytes() {
    use rustrace_editor::Movement;
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session.execute(EditorCommand::Insert('漢')).unwrap();
    // Start after 漢; advance over é and CRLF, then select the crab backwards.
    for selecting in [false, false, false] {
        session
            .execute(EditorCommand::Move {
                movement: Movement::Right,
                selecting,
            })
            .unwrap();
    }
    session
        .execute(EditorCommand::Move {
            movement: Movement::Left,
            selecting: true,
        })
        .unwrap();
    assert_eq!(
        session.workspace().active_buffer().selection_state(),
        rustrace_model::SelectionState::new(11, 7)
    );
    session.execute(EditorCommand::Cut).unwrap();
    assert_eq!(
        session.workspace().active_buffer().text(),
        "漢é\r\ne\u{301}\n"
    );
    activate(&mut session, "b.rs");
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Paste).unwrap();
    assert_eq!(session.workspace().active_buffer().text(), "🦀");
    session.quit().unwrap();
    let (initial, events) = fixture.reopen();
    let mut replay = ReplayEngine::from_initial_checkpoint(initial).unwrap();
    let mut proofs = 0;
    for event in &events {
        if let rustrace_model::Event::ClipboardCopied(source) = &event.event {
            proofs += 1;
            assert_eq!(source.version, 1);
            assert_eq!((source.start_byte, source.end_byte), (7, 11));
            assert_eq!(
                source.content_hash,
                rustrace_model::document_hash(&format!("漢{SOURCE}"))
            );
            assert_eq!(source.prefix.sequence, event.sequence - 1);
            assert_eq!(source.prefix.event_hash, event.previous_event_hash);
        }
        replay.apply(event).unwrap();
    }
    assert_eq!(proofs, 1);
}

#[test]
fn valid_noop_and_empty_selection_do_not_create_edits_or_replace_the_slot() {
    use rustrace_editor::Movement;
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Copy).unwrap();
    let before = state(&session);
    let count = session.health().unwrap().events;
    assert_eq!(
        session.execute(EditorCommand::Paste).unwrap(),
        EditorOutcome::NoChange
    );
    assert_eq!(state(&session), before);
    assert_eq!(session.health().unwrap().events, count);
    session
        .execute(EditorCommand::Move {
            movement: Movement::DocumentStart,
            selecting: false,
        })
        .unwrap();
    let count = session.health().unwrap().events;
    assert_eq!(
        session.execute(EditorCommand::Copy).unwrap(),
        EditorOutcome::NoChange
    );
    assert_eq!(
        session.execute(EditorCommand::Cut).unwrap(),
        EditorOutcome::NoChange
    );
    assert_eq!(session.health().unwrap().events, count);
    activate(&mut session, "b.rs");
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Paste).unwrap();
    assert_eq!(session.workspace().active_buffer().text(), SOURCE);
    session.clear_clipboard();
    session.execute(EditorCommand::SelectAll).unwrap();
    let before = state(&session);
    assert!(session.execute(EditorCommand::Paste).is_err());
    assert_eq!(state(&session), before);
}

#[test]
fn separate_workspaces_and_linked_recovery_start_without_authority() {
    let fixture = Fixture::new();
    let other = Fixture::new();
    let mut first = fixture.start();
    first.execute(EditorCommand::SelectAll).unwrap();
    first.execute(EditorCommand::Copy).unwrap();
    let mut second = other.start();
    second.execute(EditorCommand::SelectAll).unwrap();
    let before = state(&second);
    assert!(second.execute(EditorCommand::Paste).is_err());
    assert_eq!(state(&second), before);
    assert_ne!(first.session_id(), second.session_id());
    first.quit().unwrap();
    let linked = Fixture::new();
    let mut recovery = ProductionSession::abandon_into(&fixture.0, &linked.0, manifest()).unwrap();
    recovery.execute(EditorCommand::SelectAll).unwrap();
    let before = state(&recovery);
    assert!(recovery.execute(EditorCommand::Paste).is_err());
    assert_eq!(state(&recovery), before);
}

#[test]
fn slot_and_destination_limits_preflight_without_mutation() {
    use rustrace_model::MAX_INTERNAL_CLIPBOARD_BYTES;
    for source in [
        "x".repeat(MAX_INTERNAL_CLIPBOARD_BYTES + 1),
        "\0".repeat(MAX_INTERNAL_CLIPBOARD_BYTES),
    ] {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("a.rs"), source).unwrap();
        let mut session = fixture.start();
        session.execute(EditorCommand::SelectAll).unwrap();
        let before = state(&session);
        let count = session.health().unwrap().events;
        assert!(
            session.execute(EditorCommand::Copy).is_err(),
            "oversized or unencodable source must not activate the slot"
        );
        assert_eq!(state(&session), before);
        assert_eq!(session.health().unwrap().events, count);
        assert!(session.execute(EditorCommand::Paste).is_err());
        assert_eq!(state(&session), before);
    }
    let fixture = Fixture::new();
    fs::write(
        fixture.0.join("a.rs"),
        "x".repeat(MAX_INTERNAL_CLIPBOARD_BYTES),
    )
    .unwrap();
    // Leave enough space for the existing bounded inverse-transaction encoding
    // when replacing the whole destination, but not for appending this slot.
    fs::write(fixture.0.join("b.rs"), "d".repeat(1024 * 1024 - 4096)).unwrap();
    let mut session = fixture.start();
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Copy).unwrap();
    activate(&mut session, "b.rs");
    let before = state(&session);
    let count = session.health().unwrap().events;
    assert!(session.execute(EditorCommand::Paste).is_err());
    assert_eq!(state(&session), before);
    assert_eq!(session.health().unwrap().events, count);
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Paste).unwrap();
    assert_eq!(
        session.workspace().active_buffer().text(),
        "x".repeat(MAX_INTERNAL_CLIPBOARD_BYTES)
    );
}

#[test]
fn repeated_rejections_charge_event_budget_and_stop_preserve_at_capacity() {
    use rustrace::session::SessionBudgets;
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session.execute(EditorCommand::SelectAll).unwrap();
    let before = state(&session);
    let initial = session.health().unwrap().events;
    session
        .set_budgets(SessionBudgets {
            events: initial + 5,
            ..SessionBudgets::default()
        })
        .unwrap();
    for _ in 0..3 {
        let result = session.execute(EditorCommand::PasteExternal("PRIVATE_ATTEMPT".to_owned()));
        assert!(result.unwrap_err().to_string().contains(WARNING));
        assert!(session.recovery_reason().is_none());
        assert!(state(&session) == before);
    }
    assert_eq!(session.health().unwrap().events, initial + 3);
    assert!(
        session
            .execute(EditorCommand::PasteExternal("PRIVATE_ATTEMPT".to_owned()))
            .unwrap_err()
            .to_string()
            .contains(WARNING)
    );
    assert!(session.recovery_reason().is_some());
    assert_eq!(session.health().unwrap().events, initial + 3);
    assert!(state(&session) == before);
    assert!(session.execute(EditorCommand::Insert('!')).is_err());
    assert!(session.save_all().is_err());
    assert_eq!(fs::read(fixture.0.join("a.rs")).unwrap(), SOURCE.as_bytes());
}

#[test]
fn rejected_attempt_storage_failure_never_accepts_or_retains_input() {
    use rustrace::session::SessionBudgets;
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session.execute(EditorCommand::SelectAll).unwrap();
    let before = state(&session);
    let count = session.health().unwrap().events;
    session
        .set_budgets(SessionBudgets {
            storage_bytes: 1,
            ..SessionBudgets::default()
        })
        .unwrap();
    let result = session.execute(EditorCommand::PasteExternal(
        "PRIVATE_STORAGE_SENTINEL".into(),
    ));
    let error = result.unwrap_err().to_string();
    assert!(error.contains(WARNING));
    assert!(!error.contains("PRIVATE_STORAGE_SENTINEL"));
    assert!(session.recovery_reason().is_some());
    assert!(state(&session) == before);
    assert_eq!(session.health().unwrap().events, count);
}

#[test]
fn journal_failure_during_copy_cut_paste_or_rejection_preserves_editor_and_disk() {
    for stage in ["copy", "cut", "paste", "reject"] {
        let fixture = Fixture::new();
        let mut session = fixture.start();
        session.execute(EditorCommand::SelectAll).unwrap();
        if stage == "paste" {
            session.execute(EditorCommand::Copy).unwrap();
            activate(&mut session, "b.rs");
            session.execute(EditorCommand::SelectAll).unwrap();
        }
        let before = state(&session);
        let count = session.health().unwrap().events;
        let connection = rusqlite::Connection::open(fixture.journal_path()).unwrap();
        // The accepted schema guard rejects unexpected triggers before insert.
        // This exercises a real writer failure, not a selective late-cut hook.
        connection
            .execute_batch(
                "CREATE TRIGGER fail_clipboard BEFORE INSERT ON events
             BEGIN SELECT RAISE(ABORT, 'injected clipboard append failure'); END;",
            )
            .unwrap();
        let command = match stage {
            "copy" => EditorCommand::Copy,
            "cut" => EditorCommand::Cut,
            "paste" => EditorCommand::Paste,
            "reject" => EditorCommand::PasteExternal("PRIVATE_FAILED_APPEND".to_owned()),
            _ => unreachable!(),
        };
        let error = session.execute(command).unwrap_err().to_string();
        assert!(!error.contains("PRIVATE_FAILED_APPEND"));
        if stage == "reject" {
            assert!(error.contains(WARNING));
        }
        assert!(
            state(&session) == before,
            "{stage}: failed append changed editor state"
        );
        assert!(
            session.recovery_reason().is_some(),
            "{stage}: must stop/preserve"
        );
        assert_eq!(session.health().unwrap().events, count);
        assert!(session.execute(EditorCommand::Paste).is_err());
        assert!(session.execute(EditorCommand::Insert('!')).is_err());
        assert!(state(&session) == before);
        assert!(session.save_all().is_err());
        assert_eq!(fs::read(fixture.0.join("a.rs")).unwrap(), SOURCE.as_bytes());
        assert_eq!(fs::read(fixture.0.join("b.rs")).unwrap(), b"destination");
        drop(session);
        connection
            .execute_batch("DROP TRIGGER fail_clipboard")
            .unwrap();
        drop(connection);
        let (initial, events) = fixture.reopen();
        let mut replay = ReplayEngine::from_initial_checkpoint(initial).unwrap();
        for event in &events {
            replay.apply(event).unwrap();
        }
        assert_eq!(
            replay
                .workspace_state()
                .file(&WorkspacePath::new("a.rs").unwrap())
                .unwrap(),
            SOURCE.as_bytes()
        );
        assert_eq!(
            replay
                .workspace_state()
                .file(&WorkspacePath::new("b.rs").unwrap())
                .unwrap(),
            b"destination"
        );
    }
}

#[test]
fn cut_failure_after_durable_source_proof_does_not_activate_the_new_slot() {
    use rustrace::session::SessionBudgets;
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session.execute(EditorCommand::SelectAll).unwrap();
    let before = state(&session);
    let count = session.health().unwrap().events;
    session
        .set_budgets(SessionBudgets {
            events: count + 3,
            ..SessionBudgets::default()
        })
        .unwrap();
    assert!(session.execute(EditorCommand::Cut).is_err());
    assert_eq!(
        session.health().unwrap().events,
        count + 1,
        "source proof persisted, deletion did not"
    );
    assert_eq!(state(&session), before);
    assert!(session.recovery_reason().is_some());
    assert!(
        session
            .execute(EditorCommand::Paste)
            .unwrap_err()
            .to_string()
            .contains(WARNING)
    );
    assert_eq!(state(&session), before);
    drop(session);
    let (initial, events) = fixture.reopen();
    let mut replay = ReplayEngine::from_initial_checkpoint(initial).unwrap();
    for event in &events {
        replay.apply(event).unwrap();
    }
    assert_eq!(
        replay
            .workspace_state()
            .file(&WorkspacePath::new("a.rs").unwrap())
            .unwrap(),
        SOURCE.as_bytes()
    );
    let mut resumed =
        ProductionSession::resume(&fixture.0, manifest(), ResumeChoice::Resume).unwrap();
    assert!(resumed.execute(EditorCommand::Paste).is_err());
    assert_eq!(resumed.workspace().active_buffer().text(), SOURCE);
}

#[test]
fn rejection_privacy_covers_encoded_events_decoded_checkpoints_and_state_artifacts() {
    use std::io::Read;
    let fixture = Fixture::new();
    let mut session = fixture.start();
    session.execute(EditorCommand::SelectAll).unwrap();
    let sentinel = "PRIVATE_CLIPBOARD_CONTENT_é\r\n🦀";
    assert!(
        session
            .execute(EditorCommand::PasteExternal(sentinel.into()))
            .is_err()
    );
    session.capture_boundary().unwrap();
    session.quit().unwrap();
    let metadata = ProductionSession::read_metadata(&fixture.0).unwrap();
    let mut journal = Journal::open_no_follow(fixture.journal_path()).unwrap();
    let (_, events) = fixture.reopen();
    let forbidden = [
        b"PRIVATE_CLIPBOARD".to_vec(),
        rustrace_model::document_hash(sentinel)
            .to_string()
            .into_bytes(),
        rustrace_model::observation_hash(sentinel.as_bytes())
            .to_string()
            .into_bytes(),
        blake3::hash(sentinel.as_bytes())
            .to_hex()
            .as_bytes()
            .to_vec(),
    ];
    let check = |bytes: &[u8]| {
        for pattern in &forbidden {
            assert!(
                !bytes.windows(pattern.len()).any(|part| part == pattern),
                "rejected payload or digest was retained"
            );
        }
    };
    for event in &events {
        check(&rustrace_model::encode_envelope(event).unwrap());
        if matches!(event.event, rustrace_model::Event::WorkspaceCheckpoint(_)) {
            let checkpoint = journal
                .load_checkpoint(&metadata.session_id, event.sequence)
                .unwrap()
                .unwrap();
            for file in checkpoint.snapshot.files() {
                check(&file.contents);
            }
        }
    }
    drop(journal);
    let views = ProductionSession::inspect(&fixture.0).unwrap();
    for bytes in views
        .saved
        .values()
        .chain(views.logical.values())
        .chain(views.disk.values())
    {
        check(bytes);
    }
    for entry in fs::read_dir(fixture.0.join(".rustrace")).unwrap() {
        let mut file = fs::File::open(entry.unwrap().path()).unwrap();
        let mut buffer = vec![0; 65536 + 64];
        let mut retained = 0;
        loop {
            let read = file.read(&mut buffer[retained..]).unwrap();
            if read == 0 {
                break;
            }
            let total = retained + read;
            check(&buffer[..total]);
            retained = total.min(64);
            buffer.copy_within(total - retained..total, 0);
        }
    }
}
