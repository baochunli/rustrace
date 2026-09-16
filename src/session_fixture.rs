//! Deterministic input schedules, not synthetic production timestamps.
//! The probe may execute fast for coverage or wait the declared hour using Instant.
use rustrace_model::WorkspacePath;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FixtureAction {
    Insert(char),
    Paste(String),
    Undo,
    Redo,
    Save,
    Checkpoint,
    Restart,
    External,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixtureStep {
    pub at_millis: u64,
    pub action: FixtureAction,
}

pub fn representative_hour(mut seed: u64) -> Vec<FixtureStep> {
    let mut steps = Vec::new();
    for second in 1..=3600_u64 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let action = if second == 1800 {
            FixtureAction::Restart
        } else if second % 97 == 0 {
            FixtureAction::External
        } else if second % 60 == 0 {
            FixtureAction::Checkpoint
        } else if second % 15 == 0 {
            FixtureAction::Save
        } else if second % 23 == 0 {
            FixtureAction::Paste("// café 東京\n".into())
        } else if second % 17 == 0 {
            FixtureAction::Undo
        } else if second % 17 == 1 {
            FixtureAction::Redo
        } else {
            FixtureAction::Insert((b'a' + (seed % 26) as u8) as char)
        };
        steps.push(FixtureStep {
            at_millis: second * 1000,
            action,
        });
    }
    steps
}

pub fn maximum_workspace(seed: u64) -> BTreeMap<WorkspacePath, Vec<u8>> {
    let mut files = BTreeMap::new();
    let remaining = 9 * 1024 * 1024;
    for index in 0..256 {
        let length = if index == 0 {
            1024 * 1024
        } else {
            remaining / 255 + usize::from(index <= remaining % 255)
        };
        let line = format!("// fixture seed={seed:016x} file={index:03} bounded Rust source\n");
        let mut bytes = line.repeat(length / line.len()).into_bytes();
        bytes.resize(length, b' ');
        files.insert(
            WorkspacePath::new(format!("file-{index:03}.rs"))
                .expect("fixed canonical fixture path"),
            bytes,
        );
    }
    files
}
