//! Headless, one-package-at-a-time submission batch scanning.

use crate::{
    display,
    process_indicators::{
        INDICATOR_EXPLANATION, ProcessAttempt, display_factual_indicators, display_process_attempt,
        display_process_attempt_details, display_terminal_factual_indicators,
    },
    review_flags::{
        AdvisoryFlagKind, EVIDENCE_LIMITATIONS_BATCH, display_flag, evidence_statement,
        review_flags,
    },
    verify::{
        AssignmentReferenceStatus, SubmittedSourceStatus, TestCaseEvidenceStatus,
        VerificationIssueKind, VerificationReport, VerificationStatus,
        validate_assignment_reference, verify_path,
    },
};
use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};
use unicode_segmentation::UnicodeSegmentation;

const USAGE: &str =
    "Usage: rustrace scan DIRECTORY [--output review.csv] [--reference assignment.rta]";
const PRIORITY_RULE: &str = "High for structural, assignment-reference mismatch, event-chain/\
sequence, checkpoint, replay, or submitted-source mismatch; otherwise Normal; \
factual counts and sizes do not change priority.";
const CSV_PRIORITY_RULE: &str = "High for structural; assignment-reference mismatch; event-chain/\
sequence; checkpoint; replay; or submitted-source mismatch; otherwise Normal; \
factual counts and sizes do not change priority.";
const FILE_WIDTH: usize = 32;
const STUDENT_WIDTH: usize = 32;
const ASSIGNMENT_WIDTH: usize = 32;
const MAX_CSV_REVIEW_FLAGS_BYTES: usize = 256 * 1024;
const MAX_CSV_ADVISORIES_BYTES: usize = 256 * 1024;
const MAX_CSV_INDICATORS_BYTES: usize = 512 * 1024;
const MAX_CSV_PROCESS_RULE_BYTES: usize = 1024 * 1024;
const MAX_PROCESS_OUTCOME_BYTES: usize = 512;
const MAX_TERMINAL_REVIEW_BYTES: usize = 768;
const MAX_TERMINAL_INDICATORS_BYTES: usize = 4 * 1024;
const CSV_COLUMN_TRUNCATED: &str = " [column truncated]";

#[derive(Debug)]
struct ScanOptions {
    directory: PathBuf,
    output: Option<PathBuf>,
    reference: Option<PathBuf>,
}

#[derive(Debug)]
struct ScanRow {
    file_name: String,
    student_id: String,
    assignment_id: String,
    verify_result: &'static str,
    submitted_source_result: &'static str,
    reference_result: &'static str,
    priority: &'static str,
    test_case_runs: String,
    test_case_passes: String,
    test_case_mismatches: String,
    test_case_errors: String,
    test_case_evidence: &'static str,
    first_failing_case: String,
    first_failing_line: String,
    review_flags: String,
    indicators: String,
    process_rule: String,
    explanation: &'static str,
    evidence: &'static str,
    advisories: String,
    terminal_review: String,
    terminal_indicators: Option<String>,
    terminal_process_rules: Vec<String>,
}

pub fn run_scan(args: &[String], terminal: &mut impl Write) -> Result<(), String> {
    let options = parse_options(args)?;
    let entries = fs::read_dir(&options.directory).map_err(|error| {
        display::label_fmt(
            format_args!(
                "could not read scan directory {}: {error}",
                options.directory.display()
            ),
            4096,
        )
    })?;
    let mut csv = options.output.as_deref().map(open_csv).transpose()?;
    let reference = match options.reference.as_deref() {
        Some(reference) => match validate_assignment_reference(reference) {
            Ok(()) => Some(reference),
            Err(error) => {
                writeln!(
                    terminal,
                    "{}",
                    display::label_fmt(
                        format_args!(
                            "Assignment reference unavailable: could not read {}: {error}",
                            reference.display()
                        ),
                        4096,
                    )
                )
                .map_err(io_error)?;
                // Keep the path so each row records the same "reference not
                // evaluated" outcome that `verify` reports for this input.
                Some(reference)
            }
        },
        None => None,
    };

    writeln!(terminal, "Priority rule: {PRIORITY_RULE}").map_err(io_error)?;
    writeln!(
        terminal,
        "Evidence limitations: {EVIDENCE_LIMITATIONS_BATCH}"
    )
    .map_err(io_error)?;
    writeln!(
        terminal,
        "Review indicator context: {INDICATOR_EXPLANATION}"
    )
    .map_err(io_error)?;
    writeln!(
        terminal,
        "{} {} {} {:<6} {:<10} {:<10} {:<8} {:<10} {:<10} {:<10} {:<10} {:<21} {} {:<7} Review flags / review indicators",
        column("File", FILE_WIDTH),
        column("Student", STUDENT_WIDTH),
        column("Assignment", ASSIGNMENT_WIDTH),
        "Verify",
        "Source",
        "Reference",
        "Priority",
        "Runs",
        "Passes",
        "Mismatches",
        "Errors",
        "Case evidence",
        column("First case", 32),
        "Line"
    )
    .map_err(io_error)?;
    writeln!(
        terminal,
        "{} {} {} {:-<6} {:-<10} {:-<10} {:-<8} {:-<10} {:-<10} {:-<10} {:-<10} {:-<21} {:-<32} {:-<7} {:-<32}",
        "-".repeat(FILE_WIDTH),
        "-".repeat(STUDENT_WIDTH),
        "-".repeat(ASSIGNMENT_WIDTH),
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        ""
    )
    .map_err(io_error)?;

    if let Some(csv) = &mut csv {
        write!(
            csv,
            "# Priority rule: {CSV_PRIORITY_RULE} Evidence limitations: {EVIDENCE_LIMITATIONS_BATCH}\r\n"
        )
        .map_err(io_error)?;
        write_csv_record(
            csv,
            &[
                "file_name",
                "student_id",
                "assignment_id",
                "verify_result",
                "submitted_source_result",
                "reference_result",
                "priority",
                "test_case_runs",
                "test_case_passes",
                "test_case_mismatches",
                "test_case_errors",
                "test_case_evidence",
                "first_failing_case",
                "first_failing_line",
                "review_flags",
                "indicators",
                "process_rule",
                "explanation",
                "evidence",
                "advisories",
            ],
        )?;
    }

    for entry in entries {
        let entry = entry.map_err(|error| {
            display::label_fmt(
                format_args!("could not enumerate scan directory: {error}"),
                4096,
            )
        })?;
        if !is_candidate(&entry.file_name()) {
            continue;
        }
        let file_name =
            display::label_fmt(format_args!("{}", entry.file_name().to_string_lossy()), 512);
        let row = scan_entry(&entry.path(), file_name, reference);
        write_terminal_row(terminal, &row)?;
        if let Some(csv) = &mut csv {
            write_csv_row(csv, &row)?;
        }
    }
    if let Some(csv) = &mut csv {
        csv.flush().map_err(io_error)?;
    }
    Ok(())
}

fn parse_options(args: &[String]) -> Result<ScanOptions, String> {
    let Some(directory) = args.first() else {
        return Err(USAGE.to_owned());
    };
    if directory.starts_with("--") {
        return Err(USAGE.to_owned());
    }
    let mut output = None;
    let mut reference = None;
    let mut index = 1;
    while index < args.len() {
        let target = match args[index].as_str() {
            "--output" if output.is_none() => &mut output,
            "--reference" if reference.is_none() => &mut reference,
            _ => return Err(USAGE.to_owned()),
        };
        index += 1;
        let Some(value) = args.get(index) else {
            return Err(USAGE.to_owned());
        };
        *target = Some(PathBuf::from(value));
        index += 1;
    }
    Ok(ScanOptions {
        directory: PathBuf::from(directory),
        output,
        reference,
    })
}

fn open_csv(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|error| {
            display::label_fmt(
                format_args!("could not create CSV output {}: {error}", path.display()),
                4096,
            )
        })
}

fn is_candidate(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("zip") || extension.eq_ignore_ascii_case("rprov")
        })
}

fn scan_entry(path: &Path, file_name: String, reference: Option<&Path>) -> ScanRow {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            ScanRow::invalid(file_name, "symlink not followed")
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            ScanRow::invalid(file_name, "not a regular file")
        }
        Ok(_) => ScanRow::from_report(file_name, &verify_path(path, reference)),
        Err(error) => ScanRow::invalid(
            file_name,
            &display::label_fmt(format_args!("could not inspect candidate: {error}"), 512),
        ),
    }
}

impl ScanRow {
    fn invalid(file_name: String, reason: &str) -> Self {
        Self::from_report(file_name, &VerificationReport::input_failure(reason))
    }

    fn from_report(file_name: String, report: &VerificationReport) -> Self {
        let validation_ok = report.package_structure == VerificationStatus::Ok
            && report.event_chain == VerificationStatus::Ok
            && report.checkpoint_hashes == VerificationStatus::Ok
            && report.replay == VerificationStatus::Ok;
        let structural_failure = report.package_structure != VerificationStatus::Ok;
        let reference_failure = report.assignment_reference == AssignmentReferenceStatus::Mismatch;
        let chain_failure = report.event_chain == VerificationStatus::Failed;
        let checkpoint_failure = report.checkpoint_hashes == VerificationStatus::Failed;
        let replay_failure = report.replay == VerificationStatus::Failed;
        let source_failure = report.submitted_source_match == SubmittedSourceStatus::SourceMismatch;
        let high = structural_failure
            || reference_failure
            || chain_failure
            || checkpoint_failure
            || replay_failure
            || source_failure;

        let mut flags = review_flags(report)
            .iter()
            .map(display_flag)
            .collect::<Vec<_>>();
        if reference_failure {
            let detail = report
                .issues
                .iter()
                .find(|issue| issue.kind == VerificationIssueKind::AssignmentReference)
                .map_or("assignment identity differs", |issue| issue.detail.as_str());
            flags.push(display::label_fmt(
                format_args!(
                    "assignment-reference mismatch: {}",
                    display::label(detail, 256)
                ),
                512,
            ));
        }
        let review_flags = bounded_join(&flags, MAX_CSV_REVIEW_FLAGS_BYTES);
        let advisories = bounded_join(
            &AdvisoryFlagKind::ALL
                .into_iter()
                .filter_map(|kind| {
                    let count = report
                        .advisories
                        .iter()
                        .filter(|advisory| advisory.kind == kind)
                        .count();
                    (count > 0).then(|| format!("{}: {count}", kind.name()))
                })
                .collect::<Vec<_>>(),
            MAX_CSV_ADVISORIES_BYTES,
        );
        let (indicator_details, process_rule, terminal_indicators, terminal_process_rules) =
            if let Some(indicators) = &report.review_indicators {
                (
                    bounded_join(
                        &display_factual_indicators(&indicators.factual),
                        MAX_CSV_INDICATORS_BYTES,
                    ),
                    bounded_process_rules(&indicators.attempts),
                    Some(display::label(
                        &display_terminal_factual_indicators(&indicators.factual),
                        MAX_TERMINAL_INDICATORS_BYTES,
                    )),
                    indicators
                        .attempts
                        .iter()
                        .map(|attempt| {
                            format!(
                                "{} attempt {}: {}",
                                attempt.rule_version,
                                attempt.attempt,
                                display::label(
                                    &display_process_attempt(attempt),
                                    MAX_PROCESS_OUTCOME_BYTES,
                                )
                            )
                        })
                        .collect(),
                )
            } else {
                (
                "review indicators unavailable: factual counts unknown because the validated event stream is unavailable"
                    .to_owned(),
                "review indicators unavailable: process rule not evaluated because the validated event stream is unavailable"
                    .to_owned(),
                None,
                Vec::new(),
            )
            };
        let terminal_review = if flags.is_empty() {
            if report.review_indicators.is_some() {
                "No review flags; see Indicators below".to_owned()
            } else {
                "No review flags; review indicators unavailable".to_owned()
            }
        } else {
            display::label(&flags.join("; "), MAX_TERMINAL_REVIEW_BYTES)
        };
        let test_case_evidence = match report.test_case_evidence {
            Some(TestCaseEvidenceStatus::Recorded) => "recorded (unverified)",
            Some(TestCaseEvidenceStatus::ReferenceVerified) => "reference-verified",
            None => "unavailable",
        };
        let first_failing_case = report.first_failing_case.as_ref().map_or_else(
            || {
                if report.test_case_runs.is_some() {
                    String::new()
                } else {
                    "unknown".to_owned()
                }
            },
            |failure| display::label(&failure.case, 64),
        );
        let first_failing_line = report.first_failing_case.as_ref().map_or_else(
            || {
                if report.test_case_runs.is_some() {
                    String::new()
                } else {
                    "unknown".to_owned()
                }
            },
            |failure| {
                failure
                    .line
                    .map_or_else(String::new, |line| line.to_string())
            },
        );

        Self {
            file_name,
            student_id: safe_optional(report.student_id.as_deref()),
            assignment_id: safe_optional(report.assignment_id.as_deref()),
            verify_result: if validation_ok { "OK" } else { "Fail" },
            submitted_source_result: match report.submitted_source_match {
                SubmittedSourceStatus::Ok => "Match",
                SubmittedSourceStatus::SourceMismatch => "No",
                SubmittedSourceStatus::Unavailable => "Unverified",
            },
            reference_result: match report.assignment_reference {
                AssignmentReferenceStatus::Ok => "OK",
                AssignmentReferenceStatus::Mismatch => "Mismatch",
                AssignmentReferenceStatus::Unverified => "Unverified",
            },
            priority: if high { "High" } else { "Normal" },
            test_case_runs: scan_count(report.test_case_runs),
            test_case_passes: scan_count(report.test_case_passes),
            test_case_mismatches: scan_count(report.test_case_mismatches),
            test_case_errors: scan_count(report.test_case_errors),
            test_case_evidence,
            first_failing_case,
            first_failing_line,
            review_flags,
            indicators: indicator_details,
            process_rule,
            explanation: INDICATOR_EXPLANATION,
            evidence: evidence_statement(report),
            advisories,
            terminal_review,
            terminal_indicators,
            terminal_process_rules,
        }
    }
}

fn scan_count(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |count| count.to_string())
}

fn bounded_join(values: &[String], maximum_bytes: usize) -> String {
    bound_csv_column(values.join("; "), maximum_bytes)
}

fn bound_csv_column(mut value: String, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value;
    }
    let mut retained_bytes = maximum_bytes
        .saturating_sub(CSV_COLUMN_TRUNCATED.len())
        .min(value.len());
    while !value.is_char_boundary(retained_bytes) {
        retained_bytes -= 1;
    }
    value.truncate(retained_bytes);
    value.push_str(&CSV_COLUMN_TRUNCATED[..CSV_COLUMN_TRUNCATED.len().min(maximum_bytes)]);
    value
}

fn bounded_process_rules(attempts: &[ProcessAttempt]) -> String {
    let details = attempts
        .iter()
        .map(display_process_attempt_details)
        .collect::<Vec<_>>()
        .join("; ");
    if details.len() <= MAX_CSV_PROCESS_RULE_BYTES {
        return details;
    }

    let outcomes = attempts
        .iter()
        .map(|attempt| {
            format!(
                "{} attempt {}: {}",
                attempt.rule_version,
                attempt.attempt,
                display::label(&display_process_attempt(attempt), MAX_PROCESS_OUTCOME_BYTES)
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let retained =
        format!("[process-rule details truncated; all attempt outcomes retained] {outcomes}");
    debug_assert!(retained.len() <= MAX_CSV_PROCESS_RULE_BYTES);
    retained
}

fn safe_optional(value: Option<&str>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| display::label(value, 512))
}

fn write_terminal_row(output: &mut impl Write, row: &ScanRow) -> Result<(), String> {
    writeln!(
        output,
        "{} {} {} {:<6} {:<10} {:<10} {:<8} {:<10} {:<10} {:<10} {:<10} {:<21} {} {:<7} {}",
        column(&row.file_name, FILE_WIDTH),
        column(&row.student_id, STUDENT_WIDTH),
        column(&row.assignment_id, ASSIGNMENT_WIDTH),
        row.verify_result,
        row.submitted_source_result,
        row.reference_result,
        row.priority,
        row.test_case_runs,
        row.test_case_passes,
        row.test_case_mismatches,
        row.test_case_errors,
        row.test_case_evidence,
        column(
            if row.first_failing_case.is_empty() {
                "none"
            } else {
                &row.first_failing_case
            },
            32
        ),
        if row.first_failing_line.is_empty() {
            "-"
        } else {
            &row.first_failing_line
        },
        row.terminal_review
    )
    .map_err(io_error)?;
    let first = if row.first_failing_case.is_empty() {
        "none".to_owned()
    } else if row.first_failing_line.is_empty() || row.first_failing_line == "unknown" {
        row.first_failing_case.clone()
    } else {
        format!("{} line={}", row.first_failing_case, row.first_failing_line)
    };
    writeln!(
        output,
        "  Test cases: runs={} passes={} mismatches={} errors={} evidence={} first={}",
        row.test_case_runs,
        row.test_case_passes,
        row.test_case_mismatches,
        row.test_case_errors,
        row.test_case_evidence,
        first,
    )
    .map_err(io_error)?;
    if let Some(indicators) = &row.terminal_indicators {
        writeln!(output, "  Indicators: {indicators}").map_err(io_error)?;
    }
    for process_rule in &row.terminal_process_rules {
        writeln!(output, "  Process rule: {process_rule}").map_err(io_error)?;
    }
    if !row.advisories.is_empty() {
        writeln!(output, "  Advisories: {}", row.advisories).map_err(io_error)?;
    }
    writeln!(output, "  Evidence: {}", row.evidence).map_err(io_error)
}

fn column(value: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    let mut clipped = false;
    for cluster in value.graphemes(true) {
        let cluster_width = display::grapheme_width(cluster, used);
        if used + cluster_width > width {
            clipped = true;
            break;
        }
        result.push_str(cluster);
        used += cluster_width;
    }
    if clipped {
        while used >= width {
            let Some((index, cluster)) = result.grapheme_indices(true).next_back() else {
                break;
            };
            let cluster_width = display::grapheme_width(cluster, used);
            result.truncate(index);
            used = used.saturating_sub(cluster_width);
        }
        result.push('…');
        used += 1;
    }
    result.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    result
}

fn write_csv_row(output: &mut impl Write, row: &ScanRow) -> Result<(), String> {
    write_csv_record(
        output,
        &[
            &row.file_name,
            &row.student_id,
            &row.assignment_id,
            row.verify_result,
            row.submitted_source_result,
            row.reference_result,
            row.priority,
            &row.test_case_runs,
            &row.test_case_passes,
            &row.test_case_mismatches,
            &row.test_case_errors,
            row.test_case_evidence,
            &row.first_failing_case,
            &row.first_failing_line,
            &row.review_flags,
            &row.indicators,
            &row.process_rule,
            row.explanation,
            row.evidence,
            &row.advisories,
        ],
    )
}

fn write_csv_record(output: &mut impl Write, values: &[&str]) -> Result<(), String> {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output.write_all(b",").map_err(io_error)?;
        }
        write_csv_field(output, value)?;
    }
    output.write_all(b"\r\n").map_err(io_error)
}

fn write_csv_field(output: &mut impl Write, value: &str) -> Result<(), String> {
    if value
        .as_bytes()
        .iter()
        .any(|byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'))
    {
        output.write_all(b"\"").map_err(io_error)?;
        for (index, part) in value.split('"').enumerate() {
            if index > 0 {
                output.write_all(b"\"\"").map_err(io_error)?;
            }
            output.write_all(part.as_bytes()).map_err(io_error)?;
        }
        output.write_all(b"\"").map_err(io_error)
    } else {
        output.write_all(value.as_bytes()).map_err(io_error)
    }
}

fn io_error(error: io::Error) -> String {
    display::label_fmt(format_args!("output failed: {error}"), 4096)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        review_flags::{AdvisoryFlag, AdvisoryFlagKind},
        verify::VerificationEventLocation,
    };

    #[test]
    fn csv_columns_use_their_own_utf8_safe_marked_budget() {
        let above_terminal_budget = "x".repeat(display::MAX_OUTPUT_BYTES + 1);
        assert_eq!(
            bound_csv_column(above_terminal_budget.clone(), MAX_CSV_REVIEW_FLAGS_BYTES),
            above_terminal_budget
        );

        let bounded = bound_csv_column("é".repeat(64), 64);
        assert!(bounded.len() <= 64);
        assert!(bounded.ends_with(CSV_COLUMN_TRUNCATED));
    }

    #[test]
    fn advisory_csv_field_is_appended_and_leaves_every_existing_field_byte_identical() {
        let without_advisory = VerificationReport::input_failure("fixture");
        let mut with_advisory = without_advisory.clone();
        with_advisory.advisories.push(AdvisoryFlag {
            kind: AdvisoryFlagKind::LargeSingleInsertion,
            link: VerificationEventLocation {
                segment: 1,
                sequence: 7,
            },
            measured_value: "200 bytes".to_owned(),
        });
        let mut baseline = Vec::new();
        write_csv_row(
            &mut baseline,
            &ScanRow::from_report("fixture.zip".to_owned(), &without_advisory),
        )
        .unwrap();
        let mut advisory = Vec::new();
        write_csv_row(
            &mut advisory,
            &ScanRow::from_report("fixture.zip".to_owned(), &with_advisory),
        )
        .unwrap();
        let baseline = String::from_utf8(baseline).unwrap();
        let advisory = String::from_utf8(advisory).unwrap();
        let baseline_existing = baseline.rsplit_once(',').unwrap().0;
        let advisory_existing = advisory.rsplit_once(',').unwrap().0;
        assert_eq!(advisory_existing, baseline_existing);
        assert!(baseline.ends_with(",\r\n"), "{baseline:?}");
        assert!(
            advisory.ends_with(",LARGE_SINGLE_INSERTION: 1\r\n"),
            "{advisory:?}"
        );
    }
}
