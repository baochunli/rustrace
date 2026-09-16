//! Technical documentation guardrails for assignment package v2.

use std::{fs, path::PathBuf};

fn supported_environment() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("supported-environment.md");
    fs::read_to_string(path)
        .unwrap()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn supported_environment_pins_package_v2_layout_limits_and_deployment() {
    let policy = supported_environment();
    for wording in [
        "`format_version = 2` keeps the version 1 manifest fields",
        "a required, nonempty, flat `test-cases/` tree",
        "complete `NAME.in` and `NAME.expected` pairs",
        "ASCII letters, digits, `-`, or `_`",
        "1,048,576 bytes (1 MiB) per case file",
        "256 cases (512 files)",
        "10,485,760 bytes (10 MiB) across case files",
        "32 MiB source-archive gate",
        "21,037,056 bytes",
        "preflights the fixed `WORKSPACE_PARENT/test-cases/` sibling before publishing a fresh workspace",
        "Resume creates missing managed files with no-replace semantics",
        "accepts exact byte matches, preserves unrelated files",
        "Inspect, doctor scratch extraction, revision validation, and recovery validation never write the sibling",
        "The validated suite hash is computed from package bytes",
    ] {
        assert!(
            policy.contains(wording),
            "supported environment must say: {wording}"
        );
    }
}

#[test]
fn comparison_privacy_and_environment_pin_exact_fields_and_bounds() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs");
    let privacy = fs::read_to_string(root.join("privacy.md")).unwrap();
    let privacy = privacy.split_whitespace().collect::<Vec<_>>().join(" ");
    for wording in [
        "`command_id`, `case`, `expected_blake3`, `actual_blake3`, and `outcome`",
        "`actual_blake3` is `null` when stdout capture is unavailable",
        "Mismatch lines are positive and at most 1,048,577",
        "expected line lengths are at most 1 MiB and actual line lengths at most 8 MiB",
        "Preceding LF bytes plus the expected line length must also fit the 1 MiB expected-file limit",
    ] {
        assert!(privacy.contains(wording), "privacy must say: {wording}");
    }
    let environment = supported_environment();
    assert!(environment.contains("A recorded PASS cannot exceed the 1 MiB expected-file limit"));
    for reason in [
        "launch_failed",
        "nonzero_exit",
        "terminated",
        "capture_truncated",
        "capture_unavailable",
        "capture_read_failed",
        "expected_unreadable",
        "expected_oversized",
    ] {
        let reason = format!("`{reason}`");
        assert!(privacy.contains(&reason), "privacy missing {reason}");
        assert!(
            environment.contains(&reason),
            "environment missing {reason}"
        );
    }
}
