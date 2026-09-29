//! Shared hard review-flag vocabulary and verifier mapping.

use crate::{
    display,
    verify::{
        AssignmentReferenceStatus, SubmittedSourceStatus, VerificationEventLocation,
        VerificationIssueKind, VerificationIssueLocation, VerificationReport, VerificationStatus,
    },
};
use rustrace_model::{EditOrigin, Event, EventEnvelope, inserted_text_counts};
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
const ADVISORY_SUFFIX: &str = "advisory: heuristic; expect false positives";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AdvisoryFlagKind {
    LargeSingleInsertion,
    SustainedHighRate,
}

impl AdvisoryFlagKind {
    pub const ALL: [Self; 2] = [Self::LargeSingleInsertion, Self::SustainedHighRate];

    pub const fn name(self) -> &'static str {
        match self {
            Self::LargeSingleInsertion => "LARGE_SINGLE_INSERTION",
            Self::SustainedHighRate => "SUSTAINED_HIGH_RATE",
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

pub(crate) struct TypingShapeAccumulator {
    segment: u32,
    advisories: Vec<AdvisoryFlag>,
    rate_window: VecDeque<KeyboardObservation>,
    first_keyboard_millis: Option<u64>,
    rate_advisory_active: bool,
}

impl TypingShapeAccumulator {
    pub(crate) fn new(segment: u32) -> Self {
        Self {
            segment,
            advisories: Vec::new(),
            rate_window: VecDeque::new(),
            first_keyboard_millis: None,
            rate_advisory_active: false,
        }
    }

    pub(crate) fn observe(&mut self, envelope: &EventEnvelope) {
        let Event::FileEdited(transaction) = &envelope.event else {
            return;
        };
        if transaction.origin != EditOrigin::Keyboard {
            return;
        }

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
        let observation = KeyboardObservation {
            link: VerificationEventLocation {
                segment: self.segment,
                sequence: envelope.sequence,
            },
            monotonic_millis: envelope.monotonic_millis,
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

    pub(crate) fn finish(self) -> Vec<AdvisoryFlag> {
        self.advisories
    }
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
        DocumentId, EditOrigin, EditorTransaction, Event, EventEnvelope, Hash, SelectionState,
        SessionId, TextEdit,
    };

    fn edit_event(sequence: u64, millis: u64, origin: EditOrigin, inserted: &str) -> EventEnvelope {
        EventEnvelope {
            format_version: 1,
            session_id: SessionId::new("advisory-test").unwrap(),
            sequence,
            monotonic_millis: millis,
            wall_clock_utc: None,
            previous_event_hash: Hash::zero(),
            event_hash: Hash::zero(),
            event: Event::FileEdited(EditorTransaction {
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
        }
    }

    fn derive(events: &[EventEnvelope]) -> Vec<AdvisoryFlag> {
        let mut accumulator = TypingShapeAccumulator::new(2);
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
