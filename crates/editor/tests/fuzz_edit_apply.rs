//! Bounded fuzzing of edit application over arbitrary documents and ranges.

#[path = "../../../tests/fuzz_support/mod.rs"]
mod fuzz_support;

use std::cell::Cell;

use proptest::prelude::*;
use rustrace_editor::{
    EditOrigin, EditorBuffer, EditorTransaction, MAX_TRANSACTION_JSON_BYTES, NoopEditorEffects,
    SelectionState, decode_transaction, document_hash, encode_transaction,
};
use rustrace_model::{DocumentId, Hash, MAX_INSERTED_TEXT_BYTES, TextEdit};

const APPLY_SEED: [u8; 32] = *b"rustrace-edit-apply-fuzz-seed-v1";
const CODEC_SEED: [u8; 32] = *b"rustrace-edit-codec-fuzz-seed-v1";

/// The measured envelope for this target.
///
/// This is **not** a derived ceiling. The pre-edit text, the applied text, the
/// history entry and the encoded transaction are live together, so the peak is
/// a multiple of the inserted run rather than the run itself.
///
/// The family, one line per input:
///
/// | Input | Peak |
/// | --- | --- |
/// | the randomized `edit-apply` arm at its largest, documents up to 4 KiB with up to six canonical edits | **1 048 098 B** |
/// | `inserted-text-262144`, one inserted run at `MAX_INSERTED_TEXT_BYTES` | 821 354 B |
/// | `transaction-json-*`, at the transaction JSON limit and one over | 4 231 B |
/// | `inserted-text-262145`, one byte over the run limit, rejected | 13 B |
///
/// The bound is sixteen times `MAX_INSERTED_TEXT_BYTES`, four times the
/// largest of those.
///
/// **The true worst case over all legal inputs is not derived and may be
/// higher than this.** The bound is an observed envelope guarding against
/// regression; what these cases establish is that edit application does not
/// panic and that its allocation is a bounded function of the documented
/// limits.
const ALLOCATION_BOUND: usize = 16 * MAX_INSERTED_TEXT_BYTES;
const MUTATION_CEILING: usize = MAX_TRANSACTION_JSON_BYTES + 1024;
const MAX_DOCUMENT_BYTES: usize = 4 * 1024;

/// Multi-byte, combining, and astral text so range endpoints land inside
/// scalars and grapheme clusters as often as between them.
const TEXT_TOKENS: &[&str] = &[
    "a",
    "\n",
    "\r\n",
    "\t",
    "é",
    "e\u{301}",
    "猫",
    "🦀",
    "👩‍💻",
    "🇨🇦",
    "مرحبا",
    "fn main() {}",
];

#[test]
fn fuzz_edit_apply() {
    fuzz_support::isolated("fuzz_edit_apply", body);
}

fn body() {
    boundary_cases();

    let strategy = (
        document(),
        proptest::collection::vec(edit_seed(), 0..6),
        any::<u64>(),
        any::<u64>(),
        // An arbitrary u64 selection is out of bounds for every short document,
        // so the editor would reject nearly every case on the selection before
        // the edits mattered and the accepted branch would go unexercised.
        prop_oneof![4 => Just(true), 1 => Just(false)],
        prop_oneof![4 => Just(true), 1 => Just(false)],
    );
    let accepted = Cell::new(0_u64);
    let rejected = Cell::new(0_u64);
    fuzz_support::run_cases(
        "edit-apply",
        APPLY_SEED,
        strategy,
        |(text, seeds, anchor, active, selection_in_range, canonical)| {
            let mut buffer = editor(&text);
            let edits = seeds
                .iter()
                .map(|seed| seed.into_edit(&text))
                .collect::<Vec<_>>();
            let edits = if canonical {
                canonicalize(edits)
            } else {
                edits
            };
            let expected = expected_text(&text, &edits);
            let selection = if selection_in_range {
                let basis = expected.as_deref().unwrap_or(text.as_str());
                SelectionState::new(
                    boundary_offset(basis, anchor),
                    boundary_offset(basis, active),
                )
            } else {
                SelectionState::new(anchor, active)
            };
            let version_before = buffer.version();
            let outcome = fuzz_support::bounded(ALLOCATION_BOUND, || {
                buffer.apply_edits(EditOrigin::Keyboard, edits.clone(), selection)
            })?;
            match outcome {
                Ok(changed) => {
                    accepted.set(accepted.get() + 1);
                    let after = buffer.text();
                    let Some(expected) = expected else {
                        return Err(TestCaseError::fail(
                            "accepted edits that cannot be applied to the pre-edit text",
                        ));
                    };
                    prop_assert_eq!(
                        &after,
                        &expected,
                        "applied edits did not produce the expected text"
                    );
                    prop_assert_eq!(
                        buffer.hash(),
                        document_hash(&after),
                        "applied edit left the cached hash stale"
                    );
                    prop_assert_eq!(
                        buffer.len_bytes(),
                        after.len(),
                        "the reported byte length disagrees with the document"
                    );
                    prop_assert_eq!(
                        buffer.version(),
                        if changed {
                            version_before + 1
                        } else {
                            version_before
                        },
                        "the version does not match whether the document changed"
                    );
                    prop_assert!(
                        !changed || after != text,
                        "a change was reported without changing the document"
                    );
                    let selection = buffer.selection_state();
                    for offset in [selection.anchor_byte, selection.active_byte] {
                        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
                        prop_assert!(
                            offset <= after.len() && after.is_char_boundary(offset),
                            "the selection landed at {offset}, outside a scalar boundary of a \
                             {}-byte document",
                            after.len()
                        );
                    }
                    Ok(())
                }
                Err(_) => {
                    rejected.set(rejected.get() + 1);
                    prop_assert_eq!(
                        buffer.text(),
                        text,
                        "a rejected edit still mutated the document"
                    );
                    prop_assert_eq!(
                        buffer.version(),
                        version_before,
                        "a rejected edit advanced the version"
                    );
                    Ok(())
                }
            }
        },
    );
    println!(
        "FUZZ_COVERAGE target=edit-apply accepted={} rejected={}",
        accepted.get(),
        rejected.get()
    );
    assert!(
        accepted.get() > 0 && rejected.get() > 0,
        "edit-apply exercised only one branch: {} accepted, {} rejected",
        accepted.get(),
        rejected.get()
    );

    let fixtures = valid_transactions();
    fuzz_support::run_cases(
        "edit-transaction-codec",
        CODEC_SEED,
        (
            proptest::sample::select(fixtures),
            fuzz_support::mutations(6),
            document(),
        ),
        |((encoded, _), operations, text)| {
            let bytes = fuzz_support::apply_mutations(&encoded, &operations, MUTATION_CEILING);
            let mut buffer = editor(&text);
            let decoded = fuzz_support::bounded(ALLOCATION_BOUND, || decode_transaction(&bytes))?;
            let Ok(transaction) = decoded else {
                return Ok(());
            };
            let outcome =
                fuzz_support::bounded(ALLOCATION_BOUND, || buffer.apply_transaction(transaction))?;
            prop_assert!(
                outcome.is_ok() || buffer.text() == text,
                "a rejected transaction still mutated the document"
            );
            Ok(())
        },
    );
}

/// Fixed inputs at each documented editor limit and at the limit plus one.
fn boundary_cases() {
    let text = "fn main() {}\n";
    for length in [MAX_INSERTED_TEXT_BYTES, MAX_INSERTED_TEXT_BYTES + 1] {
        let mut buffer = editor(text);
        let edits = vec![TextEdit {
            start_byte: 0,
            end_byte: 0,
            inserted_text: "a".repeat(length),
        }];
        let label = format!("inserted-text-{length}");
        let (applied, peak) = fuzz_support::assert_measured(&label, ALLOCATION_BOUND, || {
            buffer.apply_edits(EditOrigin::Keyboard, edits, SelectionState::caret(0))
        });
        assert_eq!(
            applied.is_ok(),
            length <= MAX_INSERTED_TEXT_BYTES,
            "the inserted-text limit is not enforced at {length} bytes"
        );
        println!("FUZZ_PEAK case={label} peak={peak}");
    }

    for (label, start, end, expect_rejection) in [
        ("range-at-end", text.len() as u64, text.len() as u64, false),
        (
            "range-past-end",
            text.len() as u64 + 1,
            text.len() as u64 + 1,
            true,
        ),
        ("range-reversed", 4, 2, true),
        ("range-saturating", u64::MAX - 1, u64::MAX, true),
    ] {
        let mut buffer = editor(text);
        let edits = vec![TextEdit {
            start_byte: start,
            end_byte: end,
            inserted_text: "x".to_owned(),
        }];
        let applied = fuzz_support::assert_bounded(label, ALLOCATION_BOUND, || {
            buffer.apply_edits(EditOrigin::Keyboard, edits, SelectionState::caret(0))
        });
        if expect_rejection {
            assert!(applied.is_err(), "{label} was accepted");
            assert_eq!(buffer.text(), text, "{label} mutated the document");
        }
    }

    for length in [MAX_TRANSACTION_JSON_BYTES, MAX_TRANSACTION_JSON_BYTES + 1] {
        fuzz_support::assert_bounded(
            &format!("transaction-json-{length}"),
            ALLOCATION_BOUND,
            || {
                let _ = decode_transaction(&vec![b'a'; length]);
            },
        );
    }
}

/// The largest scalar boundary of `text` at or below `raw` reduced into range,
/// so a generated selection usually lands somewhere the editor can accept.
fn boundary_offset(text: &str, raw: u64) -> u64 {
    let mut offset = usize::try_from(raw % (text.len() as u64 + 1)).unwrap_or(0);
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset as u64
}

/// Applies `edits` to `text` independently of the editor, so an accepted
/// transaction can be compared against a result the test computed itself.
///
/// Returns `None` when the edits are not something the editor may accept:
/// out of range, off a scalar boundary, unordered, or overlapping. Ranges are
/// in the pre-edit snapshot, so applying them last-first keeps every remaining
/// offset valid.
fn expected_text(text: &str, edits: &[TextEdit]) -> Option<String> {
    let mut previous_end = 0_u64;
    for edit in edits {
        let start = usize::try_from(edit.start_byte).ok()?;
        let end = usize::try_from(edit.end_byte).ok()?;
        if start > end || end > text.len() {
            return None;
        }
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            return None;
        }
        if edit.start_byte < previous_end {
            return None;
        }
        previous_end = edit.end_byte;
    }

    let mut result = text.to_owned();
    for edit in edits.iter().rev() {
        let start = edit.start_byte as usize;
        let end = edit.end_byte as usize;
        result.replace_range(start..end, &edit.inserted_text);
    }
    Some(result)
}

fn editor(text: &str) -> EditorBuffer<NoopEditorEffects> {
    EditorBuffer::new(
        DocumentId::new("fuzz-document").unwrap(),
        text,
        NoopEditorEffects,
    )
}

/// A range endpoint pair biased towards document boundaries and interior
/// scalar boundaries, plus wholly out-of-range values.
#[derive(Clone, Debug)]
struct EditSeed {
    start: u64,
    span: u64,
    text: u8,
    out_of_range: bool,
}

impl EditSeed {
    fn into_edit(&self, text: &str) -> TextEdit {
        let (start, end) = if self.out_of_range {
            (self.start, self.start.saturating_add(self.span % 16))
        } else {
            let start = boundary_offset(text, self.start);
            let end = boundary_offset(text, start.saturating_add(self.span % 16));
            (start, end.max(start))
        };
        TextEdit {
            start_byte: start,
            end_byte: end,
            inserted_text: TEXT_TOKENS[usize::from(self.text) % TEXT_TOKENS.len()].to_owned(),
        }
    }
}

/// Sorts edits by start offset and drops any that overlaps its predecessor.
///
/// The editor requires canonical, non-overlapping ranges, and independently
/// drawn offsets almost never are, so without this most multi-edit cases would
/// be rejected on ordering before anything else could be checked.
fn canonicalize(edits: Vec<TextEdit>) -> Vec<TextEdit> {
    let mut edits = edits;
    edits.sort_by_key(|edit| (edit.start_byte, edit.end_byte));
    let mut canonical: Vec<TextEdit> = Vec::with_capacity(edits.len());
    for edit in edits {
        if canonical
            .last()
            .is_none_or(|previous| edit.start_byte >= previous.end_byte)
        {
            canonical.push(edit);
        }
    }
    canonical
}

fn edit_seed() -> impl Strategy<Value = EditSeed> {
    (any::<u64>(), any::<u64>(), any::<u8>(), any::<bool>()).prop_map(
        |(start, span, text, out_of_range)| EditSeed {
            start,
            span,
            text,
            out_of_range,
        },
    )
}

fn document() -> impl Strategy<Value = String> {
    proptest::collection::vec(0_usize..TEXT_TOKENS.len(), 0..48).prop_map(|indices| {
        let mut text = String::new();
        for index in indices {
            if text.len() + TEXT_TOKENS[index].len() > MAX_DOCUMENT_BYTES {
                break;
            }
            text.push_str(TEXT_TOKENS[index]);
        }
        text
    })
}

/// Production-shaped transactions used as mutation seeds for the codec.
fn valid_transactions() -> Vec<(Vec<u8>, EditorTransaction)> {
    let text = "fn main() {}\n";
    let mut fixtures = Vec::new();
    for (origin, edits) in [
        (
            EditOrigin::Keyboard,
            vec![TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: "let value = 1;".to_owned(),
            }],
        ),
        (
            EditOrigin::Paste,
            vec![
                TextEdit {
                    start_byte: 0,
                    end_byte: 2,
                    inserted_text: String::new(),
                },
                TextEdit {
                    start_byte: 4,
                    end_byte: 4,
                    inserted_text: "🦀".to_owned(),
                },
            ],
        ),
    ] {
        let transaction = EditorTransaction {
            document_id: DocumentId::new("fuzz-document").unwrap(),
            version_before: 0,
            version_after: 1,
            origin,
            edits,
            selection_before: SelectionState::caret(0),
            selection_after: SelectionState::caret(0),
            hash_before: document_hash(text),
            hash_after: Hash::zero(),
        };
        let encoded = encode_transaction(&transaction).expect("fixture transaction encodes");
        fixtures.push((encoded, transaction));
    }
    fixtures
}
