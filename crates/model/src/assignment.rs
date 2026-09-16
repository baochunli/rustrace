//! Versioned assignment manifest parsing and validation.

use std::fmt;

use serde::Deserialize;

pub const SUPPORTED_FORMAT_VERSION: u32 = 2;
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_IDENTIFIER_BYTES: usize = 128;
pub const MAX_TITLE_BYTES: usize = 256;
pub const MAX_TOOLCHAIN_BYTES: usize = 64;
pub const MAX_ALLOWED_PATHS: usize = 128;
pub const MAX_ALLOWED_PATH_BYTES: usize = 256;
pub const MAX_COMMAND_ARGS: usize = 32;
pub const MAX_COMMAND_ARG_BYTES: usize = 1024;

/// A validated assignment manifest.
///
/// This type deliberately does not implement Serde deserialization. Use
/// [`Self::parse`] so encoded-size, version, and field validation cannot be
/// bypassed at an input boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentManifest {
    pub format_version: u32,
    pub course_id: String,
    pub assignment_id: String,
    pub assignment_version: String,
    pub title: String,
    pub toolchain: String,
    pub edition: String,
    pub allowed_paths: Vec<String>,
    pub commands: AssignmentCommands,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentCommands {
    pub check: Vec<String>,
    pub test: Vec<String>,
    pub run: Vec<String>,
    pub clippy: Vec<String>,
    pub format: Vec<String>,
}

impl AssignmentManifest {
    /// Parses and validates a bounded UTF-8 `assignment.toml` document.
    pub fn parse(input: &[u8]) -> Result<Self, AssignmentManifestError> {
        if input.len() > MAX_MANIFEST_BYTES {
            return Err(AssignmentManifestError::TooLarge {
                actual: input.len(),
                limit: MAX_MANIFEST_BYTES,
            });
        }

        let text =
            std::str::from_utf8(input).map_err(|error| AssignmentManifestError::InvalidUtf8 {
                message: error.to_string(),
            })?;
        let version: VersionHeader =
            toml::from_str(text).map_err(AssignmentManifestError::malformed)?;
        if !(1..=SUPPORTED_FORMAT_VERSION).contains(&version.format_version) {
            return Err(AssignmentManifestError::UnsupportedVersion {
                found: version.format_version,
                supported: SUPPORTED_FORMAT_VERSION,
            });
        }

        let wire: AssignmentManifestWire =
            toml::from_str(text).map_err(AssignmentManifestError::malformed)?;
        let manifest = Self {
            format_version: wire.format_version,
            course_id: wire.course_id,
            assignment_id: wire.assignment_id,
            assignment_version: wire.assignment_version,
            title: wire.title,
            toolchain: wire.toolchain,
            edition: wire.edition,
            allowed_paths: wire.allowed_paths,
            commands: AssignmentCommands {
                check: wire.commands.check,
                test: wire.commands.test,
                run: wire.commands.run,
                clippy: wire.commands.clippy,
                format: wire.commands.format,
            },
        };
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), AssignmentManifestError> {
        validate_identifier("course_id", &self.course_id)?;
        validate_identifier("assignment_id", &self.assignment_id)?;
        validate_identifier("assignment_version", &self.assignment_version)?;
        validate_text("title", &self.title, MAX_TITLE_BYTES)?;
        validate_text("toolchain", &self.toolchain, MAX_TOOLCHAIN_BYTES)?;

        if !matches!(self.edition.as_str(), "2015" | "2018" | "2021" | "2024") {
            return Err(AssignmentManifestError::invalid_field(
                "edition",
                "must be one of 2015, 2018, 2021, or 2024",
            ));
        }

        if self.allowed_paths.is_empty() {
            return Err(AssignmentManifestError::invalid_field(
                "allowed_paths",
                "must contain at least one path pattern",
            ));
        }
        if self.allowed_paths.len() > MAX_ALLOWED_PATHS {
            return Err(AssignmentManifestError::invalid_field(
                "allowed_paths",
                format!("contains more than {MAX_ALLOWED_PATHS} path patterns"),
            ));
        }
        for path in &self.allowed_paths {
            validate_text("allowed_paths", path, MAX_ALLOWED_PATH_BYTES)?;
        }

        validate_argv("commands.check", &self.commands.check)?;
        validate_argv("commands.test", &self.commands.test)?;
        validate_argv("commands.run", &self.commands.run)?;
        validate_argv("commands.clippy", &self.commands.clippy)?;
        validate_argv("commands.format", &self.commands.format)?;
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssignmentManifestWire {
    format_version: u32,
    course_id: String,
    assignment_id: String,
    assignment_version: String,
    title: String,
    toolchain: String,
    edition: String,
    allowed_paths: Vec<String>,
    commands: AssignmentCommandsWire,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssignmentCommandsWire {
    check: Vec<String>,
    test: Vec<String>,
    run: Vec<String>,
    clippy: Vec<String>,
    format: Vec<String>,
}

#[derive(Deserialize)]
struct VersionHeader {
    format_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignmentManifestError {
    TooLarge { actual: usize, limit: usize },
    InvalidUtf8 { message: String },
    Malformed { message: String },
    UnsupportedVersion { found: u32, supported: u32 },
    InvalidField { field: &'static str, reason: String },
}

impl AssignmentManifestError {
    fn malformed(error: toml::de::Error) -> Self {
        Self::Malformed {
            message: error.to_string(),
        }
    }

    fn invalid_field(field: &'static str, reason: impl Into<String>) -> Self {
        Self::InvalidField {
            field,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for AssignmentManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { actual, limit } => write!(
                formatter,
                "assignment.toml is {actual} bytes; the limit is {limit} bytes"
            ),
            Self::InvalidUtf8 { message } => {
                write!(formatter, "assignment.toml is not valid UTF-8: {message}")
            }
            Self::Malformed { message } => {
                write!(formatter, "assignment.toml is malformed: {message}")
            }
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "assignment.toml format_version {found} is unsupported; supported versions: 1 through {supported}"
            ),
            Self::InvalidField { field, reason } => {
                write!(formatter, "assignment.toml field `{field}` {reason}")
            }
        }
    }
}

impl std::error::Error for AssignmentManifestError {}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), AssignmentManifestError> {
    validate_text(field, value, MAX_IDENTIFIER_BYTES)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(AssignmentManifestError::invalid_field(
            field,
            "may contain only ASCII letters, digits, `-`, `_`, and `.`",
        ));
    }
    Ok(())
}

fn validate_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), AssignmentManifestError> {
    if value.trim().is_empty() {
        return Err(AssignmentManifestError::invalid_field(
            field,
            "must not be empty or whitespace-only",
        ));
    }
    if value.len() > max_bytes {
        return Err(AssignmentManifestError::invalid_field(
            field,
            format!("exceeds the {max_bytes}-byte limit"),
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(AssignmentManifestError::invalid_field(
            field,
            "must not contain control characters",
        ));
    }
    Ok(())
}

fn validate_argv(field: &'static str, arguments: &[String]) -> Result<(), AssignmentManifestError> {
    if arguments.is_empty() {
        return Err(AssignmentManifestError::invalid_field(
            field,
            "must contain a program name",
        ));
    }
    if arguments.len() > MAX_COMMAND_ARGS {
        return Err(AssignmentManifestError::invalid_field(
            field,
            format!("contains more than {MAX_COMMAND_ARGS} arguments"),
        ));
    }
    for argument in arguments {
        validate_text(field, argument, MAX_COMMAND_ARG_BYTES)?;
    }
    Ok(())
}
