use std::cell::RefCell;
use std::rc::Rc;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rustrace::editor::{
    EditOrigin, EditorBuffer, EditorTransaction, EditorWidget, HighlightKind, Movement,
    NoopEditorEffects, RustHighlighter, SelectionState, Viewport, replay_transactions,
};
use rustrace_model::{DocumentId, Hash};
use tree_sitter::{InputEdit, Parser, Point};

type TransactionLog = Rc<RefCell<Vec<EditorTransaction>>>;
type RecordingEditor = EditorBuffer<Box<dyn FnMut(&EditorTransaction)>>;

fn recording_editor(initial: &str) -> (RecordingEditor, TransactionLog) {
    let transactions = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&transactions);
    let sink: Box<dyn FnMut(&EditorTransaction)> = Box::new(move |transaction| {
        recorded.borrow_mut().push(transaction.clone());
    });
    let editor = EditorBuffer::new(DocumentId::new("editor-spike").unwrap(), initial, sink);
    (editor, transactions)
}

#[test]
fn every_mutation_emits_a_replayable_transaction_with_its_logical_origin() {
    let initial = "aé\n";
    let (mut editor, transactions) = recording_editor(initial);

    editor.move_cursor(Movement::DocumentEnd, false);
    assert!(editor.insert_char('🦀').unwrap());
    assert!(editor.paste("\nfn pasted() {}").unwrap());
    editor.move_cursor(Movement::Left, true);
    assert!(editor.delete_backward().unwrap());
    assert!(editor.undo().unwrap());
    assert!(editor.redo().unwrap());

    let transactions = transactions.borrow();
    assert_eq!(
        transactions
            .iter()
            .map(|transaction| transaction.origin)
            .collect::<Vec<_>>(),
        [
            EditOrigin::Keyboard,
            EditOrigin::Paste,
            EditOrigin::Keyboard,
            EditOrigin::Undo,
            EditOrigin::Redo,
        ]
    );
    assert_eq!(
        transactions
            .iter()
            .map(|transaction| transaction.version_after)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
    let replayed = replay_transactions(
        DocumentId::new("editor-spike").unwrap(),
        initial,
        &transactions,
        NoopEditorEffects,
    )
    .unwrap();
    assert_eq!(replayed.text(), editor.text());
    assert_eq!(editor.text(), "aé\n🦀\nfn pasted() {");

    let json = serde_json::to_value(&transactions[1]).unwrap();
    assert_eq!(json["origin"], "paste");
    assert_eq!(json["edits"][0]["inserted_text"], "\nfn pasted() {}");
    assert!(json["edits"][0]["start_byte"].is_number());
}

#[test]
fn unicode_cursor_and_selection_follow_grapheme_boundaries() {
    let (mut editor, transactions) = recording_editor("a👩‍💻e\u{301}界\nsecond");

    editor.move_cursor(Movement::DocumentStart, false);
    editor.move_cursor(Movement::Right, false);
    assert_eq!(editor.cursor().char_index, 1);
    editor.move_cursor(Movement::Right, false);
    assert_eq!(editor.cursor().char_index, 4);
    editor.move_cursor(Movement::Right, false);
    assert_eq!(editor.cursor().char_index, 6);

    editor.move_cursor(Movement::Left, true);
    let selection = editor.selection().expect("selected combined grapheme");
    assert_eq!(editor.slice(selection.clone()), "e\u{301}");
    assert!(editor.insert_char('é').unwrap());
    assert_eq!(editor.text(), "a👩‍💻é界\nsecond");
    assert_eq!(transactions.borrow().len(), 1);

    editor.move_cursor(Movement::Down, false);
    assert_eq!(editor.cursor().line, 1);
    editor.move_cursor(Movement::Up, false);
    assert_eq!(editor.cursor().line, 0);
}

#[test]
fn insertion_before_a_combining_mark_normalizes_cursor_and_later_edits() {
    let initial = "\u{301}x";
    let (mut editor, transactions) = recording_editor(initial);

    assert!(editor.insert_char('e').unwrap());
    assert_eq!(editor.text(), "e\u{301}x");
    assert_eq!(editor.cursor().char_index, 2);
    assert_eq!(
        transactions.borrow()[0].selection_after,
        SelectionState::caret(3)
    );

    editor.move_cursor(Movement::Left, false);
    assert_eq!(editor.cursor().char_index, 0);
    editor.move_cursor(Movement::Right, false);
    assert_eq!(editor.cursor().char_index, 2);
    assert!(editor.delete_backward().unwrap());
    assert_eq!(editor.text(), "x");
    assert_eq!(
        transactions.borrow().last().unwrap().hash_after,
        editor.hash()
    );
}

#[test]
fn insertion_adjacent_to_a_zwj_sequence_normalizes_cursor_and_later_edits() {
    let initial = "\u{200d}💻!";
    let (mut editor, transactions) = recording_editor(initial);

    assert!(editor.insert_char('👩').unwrap());
    assert_eq!(editor.text(), "👩‍💻!");
    assert_eq!(editor.cursor().char_index, 3);
    assert_eq!(
        transactions.borrow()[0].selection_after,
        SelectionState::caret("👩‍💻".len() as u64)
    );

    editor.move_cursor(Movement::Left, false);
    assert_eq!(editor.cursor().char_index, 0);
    editor.move_cursor(Movement::Right, false);
    assert_eq!(editor.cursor().char_index, 3);
    assert!(editor.delete_backward().unwrap());
    assert_eq!(editor.text(), "!");
    assert_eq!(
        transactions.borrow().last().unwrap().hash_after,
        editor.hash()
    );
}

#[test]
fn replacement_that_joins_a_zwj_sequence_normalizes_cursor_and_replays() {
    let initial = "👩‍x💻!";
    let (mut editor, transactions) = recording_editor(initial);

    editor.move_cursor(Movement::Right, false);
    assert_eq!(editor.cursor().char_index, 2);
    editor.move_cursor(Movement::Right, false);
    editor.move_cursor(Movement::Left, true);
    assert_eq!(editor.slice(editor.selection().unwrap()), "x");
    assert!(editor.delete_backward().unwrap());
    assert_eq!(editor.text(), "👩‍💻!");
    assert_eq!(editor.cursor().char_index, 3);
    assert_eq!(
        transactions.borrow()[0].selection_after,
        SelectionState::caret("👩‍💻".len() as u64)
    );

    editor.move_cursor(Movement::Left, false);
    assert_eq!(editor.cursor().char_index, 0);
    editor.move_cursor(Movement::Right, false);
    assert_eq!(editor.cursor().char_index, 3);
    assert!(editor.delete_backward().unwrap());
    assert_eq!(editor.text(), "!");
    assert_eq!(
        transactions.borrow().last().unwrap().hash_after,
        editor.hash()
    );
}

#[test]
fn keyboard_insert_forward_and_backward_delete_all_emit_once() {
    let (mut editor, transactions) = recording_editor("ab");

    editor.move_cursor(Movement::Right, false);
    assert!(editor.insert_char('界').unwrap());
    assert!(editor.delete_backward().unwrap());
    assert!(editor.delete_forward().unwrap());
    assert!(!editor.delete_forward().unwrap());

    let transactions = transactions.borrow();
    assert_eq!(transactions.len(), 3);
    assert!(
        transactions
            .iter()
            .all(|transaction| transaction.origin == EditOrigin::Keyboard)
    );
    assert_eq!(editor.text(), "a");
}

#[test]
fn a_five_thousand_line_file_supports_vertical_and_horizontal_scrolling() {
    let source = (0..5_000)
        .map(|line| {
            format!(
                "fn function_{line:04}() {{ let descriptive_value = {line}; }} // a deliberately long Rust line"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let (mut editor, _) = recording_editor(&source);
    let mut viewport = Viewport::default();

    assert_eq!(editor.line_count(), 5_000);
    editor.move_cursor(Movement::DocumentEnd, false);
    viewport.follow_cursor(&editor, 24, 12);
    assert!(viewport.top_line() >= 4_988);
    assert!(viewport.left_column() > 0);

    let backend = TestBackend::new(24, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            frame.render_widget(EditorWidget::new(&editor, &viewport, &[]), frame.area());
        })
        .unwrap();

    let rendered_bottom_row = terminal
        .backend()
        .buffer()
        .content()
        .chunks(24)
        .nth(11)
        .unwrap()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(rendered_bottom_row, "berately long Rust line ");

    viewport.scroll_vertical(-20, editor.line_count(), 12);
    viewport.scroll_horizontal(-10);
    assert!(viewport.top_line() < 4_988);
    assert!(viewport.left_column() > 0);
}

#[test]
fn tree_sitter_rust_highlighting_classifies_real_syntax() {
    let source = "pub fn main() { let crab = \"🦀\"; // Unicode\n}\n";
    let mut highlighter = RustHighlighter::new().unwrap();

    let spans = highlighter.highlight(source).unwrap();
    let captured = |kind| {
        spans.iter().any(|span| {
            span.kind == kind
                && source
                    .get(span.byte_range.clone())
                    .is_some_and(|text| !text.is_empty())
        })
    };

    assert!(captured(HighlightKind::Keyword));
    assert!(captured(HighlightKind::Function));
    assert!(captured(HighlightKind::String));
    assert!(captured(HighlightKind::Comment));
}

#[test]
fn tree_sitter_incremental_parse_tolerates_and_recovers_from_invalid_rust() {
    let valid = "fn main() { let value = 1; }\n";
    let invalid = "fn main() { let value = ; }\n";
    let value_offset = valid.find('1').unwrap();
    let value_position = Point::new(0, value_offset);
    let language = tree_sitter_rust::LANGUAGE.into();
    let mut parser = Parser::new();
    parser.set_language(&language).unwrap();

    let mut tree = parser.parse(valid, None).unwrap();
    tree.edit(&InputEdit {
        start_byte: value_offset,
        old_end_byte: value_offset + 1,
        new_end_byte: value_offset,
        start_position: value_position,
        old_end_position: Point::new(0, value_offset + 1),
        new_end_position: value_position,
    });
    let invalid_tree = parser.parse(invalid, Some(&tree)).unwrap();

    assert!(invalid_tree.root_node().has_error());
    assert!(tree.changed_ranges(&invalid_tree).next().is_some());
    assert!(RustHighlighter::new().unwrap().highlight(invalid).is_ok());

    let mut edited_invalid_tree = invalid_tree.clone();
    edited_invalid_tree.edit(&InputEdit {
        start_byte: value_offset,
        old_end_byte: value_offset,
        new_end_byte: value_offset + 1,
        start_position: value_position,
        old_end_position: value_position,
        new_end_position: Point::new(0, value_offset + 1),
    });
    let repaired_tree = parser.parse(valid, Some(&edited_invalid_tree)).unwrap();

    assert!(!repaired_tree.root_node().has_error());
    assert!(
        edited_invalid_tree
            .changed_ranges(&repaired_tree)
            .next()
            .is_some()
    );
}

#[test]
fn ratatui_widget_renders_scrolled_highlighted_editor_state() {
    let (mut editor, _) = recording_editor("fn first() {}\nfn second() { let value = \"界\"; }");
    let mut highlighter = RustHighlighter::new().unwrap();
    let highlights = highlighter.highlight(&editor.text()).unwrap();
    let mut viewport = Viewport::default();
    editor.move_cursor(Movement::DocumentEnd, false);
    viewport.follow_cursor(&editor, 44, 1);
    let backend = TestBackend::new(44, 1);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| {
            frame.render_widget(
                EditorWidget::new(&editor, &viewport, &highlights),
                frame.area(),
            );
        })
        .unwrap();

    let rendered = terminal.backend().buffer().content();
    assert!(rendered.iter().any(|cell| cell.symbol() == "f"));
    assert!(rendered.iter().any(|cell| cell.symbol() == "界"));
}

#[test]
fn production_core_rejects_a_tampered_spike_transaction() {
    let (mut editor, transactions) = recording_editor("abc");
    editor.move_cursor(Movement::DocumentEnd, false);
    assert!(editor.insert_char('d').unwrap());
    let mut transaction = transactions.borrow()[0].clone();
    transaction.hash_after = Hash::zero();

    let (mut replay_editor, replay_transactions) = recording_editor("abc");
    replay_editor
        .set_selection(SelectionState::caret(3))
        .unwrap();
    assert!(replay_editor.apply_transaction(transaction).is_err());
    assert_eq!(replay_editor.text(), "abc");
    assert!(replay_transactions.borrow().is_empty());
}
