use std::{
    error::Error,
    fmt, io,
    num::NonZeroU64,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
};

use chrono::{DateTime, Utc};
use rustrace_model::{Event, Hash, SessionId};

use crate::{CheckpointInput, CheckpointSnapshot, Journal, JournalError};

pub const MAX_JOURNAL_WRITE_QUEUE_CAPACITY: usize = 16;
static NEXT_JOURNAL_WRITE_ATTEMPT: AtomicU64 = AtomicU64::new(1);

/// Opaque process-local identity for one accepted journal-writer submission.
/// Values are unique across writer instances and never wrap or get reused.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct JournalWriteAttemptId(NonZeroU64);

fn next_attempt_id(counter: &AtomicU64) -> Option<JournalWriteAttemptId> {
    let value = counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, checked_add_one)
        .ok()?;
    NonZeroU64::new(value).map(JournalWriteAttemptId)
}

const fn checked_add_one(value: u64) -> Option<u64> {
    value.checked_add(1)
}

/// The complete owned checkpoint input crossing the writer-thread boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointSubmission {
    pub monotonic_millis: u64,
    pub wall_clock_utc: Option<DateTime<Utc>>,
    pub input: CheckpointInput,
}

/// An ordinary event whose sequence and chain hashes are assigned by the
/// journal writer in its authoritative append transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventSubmission {
    pub session_id: SessionId,
    pub monotonic_millis: u64,
    pub wall_clock_utc: Option<DateTime<Utc>>,
    pub event: Event,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JournalWriteJob {
    Event(EventSubmission),
    /// Exactly two ordinary events in one transaction and one FIFO slot.
    /// The persisted receipt identifies the second event, the committed tail.
    EventPair([EventSubmission; 2]),
    Checkpoint(CheckpointSubmission),
}

impl JournalWriteJob {
    pub const fn kind(&self) -> JournalWriteKind {
        match self {
            Self::Event(_) | Self::EventPair(_) => JournalWriteKind::Event,
            Self::Checkpoint(_) => JournalWriteKind::Checkpoint,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalWriteKind {
    Event,
    Checkpoint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JournalWriteCompletion {
    Persisted {
        attempt_id: JournalWriteAttemptId,
        kind: JournalWriteKind,
        sequence: u64,
        event_hash: Hash,
    },
    Failed {
        attempt_id: JournalWriteAttemptId,
        kind: JournalWriteKind,
        detail: String,
    },
}

impl JournalWriteCompletion {
    pub const fn kind(&self) -> JournalWriteKind {
        match self {
            Self::Persisted { kind, .. } | Self::Failed { kind, .. } => *kind,
        }
    }

    pub const fn attempt_id(&self) -> JournalWriteAttemptId {
        match self {
            Self::Persisted { attempt_id, .. } | Self::Failed { attempt_id, .. } => *attempt_id,
        }
    }

    pub const fn persisted(&self) -> bool {
        matches!(self, Self::Persisted { .. })
    }
}

pub struct JournalWriteReceipt {
    attempt_id: JournalWriteAttemptId,
    receiver: Receiver<JournalWriteCompletion>,
}

impl JournalWriteReceipt {
    /// Returns the accepted submission identity echoed by every outcome.
    pub const fn attempt_id(&self) -> JournalWriteAttemptId {
        self.attempt_id
    }

    /// Polls without blocking. `Ok(None)` means the accepted job is still
    /// pending; successful submission alone never means persisted.
    pub fn try_recv(&self) -> Result<Option<JournalWriteCompletion>, JournalWriteReceiptError> {
        match self.receiver.try_recv() {
            Ok(completion) => Ok(Some(completion)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(JournalWriteReceiptError::WorkerStopped {
                attempt_id: self.attempt_id,
            }),
        }
    }

    /// Waits for this accepted job's completion acknowledgement.
    pub fn wait(self) -> Result<JournalWriteCompletion, JournalWriteReceiptError> {
        self.receiver
            .recv()
            .map_err(|_| JournalWriteReceiptError::WorkerStopped {
                attempt_id: self.attempt_id,
            })
    }
}

struct WriteRequest {
    attempt_id: JournalWriteAttemptId,
    job: JournalWriteJob,
    completion: mpsc::Sender<JournalWriteCompletion>,
}

struct PersistedJournalWrite {
    sequence: u64,
    event_hash: Hash,
}

/// A single-owner, bounded FIFO for ordinary journal events and checkpoints.
pub struct JournalWriter {
    sender: Option<SyncSender<WriteRequest>>,
    handle: Option<JoinHandle<Result<(), JournalError>>>,
}

impl JournalWriter {
    pub fn spawn(capacity: usize, mut journal: Journal) -> Result<Self, JournalWriterStartError> {
        Self::spawn_with_processor(capacity, move |job| process_job(&mut journal, job))
    }

    fn spawn_with_processor(
        capacity: usize,
        mut processor: impl FnMut(JournalWriteJob) -> Result<PersistedJournalWrite, JournalError>
        + Send
        + 'static,
    ) -> Result<Self, JournalWriterStartError> {
        if !(1..=MAX_JOURNAL_WRITE_QUEUE_CAPACITY).contains(&capacity) {
            return Err(JournalWriterStartError::InvalidCapacity {
                actual: capacity,
                maximum: MAX_JOURNAL_WRITE_QUEUE_CAPACITY,
            });
        }
        let (sender, receiver) = mpsc::sync_channel::<WriteRequest>(capacity);
        let handle = thread::Builder::new()
            .name("rustrace-journal-writer".to_owned())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    let kind = request.job.kind();
                    match processor(request.job) {
                        Ok(persisted) => {
                            if kind == JournalWriteKind::Event || persisted.sequence > 1 {
                                process_probe(match kind {
                                    JournalWriteKind::Event => "journal-write",
                                    JournalWriteKind::Checkpoint => "checkpoint-write",
                                });
                            }
                            let _ = request.completion.send(JournalWriteCompletion::Persisted {
                                attempt_id: request.attempt_id,
                                kind,
                                sequence: persisted.sequence,
                                event_hash: persisted.event_hash,
                            });
                        }
                        Err(error) => {
                            let _ = request.completion.send(JournalWriteCompletion::Failed {
                                attempt_id: request.attempt_id,
                                kind,
                                detail: error.to_string(),
                            });
                            return Err(error);
                        }
                    }
                }
                Ok(())
            })
            .map_err(JournalWriterStartError::Spawn)?;
        Ok(Self {
            sender: Some(sender),
            handle: Some(handle),
        })
    }

    /// Attempts to queue one owned job without waiting for compression or
    /// storage. Success means queued, not persisted; observe the attempt-bound
    /// receipt. Rejection returns the job and produces no accepted attempt.
    pub fn try_submit(
        &self,
        job: JournalWriteJob,
    ) -> Result<JournalWriteReceipt, JournalWriteSubmitError> {
        let Some(sender) = &self.sender else {
            return Err(JournalWriteSubmitError::Closed(Box::new(job)));
        };
        let Some(attempt_id) = next_attempt_id(&NEXT_JOURNAL_WRITE_ATTEMPT) else {
            return Err(JournalWriteSubmitError::AttemptIdsExhausted(Box::new(job)));
        };
        let (completion, receiver) = mpsc::channel();
        let request = WriteRequest {
            attempt_id,
            job,
            completion,
        };
        match sender.try_send(request) {
            Ok(()) => Ok(JournalWriteReceipt {
                attempt_id,
                receiver,
            }),
            Err(TrySendError::Full(request)) => {
                Err(JournalWriteSubmitError::QueueFull(Box::new(request.job)))
            }
            Err(TrySendError::Disconnected(request)) => {
                Err(JournalWriteSubmitError::Closed(Box::new(request.job)))
            }
        }
    }

    pub fn try_submit_event(
        &self,
        submission: EventSubmission,
    ) -> Result<JournalWriteReceipt, JournalWriteSubmitError> {
        self.try_submit(JournalWriteJob::Event(submission))
    }

    pub fn try_submit_checkpoint(
        &self,
        submission: CheckpointSubmission,
    ) -> Result<JournalWriteReceipt, JournalWriteSubmitError> {
        self.try_submit(JournalWriteJob::Checkpoint(submission))
    }

    /// Stops accepting jobs. Already accepted jobs remain owned by the worker.
    pub fn close(&mut self) {
        self.sender.take();
    }

    /// Closes submission, drains accepted production jobs in FIFO order, and
    /// waits for the worker so storage failures and panics are reported.
    pub fn shutdown(mut self) -> Result<(), JournalWriterError> {
        self.close();
        let handle = self.handle.take().ok_or(JournalWriterError::Panicked)?;
        handle
            .join()
            .map_err(|_| JournalWriterError::Panicked)?
            .map_err(JournalWriterError::Processor)
    }
}

// Dedicated probe builds only. Ordinary production builds do not inspect
// environment variables or pause the journal writer.
#[cfg(feature = "process-probes")]
fn process_probe(stage: &str) {
    if std::env::var("RUSTRACE_CRASH_STAGE").as_deref() != Ok(stage) {
        return;
    }
    let marker = std::env::var_os("RUSTRACE_CRASH_MARKER")
        .expect("crash probe requires RUSTRACE_CRASH_MARKER");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
        .expect("crash probe marker must be created once");
    use std::io::Write;
    writeln!(file, "{stage} {}", std::process::id()).expect("crash probe marker write");
    file.sync_all().expect("crash probe marker sync");
    loop {
        std::thread::park_timeout(std::time::Duration::from_secs(60));
    }
}

#[cfg(not(feature = "process-probes"))]
fn process_probe(_: &str) {}

impl Drop for JournalWriter {
    fn drop(&mut self) {
        self.sender.take();
        self.handle.take();
    }
}

fn process_job(
    journal: &mut Journal,
    job: JournalWriteJob,
) -> Result<PersistedJournalWrite, JournalError> {
    let envelope = match job {
        JournalWriteJob::Event(submission) => journal.append_submitted_event(submission)?,
        JournalWriteJob::EventPair(submissions) => {
            let [_, tail] = journal.append_submitted_event_pair(submissions)?;
            tail
        }
        JournalWriteJob::Checkpoint(submission) => {
            let sequence = journal
                .inspect_session(&submission.input.session_id)?
                .next_sequence;
            let snapshot =
                CheckpointSnapshot::from_input(submission.input, sequence).map_err(|source| {
                    JournalError::InvalidCheckpoint {
                        source: Box::new(source),
                    }
                })?;
            journal.append_checkpoint(
                snapshot.session_id(),
                submission.monotonic_millis,
                submission.wall_clock_utc,
                &snapshot,
            )?
        }
    };
    Ok(PersistedJournalWrite {
        sequence: envelope.sequence,
        event_hash: envelope.event_hash,
    })
}

#[derive(Debug)]
pub enum JournalWriteSubmitError {
    QueueFull(Box<JournalWriteJob>),
    Closed(Box<JournalWriteJob>),
    AttemptIdsExhausted(Box<JournalWriteJob>),
}

impl fmt::Display for JournalWriteSubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueFull(_) => formatter.write_str("journal writer queue is full"),
            Self::Closed(_) => formatter.write_str("journal writer queue is closed"),
            Self::AttemptIdsExhausted(_) => {
                formatter.write_str("journal writer attempt identities are exhausted")
            }
        }
    }
}

impl Error for JournalWriteSubmitError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalWriteReceiptError {
    WorkerStopped { attempt_id: JournalWriteAttemptId },
}

impl JournalWriteReceiptError {
    pub const fn attempt_id(self) -> JournalWriteAttemptId {
        match self {
            Self::WorkerStopped { attempt_id } => attempt_id,
        }
    }
}

impl fmt::Display for JournalWriteReceiptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("journal writer stopped before acknowledging the job")
    }
}

impl Error for JournalWriteReceiptError {}

#[derive(Debug)]
pub enum JournalWriterStartError {
    InvalidCapacity { actual: usize, maximum: usize },
    Spawn(io::Error),
}

impl fmt::Display for JournalWriterStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCapacity { actual, maximum } => write!(
                formatter,
                "journal writer queue capacity {actual} is outside 1..={maximum}"
            ),
            Self::Spawn(error) => write!(formatter, "failed to spawn journal writer: {error}"),
        }
    }
}

impl Error for JournalWriterStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            Self::InvalidCapacity { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum JournalWriterError {
    Processor(JournalError),
    Panicked,
}

impl fmt::Display for JournalWriterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Processor(error) => write!(formatter, "journal writer failed: {error}"),
            Self::Panicked => formatter.write_str("journal writer panicked"),
        }
    }
}

impl Error for JournalWriterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Processor(error) => Some(error),
            Self::Panicked => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use rustrace_model::{DocumentId, FileFocused};

    use super::*;
    use crate::{
        CheckpointPolicy, CheckpointScheduleError, CheckpointScheduleState, CheckpointTrigger,
    };

    fn job(marker: &str) -> JournalWriteJob {
        JournalWriteJob::Event(EventSubmission {
            session_id: SessionId::new("worker-test").unwrap(),
            monotonic_millis: 1,
            wall_clock_utc: None,
            event: Event::FileFocused(FileFocused {
                document_id: DocumentId::new(marker).unwrap(),
            }),
        })
    }

    fn persisted() -> PersistedJournalWrite {
        PersistedJournalWrite {
            sequence: 1,
            event_hash: Hash::zero(),
        }
    }

    fn pair_job() -> JournalWriteJob {
        let JournalWriteJob::Event(first) = job("pair-first") else {
            unreachable!()
        };
        let JournalWriteJob::Event(second) = job("pair-second") else {
            unreachable!()
        };
        JournalWriteJob::EventPair([first, second])
    }

    #[test]
    fn event_pair_is_one_fifo_job_and_receipt_names_committed_tail() {
        let id = SessionId::new("worker-test").unwrap();
        let mut journal = Journal::open_in_memory().unwrap();
        journal.create_or_resume_session(&id).unwrap();
        let (events_tx, events_rx) = mpsc::channel();
        let writer = JournalWriter::spawn_with_processor(3, move |job| {
            let result = process_job(&mut journal, job)?;
            events_tx
                .send(journal.read_events(&id, 1, 10).unwrap())
                .unwrap();
            Ok(result)
        })
        .unwrap();
        let prefix = writer.try_submit(job("prefix")).unwrap();
        let pair = writer.try_submit(pair_job()).unwrap();
        let suffix = writer.try_submit(job("suffix")).unwrap();
        let attempt = pair.attempt_id();
        let first = events_rx.recv().unwrap();
        let through_pair = events_rx.recv().unwrap();
        let all = events_rx.recv().unwrap();
        assert_eq!([first.len(), through_pair.len(), all.len()], [1, 3, 4]);
        assert_eq!(
            all.iter().map(|event| event.sequence).collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(through_pair[1].previous_event_hash, first[0].event_hash);
        assert_eq!(
            through_pair[2].previous_event_hash,
            through_pair[1].event_hash
        );
        assert_eq!(all[3].previous_event_hash, through_pair[2].event_hash);
        assert!(matches!(
            prefix.wait().unwrap(),
            JournalWriteCompletion::Persisted { sequence: 1, .. }
        ));
        assert_eq!(
            pair.wait().unwrap(),
            JournalWriteCompletion::Persisted {
                attempt_id: attempt,
                kind: JournalWriteKind::Event,
                sequence: 3,
                event_hash: through_pair[2].event_hash,
            }
        );
        assert!(matches!(
            suffix.wait().unwrap(),
            JournalWriteCompletion::Persisted { sequence: 4, .. }
        ));
        writer.shutdown().unwrap();
    }

    #[test]
    fn second_pair_insert_failure_has_one_failed_receipt_and_no_durable_cursor_advance() {
        let id = SessionId::new("worker-test").unwrap();
        let mut journal = Journal::open_in_memory().unwrap();
        journal.create_or_resume_session(&id).unwrap();
        let (rollback_tx, rollback_rx) = mpsc::channel();
        let writer = JournalWriter::spawn_with_processor(1, move |job| {
            let JournalWriteJob::EventPair(submissions) = job else {
                panic!("expected pair job")
            };
            let result =
                journal.append_submitted_event_pair_with_hook(submissions, |transaction, first| {
                    assert_eq!(first.sequence, 1);
                    transaction
                        .execute_batch(
                            "CREATE TRIGGER fail_second_pair_insert BEFORE INSERT ON events
                     WHEN NEW.sequence = 2
                     BEGIN SELECT RAISE(ABORT, 'injected second pair INSERT failure'); END;",
                        )
                        .unwrap();
                    Ok(())
                });
            rollback_tx
                .send((
                    journal.inspect_session(&id).unwrap().next_sequence,
                    journal.read_events(&id, 1, 10).unwrap(),
                ))
                .unwrap();
            result.map(|[_, tail]| PersistedJournalWrite {
                sequence: tail.sequence,
                event_hash: tail.event_hash,
            })
        })
        .unwrap();
        let receipt = writer.try_submit(pair_job()).unwrap();
        let attempt = receipt.attempt_id();
        let completion = receipt.wait().unwrap();
        let JournalWriteCompletion::Failed {
            attempt_id,
            kind,
            detail,
        } = completion
        else {
            panic!("second INSERT failure must never acknowledge persistence");
        };
        assert_eq!(attempt_id, attempt);
        assert_eq!(kind, JournalWriteKind::Event);
        assert!(detail.contains("injected second pair INSERT failure"));
        let (next_sequence, events) = rollback_rx.recv().unwrap();
        assert_eq!(next_sequence, 1);
        assert!(events.is_empty());
        assert!(matches!(
            writer.shutdown(),
            Err(JournalWriterError::Processor(_))
        ));
    }

    fn checkpoint_job(marker: &str) -> JournalWriteJob {
        JournalWriteJob::Checkpoint(CheckpointSubmission {
            monotonic_millis: 1,
            wall_clock_utc: None,
            input: CheckpointInput {
                session_id: SessionId::new(marker).unwrap(),
                files: vec![],
                active_document: None,
                documents: vec![],
            },
        })
    }

    fn stopped_checkpoint_error(marker: &str) -> JournalWriteReceiptError {
        let writer = JournalWriter::spawn_with_processor(1, |_job| panic!("test panic")).unwrap();
        let receipt = writer.try_submit(checkpoint_job(marker)).unwrap();
        let error = receipt.wait().unwrap_err();
        assert!(matches!(
            writer.shutdown(),
            Err(JournalWriterError::Panicked)
        ));
        error
    }

    #[test]
    fn submission_is_nonblocking_and_the_queue_is_bounded() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut first = true;
        let writer = JournalWriter::spawn_with_processor(1, move |_job| {
            started_tx.send(()).unwrap();
            if first {
                first = false;
                release_rx.recv().unwrap();
            }
            Ok(persisted())
        })
        .unwrap();

        let first = writer.try_submit(job("first")).unwrap();
        started_rx.recv().unwrap();
        let second = writer.try_submit(pair_job()).unwrap();
        assert_ne!(first.attempt_id(), second.attempt_id());
        let third = pair_job();
        let returned = match writer.try_submit(third.clone()) {
            Err(JournalWriteSubmitError::QueueFull(returned)) => returned,
            _ => panic!("third job should be returned from the full queue"),
        };
        assert_eq!(*returned, third);
        assert_eq!(first.try_recv().unwrap(), None);
        release_tx.send(()).unwrap();
        let first_attempt = first.attempt_id();
        let second_attempt = second.attempt_id();
        let first_completion = first.wait().unwrap();
        let second_completion = second.wait().unwrap();
        assert!(first_completion.persisted());
        assert!(second_completion.persisted());
        assert_eq!(first_completion.attempt_id(), first_attempt);
        assert_eq!(second_completion.attempt_id(), second_attempt);
        writer.shutdown().unwrap();
    }

    #[test]
    fn drop_closes_and_detaches_without_waiting_for_a_blocked_processor() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let writer = JournalWriter::spawn_with_processor(1, move |_job| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            finished_tx.send(()).unwrap();
            Ok(persisted())
        })
        .unwrap();
        let receipt = writer.try_submit(job("blocked")).unwrap();
        started_rx.recv().unwrap();

        drop(writer);
        assert_eq!(receipt.try_recv().unwrap(), None);
        release_tx.send(()).unwrap();
        finished_rx.recv().unwrap();
        assert!(receipt.wait().unwrap().persisted());
    }

    #[test]
    fn explicit_shutdown_reports_a_processor_panic() {
        let writer = JournalWriter::spawn_with_processor(1, |_job| panic!("test panic")).unwrap();
        let receipt = writer.try_submit(job("panic")).unwrap();
        let attempt_id = receipt.attempt_id();
        assert_eq!(receipt.wait().unwrap_err().attempt_id(), attempt_id);
        assert!(matches!(
            writer.shutdown(),
            Err(JournalWriterError::Panicked)
        ));
    }

    #[test]
    fn attempt_identity_generation_never_wraps() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert!(next_attempt_id(&counter).is_some());
        assert!(next_attempt_id(&counter).is_none());
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn processor_failure_and_disconnected_fanout_keep_attempt_identities() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = JournalWriter::spawn_with_processor(2, move |_job| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Err(JournalError::Io {
                operation: "test processor",
                source: io::Error::other("test failure"),
            })
        })
        .unwrap();
        let first = writer.try_submit(job("failure")).unwrap();
        started_rx.recv().unwrap();
        let second = writer.try_submit(job("discarded")).unwrap();
        let first_attempt = first.attempt_id();
        let second_attempt = second.attempt_id();
        release_tx.send(()).unwrap();

        let failure = first.wait().unwrap();
        assert!(matches!(failure, JournalWriteCompletion::Failed { .. }));
        assert_eq!(failure.attempt_id(), first_attempt);
        assert_eq!(second.wait().unwrap_err().attempt_id(), second_attempt);
        assert!(matches!(
            writer.shutdown(),
            Err(JournalWriterError::Processor(_))
        ));
    }

    #[test]
    fn stopped_worker_errors_only_resolve_their_checkpoint_attempt() {
        let first_error = stopped_checkpoint_error("stopped-first");
        let second_error = stopped_checkpoint_error("stopped-second");
        assert_ne!(first_error.attempt_id(), second_error.attempt_id());

        let policy = CheckpointPolicy::new(Duration::from_secs(30), 1).unwrap();
        let mut state = CheckpointScheduleState::new(policy, Duration::ZERO);
        state.record_edit_events(1);
        state
            .checkpoint_queued(Duration::from_secs(30), first_error.attempt_id())
            .unwrap();
        state.apply_receipt_result(Err(first_error)).unwrap();

        state.record_edit_events(2);
        state
            .checkpoint_queued(Duration::from_secs(40), second_error.attempt_id())
            .unwrap();
        assert!(matches!(
            state.apply_receipt_result(Err(first_error)),
            Err(CheckpointScheduleError::AttemptMismatch { .. })
        ));
        assert!(state.is_pending());
        assert_eq!(state.edit_events_since_checkpoint(), 3);
        state.apply_receipt_result(Err(second_error)).unwrap();
        assert!(state.should_submit(Duration::from_secs(40), CheckpointTrigger::Activity));
    }
}
