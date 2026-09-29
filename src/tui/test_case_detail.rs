//! The format 3 part of the F4 test-case picker: what the selected case runs
//! with, shown under the case list. Formats 1 and 2 have no detail and render
//! exactly as before.
//!
//! The detail area has one row per fact, with a label column on the left:
//!
//! ```text
//! Arguments  «-n» «fn main» «tests/grep.md»
//! Input      no input (stdin closed)
//! Runs in    lab2.test-cases/files (3 files)
//!            changed from the package; a run with them will not verify
//! Files      tests/grep.md
//!            … and 2 more
//! Result     FAIL at line 2
//!            expected (12 bytes) "fn main() {}"
//!            got (0 bytes) ""
//! ```
//!
//! When rows are short, the result, the first argument row, where the case
//! runs, the changed warning, its input and the first file row come first;
//! then the rest of the result, up to three argument rows, and every other
//! row lists files.

use unicode_segmentation::UnicodeSegmentation;

use super::shell::{display_width, truncate_to_width};
use crate::display;

/// The width of the label column, including the gap after the longest label.
pub(crate) const LABEL_WIDTH: u16 = 11;
/// Marks the start of each displayed argument.
const ARGUMENT_OPEN: char = '«';
/// Marks the end of each displayed argument.
const ARGUMENT_CLOSE: char = '»';
/// The most rows that arguments or a wrapped message use.
const MAX_WRAPPED_ROWS: usize = 3;

/// What the selected picker row runs with, for a format 3 assignment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestCaseDetail {
    pub selection: TestCaseSelection,
    pub fixtures: TestCaseFixtures,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TestCaseSelection {
    /// One case, with its last result in this session.
    Case {
        arguments: TestCaseArguments,
        input: TestCaseInput,
        result: Option<TestCaseRunResult>,
    },
    /// The Run all row, with the listed cases' last results.
    RunAll {
        cases: usize,
        passed: usize,
        failed: usize,
        errors: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TestCaseArguments {
    /// The arguments from `NAME.args`; empty without that file.
    Listed(Vec<String>),
    /// `NAME.args` cannot be read or parsed, so the case cannot start.
    Invalid(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TestCaseInput {
    /// No `NAME.in`: the program runs with standard input closed.
    Closed,
    /// `NAME.in`, with its size in bytes when it could be read.
    File { name: String, bytes: Option<u64> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TestCaseFixtures {
    /// The package has no fixture files, so programs run in the workspace.
    Workspace { workspace: String },
    /// The fixture folder programs run in, with its files as they are on disk
    /// now, in bytewise path order, and whether they differ from the package.
    Files {
        folder: String,
        files: Vec<String>,
        changed: bool,
    },
    /// The fixture folder is missing, so a run is refused.
    Missing { folder: String },
    /// The fixture folder cannot be read, so a run fails.
    Unreadable { folder: String, reason: String },
}

/// A completed run's result, worded as in the output pane's result line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TestCaseRunResult {
    Pass,
    Fail {
        line: u64,
        expected_len: usize,
        expected_preview: String,
        actual_len: usize,
        actual_preview: String,
    },
    Error(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DetailTone {
    Plain,
    Muted,
    Warning,
    Pass,
    Fail,
}

/// One laid-out detail row: a label (empty on continuation rows) and text
/// that fits the text column.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DetailRow {
    pub label: &'static str,
    pub text: String,
    pub tone: DetailTone,
}

/// One argument as the picker shows it: between `«` and `»`, so leading,
/// trailing and inner spaces are visible. Control characters, invisible and
/// bidirectional formatting characters (such as U+200B and U+202E),
/// whitespace other than the ASCII space (such as U+00A0), zero-width marks,
/// and the two delimiters are written as Rust escapes such as `\u{202e}`.
/// Every other character, including `\`, quotes and shell characters, appears
/// as itself, because the program receives it literally.
pub(crate) fn display_argument(argument: &str) -> String {
    let mut shown = String::with_capacity(argument.len() + 4);
    shown.push(ARGUMENT_OPEN);
    for grapheme in argument.graphemes(true) {
        let invisible = display::grapheme_width(grapheme, 0) == 0;
        if invisible || grapheme.chars().any(argument_escape) {
            for character in grapheme.chars() {
                if argument_escape(character)
                    || display::must_escape(character)
                    || display::grapheme_width(character.encode_utf8(&mut [0; 4]), 0) == 0
                {
                    shown.extend(character.escape_default());
                } else {
                    shown.push(character);
                }
            }
        } else {
            shown.push_str(&display::grapheme(grapheme, 0).0);
        }
    }
    shown.push(ARGUMENT_CLOSE);
    shown
}

fn argument_escape(character: char) -> bool {
    display::must_escape(character)
        || (character.is_whitespace() && character != ' ')
        || matches!(character, ARGUMENT_OPEN | ARGUMENT_CLOSE)
}

/// A byte count for people: bytes below 1 KiB, then KiB or MiB.
pub(crate) fn byte_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    match bytes {
        1 => "1 byte".to_owned(),
        bytes if bytes < KIB => format!("{bytes} bytes"),
        bytes if bytes < KIB * KIB => format!("{:.1} KiB", bytes as f64 / KIB as f64),
        bytes => format!("{:.1} MiB", bytes as f64 / (KIB * KIB) as f64),
    }
}

/// Lays out `detail` in at most `rows` rows of `width` columns, label column
/// included.
pub(crate) fn detail_rows(detail: &TestCaseDetail, width: usize, rows: usize) -> Vec<DetailRow> {
    let text_width = width.saturating_sub(usize::from(LABEL_WIDTH)).max(1);
    let mut sections = Vec::new();
    match &detail.selection {
        TestCaseSelection::Case {
            arguments,
            input,
            result,
        } => {
            sections.push(arguments_section(arguments, text_width));
            let (text, tone) = input_row(input);
            sections.push(Section::fixed(4, "Input", text, tone));
            sections.extend(fixture_sections(&detail.fixtures, text_width, false));
            sections.push(result_section(result.as_ref(), text_width));
        }
        TestCaseSelection::RunAll {
            cases,
            passed,
            failed,
            errors,
        } => {
            sections.extend(fixture_sections(&detail.fixtures, text_width, true));
            let not_run = cases.saturating_sub(passed + failed + errors);
            let plural = if *cases == 1 { "" } else { "s" };
            sections.push(Section::fixed(
                0,
                "Results",
                format!(
                    "{cases} case{plural}: {passed} PASS, {failed} FAIL, {errors} ERROR, {not_run} not run yet"
                ),
                DetailTone::Plain,
            ));
        }
    }

    // One row for each section, most important first; then the extra rows
    // each section can use, in the same order; then every remaining row
    // lists files.
    let mut allotted = vec![0; sections.len()];
    let mut remaining = rows;
    let mut order = (0..sections.len()).collect::<Vec<_>>();
    order.sort_by_key(|&index| sections[index].priority);
    for &index in &order {
        if remaining == 0 {
            break;
        }
        allotted[index] = 1;
        remaining -= 1;
    }
    for fill in [false, true] {
        for &index in &order {
            let section = &sections[index];
            if section.fill != fill || allotted[index] == 0 {
                continue;
            }
            let extra = section.maximum.saturating_sub(1).min(remaining);
            allotted[index] += extra;
            remaining -= extra;
        }
    }

    sections
        .into_iter()
        .zip(allotted)
        .flat_map(|(section, count)| {
            let label = section.label;
            (section.rows)(count)
                .into_iter()
                .take(count)
                .enumerate()
                .map(move |(index, (text, tone))| DetailRow {
                    label: if index == 0 { label } else { "" },
                    text: truncate_to_width(&text, text_width),
                    tone,
                })
        })
        .collect()
}

type Rows = Vec<(String, DetailTone)>;

/// One labelled fact that can use between one and `maximum` rows.
struct Section {
    label: &'static str,
    /// Lower comes first when rows are short.
    priority: u8,
    maximum: usize,
    /// Takes the rows left over after every other section.
    fill: bool,
    /// The section laid out in a given number of rows.
    rows: Box<dyn FnOnce(usize) -> Rows>,
}

impl Section {
    fn fixed(priority: u8, label: &'static str, text: String, tone: DetailTone) -> Self {
        Self {
            label,
            priority,
            maximum: 1,
            fill: false,
            rows: Box::new(move |_| vec![(text, tone)]),
        }
    }

    /// Rows that are simply cut when fewer fit.
    fn lines(priority: u8, label: &'static str, lines: Rows) -> Self {
        Self {
            label,
            priority,
            maximum: lines.len(),
            fill: false,
            rows: Box::new(move |_| lines),
        }
    }

    /// Text wrapped at spaces into at most three rows, ending in `…` when
    /// cut.
    fn wrapped(
        priority: u8,
        label: &'static str,
        text: String,
        tone: DetailTone,
        width: usize,
    ) -> Self {
        let maximum = wrap_words(&text, width, MAX_WRAPPED_ROWS).len();
        Self {
            label,
            priority,
            maximum,
            fill: false,
            rows: Box::new(move |rows| {
                wrap_words(&text, width, rows.min(MAX_WRAPPED_ROWS))
                    .into_iter()
                    .map(|line| (line, tone))
                    .collect()
            }),
        }
    }
}

fn arguments_section(arguments: &TestCaseArguments, width: usize) -> Section {
    match arguments {
        TestCaseArguments::Listed(arguments) if arguments.is_empty() => {
            Section::fixed(1, "Arguments", "none".to_owned(), DetailTone::Plain)
        }
        TestCaseArguments::Listed(arguments) => {
            let shown = arguments
                .iter()
                .map(|argument| display_argument(argument))
                .collect::<Vec<_>>();
            let maximum = pack(&shown, width, usize::MAX, "").len();
            Section {
                label: "Arguments",
                priority: 1,
                maximum: maximum.min(MAX_WRAPPED_ROWS),
                fill: false,
                rows: Box::new(move |rows| {
                    let suffix = if rows < maximum {
                        format!(" … {} arguments", shown.len())
                    } else {
                        String::new()
                    };
                    pack(&shown, width, rows, &suffix)
                        .into_iter()
                        .map(|line| (line, DetailTone::Plain))
                        .collect()
                }),
            }
        }
        TestCaseArguments::Invalid(reason) => Section::wrapped(
            1,
            "Arguments",
            format!("{reason}; the case cannot start"),
            DetailTone::Fail,
            width,
        ),
    }
}

fn input_row(input: &TestCaseInput) -> (String, DetailTone) {
    match input {
        TestCaseInput::Closed => ("no input (stdin closed)".to_owned(), DetailTone::Plain),
        TestCaseInput::File {
            name,
            bytes: Some(bytes),
        } => (format!("{name} ({})", byte_size(*bytes)), DetailTone::Plain),
        TestCaseInput::File { name, bytes: None } => {
            (format!("{name} (cannot be read)"), DetailTone::Warning)
        }
    }
}

fn fixture_sections(fixtures: &TestCaseFixtures, width: usize, run_all: bool) -> Vec<Section> {
    match fixtures {
        TestCaseFixtures::Workspace { workspace } => vec![Section::fixed(
            2,
            "Runs in",
            format!("{workspace} (the package has no fixture files)"),
            DetailTone::Plain,
        )],
        TestCaseFixtures::Missing { folder } => vec![Section::wrapped(
            2,
            "Runs in",
            format!("{folder} is missing; quit and resume the workspace to restore it"),
            DetailTone::Warning,
            width,
        )],
        TestCaseFixtures::Unreadable { folder, reason } => vec![Section::wrapped(
            2,
            "Runs in",
            format!("{folder} cannot be read: {reason}"),
            DetailTone::Warning,
            width,
        )],
        TestCaseFixtures::Files {
            folder,
            files,
            changed,
        } => {
            let plural = if files.len() == 1 { "" } else { "s" };
            let mut sections = vec![Section::fixed(
                2,
                "Runs in",
                format!("{folder} ({} file{plural})", files.len()),
                DetailTone::Plain,
            )];
            if *changed {
                let consequence = if run_all {
                    "changed from the package; runs with them will not verify"
                } else {
                    "changed from the package; a run with them will not verify"
                };
                sections.push(Section::fixed(
                    3,
                    "",
                    consequence.to_owned(),
                    DetailTone::Warning,
                ));
            }
            sections.push(files_section(files.clone(), width));
            sections
        }
    }
}

fn files_section(files: Vec<String>, width: usize) -> Section {
    Section {
        label: "Files",
        priority: 5,
        maximum: files.len().max(1),
        fill: true,
        rows: Box::new(move |rows| {
            if files.is_empty() {
                return vec![("none".to_owned(), DetailTone::Plain)];
            }
            if files.len() <= rows {
                return files
                    .into_iter()
                    .map(|file| (file, DetailTone::Plain))
                    .collect();
            }
            let listed = rows.saturating_sub(1);
            let more = files.len() - listed;
            let mut lines = files
                .iter()
                .take(listed)
                .map(|file| (file.clone(), DetailTone::Plain))
                .collect::<Rows>();
            if listed == 0 {
                let suffix = format!(" … and {} more", more - 1);
                lines.push((with_suffix(&files[0], &suffix, width), DetailTone::Plain));
            } else {
                lines.push((format!("… and {more} more"), DetailTone::Muted));
            }
            lines
        }),
    }
}

fn result_section(result: Option<&TestCaseRunResult>, width: usize) -> Section {
    match result {
        None => Section::fixed(0, "Result", "not run yet".to_owned(), DetailTone::Muted),
        Some(TestCaseRunResult::Pass) => {
            Section::fixed(0, "Result", "PASS".to_owned(), DetailTone::Pass)
        }
        Some(TestCaseRunResult::Fail {
            line,
            expected_len,
            expected_preview,
            actual_len,
            actual_preview,
        }) => Section::lines(
            0,
            "Result",
            vec![
                (format!("FAIL at line {line}"), DetailTone::Fail),
                (
                    format!("expected ({expected_len} bytes) \"{expected_preview}\""),
                    DetailTone::Plain,
                ),
                (
                    format!("got ({actual_len} bytes) \"{actual_preview}\""),
                    DetailTone::Plain,
                ),
            ],
        ),
        Some(TestCaseRunResult::Error(reason)) => Section::wrapped(
            0,
            "Result",
            format!("ERROR ({reason})"),
            DetailTone::Fail,
            width,
        ),
    }
}

/// Packs displayed arguments into at most `rows` lines of `width` columns,
/// one space apart, breaking between arguments unless one is wider than a
/// line. The last line keeps room for `suffix` when it is not empty, and
/// `suffix` ends it.
fn pack(arguments: &[String], width: usize, rows: usize, suffix: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    let line_width = |lines: &Vec<String>| {
        if !suffix.is_empty() && lines.len() + 1 >= rows {
            width.saturating_sub(display_width(suffix)).max(1)
        } else {
            width
        }
    };
    'arguments: for argument in arguments {
        let argument_width = display_width(argument);
        if used > 0 && used + 1 + argument_width <= line_width(&lines) {
            line.push(' ');
            line.push_str(argument);
            used += 1 + argument_width;
            continue;
        }
        if used > 0 {
            if lines.len() + 1 >= rows {
                break;
            }
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        for grapheme in argument.graphemes(true) {
            let grapheme_width = display::grapheme_width(grapheme, used);
            if used > 0 && used + grapheme_width > line_width(&lines) {
                if lines.len() + 1 >= rows {
                    break 'arguments;
                }
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push_str(grapheme);
            used += grapheme_width;
        }
    }
    if used > 0 || !suffix.is_empty() {
        line.push_str(suffix);
        lines.push(line);
    }
    lines
}

/// Wraps `text` at spaces into at most `rows` lines of `width` columns; the
/// last line ends in `…` when text is left over.
fn wrap_words(text: &str, width: usize, rows: usize) -> Vec<String> {
    let text = display::label(text, display::MAX_OUTPUT_BYTES);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let candidate = if line.is_empty() {
            word.to_owned()
        } else {
            format!("{line} {word}")
        };
        if line.is_empty() || display_width(&candidate) <= width {
            line = candidate;
        } else {
            lines.push(std::mem::replace(&mut line, word.to_owned()));
        }
    }
    lines.push(line);
    if lines.len() > rows {
        lines.truncate(rows.max(1));
        if let Some(last) = lines.last_mut() {
            *last = with_suffix(last, " …", width);
        }
    }
    lines
}

/// `text` shortened to leave room for `suffix` within `width` columns.
fn with_suffix(text: &str, suffix: &str, width: usize) -> String {
    let room = width.saturating_sub(display_width(suffix));
    let mut kept = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = display::grapheme_width(grapheme, used);
        if used + grapheme_width > room {
            break;
        }
        kept.push_str(grapheme);
        used += grapheme_width;
    }
    kept.push_str(suffix);
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_show_spaces_between_delimiters_and_escape_invisible_characters() {
        assert_eq!(display_argument("two words"), "«two words»");
        assert_eq!(display_argument(" padded "), "« padded »");
        assert_eq!(
            display_argument("fn\\(x\\) [a-z]* $HOME"),
            "«fn\\(x\\) [a-z]* $HOME»"
        );
        assert_eq!(display_argument("a\u{202e}b"), "«a\\u{202e}b»");
        assert_eq!(display_argument("\u{200b}"), "«\\u{200b}»");
        assert_eq!(
            display_argument("x\u{2066}y\u{2069}"),
            "«x\\u{2066}y\\u{2069}»"
        );
        assert_eq!(display_argument("no\u{a0}break"), "«no\\u{a0}break»");
        assert_eq!(
            display_argument("wide\u{3000}space"),
            "«wide\\u{3000}space»"
        );
        assert_eq!(display_argument("tab\there"), "«tab\\there»");
        assert_eq!(display_argument("esc\u{1b}[2J"), "«esc\\u{1b}[2J»");
        assert_eq!(display_argument("«quoted»"), "«\\u{ab}quoted\\u{bb}»");
        assert_eq!(display_argument("\u{301}"), "«\\u{301}»");
        assert_eq!(display_argument("café"), "«café»");
        assert_eq!(display_argument("e\u{301}"), "«e\u{301}»");
    }

    #[test]
    fn byte_sizes_read_naturally() {
        assert_eq!(byte_size(0), "0 bytes");
        assert_eq!(byte_size(1), "1 byte");
        assert_eq!(byte_size(1023), "1023 bytes");
        assert_eq!(byte_size(1536), "1.5 KiB");
        assert_eq!(byte_size(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn packing_breaks_between_arguments_and_splits_only_overlong_ones() {
        let shown = ["«-n»", "«two words»", "«tests/grep.md»"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(
            pack(&shown, 80, usize::MAX, ""),
            vec!["«-n» «two words» «tests/grep.md»"]
        );
        assert_eq!(
            pack(&shown, 16, usize::MAX, ""),
            vec!["«-n» «two words»", "«tests/grep.md»"]
        );
        assert_eq!(
            pack(&["«abcdefgh»".to_owned()], 4, usize::MAX, ""),
            vec!["«abc", "defg", "h»"]
        );
        assert_eq!(pack(&shown, 16, 1, " …"), vec!["«-n» …"]);
        assert_eq!(
            pack(&shown, 20, 2, " … 3"),
            vec!["«-n» «two words»", "«tests/grep.md» … 3"]
        );
    }

    fn texts(rows: &[DetailRow]) -> Vec<String> {
        rows.iter()
            .map(|row| format!("{:<11}{}", row.label, row.text))
            .collect()
    }

    #[test]
    fn a_case_lists_arguments_input_folder_files_and_result_in_order() {
        let detail = TestCaseDetail {
            selection: TestCaseSelection::Case {
                arguments: TestCaseArguments::Listed(vec!["-i".into(), "two words".into()]),
                input: TestCaseInput::File {
                    name: "echo.in".into(),
                    bytes: Some(6),
                },
                result: Some(TestCaseRunResult::Pass),
            },
            fixtures: TestCaseFixtures::Files {
                folder: "lab.test-cases/files".into(),
                files: vec!["data.txt".into(), "tests/grep.md".into()],
                changed: false,
            },
        };
        assert_eq!(
            texts(&detail_rows(&detail, 74, 10)),
            vec![
                "Arguments  «-i» «two words»",
                "Input      echo.in (6 bytes)",
                "Runs in    lab.test-cases/files (2 files)",
                "Files      data.txt",
                "           tests/grep.md",
                "Result     PASS",
            ]
        );
    }

    #[test]
    fn short_rows_keep_the_result_and_warning_and_truncate_files_and_arguments() {
        let detail = TestCaseDetail {
            selection: TestCaseSelection::Case {
                arguments: TestCaseArguments::Listed(
                    (0..40).map(|index| format!("argument-{index}")).collect(),
                ),
                input: TestCaseInput::Closed,
                result: Some(TestCaseRunResult::Fail {
                    line: 5,
                    expected_len: 3,
                    expected_preview: "abc".into(),
                    actual_len: 3,
                    actual_preview: "abd".into(),
                }),
            },
            fixtures: TestCaseFixtures::Files {
                folder: "lab.test-cases/files".into(),
                files: (0..30)
                    .map(|index| format!("tests/{index:02}.md"))
                    .collect(),
                changed: true,
            },
        };
        let rows = texts(&detail_rows(&detail, 74, 10));
        assert_eq!(
            rows,
            vec![
                "Arguments  «argument-0» «argument-1» «argument-2» «argument-3»",
                "           «argument-4» «argument-5» «argument-6» «argument-7»",
                "           «argument-8» «argument-9» «argument-10» … 40 arguments",
                "Input      no input (stdin closed)",
                "Runs in    lab.test-cases/files (30 files)",
                "           changed from the package; a run with them will not verify",
                "Files      tests/00.md … and 29 more",
                "Result     FAIL at line 5",
                "           expected (3 bytes) \"abc\"",
                "           got (3 bytes) \"abd\"",
            ]
        );
        assert!(rows.iter().all(|row| display_width(row) <= 74), "{rows:#?}");

        assert_eq!(
            texts(&detail_rows(&detail, 74, 4)),
            vec![
                "Arguments  «argument-0» «argument-1» «argument-2» … 40 arguments",
                "Runs in    lab.test-cases/files (30 files)",
                "           changed from the package; a run with them will not verify",
                "Result     FAIL at line 5",
            ]
        );

        let few_arguments = TestCaseDetail {
            selection: TestCaseSelection::Case {
                arguments: TestCaseArguments::Listed(vec!["-c".into()]),
                input: TestCaseInput::Closed,
                result: None,
            },
            ..detail
        };
        let rows = texts(&detail_rows(&few_arguments, 74, 10));
        assert_eq!(rows.len(), 10, "{rows:#?}");
        assert_eq!(rows[4], "Files      tests/00.md");
        assert_eq!(rows[8], "           … and 26 more");
        assert_eq!(rows[9], "Result     not run yet");
    }

    #[test]
    fn run_all_shows_the_folder_and_a_tally() {
        let detail = TestCaseDetail {
            selection: TestCaseSelection::RunAll {
                cases: 4,
                passed: 1,
                failed: 1,
                errors: 1,
            },
            fixtures: TestCaseFixtures::Workspace {
                workspace: "lab.work".into(),
            },
        };
        assert_eq!(
            texts(&detail_rows(&detail, 74, 10)),
            vec![
                "Runs in    lab.work (the package has no fixture files)",
                "Results    4 cases: 1 PASS, 1 FAIL, 1 ERROR, 1 not run yet",
            ]
        );
    }

    #[test]
    fn invalid_arguments_and_errors_wrap_and_missing_fixtures_warn() {
        let detail = TestCaseDetail {
            selection: TestCaseSelection::Case {
                arguments: TestCaseArguments::Invalid("test-case arguments line 2 is empty".into()),
                input: TestCaseInput::File {
                    name: "bad.in".into(),
                    bytes: None,
                },
                result: Some(TestCaseRunResult::Error(
                    "could not start: test-case arguments line 2 is empty".into(),
                )),
            },
            fixtures: TestCaseFixtures::Missing {
                folder: "lab.test-cases/files".into(),
            },
        };
        let rows = detail_rows(&detail, 40, 10);
        assert_eq!(
            texts(&rows),
            vec![
                "Arguments  test-case arguments line 2 is",
                "           empty; the case cannot start",
                "Input      bad.in (cannot be read)",
                "Runs in    lab.test-cases/files is",
                "           missing; quit and resume the",
                "           workspace to restore it",
                "Result     ERROR (could not start:",
                "           test-case arguments line 2 is",
                "           empty)",
            ]
        );
        assert_eq!(rows[0].tone, DetailTone::Fail);
        assert_eq!(rows[2].tone, DetailTone::Warning);
        assert_eq!(rows[3].tone, DetailTone::Warning);
    }
}
