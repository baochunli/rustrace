//! Bounded derived Cargo diagnostics. Controlled-command bytes remain authoritative.
use rustrace_model::{
    CaptureCompleteness, CommandCapture, CommandId, CommandOutcome, CommandTool, CommandTreeLink,
    ControlledAction, ControlledCommandFinished, ControlledCommandOutput, ControlledCommandStarted,
    OutputStream, preflight_json,
};
use serde::Serialize;
use serde_json::{Map, Value};

pub const MAX_CARGO_MESSAGE_BYTES: usize = 256 * 1024;
pub const MAX_DIAGNOSTICS: usize = 256;
pub const MAX_ARTIFACTS: usize = 256;
pub const MAX_DIAGNOSTIC_SPANS: usize = 1024;
pub const MAX_DIAGNOSTIC_CHILDREN: usize = 256;
pub const MAX_OUTPUT_LINES: usize = 128;
pub const MAX_OUTPUT_LINE_BYTES: usize = 4 * 1024;
const MAX_FIELD_BYTES: usize = 4 * 1024;
const MAX_RENDERED_BYTES: usize = 16 * 1024;
const MAX_LIST_ITEMS: usize = 256;
const MAX_CHILD_DEPTH: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticEvidenceIssue {
    Missing,
    Malformed,
    Oversized,
    Unexpected,
    InvalidUtf8,
    Truncated,
    ReadFailed,
    Unavailable,
    ExecutionFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticOutcome {
    Success,
    CompilerErrors,
    NonzeroExit,
    ExecutionFailed,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiagnosticNavigation {
    Navigated {
        path: rustrace_model::WorkspacePath,
        start_byte: u64,
        end_byte: u64,
    },
    NoDiagnostics,
    MissingTarget,
    UnmanagedTarget,
    OutsideWorkspace,
    InvalidCoordinates,
    Stale,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticIdentity {
    pub command_id: CommandId,
    pub action: ControlledAction,
    pub argv: Vec<String>,
    pub selected_toolchain: String,
    pub tools: Vec<CommandTool>,
    pub workspace: CommandTreeLink,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandDiagnostics {
    pub version: u32,
    pub identity: DiagnosticIdentity,
    pub stdout: CommandCapture,
    pub stderr: CommandCapture,
    pub issues: Vec<DiagnosticEvidenceIssue>,
    pub outcome: DiagnosticOutcome,
    pub diagnostics: Vec<CargoDiagnostic>,
    pub artifacts: Vec<CargoArtifact>,
    pub structured_messages: u64,
    pub output: Vec<DiagnosticOutputLine>,
    pub omitted_output_lines: u64,
}

impl CommandDiagnostics {
    /// Complete known-empty means the structured stream was fully understood.
    /// It is independent of the process exit code and is never inferred from it.
    pub fn known_empty(&self) -> bool {
        self.issues.is_empty() && self.diagnostics.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CargoTarget {
    pub name: String,
    pub kind: Vec<String>,
    pub crate_types: Vec<String>,
    pub src_path: String,
    pub edition: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CargoDiagnostic {
    pub package_id: String,
    pub manifest_path: String,
    pub target: CargoTarget,
    pub code: Option<String>,
    pub level: String,
    pub message: String,
    pub rendered: Option<String>,
    pub spans: Vec<DiagnosticSpan>,
    pub children: Vec<DiagnosticChild>,
}

impl CargoDiagnostic {
    pub fn is_error(&self) -> bool {
        matches!(
            self.level.as_str(),
            "error" | "failure-note" | "error: internal compiler error"
        )
    }

    pub fn primary_span(&self) -> Option<&DiagnosticSpan> {
        self.spans
            .iter()
            .find(|span| span.is_primary)
            .or_else(|| self.spans.first())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticChild {
    pub level: String,
    pub message: String,
    pub spans: Vec<DiagnosticSpan>,
    pub children: Vec<DiagnosticChild>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticSpan {
    pub file_name: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub line_start: u64,
    pub line_end: u64,
    pub column_start: u64,
    pub column_end: u64,
    pub is_primary: bool,
    pub label: Option<String>,
    pub suggested_replacement: Option<String>,
    pub suggestion_applicability: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CargoArtifact {
    pub package_id: String,
    pub manifest_path: String,
    pub target: CargoTarget,
    pub profile: Option<CargoProfile>,
    pub features: Vec<String>,
    pub filenames: Vec<String>,
    pub executable: Option<String>,
    pub fresh: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CargoProfile {
    pub opt_level: String,
    pub debuginfo: String,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub test: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticOutputLine {
    pub stream: OutputStream,
    /// A bounded lowercase-hex display projection. Exact bytes remain in the
    /// controlled-command capture and journal chunks.
    pub bytes_hex: String,
    pub truncated: bool,
}

impl DiagnosticOutputLine {
    pub fn original_bytes(&self) -> Result<Vec<u8>, &'static str> {
        if self.bytes_hex.len().is_multiple_of(2)
            && self
                .bytes_hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(self
                .bytes_hex
                .as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| nibble(pair[0]) * 16 + nibble(pair[1]))
                .collect())
        } else {
            Err("invalid output preview hex")
        }
    }
}

struct ParseContext {
    issues: Vec<DiagnosticEvidenceIssue>,
    span_count: usize,
    child_count: usize,
    record_rejected: bool,
}

impl ParseContext {
    fn issue(&mut self, issue: DiagnosticEvidenceIssue) {
        self.record_rejected = true;
        if !self.issues.contains(&issue) {
            self.issues.push(issue);
        }
    }

    fn begin_record(&mut self) {
        self.record_rejected = false;
    }

    fn bounded(&mut self, value: &str, maximum: usize) -> String {
        if value.len() <= maximum {
            return value.to_owned();
        }
        self.issue(DiagnosticEvidenceIssue::Oversized);
        value[..value.floor_char_boundary(maximum)].to_owned()
    }
}

/// Derive bounded student-facing diagnostics from the exact accepted command
/// evidence. This function never changes or replaces the supplied bytes.
pub fn derive_command_diagnostics(
    start: &ControlledCommandStarted,
    chunks: &[ControlledCommandOutput],
    finish: &ControlledCommandFinished,
) -> CommandDiagnostics {
    let mut context = ParseContext {
        issues: Vec::new(),
        span_count: 0,
        child_count: 0,
        record_rejected: false,
    };
    if start.command_id != finish.command_id
        || start.before.workspace_hash != finish.after.workspace_hash
        || start.before.workspace_version != finish.after.workspace_version
    {
        context.issue(DiagnosticEvidenceIssue::Missing);
    }
    capture_issue(finish.stdout.completeness, &mut context);
    capture_issue(finish.stderr.completeness, &mut context);
    let stdout = collect_stream(
        start,
        chunks,
        OutputStream::Stdout,
        &finish.stdout,
        &mut context,
    );
    let stderr = collect_stream(
        start,
        chunks,
        OutputStream::Stderr,
        &finish.stderr,
        &mut context,
    );

    let mut diagnostics = Vec::new();
    let mut artifacts = Vec::new();
    let mut output = Vec::new();
    let mut omitted_output_lines = 0;
    let mut structured_messages = 0_u64;
    let mut build_finished = None;
    for line in byte_lines(&stdout) {
        if build_finished.is_some() {
            if !matches!(start.action, ControlledAction::Test | ControlledAction::Run) {
                context.issue(DiagnosticEvidenceIssue::Unexpected);
            }
            retain_output(
                &mut output,
                &mut omitted_output_lines,
                OutputStream::Stdout,
                line,
            );
            continue;
        }
        let trimmed = trim_ascii_start(line);
        if !matches!(trimmed.first(), Some(b'{' | b'[')) {
            if std::str::from_utf8(line).is_err() {
                context.issue(DiagnosticEvidenceIssue::InvalidUtf8);
            } else {
                context.issue(DiagnosticEvidenceIssue::Unexpected);
            }
            retain_output(
                &mut output,
                &mut omitted_output_lines,
                OutputStream::Stdout,
                line,
            );
            continue;
        }
        if line.len() > MAX_CARGO_MESSAGE_BYTES {
            context.issue(DiagnosticEvidenceIssue::Oversized);
            retain_output(
                &mut output,
                &mut omitted_output_lines,
                OutputStream::Stdout,
                line,
            );
            continue;
        }
        if std::str::from_utf8(line).is_err() {
            context.issue(DiagnosticEvidenceIssue::InvalidUtf8);
            retain_output(
                &mut output,
                &mut omitted_output_lines,
                OutputStream::Stdout,
                line,
            );
            continue;
        }
        if preflight_json(line).is_err() {
            context.issue(DiagnosticEvidenceIssue::Malformed);
            retain_output(
                &mut output,
                &mut omitted_output_lines,
                OutputStream::Stdout,
                line,
            );
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            context.issue(DiagnosticEvidenceIssue::Malformed);
            retain_output(
                &mut output,
                &mut omitted_output_lines,
                OutputStream::Stdout,
                line,
            );
            continue;
        };
        let Some(object) = value.as_object() else {
            context.issue(DiagnosticEvidenceIssue::Unexpected);
            retain_output(
                &mut output,
                &mut omitted_output_lines,
                OutputStream::Stdout,
                line,
            );
            continue;
        };
        let Some(reason) = object.get("reason").and_then(Value::as_str) else {
            context.issue(DiagnosticEvidenceIssue::Malformed);
            retain_output(
                &mut output,
                &mut omitted_output_lines,
                OutputStream::Stdout,
                line,
            );
            continue;
        };
        structured_messages = structured_messages.saturating_add(1);
        context.begin_record();
        match reason {
            "compiler-message" if diagnostics.len() < MAX_DIAGNOSTICS => {
                if let Some(diagnostic) = parse_diagnostic(object, &mut context) {
                    diagnostics.push(diagnostic);
                } else {
                    context.issue(DiagnosticEvidenceIssue::Malformed);
                }
            }
            "compiler-message" => context.issue(DiagnosticEvidenceIssue::Oversized),
            "compiler-artifact" if artifacts.len() < MAX_ARTIFACTS => {
                if let Some(artifact) = parse_artifact(object, &mut context) {
                    artifacts.push(artifact);
                } else {
                    context.issue(DiagnosticEvidenceIssue::Malformed);
                }
            }
            "compiler-artifact" => context.issue(DiagnosticEvidenceIssue::Oversized),
            "build-script-executed" => {
                if parse_build_script_executed(object, &mut context).is_none() {
                    context.issue(DiagnosticEvidenceIssue::Malformed);
                }
            }
            "build-finished" => match object.get("success").and_then(Value::as_bool) {
                Some(success) if build_finished.is_none() => build_finished = Some(success),
                Some(_) => context.issue(DiagnosticEvidenceIssue::Unexpected),
                None => context.issue(DiagnosticEvidenceIssue::Malformed),
            },
            _ => context.issue(DiagnosticEvidenceIssue::Unexpected),
        }
        if context.record_rejected {
            retain_output(
                &mut output,
                &mut omitted_output_lines,
                OutputStream::Stdout,
                line,
            );
        }
    }
    if build_finished.is_none() {
        context.issue(DiagnosticEvidenceIssue::Missing);
    }
    for line in byte_lines(&stderr) {
        if std::str::from_utf8(line).is_err() {
            context.issue(DiagnosticEvidenceIssue::InvalidUtf8);
        }
        retain_output(
            &mut output,
            &mut omitted_output_lines,
            OutputStream::Stderr,
            line,
        );
    }

    let execution_failed = !matches!(finish.outcome, CommandOutcome::Exited { .. });
    if execution_failed {
        context.issue(DiagnosticEvidenceIssue::ExecutionFailed);
    }
    let error_diagnostic = diagnostics.iter().any(CargoDiagnostic::is_error);
    if let CommandOutcome::Exited { code } = finish.outcome {
        let runtime_action = matches!(start.action, ControlledAction::Test | ControlledAction::Run);
        if (build_finished == Some(false) && code == 0)
            || (build_finished == Some(true) && error_diagnostic)
            || (build_finished == Some(true) && code != 0 && !runtime_action)
        {
            context.issue(DiagnosticEvidenceIssue::Unexpected);
        }
    }
    let outcome = if execution_failed {
        DiagnosticOutcome::ExecutionFailed
    } else if !context.issues.is_empty() {
        DiagnosticOutcome::Unknown
    } else if error_diagnostic {
        DiagnosticOutcome::CompilerErrors
    } else if matches!(finish.outcome, CommandOutcome::Exited { code } if code != 0) {
        DiagnosticOutcome::NonzeroExit
    } else if build_finished == Some(true) {
        DiagnosticOutcome::Success
    } else {
        DiagnosticOutcome::Unknown
    };

    CommandDiagnostics {
        version: 1,
        identity: DiagnosticIdentity {
            command_id: start.command_id.clone(),
            action: start.action,
            argv: start.argv.clone(),
            selected_toolchain: start.selected_toolchain.clone(),
            tools: start.tools.clone(),
            workspace: start.before.clone(),
        },
        stdout: finish.stdout.clone(),
        stderr: finish.stderr.clone(),
        issues: context.issues,
        outcome,
        diagnostics,
        artifacts,
        structured_messages,
        output,
        omitted_output_lines,
    }
}

fn capture_issue(completeness: CaptureCompleteness, context: &mut ParseContext) {
    let issue = match completeness {
        CaptureCompleteness::Complete => return,
        CaptureCompleteness::Truncated => DiagnosticEvidenceIssue::Truncated,
        CaptureCompleteness::ReadFailed => DiagnosticEvidenceIssue::ReadFailed,
        CaptureCompleteness::Unavailable => DiagnosticEvidenceIssue::Unavailable,
    };
    context.issue(issue);
}

fn collect_stream(
    start: &ControlledCommandStarted,
    chunks: &[ControlledCommandOutput],
    stream: OutputStream,
    capture: &CommandCapture,
    context: &mut ParseContext,
) -> Vec<u8> {
    let maximum = usize::try_from(start.output_limit)
        .unwrap_or(usize::MAX)
        .min(rustrace_model::MAX_COMMAND_OUTPUT_BYTES as usize);
    let mut bytes = Vec::with_capacity(
        usize::try_from(capture.bytes)
            .unwrap_or(maximum)
            .min(maximum),
    );
    let mut offset = 0_u64;
    for chunk in chunks.iter().filter(|chunk| chunk.stream == stream) {
        if chunk.command_id != start.command_id || chunk.offset != offset {
            context.issue(DiagnosticEvidenceIssue::Missing);
            continue;
        }
        let Ok(original) = chunk.original_bytes() else {
            context.issue(DiagnosticEvidenceIssue::Malformed);
            continue;
        };
        if bytes.len().saturating_add(original.len()) > maximum {
            context.issue(DiagnosticEvidenceIssue::Oversized);
            break;
        }
        bytes.extend_from_slice(&original);
        offset = offset.saturating_add(original.len() as u64);
    }
    if offset != capture.bytes {
        context.issue(DiagnosticEvidenceIssue::Missing);
    }
    bytes
}

fn byte_lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\n").unwrap_or(line))
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
}

fn trim_ascii_start(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    bytes
}

fn retain_output(
    output: &mut Vec<DiagnosticOutputLine>,
    omitted: &mut u64,
    stream: OutputStream,
    bytes: &[u8],
) {
    if output.len() == MAX_OUTPUT_LINES {
        *omitted = omitted.saturating_add(1);
        return;
    }
    let prefix = &bytes[..bytes.len().min(MAX_OUTPUT_LINE_BYTES)];
    output.push(DiagnosticOutputLine {
        stream,
        bytes_hex: encode_hex(prefix),
        truncated: prefix.len() != bytes.len(),
    });
}

fn parse_diagnostic(
    object: &Map<String, Value>,
    context: &mut ParseContext,
) -> Option<CargoDiagnostic> {
    let package_id = required_string(object, "package_id", MAX_FIELD_BYTES, context)?;
    let manifest_path = required_string(object, "manifest_path", MAX_FIELD_BYTES, context)?;
    let target = parse_target(object.get("target"), context)?;
    let message = object
        .get("message")
        .and_then(Value::as_object)
        .or_else(|| {
            context.issue(DiagnosticEvidenceIssue::Malformed);
            None
        })?;
    let level = parse_diagnostic_level(message, context)?;
    let text = required_string(message, "message", MAX_FIELD_BYTES, context)?;
    let code = parse_diagnostic_code(message.get("code"), context)?;
    let rendered = required_nullable_string(message, "rendered", MAX_RENDERED_BYTES, context)?;
    let spans = parse_spans(message.get("spans"), context)?;
    let children = parse_children(message.get("children"), 0, context)?;
    Some(CargoDiagnostic {
        package_id,
        manifest_path,
        target,
        code,
        level,
        message: text,
        rendered,
        spans,
        children,
    })
}

fn parse_children(
    value: Option<&Value>,
    depth: usize,
    context: &mut ParseContext,
) -> Option<Vec<DiagnosticChild>> {
    let values = required_array(value, context)?;
    if values.len() > MAX_LIST_ITEMS || depth >= MAX_CHILD_DEPTH {
        context.issue(DiagnosticEvidenceIssue::Oversized);
    }
    let mut children = Vec::new();
    for value in values.iter().take(MAX_LIST_ITEMS) {
        if context.child_count == MAX_DIAGNOSTIC_CHILDREN {
            context.issue(DiagnosticEvidenceIssue::Oversized);
            break;
        }
        context.child_count += 1;
        let Some(object) = value.as_object() else {
            context.issue(DiagnosticEvidenceIssue::Malformed);
            continue;
        };
        let Some(level) = parse_diagnostic_level(object, context) else {
            continue;
        };
        let Some(message) = required_string(object, "message", MAX_FIELD_BYTES, context) else {
            continue;
        };
        if parse_diagnostic_code(object.get("code"), context).is_none()
            || required_nullable_string(object, "rendered", MAX_RENDERED_BYTES, context).is_none()
        {
            continue;
        }
        let Some(spans) = parse_spans(object.get("spans"), context) else {
            continue;
        };
        let nested = if depth + 1 < MAX_CHILD_DEPTH {
            parse_children(object.get("children"), depth + 1, context).unwrap_or_default()
        } else {
            if required_array(object.get("children"), context)
                .is_some_and(|children| !children.is_empty())
            {
                context.issue(DiagnosticEvidenceIssue::Oversized);
            }
            Vec::new()
        };
        children.push(DiagnosticChild {
            level,
            message,
            spans,
            children: nested,
        });
    }
    Some(children)
}

fn parse_spans(value: Option<&Value>, context: &mut ParseContext) -> Option<Vec<DiagnosticSpan>> {
    let values = required_array(value, context)?;
    let mut spans = Vec::new();
    for value in values.iter().take(MAX_LIST_ITEMS) {
        if context.span_count == MAX_DIAGNOSTIC_SPANS {
            context.issue(DiagnosticEvidenceIssue::Oversized);
            break;
        }
        let Some(object) = value.as_object() else {
            context.issue(DiagnosticEvidenceIssue::Malformed);
            continue;
        };
        let Some(span) = parse_span(object, context) else {
            continue;
        };
        context.span_count += 1;
        spans.push(span);
    }
    if values.len() > MAX_LIST_ITEMS {
        context.issue(DiagnosticEvidenceIssue::Oversized);
    }
    Some(spans)
}

fn parse_span(object: &Map<String, Value>, context: &mut ParseContext) -> Option<DiagnosticSpan> {
    let span = DiagnosticSpan {
        file_name: required_string(object, "file_name", MAX_FIELD_BYTES, context)?,
        byte_start: required_u64(object, "byte_start", context)?,
        byte_end: required_u64(object, "byte_end", context)?,
        line_start: required_u64(object, "line_start", context)?,
        line_end: required_u64(object, "line_end", context)?,
        column_start: required_u64(object, "column_start", context)?,
        column_end: required_u64(object, "column_end", context)?,
        is_primary: required_bool(object, "is_primary", context)?,
        label: required_nullable_string(object, "label", MAX_FIELD_BYTES, context)?,
        suggested_replacement: required_nullable_string(
            object,
            "suggested_replacement",
            MAX_RENDERED_BYTES,
            context,
        )?,
        suggestion_applicability: required_nullable_string(
            object,
            "suggestion_applicability",
            128,
            context,
        )?,
    };
    if span.byte_start > span.byte_end
        || span.line_start == 0
        || span.line_end == 0
        || span.column_start == 0
        || span.column_end == 0
        || span.line_start > span.line_end
    {
        context.issue(DiagnosticEvidenceIssue::Malformed);
        return None;
    }
    Some(span)
}

fn parse_artifact(
    object: &Map<String, Value>,
    context: &mut ParseContext,
) -> Option<CargoArtifact> {
    Some(CargoArtifact {
        package_id: required_string(object, "package_id", MAX_FIELD_BYTES, context)?,
        manifest_path: required_string(object, "manifest_path", MAX_FIELD_BYTES, context)?,
        target: parse_target(object.get("target"), context)?,
        profile: Some(parse_profile(object.get("profile"), context)?),
        features: parse_string_list(object.get("features"), MAX_FIELD_BYTES, context)?,
        filenames: parse_string_list(object.get("filenames"), MAX_FIELD_BYTES, context)?,
        executable: required_nullable_string(object, "executable", MAX_FIELD_BYTES, context)?,
        fresh: required_bool(object, "fresh", context)?,
    })
}

fn parse_profile(value: Option<&Value>, context: &mut ParseContext) -> Option<CargoProfile> {
    let object = value.and_then(Value::as_object).or_else(|| {
        context.issue(DiagnosticEvidenceIssue::Malformed);
        None
    })?;
    let debuginfo_value = object.get("debuginfo").or_else(|| {
        context.issue(DiagnosticEvidenceIssue::Malformed);
        None
    })?;
    if !matches!(debuginfo_value, Value::Null | Value::String(_))
        && debuginfo_value.as_u64().is_none()
    {
        context.issue(DiagnosticEvidenceIssue::Malformed);
        return None;
    }
    let debuginfo = serde_json::to_string(debuginfo_value)
        .ok()
        .map(|value| context.bounded(&value, 64))?;
    Some(CargoProfile {
        opt_level: required_string(object, "opt_level", 64, context)?,
        debuginfo,
        debug_assertions: required_bool(object, "debug_assertions", context)?,
        overflow_checks: required_bool(object, "overflow_checks", context)?,
        test: required_bool(object, "test", context)?,
    })
}

fn parse_target(value: Option<&Value>, context: &mut ParseContext) -> Option<CargoTarget> {
    let object = value.and_then(Value::as_object).or_else(|| {
        context.issue(DiagnosticEvidenceIssue::Malformed);
        None
    })?;
    Some(CargoTarget {
        name: required_string(object, "name", MAX_FIELD_BYTES, context)?,
        kind: parse_string_list(object.get("kind"), 128, context)?,
        crate_types: parse_string_list(object.get("crate_types"), 128, context)?,
        src_path: required_string(object, "src_path", MAX_FIELD_BYTES, context)?,
        edition: required_string(object, "edition", 64, context)?,
    })
}

fn parse_build_script_executed(
    object: &Map<String, Value>,
    context: &mut ParseContext,
) -> Option<()> {
    required_string(object, "package_id", MAX_FIELD_BYTES, context)?;
    validate_string_list(object.get("linked_libs"), context)?;
    validate_string_list(object.get("linked_paths"), context)?;
    validate_string_list(object.get("cfgs"), context)?;
    validate_build_script_environment(object.get("env"), context)?;
    required_string(object, "out_dir", MAX_FIELD_BYTES, context)?;
    Some(())
}

fn validate_string_list(value: Option<&Value>, context: &mut ParseContext) -> Option<()> {
    let values = required_array(value, context)?;
    if values.len() > MAX_LIST_ITEMS {
        context.issue(DiagnosticEvidenceIssue::Oversized);
    }
    let mut valid = true;
    for value in values.iter().take(MAX_LIST_ITEMS) {
        if let Some(value) = value.as_str() {
            context.bounded(value, MAX_FIELD_BYTES);
        } else {
            context.issue(DiagnosticEvidenceIssue::Malformed);
            valid = false;
        }
    }
    valid.then_some(())
}

fn validate_build_script_environment(
    value: Option<&Value>,
    context: &mut ParseContext,
) -> Option<()> {
    let values = required_array(value, context)?;
    if values.len() > MAX_LIST_ITEMS {
        context.issue(DiagnosticEvidenceIssue::Oversized);
    }
    let mut valid = true;
    for value in values.iter().take(MAX_LIST_ITEMS) {
        let Some(pair) = value.as_array().filter(|pair| pair.len() == 2) else {
            context.issue(DiagnosticEvidenceIssue::Malformed);
            valid = false;
            continue;
        };
        for item in pair {
            if let Some(item) = item.as_str() {
                context.bounded(item, MAX_FIELD_BYTES);
            } else {
                context.issue(DiagnosticEvidenceIssue::Malformed);
                valid = false;
            }
        }
    }
    valid.then_some(())
}

fn parse_string_list(
    value: Option<&Value>,
    maximum: usize,
    context: &mut ParseContext,
) -> Option<Vec<String>> {
    let values = required_array(value, context)?;
    if values.len() > MAX_LIST_ITEMS {
        context.issue(DiagnosticEvidenceIssue::Oversized);
    }
    let mut strings = Vec::new();
    for value in values.iter().take(MAX_LIST_ITEMS) {
        let Some(value) = value.as_str() else {
            context.issue(DiagnosticEvidenceIssue::Malformed);
            continue;
        };
        strings.push(context.bounded(value, maximum));
    }
    Some(strings)
}

fn required_array<'a>(value: Option<&'a Value>, context: &mut ParseContext) -> Option<&'a [Value]> {
    value
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .or_else(|| {
            context.issue(DiagnosticEvidenceIssue::Malformed);
            None
        })
}

fn required_string(
    object: &Map<String, Value>,
    key: &str,
    maximum: usize,
    context: &mut ParseContext,
) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(|value| context.bounded(value, maximum))
        .or_else(|| {
            context.issue(DiagnosticEvidenceIssue::Malformed);
            None
        })
}

fn parse_diagnostic_level(
    object: &Map<String, Value>,
    context: &mut ParseContext,
) -> Option<String> {
    let level = required_string(object, "level", 64, context)?;
    if matches!(
        level.as_str(),
        "error" | "warning" | "note" | "help" | "failure-note" | "error: internal compiler error"
    ) {
        Some(level)
    } else {
        context.issue(DiagnosticEvidenceIssue::Unexpected);
        None
    }
}

fn parse_diagnostic_code(
    value: Option<&Value>,
    context: &mut ParseContext,
) -> Option<Option<String>> {
    match value {
        Some(Value::Null) => Some(None),
        Some(Value::Object(code)) => Some(Some(required_string(code, "code", 128, context)?)),
        None | Some(_) => {
            context.issue(DiagnosticEvidenceIssue::Malformed);
            None
        }
    }
}

fn required_nullable_string(
    object: &Map<String, Value>,
    key: &str,
    maximum: usize,
    context: &mut ParseContext,
) -> Option<Option<String>> {
    match object.get(key) {
        Some(Value::Null) => Some(None),
        Some(Value::String(value)) => Some(Some(context.bounded(value, maximum))),
        None | Some(_) => {
            context.issue(DiagnosticEvidenceIssue::Malformed);
            None
        }
    }
}

fn required_u64(object: &Map<String, Value>, key: &str, context: &mut ParseContext) -> Option<u64> {
    object.get(key).and_then(Value::as_u64).or_else(|| {
        context.issue(DiagnosticEvidenceIssue::Malformed);
        None
    })
}

fn required_bool(
    object: &Map<String, Value>,
    key: &str,
    context: &mut ParseContext,
) -> Option<bool> {
    object.get(key).and_then(Value::as_bool).or_else(|| {
        context.issue(DiagnosticEvidenceIssue::Malformed);
        None
    })
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 15) as usize] as char);
    }
    output
}

fn nibble(byte: u8) -> u8 {
    if byte <= b'9' {
        byte - b'0'
    } else {
        byte - b'a' + 10
    }
}
