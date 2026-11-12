//! Source discovery for conventional Cargo integration tests. Opening F4 never builds code.

use crate::console::{TestCase, TestCaseComparison, TestCaseMismatch, TestCaseOutcome};
use rustrace_model::{CaptureCompleteness, CommandOutcome};
use std::{error::Error, path::Path};

pub(crate) fn discover(root: &Path) -> Result<Vec<TestCase>, Box<dyn Error>> {
    match std::fs::symlink_metadata(root.join("tests")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            return Err("Cargo tests must be a regular workspace directory".into());
        }
        Ok(_) => {}
    }
    let files = rustrace_workspace::hash::read_workspace(root)?;
    let mut cases = Vec::new();
    for (path, source) in files {
        let Some(target) = path
            .as_str()
            .strip_prefix("tests/")
            .and_then(|path| path.strip_suffix(".rs"))
        else {
            continue;
        };
        if target.contains('/') || !rustrace_model::is_valid_test_case_name(target) {
            continue;
        }
        cases.extend(discover_source(target, &source)?);
        if cases.len() > rustrace_workspace::assignment_package::MAX_TEST_CASES {
            return Err("too many Cargo integration tests".into());
        }
    }
    Ok(cases)
}

fn discover_source(target: &str, source: &[u8]) -> Result<Vec<TestCase>, Box<dyn Error>> {
    fn visit(
        node: tree_sitter::Node<'_>,
        modules: &str,
        target: &str,
        source: &[u8],
        cases: &mut Vec<TestCase>,
    ) {
        let mut test = false;
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "attribute_item" => {
                    if child.utf8_text(source).ok().is_some_and(|text| {
                        text.chars()
                            .filter(|c| !c.is_whitespace())
                            .collect::<String>()
                            == "#[test]"
                    }) {
                        test = true;
                    }
                    continue;
                }
                "line_comment" | "block_comment" => continue,
                "function_item" if test => {
                    if let Some(name) = child
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(source).ok())
                    {
                        let filter = format!("{modules}{name}");
                        if rustrace_model::is_valid_console_test_tail(&[
                            "--test", target, &filter, "--", "--exact",
                        ]) {
                            cases.push(TestCase::cargo(target, &filter, source));
                        }
                    }
                }
                "mod_item" => {
                    if let (Some(name), Some(body)) = (
                        child.child_by_field_name("name"),
                        child.child_by_field_name("body"),
                    ) {
                        if let Ok(name) = name.utf8_text(source) {
                            visit(body, &format!("{modules}{name}::"), target, source, cases);
                        }
                    }
                }
                _ => {}
            }
            test = false;
        }
    }
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let tree = parser
        .parse(source, None)
        .ok_or("could not parse Cargo tests")?;
    let mut cases = Vec::new();
    visit(tree.root_node(), "", target, source, &mut cases);
    Ok(cases)
}

pub(crate) fn classify(
    case: TestCase,
    process: &CommandOutcome,
    completeness: CaptureCompleteness,
    stdout: &[u8],
) -> TestCaseComparison {
    // libtest's summary establishes that the exact selection actually ran.
    // cfg-disabled, ignored, stale and missing selections must never pass.
    let output = String::from_utf8_lossy(stdout);
    let passed = output
        .lines()
        .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"));
    let failed = output
        .lines()
        .any(|line| line.starts_with("test result: FAILED. 0 passed; 1 failed; 0 ignored;"));
    let outcome = match (process, completeness) {
        (CommandOutcome::Exited { code: 0 }, CaptureCompleteness::Complete) if passed => {
            TestCaseOutcome::Pass
        }
        (CommandOutcome::Exited { code: 101 }, CaptureCompleteness::Complete) if failed => {
            TestCaseOutcome::Fail(TestCaseMismatch {
                line: 1,
                expected_len: 0,
                actual_len: 0,
                expected_preview: "test passes".into(),
                actual_preview: "test failed; see Cargo output".into(),
            })
        }
        _ => TestCaseOutcome::Error(match process {
            CommandOutcome::Exited { code: 0 } if completeness == CaptureCompleteness::Complete => {
                "Cargo did not run exactly one non-ignored test".into()
            }
            CommandOutcome::Exited { code } => {
                format!("Cargo exit {code}; see output (capture: {completeness:?})")
            }
            CommandOutcome::LaunchFailed { .. } => "launch failed".into(),
            CommandOutcome::Terminated { reason, .. } => format!("{reason:?}"),
        }),
    };
    TestCaseComparison {
        case,
        outcome,
        expected_blake3: None,
        actual_blake3: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_real_test_attributes_and_nested_modules() {
        let source = br##"// #[test] fn fake() {}
const TEXT: &str = "#[test] fn also_fake() {}";
#[test]
fn plain() {}
mod nested {
    #[test]
    #[should_panic]
    fn fails() {}
    fn helper() {}
}
"##;
        let cases = discover_source("public_cli", source).unwrap();
        assert_eq!(
            cases.iter().map(|case| case.name()).collect::<Vec<_>>(),
            ["cargo:public_cli::plain", "cargo:public_cli::nested::fails"]
        );
        assert_eq!(cases[1].cargo_test(), Some(("public_cli", "nested::fails")));
    }

    #[test]
    fn cargo_failure_classification_distinguishes_assertions_and_build_errors() {
        let case = discover_source("cli", b"#[test] fn works() {}")
            .unwrap()
            .remove(0);
        let failed = b"test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        assert!(matches!(
            classify(
                case.clone(),
                &CommandOutcome::Exited { code: 101 },
                CaptureCompleteness::Complete,
                failed
            )
            .outcome,
            TestCaseOutcome::Fail(_)
        ));
        for (outcome, output) in [
            (CommandOutcome::Exited { code: 101 }, &b""[..]),
            (CommandOutcome::Exited { code: 0 }, &b"test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n"[..]),
            (CommandOutcome::Terminated { reason: rustrace_model::CommandTermination::Cancelled, signal: None }, failed.as_slice()),
        ] {
            assert!(matches!(classify(case.clone(), &outcome, CaptureCompleteness::Complete, output).outcome, TestCaseOutcome::Error(_)));
        }
    }

    #[test]
    fn cargo_results_require_exactly_one_test_and_complete_capture() {
        let case = discover_source("cli", b"#[test] fn works() {}")
            .unwrap()
            .remove(0);
        let pass = b"running 1 test\ntest works ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s\n";
        assert!(matches!(
            classify(
                case.clone(),
                &rustrace_model::CommandOutcome::Exited { code: 0 },
                rustrace_model::CaptureCompleteness::Complete,
                pass
            )
            .outcome,
            crate::console::TestCaseOutcome::Pass
        ));
        for output in [&b"test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n"[..], &b""[..]] {
            assert!(matches!(classify(case.clone(), &rustrace_model::CommandOutcome::Exited { code: 0 }, rustrace_model::CaptureCompleteness::Complete, output).outcome, crate::console::TestCaseOutcome::Error(_)));
        }
        assert!(matches!(
            classify(
                case,
                &rustrace_model::CommandOutcome::Exited { code: 0 },
                rustrace_model::CaptureCompleteness::Truncated,
                pass
            )
            .outcome,
            crate::console::TestCaseOutcome::Error(_)
        ));
    }
}
