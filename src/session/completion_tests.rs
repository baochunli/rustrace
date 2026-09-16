use super::*;
use crate::language_service::{CompletionEdit, CompletionItem, CompletionResponse};
use rustrace_editor::position::Utf16Position;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "completion"
assignment_version = "v1"
title = "Completion"
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

struct Fixture(PathBuf);

impl Fixture {
    fn new(source: &str) -> (Self, ProductionSession) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rustrace-completion-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("main.rs"), source).unwrap();
        fs::write(root.join("other.rs"), "other").unwrap();
        let session = ProductionSession::start(&root, MANIFEST).unwrap();
        (Self(root), session)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn prepared(
    session: &mut ProductionSession,
    generation: u64,
) -> crate::language_service::CompletionRequest {
    session
        .prepare_completion_request(generation)
        .unwrap()
        .expect("completion request should pass authority gates")
}

fn respond(
    session: &mut ProductionSession,
    request: crate::language_service::CompletionRequest,
    items: Vec<CompletionItem>,
) -> bool {
    session.receive_completion(CompletionResponse { request, items })
}

fn insertion(label: &str, text: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_owned(),
        kind: Some(6),
        edit: CompletionEdit::Insert(text.to_owned()),
    }
}

#[test]
fn automatic_request_state_defers_one_request_per_document_until_completion() {
    let document = DocumentId::new("main").unwrap();
    let other = DocumentId::new("other").unwrap();
    let now = Instant::now();
    let mut requests = AutomaticCompletionRequests::default();

    assert!(requests.request(&document));
    requests.started(document.clone(), 11, now + Duration::from_secs(3));
    assert!(
        requests.request(&other),
        "documents are bounded independently"
    );
    requests.started(other.clone(), 12, now + Duration::from_secs(3));
    assert!(
        !requests.request(&document),
        "the second request is deferred"
    );
    assert!(
        !requests.request(&document),
        "only one deferred request is retained"
    );
    assert!(
        !requests.completed(&document, 10),
        "an unrelated response cannot release the bound"
    );
    assert!(requests.completed(&document, 11));
    assert!(requests.request(&document));
    requests.started(document.clone(), 13, now + Duration::from_secs(3));
    assert!(!requests.completed(&document, 11));
    assert!(!requests.completed(&document, 13));
    assert_eq!(requests.len(), 1, "the other document remains in flight");
}

#[test]
fn automatic_request_deadline_releases_exactly_one_deferred_request() {
    let document = DocumentId::new("main").unwrap();
    let now = Instant::now();
    let deadline = now + Duration::from_secs(3);
    let mut requests = AutomaticCompletionRequests::default();

    assert!(requests.request(&document));
    requests.started(document.clone(), 21, deadline);
    assert!(!requests.request(&document));
    assert!(
        requests
            .expire(deadline - Duration::from_millis(1))
            .is_empty()
    );
    assert_eq!(requests.expire(deadline), vec![document.clone()]);
    assert!(requests.expire(deadline).is_empty());
    assert!(requests.request(&document));
}

#[test]
fn automatic_request_state_cancels_deferred_intent_without_releasing_inflight_requests() {
    let document = DocumentId::new("main").unwrap();
    let other = DocumentId::new("other").unwrap();
    let now = Instant::now();
    let mut requests = AutomaticCompletionRequests::default();

    requests.started(document.clone(), 31, now + Duration::from_secs(3));
    requests.started(other.clone(), 32, now + Duration::from_secs(3));
    assert!(!requests.request(&document));
    assert!(!requests.request(&other));

    requests.cancel_deferred();
    assert_eq!(
        requests.len(),
        2,
        "cancellation must retain each in-flight request until response or deadline"
    );
    assert!(!requests.completed(&document, 31));
    assert!(requests.expire(now + Duration::from_secs(3)).is_empty());
    assert_eq!(requests.len(), 0);
}

#[test]
fn representative_typing_request_events_fit_the_accepted_g3_journal_budgets() {
    let automatic_requests = crate::session_fixture::representative_hour(54)
        .iter()
        .filter(|step| {
            matches!(
                step.action,
                crate::session_fixture::FixtureAction::Insert(_)
            )
        })
        .count();
    assert_eq!(automatic_requests, 2_807);

    let (_fixture, mut session) = Fixture::new("");
    let before = session.effects.0.borrow().sequence;
    for _ in 0..automatic_requests {
        prepared(&mut session, 7);
    }
    session.drain().unwrap();
    let health = session.health().unwrap();
    let recorded = health.events - before;
    let budgets = SessionBudgets::default();
    let projected_g3_events = 4_006 + recorded;
    eprintln!(
        "T10.8 request-volume measurement: requests={recorded} projected_g3_events={projected_g3_events} measured_storage_bytes={}",
        health.storage_bytes
    );
    assert_eq!(recorded, automatic_requests as u64);
    assert!(projected_g3_events < budgets.events);
    assert!(health.storage_bytes < budgets.storage_bytes);
}

fn completion_press(code: crossterm::event::KeyCode) -> crate::tui::CompletionKeyAction {
    crate::tui::completion_key_action(
        true,
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
    )
}

fn accept_stale_completion(session: &mut ProductionSession, mouse: bool) -> bool {
    if mouse {
        let mut hits = crate::tui::shell::HitMap {
            completion_popup: ratatui::layout::Rect::new(3, 4, 22, 3),
            ..Default::default()
        };
        hits.completion_rows
            .push((ratatui::layout::Rect::new(4, 5, 20, 1), 0));
        let event = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 4,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let input = crate::tui::mouse_input_for_event(
            &event,
            &hits,
            &crate::tui::ShellState {
                modal: crate::tui::ShellModal::Completion,
            },
            &crate::tui::MouseState::default(),
        );
        let Some(crate::tui::ShellInput::AcceptCompletion(index)) = input else {
            panic!("stale completion row did not map to acceptance: {input:?}")
        };
        assert!(session.select_completion(index));
    } else {
        assert_eq!(
            completion_press(crossterm::event::KeyCode::Tab),
            crate::tui::CompletionKeyAction::Accept
        );
    }
    session.accept_completion().unwrap()
}

fn recorded_events(fixture: &Fixture, session: &mut ProductionSession) -> Vec<serde_json::Value> {
    session.drain().unwrap();
    let metadata = ProductionSession::read_metadata(&fixture.0).unwrap();
    let journal_path = session
        .effects
        .0
        .borrow()
        .owner
        .display_path()
        .to_path_buf();
    let mut journal = rustrace_journal::Journal::open_read_only_no_follow(&journal_path).unwrap();
    journal
        .read_events(
            &metadata.session_id,
            1,
            rustrace_journal::MAX_EVENTS_PER_READ,
        )
        .unwrap()
        .into_iter()
        .map(|stored| serde_json::to_value(stored.event).unwrap())
        .collect()
}

#[test]
fn stale_mouse_row_after_typing_is_rejected_exactly_like_stale_tab() {
    fn run(mouse: bool) -> (Fixture, ProductionSession) {
        let (fixture, mut session) = Fixture::new("abc");
        let request = prepared(&mut session, 7);
        assert!(respond(
            &mut session,
            request,
            vec![insertion("stale", "X")]
        ));
        assert_eq!(session.selected_completion(), Some(0));
        session
            .workspace_mut()
            .execute_editor(EditorCommand::Insert('z'))
            .unwrap();
        assert!(
            !accept_stale_completion(&mut session, mouse),
            "stale completion gained acceptance authority"
        );
        assert_eq!(session.workspace().active_buffer().text(), "zabc");
        (fixture, session)
    }

    let (mouse_fixture, mut mouse) = run(true);
    let (key_fixture, mut keyboard) = run(false);
    assert_eq!(
        recorded_events(&mouse_fixture, &mut mouse),
        recorded_events(&key_fixture, &mut keyboard)
    );
}

#[test]
fn tab_and_enter_accept_only_after_a_fresh_version_bound_response() {
    for code in [
        crossterm::event::KeyCode::Tab,
        crossterm::event::KeyCode::Enter,
    ] {
        let (_fixture, mut session) = Fixture::new("abc");
        assert_eq!(
            crate::tui::completion_key_action(
                session.selected_completion().is_some(),
                crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
            ),
            crate::tui::CompletionKeyAction::PassThrough
        );
        let request = prepared(&mut session, 7);
        assert_eq!(
            crate::tui::completion_key_action(
                session.selected_completion().is_some(),
                crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
            ),
            crate::tui::CompletionKeyAction::PassThrough
        );
        assert!(respond(
            &mut session,
            request,
            vec![insertion("fresh", "X")]
        ));
        assert_eq!(
            completion_press(code),
            crate::tui::CompletionKeyAction::Accept
        );
        assert!(session.accept_completion().unwrap());
        assert_eq!(session.workspace().active_buffer().text(), "Xabc");
    }
}

#[test]
fn popup_acceptance_preempts_auto_indent_and_completion_edits_clear_pair_state() {
    for code in [
        crossterm::event::KeyCode::Tab,
        crossterm::event::KeyCode::Enter,
    ] {
        let (_fixture, mut session) = Fixture::new("fn main() {");
        session
            .execute(EditorCommand::Move {
                movement: rustrace_editor::Movement::DocumentEnd,
                selecting: false,
            })
            .unwrap();
        let request = prepared(&mut session, 7);
        assert!(respond(
            &mut session,
            request,
            vec![insertion("completed", "value")]
        ));
        assert_eq!(
            completion_press(code),
            crate::tui::CompletionKeyAction::Accept
        );
        assert!(session.accept_completion().unwrap());
        assert_eq!(
            session.workspace().active_buffer().text(),
            "fn main() {value",
            "{code:?} must not reach auto-indent while the popup is open"
        );
    }

    let (_fixture, mut session) = Fixture::new("");
    session.execute(EditorCommand::Insert('(')).unwrap();
    let request = prepared(&mut session, 7);
    assert!(respond(
        &mut session,
        request,
        vec![insertion("inside", "x")]
    ));
    assert!(session.accept_completion().unwrap());
    assert_eq!(session.workspace().active_buffer().text(), "(x)");
    session.execute(EditorCommand::Insert(')')).unwrap();
    assert_eq!(
        session.workspace().active_buffer().text(),
        "(x))",
        "the completion edit must clear transient over-type authority"
    );

    let (_fixture, mut session) = Fixture::new("fn main() {");
    session
        .execute(EditorCommand::Move {
            movement: rustrace_editor::Movement::DocumentEnd,
            selecting: false,
        })
        .unwrap();
    let request = prepared(&mut session, 7);
    assert!(respond(
        &mut session,
        request,
        vec![insertion("unused", "value")]
    ));
    assert_eq!(
        completion_press(crossterm::event::KeyCode::Esc),
        crate::tui::CompletionKeyAction::Close
    );
    session.clear_completion();
    session.execute(EditorCommand::Insert('\n')).unwrap();
    assert_eq!(
        session.workspace().active_buffer().text(),
        "fn main() {\n    "
    );
}

#[test]
fn typing_closes_the_popup_inserts_normally_and_rearms_the_timer() {
    let (_fixture, mut session) = Fixture::new("abc");
    let request = prepared(&mut session, 7);
    assert!(respond(
        &mut session,
        request,
        vec![insertion("fresh", "X")]
    ));
    assert_eq!(
        completion_press(crossterm::event::KeyCode::Char('z')),
        crate::tui::CompletionKeyAction::CloseAndPassThrough
    );
    session.clear_completion();
    let outcome = session.execute(EditorCommand::Insert('z')).unwrap();
    let mut trigger = crate::tui::CompletionTrigger::default();
    trigger.observe(
        crate::tui::CompletionTriggerInput::KeyboardInsertion('z'),
        session.workspace().active_buffer().version(),
        100,
    );
    assert_eq!(outcome, EditorOutcome::Edited);
    assert_eq!(session.workspace().active_buffer().text(), "zabc");
    assert!(session.completion_items().is_empty());
    assert!(trigger.is_armed());
}

#[test]
fn space_closes_the_popup_and_is_inserted_while_escape_only_closes() {
    let (_fixture, mut spaced) = Fixture::new("abc");
    let request = prepared(&mut spaced, 7);
    assert!(respond(&mut spaced, request, vec![insertion("fresh", "X")]));
    assert_eq!(
        completion_press(crossterm::event::KeyCode::Char(' ')),
        crate::tui::CompletionKeyAction::CloseAndPassThrough
    );
    spaced.clear_completion();
    spaced.execute(EditorCommand::Insert(' ')).unwrap();
    assert_eq!(spaced.workspace().active_buffer().text(), " abc");
    assert!(spaced.completion_items().is_empty());

    let (_fixture, mut escaped) = Fixture::new("abc");
    let request = prepared(&mut escaped, 7);
    assert!(respond(
        &mut escaped,
        request,
        vec![insertion("fresh", "X")]
    ));
    assert_eq!(
        completion_press(crossterm::event::KeyCode::Esc),
        crate::tui::CompletionKeyAction::Close
    );
    escaped.clear_completion();
    assert_eq!(escaped.workspace().active_buffer().text(), "abc");
    assert!(escaped.completion_items().is_empty());
}

#[test]
fn supported_unicode_insertion_records_one_completion_transaction_and_undo_redo_once() {
    let (_fixture, mut session) = Fixture::new("a🦀b");
    session
        .execute(EditorCommand::Move {
            movement: rustrace_editor::Movement::DocumentEnd,
            selecting: false,
        })
        .unwrap();
    let before_sequence = session.effects.0.borrow().sequence;
    let request = prepared(&mut session, 7);
    assert_eq!(request.position_byte, 6);
    assert_eq!(
        request.position,
        Utf16Position {
            line: 0,
            character: 4
        }
    );
    assert!(respond(
        &mut session,
        request,
        vec![insertion("東京", "東京")]
    ));
    assert_eq!(session.completion_items().len(), 1);
    assert!(session.accept_completion().unwrap());
    assert_eq!(session.workspace().active_buffer().text(), "a🦀b東京");
    assert_eq!(session.effects.0.borrow().sequence, before_sequence + 3);
    assert_eq!(session.workspace().active_buffer().version(), 1);

    session.execute(EditorCommand::Undo).unwrap();
    assert_eq!(session.workspace().active_buffer().text(), "a🦀b");
    session.execute(EditorCommand::Redo).unwrap();
    assert_eq!(session.workspace().active_buffer().text(), "a🦀b東京");
    let replay = session.effects.0.borrow();
    assert_eq!(
        replay
            .replay
            .as_ref()
            .unwrap()
            .workspace_state()
            .file(&WorkspacePath::new("main.rs").unwrap())
            .unwrap(),
        "a🦀b東京".as_bytes()
    );
}

#[test]
fn utf16_replacement_uses_the_exact_request_version_and_bytes() {
    let (_fixture, mut session) = Fixture::new("a🦀b");
    let request = prepared(&mut session, 7);
    assert!(respond(
        &mut session,
        request,
        vec![CompletionItem {
            label: "lambda".into(),
            kind: Some(3),
            edit: CompletionEdit::Replace {
                start: Utf16Position {
                    line: 0,
                    character: 1
                },
                end: Utf16Position {
                    line: 0,
                    character: 3
                },
                new_text: "λ".into(),
            },
        }]
    ));
    assert!(session.accept_completion().unwrap());
    assert_eq!(session.workspace().active_buffer().text(), "aλb");
}

#[test]
fn response_and_acceptance_recheck_edit_cursor_document_workspace_session_and_generation() {
    let (_fixture, mut session) = Fixture::new("abc");
    let older = prepared(&mut session, 7);
    let current = prepared(&mut session, 7);
    assert_ne!(older.request_sequence, current.request_sequence);
    assert!(!respond(
        &mut session,
        older,
        vec![insertion("out of order", "old")]
    ));
    assert!(respond(
        &mut session,
        current,
        vec![insertion("current", "")]
    ));
    assert!(session.completion_items().is_empty());

    let request = prepared(&mut session, 7);
    session.execute(EditorCommand::Insert('x')).unwrap();
    assert!(!respond(
        &mut session,
        request,
        vec![insertion("late", "late")]
    ));

    let mut request = prepared(&mut session, 7);
    request.generation = 8;
    assert!(!respond(
        &mut session,
        request,
        vec![insertion("old generation", "x")]
    ));

    let request = prepared(&mut session, 7);
    assert!(respond(
        &mut session,
        request,
        vec![insertion("cursor stale", "x")]
    ));
    session
        .execute(EditorCommand::Move {
            movement: rustrace_editor::Movement::Right,
            selecting: false,
        })
        .unwrap();
    assert!(!session.accept_completion().unwrap());

    let request = prepared(&mut session, 7);
    assert!(respond(
        &mut session,
        request,
        vec![insertion("document stale", "x")]
    ));
    session.execute(EditorCommand::NextBuffer).unwrap();
    assert!(!session.accept_completion().unwrap());

    let (other_fixture, mut other_session) = Fixture::new("abc");
    let old_request = {
        session.execute(EditorCommand::PreviousBuffer).unwrap();
        prepared(&mut session, 7)
    };
    assert!(!respond(
        &mut other_session,
        old_request,
        vec![insertion("wrong session/root", "x")]
    ));
    drop(other_fixture);
}

#[test]
fn command_ownership_and_missing_service_make_completion_inert_but_not_normal_editing() {
    let (_fixture, mut session) = Fixture::new("abc");
    assert!(
        !session.trigger_completion().unwrap(),
        "missing service is an optional no-op"
    );
    session.execute(EditorCommand::Insert('x')).unwrap();
    assert_eq!(session.workspace().active_buffer().text(), "xabc");

    session.effects.0.borrow_mut().command_active = true;
    assert!(session.prepare_completion_request(7).unwrap().is_none());
    session.effects.0.borrow_mut().command_active = false;
    let request = prepared(&mut session, 7);
    assert!(respond(
        &mut session,
        request,
        vec![insertion("blocked accept", "y")]
    ));
    session.effects.0.borrow_mut().command_active = true;
    assert!(!session.accept_completion().unwrap());
    assert_eq!(session.workspace().active_buffer().text(), "xabc");
}

#[test]
fn invalid_console_attempt_retires_pending_and_selected_completion_authority() {
    let (_pending_fixture, mut pending_session) = Fixture::new("abc");
    let pending_request = prepared(&mut pending_session, 7);

    assert!(
        pending_session
            .start_console_command("cargo run --bogus")
            .is_err()
    );
    assert!(
        !respond(
            &mut pending_session,
            pending_request,
            vec![insertion("late after rejected command", "x")]
        ),
        "a rejected direct console attempt must retire the pending request"
    );
    assert!(pending_session.completion_items().is_empty());

    let (_selected_fixture, mut selected_session) = Fixture::new("abc");
    let selected_request = prepared(&mut selected_session, 7);
    assert!(respond(
        &mut selected_session,
        selected_request,
        vec![insertion("selected before rejected command", "x")]
    ));
    assert_eq!(selected_session.completion_items().len(), 1);

    assert!(
        selected_session
            .start_console_command("cargo run --bogus")
            .is_err()
    );
    assert!(
        selected_session.completion_items().is_empty(),
        "a rejected direct console attempt must retire a selected result"
    );
    assert!(!selected_session.accept_completion().unwrap());
}

#[test]
fn journal_failure_precedes_completion_editor_mutation_and_stops_the_session() {
    for failed_event in ["lsp_completion_accepted", "file_edited"] {
        let (fixture, mut session) = Fixture::new("abc");
        let request = prepared(&mut session, 7);
        assert!(respond(
            &mut session,
            request,
            vec![insertion("failure", "x")]
        ));
        session.drain().unwrap();
        let journal_path = session
            .effects
            .0
            .borrow()
            .owner
            .display_path()
            .to_path_buf();
        let connection = rusqlite::Connection::open(&journal_path).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TRIGGER fail_completion BEFORE INSERT ON events
                 WHEN instr(CAST(NEW.payload AS TEXT), '\"type\":\"{failed_event}\"') > 0
                 BEGIN SELECT RAISE(ABORT, 'injected completion append failure'); END;"
            ))
            .unwrap();
        assert!(session.accept_completion().is_err(), "{failed_event}");
        assert_eq!(session.workspace().active_buffer().text(), "abc");
        assert_eq!(fs::read(fixture.0.join("main.rs")).unwrap(), b"abc");
        assert!(session.recovery_reason().is_some());
        assert!(session.execute(EditorCommand::Insert('!')).is_err());
        drop(session);
        connection
            .execute_batch("DROP TRIGGER fail_completion")
            .unwrap();
        drop(connection);

        let resumed =
            ProductionSession::resume(&fixture.0, MANIFEST, ResumeChoice::Resume).unwrap();
        assert_eq!(resumed.workspace().active_buffer().text(), "abc");
        resumed.quit().unwrap();
    }
}
