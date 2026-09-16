use proptest::prelude::*;
use rustrace_editor::display::grapheme_width;
use rustrace_editor::position::{
    ByteOffset, PositionError, ScalarIndex, Utf16Position, VisualPosition,
};
use rustrace_editor::{EditOrigin, EditorBuffer, Movement, NoopEditorEffects, SelectionState};
use rustrace_model::{DocumentId, TextEdit};

fn editor(text: &str) -> EditorBuffer<NoopEditorEffects> {
    EditorBuffer::new(
        DocumentId::new("positions").unwrap(),
        text,
        NoopEditorEffects,
    )
}

fn lsp(line: u32, character: u32) -> Utf16Position {
    Utf16Position { line, character }
}

fn visual(line: usize, column: usize) -> VisualPosition {
    VisualPosition { line, column }
}

#[test]
fn multilingual_offsets_have_distinct_units() {
    let text = "// naïve implementation\nlet 名称 = \"東京😀\";";
    let buffer = editor(text);
    let positions = buffer.positions(0).unwrap();
    let byte = ByteOffset(text.find('😀').unwrap() as u64);
    let scalar = ScalarIndex(text[..byte.0 as usize].chars().count());
    assert_eq!(positions.byte_to_scalar(byte), Ok(scalar));
    assert_eq!(positions.scalar_to_byte(scalar), Ok(byte));
    assert_eq!(positions.byte_to_utf16(byte), Ok(lsp(1, 12)));
    assert_eq!(positions.utf16_to_byte(lsp(1, 12)), Ok(byte));
    assert_eq!(positions.byte_to_visual(byte), Ok(visual(1, 16)));
    assert_eq!(positions.visual_to_byte(visual(1, 16)), Ok(byte));
    assert_eq!(
        positions.byte_to_utf16(ByteOffset(byte.0 + 4)),
        Ok(lsp(1, 14))
    );
    assert_eq!(
        positions.utf16_to_byte(lsp(1, 13)),
        Err(PositionError::NotUtf16Boundary)
    );
    assert_eq!(buffer.text(), text);
}

#[test]
fn literal_matches_are_case_sensitive_and_exact_across_unicode_and_crlf() {
    let text = "cat α cat\r\n猫 cat CAT\r\n";
    let buffer = editor(text);
    let matches = buffer.literal_matches("cat");

    assert_eq!(
        matches,
        [
            SelectionState::new(0, 3),
            SelectionState::new(7, 10),
            SelectionState::new(16, 19),
        ]
    );
    assert!(buffer.literal_matches("").is_empty());
    assert!(buffer.literal_matches("Cat").is_empty());

    let positions = buffer.positions(0).unwrap();
    let visual_ranges = matches
        .iter()
        .map(|selection| {
            (
                positions
                    .byte_to_visual(ByteOffset(selection.anchor_byte))
                    .unwrap(),
                positions
                    .byte_to_visual(ByteOffset(selection.active_byte))
                    .unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        visual_ranges,
        [
            (visual(0, 0), visual(0, 3)),
            (visual(0, 6), visual(0, 9)),
            (visual(1, 3), visual(1, 6)),
        ]
    );
}

#[test]
fn exact_bounds_reject_invalid_input_instead_of_panicking_or_clamping() {
    let buffer = editor("é😀");
    let p = buffer.positions(0).unwrap();
    for byte in [1, 3, 4, 5] {
        assert_eq!(
            p.byte_to_scalar(ByteOffset(byte)),
            Err(PositionError::NotUtf8Boundary)
        );
        assert_eq!(
            p.byte_to_utf16(ByteOffset(byte)),
            Err(PositionError::NotUtf8Boundary)
        );
        assert_eq!(
            p.byte_to_visual(ByteOffset(byte)),
            Err(PositionError::NotUtf8Boundary)
        );
    }
    for byte in [7, u64::MAX] {
        assert_eq!(
            p.byte_to_scalar(ByteOffset(byte)),
            Err(PositionError::ByteOutOfBounds)
        );
        assert_eq!(
            p.byte_to_utf16(ByteOffset(byte)),
            Err(PositionError::ByteOutOfBounds)
        );
        assert_eq!(
            p.byte_to_visual(ByteOffset(byte)),
            Err(PositionError::ByteOutOfBounds)
        );
    }
    assert_eq!(
        p.scalar_to_byte(ScalarIndex(3)),
        Err(PositionError::ScalarOutOfBounds)
    );
    assert!(p.scalar_to_byte(ScalarIndex(usize::MAX)).is_err());
    assert_eq!(
        p.utf16_to_byte(lsp(0, 4)),
        Err(PositionError::ColumnOutOfBounds)
    );
    assert_eq!(
        p.utf16_to_byte(lsp(1, 0)),
        Err(PositionError::LineOutOfBounds)
    );
    assert_eq!(
        p.utf16_to_byte(lsp(0, u32::MAX)),
        Err(PositionError::LspIntegerOverflow)
    );
    assert_eq!(
        p.utf16_to_byte(lsp(u32::MAX, 0)),
        Err(PositionError::LspIntegerOverflow)
    );
    assert!(p.visual_to_byte(visual(usize::MAX, 0)).is_err());
    assert!(p.visual_to_byte(visual(0, usize::MAX)).is_err());
    assert_eq!(p.scalar_to_byte(ScalarIndex(2)), Ok(ByteOffset(6)));
    assert_eq!(p.utf16_to_byte(lsp(0, 3)), Ok(ByteOffset(6)));
}

#[test]
fn empty_trailing_and_mixed_line_endings_preserve_exact_bytes() {
    for text in ["", "\n", "\r", "\r\n", "a\r\n\r\nb\rc\n", "名称\r\n東京\n"] {
        let buffer = editor(text);
        let p = buffer.positions(0).unwrap();
        for (scalar, byte) in text
            .char_indices()
            .map(|(b, _)| b)
            .chain([text.len()])
            .enumerate()
        {
            let byte = ByteOffset(byte as u64);
            assert_eq!(p.byte_to_scalar(byte), Ok(ScalarIndex(scalar)));
            assert_eq!(p.scalar_to_byte(ScalarIndex(scalar)), Ok(byte));
            let index = byte.0 as usize;
            if index > 0
                && text.as_bytes().get(index) == Some(&b'\n')
                && text.as_bytes()[index - 1] == b'\r'
            {
                assert_eq!(p.byte_to_utf16(byte), Err(PositionError::InsideLineEnding));
                assert_eq!(p.byte_to_visual(byte), Err(PositionError::InsideLineEnding));
            } else {
                assert_eq!(p.utf16_to_byte(p.byte_to_utf16(byte).unwrap()), Ok(byte));
            }
        }
        assert_eq!(buffer.text().as_bytes(), text.as_bytes());
    }
    let buffer = editor("a\r\n\r\nb\rc\n");
    let p = buffer.positions(0).unwrap();
    for (byte, position) in [
        (1, lsp(0, 1)),
        (3, lsp(1, 0)),
        (5, lsp(2, 0)),
        (7, lsp(3, 0)),
        (9, lsp(4, 0)),
    ] {
        assert_eq!(p.byte_to_utf16(ByteOffset(byte)), Ok(position));
        assert_eq!(p.utf16_to_byte(position), Ok(ByteOffset(byte)));
    }
    assert_eq!(
        p.utf16_to_byte(lsp(1, 1)),
        Err(PositionError::ColumnOutOfBounds)
    );
    let empty = editor("");
    assert_eq!(
        empty.positions(0).unwrap().visual_to_byte(visual(0, 0)),
        Ok(ByteOffset(0))
    );
}

#[test]
fn visual_cells_do_not_split_graphemes_tabs_or_wide_glyphs() {
    let text = "a\te\u{301}界👩‍💻🇯🇵";
    let buffer = editor(text);
    let p = buffer.positions(0).unwrap();
    let boundaries = [(0, 0), (1, 1), (2, 4), (5, 5), (8, 7), (19, 9), (27, 11)];
    for (byte, column) in boundaries {
        assert_eq!(p.byte_to_visual(ByteOffset(byte)), Ok(visual(0, column)));
        assert_eq!(p.visual_to_byte(visual(0, column)), Ok(ByteOffset(byte)));
    }
    for column in [2, 3, 6, 8, 10] {
        assert_eq!(
            p.visual_to_byte(visual(0, column)),
            Err(PositionError::NonAddressableVisualColumn)
        );
    }
    for byte in [3, 12, 15, 23] {
        assert_eq!(
            p.byte_to_visual(ByteOffset(byte)),
            Err(PositionError::NotGraphemeBoundary)
        );
        assert!(
            p.byte_to_utf16(ByteOffset(byte)).is_ok(),
            "protocol permits scalar boundaries inside graphemes"
        );
    }
    assert_eq!(
        p.visual_to_byte(visual(0, 12)),
        Err(PositionError::ColumnOutOfBounds)
    );
}

#[test]
fn safe_visual_expansions_are_atomic_and_keep_original_protocol_positions() {
    let text = "a\u{1b}b\u{202e}c";
    let buffer = editor(text);
    let p = buffer.positions(0).unwrap();
    for (byte, column, utf16) in [
        (0, 0, 0),
        (1, 1, 1),
        (2, 7, 2),
        (3, 8, 3),
        (6, 16, 4),
        (7, 17, 5),
    ] {
        assert_eq!(p.byte_to_visual(ByteOffset(byte)), Ok(visual(0, column)));
        assert_eq!(p.visual_to_byte(visual(0, column)), Ok(ByteOffset(byte)));
        assert_eq!(p.byte_to_utf16(ByteOffset(byte)), Ok(lsp(0, utf16)));
    }
    for column in (2..7).chain(9..16) {
        assert_eq!(
            p.visual_to_byte(visual(0, column)),
            Err(PositionError::NonAddressableVisualColumn)
        );
    }
}

#[test]
fn interactive_visual_moves_floor_graphemes_and_clamp_lines_and_columns() {
    let mut buffer = editor("a\t界e\u{301}\nq\u{1b}z");
    for (line, column, expected_byte) in [
        (0, 0, 0),
        (0, 2, 1),
        (0, 5, 2),
        (0, 6, 5),
        (0, 99, 8),
        (1, 3, 10),
        (usize::MAX, usize::MAX, 12),
    ] {
        buffer.move_to_visual(line, column, false);
        assert_eq!(
            buffer.selection_state(),
            SelectionState::caret(expected_byte),
            "unexpected target for line {line}, column {column}"
        );
    }

    buffer.move_to_visual(0, 0, false);
    buffer.move_to_visual(0, 5, true);
    assert_eq!(buffer.selection_state(), SelectionState::new(0, 2));
}

#[test]
fn word_ranges_use_unicode_boundaries_and_grapheme_safe_fallbacks() {
    let buffer = editor("one café noir!\nβeta test");
    assert_eq!(buffer.word_range_at(0, 5), SelectionState::new(4, 9));
    assert_eq!(buffer.word_range_at(1, 1), SelectionState::new(16, 21));
    assert_eq!(buffer.word_range_at(0, 13), SelectionState::new(14, 15));
    assert_eq!(
        buffer.word_range_at(usize::MAX, usize::MAX),
        SelectionState::caret(buffer.len_bytes() as u64)
    );
}

#[test]
fn modern_word_movements_skip_whitespace_and_preserve_unicode_crlf_boundaries() {
    let mut buffer = editor("one,  cafe\u{301}\r\nβeta !");

    for (movement, expected) in [
        (Movement::PreviousWord, 0),
        (Movement::NextWord, 3),
        (Movement::NextWord, 4),
        (Movement::NextWord, 12),
        (Movement::NextWord, 19),
        (Movement::NextWord, 21),
        (Movement::NextWord, 21),
    ] {
        buffer.move_cursor(movement, false);
        assert_eq!(buffer.selection_state(), SelectionState::caret(expected));
    }

    for expected in [20, 14, 6, 3, 0, 0] {
        buffer.move_cursor(Movement::PreviousWord, false);
        assert_eq!(buffer.selection_state(), SelectionState::caret(expected));
    }

    buffer.set_selection(SelectionState::caret(3)).unwrap();
    buffer.move_cursor(Movement::NextWord, true);
    assert_eq!(buffer.selection_state(), SelectionState::new(3, 4));
    buffer.move_cursor(Movement::NextWord, true);
    assert_eq!(buffer.selection_state(), SelectionState::new(3, 12));
    buffer.move_cursor(Movement::PreviousWord, true);
    assert_eq!(buffer.selection_state(), SelectionState::new(3, 6));
}

#[test]
fn invisible_clusters_are_visible_and_unicode_line_rules_are_explicit() {
    let buffer = editor("\u{301}a\u{200b}\t\u{2028}東京");
    let p = buffer.positions(0).unwrap();
    assert_eq!(p.byte_to_visual(ByteOffset(0)), Ok(visual(0, 0)));
    assert_eq!(p.visual_to_byte(visual(0, 0)), Ok(ByteOffset(0)));
    assert_eq!(p.byte_to_visual(ByteOffset(2)), Ok(visual(0, 7)));
    assert_eq!(p.byte_to_visual(ByteOffset(3)), Ok(visual(0, 8)));
    assert_eq!(p.byte_to_visual(ByteOffset(6)), Ok(visual(0, 16)));
    assert_eq!(p.byte_to_visual(ByteOffset(7)), Ok(visual(0, 20)));
    for column in (1..7).chain(9..16).chain(17..20) {
        assert_eq!(
            p.visual_to_byte(visual(0, column)),
            Err(PositionError::NonAddressableVisualColumn)
        );
    }
    assert_eq!(p.byte_to_visual(ByteOffset(10)), Ok(visual(1, 0)));
    assert_eq!(p.byte_to_utf16(ByteOffset(10)), Ok(lsp(0, 5)));
    assert_eq!(
        p.utf16_to_byte(lsp(1, 0)),
        Err(PositionError::LineOutOfBounds)
    );
}

#[test]
fn native_halfwidth_marks_and_bounded_clusters_define_visual_columns() {
    for (cluster, width) in [
        ("ｶﾞ", 2),
        ("ﾊﾟ", 2),
        ("aﾞ", 2),
        ("aﾟ", 2),
        ("ﾞ", 1),
        ("ﾟ", 1),
        ("a\u{3099}", 1),
        ("a\u{309a}", 1),
        ("a\u{034f}ﾞ", 9),
    ] {
        let text = format!("{cluster}X");
        let buffer = editor(&text);
        let p = buffer.positions(0).unwrap();
        assert_eq!(
            p.byte_to_visual(ByteOffset(cluster.len() as u64)),
            Ok(visual(0, width)),
            "{cluster:?}"
        );
        assert_eq!(
            p.visual_to_byte(visual(0, width)),
            Ok(ByteOffset(cluster.len() as u64)),
            "{cluster:?}"
        );
    }

    let clusters = [
        (format!("e{}", "\u{301}".repeat(127)), 1),
        (format!("é{}", "\u{301}".repeat(127)), 1),
        (format!("e{}", "\u{301}".repeat(128)), 10),
    ];
    assert_eq!(clusters[0].0.len(), 255);
    assert_eq!(clusters[1].0.len(), 256);
    assert_eq!(clusters[2].0.len(), 257);
    for (cluster, width) in clusters {
        let text = format!("{cluster}X");
        let buffer = editor(&text);
        let p = buffer.positions(0).unwrap();
        assert_eq!(
            p.byte_to_visual(ByteOffset(cluster.len() as u64)),
            Ok(visual(0, width))
        );
        assert_eq!(
            p.visual_to_byte(visual(0, width)),
            Ok(ByteOffset(cluster.len() as u64))
        );
    }
}

#[test]
fn current_version_is_required_after_boundary_edits_undo_and_recovery() {
    let initial = "a😀\r\n東京";
    let mut buffer = editor(initial);
    let original = buffer
        .positions(0)
        .unwrap()
        .byte_to_utf16(ByteOffset(7))
        .unwrap();
    assert_eq!(original, lsp(1, 0));
    assert_eq!(
        buffer.positions(0).unwrap().document_id(),
        buffer.document_id()
    );
    assert_eq!(buffer.positions(0).unwrap().version(), 0);
    // Delete CR, preserving LF; insert a combining scalar at an existing boundary.
    buffer
        .apply_edits(
            EditOrigin::Keyboard,
            vec![
                TextEdit {
                    start_byte: 1,
                    end_byte: 1,
                    inserted_text: "\u{301}".into(),
                },
                TextEdit {
                    start_byte: 5,
                    end_byte: 6,
                    inserted_text: String::new(),
                },
            ],
            SelectionState::caret(0),
        )
        .unwrap();
    assert!(matches!(
        buffer.positions(0),
        Err(PositionError::StaleVersion {
            expected: 0,
            current: 1
        })
    ));
    assert_eq!(
        buffer.positions(1).unwrap().utf16_to_byte(original),
        Ok(ByteOffset(8))
    );
    assert_eq!(buffer.text(), "a\u{301}😀\n東京");
    assert!(buffer.undo().unwrap());
    assert_eq!(buffer.text(), initial);
    assert!(
        buffer.positions(0).is_err(),
        "same bytes after undo do not revive an old version"
    );
    assert_eq!(
        buffer.positions(2).unwrap().utf16_to_byte(original),
        Ok(ByteOffset(7))
    );
    assert!(buffer.redo().unwrap());
    assert!(buffer.positions(2).is_err());
    let recovered = EditorBuffer::from_recovered(
        buffer.document_id().clone(),
        &buffer.text(),
        91,
        SelectionState::caret(0),
        NoopEditorEffects,
    )
    .unwrap();
    assert!(recovered.positions(3).is_err());
    assert_eq!(
        recovered.positions(91).unwrap().utf16_to_byte(original),
        Ok(ByteOffset(8))
    );
    let version = buffer.version();
    assert!(
        buffer
            .apply_edits(
                EditOrigin::Keyboard,
                vec![TextEdit {
                    start_byte: 2,
                    end_byte: 2,
                    inserted_text: "x".into()
                }],
                SelectionState::caret(0)
            )
            .is_err()
    );
    assert!(
        buffer.positions(version).is_ok(),
        "rejected edit leaves version/context unchanged"
    );
    assert!(!buffer.paste("").unwrap());
    assert!(
        buffer.positions(version).is_ok(),
        "no-op leaves version/context unchanged"
    );
}

#[test]
fn rope_chunks_do_not_change_combining_emoji_or_line_boundaries() {
    let prefix = "naïve 名称 東京\r\n".repeat(100);
    // One grapheme spanning multiple Rope chunks, followed by an emoji and CRLF.
    let grapheme = format!("a{}", "\u{301}".repeat(1500));
    let text = format!("{prefix}{grapheme}👩‍💻\r\nend");
    let buffer = editor(&text);
    let p = buffer.positions(0).unwrap();
    let emoji_byte = ByteOffset((prefix.len() + grapheme.len()) as u64);
    assert_eq!(p.byte_to_utf16(emoji_byte), Ok(lsp(100, 1501)));
    assert_eq!(p.utf16_to_byte(lsp(100, 1501)), Ok(emoji_byte));
    assert_eq!(p.byte_to_visual(emoji_byte), Ok(visual(100, 10)));
    assert_eq!(p.visual_to_byte(visual(100, 10)), Ok(emoji_byte));
    assert_eq!(
        p.byte_to_visual(ByteOffset(emoji_byte.0 - 2)),
        Err(PositionError::NotGraphemeBoundary)
    );
    assert_eq!(
        p.byte_to_utf16(ByteOffset(emoji_byte.0 + 12)),
        Err(PositionError::InsideLineEnding)
    );
    assert_eq!(
        p.utf16_to_byte(lsp(101, 0)),
        Ok(ByteOffset(emoji_byte.0 + 13))
    );
}

#[test]
fn converted_unicode_replacement_uses_the_existing_transaction_and_replay_gateway() {
    let initial = "// naïve\r\nlet 名称 = \"東京😀\";\r\n";
    let mut buffer = editor(initial);
    let p = buffer.positions(0).unwrap();
    let start = p.utf16_to_byte(lsp(1, 10)).unwrap();
    let end = p.utf16_to_byte(lsp(1, 14)).unwrap();
    assert_eq!(&initial[start.0 as usize..end.0 as usize], "東京😀");
    let transaction = buffer
        .preview_edits(
            EditOrigin::Completion,
            vec![TextEdit {
                start_byte: start.0,
                end_byte: end.0,
                inserted_text: "大阪🦀".into(),
            }],
            SelectionState::caret(start.0 + "大阪🦀".len() as u64),
        )
        .unwrap()
        .unwrap();
    let mut replay = rustrace_editor::ReplayDocument::new(
        buffer.document_id().clone(),
        initial.into(),
        0,
        SelectionState::caret(0),
    )
    .unwrap();
    assert!(buffer.apply_transaction(transaction.clone()).unwrap());
    assert!(replay.apply_transaction(&transaction).unwrap());
    assert_eq!(buffer.text(), "// naïve\r\nlet 名称 = \"大阪🦀\";\r\n");
    assert_eq!(buffer.text(), replay.text());
    assert_eq!(buffer.hash(), replay.hash());
    assert!(buffer.positions(0).is_err());
    let undo = buffer.preview_undo().unwrap().unwrap();
    assert!(buffer.apply_transaction(undo.clone()).unwrap());
    assert!(replay.apply_transaction(&undo).unwrap());
    assert_eq!(buffer.text(), initial);
    assert_eq!(replay.text(), initial);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn byte_scalar_and_utf16_roundtrips(
        lines in prop::collection::vec(prop::collection::vec(prop_oneof![
            Just('é'), Just('名'), Just('😀'), Just('\u{301}'),
            any::<char>().prop_filter("line content", |c| !matches!(c, '\r' | '\n')),
        ], 0..30), 1..10),
        crlf in any::<bool>(),
    ) {
        let lines: Vec<String> = lines.into_iter().map(|line| line.into_iter().collect()).collect();
        let ending = if crlf { "\r\n" } else { "\n" };
        let text = lines.join(ending);
        let buffer = editor(&text);
        let p = buffer.positions(0).unwrap();
        let mut start = 0;
        for (line, content) in lines.iter().enumerate() {
            for byte in content.char_indices().map(|(b, _)| b).chain([content.len()]) {
                let offset = ByteOffset((start + byte) as u64);
                let position = lsp(line as u32, content[..byte].encode_utf16().count() as u32);
                prop_assert_eq!(p.byte_to_utf16(offset), Ok(position));
                prop_assert_eq!(p.utf16_to_byte(position), Ok(offset));
            }
            prop_assert_eq!(p.utf16_to_byte(lsp(line as u32, content.encode_utf16().count() as u32 + 1)), Err(PositionError::ColumnOutOfBounds));
            start += content.len() + ending.len();
        }
        for (scalar, byte) in text.char_indices().map(|(b, _)| b).chain([text.len()]).enumerate() {
            prop_assert_eq!(p.byte_to_scalar(ByteOffset(byte as u64)), Ok(ScalarIndex(scalar)));
            prop_assert_eq!(p.scalar_to_byte(ScalarIndex(scalar)), Ok(ByteOffset(byte as u64)));
        }
        for byte in 0..text.len() {
            if !text.is_char_boundary(byte) {
                prop_assert_eq!(p.byte_to_utf16(ByteOffset(byte as u64)), Err(PositionError::NotUtf8Boundary));
            }
        }
        prop_assert_eq!(buffer.text(), text);
    }

    #[test]
    fn every_addressable_visual_position_roundtrips(
        parts in prop::collection::vec(prop::sample::select(vec!["a", "名", "😀", "👩‍💻", "🇯🇵", "\u{301}", "\u{200b}", "\t"]), 0..80),
    ) {
        let text = parts.concat();
        let buffer = editor(&text);
        let p = buffer.positions(0).unwrap();
        // The accepted safe-display API defines visible source-cell boundaries.
        let mut expected = std::collections::BTreeMap::from([(0, 0)]);
        let mut column = 0;
        use unicode_segmentation::UnicodeSegmentation;
        for (byte, grapheme) in text.grapheme_indices(true) {
            column += grapheme_width(grapheme, column);
            expected.insert(column, byte + grapheme.len());
        }
        for (&column, &byte) in &expected {
            prop_assert_eq!(p.byte_to_visual(ByteOffset(byte as u64)), Ok(visual(0, column)));
            prop_assert_eq!(p.visual_to_byte(visual(0, column)), Ok(ByteOffset(byte as u64)));
        }
        for byte in text.char_indices().map(|(b, _)| b).chain([text.len()]) {
            if let Ok(position) = p.byte_to_visual(ByteOffset(byte as u64)) {
                prop_assert_eq!(p.visual_to_byte(position), Ok(ByteOffset(byte as u64)));
            }
        }
        for target in 0..=column + 1 {
            let expected = expected.get(&target).map(|&byte| ByteOffset(byte as u64)).ok_or(
                if target > column { PositionError::ColumnOutOfBounds } else { PositionError::NonAddressableVisualColumn }
            );
            prop_assert_eq!(p.visual_to_byte(visual(0, target)), expected);
        }
    }
}
