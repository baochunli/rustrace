use std::{error::Error, fmt, time::Duration};

use crate::{
    JournalWriteAttemptId, JournalWriteCompletion, JournalWriteKind, JournalWriteReceiptError,
};

pub const MIN_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(30);
pub const MAX_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckpointPolicy {
    interval: Duration,
    edit_event_threshold: u64,
}

impl CheckpointPolicy {
    pub fn new(
        interval: Duration,
        edit_event_threshold: u64,
    ) -> Result<Self, CheckpointScheduleError> {
        if !(MIN_CHECKPOINT_INTERVAL..=MAX_CHECKPOINT_INTERVAL).contains(&interval) {
            return Err(CheckpointScheduleError::IntervalOutOfRange {
                actual: interval,
                minimum: MIN_CHECKPOINT_INTERVAL,
                maximum: MAX_CHECKPOINT_INTERVAL,
            });
        }
        if edit_event_threshold == 0 {
            return Err(CheckpointScheduleError::ZeroEditThreshold);
        }
        Ok(Self {
            interval,
            edit_event_threshold,
        })
    }

    pub const fn interval(self) -> Duration {
        self.interval
    }

    pub const fn edit_event_threshold(self) -> u64 {
        self.edit_event_threshold
    }

    /// Pure scheduling decision over caller-supplied elapsed time and count.
    pub fn should_checkpoint(
        self,
        elapsed_since_checkpoint: Duration,
        edit_events_since_checkpoint: u64,
        trigger: CheckpointTrigger,
    ) -> bool {
        trigger != CheckpointTrigger::Activity
            || elapsed_since_checkpoint >= self.interval
            || edit_events_since_checkpoint >= self.edit_event_threshold
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckpointTrigger {
    Activity,
    BeforeCargo,
    AfterCargo,
    Finalization,
}

/// Mutable checkpoint scheduling state driven only by caller-supplied time,
/// edit counts, and journal-writer completion acknowledgements.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointScheduleState {
    policy: CheckpointPolicy,
    persisted_baseline: Duration,
    edit_events_since_checkpoint: u64,
    pending: Option<PendingCheckpoint>,
    retry_due: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingCheckpoint {
    attempt_id: JournalWriteAttemptId,
    captured_at: Duration,
    captured_edit_events: u64,
    post_capture_edit_events: u64,
}

impl CheckpointScheduleState {
    pub const fn new(policy: CheckpointPolicy, persisted_baseline: Duration) -> Self {
        Self {
            policy,
            persisted_baseline,
            edit_events_since_checkpoint: 0,
            pending: None,
            retry_due: false,
        }
    }

    pub fn record_edit_events(&mut self, count: u64) {
        self.edit_events_since_checkpoint = self.edit_events_since_checkpoint.saturating_add(count);
        if let Some(pending) = &mut self.pending {
            pending.post_capture_edit_events =
                pending.post_capture_edit_events.saturating_add(count);
        }
    }

    pub const fn edit_events_since_checkpoint(&self) -> u64 {
        self.edit_events_since_checkpoint
    }

    pub const fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn is_due(&self, now: Duration, trigger: CheckpointTrigger) -> bool {
        self.retry_due
            || self.policy.should_checkpoint(
                now.saturating_sub(self.persisted_baseline),
                self.edit_events_since_checkpoint,
                trigger,
            )
    }

    pub fn should_submit(&self, now: Duration, trigger: CheckpointTrigger) -> bool {
        self.pending.is_none() && self.is_due(now, trigger)
    }

    /// Binds a successfully queued checkpoint attempt to the immutable
    /// snapshot's capture time. Later edits accrue separately until that exact
    /// attempt's receipt result is acknowledged.
    pub fn checkpoint_queued(
        &mut self,
        captured_at: Duration,
        attempt_id: JournalWriteAttemptId,
    ) -> Result<(), CheckpointScheduleError> {
        if self.pending.is_some() {
            return Err(CheckpointScheduleError::CheckpointAlreadyPending);
        }
        self.pending = Some(PendingCheckpoint {
            attempt_id,
            captured_at,
            captured_edit_events: self.edit_events_since_checkpoint,
            post_capture_edit_events: 0,
        });
        Ok(())
    }

    /// Applies the matching checkpoint job's receipt result. Durable success
    /// advances the baseline to capture time and retains later edits; a write
    /// failure or stopped worker is retryable.
    pub fn apply_receipt_result(
        &mut self,
        result: Result<JournalWriteCompletion, JournalWriteReceiptError>,
    ) -> Result<(), CheckpointScheduleError> {
        let pending = self
            .pending
            .as_ref()
            .ok_or(CheckpointScheduleError::NoCheckpointPending)?;
        if matches!(&result, Ok(completion) if completion.kind() != JournalWriteKind::Checkpoint) {
            return Err(CheckpointScheduleError::WrongCompletionKind);
        }
        let actual_attempt = match &result {
            Ok(completion) => completion.attempt_id(),
            Err(error) => error.attempt_id(),
        };
        if actual_attempt != pending.attempt_id {
            return Err(CheckpointScheduleError::AttemptMismatch {
                expected: pending.attempt_id,
                actual: actual_attempt,
            });
        }
        let pending = self
            .pending
            .take()
            .ok_or(CheckpointScheduleError::NoCheckpointPending)?;
        match result {
            Ok(JournalWriteCompletion::Persisted { .. }) => {
                self.persisted_baseline = pending.captured_at;
                self.edit_events_since_checkpoint = self
                    .edit_events_since_checkpoint
                    .saturating_sub(pending.captured_edit_events)
                    .max(pending.post_capture_edit_events);
                self.retry_due = false;
            }
            Ok(JournalWriteCompletion::Failed { .. })
            | Err(JournalWriteReceiptError::WorkerStopped { .. }) => self.retry_due = true,
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckpointScheduleError {
    IntervalOutOfRange {
        actual: Duration,
        minimum: Duration,
        maximum: Duration,
    },
    ZeroEditThreshold,
    CheckpointAlreadyPending,
    NoCheckpointPending,
    WrongCompletionKind,
    AttemptMismatch {
        expected: JournalWriteAttemptId,
        actual: JournalWriteAttemptId,
    },
}

impl fmt::Display for CheckpointScheduleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IntervalOutOfRange {
                actual,
                minimum,
                maximum,
            } => write!(
                formatter,
                "checkpoint interval {actual:?} is outside {minimum:?}..={maximum:?}"
            ),
            Self::ZeroEditThreshold => {
                formatter.write_str("checkpoint edit-event threshold must be positive")
            }
            Self::CheckpointAlreadyPending => {
                formatter.write_str("a checkpoint job is already pending")
            }
            Self::NoCheckpointPending => formatter.write_str("no checkpoint job is pending"),
            Self::WrongCompletionKind => {
                formatter.write_str("completion acknowledgement is not for a checkpoint job")
            }
            Self::AttemptMismatch { expected, actual } => write!(
                formatter,
                "checkpoint attempt mismatch: expected {expected:?}, received {actual:?}"
            ),
        }
    }
}

impl Error for CheckpointScheduleError {}
