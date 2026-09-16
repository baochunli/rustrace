//! Bounded fuzzing of the framed checkpoint decoder.

#[path = "../../../tests/fuzz_support/mod.rs"]
mod fuzz_support;

use proptest::prelude::*;
use rustrace_journal::{
    CheckpointFile, CheckpointSnapshot, MAX_CHECKPOINT_ENCODED_BYTES,
    MAX_CHECKPOINT_EXPANDED_BYTES, MAX_CHECKPOINT_FILE_BYTES, MAX_CHECKPOINT_TOTAL_FILE_BYTES,
    OpenDocument, decode_checkpoint, encode_checkpoint,
};
use rustrace_model::{DocumentId, SelectionState, SessionId, WorkspacePath};

const SEED: [u8; 32] = *b"rustrace-checkpoint-fuzz-seed-v1";

/// Mutated inputs are capped well below the format's encoded ceiling, which is
/// where every seed the randomized arm draws already sits.
const MUTATION_CEILING: usize = 512 * 1024;

/// The measured envelope for this target.
///
/// This is **not** a derived ceiling. `decode_checkpoint` re-encodes what it
/// decoded to check canonical form, so the expanded buffer, the snapshot's
/// owned file contents, `encode_snapshot`'s buffer, the compressor output and
/// the canonical vector are live together, and the total is a composite no
/// hand derivation from the format limits gets right.
///
/// The family, one line per input, each inside every documented limit:
///
/// | Input | Peak |
/// | --- | --- |
/// | `clean-checkpoint`, the large fixture: ten incompressible files just inside the per-file and total workspace limits, 10 487 180 B, decodes successfully | **58 768 119 B** |
/// | `declared-expanded-11062597`, a correctly framed and digested 69 B input declaring the expanded ceiling | 11 105 920 B |
/// | `clean-checkpoint`, 315 B fixture | 400 405 B |
/// | `clean-checkpoint`, 147 B fixture | 399 657 B |
/// | `declared-expanded-11062598`, one byte above the ceiling, refused before allocating | 0 B |
///
/// The bound is the largest of those, 58 768 119, times 1.25.
///
/// **The true worst case over all legal inputs is not derived and may be
/// higher than this.** The bound is an observed envelope guarding against
/// regression; what these cases establish is that the decoder does not panic
/// and that its allocation is a bounded function of the documented limits.
const MEASURED_MAXIMUM: usize = 58_768_119;
const SUCCESSFUL_DECODE_BOUND: usize = MEASURED_MAXIMUM * 5 / 4;

/// The tight guard for a decode that stops at or before the expanded
/// allocation. Measured 11 105 920 B for the 69-byte declared-ceiling input;
/// this is that measurement times 1.25, and it is what the over-ceiling case
/// must stay far below.
const REJECTION_BOUND: usize = 11_105_920 * 5 / 4;

/// The tight guard for the two small fixtures, for the arbitrary-bytes arm and
/// for any declaration refused before a ceiling-sized allocation. The zlib
/// inflate state dominates it: a valid 147-byte checkpoint peaks at 399 657 B
/// regardless of its size, so this still fails a regression that grew the
/// common path by a factor of two and a half, which the envelope could not.
const ORDINARY_ALLOCATION_BYTES: usize = 1024 * 1024;

#[test]
fn fuzz_checkpoint_decoder() {
    fuzz_support::isolated("fuzz_checkpoint_decoder", body);
}

fn body() {
    let fixtures = valid_checkpoints();
    boundary_cases(&fixtures);

    // The randomized arm draws only the small fixtures, and every case is held
    // to ORDINARY_ALLOCATION_BYTES: 63 times tighter than a bound loose enough
    // for the large fixture, which is what makes this arm sensitive to a
    // regression in the common path. Seeds are drawn by index and borrowed, so
    // no case copies a fixture to look at it.
    //
    // The large fixture is deliberately not drawn here. A full-size decode
    // costs about 3.2 seconds in a debug build, so at one case in a thousand
    // the extended budget fell from 373 056 cases to 9 664 — a 97 per cent loss
    // of framing coverage — to buy seventeen repetitions of a measurement
    // `boundary_cases` already makes deterministically on every run.
    let small_indices = (0..fixtures.len())
        .filter(|index| fixtures[*index].len() <= ORDINARY_ALLOCATION_BYTES)
        .collect::<Vec<_>>();
    fuzz_support::run_cases(
        "checkpoint-decoder",
        SEED,
        (
            proptest::sample::select(small_indices),
            fuzz_support::mutations(6),
        ),
        |(index, operations)| {
            let bytes =
                fuzz_support::apply_mutations(&fixtures[index], &operations, MUTATION_CEILING);
            let decoded =
                fuzz_support::bounded(ORDINARY_ALLOCATION_BYTES, || decode_checkpoint(&bytes))?;
            match decoded {
                Ok(snapshot) => {
                    let reencoded = fuzz_support::bounded(ORDINARY_ALLOCATION_BYTES, || {
                        encode_checkpoint(&snapshot)
                    })?;
                    prop_assert_eq!(
                        reencoded.as_deref(),
                        Ok(bytes.as_slice()),
                        "an accepted checkpoint did not re-encode to its own bytes"
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
        },
    );

    fuzz_support::run_cases(
        "checkpoint-decoder-arbitrary-bytes",
        SEED,
        proptest::collection::vec(any::<u8>(), 0..2048),
        |bytes| {
            let mut framed = b"RUSTCPK\0".to_vec();
            framed.extend(&bytes);
            fuzz_support::bounded(ORDINARY_ALLOCATION_BYTES, || {
                let _ = decode_checkpoint(&bytes);
                let _ = decode_checkpoint(&framed);
            })
        },
    );
}

/// The domain separator `decode_checkpoint` prefixes its framing digest with.
const INTEGRITY_DOMAIN: &[u8] = b"rustrace.checkpoint.compressed.v1\0";

/// The framing digest the decoder requires over the outer header and the
/// compressed body.
///
/// It is unkeyed, so any input can carry the correct one. A boundary input must
/// carry it, or the decoder stops at the integrity check before reaching the
/// allocation the input exists to exercise.
fn integrity_digest(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(INTEGRITY_DOMAIN);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

/// A correctly framed, correctly digested checkpoint whose declared expansion
/// is `expanded` and whose compressed body is eight bytes of nothing.
///
/// Sixty-nine bytes in total, which is the point: everything the decoder checks
/// before it sizes its expanded buffer is satisfied.
fn declared_expansion(expanded: u64) -> Vec<u8> {
    let mut encoded = b"RUSTCPK\0".to_vec();
    encoded.extend(1_u32.to_be_bytes());
    encoded.push(1);
    encoded.extend(expanded.to_be_bytes());
    encoded.extend(8_u64.to_be_bytes());
    encoded.extend([0_u8; 8]);
    let digest = integrity_digest(&encoded);
    encoded.extend(digest);
    encoded
}

/// Fixed inputs at each documented framing limit and at the limit plus one,
/// including a header that declares the largest legal expansion for a body far
/// too small to produce it.
fn boundary_cases(fixtures: &[Vec<u8>]) {
    for length in [
        MAX_CHECKPOINT_ENCODED_BYTES,
        MAX_CHECKPOINT_ENCODED_BYTES + 1,
    ] {
        let encoded = vec![0_u8; length];
        fuzz_support::assert_bounded(
            &format!("encoded-{length}"),
            ORDINARY_ALLOCATION_BYTES,
            || {
                let _ = decode_checkpoint(&encoded);
            },
        );
    }

    let ceiling = MAX_CHECKPOINT_EXPANDED_BYTES as u64;
    for (expanded, reaches_ceiling) in [(ceiling, true), (ceiling + 1, false)] {
        let encoded = declared_expansion(expanded);
        assert_eq!(encoded.len(), 69, "the boundary input changed size");
        let label = format!("declared-expanded-{expanded}");
        let (outcome, peak) =
            fuzz_support::assert_measured(&label, REJECTION_BOUND, || decode_checkpoint(&encoded));
        assert!(
            outcome.is_err(),
            "a header declaring {expanded} expanded bytes was accepted"
        );
        if reaches_ceiling {
            assert!(
                peak >= MAX_CHECKPOINT_EXPANDED_BYTES,
                "{label} allocated only {peak} bytes, so it no longer reaches the expanded \
                 buffer it exists to exercise"
            );
        } else {
            assert!(
                peak <= ORDINARY_ALLOCATION_BYTES,
                "{label} allocated {peak} bytes; a declaration above the ceiling must be \
                 refused before allocating"
            );
        }
    }

    // Every valid fixture must decode inside the bound its own size justifies.
    // Without this the loose bounds above would hide a regression in the
    // common path, and the large fixture is what measures the decoder's real
    // peak rather than the cost of stopping at the expanded allocation.
    for encoded in fixtures {
        let large = encoded.len() > ORDINARY_ALLOCATION_BYTES;
        let bound = if large {
            SUCCESSFUL_DECODE_BOUND
        } else {
            ORDINARY_ALLOCATION_BYTES
        };
        let (decoded, peak) = fuzz_support::assert_measured("clean-checkpoint", bound, || {
            decode_checkpoint(&encoded)
        });
        assert!(decoded.is_ok(), "the clean fixture failed to decode");
        if large {
            assert!(
                peak > MAX_CHECKPOINT_EXPANDED_BYTES,
                "the large fixture peaked at {peak} bytes, below the expanded ceiling, so it no \
                 longer measures a full-size successful decode"
            );
        }
        println!(
            "FUZZ_PEAK case=clean-checkpoint input={} peak={peak} bound={bound}",
            encoded.len()
        );
    }

    for length in [0, 1, 20, 40] {
        let truncated = vec![0_u8; length];
        fuzz_support::assert_bounded(
            &format!("truncated-{length}"),
            ORDINARY_ALLOCATION_BYTES,
            || {
                let _ = decode_checkpoint(&truncated);
            },
        );
    }
}

/// A legitimate checkpoint close to the workspace limits: ten incompressible
/// files of one mebibyte less sixty-four bytes each, which is just inside
/// `MAX_WORKSPACE_FILE_BYTES` per file and `MAX_WORKSPACE_TOTAL_BYTES` in
/// total, encoded by the production encoder.
///
/// This is the shape an ordinary end-of-session checkpoint has, and decoding it
/// is the decoder's real peak: the expanded buffer, the decoded snapshot's file
/// contents, the re-encoding buffer, the compressor output, and the canonical
/// vector are all live at once.
fn large_checkpoint() -> Vec<u8> {
    const FILES: usize = 10;
    const FILE_BYTES: usize = MAX_CHECKPOINT_FILE_BYTES - 64;
    assert!(
        FILES * FILE_BYTES <= MAX_CHECKPOINT_TOTAL_FILE_BYTES,
        "the large fixture must stay inside the workspace total"
    );

    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let files = (0..FILES)
        .map(|index| CheckpointFile {
            path: WorkspacePath::new(format!("src/blob{index:02}.bin")).unwrap(),
            // Incompressible, so the encoded size tracks the file bytes and the
            // decode really does inflate a ceiling-sized buffer.
            contents: (0..FILE_BYTES).map(|_| next_byte(&mut state)).collect(),
        })
        .collect();
    let snapshot = CheckpointSnapshot::new(
        SessionId::new("session-large").unwrap(),
        99,
        files,
        None,
        vec![],
    )
    .expect("large snapshot builds");
    encode_checkpoint(&snapshot).expect("large fixture encodes")
}

/// A deterministic xorshift stream, so the fixture is identical on every run.
fn next_byte(state: &mut u64) -> u8 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    (*state >> 24) as u8
}

fn valid_checkpoints() -> Vec<Vec<u8>> {
    let session = SessionId::new("session-01").unwrap();
    let empty = CheckpointSnapshot::new(session.clone(), 1, vec![], None, vec![])
        .expect("empty snapshot builds");
    let populated = CheckpointSnapshot::new(
        session,
        7,
        vec![
            CheckpointFile {
                path: WorkspacePath::new("src/main.rs").unwrap(),
                contents: b"fn main() {\n    println!(\"hello\");\n}\n".to_vec(),
            },
            CheckpointFile {
                path: WorkspacePath::new("Cargo.toml").unwrap(),
                contents: b"[package]\nname = \"student\"\n[workspace]\n".to_vec(),
            },
        ],
        Some(DocumentId::new("doc-main").unwrap()),
        vec![OpenDocument {
            document_id: DocumentId::new("doc-main").unwrap(),
            path: WorkspacePath::new("src/main.rs").unwrap(),
            selection: SelectionState::new(4, 11),
            version: 3,
        }],
    )
    .expect("populated snapshot builds");

    let mut fixtures = [empty, populated]
        .iter()
        .map(|snapshot| encode_checkpoint(snapshot).expect("fixture checkpoint encodes"))
        .collect::<Vec<_>>();
    fixtures.push(large_checkpoint());
    fixtures
}
