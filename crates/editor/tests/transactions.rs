use std::cell::RefCell;
use std::rc::Rc;

use rustrace_editor::{
    EditOrigin, EditorBuffer, EditorEffectError, EditorEffects, EditorTransaction,
    MAX_TRANSACTION_JSON_BYTES, Movement, NoopEditorEffects, ReplayError, SelectionState,
    TransactionDecodeError, TransactionEncodeError, TransactionError, decode_transaction,
    document_hash, encode_transaction, replay_transactions,
};
use rustrace_model::{
    DecodeError, DecodeOutcome, DecodePolicy, DocumentId, Event, EventEnvelope, FORMAT_VERSION_V1,
    Hash, MAX_ENVELOPE_BYTES, MAX_IDENTIFIER_BYTES, MAX_INSERTED_TEXT_BYTES, MAX_JSON_NESTING,
    MAX_JSON_RAW_STRING_BYTES, MAX_MONOTONIC_MILLIS, MAX_VECTOR_ITEMS, SessionId, TextEdit,
    decode_envelope, encode_envelope,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Consumer {
    Provenance,
    TreeSitter,
    LspDidChange,
    Replay,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EffectCall {
    consumer: Consumer,
    transaction: EditorTransaction,
    transaction_address: usize,
}

#[derive(Clone, Default)]
struct RecordingEffects(Rc<RefCell<Vec<EffectCall>>>);

impl RecordingEffects {
    fn calls(&self) -> Vec<EffectCall> {
        self.0.borrow().clone()
    }

    fn record(&mut self, consumer: Consumer, transaction: &EditorTransaction) {
        self.0.borrow_mut().push(EffectCall {
            consumer,
            transaction: transaction.clone(),
            transaction_address: std::ptr::from_ref(transaction).addr(),
        });
    }
}

impl EditorEffects for RecordingEffects {
    fn record_provenance(
        &mut self,
        transaction: &EditorTransaction,
    ) -> Result<(), EditorEffectError> {
        self.record(Consumer::Provenance, transaction);
        Ok(())
    }

    fn update_tree_sitter(&mut self, transaction: &EditorTransaction) {
        self.record(Consumer::TreeSitter, transaction);
    }

    fn send_lsp_did_change(&mut self, transaction: &EditorTransaction) {
        self.record(Consumer::LspDidChange, transaction);
    }

    fn record_replay(&mut self, transaction: &EditorTransaction) {
        self.record(Consumer::Replay, transaction);
    }
}

#[derive(Clone, Default)]
struct FallibleRecordingEffects {
    calls: Rc<RefCell<Vec<Consumer>>>,
    reject_provenance: Rc<RefCell<bool>>,
}

impl FallibleRecordingEffects {
    fn reject_next(&self) {
        *self.reject_provenance.borrow_mut() = true;
    }

    fn calls(&self) -> Vec<Consumer> {
        self.calls.borrow().clone()
    }
}

impl EditorEffects for FallibleRecordingEffects {
    fn record_provenance(
        &mut self,
        _transaction: &EditorTransaction,
    ) -> Result<(), EditorEffectError> {
        self.calls.borrow_mut().push(Consumer::Provenance);
        if std::mem::take(&mut *self.reject_provenance.borrow_mut()) {
            Err(EditorEffectError::new("injected provenance failure"))
        } else {
            Ok(())
        }
    }

    fn update_tree_sitter(&mut self, _transaction: &EditorTransaction) {
        self.calls.borrow_mut().push(Consumer::TreeSitter);
    }

    fn send_lsp_did_change(&mut self, _transaction: &EditorTransaction) {
        self.calls.borrow_mut().push(Consumer::LspDidChange);
    }

    fn record_replay(&mut self, _transaction: &EditorTransaction) {
        self.calls.borrow_mut().push(Consumer::Replay);
    }
}

#[test]
fn formatter_replacement_is_one_non_mutating_utf8_aware_preview() {
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "α old 🦀 tail", effects);
    editor.set_selection(SelectionState::new(4, 11)).unwrap();

    let transaction = editor
        .preview_formatter_replacement("α formatted 🦀 tail")
        .unwrap()
        .unwrap();
    assert_eq!(editor.text(), "α old 🦀 tail");
    assert_eq!(editor.version(), 0);
    assert!(observed.calls().is_empty());
    assert_eq!(transaction.origin, EditOrigin::Formatter);
    assert_eq!(transaction.version_before, 0);
    assert_eq!(transaction.version_after, 1);
    assert_eq!(transaction.selection_before, SelectionState::new(4, 11));
    assert_eq!(transaction.selection_after, SelectionState::new(11, 17));
    assert_eq!(transaction.edits, vec![edit(3, 5, "formatte")]);

    assert!(editor.apply_transaction(transaction).unwrap());
    assert_eq!(editor.text(), "α formatted 🦀 tail");
    assert_eq!(observed.calls().len(), 4);
}

#[test]
fn provenance_failure_prevents_buffer_history_and_downstream_mutation() {
    let effects = FallibleRecordingEffects::default();
    effects.reject_next();
    let mut editor = EditorBuffer::new(document_id(), "abc", effects.clone());
    editor.move_cursor(Movement::DocumentEnd, false);
    let before_text = editor.text();
    let before_version = editor.version();
    let before_hash = editor.hash();
    let before_selection = editor.selection_state();

    let result = editor.insert_char('!');

    assert!(matches!(result, Err(TransactionError::Provenance(_))));
    assert_eq!(editor.text(), before_text);
    assert_eq!(editor.version(), before_version);
    assert_eq!(editor.hash(), before_hash);
    assert_eq!(editor.selection_state(), before_selection);
    assert_eq!(effects.calls(), [Consumer::Provenance]);

    assert!(editor.insert_char('?').unwrap());
    assert_eq!(editor.text(), "abc?");
    assert!(editor.undo().unwrap());
    assert_eq!(editor.text(), "abc");
}

fn document_id() -> DocumentId {
    DocumentId::new("document-1").unwrap()
}

fn edit(start_byte: u64, end_byte: u64, inserted_text: &str) -> TextEdit {
    TextEdit {
        start_byte,
        end_byte,
        inserted_text: inserted_text.to_owned(),
    }
}

#[test]
fn edit_previews_are_exact_grapheme_aware_and_non_mutating() {
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "a🦀b", effects);

    editor.set_selection(SelectionState::caret(5)).unwrap();
    let backward = editor.preview_delete_backward().unwrap().unwrap();
    assert_eq!(backward.edits, [edit(1, 5, "")]);
    assert_eq!(editor.text(), "a🦀b");
    assert_eq!(editor.version(), 0);
    assert!(observed.calls().is_empty());

    editor.set_selection(SelectionState::caret(1)).unwrap();
    let forward = editor.preview_delete_forward().unwrap().unwrap();
    assert_eq!(forward.edits, [edit(1, 5, "")]);
    assert_eq!(editor.text(), "a🦀b");
    assert!(observed.calls().is_empty());

    editor.set_selection(SelectionState::new(1, 5)).unwrap();
    let replacement = editor.preview_paste("界").unwrap().unwrap();
    assert_eq!(replacement.edits, [edit(1, 5, "界")]);
    assert_eq!(editor.text(), "a🦀b");
    assert!(observed.calls().is_empty());

    editor.set_selection(SelectionState::caret(0)).unwrap();
    assert!(editor.preview_delete_backward().unwrap().is_none());
    assert!(editor.preview_paste("").unwrap().is_none());
    assert_eq!(
        editor.selection_after_movement(Movement::DocumentStart, false),
        editor.selection_state()
    );
    assert!(observed.calls().is_empty());
}

#[test]
fn previous_word_delete_uses_unicode_boundaries_and_is_one_replayable_undo_unit() {
    for (initial, caret, expected, deleted_range) in [
        ("alpha  beta", 11, "alpha  ", (7, 11)),
        ("a cafe\u{301}!", 8, "a !", (2, 8)),
        ("one\r\ntwo", 5, "two", (0, 5)),
    ] {
        let effects = RecordingEffects::default();
        let observed = effects.clone();
        let mut editor = EditorBuffer::new(document_id(), initial, effects);
        editor.set_selection(SelectionState::caret(caret)).unwrap();

        assert!(editor.delete_previous_word().unwrap());
        assert_eq!(editor.text(), expected);
        assert_eq!(
            editor.selection_state(),
            SelectionState::caret(deleted_range.0)
        );

        let transactions = observed
            .calls()
            .into_iter()
            .filter(|call| call.consumer == Consumer::Provenance)
            .map(|call| call.transaction)
            .collect::<Vec<_>>();
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0].origin, EditOrigin::Keyboard);
        assert_eq!(
            transactions[0].edits,
            [edit(deleted_range.0, deleted_range.1, "")]
        );
        assert_eq!(
            replay_transactions(document_id(), initial, &transactions, NoopEditorEffects)
                .unwrap()
                .text(),
            expected
        );

        assert!(editor.undo().unwrap());
        assert_eq!(editor.text(), initial);
    }
}

fn assert_line_deletion(
    initial: &str,
    caret: u64,
    expected: &str,
    deleted_range: (u64, u64),
    delete: impl FnOnce(&mut EditorBuffer<RecordingEffects>) -> Result<bool, TransactionError>,
) {
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), initial, effects);
    editor.set_selection(SelectionState::caret(caret)).unwrap();

    assert!(delete(&mut editor).unwrap());
    assert_eq!(editor.text(), expected);
    assert_eq!(
        editor.selection_state(),
        SelectionState::caret(deleted_range.0)
    );

    let transactions = observed
        .calls()
        .into_iter()
        .filter(|call| call.consumer == Consumer::Provenance)
        .map(|call| call.transaction)
        .collect::<Vec<_>>();
    assert_eq!(transactions.len(), 1);
    assert_eq!(transactions[0].origin, EditOrigin::Keyboard);
    assert_eq!(
        transactions[0].edits,
        [edit(deleted_range.0, deleted_range.1, "")]
    );
    assert_eq!(
        replay_transactions(document_id(), initial, &transactions, NoopEditorEffects)
            .unwrap()
            .text(),
        expected
    );
    assert!(editor.undo().unwrap());
    assert_eq!(editor.text(), initial);
}

#[test]
fn delete_to_line_start_is_atomic_replayable_and_undoable_for_ascii_unicode_and_crlf() {
    for (initial, caret, expected, deleted_range) in [
        ("alpha beta", 5, " beta", (0, 5)),
        ("α cafe\u{301}", 9, "", (0, 9)),
        ("head\r\nα beta\r\ntail", 13, "head\r\n\r\ntail", (6, 13)),
    ] {
        assert_line_deletion(
            initial,
            caret,
            expected,
            deleted_range,
            EditorBuffer::delete_to_line_start,
        );
    }
}

#[test]
fn delete_to_line_end_is_atomic_replayable_and_undoable_for_ascii_unicode_and_crlf() {
    for (initial, caret, expected, deleted_range) in [
        ("alpha beta", 5, "alpha", (5, 10)),
        ("α cafe\u{301}", 2, "α", (2, 9)),
        ("head\r\nα beta\r\ntail", 8, "head\r\nα\r\ntail", (8, 13)),
    ] {
        assert_line_deletion(
            initial,
            caret,
            expected,
            deleted_range,
            EditorBuffer::delete_to_line_end,
        );
    }
}

#[test]
fn line_deletions_are_quiet_at_boundaries() {
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "alpha\r\nbeta", effects);

    editor.set_selection(SelectionState::caret(7)).unwrap();
    assert!(!editor.delete_to_line_start().unwrap());
    editor.set_selection(SelectionState::caret(11)).unwrap();
    assert!(!editor.delete_to_line_end().unwrap());
    editor.set_selection(SelectionState::caret(0)).unwrap();
    assert!(!editor.delete_previous_word().unwrap());
    assert!(observed.calls().is_empty());
}

#[test]
fn transaction_types_round_trip_with_every_origin() {
    let origins = [
        EditOrigin::Keyboard,
        EditOrigin::Paste,
        EditOrigin::Undo,
        EditOrigin::Redo,
        EditOrigin::Completion,
        EditOrigin::AdditionalCompletionEdit,
        EditOrigin::Formatter,
        EditOrigin::CodeAction,
        EditOrigin::FileReload,
        EditOrigin::ExternalChange,
        EditOrigin::Unknown,
    ];

    for origin in origins {
        let transaction = EditorTransaction {
            document_id: document_id(),
            version_before: 7,
            version_after: 8,
            origin,
            edits: vec![edit(1, 3, "界")],
            selection_before: SelectionState::new(1, 3),
            selection_after: SelectionState::caret(4),
            hash_before: Hash::zero(),
            hash_after: Hash::from_bytes([7; 32]),
        };

        let encoded = encode_transaction(&transaction).unwrap();
        let decoded = decode_transaction(&encoded).unwrap();
        assert_eq!(decoded, transaction);
    }
}

#[test]
fn editor_transactions_feed_file_edited_without_losing_provenance() {
    let origins = [
        EditOrigin::Keyboard,
        EditOrigin::Paste,
        EditOrigin::Undo,
        EditOrigin::Redo,
        EditOrigin::Completion,
        EditOrigin::AdditionalCompletionEdit,
        EditOrigin::Formatter,
        EditOrigin::CodeAction,
        EditOrigin::FileReload,
        EditOrigin::ExternalChange,
        EditOrigin::Unknown,
    ];

    for origin in origins {
        let transaction = EditorTransaction {
            document_id: document_id(),
            version_before: 7,
            version_after: 8,
            origin,
            edits: vec![edit(2, 2, "🦀"), edit(2, 2, "界"), edit(4, 8, "é")],
            selection_before: SelectionState::new(8, 2),
            selection_after: SelectionState::new(2, 10),
            hash_before: Hash::from_bytes([1; 32]),
            hash_after: Hash::from_bytes([2; 32]),
        };
        let envelope = EventEnvelope {
            format_version: FORMAT_VERSION_V1,
            session_id: SessionId::new("session-1").unwrap(),
            sequence: 1,
            monotonic_millis: 0,
            wall_clock_utc: None,
            previous_event_hash: Hash::zero(),
            event_hash: Hash::from_bytes([3; 32]),
            event: Event::FileEdited(transaction.clone()),
        };

        let encoded = encode_envelope(&envelope).unwrap();
        let DecodeOutcome::Decoded(decoded) =
            decode_envelope(&encoded, DecodePolicy::RejectUnsupported).unwrap()
        else {
            panic!("known v1 event was skipped");
        };
        let Event::FileEdited(file_edited) = decoded.event else {
            panic!("decoded event changed variant");
        };
        assert_eq!(file_edited, transaction);
    }
}

#[test]
fn largest_live_transaction_fits_a_worst_case_file_edited_envelope() {
    let maximum_text = "x".repeat(MAX_INSERTED_TEXT_BYTES);
    let mut boundary = EditorTransaction {
        document_id: document_id(),
        version_before: u64::MAX - 1,
        version_after: u64::MAX,
        origin: EditOrigin::Redo,
        edits: vec![
            edit(0, 0, &maximum_text),
            edit(0, 0, &maximum_text),
            edit(0, 0, &maximum_text),
            edit(0, 0, ""),
        ],
        selection_before: SelectionState::caret(u64::MAX),
        selection_after: SelectionState::caret(u64::MAX),
        hash_before: Hash::zero(),
        hash_after: Hash::zero(),
    };
    let mut accepted = 0;
    let mut rejected = MAX_INSERTED_TEXT_BYTES + 1;
    while accepted + 1 < rejected {
        let candidate = accepted + (rejected - accepted) / 2;
        boundary.edits[3].inserted_text = "y".repeat(candidate);
        if encode_transaction(&boundary).is_ok() {
            accepted = candidate;
        } else {
            rejected = candidate;
        }
    }
    boundary.edits[3].inserted_text = "y".repeat(accepted);
    assert_eq!(
        encode_transaction(&boundary).unwrap().len(),
        MAX_TRANSACTION_JSON_BYTES
    );
    boundary.edits[3].inserted_text.push('y');
    assert!(encode_transaction(&boundary).is_err());
    boundary.edits[3].inserted_text.pop();

    let selection_after = SelectionState::caret((3 * MAX_INSERTED_TEXT_BYTES + accepted) as u64);

    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "", effects);
    assert!(
        editor
            .apply_edits(EditOrigin::Formatter, boundary.edits, selection_after,)
            .unwrap()
    );
    let transaction = observed.calls()[0].transaction.clone();
    assert!(encode_transaction(&transaction).unwrap().len() <= MAX_TRANSACTION_JSON_BYTES);

    let envelope = EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: SessionId::new("s".repeat(MAX_IDENTIFIER_BYTES)).unwrap(),
        sequence: u64::MAX,
        monotonic_millis: MAX_MONOTONIC_MILLIS,
        wall_clock_utc: Some("+262142-12-31T23:59:59.999999999Z".parse().unwrap()),
        previous_event_hash: Hash::from_bytes([0xff; Hash::LENGTH]),
        event_hash: Hash::from_bytes([0xff; Hash::LENGTH]),
        event: Event::FileEdited(transaction),
    };
    let encoded = encode_envelope(&envelope).unwrap();
    assert!(encoded.len() <= MAX_ENVELOPE_BYTES);
}

#[test]
fn canonical_encoding_is_bounded_exact_and_round_trips() {
    let transaction = EditorTransaction {
        document_id: document_id(),
        version_before: 0,
        version_after: 1,
        origin: EditOrigin::Formatter,
        edits: vec![edit(0, 0, "plain\n\"escaped\"")],
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(0),
        hash_before: Hash::zero(),
        hash_after: Hash::from_bytes([1; 32]),
    };
    let encoded = encode_transaction(&transaction).unwrap();
    assert_eq!(encoded, serde_json::to_vec(&transaction).unwrap());
    assert_eq!(decode_transaction(&encoded).unwrap(), transaction);

    let maximum_text = "x".repeat(MAX_INSERTED_TEXT_BYTES);
    let mut exact = EditorTransaction {
        document_id: document_id(),
        version_before: 0,
        version_after: 1,
        origin: EditOrigin::Formatter,
        edits: vec![
            edit(0, 0, &maximum_text),
            edit(0, 0, &maximum_text),
            edit(0, 0, &maximum_text),
            edit(0, 0, ""),
        ],
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(u64::MAX),
        hash_before: Hash::zero(),
        hash_after: Hash::from_bytes([1; 32]),
    };
    let base_len = serde_json::to_vec(&exact).unwrap().len();
    let exact_tail = MAX_TRANSACTION_JSON_BYTES - base_len;
    assert!(exact_tail <= MAX_INSERTED_TEXT_BYTES);
    exact.edits[3].inserted_text = "y".repeat(exact_tail);

    let encoded = encode_transaction(&exact).unwrap();
    assert_eq!(encoded.len(), MAX_TRANSACTION_JSON_BYTES);
    assert_eq!(decode_transaction(&encoded).unwrap(), exact);

    exact.edits[3].inserted_text.push('y');
    assert!(matches!(
        encode_transaction(&exact),
        Err(TransactionEncodeError::PayloadTooLarge { .. })
    ));

    let escape_heavy = EditorTransaction {
        edits: vec![edit(0, 0, &"\n".repeat(MAX_INSERTED_TEXT_BYTES))],
        ..transaction.clone()
    };
    let encoded = encode_transaction(&escape_heavy).unwrap();
    assert_eq!(decode_transaction(&encoded).unwrap(), escape_heavy);

    let oversized_escapes = EditorTransaction {
        edits: vec![
            edit(0, 0, &"\n".repeat(MAX_INSERTED_TEXT_BYTES)),
            edit(0, 0, &"\n".repeat(MAX_INSERTED_TEXT_BYTES)),
        ],
        ..transaction
    };
    assert!(matches!(
        encode_transaction(&oversized_escapes),
        Err(TransactionEncodeError::PayloadTooLarge { .. })
    ));
}

#[test]
fn canonical_encoding_observes_decoder_escape_boundaries() {
    let transaction = |inserted_text: &str| EditorTransaction {
        document_id: document_id(),
        version_before: 0,
        version_after: 1,
        origin: EditOrigin::Paste,
        edits: vec![edit(0, 0, inserted_text)],
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(inserted_text.len() as u64),
        hash_before: document_hash(""),
        hash_after: document_hash(inserted_text),
    };

    let nul_probe = transaction(&"\0".repeat(100_000));
    let unchecked = serde_json::to_vec(&nul_probe).unwrap();
    assert!(unchecked.len() < MAX_ENVELOPE_BYTES);
    assert!(matches!(
        decode_transaction(&unchecked),
        Err(TransactionDecodeError::JsonPreflight(
            DecodeError::JsonStringTooLong { .. }
        ))
    ));
    assert!(matches!(
        encode_transaction(&nul_probe),
        Err(TransactionEncodeError::Validation(
            TransactionError::CodecPreflight(DecodeError::JsonStringTooLong { .. })
        ))
    ));

    let mut exact_raw_boundary = "\0".repeat(MAX_JSON_RAW_STRING_BYTES / 6);
    exact_raw_boundary.push('"');
    assert_eq!(
        serde_json::to_string(&exact_raw_boundary).unwrap().len() - 2,
        MAX_JSON_RAW_STRING_BYTES
    );
    let exact = transaction(&exact_raw_boundary);
    let encoded = encode_transaction(&exact).unwrap();
    assert_eq!(decode_transaction(&encoded).unwrap(), exact);

    exact_raw_boundary.push('\\');
    assert!(matches!(
        encode_transaction(&transaction(&exact_raw_boundary)),
        Err(TransactionEncodeError::Validation(
            TransactionError::CodecPreflight(DecodeError::JsonStringTooLong { .. })
        ))
    ));

    for (escaped, encoded_width) in [('\u{001f}', 6), ('\n', 2), ('"', 2), ('\\', 2)] {
        let at_limit = escaped
            .to_string()
            .repeat(MAX_JSON_RAW_STRING_BYTES / encoded_width);
        let at_limit_transaction = transaction(&at_limit);
        let encoded = encode_transaction(&at_limit_transaction).unwrap();
        assert_eq!(decode_transaction(&encoded).unwrap(), at_limit_transaction);

        if at_limit.len() < MAX_INSERTED_TEXT_BYTES {
            let beyond_limit = format!("{at_limit}{escaped}");
            assert!(encode_transaction(&transaction(&beyond_limit)).is_err());
        }
    }
}

#[test]
fn control_character_paste_and_redo_emit_only_decodable_transactions() {
    let effects = RecordingEffects::default();
    let rejected_effects = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "", effects);
    let before = (
        editor.text(),
        editor.version(),
        editor.selection_state(),
        editor.can_undo(),
        editor.can_redo(),
    );
    assert!(matches!(
        editor.paste(&"\0".repeat(100_000)),
        Err(TransactionError::CodecPreflight(
            DecodeError::JsonStringTooLong { .. }
        ))
    ));
    assert_eq!(
        (
            editor.text(),
            editor.version(),
            editor.selection_state(),
            editor.can_undo(),
            editor.can_redo(),
        ),
        before
    );
    assert!(rejected_effects.calls().is_empty());

    let mut exact_raw_boundary = "\0".repeat(MAX_JSON_RAW_STRING_BYTES / 6);
    exact_raw_boundary.push('"');
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "", effects);
    assert!(editor.paste(&exact_raw_boundary).unwrap());
    assert!(editor.undo().unwrap());
    assert!(editor.redo().unwrap());
    assert_eq!(editor.text(), exact_raw_boundary);
    let calls = observed.calls();
    assert_eq!(calls.len(), 12);
    for call in calls {
        let encoded = encode_transaction(&call.transaction).unwrap();
        assert_eq!(decode_transaction(&encoded).unwrap(), call.transaction);
    }
}

#[test]
fn control_character_deletion_requires_a_decodable_undo() {
    let oversized_inverse = "\0".repeat(100_000);
    let effects = RecordingEffects::default();
    let rejected_effects = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), &oversized_inverse, effects);
    assert!(editor.insert_char('x').unwrap());
    assert!(editor.undo().unwrap());
    let before = (
        editor.text(),
        editor.version(),
        editor.selection_state(),
        editor.can_undo(),
        editor.can_redo(),
        rejected_effects.calls().len(),
    );
    assert!(matches!(
        editor.apply_edits(
            EditOrigin::CodeAction,
            vec![edit(0, oversized_inverse.len() as u64, "")],
            SelectionState::caret(0),
        ),
        Err(TransactionError::HistoryNotRepresentable { .. })
    ));
    assert_eq!(
        (
            editor.text(),
            editor.version(),
            editor.selection_state(),
            editor.can_undo(),
            editor.can_redo(),
            rejected_effects.calls().len(),
        ),
        before
    );

    let mut exact_raw_boundary = "\0".repeat(MAX_JSON_RAW_STRING_BYTES / 6);
    exact_raw_boundary.push('"');
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), &exact_raw_boundary, effects);
    assert!(
        editor
            .apply_edits(
                EditOrigin::CodeAction,
                vec![edit(0, exact_raw_boundary.len() as u64, "")],
                SelectionState::caret(0),
            )
            .unwrap()
    );
    assert!(editor.undo().unwrap());
    assert_eq!(editor.text(), exact_raw_boundary);
    let calls = observed.calls();
    assert_eq!(calls.len(), 8);
    for call in calls {
        let encoded = encode_transaction(&call.transaction).unwrap();
        assert_eq!(decode_transaction(&encoded).unwrap(), call.transaction);
    }
}

#[test]
fn oversized_live_transaction_is_rejected_atomically_before_effects() {
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "", effects);
    let maximum_text = "x".repeat(MAX_INSERTED_TEXT_BYTES);
    let edits = (0..4)
        .map(|_| edit(0, 0, &maximum_text))
        .collect::<Vec<_>>();
    let before = (
        editor.text(),
        editor.version(),
        editor.selection_state(),
        editor.can_undo(),
        editor.can_redo(),
    );

    assert!(matches!(
        editor.apply_edits(
            EditOrigin::Formatter,
            edits,
            SelectionState::caret((4 * MAX_INSERTED_TEXT_BYTES) as u64),
        ),
        Err(TransactionError::TransactionTooLarge { .. })
    ));
    assert_eq!(
        (
            editor.text(),
            editor.version(),
            editor.selection_state(),
            editor.can_undo(),
            editor.can_redo(),
        ),
        before
    );
    assert!(observed.calls().is_empty());
}

#[test]
fn aggregate_codec_rejection_precedes_snapshot_range_checks() {
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "", effects);
    let maximum_text = "x".repeat(MAX_INSERTED_TEXT_BYTES);
    let mut edits = vec![
        edit(1, 1, &maximum_text),
        edit(1, 1, &maximum_text),
        edit(1, 1, &maximum_text),
        edit(1, 1, ""),
    ];
    let probe = EditorTransaction {
        document_id: document_id(),
        version_before: 0,
        version_after: 1,
        origin: EditOrigin::Formatter,
        edits: edits.clone(),
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(0),
        hash_before: document_hash(""),
        hash_after: Hash::zero(),
    };
    let base_len = serde_json::to_vec(&probe).unwrap().len();
    let tail_len = MAX_TRANSACTION_JSON_BYTES + 1 - base_len;
    assert!(tail_len <= MAX_INSERTED_TEXT_BYTES);
    edits[3].inserted_text = "y".repeat(tail_len);
    let mut probe = probe;
    probe.edits = edits.clone();
    let probe_len = serde_json::to_vec(&probe).unwrap().len();
    assert_eq!(probe_len, MAX_TRANSACTION_JSON_BYTES + 1);
    assert!(probe_len < MAX_ENVELOPE_BYTES);

    assert!(matches!(
        editor.apply_edits(EditOrigin::Formatter, edits, SelectionState::caret(0),),
        Err(TransactionError::TransactionTooLarge { .. })
    ));
    assert_eq!(editor.text(), "");
    assert_eq!(editor.version(), 0);
    assert_eq!(editor.selection_state(), SelectionState::caret(0));
    assert!(!editor.can_undo());
    assert!(!editor.can_redo());
    assert!(observed.calls().is_empty());
}

#[test]
fn large_utf8_deletions_have_bounded_usable_undo_and_redo() {
    let ascii = "a".repeat(MAX_INSERTED_TEXT_BYTES + 1);
    let mut editor = EditorBuffer::new(document_id(), &ascii, NoopEditorEffects);
    assert!(
        editor
            .apply_edits(
                EditOrigin::CodeAction,
                vec![edit(0, ascii.len() as u64, "")],
                SelectionState::caret(0),
            )
            .unwrap()
    );
    assert_eq!(editor.text(), "");
    assert!(editor.undo().unwrap());
    assert_eq!(editor.text(), ascii);
    assert!(editor.redo().unwrap());
    assert_eq!(editor.text(), "");

    let unicode_prefix = "a".repeat(MAX_INSERTED_TEXT_BYTES - 1);
    let large_deleted = format!("{unicode_prefix}🦀");
    let initial = format!("{large_deleted}|猫x");
    let second_start = large_deleted.len() + 1;
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), &initial, effects);
    assert!(
        editor
            .apply_edits(
                EditOrigin::Formatter,
                vec![
                    edit(0, large_deleted.len() as u64, ""),
                    edit(second_start as u64, (second_start + "猫".len()) as u64, "C"),
                ],
                SelectionState::caret(3),
            )
            .unwrap()
    );
    assert_eq!(editor.text(), "|Cx");
    assert!(editor.undo().unwrap());
    assert_eq!(editor.text(), initial);
    assert!(editor.redo().unwrap());
    assert_eq!(editor.text(), "|Cx");

    let transactions = observed
        .calls()
        .into_iter()
        .filter(|call| call.consumer == Consumer::Provenance)
        .map(|call| call.transaction)
        .collect::<Vec<_>>();
    assert_eq!(transactions[1].origin, EditOrigin::Undo);
    assert_eq!(transactions[1].edits.len(), 3);
    assert_eq!(
        transactions[1].edits[0].inserted_text.len(),
        MAX_INSERTED_TEXT_BYTES - 1
    );
    assert_eq!(transactions[1].edits[1].inserted_text, "🦀");
    for transaction in transactions {
        let encoded = encode_transaction(&transaction).unwrap();
        assert_eq!(decode_transaction(&encoded).unwrap(), transaction);
    }
}

#[test]
fn deletion_with_unencodable_inverse_is_rejected_atomically() {
    let initial = "\n".repeat(600_000);
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), &initial, effects);
    assert!(editor.insert_char('x').unwrap());
    assert!(editor.undo().unwrap());
    let before = (
        editor.text(),
        editor.version(),
        editor.selection_state(),
        editor.can_undo(),
        editor.can_redo(),
        observed.calls().len(),
    );

    assert!(matches!(
        editor.apply_edits(
            EditOrigin::FileReload,
            vec![edit(0, initial.len() as u64, "")],
            SelectionState::caret(0),
        ),
        Err(TransactionError::HistoryNotRepresentable { .. })
    ));
    assert_eq!(
        (
            editor.text(),
            editor.version(),
            editor.selection_state(),
            editor.can_undo(),
            editor.can_redo(),
            observed.calls().len(),
        ),
        before
    );
    assert!(editor.can_redo());
}

#[test]
fn replay_rejects_oversized_borrowed_transaction_before_selection_staging() {
    let maximum_text = "x".repeat(MAX_INSERTED_TEXT_BYTES);
    let transaction = EditorTransaction {
        document_id: document_id(),
        version_before: 0,
        version_after: 1,
        origin: EditOrigin::Formatter,
        edits: (0..4).map(|_| edit(0, 0, &maximum_text)).collect(),
        selection_before: SelectionState::caret(1),
        selection_after: SelectionState::caret(0),
        hash_before: document_hash(""),
        hash_after: Hash::zero(),
    };

    assert!(matches!(
        replay_transactions(
            document_id(),
            "",
            std::slice::from_ref(&transaction),
            NoopEditorEffects,
        ),
        Err(ReplayError::Transaction {
            source: TransactionError::TransactionTooLarge { .. },
            ..
        })
    ));
}

#[test]
fn recorded_stream_replays_cursor_moves_unicode_multi_edits_undo_and_redo() {
    let initial = "aé\n";
    let effects = RecordingEffects::default();
    let recorded = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), initial, effects);

    editor.move_cursor(rustrace_editor::Movement::DocumentEnd, false);
    assert!(editor.insert_char('🦀').unwrap());
    editor.move_cursor(rustrace_editor::Movement::DocumentStart, false);
    assert!(editor.paste("λ").unwrap());
    editor.move_cursor(rustrace_editor::Movement::Right, false);
    assert!(
        editor
            .apply_edits(
                EditOrigin::Formatter,
                vec![edit(0, 2, "L"), edit(6, 10, "C")],
                SelectionState::caret(6),
            )
            .unwrap()
    );
    editor.move_cursor(rustrace_editor::Movement::DocumentStart, false);
    assert!(editor.undo().unwrap());
    editor.move_cursor(rustrace_editor::Movement::DocumentEnd, false);
    assert!(editor.redo().unwrap());

    let transactions = recorded
        .calls()
        .into_iter()
        .filter(|call| call.consumer == Consumer::Provenance)
        .map(|call| call.transaction)
        .collect::<Vec<_>>();
    assert_eq!(transactions.len(), 5);
    assert_ne!(
        transactions[0].selection_after,
        transactions[1].selection_before
    );
    assert_ne!(
        transactions[2].selection_after,
        transactions[3].selection_before
    );

    let replay_effects = RecordingEffects::default();
    let replayed_calls = replay_effects.clone();
    let replayed = replay_transactions(document_id(), initial, &transactions, replay_effects)
        .expect("recorded stream replays without caller-staged selections");
    assert_eq!(replayed.text(), editor.text());
    assert_eq!(replayed.text(), "Laé\nC");
    assert_eq!(replayed.version(), transactions.len() as u64);
    assert_eq!(
        replayed.selection_state(),
        transactions.last().unwrap().selection_after
    );

    let replayed_transactions = replayed_calls
        .calls()
        .into_iter()
        .filter(|call| call.consumer == Consumer::Provenance)
        .map(|call| call.transaction)
        .collect::<Vec<_>>();
    assert_eq!(replayed_transactions, transactions);
    assert_eq!(replayed_calls.calls().len(), transactions.len() * 4);
}

#[test]
fn replay_selection_staging_does_not_weaken_live_application_validation() {
    let effects = RecordingEffects::default();
    let recorded = effects.clone();
    let mut source = EditorBuffer::new(document_id(), "abc", effects);
    source.set_selection(SelectionState::caret(3)).unwrap();
    assert!(source.insert_char('d').unwrap());
    let transaction = recorded.calls()[0].transaction.clone();

    let mut live = EditorBuffer::new(document_id(), "abc", NoopEditorEffects);
    assert!(live.apply_transaction(transaction.clone()).is_err());
    assert_eq!(live.text(), "abc");
    assert_eq!(live.version(), 0);

    let replayed = replay_transactions(
        document_id(),
        "abc",
        std::slice::from_ref(&transaction),
        NoopEditorEffects,
    )
    .unwrap();
    assert_eq!(replayed.text(), "abcd");

    let replay_effects = RecordingEffects::default();
    let rejected_effects = replay_effects.clone();
    let mut tampered = transaction;
    tampered.hash_after = Hash::zero();
    assert!(
        replay_transactions(
            document_id(),
            "abc",
            std::slice::from_ref(&tampered),
            replay_effects,
        )
        .is_err()
    );
    assert!(rejected_effects.calls().is_empty());
}

#[test]
fn transaction_json_decode_is_bounded_and_preflighted() {
    let transaction = EditorTransaction {
        document_id: document_id(),
        version_before: 0,
        version_after: 1,
        origin: EditOrigin::Paste,
        edits: vec![edit(0, 0, "x")],
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(1),
        hash_before: Hash::zero(),
        hash_after: Hash::from_bytes([1; 32]),
    };
    let encoded = serde_json::to_string(&transaction).unwrap();

    let oversized_raw = vec![b' '; MAX_ENVELOPE_BYTES + 1];
    assert!(matches!(
        decode_transaction(&oversized_raw),
        Err(TransactionDecodeError::PayloadTooLarge { .. })
    ));

    let oversized_insert = encoded.replace(
        "\"inserted_text\":\"x\"",
        &format!(
            "\"inserted_text\":\"{}\"",
            "x".repeat(MAX_INSERTED_TEXT_BYTES + 1)
        ),
    );
    let oversized_insert = oversized_insert.replace("document-1", "!invalid");
    assert!(matches!(
        decode_transaction(oversized_insert.as_bytes()),
        Err(TransactionDecodeError::JsonPreflight(
            DecodeError::JsonStringTooLong { .. }
        ))
    ));

    let edit_json = r#"{"start_byte":0,"end_byte":0,"inserted_text":""}"#;
    let oversized_vector = encoded.replace(
        r#"[{"start_byte":0,"end_byte":0,"inserted_text":"x"}]"#,
        &format!("[{}]", vec![edit_json; MAX_VECTOR_ITEMS + 1].join(",")),
    );
    assert!(matches!(
        decode_transaction(oversized_vector.as_bytes()),
        Err(TransactionDecodeError::JsonPreflight(
            DecodeError::JsonContainerTooLarge { .. }
        ))
    ));

    let duplicate = format!(r#"{{"document_id":"duplicate",{}"#, &encoded[1..]);
    assert!(matches!(
        decode_transaction(duplicate.as_bytes()),
        Err(TransactionDecodeError::JsonPreflight(
            DecodeError::DuplicateObjectKey { .. }
        ))
    ));

    let nested = format!(
        r#"{{"extra":{},{}"#,
        "[".repeat(MAX_JSON_NESTING + 1) + &"]".repeat(MAX_JSON_NESTING + 1),
        &encoded[1..]
    );
    assert!(matches!(
        decode_transaction(nested.as_bytes()),
        Err(TransactionDecodeError::JsonPreflight(
            DecodeError::NestingTooDeep { .. }
        ))
    ));

    let unknown = format!(r#"{{"extra":0,{}"#, &encoded[1..]);
    assert!(matches!(
        decode_transaction(unknown.as_bytes()),
        Err(TransactionDecodeError::Typed { .. })
    ));

    let invalid_version = encoded.replace("\"version_after\":1", "\"version_after\":2");
    assert!(matches!(
        decode_transaction(invalid_version.as_bytes()),
        Err(TransactionDecodeError::Validation(_))
    ));

    let mut unsorted = transaction;
    unsorted.edits = vec![edit(2, 2, "y"), edit(0, 0, "x")];
    let unsorted = serde_json::to_vec(&unsorted).unwrap();
    assert!(matches!(
        decode_transaction(&unsorted),
        Err(TransactionDecodeError::Validation(_))
    ));
}

#[test]
fn transaction_json_decode_accepts_exact_string_and_vector_limits() {
    let at_string_limit = EditorTransaction {
        document_id: document_id(),
        version_before: 0,
        version_after: 1,
        origin: EditOrigin::Paste,
        edits: vec![edit(0, 0, &"x".repeat(MAX_INSERTED_TEXT_BYTES))],
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(MAX_INSERTED_TEXT_BYTES as u64),
        hash_before: Hash::zero(),
        hash_after: Hash::from_bytes([1; 32]),
    };
    let encoded = encode_transaction(&at_string_limit).unwrap();
    assert_eq!(decode_transaction(&encoded).unwrap(), at_string_limit);

    let at_vector_limit = EditorTransaction {
        document_id: document_id(),
        version_before: 0,
        version_after: 1,
        origin: EditOrigin::Formatter,
        edits: (0..MAX_VECTOR_ITEMS).map(|_| edit(0, 0, "")).collect(),
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(0),
        hash_before: Hash::zero(),
        hash_after: Hash::zero(),
    };
    let encoded = encode_transaction(&at_vector_limit).unwrap();
    assert_eq!(decode_transaction(&encoded).unwrap(), at_vector_limit);
}

#[test]
fn oversized_borrowed_paste_is_rejected_without_editor_changes() {
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "unchanged", effects);
    let before = (
        editor.text(),
        editor.version(),
        editor.selection_state(),
        editor.can_undo(),
        editor.can_redo(),
    );

    assert!(
        editor
            .paste(&"x".repeat(MAX_INSERTED_TEXT_BYTES + 1))
            .is_err()
    );
    assert_eq!(
        (
            editor.text(),
            editor.version(),
            editor.selection_state(),
            editor.can_undo(),
            editor.can_redo(),
        ),
        before
    );
    assert!(observed.calls().is_empty());
}

#[test]
fn multiple_pre_snapshot_edits_reach_all_consumers_as_the_same_transaction() {
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "aé-middle-猫z", effects);
    let edits = vec![edit(1, 3, "E"), edit(11, 14, "crab")];

    assert!(
        editor
            .apply_edits(
                EditOrigin::Formatter,
                edits.clone(),
                SelectionState::caret(12),
            )
            .unwrap()
    );

    assert_eq!(editor.text(), "aE-middle-crabz");
    assert_eq!(editor.version(), 1);
    let calls = observed.calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(
        calls.iter().map(|call| call.consumer).collect::<Vec<_>>(),
        [
            Consumer::Provenance,
            Consumer::TreeSitter,
            Consumer::LspDidChange,
            Consumer::Replay,
        ]
    );
    assert!(
        calls
            .windows(2)
            .all(|pair| pair[0].transaction == pair[1].transaction)
    );
    assert!(
        calls
            .windows(2)
            .all(|pair| pair[0].transaction_address == pair[1].transaction_address)
    );
    for call in &calls {
        let encoded = encode_transaction(&call.transaction).unwrap();
        assert_eq!(decode_transaction(&encoded).unwrap(), call.transaction);
    }
    assert_eq!(calls[0].transaction.edits, edits);
    assert_eq!(calls[0].transaction.origin, EditOrigin::Formatter);
    assert_eq!(
        calls[0].transaction.hash_before,
        document_hash("aé-middle-猫z")
    );
    assert_eq!(
        calls[0].transaction.hash_after,
        document_hash(editor.text().as_str())
    );
}

#[test]
fn invalid_transactions_are_rejected_atomically() {
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut source = EditorBuffer::new(document_id(), "aéz", effects);
    assert!(
        source
            .apply_edits(
                EditOrigin::Completion,
                vec![edit(1, 3, "E")],
                SelectionState::caret(2),
            )
            .unwrap()
    );
    let valid = observed.calls()[0].transaction.clone();

    let mut cases = Vec::new();
    let mut wrong_document = valid.clone();
    wrong_document.document_id = DocumentId::new("other-document").unwrap();
    cases.push(wrong_document);
    let mut wrong_before_version = valid.clone();
    wrong_before_version.version_before = 9;
    wrong_before_version.version_after = 10;
    cases.push(wrong_before_version);
    let mut skipped_version = valid.clone();
    skipped_version.version_after = 2;
    cases.push(skipped_version);
    let mut wrong_selection = valid.clone();
    wrong_selection.selection_before = SelectionState::caret(1);
    cases.push(wrong_selection);
    let mut invalid_after_selection = valid.clone();
    invalid_after_selection.selection_after = SelectionState::caret(99);
    cases.push(invalid_after_selection);
    let mut wrong_before_hash = valid.clone();
    wrong_before_hash.hash_before = Hash::zero();
    cases.push(wrong_before_hash);
    let mut wrong_after_hash = valid.clone();
    wrong_after_hash.hash_after = Hash::zero();
    cases.push(wrong_after_hash);
    let mut split_code_point = valid.clone();
    split_code_point.edits = vec![edit(2, 3, "E")];
    cases.push(split_code_point);
    let mut unsorted = valid.clone();
    unsorted.edits = vec![edit(3, 3, "!"), edit(0, 0, "?")];
    cases.push(unsorted);
    let mut overlapping = valid.clone();
    overlapping.edits = vec![edit(0, 2, "x"), edit(1, 3, "y")];
    cases.push(overlapping);
    let mut too_many = valid.clone();
    too_many.edits = (0..=MAX_VECTOR_ITEMS).map(|_| edit(0, 0, "x")).collect();
    cases.push(too_many);
    let mut too_much_text = valid.clone();
    too_much_text.edits = vec![edit(0, 0, &"x".repeat(MAX_INSERTED_TEXT_BYTES + 1))];
    cases.push(too_much_text);

    for transaction in cases {
        let effects = RecordingEffects::default();
        let rejected = effects.clone();
        let mut editor = EditorBuffer::new(document_id(), "aéz", effects);
        let before = (
            editor.text(),
            editor.version(),
            editor.selection_state(),
            editor.can_undo(),
            editor.can_redo(),
        );

        assert!(editor.apply_transaction(transaction).is_err());
        assert_eq!(
            (
                editor.text(),
                editor.version(),
                editor.selection_state(),
                editor.can_undo(),
                editor.can_redo(),
            ),
            before
        );
        assert!(rejected.calls().is_empty());
    }
}

#[test]
fn invalid_transaction_preserves_existing_redo_history() {
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "abc", effects);
    editor.set_selection(SelectionState::caret(3)).unwrap();
    assert!(editor.insert_char('d').unwrap());
    assert!(editor.undo().unwrap());
    let calls_before = observed.calls().len();
    let state_before = (
        editor.text(),
        editor.version(),
        editor.selection_state(),
        editor.can_undo(),
        editor.can_redo(),
    );
    let invalid = EditorTransaction {
        document_id: editor.document_id().clone(),
        version_before: editor.version(),
        version_after: editor.version() + 1,
        origin: EditOrigin::Completion,
        edits: vec![edit(99, 99, "x")],
        selection_before: editor.selection_state(),
        selection_after: editor.selection_state(),
        hash_before: editor.hash(),
        hash_after: Hash::zero(),
    };

    assert!(editor.apply_transaction(invalid).is_err());
    assert_eq!(
        (
            editor.text(),
            editor.version(),
            editor.selection_state(),
            editor.can_undo(),
            editor.can_redo(),
        ),
        state_before
    );
    assert_eq!(observed.calls().len(), calls_before);
    assert!(editor.redo().unwrap());
    assert_eq!(editor.text(), "abcd");
}

#[test]
fn touching_ranges_and_equal_offset_insertions_have_deterministic_semantics() {
    let mut editor = EditorBuffer::new(document_id(), "abc", NoopEditorEffects);
    assert!(
        editor
            .apply_edits(
                EditOrigin::Formatter,
                vec![edit(0, 1, ""), edit(1, 2, "")],
                SelectionState::caret(0),
            )
            .unwrap()
    );
    assert_eq!(editor.text(), "c");
    assert!(editor.undo().unwrap());
    assert_eq!(editor.text(), "abc");
    assert!(editor.redo().unwrap());
    assert_eq!(editor.text(), "c");

    let mut editor = EditorBuffer::new(document_id(), "", NoopEditorEffects);
    assert!(
        editor
            .apply_edits(
                EditOrigin::AdditionalCompletionEdit,
                vec![edit(0, 0, "a"), edit(0, 0, "界")],
                SelectionState::caret(4),
            )
            .unwrap()
    );
    assert_eq!(editor.text(), "a界");
    assert!(editor.undo().unwrap());
    assert_eq!(editor.text(), "");
    assert!(editor.redo().unwrap());
    assert_eq!(editor.text(), "a界");
}

#[test]
fn undo_and_redo_round_trip_unicode_multi_edits_and_selections() {
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "aé-猫-z", effects);
    editor.set_selection(SelectionState::new(1, 7)).unwrap();

    assert!(
        editor
            .apply_edits(
                EditOrigin::CodeAction,
                vec![edit(1, 3, "E"), edit(4, 7, "🦀")],
                SelectionState::new(2, 8),
            )
            .unwrap()
    );
    let changed = editor.text();
    let changed_selection = editor.selection_state();
    assert_eq!(changed, "aE-🦀-z");

    editor.set_selection(SelectionState::caret(0)).unwrap();
    assert!(editor.undo().unwrap());
    assert_eq!(editor.text(), "aé-猫-z");
    assert_eq!(editor.selection_state(), SelectionState::new(1, 7));
    editor.set_selection(SelectionState::caret(0)).unwrap();
    assert!(editor.redo().unwrap());
    assert_eq!(editor.text(), changed);
    assert_eq!(editor.selection_state(), changed_selection);

    let transactions = observed
        .calls()
        .into_iter()
        .filter(|call| call.consumer == Consumer::Provenance)
        .map(|call| call.transaction)
        .collect::<Vec<_>>();
    assert_eq!(transactions.len(), 3);
    assert_eq!(
        transactions
            .iter()
            .map(|transaction| transaction.origin)
            .collect::<Vec<_>>(),
        [EditOrigin::CodeAction, EditOrigin::Undo, EditOrigin::Redo]
    );
    assert_eq!(transactions[0].hash_before, transactions[1].hash_after);
    assert_eq!(transactions[0].hash_after, transactions[1].hash_before);
    assert_eq!(transactions[0].hash_before, transactions[2].hash_before);
    assert_eq!(transactions[0].hash_after, transactions[2].hash_after);
    assert_eq!(transactions[1].selection_before, SelectionState::caret(0));
    assert_eq!(transactions[1].selection_after, SelectionState::new(1, 7));
    assert_eq!(transactions[2].selection_before, SelectionState::caret(0));
    assert_eq!(transactions[2].selection_after, changed_selection);

    assert!(editor.undo().unwrap());
    assert!(
        editor
            .apply_edits(
                EditOrigin::ExternalChange,
                vec![edit(0, 0, "new-")],
                SelectionState::caret(4),
            )
            .unwrap()
    );
    assert!(!editor.can_redo());
}

#[test]
fn no_op_edits_do_not_change_document_state_or_emit_effects() {
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "é", effects);

    assert!(
        !editor
            .apply_edits(
                EditOrigin::FileReload,
                vec![edit(0, 2, "é")],
                SelectionState::caret(2),
            )
            .unwrap()
    );
    assert_eq!(editor.version(), 0);
    assert_eq!(editor.selection_state(), SelectionState::caret(0));
    assert!(!editor.can_undo());
    assert!(observed.calls().is_empty());
}

#[test]
fn every_programmatic_origin_uses_the_transaction_path() {
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "", effects);
    let origins = [
        EditOrigin::Completion,
        EditOrigin::AdditionalCompletionEdit,
        EditOrigin::Formatter,
        EditOrigin::CodeAction,
        EditOrigin::FileReload,
        EditOrigin::ExternalChange,
        EditOrigin::Unknown,
    ];

    for (index, origin) in origins.into_iter().enumerate() {
        let offset = editor.text().len() as u64;
        assert!(
            editor
                .apply_edits(
                    origin,
                    vec![edit(offset, offset, &(index + 1).to_string())],
                    SelectionState::caret(offset + 1),
                )
                .unwrap()
        );
    }

    let recorded_origins = observed
        .calls()
        .into_iter()
        .filter(|call| call.consumer == Consumer::Provenance)
        .map(|call| call.transaction.origin)
        .collect::<Vec<_>>();
    assert_eq!(recorded_origins, origins);
    assert_eq!(editor.version(), origins.len() as u64);
}

#[test]
fn document_hash_has_a_pinned_domain_separated_vector() {
    assert_eq!(
        document_hash("exact UTF-8 🦀 bytes").to_string(),
        "0213ea52ad0abf84b128f51929a04521b63e753cd00a60683473a78edfa7716c"
    );
    assert_ne!(document_hash("é"), document_hash("e\u{301}"));
}

#[test]
fn enter_copies_tab_indentation_and_crlf_as_one_keyboard_transaction() {
    let initial = "fn α() {\r\n\tif ready {";
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), initial, effects);
    editor.move_cursor(Movement::DocumentEnd, false);

    assert!(editor.insert_char('\n').unwrap());
    assert_eq!(editor.text(), "fn α() {\r\n\tif ready {\r\n\t\t");
    assert_eq!(
        editor.selection_state(),
        SelectionState::caret((initial.len() + "\r\n\t\t".len()) as u64)
    );

    let transactions = observed
        .calls()
        .into_iter()
        .filter(|call| call.consumer == Consumer::Provenance)
        .map(|call| call.transaction)
        .collect::<Vec<_>>();
    assert_eq!(transactions.len(), 1);
    assert_eq!(transactions[0].origin, EditOrigin::Keyboard);
    assert_eq!(
        transactions[0].edits,
        [edit(initial.len() as u64, initial.len() as u64, "\r\n\t\t")]
    );

    assert!(editor.undo().unwrap());
    assert_eq!(editor.text(), initial);

    let initial = "if ready {\u{a0}";
    let mut editor = EditorBuffer::new(document_id(), initial, NoopEditorEffects);
    editor.move_cursor(Movement::DocumentEnd, false);
    assert!(editor.insert_char('\n').unwrap());
    assert_eq!(editor.text(), "if ready {\u{a0}\n    ");
}

#[test]
fn enter_between_pairs_places_the_closer_at_original_indent_and_replaces_selection_plainly() {
    let initial = "fn α() {\r\n    call() {}\r\n}";
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), initial, effects);
    let between = (initial.find("{}\r\n").unwrap() + 1) as u64;
    editor
        .set_selection(SelectionState::caret(between))
        .unwrap();

    assert!(editor.insert_char('\n').unwrap());
    assert_eq!(
        editor.text(),
        "fn α() {\r\n    call() {\r\n        \r\n    }\r\n}"
    );
    assert_eq!(
        editor.selection_state(),
        SelectionState::caret(between + "\r\n        ".len() as u64)
    );
    let transactions = observed
        .calls()
        .into_iter()
        .filter(|call| call.consumer == Consumer::Provenance)
        .map(|call| call.transaction)
        .collect::<Vec<_>>();
    assert_eq!(transactions.len(), 1);
    assert_eq!(
        transactions[0].edits,
        [edit(between, between, "\r\n        \r\n    ")]
    );
    assert!(editor.undo().unwrap());
    assert_eq!(editor.text(), initial);

    let mut selected = EditorBuffer::new(document_id(), "    α{}", NoopEditorEffects);
    selected.set_selection(SelectionState::new(4, 6)).unwrap();
    assert!(selected.insert_char('\n').unwrap());
    assert_eq!(selected.text(), "    \n{}");
    assert_eq!(selected.selection_state(), SelectionState::caret(5));
}

#[test]
fn a_closer_on_whitespace_dedents_to_its_matching_opening_line_in_one_unit() {
    for (opener, closer) in [('{', '}'), ('(', ')'), ('[', ']')] {
        let initial = format!("fn α() {{\r\n    let value = {opener}\r\n        ");
        let expected = format!("fn α() {{\r\n    let value = {opener}\r\n    {closer}");
        let effects = RecordingEffects::default();
        let observed = effects.clone();
        let mut editor = EditorBuffer::new(document_id(), &initial, effects);
        editor.move_cursor(Movement::DocumentEnd, false);

        assert!(editor.insert_char(closer).unwrap());
        assert_eq!(editor.text(), expected);
        let transactions = observed
            .calls()
            .into_iter()
            .filter(|call| call.consumer == Consumer::Provenance)
            .map(|call| call.transaction)
            .collect::<Vec<_>>();
        assert_eq!(transactions.len(), 1, "{closer}");
        assert_eq!(transactions[0].origin, EditOrigin::Keyboard);
        assert_eq!(transactions[0].edits.len(), 1);
        assert_eq!(
            transactions[0].edits[0].inserted_text,
            format!("    {closer}")
        );

        assert!(editor.undo().unwrap());
        assert_eq!(editor.text(), initial);
    }

    let initial = "if ready {\n    \u{a0}";
    let mut editor = EditorBuffer::new(document_id(), initial, NoopEditorEffects);
    editor.move_cursor(Movement::DocumentEnd, false);
    assert!(editor.insert_char('}').unwrap());
    assert_eq!(editor.text(), "if ready {\n}");
}

#[test]
fn auto_close_overtype_and_pair_backspace_are_atomic_keyboard_actions() {
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "🦀 ", effects);
    editor.move_cursor(Movement::DocumentEnd, false);

    assert!(editor.insert_char('(').unwrap());
    assert_eq!(editor.text(), "🦀 ()");
    assert_eq!(editor.selection_state(), SelectionState::caret(6));
    assert!(
        !editor.insert_char(')').unwrap(),
        "over-typing changes only the caret"
    );
    assert_eq!(editor.text(), "🦀 ()");
    assert_eq!(editor.selection_state(), SelectionState::caret(7));

    let transactions = observed
        .calls()
        .into_iter()
        .filter(|call| call.consumer == Consumer::Provenance)
        .map(|call| call.transaction)
        .collect::<Vec<_>>();
    assert_eq!(transactions.len(), 1);
    assert_eq!(transactions[0].origin, EditOrigin::Keyboard);
    assert_eq!(transactions[0].edits, [edit(5, 5, "()")]);
    assert_eq!(
        replay_transactions(document_id(), "🦀 ", &transactions, NoopEditorEffects)
            .unwrap()
            .text(),
        editor.text()
    );

    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), "", effects);
    assert!(editor.insert_char('{').unwrap());
    assert_eq!(editor.text(), "{}");
    assert_eq!(editor.selection_state(), SelectionState::caret(1));
    assert!(editor.delete_backward().unwrap());
    assert_eq!(editor.text(), "");
    let transactions = observed
        .calls()
        .into_iter()
        .filter(|call| call.consumer == Consumer::Provenance)
        .map(|call| call.transaction)
        .collect::<Vec<_>>();
    assert_eq!(transactions.len(), 2);
    assert_eq!(transactions[0].edits, [edit(0, 0, "{}")]);
    assert_eq!(transactions[1].edits, [edit(0, 2, "")]);
    assert!(editor.undo().unwrap());
    assert_eq!(editor.text(), "{}");
    assert_eq!(editor.selection_state(), SelectionState::caret(1));
}

#[test]
fn auto_close_context_rules_and_transient_state_are_explicit() {
    for (opener, expected) in [('(', "()"), ('[', "[]"), ('{', "{}"), ('"', "\"\"")] {
        let mut editor = EditorBuffer::new(document_id(), " ", NoopEditorEffects);
        editor.move_cursor(Movement::DocumentEnd, false);
        assert!(editor.insert_char(opener).unwrap());
        assert_eq!(editor.text(), format!(" {expected}"), "{opener}");
    }

    for (initial, character, expected) in [
        ("word", '"', "word\""),
        ("e\u{301}", '"', "e\u{301}\""),
        ("\"inside", '"', "\"inside\""),
        ("next", '(', "(next"),
    ] {
        let mut editor = EditorBuffer::new(document_id(), initial, NoopEditorEffects);
        if initial != "next" {
            editor.move_cursor(Movement::DocumentEnd, false);
        }
        assert!(editor.insert_char(character).unwrap());
        assert_eq!(editor.text(), expected, "{initial:?} {character}");
    }

    let mut navigated = EditorBuffer::new(document_id(), "", NoopEditorEffects);
    assert!(navigated.insert_char('[').unwrap());
    navigated.move_cursor(Movement::Left, false);
    navigated.move_cursor(Movement::Right, false);
    assert!(navigated.insert_char(']').unwrap());
    assert_eq!(navigated.text(), "[]]");

    let mut edited = EditorBuffer::new(document_id(), "", NoopEditorEffects);
    assert!(edited.insert_char('(').unwrap());
    assert!(edited.insert_char('x').unwrap());
    assert!(edited.insert_char(')').unwrap());
    assert_eq!(edited.text(), "(x))");
}

#[test]
fn quote_auto_close_tracks_multiline_ordinary_and_raw_strings() {
    for (case, initial) in [
        (
            "ordinary LF after whitespace with an escaped quote",
            "let s = \"escaped \\\" quote\nclose ",
        ),
        (
            "ordinary CRLF after punctuation with an escaped backslash",
            "let s = \"escaped \\\\ path\r\nclose,",
        ),
        (
            "zero-hash raw LF after whitespace",
            "let s = r\"first\nclose ",
        ),
        (
            "zero-hash raw CRLF after punctuation",
            "let s = r\"first\r\nclose,",
        ),
        (
            "hash raw LF after whitespace with embedded quotes",
            "let s = r#\"first \"quoted\"\nclose ",
        ),
        (
            "hash raw CRLF after punctuation with embedded quotes",
            "let s = r#\"first \"quoted\"\r\nclose,",
        ),
    ] {
        let effects = RecordingEffects::default();
        let observed = effects.clone();
        let mut editor = EditorBuffer::new(document_id(), initial, effects);
        editor.move_cursor(Movement::DocumentEnd, false);

        assert!(editor.insert_char('"').unwrap(), "{case}");
        assert_eq!(editor.text(), format!("{initial}\""), "{case}");
        let transactions = observed
            .calls()
            .into_iter()
            .filter(|call| call.consumer == Consumer::Provenance)
            .map(|call| call.transaction)
            .collect::<Vec<_>>();
        assert_eq!(transactions.len(), 1, "{case}");
        assert_eq!(transactions[0].origin, EditOrigin::Keyboard, "{case}");
        assert_eq!(
            transactions[0].edits,
            [edit(initial.len() as u64, initial.len() as u64, "\"")],
            "{case}"
        );
    }

    let initial = "let s = \nclose ";
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut edited = EditorBuffer::new(document_id(), initial, effects);
    edited
        .set_selection(SelectionState::caret("let s = ".len() as u64))
        .unwrap();
    assert!(edited.insert_char('"').unwrap());
    assert!(edited.delete_forward().unwrap());
    edited.move_cursor(Movement::DocumentEnd, false);
    let transactions_before_close = observed
        .calls()
        .into_iter()
        .filter(|call| call.consumer == Consumer::Provenance)
        .count();

    assert!(edited.insert_char('"').unwrap());
    assert_eq!(edited.text(), "let s = \"\nclose \"");
    let transactions = observed
        .calls()
        .into_iter()
        .filter(|call| call.consumer == Consumer::Provenance)
        .map(|call| call.transaction)
        .collect::<Vec<_>>();
    assert_eq!(transactions.len(), transactions_before_close + 1);
    assert_eq!(transactions.last().unwrap().origin, EditOrigin::Keyboard);
    assert_eq!(transactions.last().unwrap().edits.len(), 1);
    assert_eq!(transactions.last().unwrap().edits[0].inserted_text, "\"");
}

#[test]
fn comment_toggle_uses_common_indentation_and_one_replayable_keyboard_transaction() {
    let initial = "    α();\r\n        β();\r\nplain\r\n";
    let selected_end = initial.find("plain").unwrap() as u64;
    let effects = RecordingEffects::default();
    let observed = effects.clone();
    let mut editor = EditorBuffer::new(document_id(), initial, effects);
    editor
        .set_selection(SelectionState::new(selected_end, 0))
        .unwrap();

    assert!(editor.toggle_line_comment().unwrap());
    assert_eq!(editor.text(), "    // α();\r\n    //     β();\r\nplain\r\n");
    assert_eq!(
        editor.selection_state(),
        SelectionState::new(selected_end + 6, 0)
    );
    assert!(editor.toggle_line_comment().unwrap());
    assert_eq!(editor.text(), initial);
    assert_eq!(
        editor.selection_state(),
        SelectionState::new(selected_end, 0)
    );

    let transactions = observed
        .calls()
        .into_iter()
        .filter(|call| call.consumer == Consumer::Provenance)
        .map(|call| call.transaction)
        .collect::<Vec<_>>();
    assert_eq!(transactions.len(), 2);
    assert!(transactions.iter().all(|transaction| {
        transaction.origin == EditOrigin::Keyboard && transaction.edits.len() == 2
    }));
    assert_eq!(transactions[0].edits[0], edit(4, 4, "// "));
    assert_eq!(transactions[0].edits[1].inserted_text, "// ");
    assert_eq!(transactions[1].edits[0].inserted_text, "");
    assert_eq!(transactions[1].edits[1].inserted_text, "");
    let replayed =
        replay_transactions(document_id(), initial, &transactions, NoopEditorEffects).unwrap();
    assert_eq!(replayed.text(), initial);
    assert_eq!(replayed.selection_state(), editor.selection_state());

    assert!(editor.undo().unwrap());
    assert_eq!(editor.text(), "    // α();\r\n    //     β();\r\nplain\r\n");
}

#[test]
fn comment_toggle_removes_spaced_or_bare_markers_and_comments_mixed_lines() {
    let initial = "\t// α\r\n\t//β";
    let mut editor = EditorBuffer::new(document_id(), initial, NoopEditorEffects);
    editor
        .set_selection(SelectionState::new(0, initial.len() as u64))
        .unwrap();
    assert!(editor.toggle_line_comment().unwrap());
    assert_eq!(editor.text(), "\tα\r\n\tβ");

    let mixed = "  // first\n    second";
    let mut editor = EditorBuffer::new(document_id(), mixed, NoopEditorEffects);
    editor
        .set_selection(SelectionState::new(0, mixed.len() as u64))
        .unwrap();
    assert!(editor.toggle_line_comment().unwrap());
    assert_eq!(editor.text(), "  // // first\n  //   second");
}

#[test]
fn matching_bracket_uses_the_caret_or_previous_cell_without_mutating_state() {
    let source = "α({[x] }) // [comment]\r\n\"(string)\"";
    let mut editor = EditorBuffer::new(document_id(), source, NoopEditorEffects);
    let version = editor.version();
    let hash = editor.hash();

    let opening_brace = source.find('{').unwrap();
    let closing_brace = source.find('}').unwrap();
    editor
        .set_selection(SelectionState::caret(opening_brace as u64))
        .unwrap();
    assert_eq!(editor.matching_bracket_byte(), Some(closing_brace as u64));

    let opening_square = source.find('[').unwrap();
    let closing_square = source.find(']').unwrap();
    editor
        .set_selection(SelectionState::caret((closing_square + 1) as u64))
        .unwrap();
    assert_eq!(editor.matching_bracket_byte(), Some(opening_square as u64));

    let comment_opening = source.find("[comment]").unwrap();
    let comment_closing = comment_opening + "[comment".len();
    editor
        .set_selection(SelectionState::caret(comment_opening as u64))
        .unwrap();
    assert_eq!(
        editor.matching_bracket_byte(),
        Some(comment_closing as u64),
        "this slice intentionally does not parse strings or comments"
    );

    editor
        .set_selection(SelectionState::caret(source.find('α').unwrap() as u64))
        .unwrap();
    assert_eq!(editor.matching_bracket_byte(), None);
    assert_eq!(editor.text(), source);
    assert_eq!(editor.version(), version);
    assert_eq!(editor.hash(), hash);

    let mut unmatched = EditorBuffer::new(document_id(), "(α", NoopEditorEffects);
    unmatched.set_selection(SelectionState::caret(0)).unwrap();
    assert_eq!(unmatched.matching_bracket_byte(), None);
}
