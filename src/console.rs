//! Small local state for the embedded piped Cargo console.

use rustrace_model::{Hash, WorkspacePath};
use rustrace_workspace::hash::PinnedWorkspaceRoot;
use rustrace_workspace::{
    OpenedRegularFile,
    assignment_package::{MAX_TEST_CASE_FILE_BYTES, MAX_TEST_CASE_TOTAL_BYTES, MAX_TEST_CASES},
    create_external_regular_file_in, external_regular_file_exists_in,
    list_external_regular_files_in_with_filter, open_external_regular_file_read_in,
    open_external_regular_file_write_in,
};
use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    io::Read,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
use unicode_segmentation::UnicodeSegmentation;

pub(crate) const MAX_CONSOLE_LINE_BYTES: usize = 4096;
pub(crate) const MAX_TEST_CASE_LINE_PREVIEW_INPUT_BYTES: usize = 256;
pub(crate) const MAX_TEST_CASE_LINE_PREVIEW_OUTPUT_BYTES: usize = 128;

const _: () =
    assert!(rustrace_model::MAX_TEST_CASE_EXPECTED_LINE_BYTES == MAX_TEST_CASE_FILE_BYTES);

type Result<T> = std::result::Result<T, Box<dyn Error>>;

static TEST_CASE_REFRESH_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutputDisposition {
    CreateNew,
    Overwrite,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TestCase {
    name: String,
    pair_blake3: Option<Hash>,
}

impl TestCase {
    pub(crate) fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.is_empty()
            || name.len() > rustrace_model::MAX_TEST_CASE_NAME_BYTES
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("invalid packaged test-case name".into());
        }
        Ok(Self {
            name,
            pair_blake3: None,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn input_path(&self) -> WorkspacePath {
        WorkspacePath::new(format!("{}.in", self.name)).expect("validated test-case path")
    }

    pub(crate) fn expected_path(&self) -> WorkspacePath {
        WorkspacePath::new(format!("{}.expected", self.name)).expect("validated test-case path")
    }
}

fn refresh_identity(refresh_id: u64, name: &str) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rustrace.live-test-case-refresh.v1");
    hasher.update(&refresh_id.to_le_bytes());
    hasher.update(&(name.len() as u64).to_le_bytes());
    hasher.update(name.as_bytes());
    Hash::from_bytes(*hasher.finalize().as_bytes())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestCaseComparison {
    pub case: TestCase,
    pub outcome: TestCaseOutcome,
    pub expected_blake3: Option<Hash>,
    pub actual_blake3: Option<Hash>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TestCaseOutcome {
    Pass,
    Fail(TestCaseMismatch),
    Error(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestCaseMismatch {
    pub line: u64,
    pub expected_len: usize,
    pub actual_len: usize,
    pub expected_preview: String,
    pub actual_preview: String,
}

/// Authority for the one fixed sibling `test-cases/` directory.
#[derive(Debug)]
pub(crate) struct TestCaseDirectory {
    root: PinnedWorkspaceRoot,
}

impl TestCaseDirectory {
    pub(crate) fn open(workspace_root: &Path) -> Result<Self> {
        let workspace = PinnedWorkspaceRoot::open(workspace_root)?;
        let parent = workspace
            .path()
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or("workspace has no parent for sibling test-cases")?;
        let expected = parent.join("test-cases");
        let metadata = fs::symlink_metadata(&expected)?;
        if metadata.file_type().is_symlink() {
            return Err("the fixed sibling test-cases root must not be a symlink".into());
        }
        let root = PinnedWorkspaceRoot::open(&expected)?;
        if root.path() != expected {
            return Err("the fixed sibling test-cases root is not canonical".into());
        }
        root.verify_binding()?;
        workspace.verify_binding()?;
        if root.is_same_directory(&workspace) {
            return Err("the fixed sibling test-cases root aliases the selected workspace".into());
        }
        Ok(Self { root })
    }

    pub(crate) fn list(&self) -> Result<Vec<WorkspacePath>> {
        Ok(list_external_regular_files_in_with_filter(
            &self.root,
            MAX_TEST_CASES * 2,
            is_test_case_candidate,
        )?)
    }

    pub(crate) fn list_cases(&self) -> Result<Vec<TestCase>> {
        #[derive(Default)]
        struct Pair {
            input: bool,
            expected: bool,
        }

        let mut pairs = BTreeMap::<String, Pair>::new();
        for path in self.list()? {
            let value = path.as_str();
            if value.contains('/') {
                continue;
            }
            let (name, input) = if let Some(name) = value.strip_suffix(".in") {
                (name, true)
            } else if let Some(name) = value.strip_suffix(".expected") {
                (name, false)
            } else {
                continue;
            };
            let Ok(case) = TestCase::new(name) else {
                continue;
            };
            let pair = pairs.entry(case.name).or_default();
            if input {
                pair.input = true;
            } else {
                pair.expected = true;
            }
        }
        let refresh_id = TEST_CASE_REFRESH_ID.fetch_add(1, Ordering::Relaxed);
        let mut cases = pairs
            .into_iter()
            .filter_map(|(name, pair)| {
                (pair.input && pair.expected).then(|| TestCase {
                    pair_blake3: Some(refresh_identity(refresh_id, &name)),
                    name,
                })
            })
            .collect::<Vec<_>>();
        let mut remaining_identity_bytes = MAX_TEST_CASE_TOTAL_BYTES;
        for case in &mut cases {
            if remaining_identity_bytes == 0 {
                break;
            }
            let Ok(identity) = self.pair_identity(case, &mut remaining_identity_bytes) else {
                continue;
            };
            case.pair_blake3 = Some(identity);
        }
        Ok(cases)
    }

    pub(crate) fn open_input(&self, path: &WorkspacePath) -> Result<OpenedRegularFile> {
        Ok(open_external_regular_file_read_in(&self.root, path)?)
    }

    pub(crate) fn read_expected(&self, case: &TestCase) -> Result<Vec<u8>> {
        self.read_bounded_case_file(
            &case.expected_path(),
            "expected output exceeds the 1048576-byte limit",
        )
    }

    fn pair_identity(&self, case: &TestCase, remaining_bytes: &mut u64) -> Result<Hash> {
        let input = self.read_case_file_for_identity(
            &case.input_path(),
            "test input exceeds the 1048576-byte limit",
            remaining_bytes,
        )?;
        let expected = self.read_case_file_for_identity(
            &case.expected_path(),
            "expected output exceeds the 1048576-byte limit",
            remaining_bytes,
        )?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"rustrace.live-test-case-pair.v1");
        hasher.update(&(input.len() as u64).to_le_bytes());
        hasher.update(&input);
        hasher.update(&(expected.len() as u64).to_le_bytes());
        hasher.update(&expected);
        Ok(Hash::from_bytes(*hasher.finalize().as_bytes()))
    }

    fn read_case_file_for_identity(
        &self,
        path: &WorkspacePath,
        oversized: &str,
        remaining_bytes: &mut u64,
    ) -> Result<Vec<u8>> {
        let allowance = *remaining_bytes;
        if allowance == 0 {
            return Err("packaged test-case identity limit reached".into());
        }
        let read_limit = (MAX_TEST_CASE_FILE_BYTES + 1).min(allowance.saturating_add(1));
        let mut opened = open_external_regular_file_read_in(&self.root, path)?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(read_limit)
                .unwrap_or(usize::MAX)
                .min(64 * 1024),
        );
        let read_result = opened.file_mut().take(read_limit).read_to_end(&mut bytes);
        let bytes_read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        *remaining_bytes = remaining_bytes.saturating_sub(bytes_read.min(allowance));
        read_result?;
        if bytes_read > allowance {
            return Err("packaged test-case identity limit reached".into());
        }
        if bytes_read > MAX_TEST_CASE_FILE_BYTES {
            return Err(oversized.into());
        }
        self.root.verify_binding()?;
        Ok(bytes)
    }

    fn read_bounded_case_file(&self, path: &WorkspacePath, oversized: &str) -> Result<Vec<u8>> {
        let mut opened = open_external_regular_file_read_in(&self.root, path)?;
        let maximum = usize::try_from(MAX_TEST_CASE_FILE_BYTES)
            .expect("packaged test-case file limit fits usize");
        let mut bytes = Vec::with_capacity(maximum.min(64 * 1024));
        opened
            .file_mut()
            .take(MAX_TEST_CASE_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > maximum {
            return Err(oversized.into());
        }
        self.root.verify_binding()?;
        Ok(bytes)
    }

    pub(crate) fn output_disposition(&self, path: &WorkspacePath) -> Result<OutputDisposition> {
        Ok(if external_regular_file_exists_in(&self.root, path)? {
            OutputDisposition::Overwrite
        } else {
            OutputDisposition::CreateNew
        })
    }

    pub(crate) fn open_output(
        &self,
        path: &WorkspacePath,
        disposition: OutputDisposition,
    ) -> Result<OpenedRegularFile> {
        let opened = match disposition {
            OutputDisposition::CreateNew => create_external_regular_file_in(&self.root, path),
            OutputDisposition::Overwrite => open_external_regular_file_write_in(&self.root, path),
        };
        Ok(opened?)
    }
}

fn is_test_case_candidate(path: &WorkspacePath) -> bool {
    let value = path.as_str();
    if value.contains('/') {
        return false;
    }
    value
        .strip_suffix(".in")
        .or_else(|| value.strip_suffix(".expected"))
        .is_some_and(|name| TestCase::new(name).is_ok())
}

pub(crate) fn hash_bytes(bytes: &[u8]) -> Hash {
    Hash::from_bytes(*blake3::hash(bytes).as_bytes())
}

pub(crate) fn compare_test_case_bytes(
    case: TestCase,
    expected: &[u8],
    actual: &[u8],
) -> TestCaseComparison {
    let expected_blake3 = Some(hash_bytes(expected));
    let actual_blake3 = Some(hash_bytes(actual));
    let outcome = if expected == actual {
        TestCaseOutcome::Pass
    } else {
        TestCaseOutcome::Fail(first_mismatch(expected, actual))
    };
    TestCaseComparison {
        case,
        outcome,
        expected_blake3,
        actual_blake3,
    }
}

pub(crate) fn classify_test_case_result(
    case: TestCase,
    process_outcome: &rustrace_model::CommandOutcome,
    completeness: rustrace_model::CaptureCompleteness,
    actual: &[u8],
    expected: std::result::Result<Vec<u8>, String>,
) -> TestCaseComparison {
    use rustrace_model::{CaptureCompleteness, CommandOutcome, CommandTermination};

    let expected_blake3 = expected.as_ref().ok().map(|bytes| hash_bytes(bytes));
    let actual_blake3 =
        (completeness != CaptureCompleteness::Unavailable).then(|| hash_bytes(actual));
    let error = match process_outcome {
        CommandOutcome::LaunchFailed { .. } => Some("launch failed".to_owned()),
        CommandOutcome::Exited { code } if *code != 0 => Some(format!("exit {code}")),
        CommandOutcome::Terminated { reason, .. } => Some(
            match reason {
                CommandTermination::Cancelled => "cancelled",
                CommandTermination::Quit => "quit",
                CommandTermination::Deadline => "deadline exceeded",
                CommandTermination::OutputLimit => "output limit exceeded",
                CommandTermination::CaptureFailure => "capture failed",
                CommandTermination::Signal => "terminated by signal",
                CommandTermination::CleanupFailure => "process cleanup failed",
            }
            .to_owned(),
        ),
        CommandOutcome::Exited { code: 0 } => match completeness {
            CaptureCompleteness::Complete => None,
            CaptureCompleteness::Truncated => Some("stdout capture was truncated".to_owned()),
            CaptureCompleteness::ReadFailed => Some("stdout capture read failed".to_owned()),
            CaptureCompleteness::Unavailable => Some("stdout capture is unavailable".to_owned()),
        },
        CommandOutcome::Exited { .. } => unreachable!("nonzero exit handled above"),
    }
    .or_else(|| {
        expected.as_ref().err().map(|detail| {
            if detail.contains("exceeds the 1048576-byte limit") {
                "expected output is oversized".to_owned()
            } else {
                "expected output is unreadable".to_owned()
            }
        })
    });
    if let Some(reason) = error {
        return TestCaseComparison {
            case,
            outcome: TestCaseOutcome::Error(reason),
            expected_blake3,
            actual_blake3,
        };
    }
    compare_test_case_bytes(
        case,
        expected.as_ref().expect("comparable expected bytes"),
        actual,
    )
}

fn first_mismatch(expected: &[u8], actual: &[u8]) -> TestCaseMismatch {
    let mut expected_at = 0;
    let mut actual_at = 0;
    let mut line = 1_u64;
    loop {
        let expected_line = next_line(expected, expected_at);
        let actual_line = next_line(actual, actual_at);
        match (expected_line, actual_line) {
            (
                Some((expected, expected_lf, next_expected)),
                Some((actual, actual_lf, next_actual)),
            ) if expected == actual && expected_lf == actual_lf => {
                expected_at = next_expected;
                actual_at = next_actual;
                line += 1;
            }
            (expected, actual) => {
                let expected = expected.map_or(&[][..], |(line, _, _)| line);
                let actual = actual.map_or(&[][..], |(line, _, _)| line);
                return TestCaseMismatch {
                    line,
                    expected_len: expected.len(),
                    actual_len: actual.len(),
                    expected_preview: line_preview(expected),
                    actual_preview: line_preview(actual),
                };
            }
        }
    }
}

fn next_line(bytes: &[u8], at: usize) -> Option<(&[u8], bool, usize)> {
    let remaining = bytes.get(at..)?;
    if remaining.is_empty() {
        return None;
    }
    if let Some(relative_lf) = remaining.iter().position(|byte| *byte == b'\n') {
        Some((&remaining[..relative_lf], true, at + relative_lf + 1))
    } else {
        Some((remaining, false, bytes.len()))
    }
}

fn line_preview(bytes: &[u8]) -> String {
    crate::display::plain(
        bytes,
        crate::display::Limits {
            input_bytes: MAX_TEST_CASE_LINE_PREVIEW_INPUT_BYTES,
            output_bytes: MAX_TEST_CASE_LINE_PREVIEW_OUTPUT_BYTES,
            spans: crate::display::MAX_SPANS,
            lines: 1,
        },
    )
    .text
    .to_string()
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ConsoleLine {
    text: String,
    cursor: usize,
}

impl ConsoleLine {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn insert(&mut self, character: char) -> bool {
        if character.is_control()
            || self.text.len().saturating_add(character.len_utf8()) > MAX_CONSOLE_LINE_BYTES
        {
            return false;
        }
        self.text.insert(self.cursor, character);
        let desired = self.cursor + character.len_utf8();
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .chain(std::iter::once(self.text.len()))
            .find(|index| *index >= desired)
            .expect("text end is a grapheme boundary");
        true
    }

    pub(crate) fn left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index);
    }

    pub(crate) fn right(&mut self) {
        self.cursor += self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(0, str::len);
    }

    pub(crate) fn home(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn end(&mut self) {
        self.cursor = self.text.len();
    }

    pub(crate) fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let old = self.cursor;
        self.left();
        self.text.replace_range(self.cursor..old, "");
        true
    }

    pub(crate) fn delete(&mut self) -> bool {
        let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() else {
            return false;
        };
        self.text
            .replace_range(self.cursor..self.cursor + grapheme.len(), "");
        true
    }

    pub(crate) fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustrace_model::{CaptureCompleteness, CommandOutcome, CommandTermination};
    use std::{fs, os::unix::fs::symlink};

    fn fixture(name: &str) -> std::path::PathBuf {
        let parent =
            std::env::temp_dir().join(format!("rustrace-test-cases-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&parent);
        fs::create_dir(&parent).unwrap();
        fs::create_dir(parent.join("assignment.work")).unwrap();
        fs::canonicalize(parent).unwrap()
    }

    #[test]
    fn test_case_name_limit_matches_provenance_model() {
        let maximum = "a".repeat(rustrace_model::MAX_TEST_CASE_NAME_BYTES);
        assert!(TestCase::new(maximum.clone()).is_ok());
        assert!(TestCase::new(format!("{maximum}a")).is_err());
    }

    #[test]
    fn unicode_line_editing_uses_grapheme_boundaries() {
        let mut line = ConsoleLine::default();
        for character in "Ae\u{301}🦀Z".chars() {
            assert!(line.insert(character));
        }
        assert_eq!(line.text(), "Ae\u{301}🦀Z");
        assert_eq!(line.cursor(), line.text().len());

        line.left();
        line.left();
        assert_eq!(&line.text()[line.cursor()..], "🦀Z");
        assert!(line.delete());
        assert_eq!(line.text(), "Ae\u{301}Z");
        assert!(line.backspace());
        assert_eq!(line.text(), "AZ");
        line.home();
        line.right();
        assert_eq!(line.cursor(), 1);
        line.end();
        assert_eq!(line.cursor(), 2);
        assert_eq!(line.take(), "AZ");
        assert_eq!((line.text(), line.cursor()), ("", 0));
    }

    #[test]
    fn insertion_that_joins_neighbors_keeps_the_cursor_on_a_grapheme_boundary() {
        let mut line = ConsoleLine::default();
        assert!(line.insert('👩'));
        assert!(line.insert('💻'));
        line.home();
        line.right();
        assert!(line.insert('\u{200d}'));
        assert_eq!(line.text(), "👩‍💻");
        assert_eq!(line.cursor(), line.text().len());
        line.left();
        assert_eq!(line.cursor(), 0);
        line.right();
        assert_eq!(line.cursor(), line.text().len());
    }

    #[test]
    fn line_bound_is_utf8_atomic() {
        let mut line = ConsoleLine::default();
        for _ in 0..MAX_CONSOLE_LINE_BYTES - 4 {
            assert!(line.insert('a'));
        }
        assert!(line.insert('🦀'));
        assert_eq!(line.text().len(), MAX_CONSOLE_LINE_BYTES);
        assert!(!line.insert('b'));
        line.left();
        assert!(line.delete());
        assert_eq!(line.text().len(), MAX_CONSOLE_LINE_BYTES - 4);
        assert!(line.insert('🦀'));
        assert_eq!(line.text().len(), MAX_CONSOLE_LINE_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn fixed_sibling_pairs_refresh_in_bytewise_order_without_mutating_output() {
        let parent = fixture("refresh");
        fs::create_dir(parent.join("test-cases")).unwrap();
        fs::write(parent.join("test-cases/zebra.in"), b"one").unwrap();
        fs::write(parent.join("test-cases/zebra.expected"), b"expected one").unwrap();
        fs::write(parent.join("test-cases/Alpha.in"), b"two").unwrap();
        fs::write(parent.join("test-cases/Alpha.expected"), b"expected two").unwrap();
        fs::write(parent.join("test-cases/unpaired.in"), b"ignored").unwrap();
        fs::write(parent.join("test-cases/output.txt"), b"preserve").unwrap();
        let cases = TestCaseDirectory::open(&parent.join("assignment.work")).unwrap();
        assert_eq!(
            cases
                .list_cases()
                .unwrap()
                .iter()
                .map(TestCase::name)
                .collect::<Vec<_>>(),
            ["Alpha", "zebra"]
        );
        let zebra = cases.list_cases().unwrap().pop().unwrap();
        assert_eq!(cases.read_expected(&zebra).unwrap(), b"expected one");
        fs::write(parent.join("test-cases/zebra.expected"), b"refreshed").unwrap();
        assert_eq!(cases.read_expected(&zebra).unwrap(), b"refreshed");
        let output = WorkspacePath::new("output.txt").unwrap();
        assert_eq!(
            cases.output_disposition(&output).unwrap(),
            OutputDisposition::Overwrite
        );
        drop(
            cases
                .open_output(&output, OutputDisposition::Overwrite)
                .unwrap(),
        );
        assert_eq!(
            fs::read(parent.join("test-cases/output.txt")).unwrap(),
            b"preserve"
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn maximum_packaged_pair_count_is_listable() {
        let parent = fixture("maximum-pairs");
        fs::create_dir(parent.join("test-cases")).unwrap();
        for index in 0..MAX_TEST_CASES {
            fs::write(parent.join(format!("test-cases/case-{index:03}.in")), b"i").unwrap();
            fs::write(
                parent.join(format!("test-cases/case-{index:03}.expected")),
                b"o",
            )
            .unwrap();
        }
        fs::write(
            parent.join("test-cases/case-000.actual"),
            b"ordinary generated output",
        )
        .unwrap();
        let cases = TestCaseDirectory::open(&parent.join("assignment.work"))
            .unwrap()
            .list_cases()
            .unwrap();
        assert_eq!(cases.len(), MAX_TEST_CASES);
        assert_eq!(cases.first().unwrap().name(), "case-000");
        assert_eq!(cases.last().unwrap().name(), "case-255");
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refreshed_case_identity_changes_with_input_or_expected_bytes() {
        let parent = fixture("pair-identity");
        fs::create_dir(parent.join("test-cases")).unwrap();
        fs::write(parent.join("test-cases/sample.in"), b"input one").unwrap();
        fs::write(parent.join("test-cases/sample.expected"), b"output one").unwrap();
        let directory = TestCaseDirectory::open(&parent.join("assignment.work")).unwrap();
        let original = directory.list_cases().unwrap().pop().unwrap();

        fs::write(parent.join("test-cases/sample.in"), b"input two").unwrap();
        let changed_input = directory.list_cases().unwrap().pop().unwrap();
        assert_ne!(changed_input, original);

        fs::write(parent.join("test-cases/sample.expected"), b"output two").unwrap();
        let changed_expected = directory.list_cases().unwrap().pop().unwrap();
        assert_ne!(changed_expected, changed_input);
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fixed_sibling_root_and_refreshed_entries_reject_symlinks() {
        let parent = fixture("symlink-root");
        fs::create_dir(parent.join("actual-cases")).unwrap();
        symlink("actual-cases", parent.join("test-cases")).unwrap();
        assert!(TestCaseDirectory::open(&parent.join("assignment.work")).is_err());
        fs::remove_dir_all(&parent).unwrap();

        let parent = fixture("symlink-entry");
        fs::create_dir(parent.join("test-cases")).unwrap();
        fs::write(parent.join("outside"), b"outside").unwrap();
        symlink("../outside", parent.join("test-cases/unsafe")).unwrap();
        let cases = TestCaseDirectory::open(&parent.join("assignment.work")).unwrap();
        assert!(cases.list_cases().unwrap().is_empty());
        assert!(
            cases
                .read_expected(&TestCase::new("unsafe").unwrap())
                .is_err()
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn selected_workspace_named_test_cases_cannot_become_external_file_authority() {
        let parent = fixture("same-root");
        fs::remove_dir(parent.join("assignment.work")).unwrap();
        fs::create_dir(parent.join("test-cases")).unwrap();
        fs::write(parent.join("test-cases/managed.rs"), b"preserve").unwrap();
        assert!(TestCaseDirectory::open(&parent.join("test-cases")).is_err());
        assert_eq!(
            fs::read(parent.join("test-cases/managed.rs")).unwrap(),
            b"preserve"
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn case_pairing_accepts_only_the_packaged_name_grammar_and_complete_pairs() {
        let parent = fixture("pairing");
        fs::create_dir(parent.join("test-cases")).unwrap();
        for path in [
            "0.in",
            "0.expected",
            "a-b_C9.in",
            "a-b_C9.expected",
            "missing-input.expected",
            "missing-expected.in",
            ".in",
            ".expected",
            "dot.name.in",
            "dot.name.expected",
            "too-long-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.in",
            "too-long-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.expected",
            "notes.txt",
        ] {
            fs::write(parent.join("test-cases").join(path), path).unwrap();
        }
        fs::create_dir(parent.join("test-cases/nested")).unwrap();
        fs::write(parent.join("test-cases/nested/hidden.in"), b"ignored").unwrap();
        fs::write(parent.join("test-cases/nested/hidden.expected"), b"ignored").unwrap();

        let cases = TestCaseDirectory::open(&parent.join("assignment.work")).unwrap();
        assert_eq!(
            cases
                .list_cases()
                .unwrap()
                .iter()
                .map(TestCase::name)
                .collect::<Vec<_>>(),
            ["0", "a-b_C9"]
        );
        fs::remove_dir_all(parent).unwrap();
    }

    fn mismatch(comparison: &TestCaseComparison) -> &TestCaseMismatch {
        let TestCaseOutcome::Fail(mismatch) = &comparison.outcome else {
            panic!("expected mismatch, got {:?}", comparison.outcome);
        };
        mismatch
    }

    #[test]
    fn exact_byte_comparison_table_reports_the_first_lf_delimited_mismatch() {
        let case = TestCase::new("33").unwrap();
        for (expected, actual) in [
            (&b""[..], &b""[..]),
            (&b"same"[..], &b"same"[..]),
            (&b"same\n"[..], &b"same\n"[..]),
            (&b"\xff\x00\n"[..], &b"\xff\x00\n"[..]),
        ] {
            assert_eq!(
                compare_test_case_bytes(case.clone(), expected, actual).outcome,
                TestCaseOutcome::Pass
            );
        }

        type MismatchCase<'a> = (&'a [u8], &'a [u8], u64, usize, usize);
        let table: &[MismatchCase<'_>] = &[
            (b"same\n", b"same", 1, 4, 4),
            (b"one\n", b"one\ntwo\n", 2, 0, 3),
            (b"abc\n", b"abX\n", 1, 3, 3),
            (b"line\r\n", b"line\n", 1, 5, 4),
            (b"\xff\n", b"\xfe\n", 1, 1, 1),
            (b"one\n", b"one\nextra", 2, 0, 5),
            (b"one\nmissing", b"one\n", 2, 7, 0),
            (b"first\nsecond\nthird", b"first\nSECOND\nthird", 2, 6, 6),
        ];
        for (expected, actual, line, expected_len, actual_len) in table {
            let comparison = compare_test_case_bytes(case.clone(), expected, actual);
            let mismatch = mismatch(&comparison);
            assert_eq!(
                mismatch.line, *line,
                "expected={expected:?} actual={actual:?}"
            );
            assert_eq!(mismatch.expected_len, *expected_len);
            assert_eq!(mismatch.actual_len, *actual_len);
            assert_eq!(comparison.expected_blake3, Some(hash_bytes(expected)));
            assert_eq!(comparison.actual_blake3, Some(hash_bytes(actual)));
        }
    }

    #[test]
    fn comparison_previews_are_shared_safe_display_output_and_strictly_bounded() {
        let case = TestCase::new("unsafe").unwrap();
        let mut expected = vec![b'a'; MAX_TEST_CASE_LINE_PREVIEW_INPUT_BYTES * 2];
        expected[0] = 0x1b;
        expected[1] = 0xff;
        let actual = vec![b'b'; MAX_TEST_CASE_LINE_PREVIEW_INPUT_BYTES * 2];
        let comparison = compare_test_case_bytes(case, &expected, &actual);
        let mismatch = mismatch(&comparison);

        assert_eq!(mismatch.expected_len, expected.len());
        assert_eq!(mismatch.actual_len, actual.len());
        assert!(mismatch.expected_preview.contains("\\u{1b}\\xff"));
        assert!(
            mismatch
                .expected_preview
                .contains(crate::display::TRUNCATED)
        );
        assert!(mismatch.actual_preview.contains(crate::display::TRUNCATED));
        assert!(mismatch.expected_preview.len() <= MAX_TEST_CASE_LINE_PREVIEW_OUTPUT_BYTES);
        assert!(mismatch.actual_preview.len() <= MAX_TEST_CASE_LINE_PREVIEW_OUTPUT_BYTES);
        assert!(!mismatch.expected_preview.as_bytes().contains(&0x1b));
    }

    #[test]
    fn finished_case_classifies_every_non_comparable_condition_as_error() {
        let case = TestCase::new("errors").unwrap();
        let successful = CommandOutcome::Exited { code: 0 };
        let errors = [
            (
                CommandOutcome::LaunchFailed { os_code: Some(2) },
                CaptureCompleteness::Complete,
                Ok(b"expected".to_vec()),
                "launch failed",
            ),
            (
                CommandOutcome::Exited { code: 9 },
                CaptureCompleteness::Complete,
                Ok(b"expected".to_vec()),
                "exit 9",
            ),
            (
                CommandOutcome::Terminated {
                    reason: CommandTermination::Deadline,
                    signal: None,
                },
                CaptureCompleteness::Complete,
                Ok(b"expected".to_vec()),
                "deadline",
            ),
            (
                successful.clone(),
                CaptureCompleteness::Truncated,
                Ok(b"expected".to_vec()),
                "truncated",
            ),
            (
                successful.clone(),
                CaptureCompleteness::Unavailable,
                Ok(b"expected".to_vec()),
                "unavailable",
            ),
            (
                successful.clone(),
                CaptureCompleteness::ReadFailed,
                Ok(b"expected".to_vec()),
                "read failed",
            ),
            (
                successful,
                CaptureCompleteness::Complete,
                Err("arbitrary filesystem detail must not escape".to_owned()),
                "expected output is unreadable",
            ),
            (
                CommandOutcome::Exited { code: 0 },
                CaptureCompleteness::Complete,
                Err("expected output exceeds the 1048576-byte limit".to_owned()),
                "expected output is oversized",
            ),
        ];

        for (outcome, completeness, expected, reason) in errors {
            let comparison = classify_test_case_result(
                case.clone(),
                &outcome,
                completeness,
                b"actual",
                expected,
            );
            let TestCaseOutcome::Error(actual) = &comparison.outcome else {
                panic!("{reason} was not classified as an error: {comparison:?}");
            };
            assert!(
                actual.contains(reason),
                "{actual:?} does not contain {reason:?}"
            );
            if completeness == CaptureCompleteness::Unavailable {
                assert_eq!(comparison.actual_blake3, None);
            }
        }

        for reason in [
            CommandTermination::Cancelled,
            CommandTermination::Quit,
            CommandTermination::OutputLimit,
            CommandTermination::CaptureFailure,
            CommandTermination::Signal,
            CommandTermination::CleanupFailure,
        ] {
            let comparison = classify_test_case_result(
                case.clone(),
                &CommandOutcome::Terminated {
                    reason,
                    signal: Some(9),
                },
                CaptureCompleteness::Complete,
                b"actual",
                Ok(b"expected".to_vec()),
            );
            assert!(matches!(comparison.outcome, TestCaseOutcome::Error(_)));
        }
    }

    #[test]
    fn oversized_expected_output_is_unreadable_for_comparison() {
        let parent = fixture("oversized-expected");
        fs::create_dir(parent.join("test-cases")).unwrap();
        fs::write(parent.join("test-cases/large.in"), b"").unwrap();
        fs::write(
            parent.join("test-cases/large.expected"),
            vec![b'x'; MAX_TEST_CASE_FILE_BYTES as usize + 1],
        )
        .unwrap();
        let cases = TestCaseDirectory::open(&parent.join("assignment.work")).unwrap();
        let case = cases.list_cases().unwrap().pop().unwrap();
        assert!(
            cases
                .read_expected(&case)
                .unwrap_err()
                .to_string()
                .contains("1048576-byte limit")
        );
        fs::remove_dir_all(parent).unwrap();
    }
}
