//! Recorded counts that one submission caps, reported before they matter.
//!
//! Recording never stops at a limit: a student keeps working and keeps every
//! event. `rustrace status` shows how much room an unfinished attempt has
//! left, and `rustrace work` warns once a count passes three quarters of what
//! the attempt can hold, so a long attempt is submitted before `rustrace
//! submit` would stop. A linked revision's ZIP also carries its earlier
//! attempts, so its room is what those leave.

use super::{
    METADATA_LIMIT, ProductionSession, Result, SessionMetadata,
    finalization::{ChainTotals, earlier_attempt_totals},
};
use crate::display::grouped;
use rustrace_journal::{Journal, MAX_JOURNAL_SEQUENCE};
use rustrace_model::{
    MAX_RPROV_ARCHIVE_ENTRIES, MAX_RPROV_CHECKPOINTS_PER_SEGMENT, MAX_RPROV_EVENTS,
    MAX_RPROV_METADATA_PER_SEGMENT,
};
use rustrace_workspace::hash::PinnedWorkspaceRoot;
use std::{fs, path::Path};

/// What `rustrace submit` records before it checks the limits: one boundary
/// checkpoint, and up to three events (resume, checkpoint, and terminal).
const SUBMIT_CHECKPOINTS: i128 = 1;
const SUBMIT_EVENTS: i128 = 3;
/// `manifest.json` inside the `.rprov`, and `session.rprov` in the ZIP.
const PACKAGE_FILES: i128 = 2;

/// Counts for one unfinished attempt (one `.rprov` segment).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordingUsage {
    pub checkpoints: u64,
    pub events: u64,
    /// `rustrace work` starts that recorded a toolchain observation.
    pub launches: u64,
    /// Files besides checkpoints this attempt adds to a submission: its event
    /// stream, exported toolchain observations, outside-change evidence, and
    /// the source files beside the `.rprov` in the ZIP.
    pub other_files: u64,
}

/// The earlier attempts a linked revision's submission also carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EarlierAttempts {
    None,
    Known(ChainTotals),
    Unreadable,
}

/// The limits the counts are measured against.
#[derive(Clone, Copy, Debug)]
pub(crate) struct UsageLimits {
    /// Checkpoints in one attempt.
    pub(crate) checkpoints: u64,
    /// Events in one submission, earlier attempts included.
    pub(crate) events: u64,
    /// Files in one submission, earlier attempts included.
    pub(crate) files: u64,
}

impl UsageLimits {
    pub(crate) const PACKAGE: Self = Self {
        checkpoints: MAX_RPROV_CHECKPOINTS_PER_SEGMENT as u64,
        events: MAX_RPROV_EVENTS,
        files: MAX_RPROV_ARCHIVE_ENTRIES as u64,
    };
}

/// Room left for one count before `rustrace submit` would stop.
struct Room {
    what: &'static str,
    recorded: u64,
    /// Negative once the attempt is already over.
    room: i128,
    /// Whether the attempt's own limit binds, so a linked attempt started
    /// after submitting would have more room.
    revise_adds_room: bool,
}

impl RecordingUsage {
    fn rooms(&self, limits: UsageLimits, earlier: EarlierAttempts) -> [Room; 2] {
        let recorded = i128::from(self.checkpoints);
        let attempt = i128::from(limits.checkpoints) - SUBMIT_CHECKPOINTS - recorded;
        let chain = match earlier {
            EarlierAttempts::Known(totals) if totals.segments > 0 => Some(
                i128::from(limits.files)
                    - PACKAGE_FILES
                    - i128::from(totals.entries)
                    - i128::from(self.other_files)
                    - SUBMIT_CHECKPOINTS
                    - recorded,
            ),
            _ => None,
        };
        let (checkpoint_room, revise_adds_room) = match chain {
            Some(chain) if chain < attempt => (chain, false),
            _ => (attempt, true),
        };
        let earlier_events = match earlier {
            EarlierAttempts::Known(totals) => i128::from(totals.events),
            EarlierAttempts::None | EarlierAttempts::Unreadable => 0,
        };
        [
            Room {
                what: "checkpoints",
                recorded: self.checkpoints,
                room: checkpoint_room,
                revise_adds_room,
            },
            Room {
                what: "events",
                recorded: self.events,
                room: i128::from(limits.events)
                    - SUBMIT_EVENTS
                    - earlier_events
                    - i128::from(self.events),
                // One submission holds the same number of events as one attempt.
                revise_adds_room: false,
            },
        ]
    }

    pub(crate) fn status_lines(
        &self,
        limits: UsageLimits,
        earlier: EarlierAttempts,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        match earlier {
            EarlierAttempts::Known(totals) if totals.segments > 0 => lines.push(format!(
                "Earlier attempts: {}, already holding {} of the {} files and {} of the {} events one submission can hold",
                grouped(totals.segments),
                grouped(totals.entries),
                grouped(limits.files),
                grouped(totals.events),
                grouped(limits.events)
            )),
            EarlierAttempts::Unreadable => lines.push(
                "Earlier attempts: unreadable, so the room below counts this attempt alone"
                    .to_owned(),
            ),
            EarlierAttempts::Known(_) | EarlierAttempts::None => {}
        }
        for room in self.rooms(limits, earlier) {
            lines.push(if room.room < 0 {
                format!(
                    "{}: {} recorded, {} more than `rustrace submit` can package",
                    capitalized(room.what),
                    grouped(room.recorded),
                    grouped(room.room.unsigned_abs() as u64)
                )
            } else {
                format!(
                    "{}: {} recorded; room for {} more before `rustrace submit` stops",
                    capitalized(room.what),
                    grouped(room.recorded),
                    grouped(room.room as u64)
                )
            });
        }
        lines.push(format!(
            "Launches: {} recorded; a submission keeps at most {} of their toolchain observations (the first, the last, and each change)",
            grouped(self.launches),
            MAX_RPROV_METADATA_PER_SEGMENT
        ));
        lines.extend(self.warning(limits, earlier));
        lines
    }

    /// One line once a count passes three quarters of what the attempt can
    /// hold, counting what `rustrace submit` itself adds.
    pub(crate) fn warning(&self, limits: UsageLimits, earlier: EarlierAttempts) -> Option<String> {
        let room = self.rooms(limits, earlier).into_iter().find(|room| {
            let recorded = i128::from(room.recorded);
            room.room < 0 || recorded * 4 > (recorded + room.room) * 3
        })?;
        Some(if room.room < 0 {
            format!(
                "Warning: tell your course staff now. This attempt has {} {}, {} more than `rustrace submit` can package, so it stops before creating a bundle; your work and its recorded history are kept.",
                grouped(room.recorded),
                room.what,
                grouped(room.room.unsigned_abs() as u64)
            )
        } else {
            format!(
                "Warning: this attempt has {} {}, and `rustrace submit` has room for only {} more. {}",
                grouped(room.recorded),
                room.what,
                grouped(room.room as u64),
                if room.revise_adds_room {
                    "Submit it soon; to keep working after that, use `rustrace revise`."
                } else {
                    "Submit it soon and tell your course staff: a linked attempt would not add room."
                }
            )
        })
    }
}

fn capitalized(word: &str) -> String {
    let mut characters = word.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
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
        let workspace_files = authority
            .replay
            .as_ref()
            .map_or(0, |replay| replay.workspace_state().files().len() as u64);
        let (launches, evidence) = count_state_files(state_directory)?;
        Ok(RecordingUsage {
            checkpoints: authority.checkpoints,
            events: authority.sequence,
            launches,
            other_files: other_files(launches, evidence, workspace_files),
        })
    }

    /// What a linked revision's earlier attempts already hold.
    pub(crate) fn earlier_attempts(&self) -> EarlierAttempts {
        if self.metadata.parent_evidence.is_none() {
            return EarlierAttempts::None;
        }
        self.effects
            .0
            .borrow()
            .owner
            .read_artifact("parent.json", METADATA_LIMIT)
            .ok()
            .map_or(EarlierAttempts::Unreadable, |bytes| {
                earlier_from_link(&bytes, &self.metadata)
            })
    }
}

/// What a closed attempt's earlier attempts hold, read without a lock.
pub(crate) fn inspect_earlier_attempts(root: &Path, metadata: &SessionMetadata) -> EarlierAttempts {
    if metadata.parent_evidence.is_none() {
        return EarlierAttempts::None;
    }
    PinnedWorkspaceRoot::open(root)
        .and_then(|root| root.open_existing_state_directory())
        .and_then(|state| state.read_artifact("parent.json", METADATA_LIMIT))
        .ok()
        .map_or(EarlierAttempts::Unreadable, |bytes| {
            earlier_from_link(&bytes, metadata)
        })
}

fn earlier_from_link(link: &[u8], metadata: &SessionMetadata) -> EarlierAttempts {
    // Abandonment evidence links a recovery copy, not finalized ancestry.
    match serde_json::from_slice::<serde_json::Value>(link) {
        Ok(value)
            if value.get("kind").and_then(|kind| kind.as_str()) != Some("finalized_revision") =>
        {
            EarlierAttempts::None
        }
        _ => earlier_attempt_totals(link, metadata)
            .map_or(EarlierAttempts::Unreadable, EarlierAttempts::Known),
    }
}

/// Reads the counts of a closed attempt without changing it. Returns `None`
/// while a session holds the workspace: its journal is then never opened.
pub(crate) fn inspect_unfinished_usage(
    root: &Path,
    metadata: &SessionMetadata,
) -> Result<Option<RecordingUsage>> {
    let session_id = &metadata.session_id;
    let state = PinnedWorkspaceRoot::open(root)?.open_existing_state_directory()?;
    let mut owner = match state.open_journal_file_if_idle(session_id) {
        Ok(owner) => owner,
        Err(error) if error.is_writer_contention() => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let usage = (|| -> Result<RecordingUsage> {
        let mut journal = Journal::open_read_only_no_follow(owner.display_path())?;
        let checkpoints = journal.checkpoint_totals(session_id)?.count;
        let events = journal
            .inspect_session(session_id)?
            .next_sequence
            .saturating_sub(1);
        let workspace_files = journal
            .latest_checkpoint_at_or_before(session_id, MAX_JOURNAL_SEQUENCE)?
            .map_or(0, |checkpoint| checkpoint.snapshot.files().len() as u64);
        drop(journal);
        owner.verify()?;
        let state_directory = owner
            .display_path()
            .parent()
            .ok_or("session state directory is missing")?;
        let (launches, evidence) = count_state_files(state_directory)?;
        Ok(RecordingUsage {
            checkpoints,
            events,
            launches,
            other_files: other_files(launches, evidence, workspace_files),
        })
    })();
    owner.release_ownership()?;
    usage.map(Some)
}

fn other_files(launches: u64, evidence: u64, workspace_files: u64) -> u64 {
    // The event stream, the exported observations, and the evidence files,
    // plus the source files the ZIP carries beside the `.rprov`.
    1 + launches.min(MAX_RPROV_METADATA_PER_SEGMENT as u64) + evidence + workspace_files
}

/// Launches (toolchain observations) and outside-change evidence files.
fn count_state_files(state_directory: &Path) -> Result<(u64, u64)> {
    let mut launches = 0_u64;
    let mut evidence = 0_u64;
    for entry in fs::read_dir(state_directory)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("toolchain-") && name.ends_with(".json") {
            launches += 1;
        } else if name.starts_with("evidence-") && name.ends_with(".bin") {
            evidence += 1;
        }
    }
    Ok((launches, evidence))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: UsageLimits = UsageLimits::PACKAGE;

    fn usage(checkpoints: u64, events: u64) -> RecordingUsage {
        RecordingUsage {
            checkpoints,
            events,
            launches: 70,
            other_files: 100,
        }
    }

    #[test]
    fn room_counts_what_submit_adds_so_the_limit_itself_is_already_over() {
        // Submit's own boundary checkpoint makes 8,192 recorded one too many.
        assert_eq!(
            usage(8_191, 0).status_lines(LIMITS, EarlierAttempts::None)[0],
            "Checkpoints: 8,191 recorded; room for 0 more before `rustrace submit` stops"
        );
        assert_eq!(
            usage(8_192, 0).status_lines(LIMITS, EarlierAttempts::None)[0],
            "Checkpoints: 8,192 recorded, 1 more than `rustrace submit` can package"
        );
        assert_eq!(
            usage(8_192, 0)
                .warning(LIMITS, EarlierAttempts::None)
                .unwrap(),
            "Warning: tell your course staff now. This attempt has 8,192 checkpoints, 1 more than `rustrace submit` can package, so it stops before creating a bundle; your work and its recorded history are kept."
        );
        assert_eq!(
            usage(0, 999_997).status_lines(LIMITS, EarlierAttempts::None)[1],
            "Events: 999,997 recorded; room for 0 more before `rustrace submit` stops"
        );
    }

    #[test]
    fn warning_starts_once_a_count_passes_three_quarters_of_its_room() {
        // 6,143 recorded + 2,048 room = 8,191 holdable; 75% is 6,143.25.
        assert_eq!(usage(6_143, 0).warning(LIMITS, EarlierAttempts::None), None);
        assert_eq!(
            usage(6_144, 0)
                .warning(LIMITS, EarlierAttempts::None)
                .unwrap(),
            "Warning: this attempt has 6,144 checkpoints, and `rustrace submit` has room for only 2,047 more. Submit it soon; to keep working after that, use `rustrace revise`."
        );
        assert!(
            usage(0, 900_000)
                .warning(LIMITS, EarlierAttempts::None)
                .unwrap()
                .ends_with("Submit it soon and tell your course staff: a linked attempt would not add room."),
            "one submission holds as many events as one attempt"
        );
    }

    #[test]
    fn a_linked_revision_counts_what_its_earlier_attempts_hold() {
        let earlier = EarlierAttempts::Known(ChainTotals {
            segments: 1,
            entries: 8_500,
            events: 100_000,
            ..ChainTotals::default()
        });
        let lines = usage(1_000, 5_000).status_lines(LIMITS, earlier);
        assert_eq!(
            lines[0],
            "Earlier attempts: 1, already holding 8,500 of the 16,384 files and 100,000 of the 1,000,000 events one submission can hold"
        );
        // 16,384 - 2 - 8,500 - 100 - 1 - 1,000 = 6,781 < 8,192 - 1 - 1,000.
        assert_eq!(
            lines[1],
            "Checkpoints: 1,000 recorded; room for 6,781 more before `rustrace submit` stops"
        );
        assert_eq!(
            lines[2],
            "Events: 5,000 recorded; room for 894,997 more before `rustrace submit` stops"
        );
        // Near the chain-wide limit a further linked attempt adds no room.
        assert!(
            usage(6_000, 0)
                .warning(LIMITS, earlier)
                .unwrap()
                .ends_with("a linked attempt would not add room.")
        );
        assert_eq!(
            usage(1_000, 0).status_lines(LIMITS, EarlierAttempts::Unreadable)[0],
            "Earlier attempts: unreadable, so the room below counts this attempt alone"
        );
    }

    #[test]
    fn status_lines_show_each_count_with_its_room() {
        assert_eq!(
            usage(1_234, 20_345).status_lines(LIMITS, EarlierAttempts::None),
            [
                "Checkpoints: 1,234 recorded; room for 6,957 more before `rustrace submit` stops",
                "Events: 20,345 recorded; room for 979,652 more before `rustrace submit` stops",
                "Launches: 70 recorded; a submission keeps at most 64 of their toolchain observations (the first, the last, and each change)",
            ]
        );
    }
}
