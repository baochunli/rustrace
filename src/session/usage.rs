//! Recorded counts that one submission caps, reported before they matter.
//!
//! Recording never stops at a limit: a student keeps working and keeps every
//! event. `rustrace status` shows these counts for an unfinished attempt, and
//! `rustrace work` warns once a count passes three quarters of its limit, so a
//! long attempt is submitted before `rustrace submit` would stop.

use super::{ProductionSession, Result};
use crate::display::grouped;
use rustrace_journal::Journal;
use rustrace_model::{
    MAX_RPROV_CHECKPOINTS_PER_SEGMENT, MAX_RPROV_EVENTS, MAX_RPROV_METADATA_PER_SEGMENT, SessionId,
};
use rustrace_workspace::hash::{PinnedWorkspaceRoot, WorkspaceHashError};
use std::{fs, path::Path};

/// Counts for one unfinished attempt (one `.rprov` segment).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordingUsage {
    pub checkpoints: u64,
    pub events: u64,
    /// `rustrace work` starts that recorded a toolchain observation.
    pub launches: u64,
}

/// The per-segment limits the counts are measured against.
#[derive(Clone, Copy, Debug)]
pub(crate) struct UsageLimits {
    pub(crate) checkpoints: u64,
    pub(crate) events: u64,
}

impl UsageLimits {
    pub(crate) const PACKAGE: Self = Self {
        checkpoints: MAX_RPROV_CHECKPOINTS_PER_SEGMENT as u64,
        events: MAX_RPROV_EVENTS,
    };
}

impl RecordingUsage {
    pub(crate) fn status_lines(&self, limits: UsageLimits) -> Vec<String> {
        let mut lines = vec![
            format!(
                "Recorded checkpoints: {} of the {} one submission can hold",
                grouped(self.checkpoints),
                grouped(limits.checkpoints)
            ),
            format!(
                "Recorded events: {} of the {} one submission can hold",
                grouped(self.events),
                grouped(limits.events)
            ),
            format!(
                "Recorded launches: {}; a submission keeps at most {} of their toolchain observations (the first, the last, and each change)",
                grouped(self.launches),
                MAX_RPROV_METADATA_PER_SEGMENT
            ),
        ];
        lines.extend(self.warning(limits));
        lines
    }

    /// One line once a count passes three quarters of its limit.
    pub(crate) fn warning(&self, limits: UsageLimits) -> Option<String> {
        let (what, count, limit) = [
            ("checkpoints", self.checkpoints, limits.checkpoints),
            ("events", self.events, limits.events),
        ]
        .into_iter()
        .find(|(_, count, limit)| u128::from(*count) * 4 > u128::from(*limit) * 3)?;
        Some(if count > limit {
            format!(
                "Warning: this attempt has {} {what}, more than the {} one submission can hold, so `rustrace submit` stops before creating a bundle. Nothing is lost and you can keep working; run `rustrace update`, and tell your course staff if submit still stops.",
                grouped(count),
                grouped(limit)
            )
        } else {
            format!(
                "Warning: this attempt has {} {what}, {}% of the {} one submission can hold. Submit it before it reaches the limit; to keep working after that, use `rustrace revise`.",
                grouped(count),
                u128::from(count) * 100 / u128::from(limit),
                grouped(limit)
            )
        })
    }
}

impl ProductionSession {
    /// Counts for this live session, from its own recorded state.
    pub fn recording_usage(&self) -> Result<RecordingUsage> {
        let authority = self.effects.0.borrow();
        authority.owner.verify()?;
        let state_directory = authority
            .owner
            .display_path()
            .parent()
            .ok_or("session state directory is missing")?;
        Ok(RecordingUsage {
            checkpoints: authority.checkpoints,
            events: authority.sequence,
            launches: count_launches(state_directory)?,
        })
    }
}

/// Reads the counts of a closed attempt without changing it. Returns `None`
/// while a session holds the workspace: its journal is then never opened.
pub(crate) fn inspect_unfinished_usage(
    root: &Path,
    session_id: &SessionId,
) -> Result<Option<RecordingUsage>> {
    let state = PinnedWorkspaceRoot::open(root)?.open_existing_state_directory()?;
    let mut owner = match state.open_journal_file(session_id) {
        Ok(owner) => owner,
        Err(WorkspaceHashError::Filesystem {
            operation: "acquire exclusive workspace writer ownership",
            ..
        }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let usage = (|| -> Result<RecordingUsage> {
        let mut journal = Journal::open_read_only_no_follow(owner.display_path())?;
        let checkpoints = journal.checkpoint_totals(session_id)?.count;
        let events = journal
            .inspect_session(session_id)?
            .next_sequence
            .saturating_sub(1);
        drop(journal);
        owner.verify()?;
        let state_directory = owner
            .display_path()
            .parent()
            .ok_or("session state directory is missing")?;
        Ok(RecordingUsage {
            checkpoints,
            events,
            launches: count_launches(state_directory)?,
        })
    })();
    owner.release_ownership()?;
    usage.map(Some)
}

fn count_launches(state_directory: &Path) -> Result<u64> {
    let mut launches = 0_u64;
    for entry in fs::read_dir(state_directory)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("toolchain-") && name.ends_with(".json") {
            launches += 1;
        }
    }
    Ok(launches)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: UsageLimits = UsageLimits {
        checkpoints: 8_192,
        events: 1_000_000,
    };

    fn usage(checkpoints: u64, events: u64) -> RecordingUsage {
        RecordingUsage {
            checkpoints,
            events,
            launches: 70,
        }
    }

    #[test]
    fn warning_starts_once_a_count_passes_three_quarters_of_its_limit() {
        assert_eq!(usage(6_144, 750_000).warning(LIMITS), None);
        assert_eq!(
            usage(6_145, 0).warning(LIMITS).unwrap(),
            "Warning: this attempt has 6,145 checkpoints, 75% of the 8,192 one submission can hold. Submit it before it reaches the limit; to keep working after that, use `rustrace revise`."
        );
        assert!(
            usage(0, 900_000)
                .warning(LIMITS)
                .unwrap()
                .starts_with("Warning: this attempt has 900,000 events, 90% of the 1,000,000")
        );
        // One line even when both counts are high; checkpoints come first.
        assert!(
            usage(8_192, 999_999)
                .warning(LIMITS)
                .unwrap()
                .contains("8,192 checkpoints, 100% of the 8,192")
        );
        assert_eq!(
            usage(9_000, 0).warning(LIMITS).unwrap(),
            "Warning: this attempt has 9,000 checkpoints, more than the 8,192 one submission can hold, so `rustrace submit` stops before creating a bundle. Nothing is lost and you can keep working; run `rustrace update`, and tell your course staff if submit still stops."
        );
    }

    #[test]
    fn status_lines_show_each_count_against_its_limit() {
        assert_eq!(
            usage(1_234, 20_345).status_lines(LIMITS),
            [
                "Recorded checkpoints: 1,234 of the 8,192 one submission can hold",
                "Recorded events: 20,345 of the 1,000,000 one submission can hold",
                "Recorded launches: 70; a submission keeps at most 64 of their toolchain observations (the first, the last, and each change)",
            ]
        );
        let near = usage(7_000, 10).status_lines(LIMITS);
        assert_eq!(near.len(), 4);
        assert!(near[3].starts_with("Warning: this attempt has 7,000 checkpoints, 85%"));
    }
}
