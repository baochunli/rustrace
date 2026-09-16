use super::*;
use crate::{
    diagnostics::{DiagnosticNavigation, DiagnosticOutcome},
    language_service::{LiveDiagnosticSeverity, PublishedDiagnostics},
    tui::{DiagnosticLineMarker, DiagnosticMarkerKind, OutputRow},
};

#[derive(Clone, Debug, Eq, PartialEq)]
enum NavigableDiagnostic {
    Cargo {
        index: usize,
        path: Option<WorkspacePath>,
        start_byte: u64,
    },
    Live {
        document_id: DocumentId,
        index: usize,
        path: WorkspacePath,
        start_byte: u64,
    },
}

impl ProductionSession {
    pub(super) fn apply_live_diagnostic_poll(
        &mut self,
        generation: Option<u64>,
        notifications: Vec<PublishedDiagnostics>,
    ) -> bool {
        let mut changed = false;
        if self.live_diagnostic_generation != generation {
            changed |= !self.live_diagnostics.is_empty()
                || self.selected_live_diagnostic.is_some()
                || self.live_diagnostic_generation.is_some();
            self.live_diagnostics.clear();
            self.selected_live_diagnostic = None;
            self.live_diagnostic_generation = generation;
        }
        let Some(generation) = generation else {
            return changed;
        };

        self.live_diagnostics.retain(|document_id, current| {
            let keep = current.generation == generation
                && crate::language_service::supports_document(&current.path)
                && self
                    .workspace
                    .buffer_by_document_id(document_id)
                    .is_some_and(|buffer| {
                        buffer.version() == current.version
                            && self.workspace.file_tree().iter().any(|file| {
                                file.path() == &current.path
                                    && file.document_id() == Some(document_id)
                                    && file.is_editable()
                            })
                    });
            changed |= !keep;
            keep
        });
        if self
            .selected_live_diagnostic
            .as_ref()
            .is_some_and(|(document_id, index)| {
                self.live_diagnostics
                    .get(document_id)
                    .is_none_or(|document| *index >= document.diagnostics.len())
            })
        {
            self.selected_live_diagnostic = None;
            changed = true;
        }

        for notification in notifications {
            if notification.generation != generation {
                continue;
            }
            let Some(converted) = self.convert_live_diagnostics(notification) else {
                continue;
            };
            let document_id = converted.document_id.clone();
            if self
                .selected_live_diagnostic
                .as_ref()
                .is_some_and(|(selected, _)| selected == &document_id)
            {
                self.selected_live_diagnostic = None;
            }
            if converted.diagnostics.is_empty() {
                changed |= self.live_diagnostics.remove(&document_id).is_some();
            } else if self.live_diagnostics.get(&document_id) != Some(&converted) {
                self.live_diagnostics.insert(document_id, converted);
                changed = true;
            }
        }
        changed
    }

    fn convert_live_diagnostics(
        &self,
        notification: PublishedDiagnostics,
    ) -> Option<LiveDocumentDiagnostics> {
        if !crate::language_service::supports_document(&notification.path) {
            return None;
        }
        let buffer = self
            .workspace
            .buffer_by_document_id(&notification.document_id)?;
        if buffer.version() != notification.version
            || self.workspace.file_tree().iter().all(|file| {
                file.path() != &notification.path
                    || file.document_id() != Some(&notification.document_id)
                    || !file.is_editable()
            })
        {
            return None;
        }
        let positions = buffer.positions(notification.version).ok()?;
        let mut diagnostics = Vec::with_capacity(notification.diagnostics.len());
        for diagnostic in notification
            .diagnostics
            .into_iter()
            .take(crate::language_service::MAX_LIVE_DIAGNOSTICS_PER_DOCUMENT)
        {
            let start = positions.utf16_to_byte_clamped(diagnostic.start).ok()?;
            let end = positions.utf16_to_byte_clamped(diagnostic.end).ok()?;
            if start.0 > end.0 {
                return None;
            }
            let start_line = positions.byte_to_line(start).ok()?;
            let mut end_line = positions.byte_to_line(end).ok()?;
            if end.0 > start.0
                && end_line > start_line
                && end.0 == buffer.line_start_byte(end_line) as u64
            {
                end_line -= 1;
            }
            let kind = match diagnostic.severity {
                LiveDiagnosticSeverity::Error => DiagnosticMarkerKind::Error,
                LiveDiagnosticSeverity::Warning => DiagnosticMarkerKind::Warning,
                LiveDiagnosticSeverity::Information | LiveDiagnosticSeverity::Hint => {
                    DiagnosticMarkerKind::Other
                }
            };
            diagnostics.push(crate::editor::LiveDiagnosticSpan {
                start_byte: start.0,
                end_byte: end.0,
                start_line,
                end_line,
                kind,
                message: crate::display::label(
                    &diagnostic.message,
                    crate::language_service::MAX_LIVE_DIAGNOSTIC_MESSAGE_BYTES,
                ),
            });
        }
        Some(LiveDocumentDiagnostics {
            generation: notification.generation,
            document_id: notification.document_id,
            path: notification.path,
            version: notification.version,
            diagnostics,
        })
    }

    pub(crate) fn active_live_diagnostic_spans(&self) -> Vec<crate::editor::LiveDiagnosticSpan> {
        let document_id = self.workspace.active_document_id();
        let buffer = self.workspace.active_buffer();
        self.live_diagnostics
            .get(document_id)
            .filter(|document| {
                self.live_diagnostic_generation == Some(document.generation)
                    && document.path == *self.workspace.active_path()
                    && document.version == buffer.version()
            })
            .map(|document| document.diagnostics.clone())
            .unwrap_or_default()
    }

    pub(crate) fn live_diagnostic_header_for_caret(&self) -> Option<String> {
        let line = self.workspace.active_buffer().cursor().line;
        let diagnostic = self
            .active_live_diagnostic_spans()
            .into_iter()
            .filter(|diagnostic| (diagnostic.start_line..=diagnostic.end_line).contains(&line))
            .min_by_key(|diagnostic| match diagnostic.kind {
                DiagnosticMarkerKind::Error => 0,
                DiagnosticMarkerKind::Warning => 1,
                DiagnosticMarkerKind::Other => 2,
            })?;
        Some(crate::display::label_fmt(
            format_args!("{}", diagnostic.message),
            4096,
        ))
    }

    pub(super) fn clear_live_diagnostics(&mut self) -> bool {
        let had_generation = self.live_diagnostic_generation.take().is_some();
        let had_diagnostics = !self.live_diagnostics.is_empty();
        let had_selection = self.selected_live_diagnostic.take().is_some();
        self.live_diagnostics.clear();
        had_generation || had_diagnostics || had_selection
    }

    pub fn diagnostic_display_rows(&self, maximum: usize) -> Vec<OutputRow> {
        let Some(result) = self.command.diagnostics.as_ref() else {
            return Vec::new();
        };
        let maximum = maximum.min(crate::display::MAX_LINES);
        if maximum == 0 {
            return Vec::new();
        }
        let evidence = if result.issues.is_empty() {
            "complete".to_owned()
        } else {
            crate::display::label_fmt(format_args!("incomplete: {:?}", result.issues), 1024)
        };
        let freshness = if self.diagnostic_identity_is_current(&result.identity.workspace) {
            ""
        } else {
            "; stale for current workspace"
        };
        let outcome = match result.outcome {
            DiagnosticOutcome::Success => "success",
            DiagnosticOutcome::CompilerErrors => "compiler errors",
            DiagnosticOutcome::NonzeroExit => "nonzero exit (not compiler errors)",
            DiagnosticOutcome::ExecutionFailed => "execution failed",
            DiagnosticOutcome::Unknown => "unknown outcome",
        };
        let visible_diagnostics = result
            .diagnostics
            .iter()
            .enumerate()
            .filter(|(_, diagnostic)| diagnostic_is_visible(diagnostic))
            .collect::<Vec<_>>();
        let selected_visible = self.command.selected_diagnostic.and_then(|selected| {
            visible_diagnostics
                .iter()
                .position(|(index, _)| *index == selected)
        });
        let mut rows = vec![OutputRow::plain(crate::display::label_fmt(
            format_args!(
                "Diagnostics {}/{} | {outcome}; {evidence}{freshness} | Alt-Up/Down navigate",
                selected_visible.map(|index| index + 1).unwrap_or(0),
                visible_diagnostics.len()
            ),
            4096,
        ))];
        let remaining = maximum.saturating_sub(1);
        let mixed = !visible_diagnostics.is_empty() && !result.output.is_empty();
        let reserved_output = usize::from(mixed && maximum >= 3);
        let diagnostic_rows = if visible_diagnostics.is_empty() {
            0
        } else {
            remaining
                .saturating_sub(reserved_output)
                .max(usize::from(remaining > 0))
        };
        if diagnostic_rows > 0 {
            let selected = selected_visible.unwrap_or(0);
            let start = selected.saturating_sub(diagnostic_rows.saturating_sub(1));
            for &(index, diagnostic) in visible_diagnostics.iter().skip(start).take(diagnostic_rows)
            {
                let is_selected = self.command.selected_diagnostic == Some(index);
                let marker = if is_selected { '>' } else { ' ' };
                let code = diagnostic
                    .code
                    .as_deref()
                    .map(|code| format!("[{code}]"))
                    .unwrap_or_default();
                let target = diagnostic
                    .primary_span()
                    .map_or("no managed span", |span| span.file_name.as_str());
                let location = diagnostic.primary_span().map_or(String::new(), |span| {
                    format!(":{}:{}", span.line_start, span.column_start)
                });
                rows.push(
                    OutputRow::diagnostic(
                        crate::display::label_fmt(
                            format_args!(
                                "{marker} {}{code} {target}{location}: {}",
                                diagnostic.level, diagnostic.message
                            ),
                            4096,
                        ),
                        index,
                    )
                    .with_selected(is_selected),
                );
            }
        }
        let output_rows = maximum.saturating_sub(rows.len());
        for line in result.output.iter().take(output_rows) {
            let bytes = line.original_bytes().unwrap_or_default();
            let rendered = crate::display::output(
                &bytes,
                crate::display::Limits {
                    lines: 1,
                    ..crate::display::Limits::default()
                },
            );
            rows.push(OutputRow::captured(
                crate::display::label_fmt(format_args!("{}", rendered.text), 4096),
                line.stream,
            ));
        }
        rows
    }

    pub fn active_diagnostic_markers(&self) -> Vec<DiagnosticLineMarker> {
        let active = self.workspace.active_path().clone();
        let mut live_markers = BTreeMap::new();
        let selected_live = self.selected_live_diagnostic.as_ref();
        if let Some(document) = self
            .live_diagnostics
            .get(self.workspace.active_document_id())
            .filter(|document| {
                self.live_diagnostic_generation == Some(document.generation)
                    && document.path == active
                    && document.version == self.workspace.active_buffer().version()
            })
        {
            for (index, diagnostic) in document.diagnostics.iter().enumerate() {
                for line in diagnostic.start_line..=diagnostic.end_line {
                    if live_markers.len() == crate::diagnostics::MAX_DIAGNOSTICS {
                        break;
                    }
                    let selected = selected_live.is_some_and(|(document_id, selected_index)| {
                        document_id == &document.document_id && *selected_index == index
                    });
                    let marker = DiagnosticLineMarker::live(line, diagnostic.kind, selected);
                    match live_markers.entry(line) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(marker);
                        }
                        std::collections::btree_map::Entry::Occupied(mut entry)
                            if marker_priority(&marker) < marker_priority(entry.get()) =>
                        {
                            entry.insert(marker);
                        }
                        _ => {}
                    }
                }
            }
        }
        let Some(result) = self.command.diagnostics.as_ref() else {
            return live_markers.into_values().collect();
        };
        if !self.diagnostic_identity_is_current(&result.identity.workspace) {
            return live_markers.into_values().collect();
        }
        let candidates = result
            .diagnostics
            .iter()
            .enumerate()
            .filter_map(|(index, diagnostic)| {
                if !diagnostic_is_visible(diagnostic) {
                    return None;
                }
                let span = diagnostic.primary_span()?;
                let path = workspace_path_for_diagnostic(self.workspace.root(), &span.file_name)?;
                if path != active {
                    return None;
                }
                let kind = match diagnostic.level.as_str() {
                    "error" | "failure-note" | "error: internal compiler error" => {
                        DiagnosticMarkerKind::Error
                    }
                    "warning" => DiagnosticMarkerKind::Warning,
                    _ => DiagnosticMarkerKind::Other,
                };
                Some((index, span, kind))
            })
            .take(crate::diagnostics::MAX_DIAGNOSTICS)
            .collect::<Vec<_>>();
        let spans = candidates
            .iter()
            .map(|(_, span, _)| *span)
            .collect::<Vec<_>>();
        let valid = self.workspace.diagnostic_spans_are_valid(&active, &spans);
        let mut compiler_markers = BTreeMap::new();
        for ((index, span, kind), valid) in candidates.into_iter().zip(valid) {
            if valid && let Ok(line) = usize::try_from(span.line_start.saturating_sub(1)) {
                let marker = DiagnosticLineMarker::compiler(
                    line,
                    kind,
                    self.command.selected_diagnostic == Some(index),
                    index,
                );
                match compiler_markers.entry(line) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(marker);
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry)
                        if marker_priority(&marker) < marker_priority(entry.get()) =>
                    {
                        entry.insert(marker);
                    }
                    _ => {}
                }
            }
        }
        let mut markers = compiler_markers.into_values().collect::<Vec<_>>();
        markers.extend(
            live_markers
                .into_values()
                .take(crate::diagnostics::MAX_DIAGNOSTICS.saturating_sub(markers.len())),
        );
        markers.sort_by_key(|marker| (marker.line, marker.live));
        markers
    }

    /// Move through the latest diagnostic list. The first forward selection
    /// chooses item zero; subsequent selections wrap. A selected valid target
    /// records normal file-focus/selection provenance before changing the UI.
    pub fn select_diagnostic(&mut self, delta: isize) -> Result<DiagnosticNavigation> {
        self.require_command_idle()?;
        let diagnostics = self.navigable_diagnostics();
        if diagnostics.is_empty() {
            return Ok(DiagnosticNavigation::NoDiagnostics);
        }
        let current = diagnostics.iter().position(|diagnostic| match diagnostic {
            NavigableDiagnostic::Cargo { index, .. } => {
                self.selected_live_diagnostic.is_none()
                    && self.command.selected_diagnostic == Some(*index)
            }
            NavigableDiagnostic::Live {
                document_id, index, ..
            } => self.selected_live_diagnostic == Some((document_id.clone(), *index)),
        });
        let length = diagnostics.len();
        let index = match current {
            None if delta < 0 => length - 1,
            None => 0,
            Some(index) if delta < 0 => (index + length - 1) % length,
            Some(index) => (index + 1) % length,
        };
        match diagnostics[index].clone() {
            NavigableDiagnostic::Cargo { index, .. } => self.select_diagnostic_index_inner(index),
            NavigableDiagnostic::Live {
                document_id, index, ..
            } => self.select_live_diagnostic(document_id, index),
        }
    }

    /// Select one rendered diagnostic by its stable source index.
    pub fn select_diagnostic_index(&mut self, index: usize) -> Result<DiagnosticNavigation> {
        self.require_command_idle()?;
        let Some(result) = self.command.diagnostics.as_ref() else {
            return Ok(DiagnosticNavigation::NoDiagnostics);
        };
        if index >= result.diagnostics.len() {
            return Ok(DiagnosticNavigation::NoDiagnostics);
        }
        if !diagnostic_is_visible(&result.diagnostics[index]) {
            return Ok(DiagnosticNavigation::NoDiagnostics);
        }
        self.select_diagnostic_index_inner(index)
    }

    /// Select a compiler diagnostic for output emphasis without navigating its
    /// source span. Source-line clicks use this after recording their ordinary
    /// caret movement.
    pub(crate) fn select_diagnostic_for_output(&mut self, index: usize) -> bool {
        let Some(result) = self.command.diagnostics.as_ref() else {
            return false;
        };
        if !self.diagnostic_identity_is_current(&result.identity.workspace)
            || result
                .diagnostics
                .get(index)
                .is_none_or(|diagnostic| !diagnostic_is_visible(diagnostic))
        {
            return false;
        }
        self.selected_live_diagnostic = None;
        self.command.selected_diagnostic = Some(index);
        true
    }

    fn select_diagnostic_index_inner(&mut self, index: usize) -> Result<DiagnosticNavigation> {
        self.selected_live_diagnostic = None;
        self.command.selected_diagnostic = Some(index);

        let (identity, span) = {
            let result = self.command.diagnostics.as_ref().expect("checked above");
            (
                result.identity.workspace.clone(),
                result.diagnostics[index].primary_span().cloned(),
            )
        };
        if !self.diagnostic_identity_is_current(&identity) {
            return Ok(DiagnosticNavigation::Stale);
        }

        let Some(span) = span else {
            return Ok(DiagnosticNavigation::MissingTarget);
        };
        let Some(path) = workspace_path_for_diagnostic(self.workspace.root(), &span.file_name)
        else {
            return Ok(DiagnosticNavigation::OutsideWorkspace);
        };
        let Some(file) = self
            .workspace
            .file_tree()
            .iter()
            .find(|file| file.path() == &path)
        else {
            return Ok(DiagnosticNavigation::UnmanagedTarget);
        };
        if !file.is_editable() {
            return Ok(DiagnosticNavigation::UnmanagedTarget);
        }
        match self.workspace.navigate_to_diagnostic_span(&path, &span) {
            Ok(()) => Ok(DiagnosticNavigation::Navigated {
                path,
                start_byte: span.byte_start,
                end_byte: span.byte_end,
            }),
            Err(crate::tui::WorkspaceError::InvalidDiagnosticSpan) => {
                Ok(DiagnosticNavigation::InvalidCoordinates)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn select_live_diagnostic(
        &mut self,
        document_id: DocumentId,
        index: usize,
    ) -> Result<DiagnosticNavigation> {
        self.command.selected_diagnostic = None;
        self.selected_live_diagnostic = Some((document_id.clone(), index));
        let Some(document) = self.live_diagnostics.get(&document_id) else {
            return Ok(DiagnosticNavigation::Stale);
        };
        if self.live_diagnostic_generation != Some(document.generation) {
            return Ok(DiagnosticNavigation::Stale);
        }
        let Some(diagnostic) = document.diagnostics.get(index) else {
            return Ok(DiagnosticNavigation::Stale);
        };
        let path = document.path.clone();
        let (start_byte, end_byte) = (diagnostic.start_byte, diagnostic.end_byte);
        match self
            .workspace
            .navigate_to_live_diagnostic(&path, start_byte, end_byte)
        {
            Ok(()) => Ok(DiagnosticNavigation::Navigated {
                path,
                start_byte,
                end_byte,
            }),
            Err(crate::tui::WorkspaceError::InvalidDiagnosticSpan) => {
                Ok(DiagnosticNavigation::InvalidCoordinates)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn navigable_diagnostics(&self) -> Vec<NavigableDiagnostic> {
        let mut diagnostics = Vec::new();
        if let Some(result) = self
            .command
            .diagnostics
            .as_ref()
            .filter(|result| self.diagnostic_identity_is_current(&result.identity.workspace))
        {
            diagnostics.extend(
                result
                    .diagnostics
                    .iter()
                    .enumerate()
                    .filter(|(_, diagnostic)| diagnostic_is_visible(diagnostic))
                    .map(|(index, diagnostic)| {
                        let span = diagnostic.primary_span();
                        NavigableDiagnostic::Cargo {
                            index,
                            path: span.and_then(|span| {
                                workspace_path_for_diagnostic(
                                    self.workspace.root(),
                                    &span.file_name,
                                )
                            }),
                            start_byte: span.map_or(u64::MAX, |span| span.byte_start),
                        }
                    }),
            );
        }
        for document in self.live_diagnostics.values().filter(|document| {
            self.live_diagnostic_generation == Some(document.generation)
                && self
                    .workspace
                    .buffer_by_document_id(&document.document_id)
                    .is_some_and(|buffer| buffer.version() == document.version)
        }) {
            diagnostics.extend(document.diagnostics.iter().enumerate().map(
                |(index, diagnostic)| NavigableDiagnostic::Live {
                    document_id: document.document_id.clone(),
                    index,
                    path: document.path.clone(),
                    start_byte: diagnostic.start_byte,
                },
            ));
        }
        let document_rank = |path: Option<&WorkspacePath>| {
            path.and_then(|path| {
                self.workspace
                    .file_tree()
                    .iter()
                    .position(|file| file.path() == path && file.is_editable())
            })
            .unwrap_or(usize::MAX)
        };
        diagnostics.sort_by_key(|diagnostic| match diagnostic {
            NavigableDiagnostic::Cargo {
                index,
                path,
                start_byte,
            } => (document_rank(path.as_ref()), *start_byte, 0, *index),
            NavigableDiagnostic::Live {
                index,
                path,
                start_byte,
                ..
            } => (document_rank(Some(path)), *start_byte, 1, *index),
        });
        diagnostics
    }

    fn diagnostic_identity_is_current(&self, identity: &CommandTreeLink) -> bool {
        self.effects
            .0
            .borrow()
            .replay
            .as_ref()
            .is_some_and(|replay| {
                replay.current_workspace_hash() == identity.workspace_hash
                    && replay.workspace_version() == identity.workspace_version
            })
    }
}

fn diagnostic_is_visible(diagnostic: &crate::diagnostics::CargoDiagnostic) -> bool {
    if diagnostic.level == "failure-note" {
        return false;
    }
    if diagnostic.primary_span().is_some() || !matches!(diagnostic.level.as_str(), "note" | "help")
    {
        return true;
    }
    let message = diagnostic.message.trim_start();
    !message.starts_with("For more information about this error")
        && !message.starts_with("aborting due to")
        && !message.starts_with("Some errors have detailed explanations")
}

fn marker_priority(marker: &DiagnosticLineMarker) -> u8 {
    match marker.kind {
        DiagnosticMarkerKind::Error => 0,
        DiagnosticMarkerKind::Warning => 1,
        DiagnosticMarkerKind::Other => 2,
    }
}

fn workspace_path_for_diagnostic(root: &Path, file_name: &str) -> Option<WorkspacePath> {
    if file_name.is_empty() {
        return None;
    }
    let path = Path::new(file_name);
    let relative = if path.is_absolute() {
        path.strip_prefix(root).ok()?
    } else {
        path
    };
    WorkspacePath::new(relative.to_str()?).ok()
}
