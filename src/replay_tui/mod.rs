//! Validation-first, read-only TA replay.

mod diff;
mod view;

use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    io::{BufRead, BufReader, Read},
    path::Path,
    sync::Arc,
    time::Duration,
};

#[cfg(test)]
use std::cell::Cell;

use rustrace_journal::{CheckpointSnapshot, StoredCheckpoint, decode_checkpoint};
use rustrace_model::{
    DecodeOutcome, DecodePolicy, DocumentId, EditOrigin, Event, EventEnvelope, MAX_ENVELOPE_BYTES,
    RprovCheckpointRef, RprovCheckpointRole, SelectionState, decode_envelope, inserted_text_counts,
};
use rustrace_replay::{CertifiedCheckpoint, CertifiedCheckpointCursor, ReplayEngine};
use rustrace_workspace::rprov_import::ImportedRprov;

use crate::{
    display,
    process_indicators::indicator_links,
    review_flags::{AdvisoryFlag, ReviewFlag, ReviewFlagLink, review_flags},
    verify::{VerificationReport, validate_replay_input},
};

use diff::compare_files;
pub use diff::{
    ComparisonMode, ComparisonPoint, DiffContent, DiffLine, DiffLineKind, DiffNotice, DiffView,
};
pub use view::run_replay;

pub const SEEK_CACHE_LIMIT_BYTES: usize = 64 * 1024 * 1024;
// Large states stay in the validated package spool and use compact cursors;
// hot projections are an optimization, not permission to consume the G7 RSS
// headroom needed for a transient restore and rendered source.
const HOT_CACHE_ENTRY_LIMIT_BYTES: usize = 8 * 1024 * 1024;
pub const ACCELERATED_SPEED: u64 = 4;
pub const IDLE_GAP_MILLIS: u64 = 2_000;

pub type SourceProjection = Arc<Vec<(String, Vec<u8>)>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackSpeed {
    Normal,
    Accelerated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimingAvailability {
    Recorded,
    UnavailableSynthetic,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct EventPosition {
    pub segment: usize,
    pub sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvidenceTarget {
    Event(EventPosition),
    Artifact(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PasteMarker {
    HistoricalOriginUnverified { characters: u64, lines: u64 },
    AllowedInternal { source: EventPosition },
    BlockedMetadataOnly,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedEvent {
    pub position: EventPosition,
    pub millis: u64,
    pub source: SourceProjection,
    pub active_file: Option<String>,
    pub active_document: Option<SelectedDocument>,
    pub command_output: Vec<u8>,
    pub test_case_comparison: Option<rustrace_model::TestCaseCompared>,
    pub event_bytes: Vec<u8>,
    pub paste_marker: Option<PasteMarker>,
    pub evidence_target: Option<EvidenceTarget>,
    pub inter_attempt_time_unknown: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedDocument {
    pub document_id: DocumentId,
    pub path: String,
    pub selection: SelectionState,
}

impl SelectedDocument {
    fn retained_bytes(&self) -> usize {
        self.document_id
            .as_str()
            .len()
            .saturating_add(self.path.len())
            .saturating_add(std::mem::size_of::<SelectionState>())
    }
}

fn cached_metadata_bytes(active_document: Option<&SelectedDocument>) -> usize {
    active_document.map_or(0, SelectedDocument::retained_bytes)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineRow {
    pub position: EventPosition,
    pub event_name: &'static str,
    pub selected: bool,
    pub attempt_boundary: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReplayCounts {
    pub historical_origin_unverified_pastes: u64,
    pub historical_paste_characters: u64,
    pub historical_paste_lines: u64,
    pub allowed_internal_pastes: u64,
    pub blocked_paste_attempts: u64,
}

impl ReplayCounts {
    fn from_report(report: &VerificationReport) -> Self {
        let Some(indicators) = &report.review_indicators else {
            return Self::default();
        };
        Self {
            historical_origin_unverified_pastes: indicators
                .factual
                .historical_origin_unverified_paste
                .transactions,
            historical_paste_characters: indicators
                .factual
                .historical_origin_unverified_paste
                .inserted_scalars,
            historical_paste_lines: 0,
            allowed_internal_pastes: indicators.factual.allowed_internal_paste.transactions,
            blocked_paste_attempts: indicators.factual.rejected_paste.attempts,
        }
    }
}

#[derive(Clone, Copy)]
struct EventIndex {
    position: EventPosition,
    millis: u64,
    offset: u64,
    length: u32,
    name: &'static str,
    changes_projection: bool,
    command: Option<usize>,
}

#[derive(Default)]
struct CommandEventIndex {
    start: Option<usize>,
    finish: Option<usize>,
    output_events: Vec<usize>,
}

struct CachedCheckpoint {
    certificate: Option<CertifiedCheckpoint>,
    source: SourceProjection,
    active_document: Option<SelectedDocument>,
    charged_bytes: usize,
}

struct CachedCursor {
    cursor: CertifiedCheckpointCursor,
    charged_bytes: usize,
}

pub struct ReplayController {
    report: VerificationReport,
    package: Option<ImportedRprov>,
    events: Vec<EventIndex>,
    commands: Vec<CommandEventIndex>,
    by_position: BTreeMap<EventPosition, usize>,
    selected_index: Option<usize>,
    selected: Option<SelectedEvent>,
    playing: bool,
    speed: PlaybackSpeed,
    skip_idle: bool,
    timing: TimingAvailability,
    playback_budget_millis: u64,
    cache: BTreeMap<EventPosition, CachedCheckpoint>,
    cache_lru: VecDeque<EventPosition>,
    cursors: BTreeMap<EventPosition, CachedCursor>,
    cursor_lru: VecDeque<EventPosition>,
    checkpoint_bytes: RefCell<Vec<u8>>,
    cache_bytes: usize,
    last_seek_applied_events: u64,
    test_events: Option<Vec<EventEnvelope>>,
    counts: ReplayCounts,
    artifact_preview: Option<(String, Vec<u8>)>,
    flag_location_preview: Option<ReviewFlag>,
    next_flag_index: usize,
    next_indicator_index: usize,
    diff_mode: ComparisonMode,
    diff_view: Option<DiffView>,
    #[cfg(test)]
    read_event_calls: Cell<usize>,
    #[cfg(test)]
    select_index_calls: usize,
}

impl ReplayController {
    pub fn open(path: &Path) -> Result<Self, String> {
        let (report, package) = validate_replay_input(path, None);
        let mut controller = Self::empty(report, package);
        if controller.package.is_some() {
            controller.index_events()?;
            controller.warm_checkpoint_cache()?;
            if !controller.events.is_empty() {
                controller.select_index(0)?;
            }
        }
        Ok(controller)
    }

    fn empty(report: VerificationReport, package: Option<ImportedRprov>) -> Self {
        let counts = ReplayCounts::from_report(&report);
        Self {
            report,
            package,
            events: vec![],
            commands: vec![],
            by_position: BTreeMap::new(),
            selected_index: None,
            selected: None,
            playing: false,
            speed: PlaybackSpeed::Normal,
            skip_idle: false,
            timing: TimingAvailability::Recorded,
            playback_budget_millis: 0,
            cache: BTreeMap::new(),
            cache_lru: VecDeque::new(),
            cursors: BTreeMap::new(),
            cursor_lru: VecDeque::new(),
            checkpoint_bytes: RefCell::new(Vec::new()),
            cache_bytes: 0,
            last_seek_applied_events: 0,
            test_events: None,
            counts,
            artifact_preview: None,
            flag_location_preview: None,
            next_flag_index: 0,
            next_indicator_index: 0,
            diff_mode: ComparisonMode::Final,
            diff_view: None,
            #[cfg(test)]
            read_event_calls: Cell::new(0),
            #[cfg(test)]
            select_index_calls: 0,
        }
    }

    pub fn verification_report(&self) -> &VerificationReport {
        &self.report
    }
    pub fn review_flags(&self) -> Vec<ReviewFlag> {
        review_flags(&self.report)
    }
    pub fn advisories(&self) -> &[AdvisoryFlag] {
        &self.report.advisories
    }
    pub fn follow_review_flag(&mut self, flag: &ReviewFlag) -> Result<(), String> {
        self.artifact_preview = None;
        match &flag.link {
            ReviewFlagLink::Event(location) => {
                let segment = usize::try_from(location.segment)
                    .ok()
                    .and_then(|ordinal| ordinal.checked_sub(1));
                let position = segment.map(|segment| EventPosition {
                    segment,
                    sequence: location.sequence,
                });
                if let Some(position) =
                    position.filter(|position| self.by_position.contains_key(position))
                {
                    self.select(position)?;
                }
            }
            ReviewFlagLink::Artifact(path) => {
                self.diff_view = None;
                let entry = self.package.as_ref().and_then(|package| {
                    package
                        .entries()
                        .iter()
                        .find(|entry| entry.path == *path)
                        .map(|entry| entry.byte_length)
                });
                if let (Some(package), Some(length)) = (self.package.as_ref(), entry) {
                    let shown = length.min(display::MAX_INPUT_BYTES as u64);
                    let mut bytes = Vec::with_capacity(shown as usize);
                    package
                        .open_entry_range(path, 0, shown)
                        .map_err(|error| error.to_string())?
                        .read_to_end(&mut bytes)
                        .map_err(|error| error.to_string())?;
                    self.artifact_preview = Some((path.clone(), bytes));
                }
            }
            ReviewFlagLink::Location(_) => {}
        }
        self.flag_location_preview = Some(flag.clone());
        Ok(())
    }
    pub fn follow_next_review_flag(&mut self) -> Result<Option<ReviewFlag>, String> {
        let flags = self.review_flags();
        if flags.is_empty() {
            self.next_flag_index = 0;
            return Ok(None);
        }
        let index = self.next_flag_index % flags.len();
        let flag = flags.get(index).cloned();
        if let Some(flag) = &flag {
            self.follow_review_flag(flag)?;
            self.next_flag_index = (index + 1) % flags.len();
        }
        Ok(flag)
    }
    pub fn follow_next_indicator(&mut self) -> Result<Option<EventPosition>, String> {
        let Some(indicators) = &self.report.review_indicators else {
            self.next_indicator_index = 0;
            return Ok(None);
        };
        let links = indicator_links(indicators);
        if links.is_empty() {
            self.next_indicator_index = 0;
            return Ok(None);
        }
        for offset in 0..links.len() {
            let index = (self.next_indicator_index + offset) % links.len();
            let link = &links[index];
            let segment = usize::try_from(link.segment)
                .ok()
                .and_then(|ordinal| ordinal.checked_sub(1));
            let Some(position) = segment
                .map(|segment| EventPosition {
                    segment,
                    sequence: link.sequence,
                })
                .filter(|position| self.by_position.contains_key(position))
            else {
                continue;
            };
            self.select(position)?;
            self.next_indicator_index = (index + 1) % links.len();
            return Ok(Some(position));
        }
        Ok(None)
    }
    pub fn timeline_available(&self) -> bool {
        self.package.is_some() && !self.events.is_empty()
    }
    pub fn selected_event(&self) -> Option<&SelectedEvent> {
        self.selected.as_ref()
    }
    pub fn event_count(&self) -> usize {
        self.events.len()
    }
    pub fn positions(&self) -> impl Iterator<Item = EventPosition> + '_ {
        self.events.iter().map(|e| e.position)
    }
    pub fn cache_bytes(&self) -> usize {
        self.cache_bytes
    }
    pub fn last_seek_applied_events(&self) -> u64 {
        self.last_seek_applied_events
    }
    pub fn counts(&self) -> ReplayCounts {
        self.counts
    }
    pub fn artifact_preview(&self) -> Option<(&str, &[u8])> {
        self.artifact_preview
            .as_ref()
            .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
    }
    pub fn flag_location_preview(&self) -> Option<&ReviewFlag> {
        self.flag_location_preview.as_ref()
    }
    pub fn is_playing(&self) -> bool {
        self.playing
    }
    pub fn speed(&self) -> PlaybackSpeed {
        self.speed
    }
    pub fn skip_idle(&self) -> bool {
        self.skip_idle
    }

    pub fn comparison_mode(&self) -> ComparisonMode {
        self.diff_mode
    }

    pub fn diff_view(&self) -> Option<&DiffView> {
        self.diff_view.as_ref()
    }

    pub fn toggle_diff(&mut self, path: &str) -> Result<(), String> {
        if self.diff_view.is_some() {
            self.diff_view = None;
        } else {
            let diff = self.build_diff(path, self.diff_mode)?;
            self.artifact_preview = None;
            self.diff_view = Some(diff);
        }
        Ok(())
    }

    pub fn toggle_diff_mode(&mut self, path: &str) -> Result<(), String> {
        let mode = match self.diff_mode {
            ComparisonMode::Final => ComparisonMode::PreviousCheckpoint,
            ComparisonMode::PreviousCheckpoint => ComparisonMode::Final,
        };
        let diff = self
            .diff_view
            .is_some()
            .then(|| self.build_diff(path, mode))
            .transpose()?;
        self.diff_mode = mode;
        self.diff_view = diff;
        Ok(())
    }

    pub fn select_diff_file(&mut self, path: &str) -> Result<(), String> {
        if self.diff_view.is_some() {
            self.diff_view = Some(self.build_diff(path, self.diff_mode)?);
        }
        Ok(())
    }

    fn build_diff(&self, path: &str, mode: ComparisonMode) -> Result<DiffView, String> {
        let selected = self
            .selected
            .as_ref()
            .ok_or_else(|| "diff requires a selected replay event".to_owned())?;
        let current = selected
            .source
            .iter()
            .find(|(candidate, _)| candidate == path)
            .map(|(_, bytes)| bytes.as_slice());
        let Some((comparison, snapshot)) = self.comparison_snapshot(selected.position, mode)?
        else {
            return Ok(DiffView {
                path: path.to_owned(),
                mode,
                comparison: None,
                files: selected
                    .source
                    .iter()
                    .map(|(path, _)| path.clone())
                    .collect(),
                insertions: 0,
                deletions: 0,
                content: DiffContent::Notice(DiffNotice::NoPreviousCheckpoint),
            });
        };
        let baseline = snapshot
            .files()
            .iter()
            .find(|file| file.path.as_str() == path)
            .map(|file| file.contents.as_slice());
        let (insertions, deletions, content) = match mode {
            ComparisonMode::Final => compare_files(current, baseline),
            ComparisonMode::PreviousCheckpoint => compare_files(baseline, current),
        };
        let mut files = selected
            .source
            .iter()
            .map(|(path, _)| path.clone())
            .chain(
                snapshot
                    .files()
                    .iter()
                    .map(|file| file.path.as_str().to_owned()),
            )
            .collect::<Vec<_>>();
        files.sort_unstable();
        files.dedup();
        Ok(DiffView {
            path: path.to_owned(),
            mode,
            comparison: Some(comparison),
            files,
            insertions,
            deletions,
            content,
        })
    }

    fn comparison_snapshot(
        &self,
        selected: EventPosition,
        mode: ComparisonMode,
    ) -> Result<Option<(ComparisonPoint, CheckpointSnapshot)>, String> {
        let package = self
            .package
            .as_ref()
            .ok_or_else(|| "diff requires a validated replay package".to_owned())?;
        let declaration = match mode {
            ComparisonMode::Final => package
                .manifest()
                .segments
                .last()
                .and_then(|segment| {
                    segment
                        .checkpoints
                        .iter()
                        .rev()
                        .find(|checkpoint| checkpoint.role == RprovCheckpointRole::Final)
                })
                .map(|checkpoint| (package.manifest().segments.len() - 1, checkpoint.clone())),
            ComparisonMode::PreviousCheckpoint => package
                .manifest()
                .segments
                .iter()
                .enumerate()
                .flat_map(|(segment, manifest)| {
                    manifest
                        .checkpoints
                        .iter()
                        .map(move |checkpoint| (segment, checkpoint))
                })
                .filter(|(segment, checkpoint)| {
                    EventPosition {
                        segment: *segment,
                        sequence: checkpoint.owner.sequence,
                    } < selected
                })
                .max_by_key(|(segment, checkpoint)| (*segment, checkpoint.owner.sequence))
                .map(|(segment, checkpoint)| (segment, checkpoint.clone())),
        };
        let Some((segment, declaration)) = declaration else {
            return match mode {
                ComparisonMode::Final => {
                    Err("validated bundle has no tip final checkpoint".to_owned())
                }
                ComparisonMode::PreviousCheckpoint => Ok(None),
            };
        };
        let snapshot = self.decode_checkpoint(segment, &declaration)?.snapshot;
        Ok(Some((
            ComparisonPoint {
                segment,
                sequence: declaration.owner.sequence,
            },
            snapshot,
        )))
    }

    pub fn timeline_rows(&self, radius: usize) -> Vec<TimelineRow> {
        let selected = self.selected_index.unwrap_or(0);
        let start = selected.saturating_sub(radius);
        let end = self.events.len().min(selected.saturating_add(radius + 1));
        self.events[start..end]
            .iter()
            .enumerate()
            .map(|(offset, event)| TimelineRow {
                position: event.position,
                event_name: event.name,
                selected: start + offset == selected,
                attempt_boundary: event.position.segment > 0 && event.position.sequence == 1,
            })
            .collect()
    }

    pub fn select(&mut self, position: EventPosition) -> Result<(), String> {
        let index = *self
            .by_position
            .get(&position)
            .ok_or_else(|| "event is not present in the validated timeline".to_owned())?;
        self.select_manual_index(index)?;
        Ok(())
    }

    pub fn next_event(&mut self) -> Result<(), String> {
        self.pause_manual_navigation();
        let Some(index) = self.selected_index else {
            return Ok(());
        };
        if index + 1 >= self.events.len() {
            return Ok(());
        }
        self.select_index(index + 1)?;
        Ok(())
    }

    pub fn previous_event(&mut self) -> Result<(), String> {
        self.pause_manual_navigation();
        let Some(index) = self.selected_index else {
            return Ok(());
        };
        if index > 0 {
            self.select_index(index - 1)?;
        }
        Ok(())
    }

    fn scroll_events(&mut self, delta: isize, admitted: bool) -> Result<bool, String> {
        self.pause_manual_navigation();
        if !admitted {
            return Ok(false);
        }
        let Some(index) = self.selected_index else {
            return Ok(false);
        };
        let last = self.events.len().saturating_sub(1);
        let target = index.saturating_add_signed(delta).min(last);
        if target == index {
            return Ok(false);
        }
        self.select_index(target)?;
        Ok(true)
    }

    fn select_manual_index(&mut self, index: usize) -> Result<bool, String> {
        self.pause_manual_navigation();
        if self.selected_index == Some(index) {
            return Ok(false);
        }
        self.select_index(index)?;
        Ok(true)
    }

    fn pause_manual_navigation(&mut self) {
        self.playing = false;
        self.playback_budget_millis = 0;
    }

    pub fn toggle_play(&mut self) {
        if self.events.is_empty() {
            self.playing = false;
            self.playback_budget_millis = 0;
            return;
        }
        self.playing = !self.playing;
        self.playback_budget_millis = 0;
    }
    pub fn toggle_speed(&mut self) {
        self.speed = match self.speed {
            PlaybackSpeed::Normal => PlaybackSpeed::Accelerated,
            PlaybackSpeed::Accelerated => PlaybackSpeed::Normal,
        };
    }
    pub fn toggle_skip_idle(&mut self) {
        self.skip_idle = !self.skip_idle;
    }

    fn credit_playback(&mut self, elapsed: Duration) {
        if !self.playing || self.timing == TimingAvailability::UnavailableSynthetic {
            return;
        }
        let multiplier = if self.speed == PlaybackSpeed::Accelerated {
            ACCELERATED_SPEED
        } else {
            1
        };
        let added = u64::try_from(elapsed.as_millis())
            .unwrap_or(u64::MAX)
            .saturating_mul(multiplier);
        self.playback_budget_millis = self.playback_budget_millis.saturating_add(added);
    }

    pub fn advance_playback(&mut self, elapsed: Duration) -> Result<bool, String> {
        if !self.playing || self.timing == TimingAvailability::UnavailableSynthetic {
            return Ok(false);
        }
        let Some(index) = self.selected_index else {
            return Ok(false);
        };
        let Some(next) = self.events.get(index + 1).copied() else {
            self.playing = false;
            return Ok(false);
        };
        let current = self.events[index];
        let changed_segment = current.position.segment != next.position.segment;
        let gap = if changed_segment {
            0
        } else {
            next.millis.saturating_sub(current.millis)
        };
        if changed_segment || (self.skip_idle && gap > IDLE_GAP_MILLIS) {
            self.playback_budget_millis = 0;
            self.select_index(index + 1)?;
            return Ok(true);
        }
        self.credit_playback(elapsed);
        if self.playback_budget_millis < gap {
            return Ok(false);
        }
        self.playback_budget_millis -= gap;
        self.select_index(index + 1)?;
        Ok(true)
    }

    pub fn timing_label(&self) -> &'static str {
        match self.timing {
            TimingAvailability::Recorded => "recorded timing",
            TimingAvailability::UnavailableSynthetic => "timing unavailable (synthetic fixture)",
        }
    }

    pub fn follow_selected_evidence(&mut self) -> Result<Option<EvidenceTarget>, String> {
        let target = self
            .selected
            .as_ref()
            .and_then(|selected| selected.evidence_target.clone());
        match &target {
            Some(EvidenceTarget::Event(position)) => self.select(*position)?,
            Some(EvidenceTarget::Artifact(path)) => {
                self.diff_view = None;
                let package = self.package.as_ref().expect("validated package");
                let length = package
                    .entries()
                    .iter()
                    .find(|entry| entry.path == *path)
                    .map(|entry| entry.byte_length)
                    .ok_or_else(|| "evidence artifact is absent".to_owned())?;
                let shown = length.min(display::MAX_INPUT_BYTES as u64);
                let mut bytes = Vec::with_capacity(shown as usize);
                package
                    .open_entry_range(path, 0, shown)
                    .map_err(|error| error.to_string())?
                    .read_to_end(&mut bytes)
                    .map_err(|error| error.to_string())?;
                self.artifact_preview = Some((path.clone(), bytes));
            }
            None => {}
        }
        Ok(target)
    }

    fn index_events(&mut self) -> Result<(), String> {
        let event_entries = self
            .package
            .as_ref()
            .expect("validated package")
            .manifest()
            .segments
            .iter()
            .map(|segment| segment.events.entry.clone())
            .collect::<Vec<_>>();
        for (segment_index, entry) in event_entries.iter().enumerate() {
            let mut commands = BTreeMap::new();
            let mut current_command = None;
            let mut reader = BufReader::with_capacity(
                8 * 1024,
                self.package
                    .as_ref()
                    .expect("validated package")
                    .open_entry(entry)
                    .map_err(|e| e.to_string())?,
            );
            let mut line = Vec::new();
            let mut offset = 0_u64;
            loop {
                line.clear();
                let read = reader
                    .by_ref()
                    .take((MAX_ENVELOPE_BYTES as u64).saturating_add(2))
                    .read_until(b'\n', &mut line)
                    .map_err(|e| e.to_string())?;
                if read == 0 {
                    break;
                }
                if line.last() != Some(&b'\n') || line.len() > MAX_ENVELOPE_BYTES + 1 {
                    return Err("validated event stream changed while indexing".to_owned());
                }
                line.pop();
                let envelope = decode_event(&line)?;
                self.count_paste_lines(&envelope.event)?;
                let position = EventPosition {
                    segment: segment_index,
                    sequence: envelope.sequence,
                };
                let index = self.events.len();
                if self.by_position.insert(position, index).is_some() {
                    return Err("duplicate event position".to_owned());
                }
                let command = index_command_event(
                    &mut self.commands,
                    &mut commands,
                    &mut current_command,
                    index,
                    &envelope.event,
                );
                self.events.push(EventIndex {
                    position,
                    millis: envelope.monotonic_millis,
                    offset,
                    length: u32::try_from(line.len())
                        .map_err(|_| "event length overflow".to_owned())?,
                    name: event_name(&envelope.event),
                    changes_projection: changes_projection(&envelope.event),
                    command,
                });
                offset = offset
                    .checked_add(read as u64)
                    .ok_or_else(|| "event offset overflow".to_owned())?;
            }
        }
        Ok(())
    }

    fn select_index(&mut self, index: usize) -> Result<(), String> {
        #[cfg(test)]
        {
            self.select_index_calls = self.select_index_calls.saturating_add(1);
        }
        if self.package.is_none() {
            #[cfg(test)]
            if self.test_events.is_some() {
                let indexed = *self
                    .events
                    .get(index)
                    .ok_or_else(|| "event index is outside the timeline".to_owned())?;
                let selected_envelope = self.read_event(index)?;
                let command_output = self.command_output_through(index)?;
                let test_case_comparison = match &selected_envelope.event {
                    Event::TestCaseCompared(comparison) => Some(comparison.clone()),
                    _ => None,
                };
                self.selected_index = Some(index);
                self.diff_view = None;
                self.selected = Some(SelectedEvent {
                    position: indexed.position,
                    millis: indexed.millis,
                    source: Arc::new(vec![]),
                    active_file: None,
                    active_document: None,
                    command_output,
                    test_case_comparison,
                    event_bytes: rustrace_model::encode_envelope(&selected_envelope)
                        .map_err(|error| error.to_string())?,
                    paste_marker: None,
                    evidence_target: None,
                    inter_attempt_time_unknown: false,
                });
                return Ok(());
            }
            self.selected_index = Some(index);
            return Ok(());
        }
        let indexed = *self
            .events
            .get(index)
            .ok_or_else(|| "event index is outside the timeline".to_owned())?;
        let projected = self.cached_projection(indexed.position);
        let (source, active_file, active_document, applied) =
            if let Some((source, active_file, active_document)) = projected {
                (source, active_file, active_document, 0)
            } else {
                let (replay, applied) = self.replay_to(indexed.position)?;
                let active_document = selected_document_from_replay(&replay);
                let (files, active_file) = replay.into_workspace_projection();
                let source = Arc::new(
                    files
                        .into_iter()
                        .map(|(path, bytes)| (path.as_str().to_owned(), bytes))
                        .collect(),
                );
                let active_file = active_file.map(|path| path.as_str().to_owned());
                (source, active_file, active_document, applied)
            };
        self.last_seek_applied_events = applied;
        let envelope = self.read_event(index)?;
        let paste_marker = self.paste_marker(&envelope.event)?;
        let evidence_target = self.evidence_target(indexed.position, &envelope.event);
        let test_case_comparison = match &envelope.event {
            Event::TestCaseCompared(comparison) => Some(comparison.clone()),
            _ => None,
        };
        let command_output = self.command_output_through(index)?;
        let event_bytes = self.read_event_bytes(index)?;
        self.selected_index = Some(index);
        self.artifact_preview = None;
        self.flag_location_preview = None;
        self.diff_view = None;
        self.selected = Some(SelectedEvent {
            position: indexed.position,
            millis: indexed.millis,
            source,
            active_file,
            active_document,
            command_output,
            test_case_comparison,
            event_bytes,
            paste_marker,
            evidence_target,
            inter_attempt_time_unknown: indexed.position.segment > 0
                && indexed.position.sequence == 1,
        });
        Ok(())
    }

    fn replay_to(&mut self, target: EventPosition) -> Result<(ReplayEngine, u64), String> {
        let checkpoint = self.latest_checkpoint_at_or_before(target)?.clone();
        let key = EventPosition {
            segment: target.segment,
            sequence: checkpoint.owner.sequence,
        };
        let mut replay = if checkpoint.owner.sequence == 1 {
            self.initial_replay(target.segment)?
        } else if let Some(cached) = self.cache.get(&key) {
            ReplayEngine::from_checkpoint(
                cached
                    .certificate
                    .clone()
                    .ok_or_else(|| "cached checkpoint has no certificate".to_owned())?,
            )
        } else if let Some(cursor) = self.cursors.get(&key).map(|cached| cached.cursor.clone()) {
            self.touch_cursor(key);
            ReplayEngine::from_checkpoint_cursor(
                cursor,
                self.decode_checkpoint(target.segment, &checkpoint)?,
            )
            .map_err(|error| error.to_string())?
        } else {
            let certificate = self.certify_checkpoint(target.segment, &checkpoint)?;
            let cursor = certificate.cursor();
            let cursor_charge = cursor.retained_bytes();
            let replay = ReplayEngine::from_checkpoint(certificate.clone());
            let (source, active_document) = self.checkpoint_projection(&checkpoint)?;
            self.insert_cursor(target.segment, &checkpoint, cursor);
            self.insert_cache(
                target.segment,
                &checkpoint,
                certificate,
                source,
                active_document,
                cursor_charge,
            )?;
            replay
        };
        let mut applied = 0_u64;
        for sequence in replay.next_sequence()..=target.sequence {
            let index = *self
                .by_position
                .get(&EventPosition {
                    segment: target.segment,
                    sequence,
                })
                .ok_or_else(|| "validated event is missing from the replay index".to_owned())?;
            replay
                .apply(&self.read_event(index)?)
                .map_err(|e| e.to_string())?;
            applied = applied.saturating_add(1);
        }
        self.touch_cache(key);
        Ok((replay, applied))
    }

    fn certify_checkpoint(
        &self,
        segment: usize,
        checkpoint: &RprovCheckpointRef,
    ) -> Result<CertifiedCheckpoint, String> {
        let prior = self
            .cache
            .range(
                EventPosition {
                    segment,
                    sequence: 2,
                }..EventPosition {
                    segment,
                    sequence: checkpoint.owner.sequence,
                },
            )
            .next_back();
        let mut replay = prior.map_or_else(
            || self.initial_replay(segment),
            |(_, cached)| {
                cached.certificate.clone().map_or_else(
                    || self.initial_replay(segment),
                    |certificate| Ok(ReplayEngine::from_checkpoint(certificate)),
                )
            },
        )?;
        for sequence in replay.next_sequence()..=checkpoint.owner.sequence {
            let index = *self
                .by_position
                .get(&EventPosition { segment, sequence })
                .ok_or_else(|| "checkpoint prefix event is missing".to_owned())?;
            replay
                .apply(&self.read_event(index)?)
                .map_err(|e| e.to_string())?;
        }
        replay
            .certify_checkpoint(self.decode_checkpoint(segment, checkpoint)?)
            .map_err(|e| e.to_string())
    }

    /// The verifier has already established package semantics. This second
    /// streaming pass materializes only bounded seek certificates, never a
    /// state per event, so ordinary selection starts from a certified point.
    fn warm_checkpoint_cache(&mut self) -> Result<(), String> {
        let segment_count = self
            .package
            .as_ref()
            .expect("validated package")
            .manifest()
            .segments
            .len();
        for segment_index in 0..segment_count {
            let mut replay = self.initial_replay(segment_index)?;
            self.insert_initial_projection(segment_index, &replay)?;
            let positions = self
                .events
                .iter()
                .filter(|event| {
                    event.position.segment == segment_index && event.position.sequence > 1
                })
                .map(|event| event.position)
                .collect::<Vec<_>>();
            for position in positions {
                let event_index = self.by_position[&position];
                let envelope = self.read_event(event_index)?;
                replay.apply(&envelope).map_err(|error| error.to_string())?;
                if matches!(envelope.event, Event::WorkspaceCheckpoint(_)) {
                    let declaration = self
                        .package
                        .as_ref()
                        .expect("validated package")
                        .manifest()
                        .segments[segment_index]
                        .checkpoints
                        .iter()
                        .find(|checkpoint| checkpoint.owner.sequence == position.sequence)
                        .cloned()
                        .ok_or_else(|| "checkpoint declaration is missing".to_owned())?;
                    let stored = self.decode_checkpoint(segment_index, &declaration)?;
                    let cursor = replay
                        .certify_checkpoint_cursor(&stored)
                        .map_err(|error| error.to_string())?;
                    let cursor_charge = cursor.retained_bytes();
                    self.insert_cursor(segment_index, &declaration, cursor);
                    let is_last = self
                        .package
                        .as_ref()
                        .expect("validated package")
                        .manifest()
                        .segments[segment_index]
                        .checkpoints
                        .last()
                        .is_some_and(|checkpoint| checkpoint.owner == declaration.owner);
                    if is_last && declaration.byte_length <= HOT_CACHE_ENTRY_LIMIT_BYTES as u64 {
                        let certificate = replay
                            .certify_checkpoint(stored)
                            .map_err(|error| error.to_string())?;
                        let source = Arc::new(
                            replay
                                .workspace_state()
                                .files()
                                .iter()
                                .map(|(path, bytes)| (path.as_str().to_owned(), bytes.clone()))
                                .collect(),
                        );
                        let active_document = selected_document_from_replay(&replay);
                        self.insert_cache(
                            segment_index,
                            &declaration,
                            certificate,
                            source,
                            active_document,
                            cursor_charge,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn initial_replay(&self, segment: usize) -> Result<ReplayEngine, String> {
        let declaration = self
            .package
            .as_ref()
            .expect("validated package")
            .manifest()
            .segments
            .get(segment)
            .and_then(|segment| segment.checkpoints.first())
            .ok_or_else(|| "validated segment has no initial checkpoint".to_owned())?;
        ReplayEngine::from_initial_checkpoint(self.decode_checkpoint(segment, declaration)?)
            .map_err(|e| e.to_string())
    }

    fn latest_checkpoint_at_or_before(
        &self,
        target: EventPosition,
    ) -> Result<&RprovCheckpointRef, String> {
        self.package
            .as_ref()
            .expect("validated package")
            .manifest()
            .segments
            .get(target.segment)
            .and_then(|segment| {
                segment
                    .checkpoints
                    .iter()
                    .rev()
                    .find(|cp| cp.owner.sequence <= target.sequence)
            })
            .ok_or_else(|| "validated segment has no checkpoint before the event".to_owned())
    }

    fn decode_checkpoint(
        &self,
        segment: usize,
        checkpoint: &RprovCheckpointRef,
    ) -> Result<StoredCheckpoint, String> {
        let package = self.package.as_ref().expect("validated package");
        let expected = usize::try_from(checkpoint.byte_length)
            .map_err(|_| "checkpoint length does not fit this platform".to_owned())?;
        let mut bytes = self.checkpoint_bytes.borrow_mut();
        bytes.clear();
        if bytes.capacity() < expected {
            bytes.reserve(expected);
        }
        package
            .open_entry(&checkpoint.entry)
            .map_err(|error| error.to_string())?
            .take(checkpoint.byte_length.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() != expected {
            return Err("checkpoint entry changed length after validation".to_owned());
        }
        let snapshot = decode_checkpoint(&bytes).map_err(|error| error.to_string())?;
        drop(bytes);
        let event_index = *self
            .by_position
            .get(&EventPosition {
                segment,
                sequence: checkpoint.owner.sequence,
            })
            .ok_or_else(|| "checkpoint owner event is missing".to_owned())?;
        Ok(StoredCheckpoint {
            owning_event: self.read_event(event_index)?,
            snapshot,
        })
    }

    fn insert_cache(
        &mut self,
        segment: usize,
        checkpoint: &RprovCheckpointRef,
        certificate: CertifiedCheckpoint,
        source: SourceProjection,
        active_document: Option<SelectedDocument>,
        cursor_charge: usize,
    ) -> Result<(), String> {
        let package = self.package.as_ref().expect("validated package");
        let bytes = read_entry(package, &checkpoint.entry, checkpoint.byte_length)?;
        let snapshot = decode_checkpoint(&bytes).map_err(|e| e.to_string())?;
        let charged_bytes = snapshot
            .total_file_bytes()
            .saturating_mul(3)
            .saturating_add(snapshot.files().len().saturating_mul(512))
            .saturating_add(snapshot.documents().len().saturating_mul(512))
            .saturating_add(cached_metadata_bytes(active_document.as_ref()))
            .saturating_add(cursor_charge);
        if charged_bytes > HOT_CACHE_ENTRY_LIMIT_BYTES {
            return Ok(());
        }
        let key = EventPosition {
            segment,
            sequence: checkpoint.owner.sequence,
        };
        if let Some(old) = self.cache.remove(&key) {
            self.cache_bytes = self.cache_bytes.saturating_sub(old.charged_bytes);
            self.cache_lru.retain(|entry| *entry != key);
        }
        while self.cache_bytes.saturating_add(charged_bytes) > SEEK_CACHE_LIMIT_BYTES {
            if let Some(oldest) = self.cache_lru.pop_front() {
                if let Some(removed) = self.cache.remove(&oldest) {
                    self.cache_bytes = self.cache_bytes.saturating_sub(removed.charged_bytes);
                }
            } else if let Some(oldest) = self.cursor_lru.pop_front() {
                if let Some(removed) = self.cursors.remove(&oldest) {
                    self.cache_bytes = self.cache_bytes.saturating_sub(removed.charged_bytes);
                }
            } else {
                break;
            }
        }
        if self.cache_bytes.saturating_add(charged_bytes) > SEEK_CACHE_LIMIT_BYTES {
            return Ok(());
        }
        self.cache.insert(
            key,
            CachedCheckpoint {
                certificate: Some(certificate),
                source,
                active_document,
                charged_bytes,
            },
        );
        self.cache_bytes = self.cache_bytes.saturating_add(charged_bytes);
        self.touch_cache(key);
        Ok(())
    }

    fn insert_cursor(
        &mut self,
        segment: usize,
        checkpoint: &RprovCheckpointRef,
        cursor: CertifiedCheckpointCursor,
    ) {
        let charged_bytes = cursor.retained_bytes();
        if charged_bytes > SEEK_CACHE_LIMIT_BYTES {
            return;
        }
        let key = EventPosition {
            segment,
            sequence: checkpoint.owner.sequence,
        };
        if let Some(old) = self.cursors.remove(&key) {
            self.cache_bytes = self.cache_bytes.saturating_sub(old.charged_bytes);
            self.cursor_lru.retain(|entry| *entry != key);
        }
        while self.cache_bytes.saturating_add(charged_bytes) > SEEK_CACHE_LIMIT_BYTES {
            let Some(oldest) = self.cursor_lru.pop_front() else {
                break;
            };
            if let Some(removed) = self.cursors.remove(&oldest) {
                self.cache_bytes = self.cache_bytes.saturating_sub(removed.charged_bytes);
            }
        }
        if self.cache_bytes.saturating_add(charged_bytes) > SEEK_CACHE_LIMIT_BYTES {
            return;
        }
        self.cursors.insert(
            key,
            CachedCursor {
                cursor,
                charged_bytes,
            },
        );
        self.cache_bytes = self.cache_bytes.saturating_add(charged_bytes);
        self.touch_cursor(key);
    }

    fn insert_initial_projection(
        &mut self,
        segment: usize,
        replay: &ReplayEngine,
    ) -> Result<(), String> {
        let source = Arc::new(
            replay
                .workspace_state()
                .files()
                .iter()
                .map(|(path, bytes)| (path.as_str().to_owned(), bytes.clone()))
                .collect::<Vec<_>>(),
        );
        let active_document = selected_document_from_replay(replay);
        let charged_bytes = source
            .iter()
            .map(|(path, bytes)| path.len().saturating_add(bytes.len()).saturating_add(128))
            .sum::<usize>()
            .saturating_add(cached_metadata_bytes(active_document.as_ref()));
        if charged_bytes > HOT_CACHE_ENTRY_LIMIT_BYTES {
            return Ok(());
        }
        let key = EventPosition {
            segment,
            sequence: 1,
        };
        if let Some(old) = self.cache.remove(&key) {
            self.cache_bytes = self.cache_bytes.saturating_sub(old.charged_bytes);
            self.cache_lru.retain(|entry| *entry != key);
        }
        while self.cache_bytes.saturating_add(charged_bytes) > SEEK_CACHE_LIMIT_BYTES {
            let Some(oldest) = self.cache_lru.pop_front() else {
                break;
            };
            if let Some(removed) = self.cache.remove(&oldest) {
                self.cache_bytes = self.cache_bytes.saturating_sub(removed.charged_bytes);
            }
        }
        if self.cache_bytes.saturating_add(charged_bytes) > SEEK_CACHE_LIMIT_BYTES {
            return Ok(());
        }
        self.cache.insert(
            key,
            CachedCheckpoint {
                certificate: None,
                source,
                active_document,
                charged_bytes,
            },
        );
        self.cache_bytes = self.cache_bytes.saturating_add(charged_bytes);
        self.touch_cache(key);
        Ok(())
    }

    fn checkpoint_projection(
        &self,
        checkpoint: &RprovCheckpointRef,
    ) -> Result<(SourceProjection, Option<SelectedDocument>), String> {
        let package = self.package.as_ref().expect("validated package");
        let bytes = read_entry(package, &checkpoint.entry, checkpoint.byte_length)?;
        let snapshot = decode_checkpoint(&bytes).map_err(|error| error.to_string())?;
        let source = Arc::new(
            snapshot
                .files()
                .iter()
                .map(|file| (file.path.as_str().to_owned(), file.contents.clone()))
                .collect(),
        );
        let active_document = selected_document_from_snapshot(&snapshot);
        Ok((source, active_document))
    }

    fn cached_projection(
        &self,
        target: EventPosition,
    ) -> Option<(SourceProjection, Option<String>, Option<SelectedDocument>)> {
        let checkpoint = self.latest_checkpoint_at_or_before(target).ok()?;
        let key = EventPosition {
            segment: target.segment,
            sequence: checkpoint.owner.sequence,
        };
        let cached = self.cache.get(&key)?;
        let unchanged = (checkpoint.owner.sequence + 1..=target.sequence).all(|sequence| {
            self.by_position
                .get(&EventPosition {
                    segment: target.segment,
                    sequence,
                })
                .and_then(|index| self.events.get(*index))
                .is_some_and(|event| !event.changes_projection)
        });
        unchanged.then(|| {
            let active_document = cached.active_document.clone();
            let active_file = active_document
                .as_ref()
                .map(|document| document.path.clone());
            (Arc::clone(&cached.source), active_file, active_document)
        })
    }

    fn touch_cache(&mut self, key: EventPosition) {
        if self.cache.contains_key(&key) {
            self.cache_lru.retain(|entry| *entry != key);
            self.cache_lru.push_back(key);
        }
    }

    fn touch_cursor(&mut self, key: EventPosition) {
        if self.cursors.contains_key(&key) {
            self.cursor_lru.retain(|entry| *entry != key);
            self.cursor_lru.push_back(key);
        }
    }

    fn read_event(&self, index: usize) -> Result<EventEnvelope, String> {
        #[cfg(test)]
        self.read_event_calls
            .set(self.read_event_calls.get().saturating_add(1));
        if let Some(events) = &self.test_events {
            return events
                .get(index)
                .cloned()
                .ok_or_else(|| "test event index is outside the timeline".to_owned());
        }
        decode_event(&self.read_event_bytes(index)?)
    }

    fn read_event_bytes(&self, index: usize) -> Result<Vec<u8>, String> {
        let indexed = self
            .events
            .get(index)
            .ok_or_else(|| "event index is outside the timeline".to_owned())?;
        let package = self.package.as_ref().expect("validated package");
        let entry = &package.manifest().segments[indexed.position.segment]
            .events
            .entry;
        let mut reader = package
            .open_entry_range(entry, indexed.offset, u64::from(indexed.length))
            .map_err(|e| e.to_string())?;
        let mut bytes = Vec::with_capacity(indexed.length as usize);
        reader.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        if bytes.len() != indexed.length as usize {
            return Err("event entry changed length after validation".to_owned());
        }
        Ok(bytes)
    }

    fn paste_marker(&self, event: &Event) -> Result<Option<PasteMarker>, String> {
        match event {
            Event::FileEdited(tx) if tx.origin == EditOrigin::Paste => {
                let (characters, lines) = tx
                    .edits
                    .iter()
                    .try_fold((0_u64, 0_u64), |(chars, lines), edit| {
                        let counts = inserted_text_counts(&edit.inserted_text);
                        Some((
                            chars.checked_add(u64::try_from(counts.character_count).ok()?)?,
                            lines.checked_add(u64::try_from(counts.line_count).ok()?)?,
                        ))
                    })
                    .ok_or_else(|| "paste count overflow".to_owned())?;
                Ok(Some(PasteMarker::HistoricalOriginUnverified {
                    characters,
                    lines,
                }))
            }
            Event::InternalPaste(paste) => Ok(Some(PasteMarker::AllowedInternal {
                source: self.resolve_event_ref(&paste.source)?,
            })),
            Event::PasteRejected(_) => Ok(Some(PasteMarker::BlockedMetadataOnly)),
            _ => Ok(None),
        }
    }

    fn evidence_target(&self, position: EventPosition, event: &Event) -> Option<EvidenceTarget> {
        if let Event::InternalPaste(paste) = event
            && let Ok(source) = self.resolve_event_ref(&paste.source)
        {
            return Some(EvidenceTarget::Event(source));
        }
        let package = self.package.as_ref()?;
        let segment = package.manifest().segments.get(position.segment)?;
        segment
            .evidence
            .iter()
            .find(|evidence| {
                evidence.usages.iter().any(|usage| {
                    usage.session_id == segment.session_id && usage.sequence == position.sequence
                })
            })
            .map(|evidence| EvidenceTarget::Artifact(evidence.entry.clone()))
    }

    fn resolve_event_ref(
        &self,
        reference: &rustrace_model::RecordedEventRef,
    ) -> Result<EventPosition, String> {
        let package = self.package.as_ref().expect("validated package");
        let segment = package
            .manifest()
            .segments
            .iter()
            .enumerate()
            .find(|(_, segment)| segment.session_id == reference.session_id)
            .map(|(index, _)| index)
            .ok_or_else(|| "source link session is absent".to_owned())?;
        let position = EventPosition {
            segment,
            sequence: reference.sequence,
        };
        self.by_position
            .contains_key(&position)
            .then_some(position)
            .ok_or_else(|| "source link event is absent".to_owned())
    }

    fn command_output_through(&self, selected_index: usize) -> Result<Vec<u8>, String> {
        let selected = self.events[selected_index];
        let Some(command) = selected.command.and_then(|index| self.commands.get(index)) else {
            return Ok(Vec::new());
        };
        let first = command
            .start
            .or_else(|| command.output_events.first().copied())
            .unwrap_or(selected_index);
        if selected_index < first {
            return Ok(Vec::new());
        }
        let last = command.finish.unwrap_or(selected_index).min(selected_index);
        let mut bytes = Vec::new();
        for &index in command
            .output_events
            .iter()
            .take_while(|&&index| index <= last)
        {
            match self.read_event(index)?.event {
                Event::ControlledCommandOutput(output) => {
                    append_hex_bounded(&mut bytes, &output.bytes_hex)?;
                }
                Event::CargoOutput(output) => {
                    append_bounded(&mut bytes, output.output.as_bytes());
                }
                _ => {}
            }
            if bytes.len() == rustrace_model::MAX_COMMAND_OUTPUT_BYTES as usize {
                break;
            }
        }
        Ok(bytes)
    }

    fn count_paste_lines(&mut self, event: &Event) -> Result<(), String> {
        if let Event::FileEdited(transaction) = event
            && transaction.origin == EditOrigin::Paste
        {
            for edit in &transaction.edits {
                let counts = inserted_text_counts(&edit.inserted_text);
                self.counts.historical_paste_lines = self
                    .counts
                    .historical_paste_lines
                    .checked_add(
                        u64::try_from(counts.line_count)
                            .map_err(|_| "paste line count overflow")?,
                    )
                    .ok_or_else(|| "paste line count overflow".to_owned())?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn from_test_events(
        report: VerificationReport,
        timing: TimingAvailability,
        events: Vec<EventEnvelope>,
    ) -> Self {
        let mut controller = Self::empty(report, None);
        controller.timing = timing;
        controller.test_events = Some(events.clone());
        let mut commands = BTreeMap::new();
        let mut current_command = None;
        for (index, envelope) in events.into_iter().enumerate() {
            let position = EventPosition {
                segment: 0,
                sequence: envelope.sequence,
            };
            controller.by_position.insert(position, index);
            let command = index_command_event(
                &mut controller.commands,
                &mut commands,
                &mut current_command,
                index,
                &envelope.event,
            );
            controller.events.push(EventIndex {
                position,
                millis: envelope.monotonic_millis,
                offset: 0,
                length: 0,
                name: event_name(&envelope.event),
                changes_projection: changes_projection(&envelope.event),
                command,
            });
        }
        if let Some(first) = controller.events.first() {
            let envelope = controller
                .test_events
                .as_ref()
                .and_then(|events| events.first())
                .expect("first indexed test event");
            let event_bytes =
                rustrace_model::encode_envelope(envelope).expect("test event must encode");
            let test_case_comparison = match &envelope.event {
                Event::TestCaseCompared(comparison) => Some(comparison.clone()),
                _ => None,
            };
            controller.selected_index = Some(0);
            controller.selected = Some(SelectedEvent {
                position: first.position,
                millis: first.millis,
                source: Arc::new(vec![]),
                active_file: None,
                active_document: None,
                command_output: vec![],
                test_case_comparison,
                event_bytes,
                paste_marker: None,
                evidence_target: None,
                inter_attempt_time_unknown: false,
            });
        }
        controller
    }
}

fn decode_event(bytes: &[u8]) -> Result<EventEnvelope, String> {
    match decode_envelope(bytes, DecodePolicy::RejectUnsupported).map_err(|e| e.to_string())? {
        DecodeOutcome::Decoded(event) => Ok(event),
        DecodeOutcome::Skipped(_) => Err("unsupported event was not rejected".to_owned()),
    }
}

fn read_entry(package: &ImportedRprov, path: &str, length: u64) -> Result<Vec<u8>, String> {
    let expected = usize::try_from(length)
        .map_err(|_| "entry length does not fit this platform".to_owned())?;
    let mut bytes = Vec::with_capacity(expected.min(1024 * 1024));
    package
        .open_entry(path)
        .map_err(|e| e.to_string())?
        .take(length.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() != expected {
        return Err("entry changed length after validation".to_owned());
    }
    Ok(bytes)
}

fn append_bounded(target: &mut Vec<u8>, bytes: &[u8]) {
    let available =
        (rustrace_model::MAX_COMMAND_OUTPUT_BYTES as usize).saturating_sub(target.len());
    target.extend_from_slice(&bytes[..bytes.len().min(available)]);
}

fn append_hex_bounded(target: &mut Vec<u8>, hex: &str) -> Result<(), String> {
    if !hex.len().is_multiple_of(2) {
        return Err("command output has odd hex length".to_owned());
    }
    let available =
        (rustrace_model::MAX_COMMAND_OUTPUT_BYTES as usize).saturating_sub(target.len());
    let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
    debug_assert!(remainder.is_empty());
    for pair in pairs.iter().take(available) {
        target.push((hex_digit(pair[0])? << 4) | hex_digit(pair[1])?);
    }
    Ok(())
}

fn hex_digit(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err("invalid output hex".to_owned()),
    }
}

fn event_name(event: &Event) -> &'static str {
    match event {
        Event::ControlledCommandStarted(_) => "command started",
        Event::ControlledCommandOutput(_) => "command output",
        Event::ControlledCommandFinished(_) => "command finished",
        Event::TestCaseCompared(_) => "test case compared",
        Event::SessionStarted(_) => "session started",
        Event::SessionResumed(_) => "session resumed",
        Event::SessionEnded(_) => "session ended",
        Event::FileCreated(_) => "file created",
        Event::FileDeleted(_) => "file deleted",
        Event::FileRenamed(_) => "file renamed",
        Event::FileFocused(_) => "file focused",
        Event::FileEdited(transaction) if transaction.origin == EditOrigin::DependencyTool => {
            "dependency tool"
        }
        Event::FileEdited(_) => "file edited",
        Event::ClipboardCopied(_) => "clipboard copied",
        Event::InternalPaste(_) => "internal paste",
        Event::PasteRejected(_) => "paste blocked",
        Event::SelectionChanged(_) => "selection changed",
        Event::ViewportChanged(_) => "viewport changed",
        Event::CargoCommandStarted(_) => "legacy command started",
        Event::CargoDiagnostic(_) => "legacy diagnostic",
        Event::CargoOutput(_) => "legacy command output",
        Event::CargoCommandFinished(_) => "legacy command finished",
        Event::LspCompletionRequested(_) => "completion requested",
        Event::LspCompletionAccepted(_) => "completion accepted",
        Event::LspCodeActionApplied(_) => "code action",
        Event::WorkspaceCheckpoint(_) => "checkpoint",
        Event::ExternalFileChange(_) => "external file change",
        Event::ExternalObservation(_) => "external observation",
        Event::RecoveryRecorded(_) => "recovery recorded",
        Event::SubmissionFinalized(_) => "submission finalized",
    }
}

fn changes_projection(event: &Event) -> bool {
    matches!(
        event,
        Event::FileCreated(_)
            | Event::FileDeleted(_)
            | Event::FileRenamed(_)
            | Event::FileFocused(_)
            | Event::FileEdited(_)
            | Event::SelectionChanged(_)
            | Event::InternalPaste(_)
            | Event::ExternalFileChange(_)
    )
}

fn selected_document_from_replay(replay: &ReplayEngine) -> Option<SelectedDocument> {
    let state = replay.workspace_state();
    let document = state.active_document().and_then(|id| state.document(id))?;
    Some(SelectedDocument {
        document_id: document.document_id().clone(),
        path: document.path().as_str().to_owned(),
        selection: document.selection(),
    })
}

fn selected_document_from_snapshot(snapshot: &CheckpointSnapshot) -> Option<SelectedDocument> {
    let active = snapshot.active_document()?;
    let document = snapshot
        .documents()
        .iter()
        .find(|document| &document.document_id == active)?;
    Some(SelectedDocument {
        document_id: document.document_id.clone(),
        path: document.path.as_str().to_owned(),
        selection: document.selection,
    })
}

#[derive(Clone, Copy)]
enum CommandEventRole {
    Start,
    Output,
    Related,
    Finish,
}

fn command_event(event: &Event) -> Option<(&str, CommandEventRole)> {
    match event {
        Event::ControlledCommandStarted(command) => {
            Some((command.command_id.as_str(), CommandEventRole::Start))
        }
        Event::ControlledCommandOutput(output) => {
            Some((output.command_id.as_str(), CommandEventRole::Output))
        }
        Event::ControlledCommandFinished(command) => {
            Some((command.command_id.as_str(), CommandEventRole::Finish))
        }
        Event::TestCaseCompared(comparison) => {
            Some((comparison.command_id.as_str(), CommandEventRole::Related))
        }
        Event::CargoCommandStarted(command) => {
            Some((command.command_id.as_str(), CommandEventRole::Start))
        }
        Event::CargoDiagnostic(diagnostic) => {
            Some((diagnostic.command_id.as_str(), CommandEventRole::Related))
        }
        Event::CargoOutput(output) => Some((output.command_id.as_str(), CommandEventRole::Output)),
        Event::CargoCommandFinished(command) => {
            Some((command.command_id.as_str(), CommandEventRole::Finish))
        }
        _ => None,
    }
}

fn index_command_event(
    command_events: &mut Vec<CommandEventIndex>,
    commands: &mut BTreeMap<String, usize>,
    current: &mut Option<usize>,
    event_index: usize,
    event: &Event,
) -> Option<usize> {
    let Some((id, role)) = command_event(event) else {
        return *current;
    };
    let command_index = *commands.entry(id.to_owned()).or_insert_with(|| {
        let index = command_events.len();
        command_events.push(CommandEventIndex::default());
        index
    });
    let indexed = &mut command_events[command_index];
    match role {
        CommandEventRole::Start => indexed.start = Some(event_index),
        CommandEventRole::Output => indexed.output_events.push(event_index),
        CommandEventRole::Related => {}
        CommandEventRole::Finish => indexed.finish = Some(event_index),
    }
    *current = Some(command_index);
    *current
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::{AssignmentReferenceStatus, SubmittedSourceStatus, VerificationStatus};
    use rustrace_model::{
        CommandFinished, CommandId, CommandOutput, CommandStarted, DocumentId, EditOrigin,
        EditorTransaction, Event, EventEnvelope, Hash, OutputStream, PasteInputChannel,
        PasteRejected, PasteRejectionReason, SelectionState, SessionId, SubmissionFinalized,
        TestCaseCompared, TestCaseComparisonOutcome, TextEdit, WorkspaceDirectory,
    };

    fn clean_report() -> VerificationReport {
        VerificationReport {
            package_structure: VerificationStatus::Ok,
            event_chain: VerificationStatus::Ok,
            checkpoint_hashes: VerificationStatus::Ok,
            replay: VerificationStatus::Ok,
            submitted_source_match: SubmittedSourceStatus::Unavailable,
            assignment_reference: AssignmentReferenceStatus::Unverified,
            test_case_runs: Some(0),
            test_case_passes: Some(0),
            test_case_mismatches: Some(0),
            test_case_errors: Some(0),
            test_case_evidence: Some(crate::verify::TestCaseEvidenceStatus::Recorded),
            first_failing_case: None,
            external_changes: Some(0),
            unknown_edit_origins: Some(0),
            student_id: None,
            assignment_id: None,
            allowed_internal_pastes: Some(0),
            allowed_internal_paste_characters: Some(0),
            historical_origin_unverified_pastes: Some(0),
            historical_origin_unverified_paste_characters: Some(0),
            rejected_paste_attempts: Some(0),
            first_external_change: None,
            first_unknown_edit_origin: None,
            first_allowed_internal_paste: None,
            first_historical_origin_unverified_paste: None,
            first_rejected_paste_attempt: None,
            review_indicators: None,
            advisories: vec![],
            issues: vec![],
        }
    }

    fn envelope(sequence: u64, millis: u64, event: Event) -> EventEnvelope {
        EventEnvelope {
            format_version: 1,
            session_id: SessionId::new("replay-test").unwrap(),
            sequence,
            monotonic_millis: millis,
            wall_clock_utc: None,
            previous_event_hash: rustrace_model::Hash::zero(),
            event_hash: rustrace_model::Hash::zero(),
            event,
        }
    }

    #[test]
    fn playback_controls_use_recorded_offsets_two_speeds_and_idle_skipping() {
        let blocked = || {
            Event::PasteRejected(PasteRejected {
                reason: PasteRejectionReason::ExternalInput,
                channel: PasteInputChannel::TerminalBracketed,
            })
        };
        let events = vec![
            envelope(1, 0, blocked()),
            envelope(2, 1_000, blocked()),
            envelope(
                3,
                11_000,
                Event::SubmissionFinalized(SubmissionFinalized {
                    final_workspace_hash: Hash::zero(),
                    event_count: 3,
                    clean: true,
                    warnings: vec![],
                }),
            ),
        ];
        let mut replay = ReplayController::from_test_events(
            clean_report(),
            TimingAvailability::Recorded,
            events,
        );
        replay.toggle_play();
        assert!(replay.is_playing());
        assert!(!replay.advance_playback(Duration::from_millis(999)).unwrap());
        assert!(replay.advance_playback(Duration::from_millis(1)).unwrap());
        replay.toggle_speed();
        assert_eq!(replay.speed(), PlaybackSpeed::Accelerated);
        assert!(
            !replay
                .advance_playback(Duration::from_millis(2_499))
                .unwrap()
        );
        assert!(replay.advance_playback(Duration::from_millis(1)).unwrap());
        replay.previous_event().unwrap();
        assert!(
            !replay.is_playing(),
            "manual backward navigation did not pause"
        );
        replay.toggle_play();
        replay.toggle_skip_idle();
        assert!(replay.skip_idle());
        assert!(replay.advance_playback(Duration::ZERO).unwrap());
    }

    #[test]
    fn manual_seek_resets_playback_budget_and_invalid_timeline_cannot_play() {
        let blocked = || {
            Event::PasteRejected(PasteRejected {
                reason: PasteRejectionReason::ExternalInput,
                channel: PasteInputChannel::TerminalBracketed,
            })
        };
        let mut replay = ReplayController::from_test_events(
            clean_report(),
            TimingAvailability::Recorded,
            vec![
                envelope(1, 0, blocked()),
                envelope(2, 1_000, blocked()),
                envelope(3, 2_000, blocked()),
            ],
        );
        replay.toggle_play();
        assert!(!replay.advance_playback(Duration::from_millis(900)).unwrap());
        replay
            .select(EventPosition {
                segment: 0,
                sequence: 1,
            })
            .unwrap();
        assert!(!replay.is_playing(), "manual seek did not pause playback");
        assert_eq!(replay.playback_budget_millis, 0);
        assert!(
            !replay
                .advance_playback(Duration::from_millis(1_000))
                .unwrap()
        );
        replay.toggle_play();
        assert!(!replay.advance_playback(Duration::from_millis(999)).unwrap());
        assert!(replay.advance_playback(Duration::from_millis(1)).unwrap());

        let mut failed = clean_report();
        failed.package_structure = VerificationStatus::Failed;
        let mut invalid = ReplayController::empty(failed, None);
        invalid.toggle_play();
        assert!(!invalid.is_playing());
    }

    #[test]
    fn following_an_event_flag_seeks_by_segment_ordinal_and_original_sequence() {
        let events = vec![
            envelope(
                1,
                0,
                Event::PasteRejected(PasteRejected {
                    reason: PasteRejectionReason::ExternalInput,
                    channel: PasteInputChannel::TerminalBracketed,
                }),
            ),
            envelope(
                2,
                1,
                Event::PasteRejected(PasteRejected {
                    reason: PasteRejectionReason::ExternalInput,
                    channel: PasteInputChannel::TerminalBracketed,
                }),
            ),
        ];
        let mut replay = ReplayController::from_test_events(
            clean_report(),
            TimingAvailability::Recorded,
            events,
        );
        let flag = ReviewFlag {
            kind: crate::review_flags::ReviewFlagKind::EventSequenceGap,
            link: crate::review_flags::ReviewFlagLink::Event(
                crate::verify::VerificationEventLocation {
                    segment: 1,
                    sequence: 2,
                },
            ),
            detail: "the recorded sequence is not contiguous".to_owned(),
        };

        replay.follow_review_flag(&flag).unwrap();
        assert_eq!(
            replay.selected_event().unwrap().position,
            EventPosition {
                segment: 0,
                sequence: 2,
            }
        );
    }

    #[test]
    fn selection_changes_invalidate_cached_source_projection() {
        let event = Event::SelectionChanged(rustrace_model::SelectionChanged {
            document_id: DocumentId::new("main-document").unwrap(),
            anchor_byte: 4,
            active_byte: 9,
        });

        assert!(
            changes_projection(&event),
            "checkpoint metadata cannot represent a later selection-only event"
        );
    }

    #[test]
    fn evidence_event_seek_pauses_and_clears_partial_playback_budget() {
        let blocked = || {
            Event::PasteRejected(PasteRejected {
                reason: PasteRejectionReason::ExternalInput,
                channel: PasteInputChannel::TerminalBracketed,
            })
        };
        let mut replay = ReplayController::from_test_events(
            clean_report(),
            TimingAvailability::Recorded,
            vec![
                envelope(1, 0, blocked()),
                envelope(2, 1_000, blocked()),
                envelope(3, 2_000, blocked()),
            ],
        );
        replay.selected.as_mut().unwrap().evidence_target =
            Some(EvidenceTarget::Event(EventPosition {
                segment: 0,
                sequence: 3,
            }));
        replay.toggle_play();
        assert!(!replay.advance_playback(Duration::from_millis(900)).unwrap());

        assert_eq!(
            replay.follow_selected_evidence().unwrap(),
            Some(EvidenceTarget::Event(EventPosition {
                segment: 0,
                sequence: 3,
            }))
        );
        assert_eq!(replay.selected_event().unwrap().position.sequence, 3);
        assert!(!replay.is_playing(), "evidence event seek did not pause");
        assert_eq!(replay.playback_budget_millis, 0);
    }

    #[test]
    fn following_an_indicator_seeks_to_its_direct_event_link() {
        let events = vec![
            envelope(
                1,
                0,
                Event::PasteRejected(PasteRejected {
                    reason: PasteRejectionReason::ExternalInput,
                    channel: PasteInputChannel::TerminalBracketed,
                }),
            ),
            envelope(
                2,
                1,
                Event::PasteRejected(PasteRejected {
                    reason: PasteRejectionReason::ExternalInput,
                    channel: PasteInputChannel::TerminalBracketed,
                }),
            ),
        ];
        let mut report = clean_report();
        let mut indicators = crate::process_indicators::ReviewIndicators::default();
        indicators.factual.rejected_paste.attempts = 1;
        indicators.factual.rejected_paste.links.push(
            crate::process_indicators::IndicatorEventLink {
                segment: 1,
                session_id: rustrace_model::SessionId::new("replay-test").unwrap(),
                sequence: 2,
            },
        );
        report.review_indicators = Some(indicators);
        let mut replay =
            ReplayController::from_test_events(report, TimingAvailability::Recorded, events);

        assert_eq!(
            replay.follow_next_indicator().unwrap(),
            Some(EventPosition {
                segment: 0,
                sequence: 2,
            })
        );
        assert_eq!(
            replay.selected_event().unwrap().position,
            EventPosition {
                segment: 0,
                sequence: 2,
            }
        );
    }

    #[test]
    fn synthetic_fixture_timing_is_explicitly_unavailable() {
        let replay = ReplayController::from_test_events(
            clean_report(),
            TimingAvailability::UnavailableSynthetic,
            vec![],
        );
        assert_eq!(
            replay.timing_label(),
            "timing unavailable (synthetic fixture)"
        );
    }

    #[test]
    fn historical_paste_marker_uses_exact_persisted_text_counts() {
        let replay = ReplayController::from_test_events(
            clean_report(),
            TimingAvailability::Recorded,
            vec![],
        );
        let marker = replay
            .paste_marker(&Event::FileEdited(EditorTransaction {
                document_id: DocumentId::new("doc").unwrap(),
                version_before: 0,
                version_after: 1,
                origin: EditOrigin::Paste,
                edits: vec![TextEdit {
                    start_byte: 0,
                    end_byte: 0,
                    inserted_text: "é🦀\r\nnext".to_owned(),
                }],
                selection_before: SelectionState::default(),
                selection_after: SelectionState::caret(13),
                hash_before: Hash::zero(),
                hash_after: Hash::zero(),
            }))
            .unwrap();
        assert_eq!(
            marker,
            Some(PasteMarker::HistoricalOriginUnverified {
                characters: 8,
                lines: 2,
            })
        );
    }

    #[test]
    fn command_output_is_synchronized_as_exact_bytes() {
        let id = CommandId::new("command-1").unwrap();
        let events = vec![
            envelope(
                1,
                0,
                Event::CargoOutput(CommandOutput {
                    command_id: id.clone(),
                    stream: OutputStream::Stdout,
                    output: "first\n".to_owned(),
                }),
            ),
            envelope(
                2,
                1,
                Event::CargoOutput(CommandOutput {
                    command_id: id,
                    stream: OutputStream::Stderr,
                    output: "\u{1b}]52;hostile\u{7}".to_owned(),
                }),
            ),
        ];
        let replay = ReplayController::from_test_events(
            clean_report(),
            TimingAvailability::Recorded,
            events,
        );
        assert_eq!(
            replay.command_output_through(1).unwrap(),
            b"first\n\x1b]52;hostile\x07"
        );
    }

    #[test]
    fn selected_comparison_keeps_typed_details_beside_its_command_output() {
        let id = CommandId::new("command-1").unwrap();
        let comparison = TestCaseCompared {
            command_id: id.clone(),
            case: "sample".to_owned(),
            expected_blake3: Hash::from_bytes([1; 32]),
            actual_blake3: Some(Hash::from_bytes([2; 32])),
            outcome: TestCaseComparisonOutcome::Mismatch {
                line: 3,
                expected_len: 4,
                actual_len: 5,
            },
        };
        let events = vec![
            envelope(
                1,
                0,
                Event::ControlledCommandOutput(rustrace_model::ControlledCommandOutput {
                    command_id: id,
                    stream: OutputStream::Stdout,
                    offset: 0,
                    bytes_hex: "6f6b0a".to_owned(),
                }),
            ),
            envelope(2, 1, Event::TestCaseCompared(comparison.clone())),
        ];
        let mut replay = ReplayController::from_test_events(
            clean_report(),
            TimingAvailability::Recorded,
            events,
        );

        replay.select_index(1).unwrap();

        let selected = replay.selected_event().unwrap();
        assert_eq!(selected.command_output, b"ok\n");
        assert_eq!(selected.test_case_comparison, Some(comparison));
    }

    #[test]
    fn late_selection_reads_only_bounded_command_envelopes() {
        let id = CommandId::new("early-command").unwrap();
        let mut events = vec![
            envelope(
                1,
                0,
                Event::CargoCommandStarted(CommandStarted {
                    command_id: id.clone(),
                    program: "cargo".to_owned(),
                    arguments: vec!["check".to_owned()],
                    working_directory: WorkspaceDirectory::new(".").unwrap(),
                }),
            ),
            envelope(
                2,
                1,
                Event::CargoOutput(CommandOutput {
                    command_id: id.clone(),
                    stream: OutputStream::Stdout,
                    output: "first\n".to_owned(),
                }),
            ),
            envelope(
                3,
                2,
                Event::CargoOutput(CommandOutput {
                    command_id: id.clone(),
                    stream: OutputStream::Stderr,
                    output: "second\n".to_owned(),
                }),
            ),
            envelope(
                4,
                3,
                Event::CargoCommandFinished(CommandFinished {
                    command_id: id,
                    exit_code: Some(0),
                    success: true,
                }),
            ),
        ];
        for offset in 0..4_096_u64 {
            events.push(envelope(
                offset + 5,
                offset + 4,
                Event::FileEdited(EditorTransaction {
                    document_id: DocumentId::new("doc").unwrap(),
                    version_before: offset,
                    version_after: offset + 1,
                    origin: EditOrigin::Keyboard,
                    edits: vec![TextEdit {
                        start_byte: 0,
                        end_byte: 0,
                        inserted_text: "x".to_owned(),
                    }],
                    selection_before: SelectionState::default(),
                    selection_after: SelectionState::caret(1),
                    hash_before: Hash::zero(),
                    hash_after: Hash::zero(),
                }),
            ));
        }
        let late = events.len() - 1;
        let mut replay = ReplayController::from_test_events(
            clean_report(),
            TimingAvailability::Recorded,
            events,
        );
        replay.read_event_calls.set(0);

        replay.select_index(late).unwrap();

        assert_eq!(
            replay.selected_event().unwrap().command_output,
            b"first\nsecond\n"
        );
        assert_eq!(
            replay.read_event_calls.get(),
            3,
            "late selection must decode its selected event and two output events"
        );
    }
}
