//! Shared hard review-flag vocabulary and verifier mapping.

use crate::{
    display,
    verify::{
        AssignmentReferenceStatus, SubmittedSourceStatus, VerificationEventLocation,
        VerificationIssueKind, VerificationIssueLocation, VerificationReport, VerificationStatus,
    },
};
use rustrace_model::{
    EditOrigin, EditorTransaction, Event, EventEnvelope, PasteRejected, PasteRejectionReason,
    RprovKnown, RprovProducer, inserted_text_counts,
};
use std::collections::VecDeque;

pub const EVIDENCE_CONSISTENCY: &str = "This provenance is internally replayable and consistent.";
pub const EVIDENCE_FAILURE: &str = "This package did not validate.";
pub const EVIDENCE_LIMITATION_FIRST: &str =
    "It does not prove that the client was unmodified or that the recorded code";
pub const EVIDENCE_LIMITATION_SECOND: &str = "originated from the student.";
pub const EVIDENCE_NOT_EVALUATED: &str =
    "Package checks passed; reference or submitted source not evaluated.";
/// Per-batch wording for surfaces that describe many packages at once. It
/// names the three per-row outcomes instead of asserting one for every row,
/// and it contains no commas so it can sit inside the CSV comment line.
pub const EVIDENCE_LIMITATIONS_BATCH: &str = concat!(
    "Each row states its own package outcome under Evidence. Consistent: ",
    "This provenance is internally replayable and consistent.",
    " Failed: ",
    "This package did not validate.",
    " Partial: ",
    "Package checks passed; reference or submitted source not evaluated.",
    " For every row: ",
    "It does not prove that the client was unmodified or that the recorded code",
    " ",
    "originated from the student."
);
const MAX_FLAG_DETAIL_BYTES: usize = 512;
const MAX_FLAG_LINK_BYTES: usize = 512;

/// A single Keyboard transaction reaches this advisory at 200 inserted UTF-8
/// bytes. Insertions across every edit in the transaction are summed.
pub const LARGE_SINGLE_INSERTION_BYTES: usize = 200;
/// Sustained typing rate uses an exact 60-second sliding window.
pub const SUSTAINED_HIGH_RATE_WINDOW_MILLIS: u64 = 60_000;
/// A 60-second Keyboard window is advisory only when it contains more than 15
/// inserted Unicode scalar values per second.
pub const SUSTAINED_HIGH_RATE_CHARACTERS_PER_SECOND: u64 = 15;
/// An attempt reaches this advisory at one blocked attempt to paste text from
/// outside Rustrace (a `paste_rejected` event with reason `external_input`).
/// Other rejection reasons are not counted.
pub const REJECTED_PASTE_ATTEMPTS_MINIMUM: u64 = 1;
/// A blocked outside paste reaches this advisory when Keyboard transactions
/// then insert at least 200 Unicode scalar values (counted as for
/// `SUSTAINED_HIGH_RATE`; deletions insert none) within the window.
pub const TYPED_AFTER_REJECTED_PASTE_CHARACTERS: u64 = 200;
/// The window after a blocked outside paste: Keyboard transactions recorded
/// at most 300,000 ms (five minutes) later, inclusive, on the attempt's
/// monotonic event clock.
pub const TYPED_AFTER_REJECTED_PASTE_WINDOW_MILLIS: u64 = 300_000;
/// A released build records its build identity as `COMMIT;TARGET`, where
/// COMMIT is the release's Git commit in exactly 40 lowercase hexadecimal
/// digits, the form the release scripts require. A `-dirty` suffix,
/// `development-build-unavailable` and `source-archive-commit-unavailable`
/// all fall outside it.
pub const RELEASED_BUILD_COMMIT_HEX_DIGITS: usize = 40;
/// The longest recorded client version or build identity quoted in an
/// `UNOFFICIAL_CLIENT` measured value.
const MAX_QUOTED_PRODUCER_BYTES: usize = 160;
const ADVISORY_SUFFIX: &str = "advisory: heuristic; expect false positives";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AdvisoryFlagKind {
    LargeSingleInsertion,
    SustainedHighRate,
    RejectedPasteAttempts,
    TypedAfterRejectedPaste,
    UnofficialClient,
    TestFilesModified,
}

impl AdvisoryFlagKind {
    pub const ALL: [Self; 6] = [
        Self::LargeSingleInsertion,
        Self::SustainedHighRate,
        Self::RejectedPasteAttempts,
        Self::TypedAfterRejectedPaste,
        Self::UnofficialClient,
        Self::TestFilesModified,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::LargeSingleInsertion => "LARGE_SINGLE_INSERTION",
            Self::SustainedHighRate => "SUSTAINED_HIGH_RATE",
            Self::RejectedPasteAttempts => "REJECTED_PASTE_ATTEMPTS",
            Self::TypedAfterRejectedPaste => "TYPED_AFTER_REJECTED_PASTE",
            Self::UnofficialClient => "UNOFFICIAL_CLIENT",
            Self::TestFilesModified => "TEST_FILES_MODIFIED",
        }
    }

    pub const fn explanation(self) -> &'static str {
        match self {
            Self::LargeSingleInsertion => {
                "the record contains one Keyboard transaction inserting at least 200 bytes"
            }
            Self::SustainedHighRate => {
                "the record contains more than 15 inserted characters per second across a 60-second Keyboard window"
            }
            Self::RejectedPasteAttempts => {
                "the record contains at least one blocked attempt to paste text from outside Rustrace"
            }
            Self::TypedAfterRejectedPaste => {
                "the record contains at least 200 characters inserted by Keyboard transactions within 300 seconds after a blocked outside paste"
            }
            Self::UnofficialClient => {
                "the package metadata for this attempt names a client version or build identity other than a clean released build"
            }
            Self::TestFilesModified => {
                "the record contains a run of a packaged test case whose expected output, arguments, input, or fixture files differ from the reference package"
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisoryFlag {
    pub kind: AdvisoryFlagKind,
    pub link: VerificationEventLocation,
    pub measured_value: String,
}

#[derive(Clone)]
struct KeyboardObservation {
    link: VerificationEventLocation,
    monotonic_millis: u64,
    inserted_characters: u64,
}

/// The typing window opened by one blocked outside paste.
struct RejectedPasteWindow {
    link: VerificationEventLocation,
    monotonic_millis: u64,
    /// The attempt's Keyboard characters inserted before the paste.
    keyboard_characters_before: u64,
}

/// True when a recorded client version and build identity describe a clean
/// released build: a plain `MAJOR.MINOR.PATCH` version and a build identity
/// `COMMIT;TARGET` whose commit has exactly
/// [`RELEASED_BUILD_COMMIT_HEX_DIGITS`] lowercase hexadecimal digits and whose
/// target is a nonempty triple. Nothing here can confirm that the commit is
/// a published release; the check only separates released-build identities
/// from local, modified, or unidentified builds.
pub fn is_clean_released_build(client_version: &str, build_identity: &str) -> bool {
    let mut parts = client_version.split('.');
    let plain_version = parts
        .by_ref()
        .take(3)
        .filter(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        .count()
        == 3
        && parts.next().is_none();
    let released_identity = build_identity
        .split_once(';')
        .is_some_and(|(commit, target)| {
            commit.len() == RELEASED_BUILD_COMMIT_HEX_DIGITS
                && commit
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                && !target.is_empty()
                && target
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        });
    plain_version && released_identity
}

/// The `UNOFFICIAL_CLIENT` measured value for an attempt's producer, or
/// `None` for a clean released build.
fn unofficial_client(producer: &RprovProducer) -> Option<String> {
    let known = |value: &RprovKnown<String>| match value {
        RprovKnown::Known { value } => Some(value.clone()),
        RprovKnown::Unknown => None,
    };
    let client_version = known(&producer.client_version);
    let build_identity = known(&producer.build_identity);
    if let (Some(client_version), Some(build_identity)) = (&client_version, &build_identity)
        && is_clean_released_build(client_version, build_identity)
    {
        return None;
    }
    let quote = |value: Option<String>| {
        value.map_or_else(
            || "unavailable".to_owned(),
            |value| display::label(&value, MAX_QUOTED_PRODUCER_BYTES),
        )
    };
    Some(format!(
        "client version {}; build identity {}",
        quote(client_version),
        quote(build_identity)
    ))
}

/// Derives every advisory for one attempt (one `.rprov` segment) from its
/// validated events, in stream order. Advisories tied to one event are
/// emitted as their event is observed; the per-attempt summaries follow in
/// [`Self::finish`].
pub(crate) struct AdvisoryAccumulator {
    segment: u32,
    advisories: Vec<AdvisoryFlag>,
    rate_window: VecDeque<KeyboardObservation>,
    first_keyboard_millis: Option<u64>,
    rate_advisory_active: bool,
    rejected_pastes: u64,
    first_rejected_paste: Option<VerificationEventLocation>,
    /// Characters inserted by every Keyboard transaction observed so far.
    keyboard_characters: u64,
    /// Open windows, oldest paste first.
    rejected_paste_windows: VecDeque<RejectedPasteWindow>,
    typed_after_rejected_pastes: u64,
    first_typed_after_rejected_paste: Option<(VerificationEventLocation, u64)>,
    /// The measured value of an `UNOFFICIAL_CLIENT` advisory, emitted with
    /// the link of the attempt's first event.
    unofficial_client: Option<String>,
}

impl AdvisoryAccumulator {
    pub(crate) fn new(segment: u32) -> Self {
        Self {
            segment,
            advisories: Vec::new(),
            rate_window: VecDeque::new(),
            first_keyboard_millis: None,
            rate_advisory_active: false,
            rejected_pastes: 0,
            first_rejected_paste: None,
            keyboard_characters: 0,
            rejected_paste_windows: VecDeque::new(),
            typed_after_rejected_pastes: 0,
            first_typed_after_rejected_paste: None,
            unofficial_client: None,
        }
    }

    /// Records the attempt's producer from the package manifest. Only the
    /// build that started the attempt is recorded, so this is the one
    /// identity available per attempt; it is package metadata, not part of
    /// the hash-chained events.
    pub(crate) fn observe_producer(&mut self, producer: &RprovProducer) {
        self.unofficial_client = unofficial_client(producer);
    }

    pub(crate) fn observe(&mut self, envelope: &EventEnvelope) {
        let link = VerificationEventLocation {
            segment: self.segment,
            sequence: envelope.sequence,
        };
        if let Some(measured_value) = self.unofficial_client.take() {
            self.advisories.push(AdvisoryFlag {
                kind: AdvisoryFlagKind::UnofficialClient,
                link,
                measured_value,
            });
        }
        match &envelope.event {
            Event::PasteRejected(PasteRejected {
                reason: PasteRejectionReason::ExternalInput,
                ..
            }) => self.observe_rejected_paste(link, envelope.monotonic_millis),
            Event::FileEdited(transaction) if transaction.origin == EditOrigin::Keyboard => {
                self.observe_keyboard(link, envelope.monotonic_millis, transaction);
            }
            _ => {}
        }
    }

    fn observe_rejected_paste(&mut self, link: VerificationEventLocation, monotonic_millis: u64) {
        self.rejected_pastes = self.rejected_pastes.saturating_add(1);
        self.first_rejected_paste.get_or_insert(link);
        self.rejected_paste_windows.push_back(RejectedPasteWindow {
            link,
            monotonic_millis,
            keyboard_characters_before: self.keyboard_characters,
        });
    }

    /// Closes every window that ends before `now`, or every open window at
    /// the end of the attempt, and tallies the ones typed through.
    fn close_rejected_paste_windows(&mut self, now: Option<u64>) {
        while let Some(window) = self.rejected_paste_windows.front() {
            if now.is_some_and(|now| {
                now.saturating_sub(window.monotonic_millis)
                    <= TYPED_AFTER_REJECTED_PASTE_WINDOW_MILLIS
            }) {
                break;
            }
            let window = self
                .rejected_paste_windows
                .pop_front()
                .expect("front window");
            let typed = self
                .keyboard_characters
                .saturating_sub(window.keyboard_characters_before);
            if typed >= TYPED_AFTER_REJECTED_PASTE_CHARACTERS {
                self.typed_after_rejected_pastes =
                    self.typed_after_rejected_pastes.saturating_add(1);
                self.first_typed_after_rejected_paste
                    .get_or_insert((window.link, typed));
            }
        }
    }

    fn observe_keyboard(
        &mut self,
        link: VerificationEventLocation,
        monotonic_millis: u64,
        transaction: &EditorTransaction,
    ) {
        self.close_rejected_paste_windows(Some(monotonic_millis));
        let inserted_bytes = transaction
            .edits
            .iter()
            .map(|edit| edit.inserted_text.len())
            .sum::<usize>();
        let inserted_characters = transaction.edits.iter().fold(0_u64, |total, edit| {
            total.saturating_add(
                u64::try_from(inserted_text_counts(&edit.inserted_text).character_count)
                    .unwrap_or(u64::MAX),
            )
        });
        self.keyboard_characters = self.keyboard_characters.saturating_add(inserted_characters);
        let observation = KeyboardObservation {
            link,
            monotonic_millis,
            inserted_characters,
        };

        if inserted_bytes >= LARGE_SINGLE_INSERTION_BYTES {
            self.advisories.push(AdvisoryFlag {
                kind: AdvisoryFlagKind::LargeSingleInsertion,
                link: observation.link,
                measured_value: format!("{inserted_bytes} bytes"),
            });
        }
        self.observe_sustained_rate(observation);
    }

    fn observe_sustained_rate(&mut self, observation: KeyboardObservation) {
        self.first_keyboard_millis
            .get_or_insert(observation.monotonic_millis);
        while self.rate_window.front().is_some_and(|first| {
            observation
                .monotonic_millis
                .saturating_sub(first.monotonic_millis)
                > SUSTAINED_HIGH_RATE_WINDOW_MILLIS
        }) {
            self.rate_window.pop_front();
        }
        if let Some(last) = self.rate_window.back_mut()
            && last.monotonic_millis == observation.monotonic_millis
        {
            last.inserted_characters = last
                .inserted_characters
                .saturating_add(observation.inserted_characters);
        } else {
            self.rate_window.push_back(observation.clone());
        }

        let full_window_observed = self.first_keyboard_millis.is_some_and(|first| {
            observation.monotonic_millis.saturating_sub(first) >= SUSTAINED_HIGH_RATE_WINDOW_MILLIS
        });
        let inserted_characters = self.rate_window.iter().fold(0_u64, |total, item| {
            total.saturating_add(item.inserted_characters)
        });
        let threshold =
            SUSTAINED_HIGH_RATE_CHARACTERS_PER_SECOND * (SUSTAINED_HIGH_RATE_WINDOW_MILLIS / 1_000);
        let qualifies = full_window_observed && inserted_characters > threshold;
        if qualifies && !self.rate_advisory_active {
            let rate =
                inserted_characters as f64 / (SUSTAINED_HIGH_RATE_WINDOW_MILLIS as f64 / 1_000.0);
            self.advisories.push(AdvisoryFlag {
                kind: AdvisoryFlagKind::SustainedHighRate,
                link: self.rate_window.front().expect("nonempty rate window").link,
                measured_value: format!(
                    "{rate:.3} characters/second ({inserted_characters} characters)"
                ),
            });
        }
        self.rate_advisory_active = qualifies;
    }

    pub(crate) fn finish(mut self) -> Vec<AdvisoryFlag> {
        self.close_rejected_paste_windows(None);
        if self.rejected_pastes >= REJECTED_PASTE_ATTEMPTS_MINIMUM
            && let Some(first) = self.first_rejected_paste
        {
            self.advisories.push(AdvisoryFlag {
                kind: AdvisoryFlagKind::RejectedPasteAttempts,
                link: first,
                measured_value: plural(self.rejected_pastes, "blocked outside paste"),
            });
        }
        // One advisory per attempt, linked to the earliest blocked paste that
        // was typed through, with how many of the attempt's blocked pastes were.
        if let Some((link, typed)) = self.first_typed_after_rejected_paste {
            self.advisories.push(AdvisoryFlag {
                kind: AdvisoryFlagKind::TypedAfterRejectedPaste,
                link,
                measured_value: format!(
                    "{typed} characters within {} seconds after it; qualifying blocked outside pastes: {} of {}",
                    TYPED_AFTER_REJECTED_PASTE_WINDOW_MILLIS / 1_000,
                    self.typed_after_rejected_pastes,
                    self.rejected_pastes
                ),
            });
        }
        self.advisories
    }
}

/// The `TEST_FILES_MODIFIED` advisory for one attempt: `runs` recorded runs
/// of packaged cases used files that differ from the reference package, the
/// first at `first` for `first_reason`. Only a reference can tell, so only
/// `verify --reference` and `scan --reference` derive it.
pub(crate) fn test_files_modified(
    first: VerificationEventLocation,
    runs: u64,
    first_reason: &str,
) -> AdvisoryFlag {
    AdvisoryFlag {
        kind: AdvisoryFlagKind::TestFilesModified,
        link: first,
        measured_value: format!(
            "{} with changed test files; first: {}",
            plural(runs, "run"),
            display::label(first_reason, 256)
        ),
    }
}

fn plural(count: u64, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

pub fn display_advisory(advisory: &AdvisoryFlag) -> String {
    display::label_fmt(
        format_args!(
            "{} [segment:{} seq:{}]: {}; measured value: {}; {ADVISORY_SUFFIX}",
            advisory.kind.name(),
            advisory.link.segment,
            advisory.link.sequence,
            advisory.kind.explanation(),
            advisory.measured_value,
        ),
        1_024,
    )
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ReviewFlagKind {
    PackageInvalid,
    EventChainInvalid,
    EventSequenceGap,
    CheckpointMismatch,
    ReplayMismatch,
    SourceMismatch,
    UnprovenancedExternalChange,
}

impl ReviewFlagKind {
    pub const ALL: [Self; 7] = [
        Self::PackageInvalid,
        Self::EventChainInvalid,
        Self::EventSequenceGap,
        Self::CheckpointMismatch,
        Self::ReplayMismatch,
        Self::SourceMismatch,
        Self::UnprovenancedExternalChange,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::PackageInvalid => "PACKAGE_INVALID",
            Self::EventChainInvalid => "EVENT_CHAIN_INVALID",
            Self::EventSequenceGap => "EVENT_SEQUENCE_GAP",
            Self::CheckpointMismatch => "CHECKPOINT_MISMATCH",
            Self::ReplayMismatch => "REPLAY_MISMATCH",
            Self::SourceMismatch => "SOURCE_MISMATCH",
            Self::UnprovenancedExternalChange => "UNPROVENANCED_EXTERNAL_CHANGE",
        }
    }

    pub const fn explanation(self) -> &'static str {
        match self {
            Self::PackageInvalid => {
                "the package structure does not validate; its contents are unavailable for normal review"
            }
            Self::EventChainInvalid => {
                "the recorded event chain does not validate; the package cannot be trusted as internally consistent"
            }
            Self::EventSequenceGap => {
                "the recorded event sequence is not contiguous; one or more event positions are unavailable"
            }
            Self::CheckpointMismatch => {
                "a recorded checkpoint does not match its declaration or replayed state"
            }
            Self::ReplayMismatch => {
                "the recorded events do not reconstruct the declared workspace state"
            }
            Self::SourceMismatch => {
                "the submitted source tree does not match the final state reconstructed from the record"
            }
            Self::UnprovenancedExternalChange => {
                "required external-change recovery evidence is missing, unreadable, or does not reconstruct"
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewFlagLink {
    Event(VerificationEventLocation),
    Artifact(String),
    Location(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewFlag {
    pub kind: ReviewFlagKind,
    pub link: ReviewFlagLink,
    pub detail: String,
}

/// The package's own verification outcome, shared by every TA surface so a
/// header, status line, row and flags view never contradict one another.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceOutcome {
    /// Every check passed and no issue was recorded.
    Consistent,
    /// A package check failed, the submitted source or assignment reference
    /// mismatched, or a hard review flag was raised.
    Failed,
    /// The package checks passed but the assignment reference or the
    /// submitted-source comparison could not be evaluated.
    NotEvaluated,
}

pub fn evidence_outcome(report: &VerificationReport) -> EvidenceOutcome {
    if report.is_clean() {
        return EvidenceOutcome::Consistent;
    }
    let checks_failed = [
        report.package_structure,
        report.event_chain,
        report.checkpoint_hashes,
        report.replay,
    ]
    .iter()
    .any(|status| *status != VerificationStatus::Ok);
    if checks_failed
        || report.submitted_source_match == SubmittedSourceStatus::SourceMismatch
        || report.assignment_reference == AssignmentReferenceStatus::Mismatch
        || !review_flags(report).is_empty()
    {
        EvidenceOutcome::Failed
    } else {
        EvidenceOutcome::NotEvaluated
    }
}

/// The first sentence of the shared evidence-limitations wording for the
/// package's [`EvidenceOutcome`].
pub fn evidence_statement(report: &VerificationReport) -> &'static str {
    match evidence_outcome(report) {
        EvidenceOutcome::Consistent => EVIDENCE_CONSISTENCY,
        EvidenceOutcome::Failed => EVIDENCE_FAILURE,
        EvidenceOutcome::NotEvaluated => EVIDENCE_NOT_EVALUATED,
    }
}

pub fn review_flags(report: &VerificationReport) -> Vec<ReviewFlag> {
    report
        .issues
        .iter()
        .filter_map(|issue| {
            let kind = match issue.kind {
                VerificationIssueKind::Input | VerificationIssueKind::PackageStructure => {
                    ReviewFlagKind::PackageInvalid
                }
                VerificationIssueKind::EventChain => ReviewFlagKind::EventChainInvalid,
                VerificationIssueKind::EventSequence => ReviewFlagKind::EventSequenceGap,
                VerificationIssueKind::CheckpointHashes => ReviewFlagKind::CheckpointMismatch,
                VerificationIssueKind::Replay => ReviewFlagKind::ReplayMismatch,
                VerificationIssueKind::SubmittedSource
                    if report.submitted_source_match == SubmittedSourceStatus::SourceMismatch =>
                {
                    ReviewFlagKind::SourceMismatch
                }
                VerificationIssueKind::SubmittedSource => return None,
                VerificationIssueKind::UnprovenancedExternalChange => {
                    ReviewFlagKind::UnprovenancedExternalChange
                }
                VerificationIssueKind::AssignmentReference => return None,
            };
            let link = match &issue.location {
                VerificationIssueLocation::Event(location) => ReviewFlagLink::Event(*location),
                VerificationIssueLocation::Artifact(path) => {
                    ReviewFlagLink::Artifact(display::label(path, MAX_FLAG_LINK_BYTES))
                }
                VerificationIssueLocation::Decoder(location) => {
                    ReviewFlagLink::Location(display::label(location, MAX_FLAG_LINK_BYTES))
                }
            };
            let verifier_detail = display::label(&issue.detail, MAX_FLAG_DETAIL_BYTES);
            let detail = display::label_fmt(
                format_args!("{}; verifier detail: {verifier_detail}", kind.explanation()),
                MAX_FLAG_DETAIL_BYTES,
            );
            Some(ReviewFlag { kind, link, detail })
        })
        .collect()
}

pub fn display_flag(flag: &ReviewFlag) -> String {
    display::label_fmt(
        format_args!("{}: {}", display_flag_link(flag), flag.detail),
        1_024,
    )
}

pub fn display_flag_link(flag: &ReviewFlag) -> String {
    let link = match &flag.link {
        ReviewFlagLink::Event(location) => {
            format!("segment:{} seq:{}", location.segment, location.sequence)
        }
        ReviewFlagLink::Artifact(path) => format!("artifact:{path}"),
        ReviewFlagLink::Location(location) => format!("location:{location}"),
    };
    display::label_fmt(format_args!("{} [{link}]", flag.kind.name()), 1_024)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustrace_model::{
        DocumentId, EditOrigin, EditorTransaction, Event, EventEnvelope, Hash, PasteInputChannel,
        PasteRejected, PasteRejectionReason, SelectionState, SessionId, TextEdit,
    };

    fn envelope(sequence: u64, millis: u64, event: Event) -> EventEnvelope {
        EventEnvelope {
            format_version: 1,
            session_id: SessionId::new("advisory-test").unwrap(),
            sequence,
            monotonic_millis: millis,
            wall_clock_utc: None,
            previous_event_hash: Hash::zero(),
            event_hash: Hash::zero(),
            event,
        }
    }

    fn rejected_paste(sequence: u64, millis: u64, reason: PasteRejectionReason) -> EventEnvelope {
        envelope(
            sequence,
            millis,
            Event::PasteRejected(PasteRejected {
                reason,
                channel: PasteInputChannel::TerminalBracketed,
            }),
        )
    }

    fn edit_event(sequence: u64, millis: u64, origin: EditOrigin, inserted: &str) -> EventEnvelope {
        envelope(
            sequence,
            millis,
            Event::FileEdited(EditorTransaction {
                document_id: DocumentId::new("main").unwrap(),
                version_before: sequence,
                version_after: sequence + 1,
                origin,
                edits: vec![TextEdit {
                    start_byte: 0,
                    end_byte: 0,
                    inserted_text: inserted.to_owned(),
                }],
                selection_before: SelectionState::caret(0),
                selection_after: SelectionState::caret(inserted.len() as u64),
                hash_before: Hash::zero(),
                hash_after: Hash::zero(),
            }),
        )
    }

    fn derive(events: &[EventEnvelope]) -> Vec<AdvisoryFlag> {
        let mut accumulator = AdvisoryAccumulator::new(2);
        for event in events {
            accumulator.observe(event);
        }
        accumulator.finish()
    }

    fn kinds(flags: &[AdvisoryFlag]) -> Vec<AdvisoryFlagKind> {
        flags.iter().map(|flag| flag.kind).collect()
    }

    fn high_rate_events(inserted_characters: usize) -> Vec<EventEnvelope> {
        let mut remaining = inserted_characters;
        (0..=60)
            .map(|second| {
                let events_left = 61 - second;
                let characters = remaining.div_ceil(events_left);
                remaining -= characters;
                edit_event(
                    second as u64 + 1,
                    second as u64 * 1_000,
                    EditOrigin::Keyboard,
                    &"x".repeat(characters),
                )
            })
            .collect()
    }

    #[test]
    fn large_single_insertion_uses_the_inclusive_byte_threshold() {
        for (inserted_bytes, expected) in [(199, false), (200, true), (201, true)] {
            let flags = derive(&[edit_event(
                7,
                10,
                EditOrigin::Keyboard,
                &"x".repeat(inserted_bytes),
            )]);
            assert_eq!(
                kinds(&flags).contains(&AdvisoryFlagKind::LargeSingleInsertion),
                expected,
                "inserted bytes: {inserted_bytes}"
            );
        }
    }

    #[test]
    fn sustained_high_rate_uses_a_strict_sixty_second_rate_threshold() {
        for (characters, expected) in [(899, false), (900, false), (901, true)] {
            let flags = derive(&high_rate_events(characters));
            assert_eq!(
                kinds(&flags).contains(&AdvisoryFlagKind::SustainedHighRate),
                expected,
                "inserted characters: {characters}"
            );
        }
    }

    #[test]
    fn sustained_high_rate_uses_the_exact_sixty_second_window() {
        for (millis, expected) in [(59_999, false), (60_000, true), (60_001, true)] {
            let events = [
                edit_event(1, 0, EditOrigin::Keyboard, ""),
                edit_event(2, millis, EditOrigin::Keyboard, &"x".repeat(901)),
            ];
            assert_eq!(
                kinds(&derive(&events)).contains(&AdvisoryFlagKind::SustainedHighRate),
                expected,
                "window end: {millis}"
            );
        }
    }

    #[test]
    fn evenly_timed_keyboard_edits_such_as_a_held_backspace_raise_no_advisory() {
        // Holding Backspace repeats deletion-only Keyboard edits at a fixed
        // interval. Regular timing is not an advisory condition.
        let deletions = (0..120)
            .map(|index| {
                let mut event = edit_event(index + 1, index * 33, EditOrigin::Keyboard, "");
                if let Event::FileEdited(transaction) = &mut event.event {
                    transaction.edits[0].start_byte = 120 - index - 1;
                    transaction.edits[0].end_byte = 120 - index;
                }
                event
            })
            .collect::<Vec<_>>();
        assert!(derive(&deletions).is_empty());
        let typed = (0..120)
            .map(|index| edit_event(index + 1, index * 100, EditOrigin::Keyboard, "x"))
            .collect::<Vec<_>>();
        assert!(derive(&typed).is_empty());
    }

    #[test]
    fn rejected_paste_attempts_counts_blocked_outside_pastes_and_links_the_first() {
        assert!(derive(&[]).is_empty());

        // One blocked outside paste reaches the minimum.
        let flags = derive(&[rejected_paste(4, 10, PasteRejectionReason::ExternalInput)]);
        assert_eq!(REJECTED_PASTE_ATTEMPTS_MINIMUM, 1);
        assert_eq!(kinds(&flags), vec![AdvisoryFlagKind::RejectedPasteAttempts]);
        assert_eq!(
            flags[0].link,
            VerificationEventLocation {
                segment: 2,
                sequence: 4
            }
        );
        assert_eq!(flags[0].measured_value, "1 blocked outside paste");

        // Other rejection reasons are not outside text and are not counted.
        let other_reasons = [
            PasteRejectionReason::UnverifiableInput,
            PasteRejectionReason::MissingLiveSource,
            PasteRejectionReason::OutsideEditor,
        ];
        let others = other_reasons
            .iter()
            .enumerate()
            .map(|(index, reason)| rejected_paste(index as u64 + 1, 10, *reason))
            .collect::<Vec<_>>();
        assert!(derive(&others).is_empty());

        let mut mixed = others.clone();
        mixed.extend([
            rejected_paste(5, 20, PasteRejectionReason::ExternalInput),
            edit_event(6, 30, EditOrigin::Keyboard, "x"),
            rejected_paste(7, 40, PasteRejectionReason::ExternalInput),
            rejected_paste(8, 50, PasteRejectionReason::OutsideEditor),
            rejected_paste(9, 60, PasteRejectionReason::ExternalInput),
        ]);
        let flags = derive(&mixed);
        assert_eq!(kinds(&flags), vec![AdvisoryFlagKind::RejectedPasteAttempts]);
        assert_eq!(flags[0].link.sequence, 5);
        assert_eq!(flags[0].measured_value, "3 blocked outside pastes");
        assert!(
            display_advisory(&flags[0]).starts_with(
                "REJECTED_PASTE_ATTEMPTS [segment:2 seq:5]: the record contains at least one blocked attempt to paste text from outside Rustrace; measured value: 3 blocked outside pastes; "
            ),
            "{}",
            display_advisory(&flags[0])
        );
    }

    /// Keyboard transactions of at most 20 characters each, so that no
    /// single one reaches `LARGE_SINGLE_INSERTION`, all at `millis`.
    fn typed(first_sequence: u64, millis: u64, characters: usize) -> Vec<EventEnvelope> {
        let mut remaining = characters;
        let mut sequence = first_sequence;
        let mut events = Vec::new();
        while remaining > 0 {
            let count = remaining.min(20);
            events.push(edit_event(
                sequence,
                millis,
                EditOrigin::Keyboard,
                &"x".repeat(count),
            ));
            remaining -= count;
            sequence += 1;
        }
        events
    }

    fn deletion(sequence: u64, millis: u64) -> EventEnvelope {
        let mut event = edit_event(sequence, millis, EditOrigin::Keyboard, "");
        if let Event::FileEdited(transaction) = &mut event.event {
            transaction.edits[0].end_byte = 1;
        }
        event
    }

    fn typed_after(flags: &[AdvisoryFlag]) -> Option<&AdvisoryFlag> {
        flags
            .iter()
            .find(|flag| flag.kind == AdvisoryFlagKind::TypedAfterRejectedPaste)
    }

    #[test]
    fn typed_after_rejected_paste_uses_the_inclusive_character_threshold() {
        assert_eq!(TYPED_AFTER_REJECTED_PASTE_CHARACTERS, 200);
        for (characters, expected) in [(199, false), (200, true), (201, true)] {
            let mut events = vec![rejected_paste(
                1,
                1_000,
                PasteRejectionReason::ExternalInput,
            )];
            events.extend(typed(2, 2_000, characters));
            let flags = derive(&events);
            assert_eq!(
                typed_after(&flags).is_some(),
                expected,
                "characters: {characters}"
            );
            assert!(
                !kinds(&flags).contains(&AdvisoryFlagKind::LargeSingleInsertion),
                "{flags:#?}"
            );
            if expected {
                assert_eq!(
                    kinds(&flags),
                    vec![
                        AdvisoryFlagKind::RejectedPasteAttempts,
                        AdvisoryFlagKind::TypedAfterRejectedPaste,
                    ]
                );
                let flag = typed_after(&flags).unwrap();
                assert_eq!(flag.link.sequence, 1);
                assert_eq!(flag.link.segment, 2);
                assert_eq!(
                    flag.measured_value,
                    format!(
                        "{characters} characters within 300 seconds after it; qualifying blocked outside pastes: 1 of 1"
                    )
                );
            }
        }
    }

    #[test]
    fn typed_after_rejected_paste_uses_the_inclusive_five_minute_window() {
        assert_eq!(TYPED_AFTER_REJECTED_PASTE_WINDOW_MILLIS, 300_000);
        for (offset, expected) in [(299_999, true), (300_000, true), (300_001, false)] {
            let mut events = vec![rejected_paste(
                1,
                5_000,
                PasteRejectionReason::ExternalInput,
            )];
            // 199 characters well inside the window, then the last one at the
            // boundary under test.
            events.extend(typed(2, 6_000, 199));
            events.push(edit_event(20, 5_000 + offset, EditOrigin::Keyboard, "x"));
            assert_eq!(
                typed_after(&derive(&events)).is_some(),
                expected,
                "offset: {offset}"
            );
        }
        // Characters in a later window never count toward an expired one.
        let mut events = vec![rejected_paste(1, 0, PasteRejectionReason::ExternalInput)];
        events.extend(typed(2, 300_001, 1_000));
        assert!(typed_after(&derive(&events)).is_none());
    }

    #[test]
    fn typed_after_rejected_paste_counts_only_later_inserted_keyboard_characters() {
        // Deletion-only edits, such as a held Backspace, insert nothing.
        let mut events = vec![rejected_paste(1, 0, PasteRejectionReason::ExternalInput)];
        events.extend((0..500).map(|index| deletion(index + 2, 1_000 + index)));
        events.extend(typed(600, 2_000, 199));
        assert!(typed_after(&derive(&events)).is_none());

        // Typing before the blocked paste does not count.
        let mut events = typed(1, 0, 500);
        events.push(rejected_paste(
            100,
            1_000,
            PasteRejectionReason::ExternalInput,
        ));
        events.extend(typed(101, 2_000, 199));
        assert!(typed_after(&derive(&events)).is_none());

        // Other origins after the paste do not count.
        let mut events = vec![rejected_paste(1, 0, PasteRejectionReason::ExternalInput)];
        for (index, origin) in [
            EditOrigin::Paste,
            EditOrigin::Completion,
            EditOrigin::Formatter,
            EditOrigin::Undo,
            EditOrigin::Unknown,
        ]
        .into_iter()
        .enumerate()
        {
            events.push(edit_event(
                index as u64 + 2,
                1_000,
                origin,
                &"x".repeat(100),
            ));
        }
        events.extend(typed(10, 2_000, 199));
        assert!(typed_after(&derive(&events)).is_none());

        // Characters are Unicode scalar values, not bytes.
        for (pairs, expected) in [(99, false), (100, true)] {
            let mut events = vec![rejected_paste(1, 0, PasteRejectionReason::ExternalInput)];
            events.extend(
                (0..pairs).map(|index| edit_event(index + 2, 1_000, EditOrigin::Keyboard, "é🦀")),
            );
            assert_eq!(
                typed_after(&derive(&events)).is_some(),
                expected,
                "pairs: {pairs}"
            );
        }

        // Only blocked outside pastes open a window.
        let mut events = vec![
            rejected_paste(1, 0, PasteRejectionReason::UnverifiableInput),
            rejected_paste(2, 0, PasteRejectionReason::MissingLiveSource),
            rejected_paste(3, 0, PasteRejectionReason::OutsideEditor),
        ];
        events.extend(typed(4, 1_000, 1_000));
        assert!(derive(&events).is_empty());
    }

    #[test]
    fn typed_after_rejected_paste_reports_the_first_qualifying_paste_with_a_count() {
        let mut events = vec![
            rejected_paste(1, 0, PasteRejectionReason::ExternalInput),
            rejected_paste(2, 10_000, PasteRejectionReason::ExternalInput),
        ];
        events.extend(typed(3, 20_000, 250));
        // A third blocked paste that nobody types after.
        events.push(rejected_paste(
            50,
            400_000,
            PasteRejectionReason::ExternalInput,
        ));
        events.push(edit_event(51, 400_001, EditOrigin::Keyboard, "x"));
        let flags = derive(&events);
        assert_eq!(
            kinds(&flags),
            vec![
                AdvisoryFlagKind::RejectedPasteAttempts,
                AdvisoryFlagKind::TypedAfterRejectedPaste,
            ]
        );
        assert_eq!(flags[0].measured_value, "3 blocked outside pastes");
        let flag = typed_after(&flags).unwrap();
        assert_eq!(flag.link.sequence, 1);
        assert_eq!(
            flag.measured_value,
            "250 characters within 300 seconds after it; qualifying blocked outside pastes: 2 of 3"
        );
        assert_eq!(
            display_advisory(flag),
            "TYPED_AFTER_REJECTED_PASTE [segment:2 seq:1]: the record contains at least 200 characters inserted by Keyboard transactions within 300 seconds after a blocked outside paste; measured value: 250 characters within 300 seconds after it; qualifying blocked outside pastes: 2 of 3; advisory: heuristic; expect false positives"
        );

        // Only the later paste qualifies: the link moves to it.
        let mut events = vec![rejected_paste(1, 0, PasteRejectionReason::ExternalInput)];
        events.extend(typed(2, 1_000, 150));
        events.push(rejected_paste(
            20,
            400_000,
            PasteRejectionReason::ExternalInput,
        ));
        events.extend(typed(21, 400_500, 200));
        let flag = typed_after(&derive(&events)).cloned().unwrap();
        assert_eq!(flag.link.sequence, 20);
        assert_eq!(
            flag.measured_value,
            "200 characters within 300 seconds after it; qualifying blocked outside pastes: 1 of 2"
        );
    }

    const RELEASE_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn producer(client_version: Option<&str>, build_identity: Option<&str>) -> RprovProducer {
        let known = |value: Option<&str>| {
            value.map_or(RprovKnown::Unknown, |value| RprovKnown::Known {
                value: value.to_owned(),
            })
        };
        RprovProducer {
            client_version: known(client_version),
            build_identity: known(build_identity),
            os: RprovKnown::Unknown,
            architecture: RprovKnown::Unknown,
            rust_tools: Vec::new(),
        }
    }

    fn derive_with_producer(
        producer: &RprovProducer,
        events: &[EventEnvelope],
    ) -> Vec<AdvisoryFlag> {
        let mut accumulator = AdvisoryAccumulator::new(3);
        accumulator.observe_producer(producer);
        for event in events {
            accumulator.observe(event);
        }
        accumulator.finish()
    }

    #[test]
    fn clean_released_build_needs_a_plain_version_and_a_clean_release_commit() {
        let released = format!("{RELEASE_COMMIT};aarch64-apple-darwin");
        assert!(is_clean_released_build("0.1.8", &released));
        assert!(is_clean_released_build(
            "12.0.345",
            &format!("{RELEASE_COMMIT};x86_64-unknown-linux-gnu")
        ));
        assert_eq!(RELEASED_BUILD_COMMIT_HEX_DIGITS, 40);
        for identity in [
            format!("{RELEASE_COMMIT}-dirty;aarch64-apple-darwin"),
            "development-build-unavailable".to_owned(),
            "source-archive-commit-unavailable;aarch64-apple-darwin".to_owned(),
            "source-archive-commit-unavailable-dirty;aarch64-apple-darwin".to_owned(),
            format!("{};aarch64-apple-darwin", &RELEASE_COMMIT[..39]),
            format!("{RELEASE_COMMIT}0;aarch64-apple-darwin"),
            format!(
                "{};aarch64-apple-darwin",
                RELEASE_COMMIT.to_ascii_uppercase()
            ),
            format!("{}g;aarch64-apple-darwin", &RELEASE_COMMIT[..39]),
            format!("{RELEASE_COMMIT};"),
            RELEASE_COMMIT.to_owned(),
            format!("{RELEASE_COMMIT};aarch64 apple"),
            format!("{RELEASE_COMMIT};aarch64-apple-darwin;extra"),
            String::new(),
        ] {
            assert!(!is_clean_released_build("0.1.8", &identity), "{identity}");
        }
        for version in [
            "0.1.8-dev",
            "0.1.8+local",
            "0.1",
            "0.1.8.1",
            "v0.1.8",
            "0..8",
            "",
        ] {
            assert!(!is_clean_released_build(version, &released), "{version}");
        }
    }

    #[test]
    fn unofficial_client_links_the_attempts_first_event_and_quotes_the_identity() {
        let events = [
            edit_event(1, 0, EditOrigin::Keyboard, "x"),
            edit_event(2, 10, EditOrigin::Keyboard, "y"),
        ];
        let released = format!("{RELEASE_COMMIT};aarch64-apple-darwin");
        assert!(
            derive_with_producer(&producer(Some("0.1.8"), Some(&released)), &events).is_empty()
        );

        let dirty = format!("{RELEASE_COMMIT}-dirty;aarch64-apple-darwin");
        let flags = derive_with_producer(&producer(Some("0.1.8"), Some(&dirty)), &events);
        assert_eq!(kinds(&flags), vec![AdvisoryFlagKind::UnofficialClient]);
        assert_eq!(
            flags[0].link,
            VerificationEventLocation {
                segment: 3,
                sequence: 1
            }
        );
        assert_eq!(
            display_advisory(&flags[0]),
            format!(
                "UNOFFICIAL_CLIENT [segment:3 seq:1]: the package metadata for this attempt names a client version or build identity other than a clean released build; measured value: client version 0.1.8; build identity {dirty}; advisory: heuristic; expect false positives"
            )
        );

        let flags = derive_with_producer(&producer(None, Some(&released)), &events);
        assert_eq!(
            flags[0].measured_value,
            format!("client version unavailable; build identity {released}")
        );
        let flags = derive_with_producer(
            &producer(Some("0.1.8"), Some("development-build-unavailable")),
            &events,
        );
        assert_eq!(
            flags[0].measured_value,
            "client version 0.1.8; build identity development-build-unavailable"
        );
        let flags = derive_with_producer(&producer(Some("0.1.8"), None), &events);
        assert_eq!(
            flags[0].measured_value,
            "client version 0.1.8; build identity unavailable"
        );

        // A hostile identity is quoted escaped and bounded.
        let hostile = format!("\u{1b}[31m{}", "z".repeat(1_000));
        let flags = derive_with_producer(&producer(Some(&hostile), Some(&dirty)), &events);
        assert!(!flags[0].measured_value.contains('\u{1b}'));
        assert!(flags[0].measured_value.len() < 2 * MAX_QUOTED_PRODUCER_BYTES + 64);

        // The advisory comes first, once, even with other advisories.
        let mut events = vec![rejected_paste(1, 0, PasteRejectionReason::ExternalInput)];
        events.push(edit_event(
            2,
            5,
            EditOrigin::Keyboard,
            &"x".repeat(LARGE_SINGLE_INSERTION_BYTES),
        ));
        let flags = derive_with_producer(&producer(Some("0.1.8"), Some(&dirty)), &events);
        assert_eq!(
            kinds(&flags),
            vec![
                AdvisoryFlagKind::UnofficialClient,
                AdvisoryFlagKind::LargeSingleInsertion,
                AdvisoryFlagKind::RejectedPasteAttempts,
                AdvisoryFlagKind::TypedAfterRejectedPaste,
            ]
        );
    }

    #[test]
    fn non_keyboard_origins_are_excluded_from_every_advisory() {
        for origin in [
            EditOrigin::Paste,
            EditOrigin::Completion,
            EditOrigin::AdditionalCompletionEdit,
            EditOrigin::Formatter,
            EditOrigin::DependencyTool,
            EditOrigin::CodeAction,
            EditOrigin::Undo,
            EditOrigin::Redo,
            EditOrigin::FileReload,
            EditOrigin::ExternalChange,
            EditOrigin::Unknown,
        ] {
            let events = (0..=60)
                .map(|second| edit_event(second + 1, second * 1_000, origin, &"x".repeat(1_000)))
                .collect::<Vec<_>>();
            assert!(derive(&events).is_empty(), "origin: {origin:?}");
        }
    }

    #[test]
    fn scripted_typing_journal_fires_both_typing_advisories_with_first_event_links_and_neutral_wording()
     {
        let mut events = vec![edit_event(
            10,
            0,
            EditOrigin::Keyboard,
            &"x".repeat(LARGE_SINGLE_INSERTION_BYTES),
        )];
        events.extend((1..=60).map(|second| {
            edit_event(
                second + 10,
                second * 1_000,
                EditOrigin::Keyboard,
                &"x".repeat(16),
            )
        }));

        let flags = derive(&events);
        assert_eq!(
            kinds(&flags),
            vec![
                AdvisoryFlagKind::LargeSingleInsertion,
                AdvisoryFlagKind::SustainedHighRate,
            ]
        );
        assert_eq!(flags[0].link.sequence, 10);
        assert_eq!(flags[1].link.sequence, 10);
        assert!(flags.iter().all(|flag| flag.link.segment == 2));
        for flag in &flags {
            assert!(!flag.measured_value.is_empty());
            assert!(
                display_advisory(flag).ends_with("advisory: heuristic; expect false positives")
            );
        }
        let wording = flags
            .iter()
            .map(display_advisory)
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        for forbidden in ["cheat", "misconduct", "plagiar", "authorship"] {
            assert!(
                !wording.contains(forbidden),
                "forbidden wording: {forbidden}"
            );
        }
    }

    #[test]
    fn names_explanations_and_limitations_avoid_verdict_vocabulary() {
        let mut wording = String::from(EVIDENCE_CONSISTENCY);
        wording.push_str(EVIDENCE_LIMITATION_FIRST);
        wording.push_str(EVIDENCE_LIMITATION_SECOND);
        wording.push_str(EVIDENCE_NOT_EVALUATED);
        wording.push_str(EVIDENCE_LIMITATIONS_BATCH);
        wording.push_str(EVIDENCE_FAILURE);
        for kind in ReviewFlagKind::ALL {
            wording.push_str(kind.name());
            wording.push_str(kind.explanation());
        }
        for kind in AdvisoryFlagKind::ALL {
            wording.push_str(kind.name());
            wording.push_str(kind.explanation());
        }
        wording.push_str(ADVISORY_SUFFIX);
        let wording = wording.to_ascii_lowercase();
        for forbidden in ["cheat", "misconduct", "plagiar", "authorship"] {
            assert!(
                !wording.contains(forbidden),
                "forbidden wording: {forbidden}"
            );
        }
    }

    #[test]
    fn batch_wording_reuses_the_exact_shared_sentences() {
        for sentence in [
            EVIDENCE_CONSISTENCY,
            EVIDENCE_FAILURE,
            EVIDENCE_NOT_EVALUATED,
        ] {
            assert!(EVIDENCE_LIMITATIONS_BATCH.contains(sentence), "{sentence}");
        }
        assert!(EVIDENCE_LIMITATIONS_BATCH.contains(&format!(
            "{EVIDENCE_LIMITATION_FIRST} {EVIDENCE_LIMITATION_SECOND}"
        )));
        assert!(!EVIDENCE_LIMITATIONS_BATCH.starts_with(EVIDENCE_CONSISTENCY));
        assert!(!EVIDENCE_LIMITATIONS_BATCH.contains(','));
    }

    #[test]
    fn evidence_statement_follows_the_package_outcome_not_the_issue_list() {
        let mut clean = VerificationReport::input_failure("placeholder");
        clean.issues.clear();
        clean.package_structure = VerificationStatus::Ok;
        clean.event_chain = VerificationStatus::Ok;
        clean.checkpoint_hashes = VerificationStatus::Ok;
        clean.replay = VerificationStatus::Ok;
        clean.submitted_source_match = SubmittedSourceStatus::Ok;
        clean.assignment_reference = AssignmentReferenceStatus::Unverified;
        assert_eq!(evidence_statement(&clean), EVIDENCE_CONSISTENCY);

        // Mirrors `VerificationReport::record_unavailable` for an unreadable
        // TA-side reference: status stays Unverified, one issue is recorded.
        let mut unreadable_reference = clean.clone();
        unreadable_reference
            .issues
            .push(crate::verify::VerificationIssue {
                kind: VerificationIssueKind::AssignmentReference,
                location: VerificationIssueLocation::Decoder("assignment reference".to_owned()),
                detail: "could not read reference".to_owned(),
            });
        assert_eq!(
            evidence_statement(&unreadable_reference),
            EVIDENCE_NOT_EVALUATED
        );

        let mut unavailable_source = clean.clone();
        unavailable_source.submitted_source_match = SubmittedSourceStatus::Unavailable;
        unavailable_source
            .issues
            .push(crate::verify::VerificationIssue {
                kind: VerificationIssueKind::SubmittedSource,
                location: VerificationIssueLocation::Decoder(
                    "outer submitted source tree".to_owned(),
                ),
                detail: "outer source unreadable".to_owned(),
            });
        assert_eq!(
            evidence_statement(&unavailable_source),
            EVIDENCE_NOT_EVALUATED
        );

        let mut source_mismatch = clean.clone();
        source_mismatch.submitted_source_match = SubmittedSourceStatus::SourceMismatch;
        assert_eq!(evidence_statement(&source_mismatch), EVIDENCE_FAILURE);

        let mut reference_mismatch = clean.clone();
        reference_mismatch.assignment_reference = AssignmentReferenceStatus::Mismatch;
        assert_eq!(evidence_statement(&reference_mismatch), EVIDENCE_FAILURE);

        let mut replay_failed = clean.clone();
        replay_failed.replay = VerificationStatus::Failed;
        assert_eq!(evidence_statement(&replay_failed), EVIDENCE_FAILURE);

        assert_eq!(
            evidence_statement(&VerificationReport::input_failure("bad")),
            EVIDENCE_FAILURE
        );
    }

    #[test]
    fn vocabulary_is_exact_and_stable() {
        assert_eq!(
            ReviewFlagKind::ALL.map(ReviewFlagKind::name),
            [
                "PACKAGE_INVALID",
                "EVENT_CHAIN_INVALID",
                "EVENT_SEQUENCE_GAP",
                "CHECKPOINT_MISMATCH",
                "REPLAY_MISMATCH",
                "SOURCE_MISMATCH",
                "UNPROVENANCED_EXTERNAL_CHANGE",
            ]
        );
    }

    #[test]
    fn mapped_detail_is_bounded_and_safe_for_single_line_display() {
        let report = VerificationReport::input_failure(format!(
            "bad\u{1b}\n{}",
            "x".repeat(MAX_FLAG_DETAIL_BYTES * 4)
        ));
        let flags = review_flags(&report);
        let flag = &flags[0];

        assert!(flag.detail.len() <= MAX_FLAG_DETAIL_BYTES);
        assert!(flag.detail.contains("\\u{1b}\\n"), "{}", flag.detail);
        assert!(!flag.detail.contains('\u{1b}'));
        assert!(!flag.detail.contains('\n'));
    }

    #[test]
    fn unavailable_submitted_source_is_not_a_mismatch_flag() {
        let mut report = VerificationReport::input_failure("invalid package");
        report.issues.push(crate::verify::VerificationIssue {
            kind: VerificationIssueKind::SubmittedSource,
            location: VerificationIssueLocation::Decoder("outer submitted source tree".to_owned()),
            detail: "outer source unavailable".to_owned(),
        });

        assert!(
            review_flags(&report)
                .iter()
                .all(|flag| flag.kind != ReviewFlagKind::SourceMismatch)
        );
    }
}
