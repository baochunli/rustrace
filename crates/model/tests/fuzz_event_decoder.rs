//! Bounded fuzzing of the event envelope decoder.

#[path = "../../../tests/fuzz_support/mod.rs"]
mod fuzz_support;

use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{TimeZone, Utc};
use proptest::prelude::*;
use rustrace_model::{
    CommandId, CommandOutput, DecodeError, DecodeOutcome, DecodePolicy, DocumentId, EditOrigin,
    Event, EventEnvelope, FORMAT_VERSION_V1, FileEdited, FileFocused, Hash, MAX_ENVELOPE_BYTES,
    MAX_INSERTED_TEXT_BYTES, MAX_JSON_NESTING, MAX_PASTE_REJECTION_BYTES, MAX_VECTOR_ITEMS,
    OutputStream, PasteInputChannel, PasteRejected, PasteRejectionReason, SelectionState,
    SessionId, TextEdit, WorkspaceCheckpoint, decode_envelope, encode_envelope,
};

const SEED: [u8; 32] = *b"rustrace-event-fuzz-seed-v1.....";

/// The measured envelope for this target.
///
/// This is **not** a derived ceiling, though this decoder is the easiest of the
/// six to reason about: nothing is sized from a header before the bytes it
/// describes are read, and input is capped at one mebibyte and rejected there.
///
/// The family, one line per input:
///
/// | Input | Peak |
/// | --- | --- |
/// | `near-maximum-envelope`, a valid 1 045 370 B envelope of four inserted runs just under the per-run limit | **1 051 883 B** |
/// | `vector-1024`, a 1 024-element array at `MAX_VECTOR_ITEMS` | 32 790 B |
/// | `nesting-16`, nesting at `MAX_JSON_NESTING` | 1 942 B |
/// | `envelope-at-limit`, one mebibyte of filler, rejected at preflight | 21 B |
/// | `envelope-over-limit`, `nesting-17`, `vector-1025` | 0 B |
///
/// The bound is four times `MAX_ENVELOPE_BYTES`, which is 3.99 times the
/// largest of those. It is kept at that round figure rather than lowered to
/// 1.25x, because the decoder's peak tracks its input and the input ceiling is
/// the honest scale for it.
///
/// **The true worst case over all legal inputs is not derived and may be
/// higher than this.** The bound is an observed envelope guarding against
/// regression; what these cases establish is that the decoder does not panic
/// and that its allocation is a bounded function of the documented limits.
const ALLOCATION_BOUND: usize = 4 * MAX_ENVELOPE_BYTES;
const MUTATION_CEILING: usize = MAX_ENVELOPE_BYTES + 1024;

/// Regression for a harness defect: `run_cases` reused one `TestRunner`, whose
/// success counter is never reset, so every iteration after the first returned
/// without generating or running a case.
#[test]
fn fuzz_harness_runs_every_budgeted_case() {
    const BUDGET: u32 = 137;
    let executed = AtomicUsize::new(0);
    let completed =
        fuzz_support::run_bounded_cases("harness-budget", SEED, BUDGET, Just(()), |()| {
            executed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
    assert_eq!(completed, BUDGET, "the harness stopped short of its budget");
    assert_eq!(
        executed.load(Ordering::Relaxed) as u32,
        BUDGET,
        "the harness did not run its whole iteration budget"
    );
}

#[test]
fn fuzz_event_decoder() {
    fuzz_support::isolated("fuzz_event_decoder", body);
}

fn body() {
    for (label, bytes) in boundary_inputs() {
        let (_, peak) =
            fuzz_support::assert_measured(&label, ALLOCATION_BOUND, || decode_both(&bytes));
        println!("FUZZ_PEAK case={label} input={} peak={peak}", bytes.len());
    }

    // A legitimate envelope close to the one-mebibyte ceiling. The decoder has
    // no declared-length allocation to size from a header, so its peak tracks
    // the bytes actually present; this is what that costs at the limit.
    let near_maximum = near_maximum_envelope();
    let (outcome, peak) =
        fuzz_support::assert_measured("near-maximum-envelope", ALLOCATION_BOUND, || {
            decode_envelope(&near_maximum, DecodePolicy::RejectUnsupported)
        });
    assert!(
        outcome.is_ok(),
        "the near-maximum envelope failed to decode: {outcome:?}"
    );
    assert!(
        peak > near_maximum.len(),
        "decoding a {}-byte envelope peaked at {peak}, so it no longer measures a full-size \
         decode",
        near_maximum.len()
    );
    println!(
        "FUZZ_PEAK case=near-maximum-envelope input={} peak={peak}",
        near_maximum.len()
    );

    let fixtures = valid_envelopes();
    let strategy = (
        proptest::sample::select(fixtures),
        fuzz_support::mutations(6),
        any::<bool>(),
    );
    fuzz_support::run_cases(
        "event-decoder",
        SEED,
        strategy,
        |(fixture, operations, reject_unsupported)| {
            let bytes = fuzz_support::apply_mutations(&fixture, &operations, MUTATION_CEILING);
            let policy = if reject_unsupported {
                DecodePolicy::RejectUnsupported
            } else {
                DecodePolicy::SkipUnsupported
            };
            let outcome =
                fuzz_support::bounded(ALLOCATION_BOUND, || decode_envelope(&bytes, policy))?;
            check_outcome(&bytes, outcome)
        },
    );

    fuzz_support::run_cases(
        "event-decoder-arbitrary-bytes",
        SEED,
        proptest::collection::vec(any::<u8>(), 0..4096),
        |bytes| {
            let outcome = fuzz_support::bounded(ALLOCATION_BOUND, || {
                decode_envelope(&bytes, DecodePolicy::RejectUnsupported)
            })?;
            check_outcome(&bytes, outcome)
        },
    );
}

fn decode_both(bytes: &[u8]) {
    let _ = decode_envelope(bytes, DecodePolicy::RejectUnsupported);
    let _ = decode_envelope(bytes, DecodePolicy::SkipUnsupported);
}

/// A decoded envelope must survive re-validation, and a rejection must never
/// quote the raw payload it rejected.
fn check_outcome(
    bytes: &[u8],
    outcome: Result<DecodeOutcome, DecodeError>,
) -> Result<(), TestCaseError> {
    match outcome {
        Ok(DecodeOutcome::Decoded(envelope)) => {
            prop_assert!(
                envelope.validate().is_ok(),
                "decoder accepted an envelope that fails validation"
            );
            Ok(())
        }
        Ok(DecodeOutcome::Skipped(skipped)) => {
            prop_assert!(
                skipped.encoded_len == bytes.len(),
                "skipped event reported {} bytes for a {}-byte input",
                skipped.encoded_len,
                bytes.len()
            );
            Ok(())
        }
        Err(error) => {
            let message = error.to_string();
            prop_assert!(
                message.len() <= 4096,
                "rejection reason grew to {} bytes",
                message.len()
            );
            Ok(())
        }
    }
}

/// Fixed inputs at each documented decoder limit and at the limit plus one.
fn boundary_inputs() -> Vec<(String, Vec<u8>)> {
    let mut inputs = Vec::new();
    for (label, limit) in [
        ("envelope", MAX_ENVELOPE_BYTES),
        ("paste-rejection", MAX_PASTE_REJECTION_BYTES),
    ] {
        inputs.push((format!("{label}-at-limit"), vec![b'a'; limit]));
        inputs.push((format!("{label}-over-limit"), vec![b'a'; limit + 1]));
    }
    for depth in [MAX_JSON_NESTING, MAX_JSON_NESTING + 1] {
        let mut bytes = vec![b'['; depth];
        bytes.extend(std::iter::repeat_n(b']', depth));
        inputs.push((format!("nesting-{depth}"), bytes));
    }
    for items in [MAX_VECTOR_ITEMS, MAX_VECTOR_ITEMS + 1] {
        let body = vec!["0"; items].join(",");
        inputs.push((format!("vector-{items}"), format!("[{body}]").into_bytes()));
    }
    inputs.push(("empty".to_owned(), Vec::new()));
    inputs
}

/// A valid envelope close to `MAX_ENVELOPE_BYTES`: four inserted runs just
/// under the per-run limit, which is the largest legitimate event the schema
/// admits.
fn near_maximum_envelope() -> Vec<u8> {
    const RUNS: usize = 4;
    let run = "a".repeat(MAX_INSERTED_TEXT_BYTES - 1024);
    let edits = (0..RUNS)
        .map(|index| TextEdit {
            start_byte: index as u64,
            end_byte: index as u64,
            inserted_text: run.clone(),
        })
        .collect();
    let encoded = encode_envelope(&envelope(Event::FileEdited(FileEdited {
        document_id: document_id(),
        version_before: 3,
        version_after: 4,
        origin: EditOrigin::Keyboard,
        edits,
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(0),
        hash_before: hash(5),
        hash_after: hash(6),
    })))
    .expect("the near-maximum envelope encodes");
    assert!(
        encoded.len() > MAX_ENVELOPE_BYTES / 2,
        "the near-maximum envelope shrank to {} bytes",
        encoded.len()
    );
    encoded
}

fn valid_envelopes() -> Vec<Vec<u8>> {
    vec![
        Event::FileFocused(FileFocused {
            document_id: document_id(),
        }),
        Event::FileEdited(FileEdited {
            document_id: document_id(),
            version_before: 3,
            version_after: 4,
            origin: EditOrigin::Keyboard,
            edits: vec![TextEdit {
                start_byte: 0,
                end_byte: 2,
                inserted_text: "let value = 1;".to_owned(),
            }],
            selection_before: SelectionState::new(2, 4),
            selection_after: SelectionState::caret(12),
            hash_before: hash(5),
            hash_after: hash(6),
        }),
        Event::PasteRejected(PasteRejected {
            reason: PasteRejectionReason::ExternalInput,
            channel: PasteInputChannel::TerminalBracketed,
        }),
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: hash(7),
            documents: vec![],
        }),
        Event::CargoOutput(CommandOutput {
            command_id: CommandId::new("command-01").unwrap(),
            stream: OutputStream::Stderr,
            output: "error[E0308]: mismatched types\n".to_owned(),
        }),
    ]
    .into_iter()
    .map(|event| encode_envelope(&envelope(event)).expect("fixture envelope encodes"))
    .collect()
}

fn envelope(event: Event) -> EventEnvelope {
    EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: SessionId::new("session-01").unwrap(),
        sequence: 1,
        monotonic_millis: 25,
        wall_clock_utc: Some(Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()),
        previous_event_hash: hash(0),
        event_hash: hash(0x11),
        event,
    }
}

fn document_id() -> DocumentId {
    DocumentId::new("document-01").unwrap()
}

fn hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; Hash::LENGTH])
}
