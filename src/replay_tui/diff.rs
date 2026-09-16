//! Bounded, ephemeral line-diff presentation for replay.

use rustrace_journal::MAX_CHECKPOINT_FILE_BYTES;

pub const MAX_DIFF_INPUT_LINES: usize = 4_096;
pub const MAX_DIFF_TABLE_CELLS: usize = 1_000_000;
pub const MAX_DIFF_OUTPUT_LINES: usize = 4_096;
pub const MAX_DIFF_OUTPUT_BYTES: usize = 256 * 1024;

pub const DIFF_TOO_LARGE_NOTICE: &str = "diff too large to display";
pub const BINARY_DIFF_NOTICE: &str = "binary/non-UTF-8 file; diff unavailable";
pub const NO_PREVIOUS_CHECKPOINT_NOTICE: &str = "no preceding checkpoint for this event";
pub(super) const NO_NEWLINE_AT_END_MARKER: &str = " [no newline at end]";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComparisonMode {
    Final,
    PreviousCheckpoint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffLineKind {
    Context,
    Insertion,
    Deletion,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
    pub unterminated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffNotice {
    TooLarge,
    Binary,
    NoPreviousCheckpoint,
}

impl DiffNotice {
    pub const fn text(self) -> &'static str {
        match self {
            Self::TooLarge => DIFF_TOO_LARGE_NOTICE,
            Self::Binary => BINARY_DIFF_NOTICE,
            Self::NoPreviousCheckpoint => NO_PREVIOUS_CHECKPOINT_NOTICE,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffContent {
    Lines(Vec<DiffLine>),
    Notice(DiffNotice),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComparisonPoint {
    pub segment: usize,
    pub sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffView {
    pub mode: ComparisonMode,
    pub path: String,
    pub comparison: Option<ComparisonPoint>,
    pub files: Vec<String>,
    pub insertions: usize,
    pub deletions: usize,
    pub content: DiffContent,
}

pub(crate) fn compare_files(
    left: Option<&[u8]>,
    right: Option<&[u8]>,
) -> (usize, usize, DiffContent) {
    let left = left.unwrap_or_default();
    let right = right.unwrap_or_default();
    if left.len() > MAX_CHECKPOINT_FILE_BYTES || right.len() > MAX_CHECKPOINT_FILE_BYTES {
        return too_large();
    }
    let (Ok(left), Ok(right)) = (std::str::from_utf8(left), std::str::from_utf8(right)) else {
        return (0, 0, DiffContent::Notice(DiffNotice::Binary));
    };
    let Some(left) = split_lines(left) else {
        return too_large();
    };
    let Some(right) = split_lines(right) else {
        return too_large();
    };
    let Some(cells) = (left.len() + 1).checked_mul(right.len() + 1) else {
        return too_large();
    };
    if cells > MAX_DIFF_TABLE_CELLS {
        return too_large();
    }

    let width = right.len() + 1;
    let mut lcs = vec![0_u16; cells];
    for left_index in (0..left.len()).rev() {
        for right_index in (0..right.len()).rev() {
            let at = left_index * width + right_index;
            lcs[at] = if left[left_index].raw == right[right_index].raw {
                lcs[(left_index + 1) * width + right_index + 1].saturating_add(1)
            } else {
                lcs[(left_index + 1) * width + right_index]
                    .max(lcs[left_index * width + right_index + 1])
            };
        }
    }

    let mut lines = Vec::new();
    let mut output_bytes = 0_usize;
    let mut insertions = 0_usize;
    let mut deletions = 0_usize;
    let mut left_index = 0_usize;
    let mut right_index = 0_usize;
    while left_index < left.len() || right_index < right.len() {
        let (kind, line) = if left_index < left.len()
            && right_index < right.len()
            && left[left_index].raw == right[right_index].raw
        {
            let line = left[left_index];
            left_index += 1;
            right_index += 1;
            (DiffLineKind::Context, line)
        } else if left_index < left.len()
            && (right_index == right.len()
                || lcs[(left_index + 1) * width + right_index]
                    >= lcs[left_index * width + right_index + 1])
        {
            let line = left[left_index];
            left_index += 1;
            deletions += 1;
            (DiffLineKind::Deletion, line)
        } else {
            let line = right[right_index];
            right_index += 1;
            insertions += 1;
            (DiffLineKind::Insertion, line)
        };
        let unterminated = !line.terminated;
        let rendered_bytes = line
            .display
            .len()
            .saturating_add(2)
            .saturating_add(usize::from(unterminated) * NO_NEWLINE_AT_END_MARKER.len());
        let Some(next_bytes) = output_bytes.checked_add(rendered_bytes) else {
            return too_large();
        };
        if lines.len() == MAX_DIFF_OUTPUT_LINES || next_bytes > MAX_DIFF_OUTPUT_BYTES {
            return too_large();
        }
        output_bytes = next_bytes;
        lines.push(DiffLine {
            kind,
            text: line.display.to_owned(),
            unterminated,
        });
    }
    (insertions, deletions, DiffContent::Lines(lines))
}

#[derive(Clone, Copy)]
struct LogicalLine<'a> {
    raw: &'a str,
    display: &'a str,
    terminated: bool,
}

fn split_lines(text: &str) -> Option<Vec<LogicalLine<'_>>> {
    let mut lines = Vec::new();
    for raw in text.split_inclusive('\n') {
        if lines.len() == MAX_DIFF_INPUT_LINES {
            return None;
        }
        lines.push(LogicalLine {
            raw,
            display: raw.strip_suffix('\n').unwrap_or(raw),
            terminated: raw.ends_with('\n'),
        });
    }
    Some(lines)
}

fn too_large() -> (usize, usize, DiffContent) {
    (0, 0, DiffContent::Notice(DiffNotice::TooLarge))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustrace_journal::MAX_CHECKPOINT_FILE_BYTES;

    fn lines(content: DiffContent) -> Vec<DiffLine> {
        match content {
            DiffContent::Lines(lines) => lines,
            DiffContent::Notice(notice) => panic!("unexpected notice: {}", notice.text()),
        }
    }

    #[test]
    fn missing_side_is_a_whole_file_insertion_or_deletion() {
        let (insertions, deletions, content) = compare_files(None, Some(b"one\ntwo\n"));
        assert_eq!((insertions, deletions), (2, 0));
        assert!(
            lines(content)
                .iter()
                .all(|line| line.kind == DiffLineKind::Insertion)
        );

        let (insertions, deletions, content) = compare_files(Some(b"one\ntwo\n"), None);
        assert_eq!((insertions, deletions), (0, 2));
        assert!(
            lines(content)
                .iter()
                .all(|line| line.kind == DiffLineKind::Deletion)
        );
    }

    #[test]
    fn binary_and_maximum_size_inputs_use_bounded_notices() {
        assert_eq!(
            compare_files(Some(b"valid\n"), Some(b"bad\xff\n")).2,
            DiffContent::Notice(DiffNotice::Binary)
        );

        let maximum = vec![b'x'; MAX_CHECKPOINT_FILE_BYTES];
        assert_eq!(
            compare_files(Some(&maximum), Some(&maximum)).2,
            DiffContent::Notice(DiffNotice::TooLarge)
        );
    }

    #[test]
    fn trailing_newline_only_change_marks_the_unterminated_side() {
        let content = compare_files(Some(b"a\nb"), Some(b"a\nb\n")).2;
        let lines = lines(content);
        let deletion = lines
            .iter()
            .find(|line| line.kind == DiffLineKind::Deletion)
            .unwrap();
        let insertion = lines
            .iter()
            .find(|line| line.kind == DiffLineKind::Insertion)
            .unwrap();

        assert_eq!(deletion.text, "b");
        assert!(deletion.unterminated);
        assert_eq!(insertion.text, "b");
        assert!(!insertion.unterminated);
    }
}
