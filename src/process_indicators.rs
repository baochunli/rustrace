//! Bounded factual review indicators and the narrow `process-review-v1` rule.

use crate::diagnostics::{DiagnosticEvidenceIssue, DiagnosticOutcome, derive_command_diagnostics};
use rustrace_model::{
    CaptureCompleteness, CommandId, CommandOutcome, CommandTermination, ControlledAction,
    ControlledCommandFinished, ControlledCommandOutput, ControlledCommandStarted, EditOrigin,
    EditorTransaction, Event, EventEnvelope, SessionId, inserted_text_counts,
};
use std::collections::{BTreeMap, BTreeSet};

pub const PROCESS_RULE_VERSION: &str = "process-review-v1";
pub const MAX_RETAINED_INDICATOR_LINKS: usize = 256;
const DISPLAYED_INDICATOR_LINKS: usize = 8;
pub const INDICATOR_EXPLANATION: &str = "Unknown means a missing origin explanation, not established external input. Internal paste is allowed and not inherently suspicious. Rejected paste records metadata only, never content size.";

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct IndicatorEventLink {
    pub segment: u32,
    pub session_id: SessionId,
    pub sequence: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EditIndicator {
    pub transactions: u64,
    pub inserted_scalars: u64,
    pub links: Vec<IndicatorEventLink>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RejectionIndicator {
    pub attempts: u64,
    pub links: Vec<IndicatorEventLink>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandEventLink {
    pub start: IndicatorEventLink,
    pub finish: Option<IndicatorEventLink>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandIndicator {
    pub action: ControlledAction,
    pub started: u64,
    pub complete: u64,
    pub exit_zero: u64,
    pub nonzero_exit: u64,
    pub compiler_errors: u64,
    pub unknown: u64,
    pub links: Vec<CommandEventLink>,
}

impl CommandIndicator {
    fn new(action: ControlledAction) -> Self {
        Self {
            action,
            started: 0,
            complete: 0,
            exit_zero: 0,
            nonzero_exit: 0,
            compiler_errors: 0,
            unknown: 0,
            links: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FactualIndicators {
    pub allowed_internal_paste: EditIndicator,
    pub historical_origin_unverified_paste: EditIndicator,
    pub unknown_origin_edit: EditIndicator,
    pub rejected_external_change: RejectionIndicator,
    pub rejected_paste: RejectionIndicator,
    pub commands: Vec<CommandIndicator>,
}

impl Default for FactualIndicators {
    fn default() -> Self {
        Self {
            allowed_internal_paste: EditIndicator::default(),
            historical_origin_unverified_paste: EditIndicator::default(),
            unknown_origin_edit: EditIndicator::default(),
            rejected_external_change: RejectionIndicator::default(),
            rejected_paste: RejectionIndicator::default(),
            commands: [
                ControlledAction::Build,
                ControlledAction::Check,
                ControlledAction::Test,
                ControlledAction::Run,
                ControlledAction::Clippy,
                ControlledAction::Format,
                ControlledAction::Doc,
                ControlledAction::Add,
                ControlledAction::Remove,
                ControlledAction::Update,
            ]
            .into_iter()
            .map(CommandIndicator::new)
            .collect(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OriginScalars {
    pub keyboard: u64,
    pub internal_paste: u64,
    pub historical_paste: u64,
    pub completion: u64,
    pub formatter: u64,
    pub dependency_tool: u64,
    pub unknown: u64,
    pub other: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProcessValues {
    pub inserted_keyboard_scalars: u64,
    pub keyboard_transactions: u64,
    pub appended_keyboard_scalars: u64,
    pub removed_keyboard_scalars: u64,
    pub forward_inserted_scalars: u64,
    pub before_first_build_scalars: u64,
    pub feedback_opportunities: u64,
    pub error_edit_rebuild_sequences: u64,
    pub complete_check_test_commands: u64,
    pub compiler_error_commands: u64,
    pub failed_test_commands: u64,
    pub origins: OriginScalars,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessObservations {
    pub entry: bool,
    pub feedback: bool,
    pub before_build: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessRuleOutcome {
    Suggested,
    NotEligible { reason: String },
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessAttempt {
    pub attempt: u32,
    pub session_id: SessionId,
    pub rule_version: &'static str,
    pub values: ProcessValues,
    pub observations: ProcessObservations,
    pub outcome: ProcessRuleOutcome,
    pub edit_links: Vec<IndicatorEventLink>,
    pub command_links: Vec<IndicatorEventLink>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReviewIndicators {
    pub factual: FactualIndicators,
    pub attempts: Vec<ProcessAttempt>,
}

struct ActiveCommand {
    start: ControlledCommandStarted,
    start_link: IndicatorEventLink,
    output: Vec<ControlledCommandOutput>,
    retained_link: Option<(usize, usize)>,
}

#[derive(Clone, Copy)]
struct CompleteCommand {
    compiler_errors: bool,
}

pub(crate) struct AttemptIndicatorAccumulator {
    segment: u32,
    session_id: SessionId,
    factual: FactualIndicators,
    values: ProcessValues,
    unavailable: Option<String>,
    edit_links: Vec<IndicatorEventLink>,
    command_links: Vec<IndicatorEventLink>,
    active_command: Option<ActiveCommand>,
    legacy_commands: BTreeMap<CommandId, (ControlledAction, Option<(usize, usize)>)>,
    last_complete_command: Option<CompleteCommand>,
    qualifying_edit_since_command: bool,
    first_check_test_started: bool,
}

impl AttemptIndicatorAccumulator {
    pub(crate) fn new(segment: u32, session_id: SessionId) -> Self {
        Self {
            segment,
            session_id,
            factual: FactualIndicators::default(),
            values: ProcessValues::default(),
            unavailable: None,
            edit_links: Vec::new(),
            command_links: Vec::new(),
            active_command: None,
            legacy_commands: BTreeMap::new(),
            last_complete_command: None,
            qualifying_edit_since_command: false,
            first_check_test_started: false,
        }
    }

    pub(crate) fn observe(
        &mut self,
        envelope: &EventEnvelope,
        pre_edit_text: Option<&str>,
    ) -> Result<(), String> {
        let event_link = IndicatorEventLink {
            segment: self.segment,
            session_id: envelope.session_id.clone(),
            sequence: envelope.sequence,
        };
        match &envelope.event {
            Event::FileEdited(transaction) => {
                self.observe_transaction(transaction, event_link, pre_edit_text)?;
            }
            Event::InternalPaste(paste) => {
                let inserted = transaction_inserted_scalars(&paste.transaction)?;
                add_edit_indicator(
                    &mut self.factual.allowed_internal_paste,
                    inserted,
                    event_link.clone(),
                )?;
                self.observe_forward_scalars(EditOrigin::Paste, inserted, true, event_link)?;
            }
            Event::ExternalObservation(_) => {
                add_rejection_indicator(&mut self.factual.rejected_external_change, event_link)?;
            }
            Event::PasteRejected(_) => {
                add_rejection_indicator(&mut self.factual.rejected_paste, event_link)?;
            }
            Event::FileDeleted(_) => {
                self.mark_unavailable("file deletion occurred");
            }
            Event::FileRenamed(_) => {
                self.mark_unavailable("file rename occurred");
            }
            Event::SessionResumed(_) => {
                self.last_complete_command = None;
                self.qualifying_edit_since_command = false;
            }
            Event::ControlledCommandStarted(start) => {
                self.start_command(start, event_link)?;
            }
            Event::ControlledCommandOutput(output) => {
                self.command_output(output)?;
            }
            Event::ControlledCommandFinished(finish) => {
                self.finish_command(finish, event_link)?;
            }
            Event::CargoCommandStarted(start) => {
                self.start_legacy_command(start, event_link)?;
            }
            Event::CargoCommandFinished(finish) => {
                self.finish_legacy_command(&finish.command_id, event_link);
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<(FactualIndicators, ProcessAttempt), String> {
        if let Some(active) = self.active_command.take() {
            let action = active.start.action;
            let facts = command_indicator_mut(&mut self.factual, action);
            checked_increment(&mut facts.unknown, "unknown command count")?;
            if is_check_test(action) {
                self.mark_unavailable(&format!(
                    "unpaired {} command evidence",
                    action_name(action)
                ));
            }
        }
        let observations = observations(&self.values);
        let outcome = evaluate(&self.values, self.unavailable);
        Ok((
            self.factual,
            ProcessAttempt {
                attempt: self.segment,
                session_id: self.session_id,
                rule_version: PROCESS_RULE_VERSION,
                values: self.values,
                observations,
                outcome,
                edit_links: self.edit_links,
                command_links: self.command_links,
            },
        ))
    }
}

pub(crate) fn merge_factual(
    target: &mut FactualIndicators,
    source: FactualIndicators,
) -> Result<(), String> {
    merge_edit_indicator(
        &mut target.allowed_internal_paste,
        source.allowed_internal_paste,
    )?;
    merge_edit_indicator(
        &mut target.historical_origin_unverified_paste,
        source.historical_origin_unverified_paste,
    )?;
    merge_edit_indicator(&mut target.unknown_origin_edit, source.unknown_origin_edit)?;
    merge_rejection_indicator(
        &mut target.rejected_external_change,
        source.rejected_external_change,
    )?;
    merge_rejection_indicator(&mut target.rejected_paste, source.rejected_paste)?;
    for source_command in source.commands {
        let target_command = command_indicator_mut(target, source_command.action);
        checked_add(
            &mut target_command.started,
            source_command.started,
            "command start count",
        )?;
        checked_add(
            &mut target_command.complete,
            source_command.complete,
            "complete command count",
        )?;
        checked_add(
            &mut target_command.exit_zero,
            source_command.exit_zero,
            "zero-exit command count",
        )?;
        checked_add(
            &mut target_command.nonzero_exit,
            source_command.nonzero_exit,
            "nonzero command count",
        )?;
        checked_add(
            &mut target_command.compiler_errors,
            source_command.compiler_errors,
            "compiler-error command count",
        )?;
        checked_add(
            &mut target_command.unknown,
            source_command.unknown,
            "unknown command count",
        )?;
        extend_bounded(&mut target_command.links, source_command.links);
    }
    Ok(())
}

fn observations(values: &ProcessValues) -> ProcessObservations {
    let has_keyboard = values.inserted_keyboard_scalars > 0;
    ProcessObservations {
        entry: has_keyboard
            && ratio_at_least(
                values.appended_keyboard_scalars,
                values.inserted_keyboard_scalars,
                4,
                5,
            )
            && ratio_at_most(
                values.removed_keyboard_scalars,
                values.inserted_keyboard_scalars,
                1,
                10,
            ),
        feedback: values.feedback_opportunities >= 2
            && ratio_at_most(
                values.error_edit_rebuild_sequences,
                values.feedback_opportunities,
                1,
                4,
            ),
        before_build: has_keyboard
            && ratio_at_least(
                values.before_first_build_scalars,
                values.inserted_keyboard_scalars,
                4,
                5,
            ),
    }
}

fn evaluate(values: &ProcessValues, unavailable: Option<String>) -> ProcessRuleOutcome {
    if let Some(reason) = unavailable {
        return ProcessRuleOutcome::Unavailable { reason };
    }
    let reason = if values.inserted_keyboard_scalars < 1_000 {
        Some(format!(
            "I={} is below the 1,000-scalar sample",
            values.inserted_keyboard_scalars
        ))
    } else if values.keyboard_transactions < 20 {
        Some(format!(
            "N={} is below the 20-transaction sample",
            values.keyboard_transactions
        ))
    } else if values.forward_inserted_scalars == 0 {
        Some("J=0 provides no forward-insertion denominator".to_owned())
    } else if !ratio_at_least(
        values.inserted_keyboard_scalars,
        values.forward_inserted_scalars,
        4,
        5,
    ) {
        Some(format!(
            "I/J={}/{} is below 0.8",
            values.inserted_keyboard_scalars, values.forward_inserted_scalars
        ))
    } else if values.complete_check_test_commands < 2 {
        Some(format!(
            "only {} complete Check/Test command(s) were observed",
            values.complete_check_test_commands
        ))
    } else if values.feedback_opportunities == 0 {
        Some("O=0 provides no feedback opportunity".to_owned())
    } else {
        None
    };
    if let Some(reason) = reason {
        return ProcessRuleOutcome::NotEligible { reason };
    }
    let observed = observations(values);
    if observed.entry && (observed.feedback || observed.before_build) {
        ProcessRuleOutcome::Suggested
    } else {
        let reason = if !observed.entry {
            "entry observation E is not supported"
        } else {
            "neither feedback observation C nor before-build observation P is supported"
        };
        ProcessRuleOutcome::NotEligible {
            reason: reason.to_owned(),
        }
    }
}

pub fn display_process_attempt(attempt: &ProcessAttempt) -> String {
    match &attempt.outcome {
        ProcessRuleOutcome::Suggested => {
            let support = match (
                attempt.observations.feedback,
                attempt.observations.before_build,
            ) {
                (true, true) => concat!(
                    "few observed error/edit/rebuild sequences among known opportunities and ",
                    "most qualifying entry preceded the first recorded Check/Test"
                ),
                (true, false) => {
                    "few observed error/edit/rebuild sequences among known opportunities"
                }
                (false, true) => "most qualifying entry preceded the first recorded Check/Test",
                (false, false) => "the required feedback/order observation",
            };
            format!(
                "Review suggested: substantial predominantly append entry with limited recorded \
revision, together with {support}. Planned or familiar work can show the same pattern; these \
observations do not establish transcription, AI use or intent."
            )
        }
        ProcessRuleOutcome::NotEligible { reason } => format!("not eligible: {reason}"),
        ProcessRuleOutcome::Unavailable { reason } => format!("unavailable: {reason}"),
    }
}

pub fn display_process_attempt_details(attempt: &ProcessAttempt) -> String {
    let values = &attempt.values;
    let outcome = display_process_attempt(attempt);
    format!(
        "{} attempt {}: {} | session={}; I={} N={} A={} R={} J={} B={} O={} F={}; complete Check/Test={} compiler-error={} failed-test={}; ratios I/J={}/{} A/I={}/{} R/I={}/{} F/O={}/{} B/I={}/{}; thresholds I>=1000 N>=20 J>0 I/J>=0.8 complete>=2 O>=1 E:A/I>=0.8,R/I<=0.1 C:O>=2,F/O<=0.25 P:B/I>=0.8; origins keyboard={} internal-paste={} historical-paste={} completion={} formatter={} dependency-tool={} unknown={} other={}; edit links {}; command links {}",
        attempt.rule_version,
        attempt.attempt,
        outcome,
        attempt.session_id,
        values.inserted_keyboard_scalars,
        values.keyboard_transactions,
        values.appended_keyboard_scalars,
        values.removed_keyboard_scalars,
        values.forward_inserted_scalars,
        values.before_first_build_scalars,
        values.feedback_opportunities,
        values.error_edit_rebuild_sequences,
        values.complete_check_test_commands,
        values.compiler_error_commands,
        values.failed_test_commands,
        values.inserted_keyboard_scalars,
        values.forward_inserted_scalars,
        values.appended_keyboard_scalars,
        values.inserted_keyboard_scalars,
        values.removed_keyboard_scalars,
        values.inserted_keyboard_scalars,
        values.error_edit_rebuild_sequences,
        values.feedback_opportunities,
        values.before_first_build_scalars,
        values.inserted_keyboard_scalars,
        values.origins.keyboard,
        values.origins.internal_paste,
        values.origins.historical_paste,
        values.origins.completion,
        values.origins.formatter,
        values.origins.dependency_tool,
        values.origins.unknown,
        values.origins.other,
        display_links(&attempt.edit_links),
        display_links(&attempt.command_links),
    )
}

pub fn display_factual_indicators(factual: &FactualIndicators) -> Vec<String> {
    let mut displayed = vec![
        display_edit("allowed internal paste", &factual.allowed_internal_paste),
        display_edit(
            "historical origin-unverified paste",
            &factual.historical_origin_unverified_paste,
        ),
        display_edit("unknown-origin edits", &factual.unknown_origin_edit),
        display_rejection(
            "external-change observations",
            &factual.rejected_external_change,
        ),
        display_rejection("rejected paste attempts", &factual.rejected_paste),
    ];
    if factual.rejected_external_change.attempts >= 2 {
        displayed.push(format!(
            "repeated rejected external-change attempts: {} [{}]",
            factual.rejected_external_change.attempts,
            display_links(&factual.rejected_external_change.links)
        ));
    }
    if factual.rejected_paste.attempts >= 2 {
        displayed.push(format!(
            "repeated rejected paste attempts: {} [{}]",
            factual.rejected_paste.attempts,
            display_links(&factual.rejected_paste.links)
        ));
    }
    displayed.extend(factual.commands.iter().map(|command| {
        let links = command
            .links
            .iter()
            .map(|link| link.start.clone())
            .collect::<Vec<_>>();
        format!(
            "Cargo {}: started {} complete {} exit-0 {} nonzero {} compiler-errors {} unknown {} [{}]",
            action_name(command.action),
            command.started,
            command.complete,
            command.exit_zero,
            command.nonzero_exit,
            command.compiler_errors,
            command.unknown,
            display_links(&links)
        )
    }));
    displayed
}

pub fn display_terminal_factual_indicators(factual: &FactualIndicators) -> String {
    let mut displayed = vec![
        display_terminal_rejection(
            "external-change observations",
            &factual.rejected_external_change,
            true,
        ),
        display_terminal_edit("unknown-origin edits", &factual.unknown_origin_edit),
        display_terminal_edit("allowed internal paste", &factual.allowed_internal_paste),
        display_terminal_edit(
            "historical origin-unverified paste",
            &factual.historical_origin_unverified_paste,
        ),
        display_terminal_rejection("rejected paste attempts", &factual.rejected_paste, false),
    ];
    if factual.rejected_external_change.attempts >= 2 {
        displayed.push(format!(
            "repeated rejected external-change attempts: {} [{}]",
            factual.rejected_external_change.attempts,
            display_first_compact_link(&factual.rejected_external_change.links)
        ));
    }
    if factual.rejected_paste.attempts >= 2 {
        displayed.push(format!(
            "repeated rejected paste attempts: {} [{}]",
            factual.rejected_paste.attempts,
            display_first_compact_link(&factual.rejected_paste.links)
        ));
    }
    displayed.extend(factual.commands.iter().map(|command| {
        let first_link = command.links.first().map(|link| &link.start);
        format!(
            "Cargo {}: started {} complete {} exit-0 {} nonzero {} compiler-errors {} unknown {} [{}]",
            action_name(command.action),
            command.started,
            command.complete,
            command.exit_zero,
            command.nonzero_exit,
            command.compiler_errors,
            command.unknown,
            display_compact_link(first_link),
        )
    }));
    displayed.join("; ")
}

pub fn indicator_links(indicators: &ReviewIndicators) -> Vec<IndicatorEventLink> {
    let mut links = BTreeSet::new();
    for metric in [
        &indicators.factual.allowed_internal_paste,
        &indicators.factual.historical_origin_unverified_paste,
        &indicators.factual.unknown_origin_edit,
    ] {
        links.extend(metric.links.iter().take(DISPLAYED_INDICATOR_LINKS).cloned());
    }
    for metric in [
        &indicators.factual.rejected_external_change,
        &indicators.factual.rejected_paste,
    ] {
        links.extend(metric.links.iter().take(DISPLAYED_INDICATOR_LINKS).cloned());
    }
    for command in &indicators.factual.commands {
        for link in command.links.iter().take(DISPLAYED_INDICATOR_LINKS) {
            links.insert(link.start.clone());
            if let Some(finish) = &link.finish {
                links.insert(finish.clone());
            }
        }
    }
    for attempt in &indicators.attempts {
        links.extend(
            attempt
                .edit_links
                .iter()
                .take(DISPLAYED_INDICATOR_LINKS)
                .cloned(),
        );
        links.extend(
            attempt
                .command_links
                .iter()
                .take(DISPLAYED_INDICATOR_LINKS)
                .cloned(),
        );
    }
    links.into_iter().collect()
}

fn display_edit(label: &str, indicator: &EditIndicator) -> String {
    format!(
        "{label}: {} / {} chars (Unicode scalars) [{}]",
        indicator.transactions,
        indicator.inserted_scalars,
        display_links(&indicator.links)
    )
}

fn display_rejection(label: &str, indicator: &RejectionIndicator) -> String {
    let context = if label == "external-change observations" {
        " rejected attempts"
    } else {
        ""
    };
    format!(
        "{label}: {}{context} [{}]",
        indicator.attempts,
        display_links(&indicator.links)
    )
}

fn display_terminal_edit(label: &str, indicator: &EditIndicator) -> String {
    format!(
        "{label}: {} / {} chars (Unicode scalars) [{}]",
        indicator.transactions,
        indicator.inserted_scalars,
        display_first_compact_link(&indicator.links)
    )
}

fn display_terminal_rejection(
    label: &str,
    indicator: &RejectionIndicator,
    external_change: bool,
) -> String {
    format!(
        "{label}: {}{} [{}]",
        indicator.attempts,
        if external_change {
            " rejected attempts"
        } else {
            ""
        },
        display_first_compact_link(&indicator.links)
    )
}

fn display_first_compact_link(links: &[IndicatorEventLink]) -> String {
    display_compact_link(links.first())
}

fn display_compact_link(link: Option<&IndicatorEventLink>) -> String {
    link.map_or_else(
        || "—".to_owned(),
        |link| format!("segment:{} seq:{}", link.segment, link.sequence),
    )
}

fn display_links(links: &[IndicatorEventLink]) -> String {
    let mut value = links
        .iter()
        .take(DISPLAYED_INDICATOR_LINKS)
        .map(|link| {
            format!(
                "segment:{} session:{} seq:{}",
                link.segment, link.session_id, link.sequence
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    if links.len() > DISPLAYED_INDICATOR_LINKS {
        value.push_str(&format!(
            ", +{} more in event list",
            links.len() - DISPLAYED_INDICATOR_LINKS
        ));
    }
    if value.is_empty() {
        value.push('—');
    }
    value
}

fn retain_unique_link(target: &mut Vec<IndicatorEventLink>, link: IndicatorEventLink) {
    if target.len() < MAX_RETAINED_INDICATOR_LINKS && !target.contains(&link) {
        target.push(link);
    }
}

impl AttemptIndicatorAccumulator {
    fn observe_transaction(
        &mut self,
        transaction: &EditorTransaction,
        event_link: IndicatorEventLink,
        pre_edit_text: Option<&str>,
    ) -> Result<(), String> {
        let inserted = transaction_inserted_scalars(transaction)?;
        match transaction.origin {
            EditOrigin::Paste => add_edit_indicator(
                &mut self.factual.historical_origin_unverified_paste,
                inserted,
                event_link.clone(),
            )?,
            EditOrigin::Unknown => add_edit_indicator(
                &mut self.factual.unknown_origin_edit,
                inserted,
                event_link.clone(),
            )?,
            _ => {}
        }
        if matches!(transaction.origin, EditOrigin::Undo | EditOrigin::Redo) {
            self.mark_unavailable("undo or redo occurred");
            return Ok(());
        }
        self.observe_forward_scalars(transaction.origin, inserted, false, event_link.clone())?;
        if transaction.origin != EditOrigin::Keyboard {
            return Ok(());
        }
        if transaction.edits.len() != 1 {
            self.mark_unavailable("multi-range Keyboard edit occurred");
            return Ok(());
        }
        let edit = &transaction.edits[0];
        checked_add(
            &mut self.values.inserted_keyboard_scalars,
            scalar_count(&edit.inserted_text)?,
            "I inserted Keyboard scalar count",
        )?;
        if !edit.inserted_text.is_empty() {
            checked_increment(
                &mut self.values.keyboard_transactions,
                "N Keyboard transaction count",
            )?;
        }
        let Some(text) = pre_edit_text else {
            self.mark_unavailable("pre-edit document evidence is unavailable");
            return Ok(());
        };
        let start = usize::try_from(edit.start_byte).ok();
        let end = usize::try_from(edit.end_byte).ok();
        let removed = start
            .zip(end)
            .and_then(|(start, end)| text.get(start..end))
            .map(str::chars)
            .map(Iterator::count)
            .and_then(|count| u64::try_from(count).ok());
        let Some(removed) = removed else {
            self.mark_unavailable("pre-edit range evidence is unavailable");
            return Ok(());
        };
        checked_add(
            &mut self.values.removed_keyboard_scalars,
            removed,
            "R removed Keyboard scalar count",
        )?;
        if edit.start_byte == edit.end_byte && edit.end_byte == text.len() as u64 {
            checked_add(
                &mut self.values.appended_keyboard_scalars,
                scalar_count(&edit.inserted_text)?,
                "A appended Keyboard scalar count",
            )?;
        }
        if !self.first_check_test_started {
            checked_add(
                &mut self.values.before_first_build_scalars,
                scalar_count(&edit.inserted_text)?,
                "B before-build Keyboard scalar count",
            )?;
        }
        self.qualifying_edit_since_command = true;
        retain_unique_link(&mut self.edit_links, event_link);
        Ok(())
    }

    fn observe_forward_scalars(
        &mut self,
        origin: EditOrigin,
        inserted: u64,
        internal_paste: bool,
        event_link: IndicatorEventLink,
    ) -> Result<(), String> {
        checked_add(
            &mut self.values.forward_inserted_scalars,
            inserted,
            "J forward-inserted scalar count",
        )?;
        let origin_total = if internal_paste {
            &mut self.values.origins.internal_paste
        } else {
            match origin {
                EditOrigin::Keyboard => &mut self.values.origins.keyboard,
                EditOrigin::Paste => &mut self.values.origins.historical_paste,
                EditOrigin::Completion | EditOrigin::AdditionalCompletionEdit => {
                    &mut self.values.origins.completion
                }
                EditOrigin::Formatter => &mut self.values.origins.formatter,
                EditOrigin::DependencyTool => &mut self.values.origins.dependency_tool,
                EditOrigin::Unknown => &mut self.values.origins.unknown,
                EditOrigin::CodeAction | EditOrigin::FileReload | EditOrigin::ExternalChange => {
                    &mut self.values.origins.other
                }
                EditOrigin::Undo | EditOrigin::Redo => return Ok(()),
            }
        };
        checked_add(origin_total, inserted, "origin scalar count")?;
        if inserted > 0 {
            retain_unique_link(&mut self.edit_links, event_link);
        }
        Ok(())
    }

    fn start_command(
        &mut self,
        start: &ControlledCommandStarted,
        start_link: IndicatorEventLink,
    ) -> Result<(), String> {
        if self.active_command.is_some() {
            self.mark_unavailable("overlapping unpaired command evidence");
            return Ok(());
        }
        let facts = command_indicator_mut(&mut self.factual, start.action);
        checked_increment(&mut facts.started, "command start count")?;
        let retained_link = if facts.links.len() < MAX_RETAINED_INDICATOR_LINKS {
            let index = facts.links.len();
            facts.links.push(CommandEventLink {
                start: start_link.clone(),
                finish: None,
            });
            Some((action_index(start.action), index))
        } else {
            None
        };
        if is_check_test(start.action) {
            self.first_check_test_started = true;
        }
        self.active_command = Some(ActiveCommand {
            start: start.clone(),
            start_link,
            output: Vec::new(),
            retained_link,
        });
        Ok(())
    }

    fn command_output(&mut self, output: &ControlledCommandOutput) -> Result<(), String> {
        let Some(active) = &mut self.active_command else {
            return Ok(());
        };
        if active.start.command_id != output.command_id {
            if is_check_test(active.start.action) {
                self.mark_unavailable("mismatched command output evidence");
            }
            return Ok(());
        }
        active.output.push(output.clone());
        Ok(())
    }

    fn finish_command(
        &mut self,
        finish: &ControlledCommandFinished,
        finish_link: IndicatorEventLink,
    ) -> Result<(), String> {
        let Some(active) = self.active_command.take() else {
            self.mark_unavailable("unpaired command finish evidence");
            return Ok(());
        };
        if active.start.command_id != finish.command_id {
            if is_check_test(active.start.action) {
                self.mark_unavailable("mismatched command finish evidence");
            }
            return Ok(());
        }
        if let Some((action, index)) = active.retained_link
            && let Some(link) = self
                .factual
                .commands
                .get_mut(action)
                .and_then(|facts| facts.links.get_mut(index))
        {
            link.finish = Some(finish_link.clone());
        }
        retain_bounded(&mut self.command_links, active.start_link);
        retain_bounded(&mut self.command_links, finish_link);

        let diagnostic = (matches!(
            active.start.action,
            ControlledAction::Build
                | ControlledAction::Check
                | ControlledAction::Test
                | ControlledAction::Run
                | ControlledAction::Clippy
                | ControlledAction::Doc
        ) && !is_natural_output_console_command(&active.start))
        .then(|| derive_command_diagnostics(&active.start, &active.output, finish));
        let known = command_evidence_known(&active.start, finish, diagnostic.as_ref());
        let facts = command_indicator_mut(&mut self.factual, active.start.action);
        if known {
            checked_increment(&mut facts.complete, "complete command count")?;
            match finish.outcome {
                CommandOutcome::Exited { code: 0 } => {
                    checked_increment(&mut facts.exit_zero, "zero-exit command count")?;
                }
                CommandOutcome::Exited { .. } => {
                    checked_increment(&mut facts.nonzero_exit, "nonzero command count")?;
                }
                _ => unreachable!("known command evidence requires an ordinary exit"),
            }
            if diagnostic
                .as_ref()
                .is_some_and(|result| result.outcome == DiagnosticOutcome::CompilerErrors)
            {
                checked_increment(&mut facts.compiler_errors, "compiler-error command count")?;
            }
        } else {
            checked_increment(&mut facts.unknown, "unknown command count")?;
        }

        if !is_check_test(active.start.action) {
            return Ok(());
        }
        if !known {
            self.mark_unavailable(&format!(
                "unknown {} command evidence: {}",
                action_name(active.start.action),
                command_unknown_reason(finish, diagnostic.as_ref())
            ));
            return Ok(());
        }
        checked_increment(
            &mut self.values.complete_check_test_commands,
            "complete Check/Test command count",
        )?;
        let compiler_errors = diagnostic
            .as_ref()
            .is_some_and(|result| result.outcome == DiagnosticOutcome::CompilerErrors);
        if compiler_errors {
            checked_increment(
                &mut self.values.compiler_error_commands,
                "compiler-error Check/Test count",
            )?;
        }
        if active.start.action == ControlledAction::Test
            && diagnostic
                .as_ref()
                .is_some_and(|result| result.outcome == DiagnosticOutcome::NonzeroExit)
        {
            checked_increment(
                &mut self.values.failed_test_commands,
                "failed Test command count",
            )?;
        }
        if let Some(previous) = self.last_complete_command
            && self.qualifying_edit_since_command
        {
            checked_increment(
                &mut self.values.feedback_opportunities,
                "O feedback opportunity count",
            )?;
            if previous.compiler_errors {
                checked_increment(
                    &mut self.values.error_edit_rebuild_sequences,
                    "F error/edit/rebuild count",
                )?;
            }
        }
        self.last_complete_command = Some(CompleteCommand { compiler_errors });
        self.qualifying_edit_since_command = false;
        Ok(())
    }

    fn start_legacy_command(
        &mut self,
        start: &rustrace_model::CommandStarted,
        start_link: IndicatorEventLink,
    ) -> Result<(), String> {
        let Some(action) = legacy_action(start) else {
            return Ok(());
        };
        let facts = command_indicator_mut(&mut self.factual, action);
        checked_increment(&mut facts.started, "legacy command start count")?;
        checked_increment(&mut facts.unknown, "unknown legacy command count")?;
        let retained = if facts.links.len() < MAX_RETAINED_INDICATOR_LINKS {
            let index = facts.links.len();
            facts.links.push(CommandEventLink {
                start: start_link,
                finish: None,
            });
            Some((action_index(action), index))
        } else {
            None
        };
        self.legacy_commands
            .insert(start.command_id.clone(), (action, retained));
        if is_check_test(action) {
            self.first_check_test_started = true;
            self.mark_unavailable(&format!(
                "unknown {} command evidence: historical capture completeness is unavailable",
                action_name(action)
            ));
        }
        Ok(())
    }

    fn finish_legacy_command(&mut self, command_id: &CommandId, finish: IndicatorEventLink) {
        let Some((_, retained)) = self.legacy_commands.remove(command_id) else {
            return;
        };
        if let Some((action, index)) = retained
            && let Some(link) = self
                .factual
                .commands
                .get_mut(action)
                .and_then(|facts| facts.links.get_mut(index))
        {
            link.finish = Some(finish);
        }
    }

    fn mark_unavailable(&mut self, reason: &str) {
        if self.unavailable.is_none() {
            self.unavailable = Some(reason.to_owned());
        }
    }
}

fn legacy_action(start: &rustrace_model::CommandStarted) -> Option<ControlledAction> {
    let program = start
        .program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(start.program.as_str());
    if program != "cargo" {
        return None;
    }
    match start.arguments.first().map(String::as_str) {
        Some("build") => Some(ControlledAction::Build),
        Some("check") => Some(ControlledAction::Check),
        Some("test") => Some(ControlledAction::Test),
        Some("run") => Some(ControlledAction::Run),
        Some("clippy") => Some(ControlledAction::Clippy),
        Some("fmt") => Some(ControlledAction::Format),
        Some("doc") => Some(ControlledAction::Doc),
        Some("add") => Some(ControlledAction::Add),
        Some("remove") => Some(ControlledAction::Remove),
        Some("update") => Some(ControlledAction::Update),
        _ => None,
    }
}

fn is_natural_output_console_command(start: &ControlledCommandStarted) -> bool {
    start.console.is_some()
        && !start
            .argv
            .iter()
            .any(|argument| argument == "--message-format=json")
}

fn command_evidence_known(
    start: &ControlledCommandStarted,
    finish: &ControlledCommandFinished,
    diagnostics: Option<&crate::diagnostics::CommandDiagnostics>,
) -> bool {
    let complete_capture = [&finish.stdout, &finish.stderr]
        .iter()
        .all(|capture| capture.completeness == CaptureCompleteness::Complete);
    complete_capture
        && matches!(finish.outcome, CommandOutcome::Exited { .. })
        && (is_natural_output_console_command(start)
            || diagnostics.is_none_or(|result| {
                !matches!(
                    result.outcome,
                    DiagnosticOutcome::ExecutionFailed | DiagnosticOutcome::Unknown
                )
            }))
}

fn command_unknown_reason(
    finish: &ControlledCommandFinished,
    diagnostics: Option<&crate::diagnostics::CommandDiagnostics>,
) -> &'static str {
    match finish.outcome {
        CommandOutcome::LaunchFailed { .. } => return "launch failure",
        CommandOutcome::Terminated {
            reason: CommandTermination::Deadline,
            ..
        } => return "timeout",
        CommandOutcome::Terminated {
            reason: CommandTermination::Cancelled | CommandTermination::Quit,
            ..
        } => return "cancellation",
        CommandOutcome::Terminated { .. } => return "incomplete execution",
        CommandOutcome::Exited { .. } => {}
    }
    for capture in [&finish.stdout, &finish.stderr] {
        match capture.completeness {
            CaptureCompleteness::Complete => {}
            CaptureCompleteness::Truncated => return "truncated output",
            CaptureCompleteness::ReadFailed => return "output read failure",
            CaptureCompleteness::Unavailable => return "unavailable output",
        }
    }
    diagnostics
        .and_then(|result| result.issues.first())
        .map_or("unknown outcome", |issue| match issue {
            DiagnosticEvidenceIssue::Missing => "missing diagnostics",
            DiagnosticEvidenceIssue::Malformed => "malformed diagnostics",
            DiagnosticEvidenceIssue::Oversized => "oversized diagnostics",
            DiagnosticEvidenceIssue::Unexpected => "unparseable diagnostics",
            DiagnosticEvidenceIssue::InvalidUtf8 => "invalid UTF-8 diagnostics",
            DiagnosticEvidenceIssue::Truncated => "truncated output",
            DiagnosticEvidenceIssue::ReadFailed => "output read failure",
            DiagnosticEvidenceIssue::Unavailable => "unavailable output",
            DiagnosticEvidenceIssue::ExecutionFailed => "incomplete execution",
        })
}

fn transaction_inserted_scalars(transaction: &EditorTransaction) -> Result<u64, String> {
    transaction.edits.iter().try_fold(0_u64, |total, edit| {
        let count = scalar_count(&edit.inserted_text)?;
        total
            .checked_add(count)
            .ok_or_else(|| "inserted scalar count overflow".to_owned())
    })
}

fn scalar_count(text: &str) -> Result<u64, String> {
    u64::try_from(inserted_text_counts(text).character_count)
        .map_err(|_| "inserted scalar count does not fit u64".to_owned())
}

fn add_edit_indicator(
    indicator: &mut EditIndicator,
    inserted: u64,
    event_link: IndicatorEventLink,
) -> Result<(), String> {
    checked_increment(&mut indicator.transactions, "edit indicator count")?;
    checked_add(
        &mut indicator.inserted_scalars,
        inserted,
        "edit indicator scalar count",
    )?;
    retain_bounded(&mut indicator.links, event_link);
    Ok(())
}

fn add_rejection_indicator(
    indicator: &mut RejectionIndicator,
    event_link: IndicatorEventLink,
) -> Result<(), String> {
    checked_increment(&mut indicator.attempts, "rejection indicator count")?;
    retain_bounded(&mut indicator.links, event_link);
    Ok(())
}

fn merge_edit_indicator(target: &mut EditIndicator, source: EditIndicator) -> Result<(), String> {
    checked_add(
        &mut target.transactions,
        source.transactions,
        "edit indicator count",
    )?;
    checked_add(
        &mut target.inserted_scalars,
        source.inserted_scalars,
        "edit indicator scalar count",
    )?;
    extend_bounded(&mut target.links, source.links);
    Ok(())
}

fn merge_rejection_indicator(
    target: &mut RejectionIndicator,
    source: RejectionIndicator,
) -> Result<(), String> {
    checked_add(
        &mut target.attempts,
        source.attempts,
        "rejection indicator count",
    )?;
    extend_bounded(&mut target.links, source.links);
    Ok(())
}

fn checked_increment(value: &mut u64, label: &str) -> Result<(), String> {
    checked_add(value, 1, label)
}

fn checked_add(value: &mut u64, addition: u64, label: &str) -> Result<(), String> {
    *value = value
        .checked_add(addition)
        .ok_or_else(|| format!("{label} overflow"))?;
    Ok(())
}

fn retain_bounded<T>(target: &mut Vec<T>, item: T) {
    if target.len() < MAX_RETAINED_INDICATOR_LINKS {
        target.push(item);
    }
}

fn extend_bounded<T>(target: &mut Vec<T>, source: Vec<T>) {
    let remaining = MAX_RETAINED_INDICATOR_LINKS.saturating_sub(target.len());
    target.extend(source.into_iter().take(remaining));
}

fn action_index(action: ControlledAction) -> usize {
    match action {
        ControlledAction::Build => 0,
        ControlledAction::Check => 1,
        ControlledAction::Test => 2,
        ControlledAction::Run => 3,
        ControlledAction::Clippy => 4,
        ControlledAction::Format => 5,
        ControlledAction::Doc => 6,
        ControlledAction::Add => 7,
        ControlledAction::Remove => 8,
        ControlledAction::Update => 9,
    }
}

fn command_indicator_mut(
    factual: &mut FactualIndicators,
    action: ControlledAction,
) -> &mut CommandIndicator {
    &mut factual.commands[action_index(action)]
}

fn is_check_test(action: ControlledAction) -> bool {
    matches!(action, ControlledAction::Check | ControlledAction::Test)
}

fn action_name(action: ControlledAction) -> &'static str {
    match action {
        ControlledAction::Build => "Build",
        ControlledAction::Check => "Check",
        ControlledAction::Test => "Test",
        ControlledAction::Run => "Run",
        ControlledAction::Clippy => "Clippy",
        ControlledAction::Format => "Format",
        ControlledAction::Doc => "Doc",
        ControlledAction::Add => "Add",
        ControlledAction::Remove => "Remove",
        ControlledAction::Update => "Update",
    }
}

fn ratio_at_least(value: u64, denominator: u64, numerator: u64, divisor: u64) -> bool {
    denominator > 0
        && u128::from(value) * u128::from(divisor)
            >= u128::from(denominator) * u128::from(numerator)
}

fn ratio_at_most(value: u64, denominator: u64, numerator: u64, divisor: u64) -> bool {
    denominator > 0
        && u128::from(value) * u128::from(divisor)
            <= u128::from(denominator) * u128::from(numerator)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustrace_model::{
        CaptureCompleteness, CommandCapture, CommandCaptureMode, CommandEnvironment,
        CommandFinished, CommandId, CommandOutcome, CommandStarted, CommandTermination,
        CommandTreeLink, ConsoleCommandRoute, ConsoleStdinRoute, ConsoleStdoutRoute,
        ControlledCommandFinished, EditorTransaction, ExternalObservation, Hash, InternalPaste,
        PasteInputChannel, PasteRejected, PasteRejectionReason, RecordedEventRef, SelectionState,
        SessionId, SessionResumed, TextEdit, WorkspaceDirectory, WorkspacePath,
    };

    fn link(sequence: u64) -> IndicatorEventLink {
        IndicatorEventLink {
            segment: 1,
            session_id: SessionId::new("indicator-test").unwrap(),
            sequence,
        }
    }

    fn eligible_values() -> ProcessValues {
        ProcessValues {
            inserted_keyboard_scalars: 1_000,
            keyboard_transactions: 20,
            appended_keyboard_scalars: 800,
            removed_keyboard_scalars: 100,
            forward_inserted_scalars: 1_250,
            before_first_build_scalars: 800,
            feedback_opportunities: 4,
            error_edit_rebuild_sequences: 1,
            complete_check_test_commands: 5,
            compiler_error_commands: 1,
            failed_test_commands: 0,
            origins: OriginScalars {
                keyboard: 1_000,
                internal_paste: 250,
                ..OriginScalars::default()
            },
        }
    }

    #[test]
    fn exact_sample_and_observation_boundaries_are_inclusive() {
        let exact = eligible_values();
        assert_eq!(
            evaluate(&exact, None),
            ProcessRuleOutcome::Suggested,
            "I=1000, N=20, I/J=0.8, A/I=0.8, R/I=0.1, F/O=0.25, and B/I=0.8 are inclusive"
        );

        let cases = [
            (
                "I=999",
                ProcessValues {
                    inserted_keyboard_scalars: 999,
                    removed_keyboard_scalars: 99,
                    forward_inserted_scalars: 1_248,
                    ..exact.clone()
                },
            ),
            (
                "N=19",
                ProcessValues {
                    keyboard_transactions: 19,
                    ..exact.clone()
                },
            ),
            (
                "I/J below 0.8",
                ProcessValues {
                    forward_inserted_scalars: 1_251,
                    ..exact.clone()
                },
            ),
            (
                "A/I below 0.8",
                ProcessValues {
                    appended_keyboard_scalars: 799,
                    ..exact.clone()
                },
            ),
            (
                "R/I above 0.1",
                ProcessValues {
                    removed_keyboard_scalars: 101,
                    ..exact.clone()
                },
            ),
            (
                "F/O above 0.25",
                ProcessValues {
                    error_edit_rebuild_sequences: 2,
                    before_first_build_scalars: 799,
                    ..exact.clone()
                },
            ),
            (
                "B/I below 0.8",
                ProcessValues {
                    before_first_build_scalars: 799,
                    error_edit_rebuild_sequences: 2,
                    ..exact.clone()
                },
            ),
        ];
        for (name, values) in cases {
            assert_ne!(
                evaluate(&values, None),
                ProcessRuleOutcome::Suggested,
                "{name}"
            );
        }
    }

    #[test]
    fn zero_denominators_low_build_count_and_single_signals_never_suggest() {
        assert_ne!(
            evaluate(&ProcessValues::default(), None),
            ProcessRuleOutcome::Suggested
        );
        let exact = eligible_values();
        for values in [
            ProcessValues {
                complete_check_test_commands: 1,
                ..exact.clone()
            },
            ProcessValues {
                feedback_opportunities: 1,
                error_edit_rebuild_sequences: 0,
                before_first_build_scalars: 0,
                ..exact.clone()
            },
            ProcessValues {
                appended_keyboard_scalars: 0,
                removed_keyboard_scalars: 500,
                ..exact.clone()
            },
        ] {
            assert_ne!(evaluate(&values, None), ProcessRuleOutcome::Suggested);
        }
    }

    #[test]
    fn zero_feedback_opportunities_is_ineligible_when_every_other_guard_passes() {
        let values = ProcessValues {
            feedback_opportunities: 0,
            error_edit_rebuild_sequences: 0,
            ..eligible_values()
        };
        assert_eq!(
            evaluate(&values, None),
            ProcessRuleOutcome::NotEligible {
                reason: "O=0 provides no feedback opportunity".to_owned(),
            }
        );
    }

    #[test]
    fn suggestion_is_deduplicated_and_uses_exact_neutral_wording() {
        let values = eligible_values();
        let attempt = ProcessAttempt {
            attempt: 1,
            session_id: SessionId::new("indicator-test").unwrap(),
            rule_version: PROCESS_RULE_VERSION,
            observations: observations(&values),
            outcome: evaluate(&values, None),
            values,
            edit_links: vec![link(2)],
            command_links: vec![link(3)],
        };
        let text = display_process_attempt(&attempt);
        assert_eq!(text.matches("Review suggested:").count(), 1, "{text}");
        assert_eq!(
            text,
            "Review suggested: substantial predominantly append entry with limited recorded revision, together with few observed error/edit/rebuild sequences among known opportunities and most qualifying entry preceded the first recorded Check/Test. Planned or familiar work can show the same pattern; these observations do not establish transcription, AI use or intent."
        );
        for prohibited in [
            "misconduct",
            "cheating",
            "AI probability",
            "authorship proof",
        ] {
            assert!(!text.contains(prohibited), "{text}");
        }
    }

    struct Harness {
        accumulator: AttemptIndicatorAccumulator,
        sequence: u64,
        text: String,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                accumulator: AttemptIndicatorAccumulator::new(
                    1,
                    SessionId::new("indicator-test").unwrap(),
                ),
                sequence: 0,
                text: String::new(),
            }
        }

        fn event(&mut self, event: Event, pre_edit: bool) {
            self.sequence += 1;
            let envelope = EventEnvelope {
                format_version: 1,
                session_id: SessionId::new("indicator-test").unwrap(),
                sequence: self.sequence,
                monotonic_millis: self.sequence,
                wall_clock_utc: None,
                previous_event_hash: Hash::zero(),
                event_hash: Hash::zero(),
                event,
            };
            self.accumulator
                .observe(&envelope, pre_edit.then_some(self.text.as_str()))
                .unwrap();
        }

        fn edit(&mut self, origin: EditOrigin, edits: Vec<TextEdit>) {
            self.event(
                Event::FileEdited(EditorTransaction {
                    document_id: rustrace_model::DocumentId::new("doc").unwrap(),
                    version_before: self.sequence,
                    version_after: self.sequence + 1,
                    origin,
                    edits: edits.clone(),
                    selection_before: SelectionState::default(),
                    selection_after: SelectionState::default(),
                    hash_before: Hash::zero(),
                    hash_after: Hash::zero(),
                }),
                true,
            );
            for edit in edits.into_iter().rev() {
                self.text.replace_range(
                    edit.start_byte as usize..edit.end_byte as usize,
                    &edit.inserted_text,
                );
            }
        }

        fn finish(self) -> (FactualIndicators, ProcessAttempt) {
            self.accumulator.finish().unwrap()
        }
    }

    #[test]
    fn streamed_edit_counts_use_unicode_scalars_crlf_eof_and_forward_origins() {
        let mut harness = Harness::new();
        harness.edit(
            EditOrigin::Keyboard,
            vec![TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: "é🦀\r\n".to_owned(),
            }],
        );
        harness.edit(
            EditOrigin::Keyboard,
            vec![TextEdit {
                start_byte: 0,
                end_byte: 2,
                inserted_text: "e\u{301}".to_owned(),
            }],
        );
        harness.edit(
            EditOrigin::Unknown,
            vec![TextEdit {
                start_byte: harness.text.len() as u64,
                end_byte: harness.text.len() as u64,
                inserted_text: "λ".to_owned(),
            }],
        );
        let (factual, attempt) = harness.finish();

        assert_eq!(attempt.values.inserted_keyboard_scalars, 6);
        assert_eq!(attempt.values.appended_keyboard_scalars, 4);
        assert_eq!(attempt.values.removed_keyboard_scalars, 1);
        assert_eq!(attempt.values.forward_inserted_scalars, 7);
        assert_eq!(attempt.values.origins.keyboard, 6);
        assert_eq!(attempt.values.origins.unknown, 1);
        assert_eq!(factual.unknown_origin_edit.transactions, 1);
        assert_eq!(factual.unknown_origin_edit.inserted_scalars, 1);
        assert_eq!(factual.unknown_origin_edit.links, [link(3)]);
    }

    #[test]
    fn deletion_replacement_and_multi_range_or_inverse_events_are_classified_exactly() {
        let mut harness = Harness::new();
        harness.text = "abc🦀".to_owned();
        harness.edit(
            EditOrigin::Keyboard,
            vec![TextEdit {
                start_byte: 3,
                end_byte: 7,
                inserted_text: String::new(),
            }],
        );
        harness.edit(
            EditOrigin::Keyboard,
            vec![
                TextEdit {
                    start_byte: 0,
                    end_byte: 0,
                    inserted_text: "x".to_owned(),
                },
                TextEdit {
                    start_byte: 1,
                    end_byte: 1,
                    inserted_text: "y".to_owned(),
                },
            ],
        );
        harness.edit(
            EditOrigin::Undo,
            vec![TextEdit {
                start_byte: 0,
                end_byte: 1,
                inserted_text: String::new(),
            }],
        );
        let (_, attempt) = harness.finish();

        assert_eq!(attempt.values.removed_keyboard_scalars, 1);
        assert_eq!(attempt.values.inserted_keyboard_scalars, 0);
        assert_eq!(attempt.values.keyboard_transactions, 0);
        assert_eq!(attempt.values.forward_inserted_scalars, 2);
        assert!(matches!(
            attempt.outcome,
            ProcessRuleOutcome::Unavailable { .. }
        ));
    }

    #[test]
    fn append_requires_an_empty_removal_range_at_pre_edit_eof() {
        let mut harness = Harness::new();
        harness.text = "abc".to_owned();
        harness.edit(
            EditOrigin::Keyboard,
            vec![TextEdit {
                start_byte: 1,
                end_byte: 1,
                inserted_text: "x".to_owned(),
            }],
        );
        harness.edit(
            EditOrigin::Keyboard,
            vec![TextEdit {
                start_byte: 3,
                end_byte: 4,
                inserted_text: "y".to_owned(),
            }],
        );
        let (_, attempt) = harness.finish();

        assert_eq!(attempt.values.inserted_keyboard_scalars, 2);
        assert_eq!(attempt.values.keyboard_transactions, 2);
        assert_eq!(attempt.values.appended_keyboard_scalars, 0);
        assert_eq!(attempt.values.removed_keyboard_scalars, 1);
    }

    #[test]
    fn genesis_is_excluded_file_creation_allows_later_append_and_lifecycle_blocks_rule() {
        let mut harness = Harness::new();
        harness.event(
            Event::FileCreated(rustrace_model::FileCreated {
                document_id: rustrace_model::DocumentId::new("new-doc").unwrap(),
                path: rustrace_model::WorkspacePath::new("new.rs").unwrap(),
                contents: "starter".to_owned(),
                content_hash: Hash::zero(),
            }),
            false,
        );
        harness.text = "starter".to_owned();
        harness.edit(
            EditOrigin::Keyboard,
            vec![TextEdit {
                start_byte: 7,
                end_byte: 7,
                inserted_text: "x".to_owned(),
            }],
        );
        harness.event(
            Event::FileRenamed(rustrace_model::FileRenamed {
                document_id: rustrace_model::DocumentId::new("new-doc").unwrap(),
                old_path: rustrace_model::WorkspacePath::new("new.rs").unwrap(),
                new_path: rustrace_model::WorkspacePath::new("renamed.rs").unwrap(),
            }),
            false,
        );
        let (_, attempt) = harness.finish();
        assert_eq!(attempt.values.forward_inserted_scalars, 1);
        assert!(matches!(
            attempt.outcome,
            ProcessRuleOutcome::Unavailable { ref reason } if reason.contains("file rename")
        ));
    }

    #[test]
    fn factual_paste_and_rejection_metrics_are_exact_linked_and_bounded() {
        let mut harness = Harness::new();
        let transaction = |origin| EditorTransaction {
            document_id: rustrace_model::DocumentId::new("doc").unwrap(),
            version_before: 0,
            version_after: 1,
            origin,
            edits: vec![TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: "é🦀\r\n".to_owned(),
            }],
            selection_before: SelectionState::default(),
            selection_after: SelectionState::default(),
            hash_before: Hash::zero(),
            hash_after: Hash::zero(),
        };
        harness.event(Event::FileEdited(transaction(EditOrigin::Paste)), true);
        harness.event(
            Event::InternalPaste(InternalPaste {
                source: RecordedEventRef {
                    session_id: SessionId::new("indicator-test").unwrap(),
                    sequence: 1,
                    event_hash: Hash::zero(),
                },
                transaction: transaction(EditOrigin::Paste),
            }),
            true,
        );
        harness.event(
            Event::ExternalObservation(ExternalObservation {
                path: rustrace_model::WorkspacePath::new("main.rs").unwrap(),
                saved_hash: None,
                logical_hash: None,
                observed_hash: None,
                evidence_hash: Hash::zero(),
            }),
            false,
        );
        for _ in 0..300 {
            harness.event(
                Event::PasteRejected(PasteRejected {
                    reason: PasteRejectionReason::ExternalInput,
                    channel: PasteInputChannel::TerminalBracketed,
                }),
                false,
            );
        }
        let (factual, _) = harness.finish();
        assert_eq!(factual.historical_origin_unverified_paste.transactions, 1);
        assert_eq!(
            factual.historical_origin_unverified_paste.inserted_scalars,
            4
        );
        assert_eq!(factual.allowed_internal_paste.transactions, 1);
        assert_eq!(factual.allowed_internal_paste.inserted_scalars, 4);
        assert_eq!(factual.rejected_external_change.attempts, 1);
        assert_eq!(factual.rejected_paste.attempts, 300);
        assert_eq!(
            factual.rejected_paste.links.len(),
            MAX_RETAINED_INDICATOR_LINKS
        );
    }

    #[test]
    fn terminal_factual_summary_keeps_every_count_and_first_compact_link() {
        let mut factual = FactualIndicators {
            rejected_external_change: RejectionIndicator {
                attempts: 2,
                links: vec![link(1), link(2)],
            },
            unknown_origin_edit: EditIndicator {
                transactions: 3,
                inserted_scalars: 4,
                links: vec![link(3)],
            },
            allowed_internal_paste: EditIndicator {
                transactions: 5,
                inserted_scalars: 6,
                links: vec![link(4)],
            },
            historical_origin_unverified_paste: EditIndicator {
                transactions: 7,
                inserted_scalars: 8,
                links: vec![link(5)],
            },
            rejected_paste: RejectionIndicator {
                attempts: 9,
                links: vec![link(6), link(7)],
            },
            ..FactualIndicators::default()
        };
        factual.commands[1] = CommandIndicator {
            action: ControlledAction::Check,
            started: 10,
            complete: 11,
            exit_zero: 12,
            nonzero_exit: 13,
            compiler_errors: 14,
            unknown: 15,
            links: vec![CommandEventLink {
                start: link(8),
                finish: Some(link(9)),
            }],
        };

        let summary = display_terminal_factual_indicators(&factual);
        for expected in [
            "external-change observations: 2 rejected attempts [segment:1 seq:1]",
            "unknown-origin edits: 3 / 4 chars (Unicode scalars) [segment:1 seq:3]",
            "allowed internal paste: 5 / 6 chars (Unicode scalars) [segment:1 seq:4]",
            "historical origin-unverified paste: 7 / 8 chars (Unicode scalars) [segment:1 seq:5]",
            "rejected paste attempts: 9 [segment:1 seq:6]",
            "repeated rejected external-change attempts: 2 [segment:1 seq:1]",
            "repeated rejected paste attempts: 9 [segment:1 seq:6]",
            "Cargo Check: started 10 complete 11 exit-0 12 nonzero 13 compiler-errors 14 unknown 15 [segment:1 seq:8]",
        ] {
            assert!(
                summary.contains(expected),
                "missing {expected:?}: {summary}"
            );
        }
        assert!(!summary.contains("session:"), "{summary}");
        assert!(summary.len() < 4 * 1024, "{summary}");
    }

    fn started(id: &str, action: ControlledAction) -> ControlledCommandStarted {
        ControlledCommandStarted {
            command_id: CommandId::new(id).unwrap(),
            action,
            argv: vec![],
            environment: CommandEnvironment {
                policy_version: 1,
                retained_names: vec![],
            },
            selected_toolchain: "1.98.1".to_owned(),
            tools: vec![],
            before: CommandTreeLink {
                checkpoint_sequence: 1,
                checkpoint_event_hash: Hash::zero(),
                workspace_hash: Hash::zero(),
                workspace_version: 1,
            },
            deadline_millis: 1_000,
            output_limit: 1024 * 1024,
            console: None,
        }
    }

    fn capture(bytes: usize, completeness: CaptureCompleteness) -> CommandCapture {
        CommandCapture {
            bytes: bytes as u64,
            completeness,
            mode: CommandCaptureMode::Captured,
        }
    }

    fn command(
        harness: &mut Harness,
        id: &str,
        action: ControlledAction,
        stdout: &[u8],
        outcome: CommandOutcome,
        completeness: CaptureCompleteness,
    ) {
        let start = started(id, action);
        command_with_start(harness, start, stdout, outcome, completeness);
    }

    fn command_with_start(
        harness: &mut Harness,
        start: ControlledCommandStarted,
        stdout: &[u8],
        outcome: CommandOutcome,
        completeness: CaptureCompleteness,
    ) {
        harness.event(Event::ControlledCommandStarted(start.clone()), false);
        if !stdout.is_empty() {
            harness.event(
                Event::ControlledCommandOutput(
                    ControlledCommandOutput::from_bytes(
                        start.command_id.clone(),
                        rustrace_model::OutputStream::Stdout,
                        0,
                        stdout,
                    )
                    .unwrap(),
                ),
                false,
            );
        }
        harness.event(
            Event::ControlledCommandFinished(ControlledCommandFinished {
                command_id: start.command_id,
                after: start.before,
                started_millis: 1,
                finished_millis: 2,
                outcome,
                stdout: capture(stdout.len(), completeness),
                stderr: capture(0, completeness),
            }),
            false,
        );
    }

    fn run_facts(factual: &FactualIndicators) -> &CommandIndicator {
        factual
            .commands
            .iter()
            .find(|facts| facts.action == ControlledAction::Run)
            .unwrap()
    }

    fn successful_output() -> &'static [u8] {
        b"{\"reason\":\"build-finished\",\"success\":true}\n"
    }

    fn compiler_error_output() -> Vec<u8> {
        let diagnostic = serde_json::json!({
            "reason": "compiler-message",
            "package_id": "student 0.1.0",
            "manifest_path": "/work/Cargo.toml",
            "target": {
                "kind": ["bin"], "crate_types": ["bin"], "name": "student",
                "src_path": "/work/src/main.rs", "edition": "2024"
            },
            "message": {
                "rendered": "error: broken\n", "message": "broken",
                "code": null, "level": "error", "spans": [], "children": []
            }
        });
        format!("{diagnostic}\n{{\"reason\":\"build-finished\",\"success\":false}}\n").into_bytes()
    }

    #[test]
    fn natural_console_and_packaged_runs_are_known_without_structured_diagnostics() {
        for (id, stdin) in [
            ("console-run", ConsoleStdinRoute::Submitted),
            (
                "packaged-run",
                ConsoleStdinRoute::File {
                    path: WorkspacePath::new("case.in").unwrap(),
                },
            ),
        ] {
            let mut harness = Harness::new();
            let mut start = started(id, ControlledAction::Run);
            start.argv = vec![
                "rustup".to_owned(),
                "run".to_owned(),
                "1.98.1".to_owned(),
                "cargo".to_owned(),
                "run".to_owned(),
                "--locked".to_owned(),
            ];
            start.console = Some(ConsoleCommandRoute {
                stdin,
                stdout: ConsoleStdoutRoute::Console,
            });
            command_with_start(
                &mut harness,
                start,
                b"natural program output\n",
                CommandOutcome::Exited { code: 0 },
                CaptureCompleteness::Complete,
            );

            let (factual, _) = harness.finish();
            let run = run_facts(&factual);
            assert_eq!(run.started, 1, "{id}");
            assert_eq!(run.complete, 1, "{id}");
            assert_eq!(run.exit_zero, 1, "{id}");
            assert_eq!(run.nonzero_exit, 0, "{id}");
            assert_eq!(run.compiler_errors, 0, "{id}");
            assert_eq!(run.unknown, 0, "{id}");
            assert!(
                display_terminal_factual_indicators(&factual).contains(
                    "Cargo Run: started 1 complete 1 exit-0 1 nonzero 0 compiler-errors 0 unknown 0"
                ),
                "{id}"
            );
        }
    }

    #[test]
    fn every_natural_console_action_uses_exit_and_capture_evidence() {
        for action in [
            ControlledAction::Build,
            ControlledAction::Check,
            ControlledAction::Test,
            ControlledAction::Run,
            ControlledAction::Clippy,
            ControlledAction::Doc,
            ControlledAction::Add,
            ControlledAction::Remove,
            ControlledAction::Update,
        ] {
            for (code, completeness, known) in [
                (0, CaptureCompleteness::Complete, true),
                (101, CaptureCompleteness::Complete, true),
                (0, CaptureCompleteness::Truncated, false),
            ] {
                let mut harness = Harness::new();
                let mut start = started("natural-console", action);
                start.argv = vec!["--locked".to_owned()];
                start.console = Some(ConsoleCommandRoute {
                    stdin: ConsoleStdinRoute::Closed,
                    stdout: ConsoleStdoutRoute::Console,
                });
                command_with_start(
                    &mut harness,
                    start,
                    b"ordinary output\n",
                    CommandOutcome::Exited { code },
                    completeness,
                );
                let (factual, _) = harness.finish();
                let facts = factual
                    .commands
                    .iter()
                    .find(|facts| facts.action == action)
                    .unwrap();
                assert_eq!(facts.complete, u64::from(known), "{action:?}");
                assert_eq!(facts.unknown, u64::from(!known), "{action:?}");
                assert_eq!(facts.exit_zero, u64::from(known && code == 0), "{action:?}");
                assert_eq!(
                    facts.nonzero_exit,
                    u64::from(known && code != 0),
                    "{action:?}"
                );
                assert_eq!(facts.compiler_errors, 0, "{action:?}");
            }
        }
    }

    #[test]
    fn f7_json_run_keeps_structured_diagnostic_classification() {
        let mut harness = Harness::new();
        let mut start = started("f7-run", ControlledAction::Run);
        start.argv = vec![
            "rustup".to_owned(),
            "run".to_owned(),
            "1.98.1".to_owned(),
            "cargo".to_owned(),
            "run".to_owned(),
            "--message-format=json".to_owned(),
            "--locked".to_owned(),
        ];
        command_with_start(
            &mut harness,
            start,
            successful_output(),
            CommandOutcome::Exited { code: 0 },
            CaptureCompleteness::Complete,
        );
        let mut malformed = started("f7-run-malformed", ControlledAction::Run);
        malformed.argv = vec![
            "rustup".to_owned(),
            "run".to_owned(),
            "1.98.1".to_owned(),
            "cargo".to_owned(),
            "run".to_owned(),
            "--message-format=json".to_owned(),
            "--locked".to_owned(),
        ];
        command_with_start(
            &mut harness,
            malformed,
            b"ordinary non-JSON output\n",
            CommandOutcome::Exited { code: 0 },
            CaptureCompleteness::Complete,
        );

        let (factual, _) = harness.finish();
        let run = run_facts(&factual);
        assert_eq!(run.complete, 1);
        assert_eq!(run.exit_zero, 1);
        assert_eq!(run.started, 2);
        assert_eq!(run.unknown, 1);
    }

    #[test]
    fn complete_pairing_distinguishes_known_nonzero_and_compiler_errors() {
        let mut harness = Harness::new();
        command(
            &mut harness,
            "check-1",
            ControlledAction::Check,
            &compiler_error_output(),
            CommandOutcome::Exited { code: 1 },
            CaptureCompleteness::Complete,
        );
        harness.edit(
            EditOrigin::Keyboard,
            vec![TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: "x".to_owned(),
            }],
        );
        command(
            &mut harness,
            "test-1",
            ControlledAction::Test,
            successful_output(),
            CommandOutcome::Exited { code: 7 },
            CaptureCompleteness::Complete,
        );
        let (factual, attempt) = harness.finish();
        assert_eq!(attempt.values.complete_check_test_commands, 2);
        assert_eq!(attempt.values.feedback_opportunities, 1);
        assert_eq!(attempt.values.error_edit_rebuild_sequences, 1);
        assert_eq!(attempt.values.compiler_error_commands, 1);
        assert_eq!(attempt.values.failed_test_commands, 1);
        assert_eq!(
            factual
                .commands
                .iter()
                .find(|facts| facts.action == ControlledAction::Test)
                .unwrap()
                .nonzero_exit,
            1
        );
    }

    #[test]
    fn streamed_eligible_entry_plus_before_build_suggests_once_with_exact_links() {
        let mut harness = Harness::new();
        for _ in 0..20 {
            let eof = harness.text.len() as u64;
            harness.edit(
                EditOrigin::Keyboard,
                vec![TextEdit {
                    start_byte: eof,
                    end_byte: eof,
                    inserted_text: "x".repeat(50),
                }],
            );
        }
        command(
            &mut harness,
            "check-1",
            ControlledAction::Check,
            successful_output(),
            CommandOutcome::Exited { code: 0 },
            CaptureCompleteness::Complete,
        );
        let eof = harness.text.len() as u64;
        harness.edit(
            EditOrigin::Keyboard,
            vec![TextEdit {
                start_byte: eof,
                end_byte: eof,
                inserted_text: "y".to_owned(),
            }],
        );
        command(
            &mut harness,
            "check-2",
            ControlledAction::Check,
            successful_output(),
            CommandOutcome::Exited { code: 0 },
            CaptureCompleteness::Complete,
        );
        let (_, attempt) = harness.finish();

        assert_eq!(attempt.values.inserted_keyboard_scalars, 1_001);
        assert_eq!(attempt.values.keyboard_transactions, 21);
        assert_eq!(attempt.values.appended_keyboard_scalars, 1_001);
        assert_eq!(attempt.values.before_first_build_scalars, 1_000);
        assert_eq!(attempt.values.feedback_opportunities, 1);
        assert!(attempt.observations.entry);
        assert!(!attempt.observations.feedback);
        assert!(attempt.observations.before_build);
        assert_eq!(attempt.outcome, ProcessRuleOutcome::Suggested);
        assert_eq!(attempt.edit_links.len(), 21);
        assert_eq!(attempt.command_links.len(), 4);
        assert_eq!(
            display_process_attempt(&attempt)
                .matches("Review suggested:")
                .count(),
            1
        );
    }

    #[test]
    fn launch_timeout_cancel_truncated_and_malformed_or_missing_diagnostics_suppress_rule() {
        let cases = [
            (
                "launch failure",
                CommandOutcome::LaunchFailed { os_code: Some(2) },
                successful_output().to_vec(),
                CaptureCompleteness::Unavailable,
            ),
            (
                "timeout",
                CommandOutcome::Terminated {
                    reason: CommandTermination::Deadline,
                    signal: None,
                },
                successful_output().to_vec(),
                CaptureCompleteness::Complete,
            ),
            (
                "cancellation",
                CommandOutcome::Terminated {
                    reason: CommandTermination::Cancelled,
                    signal: None,
                },
                successful_output().to_vec(),
                CaptureCompleteness::Complete,
            ),
            (
                "truncated output",
                CommandOutcome::Exited { code: 0 },
                successful_output().to_vec(),
                CaptureCompleteness::Truncated,
            ),
            (
                "output read failure",
                CommandOutcome::Terminated {
                    reason: CommandTermination::CaptureFailure,
                    signal: None,
                },
                successful_output().to_vec(),
                CaptureCompleteness::ReadFailed,
            ),
            (
                "malformed diagnostics",
                CommandOutcome::Exited { code: 0 },
                b"not cargo json\n".to_vec(),
                CaptureCompleteness::Complete,
            ),
            (
                "missing diagnostics",
                CommandOutcome::Exited { code: 0 },
                vec![],
                CaptureCompleteness::Complete,
            ),
        ];
        for (reason, outcome, stdout, completeness) in cases {
            let mut harness = Harness::new();
            command(
                &mut harness,
                "check",
                ControlledAction::Check,
                &stdout,
                outcome,
                completeness,
            );
            let (_, attempt) = harness.finish();
            assert!(
                matches!(attempt.outcome, ProcessRuleOutcome::Unavailable { .. }),
                "{reason}: {:?}",
                attempt.outcome
            );
        }
    }

    #[test]
    fn unpaired_commands_are_unknown_and_resume_breaks_feedback_adjacency() {
        let mut unpaired = Harness::new();
        unpaired.event(
            Event::ControlledCommandStarted(started("check", ControlledAction::Check)),
            false,
        );
        let (_, attempt) = unpaired.finish();
        assert!(matches!(
            attempt.outcome,
            ProcessRuleOutcome::Unavailable { ref reason } if reason.contains("unpaired")
        ));

        let mut resumed = Harness::new();
        command(
            &mut resumed,
            "check-1",
            ControlledAction::Check,
            successful_output(),
            CommandOutcome::Exited { code: 0 },
            CaptureCompleteness::Complete,
        );
        resumed.edit(
            EditOrigin::Keyboard,
            vec![TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: "x".to_owned(),
            }],
        );
        resumed.event(
            Event::SessionResumed(SessionResumed { last_sequence: 3 }),
            false,
        );
        command(
            &mut resumed,
            "check-2",
            ControlledAction::Check,
            successful_output(),
            CommandOutcome::Exited { code: 0 },
            CaptureCompleteness::Complete,
        );
        assert_eq!(resumed.finish().1.values.feedback_opportunities, 0);
    }

    #[test]
    fn orphan_finish_is_unavailable_and_context_commands_are_not_opportunities() {
        let mut orphan = Harness::new();
        let start = started("orphan", ControlledAction::Check);
        orphan.event(
            Event::ControlledCommandFinished(ControlledCommandFinished {
                command_id: start.command_id,
                after: start.before,
                started_millis: 1,
                finished_millis: 2,
                outcome: CommandOutcome::Exited { code: 0 },
                stdout: capture(0, CaptureCompleteness::Complete),
                stderr: capture(0, CaptureCompleteness::Complete),
            }),
            false,
        );
        assert!(matches!(
            orphan.finish().1.outcome,
            ProcessRuleOutcome::Unavailable { ref reason } if reason.contains("unpaired")
        ));

        let mut context = Harness::new();
        command(
            &mut context,
            "check-1",
            ControlledAction::Check,
            successful_output(),
            CommandOutcome::Exited { code: 0 },
            CaptureCompleteness::Complete,
        );
        command(
            &mut context,
            "run-1",
            ControlledAction::Run,
            successful_output(),
            CommandOutcome::Exited { code: 0 },
            CaptureCompleteness::Complete,
        );
        context.edit(
            EditOrigin::Keyboard,
            vec![TextEdit {
                start_byte: 0,
                end_byte: 0,
                inserted_text: "x".to_owned(),
            }],
        );
        command(
            &mut context,
            "test-1",
            ControlledAction::Test,
            successful_output(),
            CommandOutcome::Exited { code: 0 },
            CaptureCompleteness::Complete,
        );
        let (facts, attempt) = context.finish();
        assert_eq!(attempt.values.feedback_opportunities, 1);
        assert_eq!(attempt.values.complete_check_test_commands, 2);
        assert_eq!(
            facts
                .commands
                .iter()
                .find(|facts| facts.action == ControlledAction::Run)
                .unwrap()
                .complete,
            1
        );
    }

    #[test]
    fn historical_cargo_commands_are_counted_as_unknown_and_suppress_check_test_rule() {
        let mut harness = Harness::new();
        let command_id = CommandId::new("legacy-check").unwrap();
        harness.event(
            Event::CargoCommandStarted(CommandStarted {
                command_id: command_id.clone(),
                program: "/usr/bin/cargo".to_owned(),
                arguments: vec!["check".to_owned()],
                working_directory: WorkspaceDirectory::new(".").unwrap(),
            }),
            false,
        );
        harness.event(
            Event::CargoCommandFinished(CommandFinished {
                command_id,
                exit_code: Some(0),
                success: true,
            }),
            false,
        );
        let (factual, attempt) = harness.finish();
        let check = factual
            .commands
            .iter()
            .find(|facts| facts.action == ControlledAction::Check)
            .unwrap();
        assert_eq!(check.started, 1);
        assert_eq!(check.complete, 0);
        assert_eq!(check.unknown, 1);
        assert_eq!(check.links[0].start, link(1));
        assert_eq!(check.links[0].finish, Some(link(2)));
        assert!(matches!(
            attempt.outcome,
            ProcessRuleOutcome::Unavailable { ref reason }
                if reason.contains("historical capture completeness")
        ));
    }

    #[test]
    fn every_disallowed_edit_shape_has_a_specific_unavailable_result() {
        let cases = [
            (
                "undo",
                Event::FileEdited(EditorTransaction {
                    document_id: rustrace_model::DocumentId::new("doc").unwrap(),
                    version_before: 0,
                    version_after: 1,
                    origin: EditOrigin::Undo,
                    edits: vec![TextEdit {
                        start_byte: 0,
                        end_byte: 0,
                        inserted_text: String::new(),
                    }],
                    selection_before: SelectionState::default(),
                    selection_after: SelectionState::default(),
                    hash_before: Hash::zero(),
                    hash_after: Hash::zero(),
                }),
            ),
            (
                "redo",
                Event::FileEdited(EditorTransaction {
                    document_id: rustrace_model::DocumentId::new("doc").unwrap(),
                    version_before: 0,
                    version_after: 1,
                    origin: EditOrigin::Redo,
                    edits: vec![TextEdit {
                        start_byte: 0,
                        end_byte: 0,
                        inserted_text: String::new(),
                    }],
                    selection_before: SelectionState::default(),
                    selection_after: SelectionState::default(),
                    hash_before: Hash::zero(),
                    hash_after: Hash::zero(),
                }),
            ),
            (
                "file deletion",
                Event::FileDeleted(rustrace_model::FileDeleted {
                    document_id: rustrace_model::DocumentId::new("doc").unwrap(),
                    path: rustrace_model::WorkspacePath::new("main.rs").unwrap(),
                    previous_hash: Hash::zero(),
                }),
            ),
            (
                "file rename",
                Event::FileRenamed(rustrace_model::FileRenamed {
                    document_id: rustrace_model::DocumentId::new("doc").unwrap(),
                    old_path: rustrace_model::WorkspacePath::new("main.rs").unwrap(),
                    new_path: rustrace_model::WorkspacePath::new("lib.rs").unwrap(),
                }),
            ),
            (
                "multi-range",
                Event::FileEdited(EditorTransaction {
                    document_id: rustrace_model::DocumentId::new("doc").unwrap(),
                    version_before: 0,
                    version_after: 1,
                    origin: EditOrigin::Keyboard,
                    edits: vec![
                        TextEdit {
                            start_byte: 0,
                            end_byte: 0,
                            inserted_text: "a".to_owned(),
                        },
                        TextEdit {
                            start_byte: 0,
                            end_byte: 0,
                            inserted_text: "b".to_owned(),
                        },
                    ],
                    selection_before: SelectionState::default(),
                    selection_after: SelectionState::default(),
                    hash_before: Hash::zero(),
                    hash_after: Hash::zero(),
                }),
            ),
        ];
        for (name, event) in cases {
            let mut harness = Harness::new();
            harness.event(event, true);
            let outcome = harness.finish().1.outcome;
            assert!(
                matches!(outcome, ProcessRuleOutcome::Unavailable { .. }),
                "{name}: {outcome:?}"
            );
        }
    }

    #[test]
    fn checked_counters_report_overflow_without_wrapping() {
        let mut value = u64::MAX;
        assert_eq!(
            checked_increment(&mut value, "fixture").unwrap_err(),
            "fixture overflow"
        );
        assert_eq!(value, u64::MAX);
    }

    #[test]
    fn navigation_keeps_visible_links_from_later_categories_and_attempts() {
        let mut indicators = ReviewIndicators::default();
        indicators.factual.allowed_internal_paste.links = (1..=MAX_RETAINED_INDICATOR_LINKS as u64)
            .map(link)
            .collect();
        indicators.factual.rejected_paste.links.push(link(300));
        let attempt = ProcessAttempt {
            attempt: 2,
            session_id: SessionId::new("second-attempt").unwrap(),
            rule_version: PROCESS_RULE_VERSION,
            values: ProcessValues::default(),
            observations: ProcessObservations::default(),
            outcome: ProcessRuleOutcome::NotEligible {
                reason: "fixture".to_owned(),
            },
            edit_links: vec![IndicatorEventLink {
                segment: 2,
                session_id: SessionId::new("second-attempt").unwrap(),
                sequence: 2,
            }],
            command_links: vec![],
        };
        let later_attempt_link = attempt.edit_links[0].clone();
        indicators.attempts.push(attempt);

        let links = indicator_links(&indicators);
        assert!(links.contains(&link(300)));
        assert!(links.contains(&later_attempt_link));
        assert_eq!(
            links
                .iter()
                .filter(|candidate| candidate.segment == 1)
                .count(),
            9
        );
    }

    #[test]
    fn detailed_presentation_includes_values_thresholds_links_and_neutral_context() {
        let values = eligible_values();
        let attempt = ProcessAttempt {
            attempt: 1,
            session_id: SessionId::new("indicator-test").unwrap(),
            rule_version: PROCESS_RULE_VERSION,
            observations: observations(&values),
            outcome: evaluate(&values, None),
            values,
            edit_links: vec![link(2)],
            command_links: vec![link(3), link(4)],
        };
        let mut text = display_process_attempt_details(&attempt);
        text.push_str(&display_factual_indicators(&FactualIndicators::default()).join("; "));
        text.push_str(INDICATOR_EXPLANATION);
        for expected in [
            PROCESS_RULE_VERSION,
            "I=1000 N=20 A=800 R=100 J=1250 B=800 O=4 F=1",
            "I/J=1000/1250",
            "thresholds I>=1000",
            "segment:1 session:indicator-test seq:2",
            "complete Check/Test=5",
            "Unknown means a missing origin explanation",
            "Internal paste is allowed and not inherently suspicious",
            "metadata only, never content size",
            "; edit links segment:1 session:indicator-test seq:2; command links segment:1 session:indicator-test seq:3",
        ] {
            assert!(text.contains(expected), "missing {expected:?}: {text}");
        }
        let lower = text.to_ascii_lowercase();
        for prohibited in [
            "misconduct",
            "cheating",
            "ai probability",
            "authorship proof",
        ] {
            assert!(!lower.contains(prohibited), "{text}");
        }
    }

    #[test]
    fn internal_reuse_contributes_context_but_never_an_observation() {
        let mut values = eligible_values();
        values.origins.internal_paste = 250;
        let observations = observations(&values);
        assert!(observations.entry);
        assert!(observations.feedback);
        assert!(observations.before_build);

        values.forward_inserted_scalars = 1_251;
        assert!(matches!(
            evaluate(&values, None),
            ProcessRuleOutcome::NotEligible { .. }
        ));
    }

    #[test]
    fn diagnostic_outcome_enum_keeps_unknown_distinct() {
        assert_ne!(DiagnosticOutcome::Unknown, DiagnosticOutcome::Success);
    }
}
