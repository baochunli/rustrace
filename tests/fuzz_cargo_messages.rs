//! Bounded fuzzing of the structured Cargo-message parser.

#[path = "fuzz_support/mod.rs"]
mod fuzz_support;

use proptest::prelude::*;
use rustrace::diagnostics::{
    CommandDiagnostics, MAX_ARTIFACTS, MAX_CARGO_MESSAGE_BYTES, MAX_DIAGNOSTIC_SPANS,
    MAX_DIAGNOSTICS, MAX_OUTPUT_LINE_BYTES, MAX_OUTPUT_LINES, derive_command_diagnostics,
};
use rustrace_model::{
    CaptureCompleteness, CommandCapture, CommandCaptureMode, CommandEnvironment, CommandId,
    CommandOutcome, CommandTool, CommandToolKind, CommandTreeLink, ControlledAction,
    ControlledCommandFinished, ControlledCommandOutput, ControlledCommandStarted, Hash,
    MAX_COMMAND_CHUNK_BYTES, MAX_COMMAND_OUTPUT_BYTES, OutputStream,
};

const SEED: [u8; 32] = *b"rustrace-cargo-message-fuzz-v1..";

const MUTATION_CEILING: usize = 128 * 1024;

/// The measured envelope for a finish record the rest of the system can
/// actually produce.
///
/// This is **not** a derived ceiling. `collect_stream` sizes each stream's
/// capture buffer from the finish record's declared byte count before reading
/// a chunk, clamped by the command's `output_limit` and
/// `MAX_COMMAND_OUTPUT_BYTES`; both streams are sized in the same call, and
/// the retained projections are live alongside them.
///
/// The family, one line per input:
///
/// | Input | Validates | Peak |
/// | --- | --- | --- |
/// | `declared-capture-one-ceiling`, the ceiling declared on one stream | yes | **8 389 160 B** |
/// | `declared-capture-split-ceiling`, the ceiling split across both | yes | 8 389 160 B |
/// | `diagnostics-257`, one more than `MAX_DIAGNOSTICS` compiler messages | — | 1 058 693 B |
/// | `message-262145`, one byte over the structured-message limit | — | under the ordinary guard |
/// | `output-lines-129`, one line over the retained-line limit | — | under the ordinary guard |
///
/// The bound is the largest of those, 8 389 160, times 1.25.
///
/// **The true worst case over all legal inputs is not derived and may be
/// higher than this.** The bound is an observed envelope guarding against
/// regression; what these cases establish is that the parser does not panic
/// and that its allocation is a bounded function of the documented limits.
const MEASURED_MAXIMUM: usize = 8_389_160;
const ALLOCATION_BOUND: usize = MEASURED_MAXIMUM * 5 / 4;

/// A record declaring the ceiling on *each* stream reaches 16 777 768 B, but
/// `ControlledCommandFinished::validate` requires
/// `stdout.bytes + stderr.bytes <= MAX_COMMAND_OUTPUT_BYTES` and every decoded
/// envelope is validated, so no journal can present one. It is measured and
/// bounded separately as a synthetic upper bound, so that admitting it does
/// not loosen everything else.
const SYNTHETIC_CAPTURE_BOUND: usize = 16_777_768 * 5 / 4;

/// The tight guard for every stream this target builds itself, including the
/// whole randomized budget and the fixed message loops. Each structured
/// message is capped at 256 kibibytes and every retained projection is a
/// bounded count; the largest peak measured is 1 058 693 B.
const ORDINARY_ALLOCATION_BYTES: usize = 16 * MAX_CARGO_MESSAGE_BYTES;

#[test]
fn fuzz_cargo_messages() {
    fuzz_support::isolated("fuzz_cargo_messages", body);
}

fn body() {
    boundary_cases();

    let fixtures = valid_messages();
    fuzz_support::run_cases(
        "cargo-message-parser",
        SEED,
        (
            proptest::collection::vec(
                (
                    proptest::sample::select(fixtures),
                    fuzz_support::mutations(4),
                ),
                0..6,
            ),
            proptest::collection::vec(any::<u8>(), 0..256),
            action(),
        ),
        |(messages, stderr, action)| {
            let mut stdout = Vec::new();
            for (fixture, operations) in messages {
                stdout.extend(fuzz_support::apply_mutations(
                    &fixture,
                    &operations,
                    MUTATION_CEILING,
                ));
                stdout.push(b'\n');
            }
            let derived = fuzz_support::bounded(ORDINARY_ALLOCATION_BYTES, || {
                derive(action, &stdout, &stderr)
            })?;
            check_bounds(&derived)
        },
    );

    fuzz_support::run_cases(
        "cargo-message-arbitrary-bytes",
        SEED,
        (
            proptest::collection::vec(any::<u8>(), 0..4096),
            proptest::collection::vec(any::<u8>(), 0..1024),
            action(),
        ),
        |(stdout, stderr, action)| {
            let derived = fuzz_support::bounded(ORDINARY_ALLOCATION_BYTES, || {
                derive(action, &stdout, &stderr)
            })?;
            check_bounds(&derived)
        },
    );
}

/// Every retained projection stays inside its documented cap.
fn check_bounds(derived: &CommandDiagnostics) -> Result<(), TestCaseError> {
    prop_assert!(
        derived.diagnostics.len() <= MAX_DIAGNOSTICS,
        "retained {} diagnostics",
        derived.diagnostics.len()
    );
    prop_assert!(
        derived.artifacts.len() <= MAX_ARTIFACTS,
        "retained {} artifacts",
        derived.artifacts.len()
    );
    prop_assert!(
        derived.output.len() <= MAX_OUTPUT_LINES,
        "retained {} output lines",
        derived.output.len()
    );
    for line in &derived.output {
        prop_assert!(
            line.bytes_hex.len() <= 2 * MAX_OUTPUT_LINE_BYTES,
            "retained a {}-hex-byte output line",
            line.bytes_hex.len()
        );
    }
    for diagnostic in &derived.diagnostics {
        prop_assert!(
            diagnostic.spans.len() <= MAX_DIAGNOSTIC_SPANS,
            "retained {} spans on one diagnostic",
            diagnostic.spans.len()
        );
    }
    Ok(())
}

/// Fixed inputs at each documented parser limit and at the limit plus one.
fn boundary_cases() {
    for length in [MAX_CARGO_MESSAGE_BYTES, MAX_CARGO_MESSAGE_BYTES + 1] {
        let mut line = b"{\"reason\":\"".to_vec();
        line.resize(length - 2, b'a');
        line.extend(b"\"}");
        assert_eq!(line.len(), length);
        let derived = fuzz_support::assert_bounded(
            &format!("message-{length}"),
            ORDINARY_ALLOCATION_BYTES,
            || derive(ControlledAction::Check, &line, b""),
        );
        check_bounds(&derived).expect("bounded projections");
    }

    for length in [MAX_OUTPUT_LINE_BYTES, MAX_OUTPUT_LINE_BYTES + 1] {
        let line = vec![b'x'; length];
        let derived = fuzz_support::assert_bounded(
            &format!("output-line-{length}"),
            ORDINARY_ALLOCATION_BYTES,
            || derive(ControlledAction::Check, &line, b""),
        );
        check_bounds(&derived).expect("bounded projections");
    }

    for count in [MAX_OUTPUT_LINES, MAX_OUTPUT_LINES + 1] {
        let stdout = vec![b'x'; count]
            .iter()
            .fold(Vec::new(), |mut bytes, byte| {
                bytes.push(*byte);
                bytes.push(b'\n');
                bytes
            });
        let derived = fuzz_support::assert_bounded(
            &format!("output-lines-{count}"),
            ORDINARY_ALLOCATION_BYTES,
            || derive(ControlledAction::Check, &stdout, b""),
        );
        check_bounds(&derived).expect("bounded projections");
    }

    // The parser sizes each stream's capture buffer from the declared byte
    // count before reading a chunk, capped by the command's output limit and
    // MAX_COMMAND_OUTPUT_BYTES. Both streams are sized in the same call, so
    // what the parser can be made to allocate depends on what a finish record
    // is allowed to declare across the two of them together.
    for (label, stdout, stderr, valid, bound, expected) in [
        (
            "declared-capture-one-ceiling",
            MAX_COMMAND_OUTPUT_BYTES,
            0,
            true,
            ALLOCATION_BOUND,
            MAX_COMMAND_OUTPUT_BYTES as usize,
        ),
        (
            "declared-capture-split-ceiling",
            MAX_COMMAND_OUTPUT_BYTES / 2,
            MAX_COMMAND_OUTPUT_BYTES / 2,
            true,
            ALLOCATION_BOUND,
            MAX_COMMAND_OUTPUT_BYTES as usize,
        ),
        (
            "declared-capture-synthetic-both-ceilings",
            MAX_COMMAND_OUTPUT_BYTES,
            MAX_COMMAND_OUTPUT_BYTES,
            false,
            SYNTHETIC_CAPTURE_BOUND,
            2 * MAX_COMMAND_OUTPUT_BYTES as usize,
        ),
        (
            "declared-capture-synthetic-over-ceiling",
            MAX_COMMAND_OUTPUT_BYTES + 1,
            MAX_COMMAND_OUTPUT_BYTES + 1,
            false,
            SYNTHETIC_CAPTURE_BOUND,
            2 * MAX_COMMAND_OUTPUT_BYTES as usize,
        ),
    ] {
        let start = started(ControlledAction::Check);
        let finish = ControlledCommandFinished {
            command_id: start.command_id.clone(),
            after: CommandTreeLink {
                checkpoint_sequence: 45,
                checkpoint_event_hash: hash(3),
                workspace_hash: start.before.workspace_hash,
                workspace_version: start.before.workspace_version,
            },
            started_millis: 100,
            finished_millis: 200,
            outcome: CommandOutcome::Exited { code: 1 },
            stdout: declared_capture(stdout),
            stderr: declared_capture(stderr),
        };
        // A record declaring a ceiling on each stream is not one the rest of
        // the system can present: validate() caps the two together, and every
        // decoded envelope is validated. Recording which side of that line a
        // case sits on is the difference between a reachable ceiling and a
        // synthetic upper bound.
        assert_eq!(
            finish.validate().is_ok(),
            valid,
            "{label} is on the wrong side of ControlledCommandFinished::validate"
        );
        let (derived, peak) = fuzz_support::assert_measured(label, bound, || {
            derive_command_diagnostics(&start, &[], &finish)
        });
        check_bounds(&derived).expect("bounded projections");
        // A declaration above the ceiling is clamped to it rather than
        // refused, which is why the two synthetic cases measure the same.
        assert!(
            peak >= expected,
            "{label} allocated only {peak} bytes, so it no longer reaches the capture buffers \
             it exists to exercise"
        );
        println!("FUZZ_PEAK case={label} valid={valid} peak={peak}");
    }

    for count in [MAX_DIAGNOSTICS, MAX_DIAGNOSTICS + 1] {
        let mut stdout = Vec::new();
        for index in 0..count {
            stdout.extend(compiler_message("warning", &format!("warning {index}")).into_bytes());
            stdout.push(b'\n');
        }
        let derived = fuzz_support::assert_bounded(
            &format!("diagnostics-{count}"),
            ORDINARY_ALLOCATION_BYTES,
            || derive(ControlledAction::Check, &stdout, b""),
        );
        check_bounds(&derived).expect("bounded projections");
    }
}

/// A finish record that declares `bytes` of captured output while the chunks
/// carry almost none, so the parser sizes its capture buffer from the claim.
fn declared_capture(bytes: u64) -> CommandCapture {
    CommandCapture {
        bytes,
        completeness: CaptureCompleteness::Complete,
        mode: CommandCaptureMode::Captured,
    }
}

fn action() -> impl Strategy<Value = ControlledAction> {
    prop_oneof![
        Just(ControlledAction::Check),
        Just(ControlledAction::Build),
        Just(ControlledAction::Test),
        Just(ControlledAction::Run),
        Just(ControlledAction::Clippy),
        Just(ControlledAction::Format),
    ]
}

fn valid_messages() -> Vec<Vec<u8>> {
    vec![
        compiler_message("error", "mismatched types"),
        compiler_message("warning", "unused variable"),
        compiler_artifact(),
        serde_json::json!({"reason": "build-finished", "success": false}).to_string(),
        serde_json::json!({"reason": "build-script-executed", "package_id": "student 0.1.0"})
            .to_string(),
        "   not structured output at all".to_owned(),
    ]
    .into_iter()
    .map(String::into_bytes)
    .collect()
}

fn compiler_message(level: &str, message: &str) -> String {
    serde_json::json!({
        "reason": "compiler-message",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": {
            "kind": ["bin"], "crate_types": ["bin"], "name": "student",
            "src_path": "/work/src/main.rs", "edition": "2024"
        },
        "message": {
            "rendered": format!("{level}: {message}\n"), "message": message,
            "code": null, "level": level,
            "spans": [{
                "file_name": "/work/src/main.rs",
                "byte_start": 0, "byte_end": 1,
                "line_start": 1, "line_end": 1,
                "column_start": 1, "column_end": 2,
                "is_primary": true, "text": [], "label": null,
                "suggested_replacement": null, "suggestion_applicability": null,
                "expansion": null
            }],
            "children": []
        }
    })
    .to_string()
}

fn compiler_artifact() -> String {
    serde_json::json!({
        "reason": "compiler-artifact",
        "package_id": "student 0.1.0",
        "manifest_path": "/work/Cargo.toml",
        "target": {
            "kind": ["bin"], "crate_types": ["bin"], "name": "student",
            "src_path": "/work/src/main.rs", "edition": "2024"
        },
        "profile": {
            "opt_level": "0", "debuginfo": "2", "debug_assertions": true,
            "overflow_checks": true, "test": false
        },
        "features": [],
        "filenames": ["/work/target/debug/student"],
        "executable": "/work/target/debug/student",
        "fresh": false
    })
    .to_string()
}

fn derive(action: ControlledAction, stdout: &[u8], stderr: &[u8]) -> CommandDiagnostics {
    let start = started(action);
    let mut output = chunks(&start, OutputStream::Stdout, stdout);
    output.extend(chunks(&start, OutputStream::Stderr, stderr));
    let finish = finished(&start, complete(stdout.len()), complete(stderr.len()));
    derive_command_diagnostics(&start, &output, &finish)
}

fn started(action: ControlledAction) -> ControlledCommandStarted {
    let subcommand = match action {
        ControlledAction::Build => "build",
        ControlledAction::Check => "check",
        ControlledAction::Test => "test",
        ControlledAction::Run => "run",
        ControlledAction::Clippy => "clippy",
        ControlledAction::Format => "fmt",
        ControlledAction::Doc => "doc",
        ControlledAction::Add => "add",
        ControlledAction::Remove => "remove",
        ControlledAction::Update => "update",
    };
    ControlledCommandStarted {
        command_id: CommandId::new("command-fuzz").unwrap(),
        action,
        argv: vec![
            "/rustup".into(),
            "run".into(),
            "1.98.1".into(),
            "/cargo".into(),
            subcommand.into(),
            "--message-format=json".into(),
            "--locked".into(),
        ],
        environment: CommandEnvironment {
            policy_version: 1,
            retained_names: vec![],
        },
        selected_toolchain: "1.98.1".into(),
        tools: vec![
            CommandTool {
                component: CommandToolKind::Rustup,
                executable: "/rustup".into(),
                version: "rustup 1.28.2".into(),
            },
            CommandTool {
                component: CommandToolKind::Rustc,
                executable: "/rustc".into(),
                version: "rustc 1.98.1".into(),
            },
            CommandTool {
                component: CommandToolKind::Cargo,
                executable: "/cargo".into(),
                version: "cargo 1.98.1".into(),
            },
            CommandTool {
                component: CommandToolKind::Rustdoc,
                executable: "/rustdoc".into(),
                version: "rustdoc 1.98.1".into(),
            },
        ],
        before: CommandTreeLink {
            checkpoint_sequence: 40,
            checkpoint_event_hash: hash(1),
            workspace_hash: hash(2),
            workspace_version: 23,
        },
        deadline_millis: 10_000,
        output_limit: 8 * 1024 * 1024,
        console: None,
    }
}

fn finished(
    start: &ControlledCommandStarted,
    stdout: CommandCapture,
    stderr: CommandCapture,
) -> ControlledCommandFinished {
    ControlledCommandFinished {
        command_id: start.command_id.clone(),
        after: CommandTreeLink {
            checkpoint_sequence: 45,
            checkpoint_event_hash: hash(3),
            workspace_hash: start.before.workspace_hash,
            workspace_version: start.before.workspace_version,
        },
        started_millis: 100,
        finished_millis: 200,
        outcome: CommandOutcome::Exited { code: 1 },
        stdout,
        stderr,
    }
}

fn complete(bytes: usize) -> CommandCapture {
    CommandCapture {
        bytes: bytes as u64,
        completeness: CaptureCompleteness::Complete,
        mode: CommandCaptureMode::Captured,
    }
}

fn chunks(
    start: &ControlledCommandStarted,
    stream: OutputStream,
    bytes: &[u8],
) -> Vec<ControlledCommandOutput> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let end = (offset + MAX_COMMAND_CHUNK_BYTES).min(bytes.len());
        result.push(
            ControlledCommandOutput::from_bytes(
                start.command_id.clone(),
                stream,
                offset as u64,
                &bytes[offset..end],
            )
            .unwrap(),
        );
        offset = end;
    }
    result
}

fn hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; Hash::LENGTH])
}
