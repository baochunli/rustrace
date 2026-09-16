//! Canonical, portable paths relative to a Rustrace workspace.

use std::{error::Error, fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use unicode_normalization::UnicodeNormalization;

/// Maximum byte length after canonical separator and NFC validation.
pub const MAX_WORKSPACE_PATH_BYTES: usize = 1024;
/// Maximum UTF-8 byte length of one canonical path component.
pub const MAX_WORKSPACE_COMPONENT_BYTES: usize = 255;
/// Maximum number of canonical path components.
pub const MAX_WORKSPACE_PATH_DEPTH: usize = 64;

/// A nonempty, NFC, slash-separated workspace-relative path.
///
/// Construction validates all invariants. In particular, rooted paths, drive
/// prefixes, empty components, `.` and `..` are rejected rather than repaired.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspacePath(String);

impl WorkspacePath {
    pub fn new(value: impl AsRef<str>) -> Result<Self, WorkspacePathError> {
        let value = value.as_ref();
        if value.is_empty() {
            return Err(WorkspacePathError::Empty);
        }
        if value.contains('\0') {
            return Err(WorkspacePathError::ContainsNull);
        }
        if value.contains('\\') {
            return Err(WorkspacePathError::ContainsBackslash);
        }
        if value.starts_with('/') {
            return Err(WorkspacePathError::Rooted);
        }
        if has_drive_prefix(value) {
            return Err(WorkspacePathError::DrivePrefixed);
        }

        if value.nfc().ne(value.chars()) {
            return Err(WorkspacePathError::NotNfc);
        }
        if value.len() > MAX_WORKSPACE_PATH_BYTES {
            return Err(WorkspacePathError::TooLong {
                actual: value.len(),
                maximum: MAX_WORKSPACE_PATH_BYTES,
            });
        }

        let mut depth = 0;
        for (index, component) in value.split('/').enumerate() {
            if component.is_empty() {
                return Err(WorkspacePathError::EmptyComponent { component: index });
            }
            if component == "." {
                return Err(WorkspacePathError::CurrentDirectory { component: index });
            }
            if component == ".." {
                return Err(WorkspacePathError::ParentDirectory { component: index });
            }
            if component.len() > MAX_WORKSPACE_COMPONENT_BYTES {
                return Err(WorkspacePathError::ComponentTooLong {
                    component: index,
                    actual: component.len(),
                    maximum: MAX_WORKSPACE_COMPONENT_BYTES,
                });
            }
            depth += 1;
        }
        if depth > MAX_WORKSPACE_PATH_DEPTH {
            return Err(WorkspacePathError::TooDeep {
                actual: depth,
                maximum: MAX_WORKSPACE_PATH_DEPTH,
            });
        }

        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
}

fn has_drive_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

impl AsRef<str> for WorkspacePath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for WorkspacePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for WorkspacePath {
    type Err = WorkspacePathError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<&str> for WorkspacePath {
    type Error = WorkspacePathError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for WorkspacePath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for WorkspacePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspacePathError {
    Empty,
    ContainsNull,
    ContainsBackslash,
    NotNfc,
    Rooted,
    DrivePrefixed,
    EmptyComponent {
        component: usize,
    },
    CurrentDirectory {
        component: usize,
    },
    ParentDirectory {
        component: usize,
    },
    TooLong {
        actual: usize,
        maximum: usize,
    },
    ComponentTooLong {
        component: usize,
        actual: usize,
        maximum: usize,
    },
    TooDeep {
        actual: usize,
        maximum: usize,
    },
}

impl fmt::Display for WorkspacePathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("workspace path must not be empty"),
            Self::ContainsNull => {
                formatter.write_str("workspace path must not contain a null byte")
            }
            Self::ContainsBackslash => formatter.write_str(
                "workspace path must use `/` separators and must not contain a backslash",
            ),
            Self::NotNfc => {
                formatter.write_str("workspace path must already use Unicode NFC normalization")
            }
            Self::Rooted => formatter.write_str("workspace path must be relative, not rooted"),
            Self::DrivePrefixed => {
                formatter.write_str("workspace path must not have a Windows drive prefix")
            }
            Self::EmptyComponent { component } => write!(
                formatter,
                "workspace path component {component} is empty; repeated or trailing separators are not allowed"
            ),
            Self::CurrentDirectory { component } => write!(
                formatter,
                "workspace path component {component} is `.`; dot components are not allowed"
            ),
            Self::ParentDirectory { component } => write!(
                formatter,
                "workspace path component {component} is `..`; traversal is not allowed"
            ),
            Self::TooLong { actual, maximum } => write!(
                formatter,
                "workspace path is {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::ComponentTooLong {
                component,
                actual,
                maximum,
            } => write!(
                formatter,
                "workspace path component {component} is {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::TooDeep { actual, maximum } => write!(
                formatter,
                "workspace path has {actual} components; maximum is {maximum} components"
            ),
        }
    }
}

impl Error for WorkspacePathError {}

/// A canonical workspace directory: `.` for the root or a [`WorkspacePath`].
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceDirectory(WorkspaceDirectoryKind);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum WorkspaceDirectoryKind {
    Root,
    Path(WorkspacePath),
}

impl WorkspaceDirectory {
    pub fn new(value: impl AsRef<str>) -> Result<Self, WorkspaceDirectoryError> {
        let value = value.as_ref();
        if value == "." {
            Ok(Self(WorkspaceDirectoryKind::Root))
        } else {
            WorkspacePath::new(value)
                .map(WorkspaceDirectoryKind::Path)
                .map(Self)
                .map_err(WorkspaceDirectoryError::Path)
        }
    }

    pub fn is_root(&self) -> bool {
        matches!(self.0, WorkspaceDirectoryKind::Root)
    }

    pub fn as_str(&self) -> &str {
        match &self.0 {
            WorkspaceDirectoryKind::Root => ".",
            WorkspaceDirectoryKind::Path(path) => path.as_str(),
        }
    }
}

impl From<WorkspacePath> for WorkspaceDirectory {
    fn from(path: WorkspacePath) -> Self {
        Self(WorkspaceDirectoryKind::Path(path))
    }
}

impl AsRef<str> for WorkspaceDirectory {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for WorkspaceDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for WorkspaceDirectory {
    type Err = WorkspaceDirectoryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<&str> for WorkspaceDirectory {
    type Error = WorkspaceDirectoryError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for WorkspaceDirectory {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for WorkspaceDirectory {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceDirectoryError {
    Path(WorkspacePathError),
}

impl fmt::Display for WorkspaceDirectoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(error) => write!(formatter, "invalid workspace directory: {error}"),
        }
    }
}

impl Error for WorkspaceDirectoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Path(error) => Some(error),
        }
    }
}
