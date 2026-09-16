use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> (Self, ProductionSession) {
        Self::new_with_source("A")
    }

    fn new_with_source(source: &str) -> (Self, ProductionSession) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rustrace-clipboard-authority-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        fs::write(root.join("main.rs"), source).unwrap();
        let manifest = br#"format_version = 1
course_id = "course"
assignment_id = "clipboard"
assignment_version = "v1"
title = "Clipboard authority"
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
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn programmatic_paste_and_unknown_are_gated_before_noop_limits_or_mutation() {
    for mode in [
        "empty-paste",
        "empty-unknown",
        "oversize",
        "transaction",
        "paste",
    ] {
        let (fixture, session) = Fixture::new();
        let mut editor = crate::tui::EditorSession::new(
            session.workspace().active_document_id().clone(),
            fixture.0.join("main.rs"),
            "A",
            session.effects.clone(),
        );
        let count = session.health().unwrap().events;
        let buffer = editor.active_buffer_mut();
        let result = match mode {
            "empty-paste" => buffer.paste(""),
            "empty-unknown" => {
                buffer.apply_edits(EditOrigin::Unknown, Vec::new(), SelectionState::caret(0))
            }
            "oversize" => buffer.paste(&"PRIVATE_PROGRAMMATIC".repeat(MAX_INSERTED_TEXT_BYTES)),
            "transaction" => {
                let mut transaction = buffer
                    .preview_paste("PRIVATE_PROGRAMMATIC")
                    .unwrap()
                    .unwrap();
                transaction.origin = EditOrigin::Unknown;
                buffer.apply_transaction(transaction)
            }
            "paste" => buffer.paste("PRIVATE_PROGRAMMATIC"),
            _ => unreachable!(),
        };
        assert!(
            result.is_err(),
            "{mode}: enum/no-op cannot bypass production authority"
        );
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains(PASTE_BLOCKED_WARNING),
            "{mode}: must use the direct warning"
        );
        assert!(!error.contains("PRIVATE_PROGRAMMATIC"));
        assert_eq!(buffer.text(), "A");
        assert_eq!(buffer.version(), 0);
        assert_eq!(buffer.selection_state(), SelectionState::caret(0));
        assert!(!buffer.can_undo());
        assert_eq!(session.workspace().active_buffer().text(), "A");
        assert_eq!(session.health().unwrap().events, count + 1);
        assert_eq!(fs::read(fixture.0.join("main.rs")).unwrap(), b"A");
    }
}

#[test]
fn direct_workspace_commands_cannot_mint_authority_or_skip_empty_attempts() {
    for command in [
        EditorCommand::PasteExternal(String::new()),
        EditorCommand::Paste,
        EditorCommand::Copy,
        EditorCommand::Cut,
    ] {
        let (_fixture, mut session) = Fixture::new();
        let count = session.health().unwrap().events;
        assert!(
            session
                .workspace_mut()
                .execute_editor(command)
                .unwrap_err()
                .to_string()
                .contains(PASTE_BLOCKED_WARNING)
        );
        assert_eq!(session.workspace().active_buffer().text(), "A");
        assert_eq!(session.health().unwrap().events, count + 1);
        assert!(session.effects.0.borrow().live_clipboard.is_none());
    }
}

#[test]
fn terminal_event_hook_clears_live_authority_without_adding_a_finalizer() {
    for finalized in [false, true] {
        let (_fixture, mut session) = Fixture::new();
        session.execute(EditorCommand::SelectAll).unwrap();
        session.execute(EditorCommand::Copy).unwrap();
        let mut authority = session.effects.0.borrow_mut();
        assert!(authority.live_clipboard.is_some());
        let hash = authority.replay.as_ref().unwrap().current_workspace_hash();
        let terminal = if finalized {
            Event::SubmissionFinalized(SubmissionFinalized {
                final_workspace_hash: hash,
                event_count: authority.sequence + 1,
                clean: true,
                warnings: Vec::new(),
            })
        } else {
            Event::SessionEnded(SessionEnded {
                final_workspace_hash: hash,
            })
        };
        authority.append(terminal).unwrap();
        assert!(authority.live_clipboard.is_none());
        assert!(authority.pending_paste.is_none());
        let count = authority.sequence;
        drop(authority);
        assert!(session.execute(EditorCommand::Paste).is_err());
        assert_eq!(session.effects.0.borrow().sequence, count);
        assert_eq!(session.workspace().active_buffer().text(), "A");
    }
}

fn normalize_journal_identity(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                if matches!(
                    key.as_str(),
                    "session_id"
                        | "event_hash"
                        | "previous_event_hash"
                        | "document_id"
                        | "monotonic_millis"
                        | "wall_clock_utc"
                ) {
                    *value = serde_json::Value::String("normalized".into());
                } else {
                    normalize_journal_identity(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                normalize_journal_identity(value);
            }
        }
        _ => {}
    }
}

fn matching_paste_run(source: &str, delivered: &str, bracketed: bool) -> Vec<serde_json::Value> {
    let (fixture, mut session) = Fixture::new_with_source(source);
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Copy).unwrap();
    session.execute(EditorCommand::Insert('!')).unwrap();
    session.execute(EditorCommand::SelectAll).unwrap();
    let outcome = if bracketed {
        session.paste_terminal(delivered)
    } else {
        session.execute(EditorCommand::Paste)
    };
    assert_eq!(outcome.unwrap(), EditorOutcome::Edited);
    assert_eq!(session.workspace().active_buffer().text(), source);

    let (initial, events) = fixture.reopen();
    let mut replay = ReplayEngine::from_initial_checkpoint(initial).unwrap();
    for event in &events {
        replay.apply(event).unwrap();
    }
    assert_eq!(
        replay
            .workspace_state()
            .file(&WorkspacePath::new("main.rs").unwrap())
            .unwrap(),
        source.as_bytes(),
    );

    events
        .into_iter()
        .map(|event| {
            let mut value = serde_json::to_value(event).unwrap();
            normalize_journal_identity(&mut value);
            value
        })
        .collect()
}

#[test]
fn matching_terminal_paste_has_the_exact_ctrl_v_journal_and_replay() {
    for (source, delivered) in [
        ("same bytes", "same bytes"),
        ("first\nsecond\n", "first\rsecond\r"),
        ("first\nsecond\n", "first\r\nsecond\r\n"),
    ] {
        let control_v = matching_paste_run(source, source, false);
        let bracketed = matching_paste_run(source, delivered, true);
        assert_eq!(bracketed, control_v, "delivered bytes: {delivered:?}");
        assert!(
            !serde_json::to_string(&bracketed).unwrap().contains("\\r"),
            "delivered terminal bytes entered the journal"
        );
    }
}

#[test]
fn terminal_paste_rejects_foreign_missing_and_invalidated_clipboards() {
    for mode in ["foreign", "missing", "invalidated"] {
        let (fixture, mut session) = Fixture::new();
        if mode != "missing" {
            session.execute(EditorCommand::SelectAll).unwrap();
            session.execute(EditorCommand::Copy).unwrap();
        }
        if mode == "invalidated" {
            session.clear_clipboard();
        }
        let before = session.workspace().active_buffer().text();
        let count = session.health().unwrap().events;
        let delivered = if mode == "foreign" { "foreign" } else { "A" };
        let error = session.paste_terminal(delivered).unwrap_err().to_string();
        assert_eq!(error, PASTE_BLOCKED_WARNING, "{mode}");
        assert_eq!(session.workspace().active_buffer().text(), before, "{mode}");
        assert_eq!(session.health().unwrap().events, count + 1, "{mode}");
        let event = fixture.reopen().1.pop().unwrap().event;
        assert_eq!(
            event,
            Event::PasteRejected(PasteRejected {
                reason: PasteRejectionReason::ExternalInput,
                channel: PasteInputChannel::TerminalBracketed,
            }),
            "{mode}",
        );
    }
}
