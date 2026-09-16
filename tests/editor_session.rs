use std::cell::RefCell;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use rustrace::config::PrimaryModifier;
use rustrace::editor::{
    EditOrigin, EditorEffectError, EditorEffects, EditorTransaction, Movement, NoopEditorEffects,
    SelectionState, replay_transactions,
};
use rustrace::tui::{
    DestructiveAction, EDITOR_KEY_HINTS, EditorCommand, EditorOutcome, EditorSession,
    EditorStorage, FileSystemStorage, SearchOutcome, SearchSummary, SessionInput,
    session_input_for_event as session_input_for_event_with_modifier,
};
use rustrace_model::{DocumentId, MAX_VECTOR_ITEMS};

#[derive(Clone, Default)]
struct RecordingEffects(Rc<RefCell<Vec<EditorTransaction>>>);

impl RecordingEffects {
    fn transactions(&self) -> Vec<EditorTransaction> {
        self.0.borrow().clone()
    }
}

impl EditorEffects for RecordingEffects {
    fn record_provenance(
        &mut self,
        transaction: &EditorTransaction,
    ) -> Result<(), EditorEffectError> {
        self.0.borrow_mut().push(transaction.clone());
        Ok(())
    }
}

fn session(initial: &str) -> (EditorSession<RecordingEffects>, RecordingEffects) {
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    (
        EditorSession::new(
            DocumentId::new("main").unwrap(),
            PathBuf::from("src/main.rs"),
            initial,
            effects,
        ),
        observed,
    )
}

fn execute(session: &mut EditorSession<RecordingEffects>, command: EditorCommand) -> EditorOutcome {
    session.execute(command).unwrap()
}

#[test]
fn selection_clipboard_edits_and_history_emit_once_and_replay() {
    let initial = "a👩‍💻e\u{301}界\nlast";
    let (mut session, observed) = session(initial);

    execute(
        &mut session,
        EditorCommand::Move {
            movement: Movement::Right,
            selecting: false,
        },
    );
    execute(
        &mut session,
        EditorCommand::Move {
            movement: Movement::Right,
            selecting: true,
        },
    );
    assert_eq!(
        execute(&mut session, EditorCommand::Copy),
        EditorOutcome::Copied
    );
    assert_eq!(session.clipboard(), "👩‍💻");
    assert!(observed.transactions().is_empty());

    assert_eq!(
        execute(&mut session, EditorCommand::Cut),
        EditorOutcome::Edited
    );
    assert_eq!(session.active_buffer().text(), "ae\u{301}界\nlast");
    assert_eq!(
        execute(&mut session, EditorCommand::Paste),
        EditorOutcome::Edited
    );
    assert_eq!(session.active_buffer().text(), initial);
    assert_eq!(
        execute(&mut session, EditorCommand::Undo),
        EditorOutcome::Edited
    );
    assert_eq!(
        execute(&mut session, EditorCommand::Redo),
        EditorOutcome::Edited
    );

    execute(
        &mut session,
        EditorCommand::Move {
            movement: Movement::DocumentEnd,
            selecting: false,
        },
    );
    assert_eq!(
        execute(&mut session, EditorCommand::Insert('!')),
        EditorOutcome::Edited
    );
    assert_eq!(
        execute(&mut session, EditorCommand::DeleteBackward),
        EditorOutcome::Edited
    );
    assert_eq!(
        execute(&mut session, EditorCommand::DeleteForward),
        EditorOutcome::NoChange
    );

    let transactions = observed.transactions();
    assert_eq!(
        transactions
            .iter()
            .map(|transaction| transaction.origin)
            .collect::<Vec<_>>(),
        [
            EditOrigin::Keyboard,
            EditOrigin::Paste,
            EditOrigin::Undo,
            EditOrigin::Redo,
            EditOrigin::Keyboard,
            EditOrigin::Keyboard,
        ]
    );
    assert_eq!(
        transactions
            .iter()
            .map(|transaction| transaction.version_after)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
    let replayed = replay_transactions(
        DocumentId::new("main").unwrap(),
        initial,
        &transactions,
        NoopEditorEffects,
    )
    .unwrap();
    assert_eq!(replayed.text(), session.active_buffer().text());
    assert_eq!(
        replayed.selection_state(),
        session.active_buffer().selection_state()
    );
}

#[test]
fn navigation_selects_across_lines_and_is_safe_at_document_and_tiny_page_bounds() {
    let (mut session, observed) = session("ab\n界\nlast");

    execute(
        &mut session,
        EditorCommand::Move {
            movement: Movement::DocumentStart,
            selecting: false,
        },
    );
    execute(
        &mut session,
        EditorCommand::PageUp {
            lines: 0,
            selecting: false,
        },
    );
    assert_eq!(session.active_buffer().cursor().line, 0);

    execute(
        &mut session,
        EditorCommand::PageDown {
            lines: 0,
            selecting: true,
        },
    );
    execute(
        &mut session,
        EditorCommand::PageDown {
            lines: 50,
            selecting: true,
        },
    );
    assert_eq!(session.active_buffer().cursor().line, 2);
    assert_eq!(
        session
            .active_buffer()
            .slice(session.active_buffer().selection().unwrap()),
        "ab\n界\n"
    );

    execute(
        &mut session,
        EditorCommand::Move {
            movement: Movement::LineEnd,
            selecting: false,
        },
    );
    assert_eq!(session.active_buffer().cursor().display_column, 4);
    execute(
        &mut session,
        EditorCommand::Move {
            movement: Movement::LineStart,
            selecting: false,
        },
    );
    assert_eq!(session.active_buffer().cursor().display_column, 0);

    session.follow_cursor(0, 0);
    assert!(session.active_viewport().top_line() <= 2);
    assert!(observed.transactions().is_empty());
}

#[test]
fn empty_document_navigation_deletion_and_clipboard_commands_are_safe_no_ops() {
    let (mut session, observed) = session("");
    for command in [
        EditorCommand::Move {
            movement: Movement::Left,
            selecting: false,
        },
        EditorCommand::Move {
            movement: Movement::Right,
            selecting: true,
        },
        EditorCommand::Move {
            movement: Movement::Up,
            selecting: false,
        },
        EditorCommand::Move {
            movement: Movement::Down,
            selecting: true,
        },
        EditorCommand::Move {
            movement: Movement::LineStart,
            selecting: false,
        },
        EditorCommand::Move {
            movement: Movement::LineEnd,
            selecting: true,
        },
        EditorCommand::PageUp {
            lines: 0,
            selecting: false,
        },
        EditorCommand::PageDown {
            lines: 0,
            selecting: true,
        },
        EditorCommand::DeleteBackward,
        EditorCommand::DeleteForward,
        EditorCommand::Copy,
        EditorCommand::Cut,
        EditorCommand::Paste,
        EditorCommand::Outdent,
    ] {
        assert_eq!(execute(&mut session, command), EditorOutcome::NoChange);
    }
    session.follow_cursor(0, 0);
    assert_eq!(
        session.active_buffer().selection_state(),
        SelectionState::caret(0)
    );
    assert_eq!(session.active_viewport().top_line(), 0);
    assert!(observed.transactions().is_empty());
}

#[test]
fn reversed_multiline_indent_and_outdent_are_single_replayable_transactions() {
    let initial = "one\n  two\n\tthree\nfour";
    let (mut session, observed) = session(initial);
    let selection_end = initial.find("four").unwrap() as u64;
    session
        .active_buffer_mut()
        .set_selection(SelectionState::new(selection_end, 0))
        .unwrap();

    assert_eq!(
        execute(&mut session, EditorCommand::Indent),
        EditorOutcome::Edited
    );
    assert_eq!(
        session.active_buffer().text(),
        "    one\n      two\n    \tthree\nfour"
    );
    let indented_selection = session.active_buffer().selection_state();
    assert!(indented_selection.anchor_byte > indented_selection.active_byte);
    assert_eq!(observed.transactions().len(), 1);
    assert_eq!(observed.transactions()[0].edits.len(), 3);

    assert_eq!(
        execute(&mut session, EditorCommand::Outdent),
        EditorOutcome::Edited
    );
    assert_eq!(session.active_buffer().text(), initial);
    let transactions = observed.transactions();
    assert_eq!(transactions.len(), 2);
    assert!(
        transactions
            .iter()
            .all(|transaction| transaction.origin == EditOrigin::Keyboard)
    );
    assert_eq!(transactions[1].edits.len(), 3);

    let replayed = replay_transactions(
        DocumentId::new("main").unwrap(),
        initial,
        &transactions,
        NoopEditorEffects,
    )
    .unwrap();
    assert_eq!(replayed.text(), initial);
    assert_eq!(
        replayed.selection_state(),
        session.active_buffer().selection_state()
    );
}

#[test]
fn caret_tab_uses_spaces_and_empty_selection_clipboard_actions_are_documented_no_ops() {
    let (mut editor_session, observed) = session("plain");

    assert_eq!(
        execute(&mut editor_session, EditorCommand::Copy),
        EditorOutcome::NoChange
    );
    assert_eq!(
        execute(&mut editor_session, EditorCommand::Cut),
        EditorOutcome::NoChange
    );
    assert_eq!(
        execute(&mut editor_session, EditorCommand::Paste),
        EditorOutcome::NoChange
    );
    assert_eq!(editor_session.clipboard(), "");
    assert!(observed.transactions().is_empty());

    execute(
        &mut editor_session,
        EditorCommand::Move {
            movement: Movement::DocumentEnd,
            selecting: false,
        },
    );
    assert_eq!(
        execute(&mut editor_session, EditorCommand::Indent),
        EditorOutcome::Edited
    );
    assert_eq!(editor_session.active_buffer().text(), "plain   ");
    assert_eq!(observed.transactions().len(), 1);

    let (mut unindented, unindented_effects) = session("plain");
    assert_eq!(
        execute(&mut unindented, EditorCommand::Outdent),
        EditorOutcome::NoChange
    );
    assert!(unindented_effects.transactions().is_empty());
}

#[test]
fn search_is_unicode_safe_wraps_once_and_reports_empty_and_missing_queries() {
    let source = "α crab β crab e\u{301}";
    let (mut session, observed) = session(source);

    assert_eq!(
        execute(&mut session, EditorCommand::Search("crab".to_owned())),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 3,
            end_byte: 7,
            wrapped: false,
        })
    );
    assert_eq!(
        execute(&mut session, EditorCommand::SearchNext),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 11,
            end_byte: 15,
            wrapped: false,
        })
    );
    assert_eq!(
        execute(&mut session, EditorCommand::SearchNext),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 3,
            end_byte: 7,
            wrapped: true,
        })
    );

    let selection_before_miss = session.active_buffer().selection_state();
    assert_eq!(
        execute(&mut session, EditorCommand::Search("missing".to_owned())),
        EditorOutcome::Search(SearchOutcome::NoMatch)
    );
    assert_eq!(
        session.active_buffer().selection_state(),
        selection_before_miss
    );
    assert_eq!(
        execute(&mut session, EditorCommand::Search(String::new())),
        EditorOutcome::Search(SearchOutcome::EmptyQuery)
    );
    assert_eq!(
        execute(&mut session, EditorCommand::Search("e\u{301}".to_owned())),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 16,
            end_byte: 19,
            wrapped: false,
        })
    );
    assert_eq!(
        execute(&mut session, EditorCommand::Search("CRAB".to_owned())),
        EditorOutcome::Search(SearchOutcome::NoMatch)
    );
    assert!(observed.transactions().is_empty());
}

#[test]
fn find_summary_and_selection_prefill_are_byte_exact_for_ascii_unicode_and_crlf() {
    let source = "cat α cat\r\n猫 cat 猫\r\n";
    let (mut session, observed) = session(source);

    assert_eq!(
        session.search_summary("cat"),
        SearchSummary {
            current: Some(1),
            total: 3,
        }
    );
    assert_eq!(
        execute(&mut session, EditorCommand::Search("cat".to_owned())),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 0,
            end_byte: 3,
            wrapped: false,
        })
    );
    assert_eq!(session.selected_text_for_find().as_deref(), Some("cat"));
    assert_eq!(
        session.search_summary("cat"),
        SearchSummary {
            current: Some(1),
            total: 3,
        }
    );

    assert_eq!(
        execute(&mut session, EditorCommand::SearchNext),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 7,
            end_byte: 10,
            wrapped: false,
        })
    );
    assert_eq!(
        session.search_summary("cat"),
        SearchSummary {
            current: Some(2),
            total: 3,
        }
    );
    assert_eq!(
        execute(&mut session, EditorCommand::SearchNext),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 16,
            end_byte: 19,
            wrapped: false,
        })
    );
    assert_eq!(
        execute(&mut session, EditorCommand::SearchNext),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 0,
            end_byte: 3,
            wrapped: true,
        })
    );

    session
        .active_buffer_mut()
        .set_selection(SelectionState::new(12, 19))
        .unwrap();
    assert_eq!(session.selected_text_for_find().as_deref(), Some("猫 cat"));
    session
        .active_buffer_mut()
        .set_selection(SelectionState::new(7, 16))
        .unwrap();
    assert_eq!(session.selected_text_for_find(), None);
    assert_eq!(
        session.search_summary("CAT"),
        SearchSummary {
            current: None,
            total: 0,
        }
    );
    assert_eq!(
        session.search_summary(""),
        SearchSummary {
            current: None,
            total: 0,
        }
    );
    assert!(observed.transactions().is_empty());
}

#[test]
fn replace_current_and_all_are_keyboard_transactions_with_exact_replay_and_unit_undo() {
    let initial = "α cat\r\ncat 猫 cat\r\n";
    let after_one = "α dog\r\ncat 猫 cat\r\n";
    let final_text = "α dog\r\ndog 猫 dog\r\n";
    let (mut session, observed) = session(initial);

    execute(&mut session, EditorCommand::Search("cat".to_owned()));
    assert_eq!(
        execute(
            &mut session,
            EditorCommand::ReplaceCurrent {
                query: "cat".to_owned(),
                replacement: "dog".to_owned(),
            },
        ),
        EditorOutcome::Replaced(1)
    );
    assert_eq!(session.active_buffer().text(), after_one);
    assert_eq!(
        session.active_buffer().selection_state(),
        SelectionState::new(8, 11),
        "replace must advance to the next match"
    );
    assert_eq!(
        execute(
            &mut session,
            EditorCommand::ReplaceAll {
                query: "cat".to_owned(),
                replacement: "dog".to_owned(),
            },
        ),
        EditorOutcome::Replaced(2)
    );
    assert_eq!(session.active_buffer().text(), final_text);

    let replacements = observed.transactions();
    assert_eq!(replacements.len(), 2);
    assert!(
        replacements
            .iter()
            .all(|transaction| transaction.origin == EditOrigin::Keyboard)
    );
    assert_eq!(replacements[0].selection_before, SelectionState::new(3, 6));
    assert_eq!(replacements[0].edits.len(), 1);
    assert_eq!(replacements[1].selection_before, SelectionState::new(8, 11));
    assert_eq!(replacements[1].edits.len(), 2);
    let replayed = replay_transactions(
        DocumentId::new("main").unwrap(),
        initial,
        &replacements,
        NoopEditorEffects,
    )
    .unwrap();
    assert_eq!(replayed.text(), final_text);
    assert_eq!(
        replayed.selection_state(),
        session.active_buffer().selection_state()
    );

    assert_eq!(
        execute(&mut session, EditorCommand::Undo),
        EditorOutcome::Edited
    );
    assert_eq!(session.active_buffer().text(), after_one);
    assert_eq!(
        execute(&mut session, EditorCommand::Undo),
        EditorOutcome::Edited
    );
    assert_eq!(session.active_buffer().text(), initial);
}

#[test]
fn replace_all_stays_bounded_at_and_above_the_transaction_edit_limit() {
    for match_count in [MAX_VECTOR_ITEMS, MAX_VECTOR_ITEMS + 1] {
        let initial = format!("{}\n", "a ".repeat(match_count));
        let final_text = format!("{}\n", "b ".repeat(match_count));
        let (mut session, observed) = session(&initial);

        assert_eq!(
            execute(
                &mut session,
                EditorCommand::ReplaceAll {
                    query: "a".to_owned(),
                    replacement: "b".to_owned(),
                },
            ),
            EditorOutcome::Replaced(match_count)
        );
        assert_eq!(session.active_buffer().text(), final_text);

        let transactions = observed.transactions();
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0].origin, EditOrigin::Keyboard);
        assert!(transactions[0].edits.len() <= MAX_VECTOR_ITEMS);
        let replayed = replay_transactions(
            DocumentId::new("main").unwrap(),
            &initial,
            &transactions,
            NoopEditorEffects,
        )
        .unwrap();
        assert_eq!(replayed.text(), final_text);
        assert_eq!(
            replayed.selection_state(),
            session.active_buffer().selection_state()
        );

        assert_eq!(
            execute(&mut session, EditorCommand::Undo),
            EditorOutcome::Edited
        );
        assert_eq!(session.active_buffer().text(), initial);
    }
}

#[test]
fn replace_is_a_no_op_for_empty_missing_or_unselected_matches() {
    let (mut session, observed) = session("cat cat");
    for query in ["", "missing", "cat"] {
        assert_eq!(
            execute(
                &mut session,
                EditorCommand::ReplaceCurrent {
                    query: query.to_owned(),
                    replacement: "dog".to_owned(),
                },
            ),
            EditorOutcome::NoChange
        );
    }
    for query in ["", "missing"] {
        assert_eq!(
            execute(
                &mut session,
                EditorCommand::ReplaceAll {
                    query: query.to_owned(),
                    replacement: "dog".to_owned(),
                },
            ),
            EditorOutcome::NoChange
        );
    }
    assert_eq!(session.active_buffer().text(), "cat cat");
    assert!(observed.transactions().is_empty());
}

#[test]
fn remembered_panel_query_keeps_f3_working_after_the_panel_closes() {
    let (mut session, observed) = session("cat crab cat");
    session
        .active_buffer_mut()
        .set_selection(SelectionState::caret(4))
        .unwrap();
    session.remember_search("cat".to_owned());

    assert_eq!(
        execute(&mut session, EditorCommand::SearchNext),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 9,
            end_byte: 12,
            wrapped: false,
        })
    );
    assert!(observed.transactions().is_empty());
}

#[test]
fn search_wrap_finds_a_match_that_straddles_the_caret() {
    let (mut session, observed) = session("needle");

    session
        .active_buffer_mut()
        .set_selection(SelectionState::caret(2))
        .unwrap();
    assert_eq!(
        execute(&mut session, EditorCommand::Search("needle".to_owned())),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 0,
            end_byte: 6,
            wrapped: true,
        })
    );

    session
        .active_buffer_mut()
        .set_selection(SelectionState::caret(6))
        .unwrap();
    assert_eq!(
        execute(&mut session, EditorCommand::Search("needle".to_owned())),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 0,
            end_byte: 6,
            wrapped: true,
        })
    );
    assert!(observed.transactions().is_empty());
}

#[test]
fn g3_m1_search_next_after_unicode_edit_uses_current_caret() {
    let (mut session, observed) = session("abc a");
    execute(&mut session, EditorCommand::Search("a".into()));
    execute(&mut session, EditorCommand::Insert('é'));
    assert_eq!(session.active_buffer().text(), "ébc a");
    assert_eq!(
        execute(&mut session, EditorCommand::SearchNext),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 5,
            end_byte: 6,
            wrapped: false,
        })
    );
    assert_eq!(
        execute(&mut session, EditorCommand::SearchNext),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 5,
            end_byte: 6,
            wrapped: true,
        })
    );
    assert_eq!(observed.transactions().len(), 1);
}

#[test]
fn g3_m1_search_offsets_follow_undo_and_redo_versions() {
    for redo in [false, true] {
        let (mut session, _) = session(if redo { "abc" } else { "ébc" });
        if redo {
            execute(&mut session, EditorCommand::Search("a".into()));
            execute(&mut session, EditorCommand::Insert('é'));
            execute(&mut session, EditorCommand::Undo);
            execute(&mut session, EditorCommand::SearchNext);
            execute(&mut session, EditorCommand::Redo);
        } else {
            session
                .active_buffer_mut()
                .set_selection(SelectionState::new(0, 2))
                .unwrap();
            execute(&mut session, EditorCommand::Insert('a'));
            execute(&mut session, EditorCommand::Search("a".into()));
            execute(&mut session, EditorCommand::Undo);
        }
        assert_eq!(session.active_buffer().text(), "ébc");
        let before = session.active_buffer().selection_state();
        assert_eq!(
            execute(&mut session, EditorCommand::SearchNext),
            EditorOutcome::Search(SearchOutcome::NoMatch)
        );
        assert_eq!(session.active_buffer().selection_state(), before);
    }
}

#[test]
fn g3_m1_search_query_and_match_version_are_independent_per_buffer() {
    let (mut session, _) = session("abc a");
    execute(&mut session, EditorCommand::Search("a".into()));
    session.open_buffer(
        DocumentId::new("second").unwrap(),
        PathBuf::from("second.rs"),
        "x x",
        RecordingEffects::default(),
    );
    execute(&mut session, EditorCommand::Search("x".into()));
    execute(&mut session, EditorCommand::PreviousBuffer);
    execute(&mut session, EditorCommand::Insert('é'));
    execute(&mut session, EditorCommand::NextBuffer);
    assert_eq!(
        execute(&mut session, EditorCommand::SearchNext),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 2,
            end_byte: 3,
            wrapped: false
        })
    );
    execute(&mut session, EditorCommand::PreviousBuffer);
    assert_eq!(
        execute(&mut session, EditorCommand::SearchNext),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 5,
            end_byte: 6,
            wrapped: false
        })
    );
    execute(&mut session, EditorCommand::NextBuffer);
    assert_eq!(
        execute(&mut session, EditorCommand::SearchNext),
        EditorOutcome::Search(SearchOutcome::Match {
            start_byte: 0,
            end_byte: 1,
            wrapped: true
        })
    );
}

#[test]
fn clipboard_edits_expand_programmatic_selection_to_grapheme_boundaries() {
    let initial = "ae\u{301}b";
    let (mut session, observed) = session(initial);
    session
        .active_buffer_mut()
        .set_selection(SelectionState::new(2, 4))
        .unwrap();

    assert_eq!(
        execute(&mut session, EditorCommand::Copy),
        EditorOutcome::Copied
    );
    assert_eq!(session.clipboard(), "e\u{301}");
    assert!(observed.transactions().is_empty());

    assert_eq!(
        execute(&mut session, EditorCommand::Cut),
        EditorOutcome::Edited
    );
    assert_eq!(session.active_buffer().text(), "ab");
    let transactions = observed.transactions();
    assert_eq!(transactions.len(), 1);
    assert_eq!(transactions[0].edits[0].start_byte, 1);
    assert_eq!(transactions[0].edits[0].end_byte, 4);
    let replayed = replay_transactions(
        DocumentId::new("main").unwrap(),
        initial,
        &transactions,
        NoopEditorEffects,
    )
    .unwrap();
    assert_eq!(replayed.text(), "ab");
}

#[derive(Default)]
struct MemoryStorage {
    writes: Vec<(PathBuf, Vec<u8>)>,
    fail: bool,
}

impl EditorStorage for MemoryStorage {
    fn write(&mut self, path: &Path, contents: &[u8]) -> io::Result<()> {
        if self.fail {
            return Err(io::Error::other("injected save failure"));
        }
        self.writes.push((path.to_owned(), contents.to_vec()));
        Ok(())
    }
}

#[test]
fn buffers_switch_deterministically_and_preserve_independent_state_and_history() {
    let (mut session, main_effects) = session("main");
    execute(
        &mut session,
        EditorCommand::Move {
            movement: Movement::DocumentEnd,
            selecting: false,
        },
    );
    execute(&mut session, EditorCommand::Insert('!'));
    let main_selection = session.active_buffer().selection_state();

    let lib_effects = RecordingEffects::default();
    let observed_lib = lib_effects.clone();
    assert_eq!(
        session.open_buffer(
            DocumentId::new("lib").unwrap(),
            PathBuf::from("src/lib.rs"),
            "library",
            lib_effects,
        ),
        1
    );
    assert_eq!(session.buffer_count(), 2);
    assert_eq!(session.active_index(), 1);
    execute(
        &mut session,
        EditorCommand::Move {
            movement: Movement::DocumentEnd,
            selecting: false,
        },
    );
    execute(&mut session, EditorCommand::Insert('?'));
    assert_eq!(session.active_buffer().text(), "library?");

    assert_eq!(
        execute(&mut session, EditorCommand::PreviousBuffer),
        EditorOutcome::BufferSwitched
    );
    assert_eq!(session.active_index(), 0);
    assert_eq!(session.active_buffer().text(), "main!");
    assert_eq!(session.active_buffer().selection_state(), main_selection);
    assert!(session.active_buffer().can_undo());
    assert_eq!(
        execute(&mut session, EditorCommand::Undo),
        EditorOutcome::Edited
    );
    assert_eq!(session.active_buffer().text(), "main");
    assert_eq!(session.buffer(1).unwrap().text(), "library?");
    assert!(session.buffer(1).unwrap().can_undo());

    assert_eq!(
        execute(&mut session, EditorCommand::NextBuffer),
        EditorOutcome::BufferSwitched
    );
    assert_eq!(session.active_index(), 1);
    assert_eq!(
        execute(&mut session, EditorCommand::Undo),
        EditorOutcome::Edited
    );
    assert_eq!(session.active_buffer().text(), "library");
    assert_eq!(main_effects.transactions().len(), 2);
    assert_eq!(observed_lib.transactions().len(), 2);
}

#[test]
fn save_writes_exact_unicode_bytes_and_only_success_marks_the_buffer_clean() {
    let (mut session, _) = session("fn main() {}\n");
    let mut storage = MemoryStorage::default();
    assert!(!session.is_active_dirty());

    execute(
        &mut session,
        EditorCommand::Move {
            movement: Movement::DocumentEnd,
            selecting: false,
        },
    );
    execute(
        &mut session,
        EditorCommand::PasteExternal("// 🦀\r\n".to_owned()),
    );
    assert!(session.is_active_dirty());

    storage.fail = true;
    let failure = session.save_active(&mut storage).unwrap_err();
    assert_eq!(failure.kind(), io::ErrorKind::Other);
    assert!(session.is_active_dirty());
    assert!(storage.writes.is_empty());

    storage.fail = false;
    session.save_active(&mut storage).unwrap();
    assert!(!session.is_active_dirty());
    assert_eq!(
        storage.writes,
        [(
            PathBuf::from("src/main.rs"),
            "fn main() {}\n// 🦀\r\n".as_bytes().to_vec(),
        )]
    );

    execute(&mut session, EditorCommand::Undo);
    assert!(session.is_active_dirty());
    execute(&mut session, EditorCommand::Redo);
    assert!(!session.is_active_dirty());
}

#[test]
fn filesystem_storage_persists_exact_bytes() {
    let unique = format!("rustrace-t3-2-storage-{}", std::process::id());
    let path = std::env::temp_dir().join(unique);
    let _ = std::fs::remove_file(&path);
    let mut storage = FileSystemStorage;
    let contents = "exact\0🦀\r\n".as_bytes();

    storage.write(&path, contents).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), contents);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn dirty_close_and_quit_require_confirmation_and_cancel_preserves_buffers() {
    let (mut clean, _) = session("clean");
    clean.open_buffer(
        DocumentId::new("clean-lib").unwrap(),
        PathBuf::from("src/lib.rs"),
        "clean lib",
        RecordingEffects::default(),
    );
    assert_eq!(
        execute(&mut clean, EditorCommand::CloseActive),
        EditorOutcome::BufferClosed
    );
    assert!(!clean.confirmation_pending());
    assert_eq!(
        execute(&mut clean, EditorCommand::RequestQuit),
        EditorOutcome::Quit
    );

    let (mut session, _) = session("main");
    session.open_buffer(
        DocumentId::new("lib").unwrap(),
        PathBuf::from("src/lib.rs"),
        "lib",
        RecordingEffects::default(),
    );
    execute(
        &mut session,
        EditorCommand::Move {
            movement: Movement::DocumentEnd,
            selecting: false,
        },
    );
    execute(&mut session, EditorCommand::Insert('!'));

    assert_eq!(
        execute(&mut session, EditorCommand::CloseActive),
        EditorOutcome::ConfirmationRequired(DestructiveAction::CloseBuffer)
    );
    assert_eq!(session.buffer_count(), 2);
    assert!(session.confirmation_pending());
    assert_eq!(
        execute(&mut session, EditorCommand::CancelDiscard),
        EditorOutcome::Cancelled
    );
    assert_eq!(session.buffer_count(), 2);
    assert_eq!(session.active_buffer().text(), "lib!");

    execute(&mut session, EditorCommand::CloseActive);
    assert_eq!(
        execute(&mut session, EditorCommand::ConfirmDiscard),
        EditorOutcome::BufferClosed
    );
    assert_eq!(session.buffer_count(), 1);
    assert_eq!(session.active_buffer().text(), "main");

    execute(&mut session, EditorCommand::Insert('?'));
    assert_eq!(
        execute(&mut session, EditorCommand::RequestQuit),
        EditorOutcome::ConfirmationRequired(DestructiveAction::Quit)
    );
    assert_eq!(
        execute(&mut session, EditorCommand::CancelDiscard),
        EditorOutcome::Cancelled
    );
    assert_eq!(session.active_buffer().text(), "?main");
    execute(&mut session, EditorCommand::RequestQuit);
    assert_eq!(
        execute(&mut session, EditorCommand::ConfirmDiscard),
        EditorOutcome::Quit
    );
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn session_input_for_event(
    event: Event,
    page_lines: usize,
    confirmation_pending: bool,
) -> Option<SessionInput> {
    session_input_for_event_with_modifier(
        event,
        page_lines,
        confirmation_pending,
        PrimaryModifier::Control,
    )
}

#[test]
fn legacy_function_keys_switch_buffers_on_press_and_repeat_but_not_release() {
    for (code, command) in [
        (KeyCode::F(5), EditorCommand::PreviousBuffer),
        (KeyCode::F(6), EditorCommand::NextBuffer),
    ] {
        assert_eq!(
            session_input_for_event(key(code, KeyModifiers::NONE), 12, false),
            Some(SessionInput::Command(command.clone()))
        );
        assert_eq!(
            session_input_for_event(
                Event::Key(KeyEvent::new_with_kind(
                    code,
                    KeyModifiers::NONE,
                    KeyEventKind::Repeat,
                )),
                12,
                false,
            ),
            Some(SessionInput::Command(command))
        );
        assert_eq!(
            session_input_for_event(
                Event::Key(KeyEvent::new_with_kind(
                    code,
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                )),
                12,
                false,
            ),
            None
        );
    }
}

#[test]
fn crossterm_events_map_to_explicit_session_commands_without_duplicate_editing_logic() {
    assert_eq!(
        session_input_for_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL), 12, false,),
        Some(SessionInput::Command(EditorCommand::Copy))
    );
    assert_eq!(
        session_input_for_event(key(KeyCode::Char('/'), KeyModifiers::CONTROL), 12, false,),
        Some(SessionInput::Command(EditorCommand::ToggleComment))
    );
    assert_eq!(
        session_input_for_event(key(KeyCode::BackTab, KeyModifiers::SHIFT), 12, false),
        Some(SessionInput::Command(EditorCommand::Outdent))
    );
    assert_eq!(
        session_input_for_event(key(KeyCode::PageDown, KeyModifiers::SHIFT), 0, false),
        Some(SessionInput::Command(EditorCommand::PageDown {
            lines: 0,
            selecting: true,
        }))
    );
    assert_eq!(
        session_input_for_event(Event::Paste("one\ntwo".to_owned()), 12, false),
        Some(SessionInput::Command(EditorCommand::PasteExternal(
            "one\ntwo".to_owned(),
        )))
    );
    assert_eq!(
        session_input_for_event(key(KeyCode::Char('s'), KeyModifiers::CONTROL), 12, false),
        Some(SessionInput::Save)
    );
    assert_eq!(
        session_input_for_event(key(KeyCode::Char('f'), KeyModifiers::CONTROL), 12, false),
        Some(SessionInput::BeginFind)
    );
    assert_eq!(
        session_input_for_event(key(KeyCode::Tab, KeyModifiers::CONTROL), 12, false),
        Some(SessionInput::Command(EditorCommand::NextBuffer))
    );
    assert_eq!(
        session_input_for_event(key(KeyCode::BackTab, KeyModifiers::CONTROL), 12, false),
        Some(SessionInput::Command(EditorCommand::PreviousBuffer))
    );

    let release = Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ));
    assert_eq!(session_input_for_event(release, 12, false), None);
    let repeat = Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
        KeyEventKind::Repeat,
    ));
    assert_eq!(
        session_input_for_event(repeat, 12, false),
        Some(SessionInput::Command(EditorCommand::Insert('x')))
    );

    assert_eq!(
        session_input_for_event(key(KeyCode::Char('y'), KeyModifiers::NONE), 12, true),
        Some(SessionInput::Command(EditorCommand::ConfirmDiscard))
    );
    assert_eq!(
        session_input_for_event(key(KeyCode::Esc, KeyModifiers::NONE), 12, true),
        Some(SessionInput::Command(EditorCommand::CancelDiscard))
    );
    for binding in [
        "Arrows",
        "Home",
        "End",
        "PageUp",
        "PageDown",
        "Shift+movement",
        "Tab indent",
        "Shift-Tab",
        "Ctrl-S",
        "Ctrl-F",
        "F3",
        "Ctrl-C",
        "Ctrl-X",
        "Ctrl-V",
        "Ctrl-Z",
        "Ctrl-Y",
        "Ctrl-/",
        "Ctrl-Tab",
        "Ctrl-BackTab",
        "Ctrl-W",
        "Ctrl-Q",
    ] {
        assert!(
            EDITOR_KEY_HINTS.contains(binding),
            "missing hint for {binding}"
        );
    }
}

#[test]
fn command_mode_maps_super_and_control_shortcuts_to_identical_inputs() {
    let shortcuts = [
        KeyCode::Char(' '),
        KeyCode::Char('q'),
        KeyCode::Char('w'),
        KeyCode::Char('c'),
        KeyCode::Char('x'),
        KeyCode::Char('v'),
        KeyCode::Char('z'),
        KeyCode::Char('y'),
        KeyCode::Char('a'),
        KeyCode::Char('/'),
        KeyCode::Tab,
        KeyCode::BackTab,
        KeyCode::Char('s'),
        KeyCode::Char('f'),
    ];

    for code in shortcuts {
        let control = session_input_for_event_with_modifier(
            key(code, KeyModifiers::CONTROL),
            12,
            false,
            PrimaryModifier::Command,
        );
        let command = session_input_for_event_with_modifier(
            key(code, KeyModifiers::SUPER),
            12,
            false,
            PrimaryModifier::Command,
        );
        assert!(control.is_some(), "Control-{code:?} stopped mapping");
        assert_eq!(command, control, "Super-{code:?} diverged from Control");
        assert_eq!(
            session_input_for_event_with_modifier(
                key(code, KeyModifiers::SUPER),
                12,
                false,
                PrimaryModifier::Control,
            ),
            None,
            "Control mode accepted Super-{code:?}"
        );
    }
}

#[test]
fn terminal_owned_super_shortcuts_remain_accepted_when_delivered() {
    for (code, expected) in [
        (KeyCode::Char('q'), EditorCommand::RequestQuit),
        (KeyCode::Char('w'), EditorCommand::CloseActive),
        (KeyCode::Char('a'), EditorCommand::SelectAll),
        (KeyCode::Char('c'), EditorCommand::Copy),
        (KeyCode::Char('x'), EditorCommand::Cut),
        (KeyCode::Char('v'), EditorCommand::Paste),
    ] {
        assert_eq!(
            session_input_for_event_with_modifier(
                key(code, KeyModifiers::SUPER),
                12,
                false,
                PrimaryModifier::Command,
            ),
            Some(SessionInput::Command(expected)),
            "delivered Super-{code:?} stopped dispatching",
        );
    }
}

#[test]
fn find_panel_shortcut_requires_the_exact_effective_primary_modifier() {
    for (primary_modifier, accepted) in [
        (PrimaryModifier::Control, KeyModifiers::CONTROL),
        (PrimaryModifier::Command, KeyModifiers::CONTROL),
        (PrimaryModifier::Command, KeyModifiers::SUPER),
    ] {
        assert_eq!(
            session_input_for_event_with_modifier(
                key(KeyCode::Char('f'), accepted),
                12,
                false,
                primary_modifier,
            ),
            Some(SessionInput::BeginFind)
        );
        for extra in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            assert_eq!(
                session_input_for_event_with_modifier(
                    key(KeyCode::Char('f'), accepted | extra),
                    12,
                    false,
                    primary_modifier,
                ),
                None
            );
        }
    }
}

#[test]
fn comment_toggle_routes_through_the_editor_session_as_one_keyboard_edit() {
    let initial = "    α();\r\n        β();";
    let (mut session, observed) = session(initial);
    session
        .active_buffer_mut()
        .set_selection(SelectionState::new(0, initial.len() as u64))
        .unwrap();

    assert_eq!(
        execute(&mut session, EditorCommand::ToggleComment),
        EditorOutcome::Edited
    );
    assert_eq!(
        session.active_buffer().text(),
        "    // α();\r\n    //     β();"
    );
    let transactions = observed.transactions();
    assert_eq!(transactions.len(), 1);
    assert_eq!(transactions[0].origin, EditOrigin::Keyboard);
    assert_eq!(transactions[0].edits.len(), 2);
}

#[test]
fn auto_pair_overtype_is_a_selection_change_and_newline_is_one_keyboard_edit() {
    let (mut editor, observed) = session("fn α() {");
    execute(
        &mut editor,
        EditorCommand::Move {
            movement: Movement::DocumentEnd,
            selecting: false,
        },
    );
    assert_eq!(
        execute(&mut editor, EditorCommand::Insert('\n')),
        EditorOutcome::Edited
    );
    assert_eq!(editor.active_buffer().text(), "fn α() {\n    ");
    assert_eq!(observed.transactions().len(), 1);
    assert_eq!(observed.transactions()[0].origin, EditOrigin::Keyboard);

    let (mut paired, observed) = session("");
    assert_eq!(
        execute(&mut paired, EditorCommand::Insert('[')),
        EditorOutcome::Edited
    );
    assert_eq!(paired.active_buffer().text(), "[]");
    assert_eq!(
        execute(&mut paired, EditorCommand::Insert(']')),
        EditorOutcome::SelectionChanged
    );
    assert_eq!(paired.active_buffer().text(), "[]");
    assert_eq!(
        paired.active_buffer().selection_state(),
        SelectionState::caret(2)
    );
    assert_eq!(observed.transactions().len(), 1);
}

#[test]
fn multiline_string_closing_quote_is_one_keyboard_transaction_in_a_session() {
    for (case, initial) in [
        (
            "ordinary LF after whitespace",
            "let s = \"escaped \\\" quote\nclose ",
        ),
        (
            "ordinary CRLF after punctuation",
            "let s = \"escaped \\\\ path\r\nclose,",
        ),
        (
            "hash raw LF after whitespace",
            "let s = r#\"first \"quoted\"\nclose ",
        ),
        (
            "zero-hash raw CRLF after punctuation",
            "let s = r\"first\r\nclose,",
        ),
    ] {
        let (mut session, observed) = session(initial);
        execute(
            &mut session,
            EditorCommand::Move {
                movement: Movement::DocumentEnd,
                selecting: false,
            },
        );

        assert_eq!(
            execute(&mut session, EditorCommand::Insert('"')),
            EditorOutcome::Edited,
            "{case}"
        );
        assert_eq!(
            session.active_buffer().text(),
            format!("{initial}\""),
            "{case}"
        );
        let transactions = observed.transactions();
        assert_eq!(transactions.len(), 1, "{case}");
        assert_eq!(transactions[0].origin, EditOrigin::Keyboard, "{case}");
        assert_eq!(transactions[0].edits.len(), 1, "{case}");
        assert_eq!(transactions[0].edits[0].inserted_text, "\"", "{case}");
    }
}

#[test]
fn switching_buffers_clears_transient_auto_pair_state() {
    let (mut session, observed) = session("");
    session.execute(EditorCommand::Insert('(')).unwrap();
    session.open_buffer(
        DocumentId::new("other").unwrap(),
        PathBuf::from("src/other.rs"),
        "",
        observed.clone(),
    );
    assert_eq!(
        session.execute(EditorCommand::PreviousBuffer).unwrap(),
        EditorOutcome::BufferSwitched
    );

    assert_eq!(
        session.execute(EditorCommand::Insert(')')).unwrap(),
        EditorOutcome::Edited
    );
    assert_eq!(session.active_buffer().text(), "())");
    assert_eq!(observed.transactions().len(), 2);
}
