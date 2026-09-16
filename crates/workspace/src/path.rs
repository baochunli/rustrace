//! Assignment path policy and symlink-safe workspace containment checks.

use std::fmt;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rustrace_model::WorkspacePath;
use rustrace_model::assignment::{AssignmentManifest, MAX_ALLOWED_PATH_BYTES, MAX_ALLOWED_PATHS};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::fs::{AtFlags, FileType, Mode, OFlags, open, openat, statat};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::io::Errno;

/// A compiled, validated set of assignment file-path patterns.
#[derive(Debug)]
pub struct AllowedPathSet {
    patterns: GlobSet,
}

impl AllowedPathSet {
    pub fn from_manifest(manifest: &AssignmentManifest) -> Result<Self, AllowedPathSetError> {
        if manifest.allowed_paths.is_empty() {
            return Err(AllowedPathSetError::Empty);
        }
        if manifest.allowed_paths.len() > MAX_ALLOWED_PATHS {
            return Err(AllowedPathSetError::TooManyPatterns {
                actual: manifest.allowed_paths.len(),
                maximum: MAX_ALLOWED_PATHS,
            });
        }

        let mut builder = GlobSetBuilder::new();
        for (index, pattern) in manifest.allowed_paths.iter().enumerate() {
            if pattern.len() > MAX_ALLOWED_PATH_BYTES {
                return Err(invalid_pattern(
                    index,
                    pattern,
                    format!("exceeds the {MAX_ALLOWED_PATH_BYTES}-byte limit"),
                ));
            }
            if pattern.chars().any(char::is_control) {
                return Err(invalid_pattern(
                    index,
                    pattern,
                    "must not contain control characters".to_owned(),
                ));
            }
            if pattern.contains('\\') {
                return Err(invalid_pattern(
                    index,
                    pattern,
                    "contains a backslash; allowed path patterns must use `/` separators"
                        .to_owned(),
                ));
            }
            let normalized = WorkspacePath::new(pattern).map_err(|error| {
                invalid_pattern(
                    index,
                    pattern,
                    format!("is not a safe relative pattern: {error}"),
                )
            })?;
            if normalized.as_str().len() > MAX_ALLOWED_PATH_BYTES {
                return Err(invalid_pattern(
                    index,
                    pattern,
                    format!(
                        "is {} bytes after normalization; maximum is {MAX_ALLOWED_PATH_BYTES} bytes",
                        normalized.as_str().len()
                    ),
                ));
            }
            validate_pattern_syntax(&normalized)
                .map_err(|reason| invalid_pattern(index, pattern, reason.to_owned()))?;

            let glob = GlobBuilder::new(normalized.as_str())
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .map_err(|error| {
                    invalid_pattern(index, pattern, format!("has invalid glob syntax: {error}"))
                })?;
            builder.add(glob);
        }

        let patterns = builder
            .build()
            .map_err(|error| AllowedPathSetError::Compile {
                reason: error.to_string(),
            })?;
        Ok(Self { patterns })
    }

    /// Requires `path` to match at least one manifest pattern.
    ///
    /// Only canonical [`WorkspacePath`] values are accepted, so matching never
    /// has to reinterpret platform separators or repair unsafe input.
    pub fn validate(&self, path: &WorkspacePath) -> Result<(), AllowedPathSetError> {
        if self.patterns.is_match(path.as_str()) {
            Ok(())
        } else {
            Err(AllowedPathSetError::NotAllowed { path: path.clone() })
        }
    }
}

fn validate_pattern_syntax(pattern: &WorkspacePath) -> Result<(), &'static str> {
    for component in pattern.components() {
        if component == "**" {
            continue;
        }
        if component.contains("**") {
            return Err("contains `**` outside a complete path component");
        }
        if component
            .chars()
            .any(|character| matches!(character, '?' | '[' | ']' | '{' | '}'))
        {
            return Err(
                "uses unsupported glob syntax; only literals, `*`, and whole-component `**` are allowed",
            );
        }
    }
    Ok(())
}

fn invalid_pattern(index: usize, pattern: &str, reason: String) -> AllowedPathSetError {
    AllowedPathSetError::InvalidPattern {
        index,
        pattern: pattern.to_owned(),
        reason,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AllowedPathSetError {
    Empty,
    TooManyPatterns {
        actual: usize,
        maximum: usize,
    },
    InvalidPattern {
        index: usize,
        pattern: String,
        reason: String,
    },
    Compile {
        reason: String,
    },
    NotAllowed {
        path: WorkspacePath,
    },
}

impl fmt::Display for AllowedPathSetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("assignment allowed_paths must not be empty"),
            Self::TooManyPatterns { actual, maximum } => write!(
                formatter,
                "assignment has {actual} allowed path patterns; maximum is {maximum}"
            ),
            Self::InvalidPattern {
                index,
                pattern,
                reason,
            } => write!(
                formatter,
                "assignment allowed_paths[{index}] `{pattern}` {reason}"
            ),
            Self::Compile { reason } => {
                write!(
                    formatter,
                    "failed to compile assignment allowed paths: {reason}"
                )
            }
            Self::NotAllowed { path } => {
                write!(
                    formatter,
                    "workspace path `{path}` is not allowed by the assignment"
                )
            }
        }
    }
}

impl std::error::Error for AllowedPathSetError {}

/// Validates an existing path prefix beneath `root` without following symlinks.
///
/// The returned path is rooted at the canonical workspace directory. A missing
/// suffix is accepted after every existing ancestor has been opened relative to
/// a retained directory descriptor with no-follow semantics.
pub fn validate_workspace_path(
    root: &Path,
    path: &WorkspacePath,
) -> Result<PathBuf, WorkspaceContainmentError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        validate_workspace_path_supported(root, path)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (root, path);
        Err(WorkspaceContainmentError::UnsupportedPlatform)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn validate_workspace_path_supported(
    root: &Path,
    path: &WorkspacePath,
) -> Result<PathBuf, WorkspaceContainmentError> {
    let canonical_root =
        std::fs::canonicalize(root).map_err(|source| WorkspaceContainmentError::InvalidRoot {
            path: root.to_owned(),
            source,
        })?;
    let metadata = std::fs::metadata(&canonical_root).map_err(|source| {
        WorkspaceContainmentError::InvalidRoot {
            path: root.to_owned(),
            source,
        }
    })?;
    if !metadata.is_dir() {
        return Err(WorkspaceContainmentError::RootNotDirectory {
            path: root.to_owned(),
        });
    }

    let directory_flags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    let mut directory = open(&canonical_root, directory_flags, Mode::empty()).map_err(|error| {
        WorkspaceContainmentError::Filesystem {
            operation: "open canonical workspace root",
            path: canonical_root.clone(),
            message: error.to_string(),
        }
    })?;

    let mut relative_prefix = PathBuf::new();
    let depth = path.components().count();
    for (index, component) in path.components().enumerate() {
        relative_prefix.push(component);
        let absolute_prefix = canonical_root.join(&relative_prefix);
        let stat = match statat(&directory, component, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(canonical_root.join(path.as_str())),
            Err(error) => {
                return Err(WorkspaceContainmentError::Filesystem {
                    operation: "inspect workspace path component",
                    path: absolute_prefix,
                    message: error.to_string(),
                });
            }
        };

        let file_type = FileType::from_raw_mode(stat.st_mode);
        if file_type == FileType::Symlink {
            return Err(WorkspaceContainmentError::SymlinkComponent {
                path: absolute_prefix,
            });
        }
        if index + 1 == depth {
            return Ok(canonical_root.join(path.as_str()));
        }
        if file_type != FileType::Directory {
            return Err(WorkspaceContainmentError::NotDirectory {
                path: absolute_prefix,
            });
        }

        directory =
            openat(&directory, component, directory_flags, Mode::empty()).map_err(|error| {
                match error {
                    Errno::LOOP => WorkspaceContainmentError::SymlinkComponent {
                        path: absolute_prefix.clone(),
                    },
                    Errno::NOTDIR => WorkspaceContainmentError::NotDirectory {
                        path: absolute_prefix.clone(),
                    },
                    _ => WorkspaceContainmentError::Filesystem {
                        operation: "open workspace directory component",
                        path: absolute_prefix.clone(),
                        message: error.to_string(),
                    },
                }
            })?;
    }

    unreachable!("WorkspacePath always contains at least one component")
}

#[derive(Debug)]
pub enum WorkspaceContainmentError {
    UnsupportedPlatform,
    InvalidRoot {
        path: PathBuf,
        source: std::io::Error,
    },
    RootNotDirectory {
        path: PathBuf,
    },
    SymlinkComponent {
        path: PathBuf,
    },
    NotDirectory {
        path: PathBuf,
    },
    Filesystem {
        operation: &'static str,
        path: PathBuf,
        message: String,
    },
}

impl fmt::Display for WorkspaceContainmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => formatter
                .write_str("workspace containment validation is supported only on Linux and macOS"),
            Self::InvalidRoot { path, source } => write!(
                formatter,
                "workspace root `{}` cannot be resolved: {source}",
                path.display()
            ),
            Self::RootNotDirectory { path } => write!(
                formatter,
                "workspace root `{}` is not a directory",
                path.display()
            ),
            Self::SymlinkComponent { path } => write!(
                formatter,
                "workspace path component `{}` is a symlink",
                path.display()
            ),
            Self::NotDirectory { path } => write!(
                formatter,
                "workspace path component `{}` is not a directory",
                path.display()
            ),
            Self::Filesystem {
                operation,
                path,
                message,
            } => write!(
                formatter,
                "failed to {operation} `{}`: {message}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for WorkspaceContainmentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidRoot { source, .. } => Some(source),
            _ => None,
        }
    }
}
