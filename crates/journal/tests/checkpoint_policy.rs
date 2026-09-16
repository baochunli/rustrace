use std::{fs, time::Duration};

use rustrace_journal::{
    CheckpointFile, CheckpointInput, CheckpointPolicy, CheckpointScheduleError,
    CheckpointScheduleState, CheckpointSubmission, CheckpointTrigger, EventSubmission, Journal,
    JournalWriteCompletion, JournalWriteJob, JournalWriteKind, JournalWriteReceiptError,
    JournalWriteSubmitError, JournalWriter, JournalWriterError, MAX_JOURNAL_WRITE_QUEUE_CAPACITY,
    OpenDocument,
};
use rustrace_model::{
    DocumentId, Event, FileFocused, Hash, SelectionState, SessionId, WorkspaceCheckpoint,
    WorkspacePath,
};

fn checkpoint_submission(sequence: u64) -> CheckpointSubmission {
    let path = WorkspacePath::new("src/lib.rs").unwrap();
    let document_id = DocumentId::new("doc").unwrap();
    CheckpointSubmission {
        monotonic_millis: sequence * 10,
        wall_clock_utc: None,
        input: CheckpointInput {
            session_id: SessionId::new("session").unwrap(),
            files: vec![CheckpointFile {
                path: path.clone(),
                contents: b"fn main() {}\n".to_vec(),
            }],
            active_document: Some(document_id.clone()),
            documents: vec![OpenDocument {
                document_id,
                path,
                selection: SelectionState::caret(0),
                version: 0,
            }],
        },
    }
}

fn event_submission(marker: &str) -> EventSubmission {
    EventSubmission {
        session_id: SessionId::new("session").unwrap(),
        monotonic_millis: 15,
        wall_clock_utc: None,
        event: Event::FileFocused(FileFocused {
            document_id: DocumentId::new(marker).unwrap(),
        }),
    }
}

fn journal_path(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let directory = std::env::temp_dir().join(format!(
        "rustrace-journal-writer-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir(&directory).unwrap();
    let path = directory.join("journal.sqlite3");
    (directory, path)
}

fn completed_checkpoint_attempts(name: &str, count: usize) -> Vec<JournalWriteCompletion> {
    let (directory, path) = journal_path(name);
    let id = SessionId::new("session").unwrap();
    let mut journal = Journal::create(&path).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    let writer = JournalWriter::spawn(count.max(1), journal).unwrap();
    let receipts = (0..count)
        .map(|index| {
            writer
                .try_submit_checkpoint(checkpoint_submission(index as u64 + 1))
                .unwrap()
        })
        .collect::<Vec<_>>();
    let completions = receipts
        .into_iter()
        .map(|receipt| receipt.wait().unwrap())
        .collect();
    writer.shutdown().unwrap();
    fs::remove_dir_all(directory).unwrap();
    completions
}

fn completed_event_attempt(name: &str) -> JournalWriteCompletion {
    let (directory, path) = journal_path(name);
    let id = SessionId::new("session").unwrap();
    let mut journal = Journal::create(&path).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    let writer = JournalWriter::spawn(1, journal).unwrap();
    let completion = writer
        .try_submit_event(event_submission("event-attempt"))
        .unwrap()
        .wait()
        .unwrap();
    writer.shutdown().unwrap();
    fs::remove_dir_all(directory).unwrap();
    completion
}

#[test]
fn policy_accepts_only_thirty_to_sixty_seconds_and_positive_thresholds() {
    for seconds in [30, 45, 60] {
        assert!(CheckpointPolicy::new(Duration::from_secs(seconds), 1).is_ok());
    }
    for seconds in [29, 61] {
        assert!(matches!(
            CheckpointPolicy::new(Duration::from_secs(seconds), 1),
            Err(CheckpointScheduleError::IntervalOutOfRange { .. })
        ));
    }
    assert!(matches!(
        CheckpointPolicy::new(Duration::from_secs(30), 0),
        Err(CheckpointScheduleError::ZeroEditThreshold)
    ));
}

#[test]
fn policy_triggers_on_either_limit_and_unconditionally_at_boundaries() {
    let policy = CheckpointPolicy::new(Duration::from_secs(45), 10).unwrap();
    assert!(!policy.should_checkpoint(Duration::from_secs(44), 9, CheckpointTrigger::Activity));
    assert!(policy.should_checkpoint(Duration::from_secs(45), 0, CheckpointTrigger::Activity));
    assert!(policy.should_checkpoint(Duration::ZERO, 10, CheckpointTrigger::Activity));
    for trigger in [
        CheckpointTrigger::BeforeCargo,
        CheckpointTrigger::AfterCargo,
        CheckpointTrigger::Finalization,
    ] {
        assert!(policy.should_checkpoint(Duration::ZERO, 0, trigger));
    }
}

#[test]
fn schedule_state_resets_only_after_a_persisted_checkpoint_receipt() {
    let completion = completed_checkpoint_attempts("schedule-reset", 1)
        .pop()
        .unwrap();
    let policy = CheckpointPolicy::new(Duration::from_secs(45), 10).unwrap();
    let mut state = CheckpointScheduleState::new(policy, Duration::from_secs(100));
    state.record_edit_events(10);
    assert!(state.is_due(Duration::from_secs(110), CheckpointTrigger::Activity));
    assert!(state.should_submit(Duration::from_secs(110), CheckpointTrigger::Activity));

    state
        .checkpoint_queued(Duration::from_secs(110), completion.attempt_id())
        .unwrap();
    assert!(state.is_pending());
    assert_eq!(state.edit_events_since_checkpoint(), 10);
    assert!(state.is_due(Duration::from_secs(110), CheckpointTrigger::Activity));
    assert!(!state.should_submit(Duration::from_secs(110), CheckpointTrigger::Activity));

    state.apply_receipt_result(Ok(completion)).unwrap();
    assert!(!state.is_pending());
    assert_eq!(state.edit_events_since_checkpoint(), 0);
    assert!(!state.is_due(Duration::from_secs(154), CheckpointTrigger::Activity));
    assert!(state.is_due(Duration::from_secs(155), CheckpointTrigger::Activity));
}

#[test]
fn edits_recorded_while_checkpoint_is_pending_survive_success() {
    let completion = completed_checkpoint_attempts("pending-edits", 1)
        .pop()
        .unwrap();
    let policy = CheckpointPolicy::new(Duration::from_secs(45), 10).unwrap();
    let mut state = CheckpointScheduleState::new(policy, Duration::ZERO);
    state.record_edit_events(10);
    state
        .checkpoint_queued(Duration::from_secs(10), completion.attempt_id())
        .unwrap();
    state.record_edit_events(7);

    state.apply_receipt_result(Ok(completion)).unwrap();

    assert_eq!(state.edit_events_since_checkpoint(), 7);
}

#[test]
fn pending_processing_time_counts_from_checkpoint_capture() {
    let completion = completed_checkpoint_attempts("pending-time", 1)
        .pop()
        .unwrap();
    let policy = CheckpointPolicy::new(Duration::from_secs(30), 100).unwrap();
    let mut state = CheckpointScheduleState::new(policy, Duration::from_secs(100));
    assert!(state.is_due(Duration::from_secs(130), CheckpointTrigger::Activity));
    state
        .checkpoint_queued(Duration::from_secs(130), completion.attempt_id())
        .unwrap();

    state.apply_receipt_result(Ok(completion)).unwrap();

    assert!(state.is_due(Duration::from_secs(160), CheckpointTrigger::Activity));
}

#[test]
fn schedule_failure_remains_due_and_time_and_counters_saturate() {
    let attempt_id = completed_checkpoint_attempts("saturated-failure", 1)[0].attempt_id();
    let policy = CheckpointPolicy::new(Duration::from_secs(30), u64::MAX).unwrap();
    let mut state = CheckpointScheduleState::new(policy, Duration::MAX);
    state.record_edit_events(u64::MAX - 1);
    state.record_edit_events(10);
    assert_eq!(state.edit_events_since_checkpoint(), u64::MAX);
    assert!(state.is_due(Duration::ZERO, CheckpointTrigger::Activity));

    state.checkpoint_queued(Duration::ZERO, attempt_id).unwrap();
    state.record_edit_events(10);
    state
        .apply_receipt_result(Ok(JournalWriteCompletion::Failed {
            attempt_id,
            kind: JournalWriteKind::Checkpoint,
            detail: "storage failed".to_owned(),
        }))
        .unwrap();
    assert!(!state.is_pending());
    assert!(state.should_submit(Duration::ZERO, CheckpointTrigger::Activity));

    for trigger in [
        CheckpointTrigger::BeforeCargo,
        CheckpointTrigger::AfterCargo,
        CheckpointTrigger::Finalization,
    ] {
        assert!(state.is_due(Duration::ZERO, trigger));
    }
}

#[test]
fn failed_checkpoint_completion_preserves_every_edit() {
    let attempt_id = completed_checkpoint_attempts("failed-edits", 1)[0].attempt_id();
    let policy = CheckpointPolicy::new(Duration::from_secs(30), 10).unwrap();
    let mut state = CheckpointScheduleState::new(policy, Duration::ZERO);
    state.record_edit_events(10);
    state
        .checkpoint_queued(Duration::from_secs(30), attempt_id)
        .unwrap();
    state.record_edit_events(7);
    state
        .apply_receipt_result(Ok(JournalWriteCompletion::Failed {
            attempt_id,
            kind: JournalWriteKind::Checkpoint,
            detail: "storage failed".to_owned(),
        }))
        .unwrap();

    assert_eq!(state.edit_events_since_checkpoint(), 17);
    assert!(state.should_submit(Duration::from_secs(30), CheckpointTrigger::Activity));
}

#[test]
fn saturated_capture_retains_post_capture_edits_after_success() {
    let completion = completed_checkpoint_attempts("saturated-success", 1)
        .pop()
        .unwrap();
    let policy = CheckpointPolicy::new(Duration::from_secs(30), u64::MAX).unwrap();
    let mut state = CheckpointScheduleState::new(policy, Duration::ZERO);
    state.record_edit_events(u64::MAX);
    state
        .checkpoint_queued(Duration::from_secs(30), completion.attempt_id())
        .unwrap();
    state.record_edit_events(7);
    state.apply_receipt_result(Ok(completion)).unwrap();
    assert_eq!(state.edit_events_since_checkpoint(), 7);
}

#[test]
fn ordinary_completion_and_worker_stop_preserve_pending_edits() {
    let checkpoint_attempt = completed_checkpoint_attempts("wrong-kind-checkpoint", 1)
        .pop()
        .unwrap();
    let event_completion = completed_event_attempt("wrong-kind-event");
    let policy = CheckpointPolicy::new(Duration::from_secs(30), 10).unwrap();
    let mut state = CheckpointScheduleState::new(policy, Duration::ZERO);
    state.record_edit_events(10);
    state
        .checkpoint_queued(Duration::from_secs(30), checkpoint_attempt.attempt_id())
        .unwrap();
    state.record_edit_events(7);

    assert!(matches!(
        state.apply_receipt_result(Ok(event_completion)),
        Err(CheckpointScheduleError::WrongCompletionKind)
    ));
    assert!(state.is_pending());
    assert_eq!(state.edit_events_since_checkpoint(), 17);

    state
        .apply_receipt_result(Err(JournalWriteReceiptError::WorkerStopped {
            attempt_id: checkpoint_attempt.attempt_id(),
        }))
        .unwrap();
    assert!(!state.is_pending());
    assert_eq!(state.edit_events_since_checkpoint(), 17);
    assert!(state.should_submit(Duration::from_secs(30), CheckpointTrigger::Activity));
}

#[test]
fn stale_checkpoint_completion_cannot_resolve_a_newer_pending_checkpoint() {
    let mut completions = completed_checkpoint_attempts("stale-completion", 2).into_iter();
    let first = completions.next().unwrap();
    let second = completions.next().unwrap();
    let policy = CheckpointPolicy::new(Duration::from_secs(30), 10).unwrap();
    let mut state = CheckpointScheduleState::new(policy, Duration::ZERO);
    state.record_edit_events(10);
    state
        .checkpoint_queued(Duration::from_secs(30), first.attempt_id())
        .unwrap();
    state.apply_receipt_result(Ok(first.clone())).unwrap();

    state.record_edit_events(3);
    state
        .checkpoint_queued(Duration::from_secs(40), second.attempt_id())
        .unwrap();
    state.record_edit_events(2);
    assert!(matches!(
        state.apply_receipt_result(Ok(first.clone())),
        Err(CheckpointScheduleError::AttemptMismatch { expected, actual })
            if expected == second.attempt_id() && actual == first.attempt_id()
    ));
    assert!(state.is_pending());
    assert_eq!(state.edit_events_since_checkpoint(), 5);

    state
        .apply_receipt_result(Ok(JournalWriteCompletion::Failed {
            attempt_id: second.attempt_id(),
            kind: JournalWriteKind::Checkpoint,
            detail: "second checkpoint failed".to_owned(),
        }))
        .unwrap();
    assert!(!state.is_pending());
    assert_eq!(state.edit_events_since_checkpoint(), 5);
    assert!(state.should_submit(Duration::from_secs(40), CheckpointTrigger::Activity));
}

#[test]
fn different_accepted_checkpoint_completion_cannot_resolve_pending_state() {
    let (directory, path) = journal_path("different-attempt");
    let id = SessionId::new("session").unwrap();
    let mut journal = Journal::create(&path).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    let writer = JournalWriter::spawn(2, journal).unwrap();
    let first = writer
        .try_submit_checkpoint(checkpoint_submission(1))
        .unwrap();
    let second = writer
        .try_submit_checkpoint(checkpoint_submission(2))
        .unwrap();
    let first_completion = first.wait().unwrap();
    let second_completion = second.wait().unwrap();

    let policy = CheckpointPolicy::new(Duration::from_secs(30), 1).unwrap();
    let mut state = CheckpointScheduleState::new(policy, Duration::ZERO);
    state.record_edit_events(1);
    state
        .checkpoint_queued(Duration::from_secs(30), second_completion.attempt_id())
        .unwrap();
    assert!(matches!(
        state.apply_receipt_result(Ok(first_completion)),
        Err(CheckpointScheduleError::AttemptMismatch { .. })
    ));
    assert!(state.is_pending());
    state.apply_receipt_result(Ok(second_completion)).unwrap();
    assert!(!state.is_due(Duration::from_secs(59), CheckpointTrigger::Activity));
    assert!(state.is_due(Duration::from_secs(60), CheckpointTrigger::Activity));

    writer.shutdown().unwrap();
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn completion_from_another_writer_cannot_resolve_pending_state() {
    let mut completions = Vec::new();
    let mut directories = Vec::new();
    for name in ["foreign-a", "foreign-b"] {
        let (directory, path) = journal_path(name);
        let id = SessionId::new("session").unwrap();
        let mut journal = Journal::create(&path).unwrap();
        journal.create_or_resume_session(&id).unwrap();
        let writer = JournalWriter::spawn(1, journal).unwrap();
        let receipt = writer
            .try_submit_checkpoint(checkpoint_submission(1))
            .unwrap();
        completions.push(receipt.wait().unwrap());
        writer.shutdown().unwrap();
        directories.push(directory);
    }

    let policy = CheckpointPolicy::new(Duration::from_secs(30), 1).unwrap();
    let mut state = CheckpointScheduleState::new(policy, Duration::ZERO);
    state.record_edit_events(1);
    state
        .checkpoint_queued(Duration::from_secs(30), completions[1].attempt_id())
        .unwrap();
    assert_ne!(completions[0].attempt_id(), completions[1].attempt_id());
    assert!(matches!(
        state.apply_receipt_result(Ok(completions[0].clone())),
        Err(CheckpointScheduleError::AttemptMismatch { .. })
    ));
    assert!(state.is_pending());
    state
        .apply_receipt_result(Ok(completions.pop().unwrap()))
        .unwrap();

    for directory in directories {
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn stale_worker_stop_cannot_resolve_a_newer_pending_checkpoint() {
    let mut completions = completed_checkpoint_attempts("stale-worker-stop", 2).into_iter();
    let first_attempt = completions.next().unwrap().attempt_id();
    let second_attempt = completions.next().unwrap().attempt_id();
    let stale_error = JournalWriteReceiptError::WorkerStopped {
        attempt_id: first_attempt,
    };
    let policy = CheckpointPolicy::new(Duration::from_secs(30), 1).unwrap();
    let mut state = CheckpointScheduleState::new(policy, Duration::ZERO);
    state.record_edit_events(1);
    state
        .checkpoint_queued(Duration::from_secs(30), first_attempt)
        .unwrap();
    state.apply_receipt_result(Err(stale_error)).unwrap();

    state.record_edit_events(2);
    state
        .checkpoint_queued(Duration::from_secs(40), second_attempt)
        .unwrap();
    assert!(matches!(
        state.apply_receipt_result(Err(stale_error)),
        Err(CheckpointScheduleError::AttemptMismatch { .. })
    ));
    assert!(state.is_pending());
    assert_eq!(state.edit_events_since_checkpoint(), 3);
    state
        .apply_receipt_result(Err(JournalWriteReceiptError::WorkerStopped {
            attempt_id: second_attempt,
        }))
        .unwrap();
    assert!(!state.is_pending());
    assert_eq!(state.edit_events_since_checkpoint(), 3);
    assert!(state.should_submit(Duration::from_secs(40), CheckpointTrigger::Activity));
}

#[test]
fn consumed_checkpoint_completion_cannot_be_reapplied() {
    let completion = completed_checkpoint_attempts("consumed-completion", 1)
        .pop()
        .unwrap();
    let policy = CheckpointPolicy::new(Duration::from_secs(30), 1).unwrap();
    let mut state = CheckpointScheduleState::new(policy, Duration::ZERO);
    state.record_edit_events(1);
    state
        .checkpoint_queued(Duration::from_secs(30), completion.attempt_id())
        .unwrap();
    state.apply_receipt_result(Ok(completion.clone())).unwrap();
    assert!(matches!(
        state.apply_receipt_result(Ok(completion)),
        Err(CheckpointScheduleError::NoCheckpointPending)
    ));
}

#[test]
fn journal_writer_persists_checkpoint_and_event_jobs_in_fifo_order() {
    let (directory, path) = journal_path("fifo");
    let id = SessionId::new("session").unwrap();
    let mut journal = Journal::create(&path).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    let writer = JournalWriter::spawn(3, journal).unwrap();

    let first = writer
        .try_submit_checkpoint(checkpoint_submission(1))
        .unwrap();
    let second = writer.try_submit_event(event_submission("middle")).unwrap();
    let third = writer
        .try_submit_checkpoint(checkpoint_submission(3))
        .unwrap();

    for (receipt, kind, sequence) in [
        (first, JournalWriteKind::Checkpoint, 1),
        (second, JournalWriteKind::Event, 2),
        (third, JournalWriteKind::Checkpoint, 3),
    ] {
        assert!(matches!(
            receipt.wait().unwrap(),
            JournalWriteCompletion::Persisted {
                kind: actual_kind,
                sequence: actual_sequence,
                ..
            } if actual_kind == kind && actual_sequence == sequence
        ));
    }
    writer.shutdown().unwrap();

    let mut reopened = Journal::open(&path).unwrap();
    let events = reopened.read_events(&id, 1, 3).unwrap();
    assert!(matches!(events[0].event, Event::WorkspaceCheckpoint(_)));
    assert!(matches!(events[1].event, Event::FileFocused(_)));
    assert!(matches!(events[2].event, Event::WorkspaceCheckpoint(_)));
    assert_eq!(reopened.verify_session_chain(&id).unwrap().event_count, 3);
    drop(reopened);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn journal_writer_returns_owned_jobs_when_closed_and_rejects_invalid_capacity() {
    let (directory, path) = journal_path("capacity");
    let id = SessionId::new("session").unwrap();
    let mut journal = Journal::create(path).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    assert!(JournalWriter::spawn(0, journal).is_err());
    fs::remove_dir_all(directory).unwrap();

    let (directory, path) = journal_path("too-large");
    let mut journal = Journal::create(path).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    assert!(JournalWriter::spawn(MAX_JOURNAL_WRITE_QUEUE_CAPACITY + 1, journal).is_err());
    fs::remove_dir_all(directory).unwrap();

    let (directory, path) = journal_path("closed");
    let mut journal = Journal::create(path).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    let mut writer = JournalWriter::spawn(1, journal).unwrap();
    writer.close();
    assert!(matches!(
        writer.try_submit(JournalWriteJob::Event(event_submission("closed"))),
        Err(JournalWriteSubmitError::Closed(_))
    ));
    writer.shutdown().unwrap();
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn explicit_shutdown_and_receipt_surface_storage_failure() {
    let (directory, path) = journal_path("failure");
    let id = SessionId::new("session").unwrap();
    let mut journal = Journal::create(path).unwrap();
    journal.create_or_resume_session(&id).unwrap();
    let writer = JournalWriter::spawn(1, journal).unwrap();
    let invalid = EventSubmission {
        event: Event::WorkspaceCheckpoint(WorkspaceCheckpoint {
            workspace_hash: Hash::zero(),
            documents: vec![],
        }),
        ..event_submission("invalid")
    };
    let receipt = writer.try_submit_event(invalid).unwrap();
    assert!(matches!(
        receipt.wait().unwrap(),
        JournalWriteCompletion::Failed { .. }
    ));
    assert!(matches!(
        writer.shutdown(),
        Err(JournalWriterError::Processor(_))
    ));
    fs::remove_dir_all(directory).unwrap();
}
