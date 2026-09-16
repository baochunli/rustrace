//! Bounded fuzzing of the hostile-input `.rprov` importer.

#[path = "../../../tests/fuzz_support/mod.rs"]
mod fuzz_support;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{Cursor, Write};

use flate2::Compression;
use flate2::write::DeflateEncoder;
use proptest::prelude::*;
use rustrace_model::*;
use rustrace_workspace::rprov_import::{RprovImportError, import_rprov, import_rprov_for_review};

const STANDALONE_SEED: [u8; 32] = *b"rustrace-rprov-standalone-seedv1";
const OUTER_SEED: [u8; 32] = *b"rustrace-rprov-outer-zip-seed-v1";
const STRUCTURE_SEED: [u8; 32] = *b"rustrace-rprov-structure-seedv1.";

const MUTATION_CEILING: usize = 512 * 1024;

/// The measured envelope for this target.
///
/// This is **not** a derived ceiling. Peak allocation here is a composite of
/// the manifest buffer, the `serde_json` value, the converted `RprovManifest`,
/// the canonical re-encoding `decode_rprov_manifest` performs, the capture
/// buffer for a checkpoint entry, and the owned copies of every outer ZIP entry
/// name — four, or five for an entry classified as a source file, which also
/// lands in `source_file_aliases`. Several of these are live at the same time.
/// Five review rounds each found one more legal input above a hand-derived bound,
/// so this target enumerates a family and measures it instead.
///
/// The family, one line per input, each inside every documented limit:
///
/// | Input | Peak |
/// | --- | --- |
/// | `manifest-decode-with-ceiling-checkpoint`, 4 segments x 1 022 checkpoints x 1 000 source links, first entry declaring the entry ceiling | **20 232 546 B** |
/// | `outer-directory-at-limit`, 4 096 maximum-length ZIP directory entries, all directories | 17 608 720 B |
/// | the same archive with 256 of those entries classified as source files, the most the importer accepts, measured by review 8 and not enumerated here | 17 883 160 B |
/// | `manifest-decode-four-segment`, the same manifest without the ceiling claim | 14 445 626 B |
/// | `near-maximum-package`, a real payload at the checkpoint entry ceiling, imports | 11 137 446 B |
/// | `checkpoint-claim-at-ceiling`, 4 KB container claiming that ceiling | 11 137 395 B |
/// | `manifest-decode-one-segment`, 1 segment x 1 024 checkpoints x 4 096 source links | 9 908 302 B |
/// | `manifest-bytes-at-limit`, 69 B container claiming the manifest ceiling | 8 388 661 B |
/// | `clean-outer-zip` | 52 459 B |
/// | `clean-standalone` | 21 808 B |
/// | `outer-extra-field`, `outer-entry-comment`, at 1 and at `u16::MAX` | 343 B |
///
/// Review 6 independently measured **24 824 094 B** on a legal container this
/// builder does not reproduce — a larger manifest that stays under the JSON
/// value preflight. That measurement is part of the envelope even though no
/// case here produces it.
///
/// The bound is the largest of those, 24 824 094, times 1.25.
///
/// **The true worst case over all legal inputs is not derived and may be
/// higher than this.** The bound is an observed envelope guarding against
/// regression; what these cases establish is that the importer does not panic
/// and that its allocation is a bounded function of the documented limits.
const MEASURED_MAXIMUM: usize = 24_824_094;
const ALLOCATION_BOUND: usize = MEASURED_MAXIMUM * 5 / 4;

/// The tight guard for the two clean fixtures and for any claim the importer
/// must refuse before allocating. Measured: `clean-standalone` 21 808 B,
/// `clean-outer-zip` 52 459 B, `manifest-bytes-over-limit` 250 B,
/// `checkpoint-claim-over-ceiling` 8 809 B. This still fails a regression that
/// grew the clean import by a factor of five, which the envelope above could
/// not.
const ORDINARY_ALLOCATION_BYTES: usize = 256 * 1024;

/// Marker bytes planted in the fixture's starter-blob payload, and offered by
/// the structure-aware generator as a record path.
///
/// A rejection reason that quotes the payload occurrence has leaked private
/// package content. The path occurrence exists so the sandbox check for an
/// extracted package-supplied name has a name to look for. The format permits a
/// rejection to name a structural entry path, so the leak check is stricter: it
/// forbids the marker in any rejection, whichever occurrence it came from.
const PRIVATE_MARKER: &str = "DO_NOT_ECHO_PRIVATE_PAYLOAD";

/// Rejections that lie past the record-type check, so the structure-aware
/// generator only reaches them when it emits legal record headers most of the
/// time. Each must occur at least once over `COVERAGE_CASES` fixed-seed cases.
const REQUIRED_COVERAGE: &[&str] = &[
    "manifest record",
    "manifest.json",
    "record path bytes",
    "record payload bytes",
    "records bytes",
];

/// Cases the coverage tally runs. Fixed, so the tally is reproducible and the
/// ordinary test run stays fast.
const COVERAGE_CASES: u32 = 1_200;

#[test]
fn fuzz_rprov_import() {
    fuzz_support::isolated("fuzz_rprov_import", body);
}

fn body() {
    boundary_cases();

    let standalone = Fixture::one_segment().encode();
    fuzz_support::run_cases(
        "rprov-import-standalone",
        STANDALONE_SEED,
        (fuzz_support::mutations(6), any::<bool>()),
        |(operations, review)| {
            let bytes = fuzz_support::apply_mutations(&standalone, &operations, MUTATION_CEILING);
            import_case(&bytes, review)
        },
    );

    let outer = fixed_outer(Fixture::one_segment().encode());
    fuzz_support::run_cases(
        "rprov-import-outer-zip",
        OUTER_SEED,
        (fuzz_support::mutations(6), any::<bool>()),
        |(operations, review)| {
            let bytes = fuzz_support::apply_mutations(&outer, &operations, MUTATION_CEILING);
            import_case(&bytes, review)
        },
    );

    fuzz_support::run_cases(
        "rprov-import-structured-container",
        STRUCTURE_SEED,
        container_strategy(),
        |(records, entry_claim, stored_claim, version)| {
            let bytes = structured_container(&records, entry_claim, stored_claim, version);
            import_case(&bytes, false)
        },
    );

    fuzz_support::run_cases(
        "rprov-import-arbitrary-bytes",
        STANDALONE_SEED,
        prop_oneof![
            proptest::collection::vec(any::<u8>(), 0..2048).prop_map(|bytes| [
                b"RUST".as_slice(),
                &bytes
            ]
            .concat()),
            proptest::collection::vec(any::<u8>(), 0..2048).prop_map(|bytes| [
                b"PK\x03\x04".as_slice(),
                &bytes
            ]
            .concat()),
            proptest::collection::vec(any::<u8>(), 0..2048),
        ],
        |bytes| import_case(&bytes, false),
    );
}

/// Tallies how the structure-aware generator's containers are rejected and
/// requires every outcome past the record-type check to occur.
///
/// Without this the generator can emit an illegal record type on nearly every
/// case, short-circuit the importer at that one check, and still report a full
/// iteration count while the record and aggregate-arithmetic paths it exists to
/// reach are never entered.
#[test]
fn fuzz_rprov_container_coverage() {
    fuzz_support::isolated("fuzz_rprov_container_coverage", coverage_body);
}

fn coverage_body() {
    let tally = RefCell::new(BTreeMap::<String, u64>::new());
    let completed = fuzz_support::run_bounded_cases(
        "rprov-container-coverage",
        STRUCTURE_SEED,
        COVERAGE_CASES,
        container_strategy(),
        |(records, entry_claim, stored_claim, version)| {
            let bytes = structured_container(&records, entry_claim, stored_claim, version);
            let outcome = classified_import_case(&bytes, false)?;
            *tally.borrow_mut().entry(outcome).or_default() += 1;
            Ok(())
        },
    );
    assert_eq!(
        completed, COVERAGE_CASES,
        "the wall-clock budget truncated the coverage tally"
    );

    let tally = tally.into_inner();
    for (outcome, count) in &tally {
        println!("FUZZ_COVERAGE target=rprov-container-coverage outcome={outcome} count={count}");
    }
    let missing = REQUIRED_COVERAGE
        .iter()
        .filter(|outcome| !tally.contains_key(**outcome))
        .copied()
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "the structure-aware generator never reached {missing:?} in {COVERAGE_CASES} cases; \
         reached only {:?}",
        tally.keys().collect::<Vec<_>>()
    );
}

/// Imports one candidate package and checks every T8.3 importer invariant.
fn import_case(bytes: &[u8], review: bool) -> Result<(), TestCaseError> {
    classified_import_case(bytes, review).map(|_| ())
}

/// Imports one candidate package, checks every invariant, and reports how the
/// importer classified it.
fn classified_import_case(bytes: &[u8], review: bool) -> Result<String, TestCaseError> {
    let before = spool_files();
    // The randomized arms mutate a 5 526-byte container and a 5 886-byte outer
    // ZIP under a 512 KiB ceiling, so they cannot build a central directory
    // large enough to approach the family's maximum. They are still held to the
    // whole envelope rather than a tighter figure, because a mutation can grow
    // an input in ways this comment should not try to predict; the clean
    // fixtures carry the tight guard instead.
    let imported = fuzz_support::bounded(ALLOCATION_BOUND, || {
        if review {
            import_rprov_for_review(Cursor::new(bytes))
        } else {
            import_rprov(Cursor::new(bytes))
        }
    })?;
    let outcome = match imported {
        Ok(package) => {
            for entry in package.entries() {
                prop_assert!(
                    entry.byte_length <= MAX_RPROV_RECORD_PAYLOAD_BYTES,
                    "an accepted entry claims {} bytes",
                    entry.byte_length
                );
            }
            drop(package);
            "accepted".to_owned()
        }
        Err(error) => {
            check_rejection(&error)?;
            classify(&error)
        }
    };
    prop_assert_eq!(
        spool_files(),
        before,
        "the importer left a private spool behind"
    );
    Ok(outcome)
}

/// Names one rejection by the field or layer it names, so a tally groups the
/// importer's distinct decision points rather than their formatted numbers.
fn classify(error: &RprovImportError) -> String {
    match error {
        RprovImportError::Io { context, .. } => format!("io {context}"),
        RprovImportError::Model(source) => classify_model(source),
        RprovImportError::InvalidRprov { field } => (*field).to_owned(),
        RprovImportError::PayloadDigest { .. } => "payload digest".to_owned(),
        RprovImportError::Truncated { layer } => format!("truncated {layer}"),
        RprovImportError::InvalidOuterArchive { detail } => format!("outer archive: {detail}"),
        RprovImportError::UnsafeOuterPath => "unsafe outer path".to_owned(),
        RprovImportError::UnsupportedOuterFeature => "unsupported outer feature".to_owned(),
        RprovImportError::LimitExceeded { field, .. } => (*field).to_owned(),
        RprovImportError::MissingEntry => "missing entry".to_owned(),
    }
}

fn classify_model(error: &RprovError) -> String {
    match error {
        RprovError::UnsupportedFormatVersion { .. } => "unsupported format version".to_owned(),
        RprovError::UnsupportedInnerVersion { layer, .. } => {
            format!("unsupported inner version {layer}")
        }
        RprovError::ManifestTooLarge { .. } => "manifest too large".to_owned(),
        RprovError::Truncated { layer } => format!("truncated {layer}"),
        RprovError::InvalidMagic => "invalid magic".to_owned(),
        RprovError::InvalidManifest { .. } | RprovError::NonCanonicalManifest => {
            "manifest.json".to_owned()
        }
        RprovError::InvalidField { field, .. } => (*field).to_owned(),
        RprovError::EventIntegrity { kind, .. } => format!("event integrity {kind:?}"),
        RprovError::LimitExceeded { field, .. } => (*field).to_owned(),
        RprovError::ArithmeticOverflow { field } => format!("arithmetic overflow {field}"),
    }
}

/// A rejection must stay short, must not quote package payload or path bytes,
/// and must not have extracted anything into the sandbox.
fn check_rejection(error: &RprovImportError) -> Result<(), TestCaseError> {
    let display = error.to_string();
    let debug = format!("{error:?}");
    prop_assert!(
        display.len() <= 4096 && debug.len() <= 8192,
        "rejection reason grew to {} display and {} debug bytes",
        display.len(),
        debug.len()
    );
    // No rejection may quote the marker, on any input. The format document
    // would permit a rejection to name a structural entry path, and the
    // generator does offer the marker as a record path, so this check is
    // stricter than the document requires; it holds because the importer's
    // error type carries only static strings, numeric limits, and validated
    // entry paths that no fixture names after the marker.
    prop_assert!(
        !display.contains(PRIVATE_MARKER) && !debug.contains(PRIVATE_MARKER),
        "rejection reason echoed private package bytes"
    );
    // The structure-aware generator offers PRIVATE_MARKER as a record path, so
    // an importer that extracted a package-supplied name would leave this file
    // in the sandbox working directory.
    prop_assert!(
        !std::path::Path::new(PRIVATE_MARKER).exists(),
        "the importer extracted a package-supplied path"
    );
    Ok(())
}

/// Names of the importer's private spool files still visible in the sandbox
/// temporary directory. An unlinked spool never appears here.
fn spool_files() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return Vec::new();
    };
    let mut names = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".rustrace-rprov-"))
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// Fixed inputs at each documented container limit and at the limit plus one.
fn boundary_cases() {
    // Ordinary imports must stay small. Without this the envelope above would
    // hide a regression in the common path.
    let fixture = Fixture::one_segment();
    let clean = fixture.encode();
    let (_, peak) =
        fuzz_support::assert_measured("clean-standalone", ORDINARY_ALLOCATION_BYTES, || {
            import_rprov(Cursor::new(clean.as_slice())).expect("the clean fixture imports")
        });
    println!(
        "FUZZ_PEAK case=clean-standalone input={} peak={peak}",
        clean.len()
    );
    let outer = fixed_outer(clean.clone());
    let (_, peak) =
        fuzz_support::assert_measured("clean-outer-zip", ORDINARY_ALLOCATION_BYTES, || {
            import_rprov(Cursor::new(outer.as_slice())).expect("the clean outer ZIP imports")
        });
    println!(
        "FUZZ_PEAK case=clean-outer-zip input={} peak={peak}",
        outer.len()
    );

    for (label, header) in [
        (
            "entry-count-at-limit",
            container_header(MAX_RPROV_ARCHIVE_ENTRIES as u32, 0),
        ),
        (
            "entry-count-over-limit",
            container_header(MAX_RPROV_ARCHIVE_ENTRIES as u32 + 1, 0),
        ),
        (
            "stored-bytes-at-limit",
            container_header(1, MAX_RPROV_STORED_BYTES),
        ),
        (
            "stored-bytes-over-limit",
            container_header(1, MAX_RPROV_STORED_BYTES + 1),
        ),
    ] {
        let outcome = fuzz_support::assert_bounded(label, ALLOCATION_BOUND, || {
            import_rprov(Cursor::new(header))
        });
        assert!(outcome.is_err(), "{label} was accepted");
    }

    for (label, payload_bytes) in [
        ("record-payload-at-limit", MAX_RPROV_RECORD_PAYLOAD_BYTES),
        (
            "record-payload-over-limit",
            MAX_RPROV_RECORD_PAYLOAD_BYTES + 1,
        ),
    ] {
        let mut bytes = container_header(1, RPROV_RECORD_HEADER_BYTES as u64 + 12);
        bytes.extend(
            encode_rprov_record_header(&RprovRecordHeader {
                path_bytes: 12,
                entry_type: RprovRecordType::RegularFile,
                payload_bytes,
            })
            .unwrap_or([0_u8; RPROV_RECORD_HEADER_BYTES]),
        );
        bytes.extend(b"manifest.json");
        let outcome = fuzz_support::assert_bounded(label, ALLOCATION_BOUND, || {
            import_rprov(Cursor::new(bytes))
        });
        assert!(outcome.is_err(), "{label} was accepted");
    }

    // The manifest is not the importer's only declared-length allocation: it
    // sizes the capture buffer for every checkpoint and runtime-metadata entry
    // from the manifest's declared byte_length before reading it.
    for (label, declared, present) in [
        (
            "checkpoint-claim-at-ceiling",
            MAX_RPROV_CHECKPOINT_ENCODED_BYTES,
            false,
        ),
        (
            "checkpoint-claim-over-ceiling",
            MAX_RPROV_CHECKPOINT_ENCODED_BYTES + 1,
            false,
        ),
    ] {
        let container = checkpoint_claim(declared, present);
        let (outcome, peak) = fuzz_support::assert_measured(label, ALLOCATION_BOUND, || {
            import_rprov(Cursor::new(container))
        });
        assert!(outcome.is_err(), "{label} was accepted");
        if declared == MAX_RPROV_CHECKPOINT_ENCODED_BYTES {
            assert!(
                peak >= MAX_RPROV_CHECKPOINT_ENCODED_BYTES as usize,
                "{label} allocated only {peak} bytes, so it no longer reaches the capture buffer \
                 it exists to exercise"
            );
        } else {
            assert!(
                peak <= ORDINARY_ALLOCATION_BYTES,
                "{label} allocated {peak} bytes; a claim above the ceiling must be refused \
                 before allocating"
            );
        }
        println!("FUZZ_PEAK case={label} peak={peak}");
    }

    // The outer LMS ZIP central directory is parsed in full before any entry
    // budget applies, and each entry retains four owned copies of its name —
    // the raw bytes on the entry, the exact-name set, the case-folded alias
    // set, and the normalized WorkspacePath — or five when the entry is
    // classified as a source file and also lands in source_file_aliases. This
    // is one of the target's larger measured peaks, and unlike the others it
    // is reached before the wrapper's shape is checked; the family table above
    // gives its place among them.
    let directory_zip = outer_directory_zip(MAX_RPROV_ARCHIVE_ENTRIES);
    let (outcome, peak) =
        fuzz_support::assert_measured("outer-directory-at-limit", ALLOCATION_BOUND, || {
            import_rprov(Cursor::new(directory_zip.as_slice()))
        });
    assert!(outcome.is_err(), "outer-directory-at-limit was accepted");
    assert!(
        peak > MAX_RPROV_CHECKPOINT_ENCODED_BYTES as usize,
        "outer-directory-at-limit peaked at {peak} bytes, below the checkpoint-entry ceiling, so \
         it no longer measures the directory parse it exists to exercise"
    );
    println!(
        "FUZZ_PEAK case=outer-directory-at-limit input={} peak={peak}",
        directory_zip.len()
    );

    // Containers whose manifest is large enough that decoding it, not any one
    // declared length, is the importer's dominant allocation.
    for (label, segments, checkpoints, links, ceiling) in [
        ("manifest-decode-one-segment", 1, 1_024, 4_096, false),
        ("manifest-decode-four-segment", 4, 1_022, 1_000, false),
        (
            "manifest-decode-with-ceiling-checkpoint",
            4,
            1_022,
            1_000,
            true,
        ),
    ] {
        let container = large_manifest_container(segments, checkpoints, links, ceiling);
        let (outcome, peak) = fuzz_support::assert_measured(label, ALLOCATION_BOUND, || {
            import_rprov(Cursor::new(container.as_slice()))
        });
        assert!(outcome.is_err(), "{label} was accepted");
        println!(
            "FUZZ_PEAK case={label} input={} peak={peak}",
            container.len()
        );
    }

    // Extra fields and entry comments are refused by their declared length
    // alone, so neither is ever read into a buffer. Measured rather than read
    // off the source, because that is what the rest of this audit asserts.
    for (label, extra, comment) in [
        ("outer-extra-field", 1_u16, 0_u16),
        ("outer-entry-comment", 0, 1),
        ("outer-extra-field-at-max", u16::MAX, 0),
        ("outer-entry-comment-at-max", 0, u16::MAX),
    ] {
        let archive = outer_directory_zip_with(1, extra, comment);
        let (outcome, peak) = fuzz_support::assert_measured(label, ALLOCATION_BOUND, || {
            import_rprov(Cursor::new(archive.as_slice()))
        });
        assert!(outcome.is_err(), "{label} was accepted");
        assert!(
            peak <= ORDINARY_ALLOCATION_BYTES,
            "{label} allocated {peak} bytes; a declared extra field or comment must be refused \
             before anything is read"
        );
        println!("FUZZ_PEAK case={label} peak={peak}");
    }

    // A legitimate near-maximum package: the same fixture with a real
    // checkpoint payload at the format's own entry ceiling, digest and all.
    // This is what an ordinary full-size submission costs to import.
    let large = checkpoint_claim(MAX_RPROV_CHECKPOINT_ENCODED_BYTES, true);
    let (imported, peak) =
        fuzz_support::assert_measured("near-maximum-package", ALLOCATION_BOUND, || {
            import_rprov(Cursor::new(large.as_slice()))
        });
    assert!(
        imported.is_ok(),
        "the near-maximum package failed to import: {imported:?}"
    );
    assert!(
        peak > MAX_RPROV_MANIFEST_BYTES,
        "the near-maximum package peaked at {peak} bytes, below the manifest ceiling, so it no \
         longer measures a full-size successful import"
    );
    println!(
        "FUZZ_PEAK case=near-maximum-package input={} peak={peak}",
        large.len()
    );

    let ceiling = MAX_RPROV_MANIFEST_BYTES as u64;
    for (label, manifest_bytes, reaches_ceiling) in [
        ("manifest-bytes-at-limit", ceiling, true),
        ("manifest-bytes-over-limit", ceiling + 1, false),
    ] {
        let container = manifest_claim(manifest_bytes);
        assert_eq!(container.len(), 69, "the boundary input changed size");
        let (outcome, peak) = fuzz_support::assert_measured(label, ALLOCATION_BOUND, || {
            import_rprov(Cursor::new(container))
        });
        assert!(outcome.is_err(), "{label} was accepted");
        if reaches_ceiling {
            assert!(
                peak >= MAX_RPROV_MANIFEST_BYTES,
                "{label} allocated only {peak} bytes, so it no longer reaches the manifest \
                 buffer it exists to exercise"
            );
        } else {
            assert!(
                peak <= ORDINARY_ALLOCATION_BYTES,
                "{label} allocated {peak} bytes; a claim above the ceiling must be refused \
                 before allocating"
            );
        }
    }

    for length in [0, 1, 3, 4, RPROV_CONTAINER_HEADER_BYTES - 1] {
        let truncated = vec![b'R'; length];
        fuzz_support::assert_bounded(&format!("truncated-{length}"), ALLOCATION_BOUND, || {
            let _ = import_rprov(Cursor::new(truncated));
        });
    }
}

/// Record paths a hostile container might claim, including the real entry
/// names, traversal and absolute forms, and boundary-length components.
fn record_path() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("manifest.json".to_owned()),
        Just("segments/0001/events.jsonl".to_owned()),
        Just("initial-workspace/blobs/deadbeef".to_owned()),
        Just(String::new()),
        Just("../escape".to_owned()),
        Just("/absolute".to_owned()),
        Just("segments/0001/../0001/events.jsonl".to_owned()),
        Just("a".repeat(MAX_RPROV_ARCHIVE_COMPONENT_BYTES)),
        Just("a".repeat(MAX_RPROV_ARCHIVE_COMPONENT_BYTES + 1)),
        Just(vec!["a"; MAX_RPROV_ARCHIVE_PATH_DEPTH + 1].join("/")),
        Just("segments/0001/checkpoints/00000000000000000001.rcpk".to_owned()),
        // A package-supplied path named after the marker, so the check that no
        // such file appears in the sandbox has something real to be about.
        Just(PRIVATE_MARKER.to_owned()),
        Just(format!("segments/0001/{PRIVATE_MARKER}")),
    ]
}

/// One record: its claimed path length, type byte, and claimed payload length
/// may each disagree with the bytes that follow it.
#[derive(Clone, Debug)]
struct RecordSeed {
    path: String,
    path_bytes_claim: HeaderClaim,
    entry_type: u8,
    payload: Vec<u8>,
    payload_bytes_claim: HeaderClaim,
    reserved: u8,
}

/// How a declared length relates to the truth that follows it.
#[derive(Clone, Copy, Debug)]
enum HeaderClaim {
    Exact,
    OneLess,
    OneMore,
    Zero,
    Maximum,
    Overflowing,
}

impl HeaderClaim {
    fn apply(self, truth: u64, maximum: u64) -> u64 {
        match self {
            Self::Exact => truth,
            Self::OneLess => truth.saturating_sub(1),
            Self::OneMore => truth.saturating_add(1),
            Self::Zero => 0,
            Self::Maximum => maximum,
            Self::Overflowing => u64::MAX,
        }
    }
}

fn header_claim() -> impl Strategy<Value = HeaderClaim> {
    prop_oneof![
        4 => Just(HeaderClaim::Exact),
        1 => Just(HeaderClaim::OneLess),
        1 => Just(HeaderClaim::OneMore),
        1 => Just(HeaderClaim::Zero),
        1 => Just(HeaderClaim::Maximum),
        1 => Just(HeaderClaim::Overflowing),
    ]
}

/// The structure-aware container strategy, shared by the fuzz sub-target and
/// the coverage tally so both explore exactly the same space.
fn container_strategy() -> impl Strategy<Value = (Vec<RecordSeed>, HeaderClaim, HeaderClaim, u32)> {
    (
        proptest::collection::vec(record_seed(), 0..8),
        header_claim(),
        header_claim(),
        // Likewise the container format version: 1 is the only supported one,
        // so drawing it most of the time keeps the unsupported-version check
        // covered without spending most cases on it.
        prop_oneof![6 => Just(1_u32), 1 => 0_u32..4],
    )
}

fn record_seed() -> impl Strategy<Value = RecordSeed> {
    (
        record_path(),
        header_claim(),
        // 1 is RPROV_RECORD_REGULAR_FILE, the only legal record type. Drawing
        // it most of the time is what carries a case past the type check into
        // the path-length, payload-length, aggregate-total, and manifest paths.
        prop_oneof![6 => Just(1_u8), 1 => any::<u8>()],
        proptest::collection::vec(any::<u8>(), 0..256),
        header_claim(),
        prop_oneof![6 => Just(0_u8), 1 => any::<u8>()],
    )
        .prop_map(
            |(path, path_bytes_claim, entry_type, payload, payload_bytes_claim, reserved)| {
                RecordSeed {
                    path,
                    path_bytes_claim,
                    entry_type,
                    payload,
                    payload_bytes_claim,
                    reserved,
                }
            },
        )
}

/// Builds a container whose declared counts, lengths, and totals may contradict
/// the record bytes that follow, exercising the importer's checked aggregate
/// arithmetic rather than only its byte-level rejections.
fn structured_container(
    records: &[RecordSeed],
    entry_claim: HeaderClaim,
    stored_claim: HeaderClaim,
    version: u32,
) -> Vec<u8> {
    let mut body = Vec::new();
    for record in records {
        let path = record.path.as_bytes();
        let path_claim = record
            .path_bytes_claim
            .apply(path.len() as u64, u64::from(u16::MAX));
        let payload_claim = record
            .payload_bytes_claim
            .apply(record.payload.len() as u64, MAX_RPROV_RECORD_PAYLOAD_BYTES);
        let mut header = [0_u8; RPROV_RECORD_HEADER_BYTES];
        header[..2].copy_from_slice(&(path_claim as u16).to_le_bytes());
        header[2] = record.entry_type;
        header[3] = record.reserved;
        header[8..16].copy_from_slice(&payload_claim.to_le_bytes());
        body.extend(header);
        body.extend(path);
        body.extend(&record.payload);
    }

    let entry_count = entry_claim.apply(records.len() as u64, u64::from(u32::MAX));
    let stored = stored_claim.apply(body.len() as u64, MAX_RPROV_STORED_BYTES);
    let mut bytes = vec![0_u8; RPROV_CONTAINER_HEADER_BYTES];
    bytes[..8].copy_from_slice(b"RUSTPROV");
    bytes[8..12].copy_from_slice(&version.to_le_bytes());
    bytes[12..14].copy_from_slice(&(RPROV_CONTAINER_HEADER_BYTES as u16).to_le_bytes());
    bytes[16..20].copy_from_slice(&(entry_count as u32).to_le_bytes());
    bytes[24..32].copy_from_slice(&stored.to_le_bytes());
    bytes[32..40].copy_from_slice(&stored.to_le_bytes());
    bytes.extend(body);
    bytes
}

/// A container whose single record is a `manifest.json` claiming
/// `manifest_bytes` of payload, with nothing after the record path.
///
/// Sixty-nine bytes in total: everything the importer checks before it sizes
/// the manifest buffer is satisfied, so the claim alone decides how much it
/// allocates.
fn manifest_claim(manifest_bytes: u64) -> Vec<u8> {
    const PATH: &[u8] = b"manifest.json";
    let stored = RPROV_RECORD_HEADER_BYTES as u64 + PATH.len() as u64 + manifest_bytes;
    let mut bytes = container_header(1, stored);
    let mut header = [0_u8; RPROV_RECORD_HEADER_BYTES];
    header[..2].copy_from_slice(&(PATH.len() as u16).to_le_bytes());
    header[2] = 1;
    header[8..16].copy_from_slice(&manifest_bytes.to_le_bytes());
    bytes.extend(header);
    bytes.extend(PATH);
    bytes
}

fn container_header(entry_count: u32, stored_records_bytes: u64) -> Vec<u8> {
    encode_rprov_container_header(&RprovContainerHeader {
        format_version: 1,
        entry_count,
        stored_records_bytes,
        expanded_records_bytes: stored_records_bytes,
    })
    .map_or_else(
        |_| {
            let mut bytes = b"RUSTPROV".to_vec();
            bytes.resize(RPROV_CONTAINER_HEADER_BYTES, 0);
            bytes
        },
        |header| header.to_vec(),
    )
}

fn hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; Hash::LENGTH])
}

fn session(value: &str) -> SessionId {
    SessionId::new(value).unwrap()
}

fn producer() -> RprovProducer {
    RprovProducer {
        client_version: RprovKnown::Unknown,
        build_identity: RprovKnown::Unknown,
        os: RprovKnown::Unknown,
        architecture: RprovKnown::Unknown,
        rust_tools: vec![],
    }
}

fn push_event(events: &mut Vec<EventEnvelope>, session_id: &SessionId, event: Event) {
    let previous = events
        .last()
        .map_or_else(Hash::zero, |envelope| envelope.event_hash);
    let sequence = events.len() as u64 + 1;
    events.push(
        EventEnvelope {
            format_version: FORMAT_VERSION_V1,
            session_id: session_id.clone(),
            sequence,
            monotonic_millis: sequence * 10,
            wall_clock_utc: None,
            previous_event_hash: Hash::zero(),
            event_hash: Hash::zero(),
            event,
        }
        .seal(previous)
        .unwrap(),
    );
}

fn event_reference(event: &EventEnvelope) -> RecordedEventRef {
    RecordedEventRef {
        session_id: event.session_id.clone(),
        sequence: event.sequence,
        event_hash: event.event_hash,
    }
}

fn jsonl(events: &[EventEnvelope]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for event in events {
        bytes.extend(encode_envelope(event).unwrap());
        bytes.push(b'\n');
    }
    bytes
}

fn segment_material(
    ordinal: u32,
    initial_tree: Hash,
    final_tree: Hash,
) -> (RprovSegment, Vec<(String, Vec<u8>)>) {
    let session_id = session(&format!("session-{ordinal}"));
    let mut events = Vec::new();
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: initial_tree,
            documents: vec![],
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: final_tree,
            documents: vec![],
        }),
    );
    push_event(
        &mut events,
        &session_id,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: final_tree,
            event_count: 3,
            clean: true,
            warnings: vec![],
        }),
    );

    let event_bytes = jsonl(&events);
    let initial_checkpoint = format!("RUSTCPK\0\0\0\0\x01initial-{ordinal}").into_bytes();
    let final_checkpoint = format!("RUSTCPK\0\0\0\0\x01final-{ordinal}").into_bytes();
    let prefix = format!("segments/{ordinal:04}");
    let event_path = format!("{prefix}/events.jsonl");
    let initial_path = format!("{prefix}/checkpoints/{:020}.rcpk", 1_u64);
    let final_path = format!("{prefix}/checkpoints/{:020}.rcpk", 2_u64);
    let segment = RprovSegment {
        ordinal,
        session_id: session_id.clone(),
        course_id: "ECE1724".to_owned(),
        assignment_id: "a3".to_owned(),
        assignment_version: "2026-09-01".to_owned(),
        assignment_manifest_blake3: hash(3),
        original_starter_tree_hash: hash(1),
        initial_tree_hash: initial_tree,
        producer: producer(),
        time: RprovSegmentTime {
            started_at_utc: RprovKnown::Unknown,
            ended_at_utc: RprovKnown::Unknown,
            inter_attempt_time: RprovInterAttemptTime::Unknown,
        },
        parent: None,
        events: RprovEventStreamRef {
            format_version: 1,
            entry: event_path.clone(),
            byte_length: event_bytes.len() as u64,
            blake3: rprov_raw_blake3(&event_bytes),
            completeness: RprovEventStreamCompleteness::Complete,
        },
        checkpoints: vec![
            RprovCheckpointRef {
                role: RprovCheckpointRole::Initial,
                format_version: 1,
                entry: initial_path.clone(),
                byte_length: initial_checkpoint.len() as u64,
                blake3: rprov_raw_blake3(&initial_checkpoint),
                owner: event_reference(&events[0]),
                workspace_hash: initial_tree,
            },
            RprovCheckpointRef {
                role: RprovCheckpointRole::Final,
                format_version: 1,
                entry: final_path.clone(),
                byte_length: final_checkpoint.len() as u64,
                blake3: rprov_raw_blake3(&final_checkpoint),
                owner: event_reference(&events[1]),
                workspace_hash: final_tree,
            },
        ],
        metadata: vec![],
        evidence: vec![],
        source_links: vec![],
        inclusive_event_count: events.len() as u64,
        last_event_hash: events.last().unwrap().event_hash,
        terminal_event_hash: RprovKnown::Known {
            value: events.last().unwrap().event_hash,
        },
        final_tree_hash: RprovKnown::Known { value: final_tree },
    };
    (
        segment,
        vec![
            (initial_path, initial_checkpoint),
            (final_path, final_checkpoint),
            (event_path, event_bytes),
        ],
    )
}

/// The clean one-segment package used as the mutation seed. Its starter blob
/// carries [`PRIVATE_MARKER`] so leaked payload bytes are detectable.
struct Fixture {
    manifest: RprovManifest,
    payloads: BTreeMap<String, Vec<u8>>,
}

impl Fixture {
    fn one_segment() -> Self {
        let starter = format!("fn main() {{ /* {PRIVATE_MARKER} */ }}\n").into_bytes();
        let starter_digest = rprov_raw_blake3(&starter);
        let starter_path = format!("initial-workspace/blobs/{starter_digest}");
        let (segment, segment_payloads) = segment_material(1, hash(1), hash(4));
        let mut payloads = BTreeMap::from([(starter_path.clone(), starter)]);
        payloads.extend(segment_payloads);
        let inventory = payloads
            .iter()
            .map(|(path, bytes)| RprovInventoryEntry {
                path: path.clone(),
                byte_length: bytes.len() as u64,
                blake3: rprov_raw_blake3(bytes),
                kind: if path.starts_with("initial-workspace/") {
                    RprovEntryKind::InitialWorkspaceBlob
                } else if path.ends_with("events.jsonl") {
                    RprovEntryKind::Events
                } else {
                    RprovEntryKind::Checkpoint
                },
            })
            .collect();
        let manifest = RprovManifest {
            format_version: 1,
            package_state: RprovPackageState::CleanFinalized,
            submitted_source_comparison: RprovSubmittedSourceComparison::UnavailableStandalone,
            course_id: "ECE1724".to_owned(),
            assignment_id: "a3".to_owned(),
            assignment_version: "2026-09-01".to_owned(),
            student_id: "student-1".to_owned(),
            latest_session_id: session("session-1"),
            original_starter_tree_hash: hash(1),
            test_case_suite_hash: None,
            final_tree_hash: RprovKnown::Known { value: hash(4) },
            aggregate_event_count: 3,
            producer: producer(),
            assignment_manifest: RprovAssignmentManifestIdentity {
                format_version: 1,
                byte_length: 123,
                blake3: hash(3),
            },
            initial_workspace: RprovInitialWorkspace {
                files: vec![RprovInitialWorkspaceFile {
                    path: WorkspacePath::new("src/main.rs").unwrap(),
                    entry: starter_path,
                }],
            },
            segments: vec![segment],
            inventory,
        };
        manifest.validate().unwrap();
        Self { manifest, payloads }
    }

    fn encode(&self) -> Vec<u8> {
        let manifest = encode_rprov_manifest(&self.manifest).unwrap();
        let records = std::iter::once(("manifest.json".to_owned(), manifest))
            .chain(self.manifest.inventory.iter().map(|entry| {
                (
                    entry.path.clone(),
                    self.payloads.get(&entry.path).unwrap().clone(),
                )
            }))
            .collect();
        encode_records(records)
    }
}

/// Rewrites the fixture's initial checkpoint entry to declare `byte_length`
/// bytes, and supplies the payload only when `present`.
///
/// The manifest still validates, so the importer reaches
/// `copy_inner_payload`, which sizes its capture buffer from this declaration
/// before reading any of it. With the payload absent that allocation happens on
/// a container of a few kilobytes.
fn checkpoint_claim(byte_length: u64, present: bool) -> Vec<u8> {
    // The encoder validates, so an over-ceiling claim is built by encoding at
    // the ceiling and then raising the two declared lengths in the manifest
    // bytes. The importer parses those bytes and refuses them at manifest
    // validation, before anything is sized from them.
    let encoded_length = byte_length.min(MAX_RPROV_CHECKPOINT_ENCODED_BYTES);
    let mut fixture = Fixture::one_segment();
    let entry = fixture.manifest.segments[0].checkpoints[0].entry.clone();
    let payload = if present {
        // The importer preflights the checkpoint magic and declared version
        // before it looks at the body, so the payload carries the same header
        // the small fixture uses and random bytes after it.
        let mut bytes = b"RUSTCPK\0\0\0\0\x01".to_vec();
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        bytes.resize(encoded_length as usize, 0);
        for byte in bytes.iter_mut().skip(12) {
            *byte = next_byte(&mut state);
        }
        fixture.payloads.insert(entry.clone(), bytes);
        fixture.payloads[&entry].clone()
    } else {
        Vec::new()
    };
    let digest = rprov_raw_blake3(&payload);
    for checkpoint in &mut fixture.manifest.segments[0].checkpoints {
        if checkpoint.entry == entry {
            checkpoint.byte_length = encoded_length;
            if present {
                checkpoint.blake3 = digest;
            }
        }
    }
    for inventory in &mut fixture.manifest.inventory {
        if inventory.path == entry {
            inventory.byte_length = encoded_length;
            if present {
                inventory.blake3 = digest;
            }
        }
    }
    // A claim at or below the entry ceiling still produces a manifest the
    // importer will accept and act on, which is what carries the case as far
    // as the capture allocation. A claim above it is refused by manifest
    // validation, long before anything is sized from it.
    fixture
        .manifest
        .validate()
        .expect("the fixture manifest validates at the entry ceiling");

    let mut manifest = encode_rprov_manifest(&fixture.manifest).unwrap();
    if byte_length > encoded_length {
        let from = format!("\"byte_length\":{encoded_length}");
        let to = format!("\"byte_length\":{byte_length}");
        let text = String::from_utf8(manifest).expect("the manifest is UTF-8");
        assert_eq!(
            text.matches(&from).count(),
            2,
            "the ceiling length should appear once in the segment and once in the inventory"
        );
        manifest = text.replace(&from, &to).into_bytes();
        assert!(
            fixture.manifest.validate().is_ok(),
            "only the encoded bytes carry the over-ceiling claim"
        );
    }
    let mut records = vec![("manifest.json".to_owned(), manifest, None)];
    for inventory in &fixture.manifest.inventory {
        let declared = (inventory.path == entry).then_some(encoded_length);
        records.push((
            inventory.path.clone(),
            fixture.payloads[&inventory.path].clone(),
            declared,
        ));
    }
    encode_claimed_records(records, !present, None)
}

/// Encodes records whose declared payload length may exceed the bytes that
/// follow, optionally stopping after the first such record's path.
fn encode_claimed_records(
    records: Vec<(String, Vec<u8>, Option<u64>)>,
    truncate_at_claim: bool,
    declared_entries: Option<u32>,
) -> Vec<u8> {
    let stored_records_bytes = records.iter().fold(0_u64, |total, (path, bytes, claim)| {
        total
            + RPROV_RECORD_HEADER_BYTES as u64
            + path.len() as u64
            + claim.unwrap_or(bytes.len() as u64)
    });
    let mut encoded = container_header(
        declared_entries.unwrap_or(records.len() as u32),
        stored_records_bytes,
    );
    for (path, payload, claim) in records {
        let payload_bytes = claim.unwrap_or(payload.len() as u64);
        encoded.extend(
            encode_rprov_record_header(&RprovRecordHeader {
                path_bytes: path.len() as u16,
                entry_type: RprovRecordType::RegularFile,
                payload_bytes,
            })
            .unwrap(),
        );
        encoded.extend(path.as_bytes());
        if claim.is_some() && truncate_at_claim {
            return encoded;
        }
        encoded.extend(payload);
    }
    encoded
}

/// A legal container whose manifest is large enough to make `decode_rprov_manifest`
/// the importer's dominant allocation.
///
/// The manifest buffer, the `serde_json` value, the converted `RprovManifest`,
/// and the canonical re-encoding are live together, and the importer keeps the
/// manifest bytes for the rest of the record loop, so they also overlap the
/// checkpoint capture buffer. Every count here is inside the documented
/// limits: `checkpoints` per segment against `MAX_RPROV_CHECKPOINTS_PER_SEGMENT`,
/// `links` per segment against `MAX_RPROV_SOURCE_LINKS_PER_SEGMENT` and the
/// package total, and the inventory against `MAX_RPROV_ARCHIVE_ENTRIES`.
///
/// The container is truncated after the records named, because the peak is
/// reached while the manifest is being decoded and, for
/// `ceiling_checkpoint`, while the first checkpoint's capture buffer is
/// sized from it.
fn large_manifest_container(
    segments: u32,
    checkpoints: u64,
    links: u64,
    ceiling_checkpoint: bool,
) -> Vec<u8> {
    let events = 4 * (checkpoints + links) + 16;
    let mut inventory: Vec<RprovInventoryEntry> = Vec::new();
    let mut built = Vec::new();
    let mut parent: Option<RprovParentLink> = None;

    for ordinal in 1..=segments {
        let session_id = session(&format!("session-{ordinal}"));
        let event_path = format!("segments/{ordinal:04}/events.jsonl");
        let terminal = hash((ordinal + 40) as u8);
        let final_tree = hash((ordinal + 80) as u8);
        let initial_tree = if ordinal == 1 {
            hash(1)
        } else {
            hash((ordinal + 79) as u8)
        };

        let checkpoint_refs = (0..checkpoints)
            .map(|index| {
                let sequence = index + 1;
                let entry = format!("segments/{ordinal:04}/checkpoints/{sequence:020}.rcpk");
                let byte_length = if ordinal == 1 && index == 0 && ceiling_checkpoint {
                    MAX_RPROV_CHECKPOINT_ENCODED_BYTES
                } else {
                    64
                };
                inventory.push(RprovInventoryEntry {
                    path: entry.clone(),
                    byte_length,
                    blake3: hash(index as u8),
                    kind: RprovEntryKind::Checkpoint,
                });
                RprovCheckpointRef {
                    role: if index == 0 {
                        RprovCheckpointRole::Initial
                    } else if index + 1 == checkpoints {
                        RprovCheckpointRole::Final
                    } else {
                        RprovCheckpointRole::Accepted
                    },
                    format_version: 1,
                    entry,
                    byte_length,
                    blake3: hash(index as u8),
                    owner: RecordedEventRef {
                        session_id: session_id.clone(),
                        sequence,
                        event_hash: hash(index as u8),
                    },
                    workspace_hash: if index == 0 {
                        initial_tree
                    } else if index + 1 == checkpoints {
                        final_tree
                    } else {
                        hash(index as u8)
                    },
                }
            })
            .collect::<Vec<_>>();

        let source_links = (0..links)
            .map(|index| {
                let paste = checkpoints + 2 * index + 2;
                RprovSourceLink::InternalPaste {
                    paste_event: RecordedEventRef {
                        session_id: session_id.clone(),
                        sequence: paste,
                        event_hash: hash(index as u8),
                    },
                    copied_event: RecordedEventRef {
                        session_id: session_id.clone(),
                        sequence: paste - 1,
                        event_hash: hash(index as u8),
                    },
                    document_id: DocumentId::new(format!("document-{index:06}-{}", "d".repeat(48)))
                        .unwrap(),
                    path: WorkspacePath::new(format!(
                        "src/{}/module{index:06}.rs",
                        "p".repeat(200)
                    ))
                    .unwrap(),
                    version: index,
                    content_hash: hash(index as u8),
                    start_byte: 0,
                    end_byte: 32,
                }
            })
            .collect::<Vec<_>>();

        inventory.push(RprovInventoryEntry {
            path: event_path.clone(),
            byte_length: 64,
            blake3: hash(7),
            kind: RprovEntryKind::Events,
        });

        built.push(RprovSegment {
            ordinal,
            session_id: session_id.clone(),
            course_id: "ECE1724".to_owned(),
            assignment_id: "a3".to_owned(),
            assignment_version: "2026-09-01".to_owned(),
            assignment_manifest_blake3: hash(3),
            original_starter_tree_hash: hash(1),
            initial_tree_hash: initial_tree,
            producer: producer(),
            time: RprovSegmentTime {
                started_at_utc: RprovKnown::Unknown,
                ended_at_utc: RprovKnown::Unknown,
                inter_attempt_time: RprovInterAttemptTime::Unknown,
            },
            parent: parent.take(),
            events: RprovEventStreamRef {
                format_version: 1,
                entry: event_path,
                byte_length: 64,
                blake3: hash(7),
                completeness: RprovEventStreamCompleteness::Complete,
            },
            checkpoints: checkpoint_refs,
            metadata: vec![],
            evidence: vec![],
            source_links,
            inclusive_event_count: events,
            last_event_hash: terminal,
            terminal_event_hash: RprovKnown::Known { value: terminal },
            final_tree_hash: RprovKnown::Known { value: final_tree },
        });
        parent = Some(RprovParentLink {
            session_id,
            terminal_event_hash: terminal,
            final_tree_hash: final_tree,
        });
    }

    inventory.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    let last = built.last().expect("at least one segment");
    let manifest = RprovManifest {
        format_version: 1,
        package_state: if segments == 1 {
            RprovPackageState::CleanFinalized
        } else {
            RprovPackageState::RecoveryIncomplete {
                unavailable_assurances: vec![RprovUnavailableAssurance::ReplayConsistency],
                gaps: vec![],
            }
        },
        submitted_source_comparison: RprovSubmittedSourceComparison::UnavailableStandalone,
        course_id: "ECE1724".to_owned(),
        assignment_id: "a3".to_owned(),
        assignment_version: "2026-09-01".to_owned(),
        student_id: "student-1".to_owned(),
        latest_session_id: last.session_id.clone(),
        original_starter_tree_hash: hash(1),
        test_case_suite_hash: None,
        final_tree_hash: last.final_tree_hash.clone(),
        aggregate_event_count: events * u64::from(segments),
        producer: producer(),
        assignment_manifest: RprovAssignmentManifestIdentity {
            format_version: 1,
            byte_length: 123,
            blake3: hash(3),
        },
        initial_workspace: RprovInitialWorkspace { files: vec![] },
        segments: built,
        inventory,
    };
    manifest
        .validate()
        .expect("the large manifest is legal under every documented limit");

    let encoded = encode_rprov_manifest(&manifest).expect("the large manifest encodes");
    let records = std::iter::once(("manifest.json".to_owned(), encoded, None))
        .chain(
            manifest
                .inventory
                .iter()
                .take(1)
                .map(|entry| (entry.path.clone(), Vec::new(), Some(entry.byte_length))),
        )
        .collect::<Vec<_>>();
    // The header declares the whole inventory, so the importer enters the
    // record loop after decoding the manifest instead of rejecting on the
    // entry count; the capture buffer for the first record is then sized while
    // the manifest bytes and the decoded manifest are still resident.
    let declared = manifest.inventory.len() as u32 + 1;
    encode_claimed_records(records, true, Some(declared))
}

/// An outer LMS ZIP whose central directory declares `entries` stored
/// directory records, each with a name at the largest length the importer
/// accepts.
///
/// One local header carries the prefix the importer dispatches on; after that
/// only the central directory and the end record are written, because the
/// importer parses the whole directory before it checks that exactly one
/// `session.rprov` is present and so never reads a local header here. Names
/// are four components of the maximum component length, which is the longest
/// `WorkspacePath` reachable, plus the trailing separator.
fn outer_directory_zip(entries: usize) -> Vec<u8> {
    outer_directory_zip_with(entries, 0, 0)
}

/// As [`outer_directory_zip`], with declared extra-field and comment lengths
/// on every central record.
fn outer_directory_zip_with(entries: usize, extra: u16, comment: u16) -> Vec<u8> {
    const COMPONENT: usize = MAX_WORKSPACE_COMPONENT_BYTES;

    // The importer dispatches on the first four bytes, so the archive has to
    // open with a local header even though this input is rejected long before
    // local headers are validated.
    let mut output = Vec::new();
    push_u32(&mut output, 0x0403_4b50);
    push_u16(&mut output, 20);
    push_u16(&mut output, 0);
    push_u16(&mut output, 0);
    push_u16(&mut output, 0);
    push_u16(&mut output, 0);
    push_u32(&mut output, 0);
    push_u32(&mut output, 0);
    push_u32(&mut output, 0);
    push_u16(&mut output, 1);
    push_u16(&mut output, 0);
    output.push(b'/');

    let mut central = Vec::new();
    for index in 0..entries {
        let mut name = [
            "a".repeat(COMPONENT),
            "b".repeat(COMPONENT),
            "c".repeat(COMPONENT),
            "d".repeat(COMPONENT),
        ]
        .join("/");
        assert_eq!(name.len(), 4 * COMPONENT + 3);
        assert!(name.len() <= MAX_WORKSPACE_PATH_BYTES);
        // Make each entry distinct without changing its length, so no two
        // collide in the exact-name or alias sets.
        let suffix = format!("{index:04x}");
        name.replace_range(name.len() - suffix.len().., &suffix);
        name.push('/');

        push_u32(&mut central, 0x0201_4b50);
        push_u16(&mut central, (3 << 8) | 20);
        push_u16(&mut central, 20);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, 0);
        push_u32(&mut central, 0);
        push_u32(&mut central, 0);
        push_u16(&mut central, name.len() as u16);
        push_u16(&mut central, extra);
        push_u16(&mut central, comment);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, (0o040755 << 16) | 0x10);
        push_u32(&mut central, 0);
        central.extend(name.as_bytes());
    }

    let central_offset = output.len() as u32;
    let central_size = central.len() as u32;
    output.extend(central);
    push_u32(&mut output, 0x0605_4b50);
    push_u16(&mut output, 0);
    push_u16(&mut output, 0);
    push_u16(&mut output, entries as u16);
    push_u16(&mut output, entries as u16);
    push_u32(&mut output, central_size);
    push_u32(&mut output, central_offset);
    push_u16(&mut output, 0);
    output
}

/// A deterministic xorshift stream, so a large fixture is identical every run.
fn next_byte(state: &mut u64) -> u8 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    (*state >> 24) as u8
}

fn encode_records(records: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let stored_records_bytes = records.iter().fold(0_u64, |total, (path, bytes)| {
        total + RPROV_RECORD_HEADER_BYTES as u64 + path.len() as u64 + bytes.len() as u64
    });
    let mut encoded = container_header(records.len() as u32, stored_records_bytes);
    for (path, payload) in records {
        encoded.extend(
            encode_rprov_record_header(&RprovRecordHeader {
                path_bytes: path.len() as u16,
                entry_type: RprovRecordType::RegularFile,
                payload_bytes: payload.len() as u64,
            })
            .unwrap(),
        );
        encoded.extend(path.as_bytes());
        encoded.extend(payload);
    }
    encoded
}

/// The fixed LMS ZIP layout: one `.rprov`, one deflated source file, and one
/// stored source file.
fn fixed_outer(rprov: Vec<u8>) -> Vec<u8> {
    let mut deflated = DeflateEncoder::new(Vec::new(), Compression::fast());
    deflated.write_all(b"fn main() {}\n").unwrap();
    let deflated = deflated.finish().unwrap();
    zip(&[
        ZipEntry::stored(
            "Cargo.toml",
            b"[package]\nname = \"student\"\n[workspace]\n".to_vec(),
        ),
        ZipEntry::stored("session.rprov", rprov),
        ZipEntry {
            method: 8,
            expanded: b"fn main() {}\n".to_vec(),
            ..ZipEntry::stored("src/main.rs", deflated)
        },
    ])
}

struct ZipEntry {
    name: String,
    stored: Vec<u8>,
    expanded: Vec<u8>,
    method: u16,
}

impl ZipEntry {
    fn stored(name: &str, bytes: Vec<u8>) -> Self {
        Self {
            name: name.to_owned(),
            expanded: bytes.clone(),
            stored: bytes,
            method: 0,
        }
    }
}

fn zip(entries: &[ZipEntry]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut central = Vec::new();
    for entry in entries {
        let offset = output.len() as u32;
        let crc = crc32fast::hash(&entry.expanded);
        let name = entry.name.as_bytes();

        push_u32(&mut output, 0x0403_4b50);
        push_u16(&mut output, 20);
        push_u16(&mut output, 0);
        push_u16(&mut output, entry.method);
        push_u16(&mut output, 0);
        push_u16(&mut output, 0);
        push_u32(&mut output, crc);
        push_u32(&mut output, entry.stored.len() as u32);
        push_u32(&mut output, entry.expanded.len() as u32);
        push_u16(&mut output, name.len() as u16);
        push_u16(&mut output, 0);
        output.extend(name);
        output.extend(&entry.stored);

        push_u32(&mut central, 0x0201_4b50);
        push_u16(&mut central, (3 << 8) | 20);
        push_u16(&mut central, 20);
        push_u16(&mut central, 0);
        push_u16(&mut central, entry.method);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, crc);
        push_u32(&mut central, entry.stored.len() as u32);
        push_u32(&mut central, entry.expanded.len() as u32);
        push_u16(&mut central, name.len() as u16);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, 0o100644 << 16);
        push_u32(&mut central, offset);
        central.extend(name);
    }
    let central_offset = output.len() as u32;
    let central_size = central.len() as u32;
    output.extend(central);
    push_u32(&mut output, 0x0605_4b50);
    push_u16(&mut output, 0);
    push_u16(&mut output, 0);
    push_u16(&mut output, entries.len() as u16);
    push_u16(&mut output, entries.len() as u16);
    push_u32(&mut output, central_size);
    push_u32(&mut output, central_offset);
    push_u16(&mut output, 0);
    output
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend(value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend(value.to_le_bytes());
}
