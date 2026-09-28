//! Assignment format 3 fixture trees below `test-cases/files/`.
//!
//! A packaged tree is validated while the `.rta` is read. A deployed tree in
//! the sibling `test-cases/files/` directory is read back with no-follow,
//! descriptor-relative operations so it can be hashed before a program runs
//! there. Both sides use one canonical hash.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::PathBuf;

use rustrace_model::{Hash, WorkspacePath};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

use crate::assignment_package::{MAX_TEST_CASE_FILE_BYTES, MAX_TEST_CASE_TOTAL_BYTES};
use crate::hash::{PinnedWorkspaceRoot, is_excluded_file_name};

/// The fixture root's name inside the packaged and deployed `test-cases/`.
pub const FIXTURE_ROOT: &str = "files";
/// Maximum regular files in one fixture tree.
pub const MAX_FIXTURE_FILES: usize = 256;
/// Maximum directories in one fixture tree, counting explicit directory
/// entries and every ancestor of a file, but not `files/` itself.
pub const MAX_FIXTURE_DIRECTORIES: usize = 256;

const FIXTURE_TREE_DOMAIN: &[u8] = b"rustrace.test-case-fixtures.v1";
const DIRECTORY_TAG: u8 = 1;
const FILE_TAG: u8 = 2;

/// A validated fixture tree. Paths are relative to `files/`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FixtureTree {
    directories: BTreeSet<WorkspacePath>,
    files: BTreeMap<WorkspacePath, Vec<u8>>,
    total_bytes: u64,
}

impl FixtureTree {
    /// Every directory, parents before children, in bytewise path order.
    pub fn directories(&self) -> impl Iterator<Item = &WorkspacePath> {
        self.directories.iter()
    }

    /// Every regular file in bytewise path order.
    pub fn files(&self) -> impl Iterator<Item = (&WorkspacePath, &[u8])> {
        self.files
            .iter()
            .map(|(path, bytes)| (path, bytes.as_slice()))
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn directory_count(&self) -> usize {
        self.directories.len()
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// The canonical fixture-tree hash:
    ///
    /// 1. the byte string `rustrace.test-case-fixtures.v1`;
    /// 2. the entry count (directories plus files) as a big-endian `u32`;
    /// 3. for each entry in bytewise path order, a kind byte (1 directory,
    ///    2 regular file), the path length as a big-endian `u32`, the path
    ///    bytes, and for a file its length as a big-endian `u64` followed by
    ///    its bytes.
    ///
    /// Paths are relative to `files/` and use `/`. An empty tree still has a
    /// hash, distinct from having no fixture tree at all.
    pub fn hash(&self) -> Hash {
        let mut entries = self
            .directories
            .iter()
            .map(|path| (path.as_str(), None))
            .chain(
                self.files
                    .iter()
                    .map(|(path, bytes)| (path.as_str(), Some(bytes.as_slice()))),
            )
            .collect::<Vec<_>>();
        entries.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        let mut hasher = blake3::Hasher::new();
        hasher.update(FIXTURE_TREE_DOMAIN);
        let count = u32::try_from(entries.len()).expect("fixture entry limits fit in a u32");
        hasher.update(&count.to_be_bytes());
        for (path, bytes) in entries {
            hasher.update(&[if bytes.is_some() {
                FILE_TAG
            } else {
                DIRECTORY_TAG
            }]);
            let length = u32::try_from(path.len()).expect("workspace paths fit in a u32");
            hasher.update(&length.to_be_bytes());
            hasher.update(path.as_bytes());
            if let Some(bytes) = bytes {
                hasher.update(&(bytes.len() as u64).to_be_bytes());
                hasher.update(bytes);
            }
        }
        Hash::from_bytes(*hasher.finalize().as_bytes())
    }

    /// Builds a validated tree from explicit directories and files, applying
    /// the package rules: canonical fixture paths, count and size limits, no
    /// file/directory conflicts, and no host-equivalent aliases.
    pub fn from_parts(
        directories: impl IntoIterator<Item = WorkspacePath>,
        files: impl IntoIterator<Item = (WorkspacePath, Vec<u8>)>,
    ) -> Result<Self, FixtureTreeError> {
        let mut tree = Self::default();
        for path in directories {
            require_fixture_path(&path)?;
            tree.insert_directory(path)?;
        }
        for (path, bytes) in files {
            require_fixture_path(&path)?;
            tree.insert_file(path, bytes)?;
        }
        tree.reject_host_aliases()?;
        Ok(tree)
    }

    /// Adds one explicit directory and its ancestors.
    pub(crate) fn insert_directory(&mut self, path: WorkspacePath) -> Result<(), FixtureTreeError> {
        self.insert_ancestors(&path)?;
        self.insert_directory_only(path)
    }

    /// Adds one regular file and its ancestor directories.
    pub(crate) fn insert_file(
        &mut self,
        path: WorkspacePath,
        bytes: Vec<u8>,
    ) -> Result<(), FixtureTreeError> {
        self.insert_ancestors(&path)?;
        if self.directories.contains(&path) || self.files.contains_key(&path) {
            return Err(FixtureTreeError::PathConflict {
                path: path.to_string(),
            });
        }
        if self.files.len() == MAX_FIXTURE_FILES {
            return Err(FixtureTreeError::LimitExceeded {
                kind: "file",
                limit: MAX_FIXTURE_FILES as u64,
            });
        }
        let length = bytes.len() as u64;
        if length > MAX_TEST_CASE_FILE_BYTES {
            return Err(FixtureTreeError::FileTooLarge {
                path: path.to_string(),
                limit: MAX_TEST_CASE_FILE_BYTES,
            });
        }
        let total = self.total_bytes.saturating_add(length);
        if total > MAX_TEST_CASE_TOTAL_BYTES {
            return Err(FixtureTreeError::LimitExceeded {
                kind: "byte",
                limit: MAX_TEST_CASE_TOTAL_BYTES,
            });
        }
        self.total_bytes = total;
        self.files.insert(path, bytes);
        Ok(())
    }

    /// Rejects two entries that a case-insensitive or normalization-
    /// insensitive filesystem would store at one name, so a package that
    /// deploys on one computer deploys on every supported one.
    pub(crate) fn reject_host_aliases(&self) -> Result<(), FixtureTreeError> {
        let mut seen = HashMap::<String, &WorkspacePath>::new();
        for path in self.directories.iter().chain(self.files.keys()) {
            let key = path.as_str().nfd().case_fold().nfd().collect::<String>();
            if let Some(previous) = seen.insert(key, path)
                && previous != path
            {
                return Err(FixtureTreeError::PathConflict {
                    path: path.to_string(),
                });
            }
        }
        Ok(())
    }

    fn insert_ancestors(&mut self, path: &WorkspacePath) -> Result<(), FixtureTreeError> {
        let components = path.components().collect::<Vec<_>>();
        for end in 1..components.len() {
            let ancestor = WorkspacePath::new(components[..end].join("/"))
                .expect("a prefix of a WorkspacePath remains valid");
            self.insert_directory_only(ancestor)?;
        }
        Ok(())
    }

    fn insert_directory_only(&mut self, path: WorkspacePath) -> Result<(), FixtureTreeError> {
        if self.files.contains_key(&path) {
            return Err(FixtureTreeError::PathConflict {
                path: path.to_string(),
            });
        }
        if self.directories.contains(&path) {
            return Ok(());
        }
        if self.directories.len() == MAX_FIXTURE_DIRECTORIES {
            return Err(FixtureTreeError::LimitExceeded {
                kind: "directory",
                limit: MAX_FIXTURE_DIRECTORIES as u64,
            });
        }
        self.directories.insert(path);
        Ok(())
    }
}

/// Fixture paths are canonical workspace paths without control characters,
/// short enough to remain canonical below `files/` in the deployed case
/// folder. No component may be a `.cargo` directory, which would configure a
/// Cargo command run from the tree, or a name that file browsers and editors
/// create (such as `.DS_Store`), which deployed-tree hashing skips.
pub fn is_fixture_path(path: &WorkspacePath) -> bool {
    fixture_path_problem(path).is_none()
}

/// Why a path cannot appear in a fixture tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FixturePathProblem {
    /// Not NFC, holds a control character, or too long or deep below `files/`.
    NotCanonical,
    /// A `.cargo` component, in any letter case.
    CargoConfiguration,
    /// A name that file browsers and editors create, such as `.DS_Store`.
    PlatformClutter,
}

impl FixturePathProblem {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::NotCanonical => "has a name that is not a canonical workspace path",
            Self::CargoConfiguration => "is a `.cargo` Cargo configuration path",
            Self::PlatformClutter => "has a name that file browsers or editors create",
        }
    }
}

/// Whether any component of `path` is `.cargo`, in any letter case (Cargo
/// finds `.CARGO` on a case-insensitive filesystem).
pub(crate) fn has_cargo_configuration_component(path: &WorkspacePath) -> bool {
    path.components()
        .any(|component| component.eq_ignore_ascii_case(".cargo"))
}

/// Why `path` cannot appear in a fixture tree, if it cannot.
pub(crate) fn fixture_path_problem(path: &WorkspacePath) -> Option<FixturePathProblem> {
    if path.as_str().chars().any(char::is_control)
        || WorkspacePath::new(format!("{FIXTURE_ROOT}/{}", path.as_str())).is_err()
    {
        return Some(FixturePathProblem::NotCanonical);
    }
    if has_cargo_configuration_component(path) {
        return Some(FixturePathProblem::CargoConfiguration);
    }
    if path.components().any(is_excluded_file_name) {
        return Some(FixturePathProblem::PlatformClutter);
    }
    None
}

fn require_fixture_path(path: &WorkspacePath) -> Result<(), FixtureTreeError> {
    match fixture_path_problem(path) {
        None => Ok(()),
        Some(problem) => Err(FixtureTreeError::UnsupportedEntry {
            path: path.to_string(),
            reason: problem.reason(),
        }),
    }
}

/// Why a packaged or deployed fixture tree is unusable.
#[derive(Debug)]
pub enum FixtureTreeError {
    UnsupportedPlatform,
    /// A path is both a file and a directory, or aliases another path.
    PathConflict {
        path: String,
    },
    FileTooLarge {
        path: String,
        limit: u64,
    },
    LimitExceeded {
        kind: &'static str,
        limit: u64,
    },
    /// A deployed entry is a symlink, special file, hard-linked file, or has a
    /// name that is not a canonical workspace path component.
    UnsupportedEntry {
        path: String,
        reason: &'static str,
    },
    Changed {
        path: String,
    },
    Filesystem {
        operation: &'static str,
        path: PathBuf,
        message: String,
    },
}

impl fmt::Display for FixtureTreeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(
                formatter,
                "fixture trees are supported only on macOS and Linux"
            ),
            Self::PathConflict { path } => write!(
                formatter,
                "fixture path `files/{path}` conflicts with another fixture file or directory"
            ),
            Self::FileTooLarge { path, limit } => write!(
                formatter,
                "fixture file `files/{path}` exceeds the {limit}-byte limit"
            ),
            Self::LimitExceeded { kind, limit } => {
                write!(formatter, "fixture tree exceeds the {limit}-{kind} limit")
            }
            Self::UnsupportedEntry { path, reason } => {
                write!(formatter, "fixture entry `files/{path}` {reason}")
            }
            Self::Changed { path } => write!(
                formatter,
                "fixture entry `files/{path}` changed while it was read"
            ),
            Self::Filesystem {
                operation,
                path,
                message,
            } => write!(
                formatter,
                "could not {operation} `{}`: {message}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for FixtureTreeError {}

/// Reads the deployed `files/` tree below a pinned `test-cases/` root.
///
/// Returns `Ok(None)` when `files` is absent. Every directory is opened
/// without following links; symlinks, special files, hard-linked files,
/// non-UTF-8 or noncanonical names, and trees beyond the package limits are
/// errors rather than silently skipped, because a program run from the tree
/// would see them.
pub fn read_deployed_fixture_tree(
    test_cases: &PinnedWorkspaceRoot,
) -> Result<Option<FixtureTree>, FixtureTreeError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        deployed::read(test_cases)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = test_cases;
        Err(FixtureTreeError::UnsupportedPlatform)
    }
}

/// Hashes the deployed `files/` tree, or `Ok(None)` when it is absent.
pub fn hash_deployed_fixture_tree(
    test_cases: &PinnedWorkspaceRoot,
) -> Result<Option<Hash>, FixtureTreeError> {
    Ok(read_deployed_fixture_tree(test_cases)?.map(|tree| tree.hash()))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod deployed {
    use std::ffi::OsStr;
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use rustix::fd::OwnedFd;
    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, Stat, fstat, openat, statat};
    use rustix::io::Errno;
    use rustrace_model::WorkspacePath;

    use super::{FIXTURE_ROOT, FixtureTree, FixtureTreeError, fixture_path_problem};
    use crate::assignment_package::MAX_TEST_CASE_FILE_BYTES;
    use crate::hash::PinnedWorkspaceRoot;
    use crate::hash::is_excluded_file_name;

    const DIRECTORY_OPEN_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    const FILE_OPEN_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::NOFOLLOW)
        .union(OFlags::NONBLOCK)
        .union(OFlags::NOCTTY)
        .union(OFlags::CLOEXEC);

    pub(super) fn read(
        test_cases: &PinnedWorkspaceRoot,
    ) -> Result<Option<FixtureTree>, FixtureTreeError> {
        verify(test_cases)?;
        let root_path = test_cases.path().join(FIXTURE_ROOT);
        let discovered = match statat(
            test_cases.directory(),
            FIXTURE_ROOT,
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => {
                verify(test_cases)?;
                return Ok(None);
            }
            Err(error) => return Err(filesystem("inspect fixture root", &root_path, error)),
        };
        if file_type(&discovered) != FileType::Directory {
            return Err(FixtureTreeError::UnsupportedEntry {
                path: String::new(),
                reason: "is not a real directory",
            });
        }
        let root = open_directory(
            test_cases.directory(),
            OsStr::new(FIXTURE_ROOT),
            &discovered,
        )
        .map_err(|error| match error {
            Some(error) => filesystem("open fixture root", &root_path, error),
            None => FixtureTreeError::Changed {
                path: String::new(),
            },
        })?;
        let mut tree = FixtureTree::default();
        walk(&root, &root_path, None, &mut tree)?;
        verify(test_cases)?;
        Ok(Some(tree))
    }

    fn walk(
        directory: &OwnedFd,
        absolute: &Path,
        relative: Option<&WorkspacePath>,
        tree: &mut FixtureTree,
    ) -> Result<(), FixtureTreeError> {
        let mut entries = Dir::read_from(directory)
            .map_err(|error| filesystem("read fixture directory", absolute, error))?;
        while let Some(entry) = entries.read() {
            let entry = entry
                .map_err(|error| filesystem("read fixture directory entry", absolute, error))?;
            let raw_name = entry.file_name().to_bytes();
            if matches!(raw_name, b"." | b"..") {
                continue;
            }
            let child_absolute = absolute.join(OsStr::from_bytes(raw_name));
            let display = |name: &str| match relative {
                Some(parent) => format!("{}/{name}", parent.as_str()),
                None => name.to_owned(),
            };
            let Ok(name) = std::str::from_utf8(raw_name) else {
                return Err(FixtureTreeError::UnsupportedEntry {
                    path: display(&String::from_utf8_lossy(raw_name)),
                    reason: "has a name that is not UTF-8",
                });
            };
            let stat = statat(directory, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| filesystem("inspect fixture entry", &child_absolute, error))?;
            // Finder and editors leave these beside real files; like the
            // workspace hash, the fixture-tree hash ignores them.
            if file_type(&stat) == FileType::RegularFile && is_excluded_file_name(name) {
                continue;
            }
            let path = WorkspacePath::new(display(name)).map_err(|_| {
                FixtureTreeError::UnsupportedEntry {
                    path: display(name),
                    reason: "has a name that is not a canonical workspace path",
                }
            })?;
            if let Some(problem) = fixture_path_problem(&path) {
                return Err(FixtureTreeError::UnsupportedEntry {
                    path: path.to_string(),
                    reason: problem.reason(),
                });
            }
            match file_type(&stat) {
                FileType::Directory => {
                    let child = open_directory(directory, OsStr::from_bytes(raw_name), &stat)
                        .map_err(|error| match error {
                            Some(error) => {
                                filesystem("open fixture directory", &child_absolute, error)
                            }
                            None => FixtureTreeError::Changed {
                                path: path.to_string(),
                            },
                        })?;
                    tree.insert_directory(path.clone())?;
                    walk(&child, &child_absolute, Some(&path), tree)?;
                }
                FileType::RegularFile if stat.st_nlink == 1 => {
                    if stat.st_size as u64 > MAX_TEST_CASE_FILE_BYTES {
                        return Err(FixtureTreeError::FileTooLarge {
                            path: path.to_string(),
                            limit: MAX_TEST_CASE_FILE_BYTES,
                        });
                    }
                    let bytes = read_file(directory, raw_name, &child_absolute, &stat, &path)?;
                    tree.insert_file(path, bytes)?;
                }
                FileType::RegularFile => {
                    return Err(FixtureTreeError::UnsupportedEntry {
                        path: path.to_string(),
                        reason: "is a hard-linked file",
                    });
                }
                FileType::Symlink => {
                    return Err(FixtureTreeError::UnsupportedEntry {
                        path: path.to_string(),
                        reason: "is a symlink",
                    });
                }
                _ => {
                    return Err(FixtureTreeError::UnsupportedEntry {
                        path: path.to_string(),
                        reason: "is a special file",
                    });
                }
            }
        }
        Ok(())
    }

    fn read_file(
        directory: &OwnedFd,
        raw_name: &[u8],
        absolute: &Path,
        discovered: &Stat,
        path: &WorkspacePath,
    ) -> Result<Vec<u8>, FixtureTreeError> {
        let descriptor = openat(
            directory,
            OsStr::from_bytes(raw_name),
            FILE_OPEN_FLAGS,
            Mode::empty(),
        )
        .map_err(|error| filesystem("open fixture file", absolute, error))?;
        let opened = fstat(&descriptor)
            .map_err(|error| filesystem("inspect fixture file", absolute, error))?;
        if file_type(&opened) != FileType::RegularFile
            || opened.st_nlink != 1
            || opened.st_dev != discovered.st_dev
            || opened.st_ino != discovered.st_ino
        {
            return Err(FixtureTreeError::Changed {
                path: path.to_string(),
            });
        }
        let mut bytes = Vec::new();
        std::fs::File::from(descriptor)
            .take(MAX_TEST_CASE_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| filesystem("read fixture file", absolute, error))?;
        if bytes.len() as u64 > MAX_TEST_CASE_FILE_BYTES {
            return Err(FixtureTreeError::FileTooLarge {
                path: path.to_string(),
                limit: MAX_TEST_CASE_FILE_BYTES,
            });
        }
        Ok(bytes)
    }

    /// Opens a discovered directory without following links and confirms it
    /// is the same object. `Err(None)` reports a concurrent replacement.
    fn open_directory(
        parent: &OwnedFd,
        name: &OsStr,
        discovered: &Stat,
    ) -> Result<OwnedFd, Option<Errno>> {
        let opened =
            openat(parent, name, DIRECTORY_OPEN_FLAGS, Mode::empty()).map_err(|error| {
                if matches!(error, Errno::LOOP | Errno::NOTDIR | Errno::NOENT) {
                    None
                } else {
                    Some(error)
                }
            })?;
        let retained = fstat(&opened).map_err(Some)?;
        if file_type(&retained) != FileType::Directory
            || retained.st_dev != discovered.st_dev
            || retained.st_ino != discovered.st_ino
        {
            return Err(None);
        }
        Ok(opened)
    }

    fn file_type(stat: &Stat) -> FileType {
        FileType::from_raw_mode(stat.st_mode)
    }

    fn verify(root: &PinnedWorkspaceRoot) -> Result<(), FixtureTreeError> {
        root.verify_binding()
            .map_err(|error| FixtureTreeError::Filesystem {
                operation: "verify the test-cases root",
                path: root.path().to_owned(),
                message: error.to_string(),
            })
    }

    fn filesystem(
        operation: &'static str,
        path: &Path,
        error: impl std::fmt::Display,
    ) -> FixtureTreeError {
        FixtureTreeError::Filesystem {
            operation,
            path: path.to_owned(),
            message: error.to_string(),
        }
    }
}
