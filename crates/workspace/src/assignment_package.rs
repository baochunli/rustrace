//! Streaming reader and safe extractor for `.rta` assignment packages.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::fd::OwnedFd;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, RenameFlags, fstat, mkdirat, open, openat, renameat_with,
    statat, unlinkat,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::io::Errno;

use rustrace_model::assignment::{AssignmentManifest, AssignmentManifestError, MAX_MANIFEST_BYTES};
use rustrace_model::{Hash, WorkspacePath, WorkspacePathError};

use crate::hash::{
    MAX_WORKSPACE_FILE_BYTES, MAX_WORKSPACE_FILES, MAX_WORKSPACE_TOTAL_BYTES, PinnedWorkspaceRoot,
};
use crate::path::{AllowedPathSet, AllowedPathSetError};

const BLOCK_SIZE: usize = 512;
const COPY_BUFFER_BYTES: usize = 8 * 1024;

/// Maximum archive entries, including `assignment.toml` and directories.
pub const HARD_MAX_ENTRIES: usize = 4096;
/// Maximum number of complete packaged test cases.
pub const MAX_TEST_CASES: usize = 256;
/// Maximum number of bytes in one packaged test-case file.
pub const MAX_TEST_CASE_FILE_BYTES: u64 = 1024 * 1024;
/// Maximum combined bytes across all packaged test-case files.
pub const MAX_TEST_CASE_TOTAL_BYTES: u64 = 10 * 1024 * 1024;
/// Maximum expanded bytes: manifest plus independent starter and case limits.
pub const HARD_MAX_EXPANDED_BYTES: u64 =
    MAX_WORKSPACE_TOTAL_BYTES + MAX_TEST_CASE_TOTAL_BYTES + MAX_MANIFEST_BYTES as u64;
pub const DEFAULT_MAX_ENTRIES: usize = HARD_MAX_ENTRIES;
pub const DEFAULT_MAX_EXPANDED_BYTES: u64 = HARD_MAX_EXPANDED_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractionLimits {
    max_entries: usize,
    max_expanded_bytes: u64,
}

impl ExtractionLimits {
    pub fn new(max_entries: usize, max_expanded_bytes: u64) -> Result<Self, ExtractionLimitError> {
        if max_entries > HARD_MAX_ENTRIES {
            return Err(ExtractionLimitError::TooManyEntries {
                requested: max_entries,
                hard_max: HARD_MAX_ENTRIES,
            });
        }
        if max_expanded_bytes > HARD_MAX_EXPANDED_BYTES {
            return Err(ExtractionLimitError::TooManyExpandedBytes {
                requested: max_expanded_bytes,
                hard_max: HARD_MAX_EXPANDED_BYTES,
            });
        }
        Ok(Self {
            max_entries,
            max_expanded_bytes,
        })
    }

    pub fn max_entries(self) -> usize {
        self.max_entries
    }

    pub fn max_expanded_bytes(self) -> u64 {
        self.max_expanded_bytes
    }
}

impl Default for ExtractionLimits {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            max_expanded_bytes: DEFAULT_MAX_EXPANDED_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractionLimitError {
    TooManyEntries { requested: usize, hard_max: usize },
    TooManyExpandedBytes { requested: u64, hard_max: u64 },
}

impl fmt::Display for ExtractionLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyEntries {
                requested,
                hard_max,
            } => write!(
                formatter,
                "requested entry limit {requested} exceeds the hard maximum {hard_max}"
            ),
            Self::TooManyExpandedBytes {
                requested,
                hard_max,
            } => write!(
                formatter,
                "requested expanded-byte limit {requested} exceeds the hard maximum {hard_max}"
            ),
        }
    }
}

impl std::error::Error for ExtractionLimitError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedAssignment {
    pub manifest: AssignmentManifest,
    pub manifest_bytes: Vec<u8>,
    pub starter_files: usize,
    pub test_cases: Option<ExtractedTestCaseSuite>,
    pub expanded_bytes: u64,
    _validated: (),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedTestCaseSuite {
    pub cases: Vec<ExtractedTestCase>,
    pub hash: Hash,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedTestCase {
    pub name: String,
    pub input: Vec<u8>,
    pub expected: Vec<u8>,
}

#[derive(Debug)]
pub enum PublishPreparedWorkspaceError {
    BeforeRename {
        source: AssignmentPackageError,
    },
    AfterRename {
        source: AssignmentPackageError,
        published: PinnedWorkspaceRoot,
    },
}

impl PublishPreparedWorkspaceError {
    pub fn published_workspace(&self) -> Option<&PinnedWorkspaceRoot> {
        match self {
            Self::BeforeRename { .. } => None,
            Self::AfterRename { published, .. } => Some(published),
        }
    }

    fn source_error(&self) -> &AssignmentPackageError {
        match self {
            Self::BeforeRename { source } | Self::AfterRename { source, .. } => source,
        }
    }
}

impl From<AssignmentPackageError> for PublishPreparedWorkspaceError {
    fn from(source: AssignmentPackageError) -> Self {
        Self::BeforeRename { source }
    }
}

impl fmt::Display for PublishPreparedWorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source_error().fmt(formatter)
    }
}

impl std::error::Error for PublishPreparedWorkspaceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source_error())
    }
}

#[derive(Default)]
struct PartialTestCase {
    input: Option<Vec<u8>>,
    expected: Option<Vec<u8>>,
}

#[derive(Clone, Copy)]
enum TestCaseFileKind {
    Input,
    Expected,
}

pub fn extract_assignment_package<R: Read>(
    source: R,
    destination: &Path,
    limits: ExtractionLimits,
) -> Result<ExtractedAssignment, AssignmentPackageError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        extract_supported(source, destination, limits)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (source, destination, limits);
        Err(AssignmentPackageError::UnsupportedPlatform)
    }
}

/// Publishes a prepared assignment workspace beside its final destination.
///
/// Both paths must name siblings. The destination is never replaced.
pub fn publish_prepared_workspace(
    prepared: &Path,
    destination: &Path,
) -> Result<PinnedWorkspaceRoot, PublishPreparedWorkspaceError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let prepared_parent = prepared
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let destination_parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if prepared_parent != destination_parent {
            return Err(AssignmentPackageError::InvalidDestination {
                path: destination.to_owned(),
                reason: "prepared workspace and destination must be siblings".to_owned(),
            }
            .into());
        }
        let prepared_name =
            prepared
                .file_name()
                .ok_or_else(|| AssignmentPackageError::InvalidDestination {
                    path: prepared.to_owned(),
                    reason: "prepared workspace must name a directory".to_owned(),
                })?;
        let destination_name =
            destination
                .file_name()
                .ok_or_else(|| AssignmentPackageError::InvalidDestination {
                    path: destination.to_owned(),
                    reason: "destination must name a directory".to_owned(),
                })?;
        let parent = open_directory_chain(destination_parent, destination)?;
        let discovered = statat(&parent, prepared_name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| filesystem_error("inspect prepared workspace", prepared, error))?;
        if FileType::from_raw_mode(discovered.st_mode) != FileType::Directory {
            return Err(AssignmentPackageError::InvalidDestination {
                path: prepared.to_owned(),
                reason: "prepared workspace is not a real directory".to_owned(),
            }
            .into());
        }
        let opened = openat(&parent, prepared_name, DIRECTORY_OPEN_FLAGS, Mode::empty())
            .map_err(|error| filesystem_error("open prepared workspace", prepared, error))?;
        let retained = fstat(&opened).map_err(|error| {
            filesystem_error("inspect opened prepared workspace", prepared, error)
        })?;
        if FileType::from_raw_mode(retained.st_mode) != FileType::Directory
            || retained.st_dev != discovered.st_dev
            || retained.st_ino != discovered.st_ino
        {
            return Err(AssignmentPackageError::InvalidDestination {
                path: prepared.to_owned(),
                reason: "prepared workspace changed before publication".to_owned(),
            }
            .into());
        }
        let canonical_parent = std::fs::canonicalize(destination_parent).map_err(|source| {
            AssignmentPackageError::Filesystem {
                operation: "resolve assignment publication parent",
                path: destination_parent.to_owned(),
                source,
            }
        })?;
        let canonical_destination = canonical_parent.join(destination_name);
        renameat_with(
            &parent,
            prepared_name,
            &parent,
            destination_name,
            RenameFlags::NOREPLACE,
        )
        .map_err(|error| match error {
            Errno::EXIST => AssignmentPackageError::DestinationExists {
                path: destination.to_owned(),
            },
            Errno::NOSYS | Errno::NOTSUP | Errno::OPNOTSUPP | Errno::INVAL => {
                AssignmentPackageError::AtomicPublishUnavailable {
                    path: destination.to_owned(),
                }
            }
            _ => filesystem_error("publish prepared assignment", destination, error),
        })?;
        let published =
            PinnedWorkspaceRoot::from_retained_directory(canonical_destination, opened, retained);
        if let Err(error) = published.verify_binding() {
            return Err(PublishPreparedWorkspaceError::AfterRename {
                source: AssignmentPackageError::InvalidDestination {
                    path: destination.to_owned(),
                    reason: format!("published workspace binding could not be verified: {error}"),
                },
                published,
            });
        }
        Ok(published)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (prepared, destination);
        Err(AssignmentPackageError::UnsupportedPlatform.into())
    }
}

/// Creates and pins one empty directory without replacing an existing entry.
pub fn create_empty_directory_no_replace(
    destination: &Path,
) -> Result<PinnedWorkspaceRoot, AssignmentPackageError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        StagingDirectory::create(destination)?.publish()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = destination;
        Err(AssignmentPackageError::UnsupportedPlatform)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn extract_supported<R: Read>(
    mut source: R,
    destination: &Path,
    limits: ExtractionLimits,
) -> Result<ExtractedAssignment, AssignmentPackageError> {
    let max_entries = limits.max_entries.min(HARD_MAX_ENTRIES);
    let max_expanded_bytes = limits.max_expanded_bytes.min(HARD_MAX_EXPANDED_BYTES);
    let mut staging = StagingDirectory::create(destination)?;

    let mut paths = HashSet::new();
    let mut manifest = None;
    let mut manifest_bytes = Vec::new();
    let mut allowed_paths = None;
    let mut starter_manifest = None;
    let mut starter_files = 0_usize;
    let mut starter_bytes = 0_u64;
    let mut test_cases = BTreeMap::<String, PartialTestCase>::new();
    let mut test_case_bytes = 0_u64;
    let mut entry_count = 0_usize;
    let mut expanded_bytes = 0_u64;

    loop {
        let mut header = [0_u8; BLOCK_SIZE];
        if !read_block(&mut source, &mut header, "an archive header")? {
            return Err(AssignmentPackageError::TruncatedArchive {
                context: "the two zero end blocks are missing".to_owned(),
            });
        }

        if header.iter().all(|byte| *byte == 0) {
            let mut second = [0_u8; BLOCK_SIZE];
            if !read_block(&mut source, &mut second, "the second zero end block")? {
                return Err(AssignmentPackageError::TruncatedArchive {
                    context: "the second zero end block is missing".to_owned(),
                });
            }
            if second.iter().any(|byte| *byte != 0) {
                return Err(AssignmentPackageError::InvalidHeader {
                    entry: entry_count + 1,
                    reason: "a single zero block appeared before another entry".to_owned(),
                });
            }
            require_end_of_input(&mut source)?;
            break;
        }

        entry_count = entry_count
            .checked_add(1)
            .ok_or(AssignmentPackageError::EntryLimitExceeded { limit: max_entries })?;
        if entry_count > max_entries {
            return Err(AssignmentPackageError::EntryLimitExceeded { limit: max_entries });
        }

        validate_header(&header, entry_count)?;
        let raw_path = header_path(&header, entry_count)?;
        let kind = EntryKind::from_type_flag(header[156], &raw_path)?;
        let path = safe_archive_path(&raw_path, kind)?;
        let size = parse_octal(&header[124..136], "size", entry_count)?;

        if kind == EntryKind::Directory && size != 0 {
            return Err(AssignmentPackageError::InvalidHeader {
                entry: entry_count,
                reason: format!("directory `{path}` declares a nonzero size"),
            });
        }
        expanded_bytes = expanded_bytes.checked_add(size).ok_or(
            AssignmentPackageError::ExpandedSizeLimitExceeded {
                attempted: u64::MAX,
                limit: max_expanded_bytes,
            },
        )?;
        if expanded_bytes > max_expanded_bytes {
            return Err(AssignmentPackageError::ExpandedSizeLimitExceeded {
                attempted: expanded_bytes,
                limit: max_expanded_bytes,
            });
        }

        if !paths.insert(path.clone()) {
            return Err(AssignmentPackageError::DuplicatePath { path });
        }

        let package_entry = classify_entry(
            &path,
            kind,
            manifest
                .as_ref()
                .map(|manifest: &AssignmentManifest| manifest.format_version),
        )?;
        if manifest.is_none() && !matches!(&package_entry, PackageEntry::Manifest) {
            return Err(AssignmentPackageError::MissingManifest);
        }

        match package_entry {
            PackageEntry::Manifest => {
                if size > MAX_MANIFEST_BYTES as u64 {
                    return Err(AssignmentPackageError::Manifest {
                        source: AssignmentManifestError::TooLarge {
                            actual: usize::try_from(size).unwrap_or(usize::MAX),
                            limit: MAX_MANIFEST_BYTES,
                        },
                    });
                }
                let mut contents = vec![0_u8; size as usize];
                read_exact_content(&mut source, &mut contents, &path)?;
                let parsed = AssignmentManifest::parse(&contents)
                    .map_err(|source| AssignmentPackageError::Manifest { source })?;
                let compiled = AllowedPathSet::from_manifest(&parsed)
                    .map_err(|source| AssignmentPackageError::PathPolicy { source })?;
                manifest = Some(parsed);
                manifest_bytes = contents;
                allowed_paths = Some(compiled);
            }
            PackageEntry::StarterRoot => {}
            PackageEntry::StarterDirectory(relative) => {
                staging.ensure_directory(Path::new(relative.as_str()))?;
            }
            PackageEntry::StarterFile(relative) => {
                allowed_paths
                    .as_ref()
                    .ok_or(AssignmentPackageError::MissingManifest)?
                    .validate(&relative)
                    .map_err(|source| AssignmentPackageError::PathPolicy { source })?;
                if size > MAX_WORKSPACE_FILE_BYTES {
                    return Err(AssignmentPackageError::StarterFileSizeLimitExceeded {
                        path: relative,
                        actual: size,
                        limit: MAX_WORKSPACE_FILE_BYTES,
                    });
                }
                let attempted_files = starter_files.checked_add(1).ok_or(
                    AssignmentPackageError::StarterFileCountLimitExceeded {
                        attempted: usize::MAX,
                        limit: MAX_WORKSPACE_FILES,
                    },
                )?;
                if attempted_files > MAX_WORKSPACE_FILES {
                    return Err(AssignmentPackageError::StarterFileCountLimitExceeded {
                        attempted: attempted_files,
                        limit: MAX_WORKSPACE_FILES,
                    });
                }
                let attempted_bytes = starter_bytes.checked_add(size).ok_or(
                    AssignmentPackageError::StarterTotalSizeLimitExceeded {
                        attempted: u64::MAX,
                        limit: MAX_WORKSPACE_TOTAL_BYTES,
                    },
                )?;
                if attempted_bytes > MAX_WORKSPACE_TOTAL_BYTES {
                    return Err(AssignmentPackageError::StarterTotalSizeLimitExceeded {
                        attempted: attempted_bytes,
                        limit: MAX_WORKSPACE_TOTAL_BYTES,
                    });
                }

                let relative_path = Path::new(relative.as_str());
                let mut file = staging.create_file(relative_path)?;
                if relative.as_str() == "Cargo.toml" {
                    let mut contents = vec![0_u8; size as usize];
                    read_exact_content(&mut source, &mut contents, &path)?;
                    file.write_all(&contents).map_err(|source| {
                        AssignmentPackageError::Filesystem {
                            operation: "write starter manifest",
                            path: destination.join(relative_path),
                            source,
                        }
                    })?;
                    starter_manifest = Some(contents);
                } else {
                    copy_exact(
                        &mut source,
                        &mut file,
                        size,
                        &destination.join(relative_path),
                    )?;
                }
                starter_files = attempted_files;
                starter_bytes = attempted_bytes;
            }
            PackageEntry::TestCasesRoot => {}
            PackageEntry::TestCaseFile { name, kind } => {
                if size > MAX_TEST_CASE_FILE_BYTES {
                    return Err(AssignmentPackageError::TestCaseFileSizeLimitExceeded {
                        path,
                        actual: size,
                        limit: MAX_TEST_CASE_FILE_BYTES,
                    });
                }
                let is_new_case = !test_cases.contains_key(&name);
                if is_new_case && test_cases.len() == MAX_TEST_CASES {
                    return Err(AssignmentPackageError::TestCaseCountLimitExceeded {
                        attempted: MAX_TEST_CASES + 1,
                        limit: MAX_TEST_CASES,
                    });
                }
                let attempted_bytes = test_case_bytes.checked_add(size).ok_or(
                    AssignmentPackageError::TestCaseTotalSizeLimitExceeded {
                        attempted: u64::MAX,
                        limit: MAX_TEST_CASE_TOTAL_BYTES,
                    },
                )?;
                if attempted_bytes > MAX_TEST_CASE_TOTAL_BYTES {
                    return Err(AssignmentPackageError::TestCaseTotalSizeLimitExceeded {
                        attempted: attempted_bytes,
                        limit: MAX_TEST_CASE_TOTAL_BYTES,
                    });
                }

                let mut contents = vec![0_u8; size as usize];
                read_exact_content(&mut source, &mut contents, &path)?;
                let case = test_cases.entry(name).or_default();
                match kind {
                    TestCaseFileKind::Input => case.input = Some(contents),
                    TestCaseFileKind::Expected => case.expected = Some(contents),
                }
                test_case_bytes = attempted_bytes;
            }
        }

        skip_padding(&mut source, size, &path, entry_count)?;
    }

    let manifest = manifest.ok_or(AssignmentPackageError::MissingManifest)?;
    if starter_files == 0 {
        return Err(AssignmentPackageError::MissingStarterFiles);
    }
    let test_cases = if manifest.format_version == 2 {
        if test_cases.is_empty() {
            return Err(AssignmentPackageError::MissingTestCases);
        }
        let cases = complete_test_cases(test_cases)?;
        let hash = hash_test_case_suite(&cases);
        Some(ExtractedTestCaseSuite {
            cases,
            hash,
            total_bytes: test_case_bytes,
        })
    } else {
        None
    };
    validate_starter_manifest(starter_manifest.as_deref())?;
    let _published = staging.publish()?;

    Ok(ExtractedAssignment {
        manifest,
        manifest_bytes,
        starter_files,
        test_cases,
        expanded_bytes,
        _validated: (),
    })
}

/// Check structure only; preserve the instructor's exact manifest bytes.
fn validate_starter_manifest(contents: Option<&[u8]>) -> Result<(), AssignmentPackageError> {
    let invalid = |reason: &str| AssignmentPackageError::StarterPackageStructure {
        reason: reason.to_owned(),
    };
    let contents = contents.ok_or_else(|| {
        invalid("add starter/Cargo.toml with a [package] table and an empty [workspace] table")
    })?;
    let text = std::str::from_utf8(contents)
        .map_err(|_| invalid("starter/Cargo.toml must be UTF-8 valid TOML"))?;
    let manifest: toml::Table = text
        .parse()
        .map_err(|_| invalid("starter/Cargo.toml must be valid TOML"))?;
    let package = manifest
        .get("package")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| invalid("starter/Cargo.toml must declare a [package] table"))?;
    if package.contains_key("workspace") {
        return Err(invalid("remove package.workspace from starter/Cargo.toml"));
    }
    let workspace = manifest
        .get("workspace")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| invalid("add an empty [workspace] table to starter/Cargo.toml"))?;
    if let Some(key) = workspace.keys().next() {
        return Err(invalid(&format!(
            "remove workspace.{key} from starter/Cargo.toml; [workspace] must be empty"
        )));
    }
    Ok(())
}

#[derive(Debug)]
pub enum AssignmentPackageError {
    UnsupportedPlatform,
    InvalidDestination {
        path: PathBuf,
        reason: String,
    },
    UnsafeDestinationAncestor {
        path: PathBuf,
    },
    DestinationExists {
        path: PathBuf,
    },
    ArchiveIo {
        context: &'static str,
        source: io::Error,
    },
    TruncatedArchive {
        context: String,
    },
    TrailingData,
    InvalidHeader {
        entry: usize,
        reason: String,
    },
    UnsafePath {
        path: String,
        reason: String,
    },
    InvalidStarterPath {
        path: String,
        source: WorkspacePathError,
    },
    UnsupportedEntryType {
        path: String,
        entry_type: u8,
    },
    UnexpectedEntry {
        path: String,
    },
    DuplicatePath {
        path: String,
    },
    EntryLimitExceeded {
        limit: usize,
    },
    ExpandedSizeLimitExceeded {
        attempted: u64,
        limit: u64,
    },
    StarterFileCountLimitExceeded {
        attempted: usize,
        limit: usize,
    },
    StarterFileSizeLimitExceeded {
        path: WorkspacePath,
        actual: u64,
        limit: u64,
    },
    StarterTotalSizeLimitExceeded {
        attempted: u64,
        limit: u64,
    },
    InvalidTestCasePath {
        path: String,
        reason: &'static str,
    },
    TestCaseFileSizeLimitExceeded {
        path: String,
        actual: u64,
        limit: u64,
    },
    TestCaseCountLimitExceeded {
        attempted: usize,
        limit: usize,
    },
    TestCaseTotalSizeLimitExceeded {
        attempted: u64,
        limit: u64,
    },
    Manifest {
        source: AssignmentManifestError,
    },
    PathPolicy {
        source: AllowedPathSetError,
    },
    MissingManifest,
    MissingStarterFiles,
    StarterPackageStructure {
        reason: String,
    },
    MissingTestCases,
    IncompleteTestCase {
        name: String,
        missing: &'static str,
    },
    WouldOverwrite {
        path: PathBuf,
    },
    FilesystemPathCollision {
        path: PathBuf,
    },
    AtomicPublishUnavailable {
        path: PathBuf,
    },
    EntropyUnavailable {
        message: String,
    },
    Filesystem {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for AssignmentPackageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(
                formatter,
                "safe assignment extraction is supported only on macOS and Linux"
            ),
            Self::InvalidDestination { path, reason } => write!(
                formatter,
                "assignment extraction destination `{}` is invalid: {reason}",
                path.display()
            ),
            Self::UnsafeDestinationAncestor { path } => write!(
                formatter,
                "refusing destination `{}` because an ancestor is a symlink or not a directory",
                path.display()
            ),
            Self::DestinationExists { path } => write!(
                formatter,
                "refusing to extract assignment package: destination `{}` already exists",
                path.display()
            ),
            Self::ArchiveIo { context, source } => {
                write!(formatter, "could not read {context}: {source}")
            }
            Self::TruncatedArchive { context } => {
                write!(formatter, "assignment package is truncated: {context}")
            }
            Self::TrailingData => write!(
                formatter,
                "assignment package has data after its two zero end blocks"
            ),
            Self::InvalidHeader { entry, reason } => {
                write!(
                    formatter,
                    "assignment package entry {entry} has an invalid ustar header: {reason}"
                )
            }
            Self::UnsafePath { path, reason } => {
                write!(
                    formatter,
                    "assignment package path `{path}` is unsafe: {reason}"
                )
            }
            Self::InvalidStarterPath { path, source } => write!(
                formatter,
                "assignment package starter path `{path}` is not canonical: {source}"
            ),
            Self::UnsupportedEntryType { path, entry_type } => write!(
                formatter,
                "assignment package entry `{path}` has unsupported type flag {entry_type:#04x}; only regular files and directories are allowed"
            ),
            Self::UnexpectedEntry { path } => write!(
                formatter,
                "assignment package entry `{path}` is outside assignment.toml and starter/"
            ),
            Self::DuplicatePath { path } => {
                write!(
                    formatter,
                    "assignment package contains duplicate path `{path}`"
                )
            }
            Self::EntryLimitExceeded { limit } => write!(
                formatter,
                "assignment package exceeds the {limit}-entry limit"
            ),
            Self::ExpandedSizeLimitExceeded { attempted, limit } => write!(
                formatter,
                "assignment package declares {attempted} expanded bytes, exceeding the {limit}-byte limit"
            ),
            Self::StarterFileCountLimitExceeded { attempted, limit } => write!(
                formatter,
                "assignment package contains {attempted} starter files; maximum is {limit}"
            ),
            Self::StarterFileSizeLimitExceeded {
                path,
                actual,
                limit,
            } => write!(
                formatter,
                "assignment package starter file `{path}` is {actual} bytes; maximum is {limit} bytes"
            ),
            Self::StarterTotalSizeLimitExceeded { attempted, limit } => write!(
                formatter,
                "assignment package starter files total {attempted} bytes; maximum is {limit} bytes"
            ),
            Self::InvalidTestCasePath { path, reason } => write!(
                formatter,
                "assignment package test-case path `{path}` is invalid: {reason}"
            ),
            Self::TestCaseFileSizeLimitExceeded {
                path,
                actual,
                limit,
            } => write!(
                formatter,
                "assignment package test-case file `{path}` is {actual} bytes; maximum is {limit} bytes"
            ),
            Self::TestCaseCountLimitExceeded { attempted, limit } => write!(
                formatter,
                "assignment package contains {attempted} test cases; maximum is {limit}"
            ),
            Self::TestCaseTotalSizeLimitExceeded { attempted, limit } => write!(
                formatter,
                "assignment package test-case files total {attempted} bytes; maximum is {limit} bytes"
            ),
            Self::Manifest { source } => write!(formatter, "invalid package manifest: {source}"),
            Self::PathPolicy { source } => {
                write!(formatter, "invalid assignment path policy: {source}")
            }
            Self::MissingManifest => {
                write!(formatter, "assignment package is missing assignment.toml")
            }
            Self::MissingStarterFiles => {
                write!(
                    formatter,
                    "assignment package has no regular files under starter/"
                )
            }
            Self::StarterPackageStructure { reason } => write!(
                formatter,
                "assignment starter must be a self-contained package: {reason}"
            ),
            Self::MissingTestCases => write!(
                formatter,
                "assignment package format_version 2 has no test cases; a nonempty test-cases/ suite is required"
            ),
            Self::IncompleteTestCase { name, missing } => write!(
                formatter,
                "assignment package test case `{name}` is missing `{missing}`"
            ),
            Self::WouldOverwrite { path } => write!(
                formatter,
                "refusing to overwrite existing extraction path `{}`",
                path.display()
            ),
            Self::FilesystemPathCollision { path } => write!(
                formatter,
                "archive directory `{}` collides with a differently spelled filesystem path",
                path.display()
            ),
            Self::AtomicPublishUnavailable { path } => write!(
                formatter,
                "cannot atomically publish `{}` without replacement on this filesystem",
                path.display()
            ),
            Self::EntropyUnavailable { message } => write!(
                formatter,
                "could not obtain operating-system entropy for private staging: {message}"
            ),
            Self::Filesystem {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "could not {operation} `{}` while extracting assignment package: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for AssignmentPackageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ArchiveIo { source, .. } | Self::Filesystem { source, .. } => Some(source),
            Self::InvalidStarterPath { source, .. } => Some(source),
            Self::Manifest { source } => Some(source),
            Self::PathPolicy { source } => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
}

impl EntryKind {
    fn from_type_flag(flag: u8, path: &str) -> Result<Self, AssignmentPackageError> {
        match flag {
            0 | b'0' => Ok(Self::File),
            b'5' => Ok(Self::Directory),
            entry_type => Err(AssignmentPackageError::UnsupportedEntryType {
                path: path.to_owned(),
                entry_type,
            }),
        }
    }
}

enum PackageEntry {
    Manifest,
    StarterRoot,
    StarterDirectory(WorkspacePath),
    StarterFile(WorkspacePath),
    TestCasesRoot,
    TestCaseFile {
        name: String,
        kind: TestCaseFileKind,
    },
}

fn validate_header(header: &[u8; BLOCK_SIZE], entry: usize) -> Result<(), AssignmentPackageError> {
    if &header[257..263] != b"ustar\0" || &header[263..265] != b"00" {
        return Err(AssignmentPackageError::InvalidHeader {
            entry,
            reason: "expected POSIX ustar magic and version".to_owned(),
        });
    }

    let expected = parse_octal(&header[148..156], "checksum", entry)?;
    for (field, name) in [
        (&header[100..108], "mode"),
        (&header[108..116], "uid"),
        (&header[116..124], "gid"),
        (&header[124..136], "size"),
        (&header[136..148], "mtime"),
        (&header[329..337], "device major number"),
        (&header[337..345], "device minor number"),
    ] {
        parse_octal(field, name, entry)?;
    }
    let actual: u64 = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(*byte)
            }
        })
        .sum();
    if expected != actual {
        return Err(AssignmentPackageError::InvalidHeader {
            entry,
            reason: format!("checksum is {expected:o}, expected {actual:o}"),
        });
    }
    Ok(())
}

fn header_path(header: &[u8; BLOCK_SIZE], entry: usize) -> Result<String, AssignmentPackageError> {
    let name = parse_text_field(&header[..100], "name", entry)?;
    if name.is_empty() {
        return Err(AssignmentPackageError::InvalidHeader {
            entry,
            reason: "entry name is empty".to_owned(),
        });
    }
    let prefix = parse_text_field(&header[345..500], "prefix", entry)?;
    if prefix.is_empty() {
        Ok(name)
    } else {
        Ok(format!("{prefix}/{name}"))
    }
}

fn parse_text_field(
    field: &[u8],
    field_name: &str,
    entry: usize,
) -> Result<String, AssignmentPackageError> {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    if field[end..].iter().any(|byte| *byte != 0) {
        return Err(AssignmentPackageError::InvalidHeader {
            entry,
            reason: format!("{field_name} contains data after a NUL terminator"),
        });
    }
    std::str::from_utf8(&field[..end])
        .map(str::to_owned)
        .map_err(|error| AssignmentPackageError::InvalidHeader {
            entry,
            reason: format!("{field_name} is not valid UTF-8: {error}"),
        })
}

fn parse_octal(
    field: &[u8],
    field_name: &str,
    entry: usize,
) -> Result<u64, AssignmentPackageError> {
    let start = field.iter().position(|byte| !matches!(byte, b' ' | 0));
    let Some(start) = start else {
        return Ok(0);
    };
    let end = field
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | 0))
        .expect("a non-padding byte was found");
    let mut value = 0_u64;
    for digit in field[start..=end].iter().copied() {
        if !(b'0'..=b'7').contains(&digit) {
            return Err(AssignmentPackageError::InvalidHeader {
                entry,
                reason: format!("{field_name} is not an octal number"),
            });
        }
        value = value
            .checked_mul(8)
            .and_then(|number| number.checked_add(u64::from(digit - b'0')))
            .ok_or_else(|| AssignmentPackageError::InvalidHeader {
                entry,
                reason: format!("{field_name} overflows a 64-bit value"),
            })?;
    }
    Ok(value)
}

fn safe_archive_path(raw: &str, kind: EntryKind) -> Result<String, AssignmentPackageError> {
    if raw.starts_with('/') {
        return Err(unsafe_path(raw, "absolute paths are not allowed"));
    }
    if raw.contains('\\') {
        return Err(unsafe_path(
            raw,
            "backslashes are not valid portable ustar separators",
        ));
    }

    let normalized = if kind == EntryKind::Directory {
        raw.strip_suffix('/').unwrap_or(raw)
    } else {
        if raw.ends_with('/') {
            return Err(unsafe_path(raw, "a regular file path ends with `/`"));
        }
        raw
    };
    if normalized.is_empty() {
        return Err(unsafe_path(raw, "the path is empty"));
    }

    for component in normalized.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(unsafe_path(
                raw,
                "empty, `.` and `..` path components are not allowed",
            ));
        }
    }
    Ok(normalized.to_owned())
}

fn unsafe_path(path: &str, reason: &str) -> AssignmentPackageError {
    AssignmentPackageError::UnsafePath {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

fn classify_entry(
    path: &str,
    kind: EntryKind,
    format_version: Option<u32>,
) -> Result<PackageEntry, AssignmentPackageError> {
    if path == "assignment.toml" {
        return if kind == EntryKind::File {
            Ok(PackageEntry::Manifest)
        } else {
            Err(AssignmentPackageError::UnexpectedEntry {
                path: path.to_owned(),
            })
        };
    }
    if path == "starter" {
        return if kind == EntryKind::Directory {
            Ok(PackageEntry::StarterRoot)
        } else {
            Err(AssignmentPackageError::UnexpectedEntry {
                path: path.to_owned(),
            })
        };
    }
    if path == "test-cases" || path.starts_with("test-cases/") {
        if format_version == Some(1) {
            return Err(AssignmentPackageError::UnexpectedEntry {
                path: path.to_owned(),
            });
        }
        if path == "test-cases" {
            return if kind == EntryKind::Directory {
                Ok(PackageEntry::TestCasesRoot)
            } else {
                Err(invalid_test_case_path(
                    path,
                    "test-cases must be a directory",
                ))
            };
        }
        if kind == EntryKind::Directory {
            return Err(invalid_test_case_path(
                path,
                "directories below test-cases/ are not allowed",
            ));
        }
        let relative = &path["test-cases/".len()..];
        let (name, kind) = if let Some(name) = relative.strip_suffix(".expected") {
            (name, TestCaseFileKind::Expected)
        } else if let Some(name) = relative.strip_suffix(".in") {
            (name, TestCaseFileKind::Input)
        } else {
            return Err(invalid_test_case_path(
                path,
                "expected NAME.in or NAME.expected",
            ));
        };
        if name.is_empty() || name.len() > 64 {
            return Err(invalid_test_case_path(
                path,
                "NAME must contain 1 to 64 bytes",
            ));
        }
        if !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(invalid_test_case_path(
                path,
                "NAME may contain only ASCII letters, digits, `-`, and `_`",
            ));
        }
        return Ok(PackageEntry::TestCaseFile {
            name: name.to_owned(),
            kind,
        });
    }
    let relative =
        path.strip_prefix("starter/")
            .ok_or_else(|| AssignmentPackageError::UnexpectedEntry {
                path: path.to_owned(),
            })?;
    let relative = WorkspacePath::new(relative).map_err(|source| match source {
        WorkspacePathError::Rooted | WorkspacePathError::DrivePrefixed => unsafe_path(
            path,
            "the starter-relative path has a drive, root, or UNC prefix",
        ),
        source => AssignmentPackageError::InvalidStarterPath {
            path: path.to_owned(),
            source,
        },
    })?;
    Ok(match kind {
        EntryKind::File => PackageEntry::StarterFile(relative),
        EntryKind::Directory => PackageEntry::StarterDirectory(relative),
    })
}

fn invalid_test_case_path(path: &str, reason: &'static str) -> AssignmentPackageError {
    AssignmentPackageError::InvalidTestCasePath {
        path: path.to_owned(),
        reason,
    }
}

fn complete_test_cases(
    cases: BTreeMap<String, PartialTestCase>,
) -> Result<Vec<ExtractedTestCase>, AssignmentPackageError> {
    cases
        .into_iter()
        .map(|(name, case)| {
            let input = case
                .input
                .ok_or_else(|| AssignmentPackageError::IncompleteTestCase {
                    name: name.clone(),
                    missing: ".in",
                })?;
            let expected =
                case.expected
                    .ok_or_else(|| AssignmentPackageError::IncompleteTestCase {
                        name: name.clone(),
                        missing: ".expected",
                    })?;
            Ok(ExtractedTestCase {
                name,
                input,
                expected,
            })
        })
        .collect()
}

fn hash_test_case_suite(cases: &[ExtractedTestCase]) -> Hash {
    const PREFIX: &[u8] = b"rustrace.test-case-suite.v1";

    let mut hasher = blake3::Hasher::new();
    hasher.update(PREFIX);
    let count = u32::try_from(cases.len()).expect("test-case limit fits in a u32");
    hasher.update(&count.to_be_bytes());
    for case in cases {
        let name_length = u32::try_from(case.name.len()).expect("case name limit fits in a u32");
        hasher.update(&name_length.to_be_bytes());
        hasher.update(case.name.as_bytes());
        hash_bytes(&mut hasher, &case.input);
        hash_bytes(&mut hasher, &case.expected);
    }
    Hash::from_bytes(*hasher.finalize().as_bytes())
}

fn hash_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    let length = u64::try_from(bytes.len()).expect("supported platforms use at most u64 lengths");
    hasher.update(&length.to_be_bytes());
    hasher.update(bytes);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
const DIRECTORY_MODE: Mode = Mode::RUSR
    .union(Mode::WUSR)
    .union(Mode::XUSR)
    .union(Mode::RGRP)
    .union(Mode::XGRP)
    .union(Mode::ROTH)
    .union(Mode::XOTH);
#[cfg(any(target_os = "linux", target_os = "macos"))]
const PRIVATE_DIRECTORY_MODE: Mode = Mode::RUSR.union(Mode::WUSR).union(Mode::XUSR);
#[cfg(any(target_os = "linux", target_os = "macos"))]
const FILE_MODE: Mode = Mode::RUSR
    .union(Mode::WUSR)
    .union(Mode::RGRP)
    .union(Mode::ROTH);
#[cfg(any(target_os = "linux", target_os = "macos"))]
const DIRECTORY_OPEN_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
#[cfg(any(target_os = "linux", target_os = "macos"))]
const FILE_CREATE_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct StagingDirectory {
    parent: OwnedFd,
    destination_name: OsString,
    destination_path: PathBuf,
    staging_name: OsString,
    root: OwnedFd,
    directories: HashSet<PathBuf>,
    created: Vec<CreatedNode>,
    published: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
enum CreatedNode {
    File { parent: PathBuf, name: OsString },
    Directory { parent: PathBuf, name: OsString },
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl StagingDirectory {
    fn create(destination: &Path) -> Result<Self, AssignmentPackageError> {
        let destination_name =
            destination
                .file_name()
                .ok_or_else(|| AssignmentPackageError::InvalidDestination {
                    path: destination.to_owned(),
                    reason: "must name a new directory below an existing parent".to_owned(),
                })?;
        let parent_path = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = open_directory_chain(parent_path, destination)?;

        match statat(&parent, destination_name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => {
                return Err(AssignmentPackageError::DestinationExists {
                    path: destination.to_owned(),
                });
            }
            Err(Errno::NOENT) => {}
            Err(error) => {
                return Err(filesystem_error("inspect destination", destination, error));
            }
        }

        let staging_name = create_private_staging(&parent, parent_path)?;
        let root = match openat(&parent, &staging_name, DIRECTORY_OPEN_FLAGS, Mode::empty()) {
            Ok(root) => root,
            Err(error) => {
                let _ = unlinkat(&parent, &staging_name, AtFlags::REMOVEDIR);
                return Err(filesystem_error(
                    "open private staging directory",
                    &parent_path.join(&staging_name),
                    error,
                ));
            }
        };

        Ok(Self {
            parent,
            destination_name: destination_name.to_owned(),
            destination_path: destination.to_owned(),
            staging_name,
            root,
            directories: HashSet::new(),
            created: Vec::new(),
            published: false,
        })
    }

    fn ensure_directory(&mut self, relative: &Path) -> Result<(), AssignmentPackageError> {
        drop(self.open_or_create_directory_chain(relative)?);
        Ok(())
    }

    fn open_or_create_directory_chain(
        &mut self,
        relative: &Path,
    ) -> Result<Vec<OwnedFd>, AssignmentPackageError> {
        let mut logical = PathBuf::new();
        let mut chain = Vec::with_capacity(relative.components().count());
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(unsafe_path(
                    &relative.to_string_lossy(),
                    "the extracted path is not a normal relative path",
                ));
            };
            let parent = logical.clone();
            logical.push(name);
            let known = self.directories.contains(&logical);
            if !known {
                let result = mkdirat(
                    directory_chain_leaf(&self.root, &chain),
                    name,
                    DIRECTORY_MODE,
                );
                match result {
                    Ok(()) => self.created.push(CreatedNode::Directory {
                        parent: parent.clone(),
                        name: name.to_owned(),
                    }),
                    Err(Errno::EXIST) => {
                        return Err(AssignmentPackageError::FilesystemPathCollision {
                            path: self.destination_path.join(&logical),
                        });
                    }
                    Err(error) => {
                        return Err(filesystem_error(
                            "create staged directory",
                            &self.destination_path.join(&logical),
                            error,
                        ));
                    }
                }
            }

            let directory = openat(
                directory_chain_leaf(&self.root, &chain),
                name,
                DIRECTORY_OPEN_FLAGS,
                Mode::empty(),
            )
            .map_err(|error| {
                filesystem_error(
                    "open staged directory",
                    &self.destination_path.join(&logical),
                    error,
                )
            })?;
            if !known {
                self.directories.insert(logical.clone());
            }
            chain.push(directory);
        }
        Ok(chain)
    }

    fn create_file(&mut self, relative: &Path) -> Result<File, AssignmentPackageError> {
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let directory_chain = self.open_or_create_directory_chain(parent)?;
        let name =
            relative
                .file_name()
                .ok_or_else(|| AssignmentPackageError::InvalidDestination {
                    path: self.destination_path.join(relative),
                    reason: "starter file has no filename".to_owned(),
                })?;
        let fd = openat(
            directory_chain_leaf(&self.root, &directory_chain),
            name,
            FILE_CREATE_FLAGS,
            FILE_MODE,
        )
        .map_err(|error| {
            if error == Errno::EXIST {
                AssignmentPackageError::WouldOverwrite {
                    path: self.destination_path.join(relative),
                }
            } else {
                filesystem_error(
                    "create staged file",
                    &self.destination_path.join(relative),
                    error,
                )
            }
        })?;
        self.created.push(CreatedNode::File {
            parent: parent.to_owned(),
            name: name.to_owned(),
        });
        Ok(File::from(fd))
    }

    fn publish(mut self) -> Result<PinnedWorkspaceRoot, AssignmentPackageError> {
        let expected = fstat(&self.root).map_err(|error| {
            filesystem_error(
                "inspect staged assignment before publication",
                &self.destination_path,
                error,
            )
        })?;
        match renameat_with(
            &self.parent,
            &self.staging_name,
            &self.parent,
            &self.destination_name,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {
                self.published = true;
                let published =
                    PinnedWorkspaceRoot::open(&self.destination_path).map_err(|error| {
                        AssignmentPackageError::InvalidDestination {
                            path: self.destination_path.clone(),
                            reason: format!("published directory could not be pinned: {error}"),
                        }
                    })?;
                if !published.has_identity(&expected) {
                    return Err(AssignmentPackageError::InvalidDestination {
                        path: self.destination_path.clone(),
                        reason: "published directory changed during publication".to_owned(),
                    });
                }
                published.verify_binding().map_err(|error| {
                    AssignmentPackageError::InvalidDestination {
                        path: self.destination_path.clone(),
                        reason: format!(
                            "published directory binding could not be verified: {error}"
                        ),
                    }
                })?;
                Ok(published)
            }
            Err(Errno::EXIST) => Err(AssignmentPackageError::DestinationExists {
                path: self.destination_path.clone(),
            }),
            Err(error)
                if error == Errno::NOSYS
                    || error == Errno::NOTSUP
                    || error == Errno::OPNOTSUPP
                    || error == Errno::INVAL =>
            {
                Err(AssignmentPackageError::AtomicPublishUnavailable {
                    path: self.destination_path.clone(),
                })
            }
            Err(error) => Err(filesystem_error(
                "atomically publish staged assignment",
                &self.destination_path,
                error,
            )),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        for node in self.created.iter().rev() {
            let (parent, name, flags) = match node {
                CreatedNode::File { parent, name } => (parent, name, AtFlags::empty()),
                CreatedNode::Directory { parent, name } => (parent, name, AtFlags::REMOVEDIR),
            };
            let Ok(chain) = open_existing_directory_chain(&self.root, parent) else {
                continue;
            };
            let _ = unlinkat(directory_chain_leaf(&self.root, &chain), name, flags);
        }
        let _ = unlinkat(&self.parent, &self.staging_name, AtFlags::REMOVEDIR);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_existing_directory_chain(root: &OwnedFd, relative: &Path) -> Result<Vec<OwnedFd>, Errno> {
    let mut chain = Vec::with_capacity(relative.components().count());
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(Errno::INVAL);
        };
        let directory = openat(
            directory_chain_leaf(root, &chain),
            name,
            DIRECTORY_OPEN_FLAGS,
            Mode::empty(),
        )?;
        chain.push(directory);
    }
    Ok(chain)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn directory_chain_leaf<'a>(root: &'a OwnedFd, chain: &'a [OwnedFd]) -> &'a OwnedFd {
    chain.last().unwrap_or(root)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_directory_chain(
    path: &Path,
    destination: &Path,
) -> Result<OwnedFd, AssignmentPackageError> {
    let (start, mut traversed) = if path.is_absolute() {
        (Path::new("/"), PathBuf::from("/"))
    } else {
        (Path::new("."), PathBuf::from("."))
    };
    let mut directory = open(start, DIRECTORY_OPEN_FLAGS, Mode::empty())
        .map_err(|error| filesystem_error("open destination path anchor", destination, error))?;

    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::ParentDir => OsStr::new(".."),
            Component::Normal(name) => name,
            Component::Prefix(_) => {
                return Err(AssignmentPackageError::InvalidDestination {
                    path: destination.to_owned(),
                    reason: "platform path prefixes are unsupported".to_owned(),
                });
            }
        };
        traversed.push(name);
        directory =
            openat(&directory, name, DIRECTORY_OPEN_FLAGS, Mode::empty()).map_err(|error| {
                if error == Errno::LOOP || error == Errno::NOTDIR {
                    AssignmentPackageError::UnsafeDestinationAncestor {
                        path: traversed.clone(),
                    }
                } else {
                    filesystem_error("open destination ancestor", &traversed, error)
                }
            })?;
    }
    Ok(directory)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn create_private_staging(
    parent: &OwnedFd,
    parent_path: &Path,
) -> Result<OsString, AssignmentPackageError> {
    create_private_staging_with(parent, parent_path, os_staging_nonce)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn create_private_staging_with<F>(
    parent: &OwnedFd,
    parent_path: &Path,
    mut next_nonce: F,
) -> Result<OsString, AssignmentPackageError>
where
    F: FnMut() -> Result<[u8; 16], AssignmentPackageError>,
{
    let mut last_error = Errno::EXIST;
    for _ in 0..128 {
        let nonce = next_nonce()?;
        let name = staging_name(&nonce);
        match mkdirat(parent, &name, PRIVATE_DIRECTORY_MODE) {
            Ok(()) => return Ok(name),
            Err(Errno::EXIST) => last_error = Errno::EXIST,
            Err(error) => {
                return Err(filesystem_error(
                    "create private staging directory",
                    parent_path,
                    error,
                ));
            }
        }
    }
    Err(filesystem_error(
        "create a uniquely named private staging directory",
        parent_path,
        last_error,
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn os_staging_nonce() -> Result<[u8; 16], AssignmentPackageError> {
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|error| AssignmentPackageError::EntropyUnavailable {
        message: error.to_string(),
    })?;
    Ok(nonce)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn staging_name(nonce: &[u8; 16]) -> OsString {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut name = String::with_capacity(".rustrace-staging-".len() + nonce.len() * 2);
    name.push_str(".rustrace-staging-");
    for byte in nonce {
        name.push(char::from(HEX[usize::from(byte >> 4)]));
        name.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    OsString::from(name)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn filesystem_error(operation: &'static str, path: &Path, error: Errno) -> AssignmentPackageError {
    AssignmentPackageError::Filesystem {
        operation,
        path: path.to_owned(),
        source: error.into(),
    }
}

fn copy_exact<R: Read>(
    source: &mut R,
    target: &mut File,
    size: u64,
    path: &Path,
) -> Result<(), AssignmentPackageError> {
    let mut remaining = size;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(COPY_BUFFER_BYTES as u64)).unwrap();
        let read = read_some(source, &mut buffer[..wanted], "archive entry contents")?;
        if read == 0 {
            return Err(AssignmentPackageError::TruncatedArchive {
                context: format!("file `{}` ended before its declared size", path.display()),
            });
        }
        target
            .write_all(&buffer[..read])
            .map_err(|source| AssignmentPackageError::Filesystem {
                operation: "write file",
                path: path.to_owned(),
                source,
            })?;
        remaining -= read as u64;
    }
    Ok(())
}

fn read_exact_content<R: Read>(
    source: &mut R,
    contents: &mut [u8],
    path: &str,
) -> Result<(), AssignmentPackageError> {
    let mut offset = 0;
    while offset < contents.len() {
        let read = read_some(source, &mut contents[offset..], "archive entry contents")?;
        if read == 0 {
            return Err(AssignmentPackageError::TruncatedArchive {
                context: format!("file `{path}` ended before its declared size"),
            });
        }
        offset += read;
    }
    Ok(())
}

fn skip_padding<R: Read>(
    source: &mut R,
    size: u64,
    path: &str,
    entry: usize,
) -> Result<(), AssignmentPackageError> {
    let padding = ((BLOCK_SIZE as u64 - size % BLOCK_SIZE as u64) % BLOCK_SIZE as u64) as usize;
    let mut bytes = [0_u8; BLOCK_SIZE - 1];
    read_exact_content(source, &mut bytes[..padding], path)?;
    if bytes[..padding].iter().any(|byte| *byte != 0) {
        return Err(AssignmentPackageError::InvalidHeader {
            entry,
            reason: format!("entry `{path}` has nonzero padding"),
        });
    }
    Ok(())
}

fn read_block<R: Read>(
    source: &mut R,
    block: &mut [u8; BLOCK_SIZE],
    context: &'static str,
) -> Result<bool, AssignmentPackageError> {
    let mut offset = 0;
    while offset < block.len() {
        let read = read_some(source, &mut block[offset..], context)?;
        if read == 0 {
            if offset == 0 {
                return Ok(false);
            }
            return Err(AssignmentPackageError::TruncatedArchive {
                context: format!("{context} contains only {offset} of {BLOCK_SIZE} bytes"),
            });
        }
        offset += read;
    }
    Ok(true)
}

fn require_end_of_input<R: Read>(source: &mut R) -> Result<(), AssignmentPackageError> {
    let mut byte = [0_u8; 1];
    if read_some(source, &mut byte, "the end of the archive")? == 0 {
        Ok(())
    } else {
        Err(AssignmentPackageError::TrailingData)
    }
}

fn read_some<R: Read>(
    source: &mut R,
    buffer: &mut [u8],
    context: &'static str,
) -> Result<usize, AssignmentPackageError> {
    loop {
        match source.read(buffer) {
            Ok(read) => return Ok(read),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(source) => {
                return Err(AssignmentPackageError::ArchiveIo { context, source });
            }
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod staging_name_tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    #[test]
    fn staging_name_is_exactly_the_128_bit_nonce_in_lower_hex() {
        let nonce = [0xab; 16];

        let name = staging_name(&nonce);

        assert_eq!(
            name.to_str(),
            Some(".rustrace-staging-abababababababababababababababab")
        );
    }

    #[test]
    fn collision_retry_requests_a_fresh_nonce() {
        let parent = TestParent::new();
        let first = [0x11; 16];
        let second = [0x22; 16];
        mkdirat(&parent.handle, staging_name(&first), PRIVATE_DIRECTORY_MODE).unwrap();
        let mut calls = 0;

        let created = create_private_staging_with(&parent.handle, &parent.path, || {
            calls += 1;
            Ok(if calls == 1 { first } else { second })
        })
        .expect("second nonce creates staging");

        assert_eq!(calls, 2);
        assert_eq!(created, staging_name(&second));
        unlinkat(&parent.handle, created, AtFlags::REMOVEDIR).unwrap();
        unlinkat(&parent.handle, staging_name(&first), AtFlags::REMOVEDIR).unwrap();
    }

    #[test]
    fn entropy_failure_is_typed_and_creates_nothing() {
        let parent = TestParent::new();

        let error = create_private_staging_with(&parent.handle, &parent.path, || {
            Err(AssignmentPackageError::EntropyUnavailable {
                message: "injected entropy failure".to_owned(),
            })
        })
        .expect_err("entropy failure");

        assert!(matches!(
            error,
            AssignmentPackageError::EntropyUnavailable { .. }
        ));
        assert_eq!(fs::read_dir(&parent.path).unwrap().count(), 0);
    }

    static NEXT_TEST_PARENT: AtomicU64 = AtomicU64::new(0);

    struct TestParent {
        path: PathBuf,
        handle: OwnedFd,
    }

    impl TestParent {
        fn new() -> Self {
            let sequence = NEXT_TEST_PARENT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rustrace-staging-nonce-test-{}-{sequence}",
                std::process::id()
            ));
            if path.exists() {
                fs::remove_dir_all(&path).unwrap();
            }
            fs::create_dir(&path).unwrap();
            let path = fs::canonicalize(path).unwrap();
            let handle = open(&path, DIRECTORY_OPEN_FLAGS, Mode::empty()).unwrap();
            Self { path, handle }
        }
    }

    impl Drop for TestParent {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.path)
                && error.kind() != io::ErrorKind::NotFound
            {
                panic!("failed to remove nonce test directory: {error}");
            }
        }
    }
}
