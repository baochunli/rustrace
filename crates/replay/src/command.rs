use crate::{ReplayError, WorkspaceState};
use rustrace_model::*;

#[derive(Clone)]
pub(super) struct ActiveCommand {
    pub started: ControlledCommandStarted,
    pub millis: u64,
    pub bytes: [u64; 2],
    pub stdout: Vec<u8>,
}

#[derive(Clone)]
pub(super) struct FinishedCommand {
    pub started: ControlledCommandStarted,
    pub finished: ControlledCommandFinished,
    pub stdout: Vec<u8>,
}

#[derive(Clone)]
pub(super) struct CommandReplay {
    pub active: Option<ActiveCommand>,
    pub last_finished: Option<FinishedCommand>,
    pub checkpoint: Option<CommandTreeLink>,
    pub workspace_version: u64,
    pub output_bytes: u64,
    pub last_millis: u64,
}

fn invalid(detail: &str) -> ReplayError {
    ReplayError::NonMutatingEvent {
        event: "controlled_command",
        detail: detail.to_owned(),
    }
}

impl CommandReplay {
    pub fn new(sequence: u64, hash: Hash, tree: Hash, millis: u64) -> Self {
        Self {
            active: None,
            last_finished: None,
            checkpoint: Some(CommandTreeLink {
                checkpoint_sequence: sequence,
                checkpoint_event_hash: hash,
                workspace_hash: tree,
                workspace_version: 1,
            }),
            workspace_version: 1,
            output_bytes: 0,
            last_millis: millis,
        }
    }

    pub fn validate(
        &self,
        envelope: &EventEnvelope,
        legacy_active: bool,
        workspace: &WorkspaceState,
    ) -> Result<(), ReplayError> {
        if (self.active.is_some()
            || self.last_finished.is_some()
            || matches!(
                envelope.event,
                Event::ControlledCommandStarted(_) | Event::TestCaseCompared(_)
            ))
            && envelope.monotonic_millis < self.last_millis
        {
            return Err(invalid("command evidence time moved backwards"));
        }
        match &envelope.event {
            Event::ControlledCommandStarted(start) => {
                if self.active.is_some() || legacy_active {
                    return Err(invalid("one command owns the workspace"));
                }
                if start.command_id.as_str() != format!("command-{}", envelope.sequence)
                    || self.checkpoint.as_ref() != Some(&start.before)
                    || start.before.checkpoint_sequence + 1 != envelope.sequence
                    || start.before.checkpoint_event_hash != envelope.previous_event_hash
                    || self.output_bytes.saturating_add(start.output_limit)
                        > MAX_SESSION_COMMAND_OUTPUT_BYTES
                {
                    return Err(invalid(
                        "start identity/pre-boundary/output reserve mismatch",
                    ));
                }
            }
            Event::ControlledCommandOutput(output) => {
                let active = self
                    .active
                    .as_ref()
                    .ok_or_else(|| invalid("output without start"))?;
                if output.stream == OutputStream::Stdout
                    && active.started.console.as_ref().is_some_and(|route| {
                        matches!(route.stdout, ConsoleStdoutRoute::File { .. })
                    })
                {
                    return Err(invalid("redirected stdout cannot have captured chunks"));
                }
                let index = if output.stream == OutputStream::Stdout {
                    0
                } else {
                    1
                };
                let bytes = output.bytes_hex.len() as u64 / 2;
                if output.command_id != active.started.command_id
                    || output.offset != active.bytes[index]
                    || active.bytes[0]
                        .saturating_add(active.bytes[1])
                        .saturating_add(bytes)
                        > active.started.output_limit
                {
                    return Err(invalid("output identity/offset/limit mismatch"));
                }
            }
            Event::ControlledCommandFinished(finish) => {
                let active = self
                    .active
                    .as_ref()
                    .ok_or_else(|| invalid("finish without start"))?;
                if finish.command_id != active.started.command_id
                    || self.checkpoint.as_ref() != Some(&finish.after)
                    || finish.after.checkpoint_sequence + 1 != envelope.sequence
                    || finish.after.checkpoint_event_hash != envelope.previous_event_hash
                    || finish.after.checkpoint_sequence <= active.started.before.checkpoint_sequence
                    || finish.started_millis != active.millis
                    || finish.finished_millis > envelope.monotonic_millis
                    || [finish.stdout.bytes, finish.stderr.bytes] != active.bytes
                {
                    return Err(invalid("finish pairing/tree/time/capture mismatch"));
                }
                let expected_stdout_mode =
                    if active.started.console.as_ref().is_some_and(|route| {
                        matches!(route.stdout, ConsoleStdoutRoute::File { .. })
                    }) {
                        CommandCaptureMode::Redirected
                    } else {
                        CommandCaptureMode::Captured
                    };
                if finish.stdout.mode != expected_stdout_mode
                    || finish.stderr.mode != CommandCaptureMode::Captured
                {
                    return Err(invalid("command route/capture mode mismatch"));
                }
                if let CommandOutcome::Terminated {
                    reason: CommandTermination::Deadline,
                    ..
                } = finish.outcome
                    && finish.finished_millis.saturating_sub(finish.started_millis)
                        < active.started.deadline_millis
                {
                    return Err(invalid("deadline termination precedes deadline"));
                }
                if matches!(
                    finish.outcome,
                    CommandOutcome::Terminated {
                        reason: CommandTermination::OutputLimit,
                        ..
                    }
                ) && active.bytes.iter().sum::<u64>() != active.started.output_limit
                {
                    return Err(invalid(
                        "output-limit termination requires exact capture cap",
                    ));
                }
                // A cap hit is explicit incomplete evidence, even after a zero
                // exit. Nothing here derives Cargo diagnostic counts.
                if active.bytes.iter().sum::<u64>() == active.started.output_limit
                    && finish.stdout.completeness == CaptureCompleteness::Complete
                    && finish.stderr.completeness == CaptureCompleteness::Complete
                {
                    return Err(invalid("capture cap cannot claim complete evidence"));
                }
            }
            Event::TestCaseCompared(comparison) => {
                let finished = self
                    .last_finished
                    .as_ref()
                    .ok_or_else(|| invalid("comparison does not immediately follow a finish"))?;
                let route = finished
                    .started
                    .console
                    .as_ref()
                    .ok_or_else(|| invalid("comparison requires a console Run"))?;
                let ConsoleStdinRoute::File { path } = &route.stdin else {
                    return Err(invalid("comparison requires file stdin"));
                };
                if comparison.command_id != finished.started.command_id
                    || finished.started.action != ControlledAction::Run
                    || path.as_str() != format!("{}.in", comparison.case)
                    || route.stdout != ConsoleStdoutRoute::Console
                    || finished.finished.stdout.mode != CommandCaptureMode::Captured
                {
                    return Err(invalid("comparison command/case/route mismatch"));
                }
                let stdout_blake3 = rustrace_model::rprov_raw_blake3(&finished.stdout);
                let expected_actual =
                    if finished.finished.stdout.completeness == CaptureCompleteness::Unavailable {
                        None
                    } else {
                        Some(stdout_blake3)
                    };
                if comparison.actual_blake3 != expected_actual {
                    return Err(invalid("comparison actual hash mismatch"));
                }
                match comparison.outcome {
                    TestCaseComparisonOutcome::Pass
                        if finished.finished.stdout.bytes > MAX_TEST_CASE_EXPECTED_LINE_BYTES =>
                    {
                        return Err(invalid("PASS stdout exceeds expected-file limit"));
                    }
                    TestCaseComparisonOutcome::Pass
                        if comparison.expected_blake3 != stdout_blake3 =>
                    {
                        return Err(invalid("PASS hashes differ"));
                    }
                    TestCaseComparisonOutcome::Mismatch {
                        line, actual_len, ..
                    } => {
                        let Some(recorded_len) = line_len(&finished.stdout, line) else {
                            return Err(invalid("mismatch line exceeds captured stdout"));
                        };
                        if actual_len != recorded_len {
                            return Err(invalid("mismatch actual length contradicts stdout"));
                        }
                    }
                    _ => {}
                }

                let required_error = match finished.finished.outcome {
                    CommandOutcome::LaunchFailed { .. } => {
                        Some(TestCaseComparisonError::LaunchFailed)
                    }
                    CommandOutcome::Exited { code } if code != 0 => {
                        Some(TestCaseComparisonError::NonzeroExit)
                    }
                    CommandOutcome::Terminated { .. } => Some(TestCaseComparisonError::Terminated),
                    CommandOutcome::Exited { code: 0 } => {
                        match finished.finished.stdout.completeness {
                            CaptureCompleteness::Complete => None,
                            CaptureCompleteness::Truncated => {
                                Some(TestCaseComparisonError::CaptureTruncated)
                            }
                            CaptureCompleteness::Unavailable => {
                                Some(TestCaseComparisonError::CaptureUnavailable)
                            }
                            CaptureCompleteness::ReadFailed => {
                                Some(TestCaseComparisonError::CaptureReadFailed)
                            }
                        }
                    }
                    CommandOutcome::Exited { .. } => unreachable!("nonzero exit handled above"),
                };
                match (&comparison.outcome, required_error) {
                    (TestCaseComparisonOutcome::Error { reason }, Some(required))
                        if *reason == required =>
                    {
                        Ok(())
                    }
                    (
                        TestCaseComparisonOutcome::Error {
                            reason:
                                TestCaseComparisonError::ExpectedUnreadable
                                | TestCaseComparisonError::ExpectedOversized,
                        },
                        None,
                    ) => Ok(()),
                    (
                        TestCaseComparisonOutcome::Pass
                        | TestCaseComparisonOutcome::Mismatch { .. },
                        None,
                    ) => Ok(()),
                    _ => Err(invalid("comparison outcome contradicts command evidence")),
                }?;
            }
            Event::FileEdited(transaction) if transaction.origin == EditOrigin::Formatter => {
                if self.active.as_ref().map(|active| active.started.action)
                    != Some(ControlledAction::Format)
                {
                    return Err(invalid(
                        "Formatter edit requires the active controlled Format command",
                    ));
                }
            }
            Event::FileEdited(transaction) if transaction.origin == EditOrigin::DependencyTool => {
                if !matches!(
                    self.active.as_ref().map(|active| active.started.action),
                    Some(
                        ControlledAction::Add | ControlledAction::Remove | ControlledAction::Update
                    )
                ) {
                    return Err(invalid(
                        "DependencyTool edit requires an active controlled dependency command",
                    ));
                }
                let path = workspace
                    .document(&transaction.document_id)
                    .map(|document| document.path().as_str());
                if !matches!(path, Some("Cargo.toml" | "Cargo.lock")) {
                    return Err(invalid(
                        "DependencyTool edit requires a root Cargo.toml or Cargo.lock document",
                    ));
                }
            }
            _ if self.active.is_some()
                && !matches!(
                    envelope.event,
                    Event::WorkspaceCheckpoint(_)
                        | Event::ExternalObservation(_)
                        | Event::RecoveryRecorded(_)
                ) =>
            {
                return Err(invalid("event violates exclusive command lifecycle"));
            }
            _ => {}
        }
        Ok(())
    }

    /// Called only after the entire event applied successfully, preserving
    /// atomic failure and certified checkpoint suffix behavior.
    pub fn applied(&mut self, envelope: &EventEnvelope, tree: Hash) {
        self.last_millis = envelope.monotonic_millis;
        // A comparison can consume only the immediately preceding finish.
        // Clear the route for every intervening event, including mutations
        // and checkpoints handled by dedicated match arms below.
        self.last_finished = None;
        match &envelope.event {
            Event::ControlledCommandStarted(started) => {
                self.active = Some(ActiveCommand {
                    started: started.clone(),
                    millis: envelope.monotonic_millis,
                    bytes: [0; 2],
                    stdout: Vec::new(),
                })
            }
            Event::ControlledCommandOutput(output) => {
                let index = if output.stream == OutputStream::Stdout {
                    0
                } else {
                    1
                };
                let count = output.bytes_hex.len() as u64 / 2;
                self.active
                    .as_mut()
                    .expect("validated active command")
                    .bytes[index] += count;
                if output.stream == OutputStream::Stdout {
                    let bytes = output
                        .original_bytes()
                        .expect("validated command output bytes");
                    self.active
                        .as_mut()
                        .expect("validated active command")
                        .stdout
                        .extend_from_slice(&bytes);
                }
                self.output_bytes += count;
            }
            Event::ControlledCommandFinished(finished) => {
                let active = self.active.take().expect("validated active command");
                self.last_finished = Some(FinishedCommand {
                    started: active.started,
                    finished: finished.clone(),
                    stdout: active.stdout,
                });
            }
            Event::FileEdited(_)
            | Event::InternalPaste(_)
            | Event::FileCreated(_)
            | Event::FileDeleted(_)
            | Event::FileRenamed(_)
            | Event::ExternalFileChange(_) => self.workspace_version = envelope.sequence,
            Event::WorkspaceCheckpoint(_) => {
                self.checkpoint = Some(CommandTreeLink {
                    checkpoint_sequence: envelope.sequence,
                    checkpoint_event_hash: envelope.event_hash,
                    workspace_hash: tree,
                    workspace_version: self.workspace_version,
                })
            }
            _ => {}
        }
    }
}

fn line_len(bytes: &[u8], requested: u64) -> Option<u64> {
    let mut line = 1_u64;
    let mut start = 0_usize;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            if line == requested {
                return Some((index - start) as u64);
            }
            line += 1;
            start = index + 1;
        }
    }
    (line == requested).then_some((bytes.len() - start) as u64)
}
