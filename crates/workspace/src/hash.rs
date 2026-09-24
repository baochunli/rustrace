//! Canonical v1 workspace tree hashing.
//!
//! [`hash_entries`] hashes a supplied set of included entries. [`hash_workspace`]
//! safely discovers included files and applies the version 1 exclusion policy.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use rustrace_model::{
    Hash, MAX_WORKSPACE_PATH_DEPTH, SessionId, WorkspacePath, WorkspacePathError,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::collections::BTreeSet;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::ffi::OsStr;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::fs::File;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::Read;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::ffi::OsStrExt;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::fd::OwnedFd;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::fs::{
    AtFlags, Dir, FileType, FlockOperation, Mode, OFlags, Stat, flock, fstat, fsync, mkdirat, open,
    openat, statat,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::io::{Errno, dup};

/// Unconditional maximum number of regular files in a hashed workspace.
pub const MAX_WORKSPACE_FILES: usize = 256;
/// Unconditional maximum byte length of one regular file.
pub const MAX_WORKSPACE_FILE_BYTES: u64 = 1024 * 1024;
/// Unconditional maximum combined byte length of all regular files.
pub const MAX_WORKSPACE_TOTAL_BYTES: u64 = 10 * 1024 * 1024;
/// Unconditional maximum number of directories inspected during one hash.
///
/// This covers the root plus the maximum distinct directory prefixes possible
/// for the supported number and depth of included file paths.
pub const MAX_WORKSPACE_DIRECTORIES: usize = MAX_WORKSPACE_FILES * MAX_WORKSPACE_PATH_DEPTH + 1;

const V1_PREFIX: &[u8] = b"rustrace.workspace-tree\0v1\0";
#[cfg(any(target_os = "linux", target_os = "macos"))]
const READ_BUFFER_BYTES: usize = 8 * 1024;

/// A no-follow directory authority retained for one workspace session.
///
/// The canonical path is only a display and binding-verification name. All
/// trusted traversal and mutation starts from the retained descriptor.
pub struct PinnedWorkspaceRoot {
    binding_path: PathBuf,
    canonical_path: PathBuf,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    directory: OwnedFd,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    identity: Stat,
}

impl fmt::Debug for PinnedWorkspaceRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PinnedWorkspaceRoot")
            .field("binding_path", &self.binding_path)
            .field("canonical_path", &self.canonical_path)
            .finish_non_exhaustive()
    }
}

impl PinnedWorkspaceRoot {
    pub fn open(root: &Path) -> Result<Self, WorkspaceHashError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let binding_path = if root.is_absolute() {
                root.to_owned()
            } else {
                std::env::current_dir()
                    .map_err(|source| WorkspaceHashError::InvalidRoot {
                        path: root.to_owned(),
                        source,
                    })?
                    .join(root)
            };
            let canonical_path = std::fs::canonicalize(&binding_path).map_err(|source| {
                WorkspaceHashError::InvalidRoot {
                    path: root.to_owned(),
                    source,
                }
            })?;
            let directory =
                open(&canonical_path, directory_open_flags(), Mode::empty()).map_err(|error| {
                    if error == Errno::NOTDIR {
                        WorkspaceHashError::RootNotDirectory {
                            path: root.to_owned(),
                        }
                    } else {
                        filesystem_error("open canonical workspace root", &canonical_path, error)
                    }
                })?;
            let identity = fstat(&directory).map_err(|error| {
                filesystem_error("inspect canonical workspace root", &canonical_path, error)
            })?;
            if FileType::from_raw_mode(identity.st_mode) != FileType::Directory {
                return Err(WorkspaceHashError::RootNotDirectory {
                    path: root.to_owned(),
                });
            }
            let pinned = Self {
                binding_path,
                canonical_path,
                directory,
                identity,
            };
            pinned.verify_binding()?;
            Ok(pinned)
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = root;
            Err(WorkspaceHashError::UnsupportedPlatform)
        }
    }

    pub fn path(&self) -> &Path {
        &self.canonical_path
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn from_retained_directory(
        canonical_path: PathBuf,
        directory: OwnedFd,
        identity: Stat,
    ) -> Self {
        Self {
            binding_path: canonical_path.clone(),
            canonical_path,
            directory,
            identity,
        }
    }

    /// Whether two retained roots name the same underlying directory object.
    /// Callers still verify each binding independently at consequential use.
    pub fn is_same_directory(&self, other: &Self) -> bool {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            same_identity(&self.identity, &other.identity)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            self.canonical_path == other.canonical_path
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn directory(&self) -> &OwnedFd {
        &self.directory
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn has_identity(&self, identity: &Stat) -> bool {
        same_identity(&self.identity, identity)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn verify_binding(&self) -> Result<(), WorkspaceHashError> {
        let resolved = std::fs::canonicalize(&self.binding_path)
            .map_err(|_| root_binding_changed(&self.binding_path))?;
        if resolved != self.canonical_path {
            return Err(root_binding_changed(&self.binding_path));
        }
        let reopened = open(&resolved, directory_open_flags(), Mode::empty())
            .map_err(|_| root_binding_changed(&self.binding_path))?;
        let current = fstat(&reopened).map_err(|_| root_binding_changed(&self.binding_path))?;
        if FileType::from_raw_mode(current.st_mode) != FileType::Directory
            || !same_identity(&self.identity, &current)
        {
            return Err(root_binding_changed(&self.binding_path));
        }
        Ok(())
    }

    pub fn open_state_directory(&self) -> Result<PinnedStateDirectory, WorkspaceHashError> {
        self.open_state_directory_inner(true)
    }

    /// Opens an existing `.rustrace` directory without creating any state.
    pub fn open_existing_state_directory(
        &self,
    ) -> Result<PinnedStateDirectory, WorkspaceHashError> {
        self.open_state_directory_inner(false)
    }

    fn open_state_directory_inner(
        &self,
        create: bool,
    ) -> Result<PinnedStateDirectory, WorkspaceHashError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            const STATE_DIRECTORY: &str = ".rustrace";
            self.verify_binding()?;
            let display_path = self.canonical_path.join(STATE_DIRECTORY);
            let discovered =
                match statat(&self.directory, STATE_DIRECTORY, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(stat) => stat,
                    Err(Errno::NOENT) if create => {
                        let mode = Mode::RUSR.union(Mode::WUSR).union(Mode::XUSR);
                        mkdirat(&self.directory, STATE_DIRECTORY, mode).map_err(|error| {
                            filesystem_error(
                                "create Rustrace state directory",
                                &display_path,
                                error,
                            )
                        })?;
                        fsync(&self.directory).map_err(|error| {
                            filesystem_error("sync workspace root", &self.canonical_path, error)
                        })?;
                        statat(&self.directory, STATE_DIRECTORY, AtFlags::SYMLINK_NOFOLLOW)
                            .map_err(|error| {
                                filesystem_error(
                                    "inspect Rustrace state directory",
                                    &display_path,
                                    error,
                                )
                            })?
                    }
                    Err(error) => {
                        return Err(filesystem_error(
                            "inspect Rustrace state directory",
                            &display_path,
                            error,
                        ));
                    }
                };
            match FileType::from_raw_mode(discovered.st_mode) {
                FileType::Directory => {}
                FileType::Symlink => {
                    return Err(WorkspaceHashError::Symlink { path: display_path });
                }
                kind => {
                    return Err(WorkspaceHashError::UnsupportedFileType {
                        path: display_path,
                        kind: file_type_name(kind),
                    });
                }
            }
            let directory = openat(
                &self.directory,
                STATE_DIRECTORY,
                directory_open_flags(),
                Mode::empty(),
            )
            .map_err(|error| {
                filesystem_error("open Rustrace state directory", &display_path, error)
            })?;
            let opened = fstat(&directory).map_err(|error| {
                filesystem_error(
                    "inspect opened Rustrace state directory",
                    &display_path,
                    error,
                )
            })?;
            if FileType::from_raw_mode(opened.st_mode) != FileType::Directory
                || !same_identity(&discovered, &opened)
            {
                return Err(WorkspaceHashError::WorkspaceChanged {
                    path: display_path,
                    change: WorkspaceChange::EntryReplaced,
                });
            }
            self.verify_binding()?;
            Ok(PinnedStateDirectory {
                display_path,
                directory,
                identity: opened,
                root_binding_path: self.binding_path.clone(),
                root_canonical_path: self.canonical_path.clone(),
                root_directory: dup(&self.directory).map_err(|error| {
                    filesystem_error(
                        "retain workspace root authority",
                        &self.canonical_path,
                        error,
                    )
                })?,
                root_identity: self.identity,
            })
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = create;
            Err(WorkspaceHashError::UnsupportedPlatform)
        }
    }
}

/// Retained authority for the workspace's excluded `.rustrace` state directory.
pub struct PinnedStateDirectory {
    display_path: PathBuf,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    directory: OwnedFd,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    identity: Stat,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    root_binding_path: PathBuf,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    root_canonical_path: PathBuf,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    root_directory: OwnedFd,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    root_identity: Stat,
}

impl fmt::Debug for PinnedStateDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PinnedStateDirectory")
            .field("display_path", &self.display_path)
            .finish_non_exhaustive()
    }
}

impl PinnedStateDirectory {
    /// Whether a named bounded artifact currently exists. No file is created.
    pub fn artifact_exists(&self, name: &str) -> Result<bool, WorkspaceHashError> {
        validate_artifact_name(name)?;
        self.verify()?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let exists = match statat(&self.directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => true,
            Ok(stat) => {
                return Err(WorkspaceHashError::UnsupportedFileType {
                    path: self.display_path.join(name),
                    kind: file_type_name(FileType::from_raw_mode(stat.st_mode)),
                });
            }
            Err(Errno::NOENT) => false,
            Err(error) => {
                return Err(filesystem_error(
                    "inspect Rustrace artifact",
                    &self.display_path.join(name),
                    error,
                ));
            }
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let exists = false;
        self.verify()?;
        Ok(exists)
    }

    /// Sync a new artifact and atomically publish it. Failed temporaries remain evidence.
    fn publish_artifact(
        &self,
        name: &str,
        bytes: &[u8],
        replace: bool,
        verify_owner: impl Fn() -> Result<(), WorkspaceHashError>,
    ) -> Result<(), WorkspaceHashError> {
        verify_owner()?;
        validate_artifact_name(name)?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use rustix::fs::{RenameFlags, renameat_with};
            use std::io::Write;
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let temporary = format!(
                ".artifact-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let directory = &self.directory;
            let existing = if replace {
                match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
                        Some(self.pin_file(name, false)?)
                    }
                    Err(Errno::NOENT) => None,
                    _ => {
                        return Err(filesystem_error(
                            "replace regular artifact",
                            &self.display_path,
                            "artifact is not a regular file",
                        ));
                    }
                }
            } else {
                None
            };
            let descriptor = openat(
                directory,
                temporary.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|e| filesystem_error("create artifact temporary", &self.display_path, e))?;
            let mut file = File::from(descriptor);
            #[cfg(feature = "process-probes")]
            if name == "session.json"
                && std::env::var("RUSTRACE_INTERRUPT_AT").as_deref() == Ok("startup-artifact")
            {
                file.write_all(&bytes[..bytes.len() / 2])
                    .and_then(|()| file.sync_all())
                    .map_err(|e| {
                        filesystem_error("partial startup publication probe", &self.display_path, e)
                    })?;
                std::process::exit(83);
            }
            file.write_all(bytes)
                .and_then(|()| file.sync_all())
                .map_err(|e| filesystem_error("sync artifact", &self.display_path, e))?;
            verify_owner()?;
            if let Some(existing) = &existing {
                existing.verify(self)?;
            }
            renameat_with(
                directory,
                temporary.as_str(),
                directory,
                name,
                if existing.is_some() {
                    RenameFlags::empty()
                } else {
                    RenameFlags::NOREPLACE
                },
            )
            .map_err(|e| filesystem_error("publish artifact", &self.display_path, e))?;
            fsync(directory)
                .map_err(|e| filesystem_error("sync artifact directory", &self.display_path, e))?;
            verify_owner()?;
            let published = self.read_artifact(name, bytes.len())?;
            if published != bytes {
                return Err(filesystem_error(
                    "verify artifact publication",
                    &self.display_path,
                    "artifact bytes differ",
                ));
            }
            verify_owner()?;
            Ok(())
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (bytes, replace);
            Err(WorkspaceHashError::UnsupportedPlatform)
        }
    }

    /// Read a bounded regular artifact through the retained state-directory handle.
    pub fn read_artifact(&self, name: &str, maximum: usize) -> Result<Vec<u8>, WorkspaceHashError> {
        self.verify()?;
        validate_artifact_name(name)?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let pinned = self.pin_file(name, false)?;
            let mut file = File::from(
                dup(&pinned.descriptor)
                    .map_err(|e| filesystem_error("duplicate artifact", &self.display_path, e))?,
            );
            let length = file
                .metadata()
                .map_err(|e| filesystem_error("inspect artifact", &self.display_path, e))?
                .len();
            if length > maximum as u64 {
                return Err(filesystem_error(
                    "read bounded artifact",
                    &self.display_path,
                    "artifact exceeds limit",
                ));
            }
            let mut bytes = Vec::with_capacity(length as usize);
            file.by_ref()
                .take(maximum as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| filesystem_error("read artifact", &self.display_path, e))?;
            if bytes.len() > maximum || bytes.len() as u64 != length {
                return Err(filesystem_error(
                    "read stable artifact",
                    &self.display_path,
                    "artifact size changed",
                ));
            }
            pinned.verify(self)?;
            self.verify()?;
            Ok(bytes)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = maximum;
            Err(WorkspaceHashError::UnsupportedPlatform)
        }
    }

    /// Hold cooperative ownership without opening or creating any journal.
    pub fn lock_for_inspection(self) -> Result<PinnedStateInspection, WorkspaceHashError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let writer = self.lock_writer()?;
            Ok(PinnedStateInspection {
                state: self,
                writer,
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Err(WorkspaceHashError::UnsupportedPlatform)
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn lock_writer(&self) -> Result<WriterOwnership, WorkspaceHashError> {
        self.lock_writer_with(|_| {})
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn lock_writer_with(
        &self,
        after_lock: impl FnOnce(&PinnedStateFile),
    ) -> Result<WriterOwnership, WorkspaceHashError> {
        self.verify()?;
        let writer = self.pin_file("writer.lock", true)?;
        flock(&writer.descriptor, FlockOperation::NonBlockingLockExclusive).map_err(|error| {
            let lock = self.display_path.join("writer.lock");
            if error == rustix::io::Errno::WOULDBLOCK {
                // Command processes inherit the lock, so it also stays held by a
                // program that a killed session started.
                filesystem_error(
                    "acquire exclusive workspace writer ownership",
                    &self.display_path,
                    format!(
                        "held by another open Rustrace session, or by a program a command started before Rustrace was killed; close that session or stop the program (`lsof {}` lists it), then retry",
                        lock.display()
                    ),
                )
            } else {
                filesystem_error(
                    "acquire exclusive workspace writer ownership",
                    &self.display_path,
                    error,
                )
            }
        })?;
        // Install cleanup immediately after successful acquisition, before any
        // fallible verification. Failed contenders never own an unlock guard.
        let writer = WriterOwnership {
            file: writer,
            path: self.display_path.join("writer.lock"),
            released: None,
        };
        after_lock(&writer.file);
        writer.verify(self)?;
        Ok(writer)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn pin_file(&self, name: &str, create: bool) -> Result<PinnedStateFile, WorkspaceHashError> {
        let mut flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        if create {
            flags |= OFlags::CREATE;
        }
        let descriptor =
            openat(&self.directory, name, flags, Mode::RUSR | Mode::WUSR).map_err(|error| {
                filesystem_error(
                    "open controlled state file",
                    &self.display_path.join(name),
                    error,
                )
            })?;
        let identity = fstat(&descriptor).map_err(|error| {
            filesystem_error(
                "inspect controlled state file",
                &self.display_path.join(name),
                error,
            )
        })?;
        if FileType::from_raw_mode(identity.st_mode) != FileType::RegularFile {
            return Err(WorkspaceHashError::UnsupportedFileType {
                path: self.display_path.join(name),
                kind: file_type_name(FileType::from_raw_mode(identity.st_mode)),
            });
        }
        let pinned = PinnedStateFile {
            name: name.to_owned(),
            descriptor,
            identity,
        };
        pinned.verify(self)?;
        Ok(pinned)
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn verify(&self) -> Result<(), WorkspaceHashError> {
        let resolved = std::fs::canonicalize(&self.root_binding_path)
            .map_err(|_| root_binding_changed(&self.root_binding_path))?;
        if resolved != self.root_canonical_path {
            return Err(root_binding_changed(&self.root_binding_path));
        }
        let reopened = open(&resolved, directory_open_flags(), Mode::empty())
            .map_err(|_| root_binding_changed(&self.root_binding_path))?;
        let reopened_root =
            fstat(&reopened).map_err(|_| root_binding_changed(&self.root_binding_path))?;
        let retained_root = fstat(&self.root_directory)
            .map_err(|_| root_binding_changed(&self.root_binding_path))?;
        if FileType::from_raw_mode(reopened_root.st_mode) != FileType::Directory
            || !same_identity(&self.root_identity, &reopened_root)
            || !same_identity(&self.root_identity, &retained_root)
        {
            return Err(root_binding_changed(&self.root_binding_path));
        }

        let linked = statat(&self.root_directory, ".rustrace", AtFlags::SYMLINK_NOFOLLOW).map_err(
            |error| filesystem_error("verify Rustrace state directory", &self.display_path, error),
        )?;
        let retained = fstat(&self.directory).map_err(|error| {
            filesystem_error(
                "inspect retained Rustrace state directory",
                &self.display_path,
                error,
            )
        })?;
        if FileType::from_raw_mode(linked.st_mode) != FileType::Directory
            || !same_identity(&self.identity, &linked)
            || !same_identity(&self.identity, &retained)
        {
            return Err(WorkspaceHashError::WorkspaceChanged {
                path: self.display_path.clone(),
                change: WorkspaceChange::EntryReplaced,
            });
        }
        Ok(())
    }

    pub fn create_journal_file(
        self,
        session_id: &SessionId,
    ) -> Result<PinnedJournalFile, WorkspaceHashError> {
        self.journal_file(session_id, true)
    }

    /// Reopen an existing journal only after acquiring cooperative ownership.
    pub fn open_journal_file(
        self,
        session_id: &SessionId,
    ) -> Result<PinnedJournalFile, WorkspaceHashError> {
        self.journal_file(session_id, false)
    }

    fn journal_file(
        self,
        session_id: &SessionId,
        create: bool,
    ) -> Result<PinnedJournalFile, WorkspaceHashError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.verify()?;
            let writer = self.lock_writer()?;
            let file_name = format!("{session_id}.sqlite");
            let display_path = self.display_path.join(&file_name);
            // A new database must not adopt leftover evidence from an earlier
            // attempt. Cooperative ownership makes this preflight stable.
            for suffix in if create {
                &["-wal", "-shm", "-journal"][..]
            } else {
                &[][..]
            } {
                let sidecar = format!("{file_name}{suffix}");
                match statat(&self.directory, sidecar.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
                    Err(Errno::NOENT) => {}
                    Ok(_) => {
                        return Err(filesystem_error(
                            "create journal without sidecar collisions",
                            &self.display_path.join(sidecar),
                            "existing SQLite sidecar requires recovery",
                        ));
                    }
                    Err(error) => {
                        return Err(filesystem_error(
                            "inspect journal sidecar",
                            &display_path,
                            error,
                        ));
                    }
                }
            }
            if !create {
                // SQLite must never be the first operation to discover a sidecar link.
                for suffix in ["-wal", "-shm", "-journal"] {
                    let name = format!("{file_name}{suffix}");
                    match statat(&self.directory, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
                        Ok(_) => {
                            self.pin_file(&name, false)?;
                        }
                        Err(Errno::NOENT) => {}
                        Err(error) => {
                            return Err(filesystem_error(
                                "preflight retained SQLite sidecar",
                                &display_path,
                                error,
                            ));
                        }
                    }
                }
            }
            let mut flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
            if create {
                flags |= OFlags::CREATE | OFlags::EXCL;
            }
            let mode = Mode::RUSR.union(Mode::WUSR);
            let descriptor =
                openat(&self.directory, file_name.as_str(), flags, mode).map_err(|error| {
                    filesystem_error("create Rustrace journal file", &display_path, error)
                })?;
            let identity = fstat(&descriptor).map_err(|error| {
                filesystem_error("inspect Rustrace journal file", &display_path, error)
            })?;
            let kind = FileType::from_raw_mode(identity.st_mode);
            if kind != FileType::RegularFile {
                return Err(WorkspaceHashError::UnsupportedFileType {
                    path: display_path,
                    kind: file_type_name(kind),
                });
            }
            fsync(&descriptor).map_err(|error| {
                filesystem_error("sync Rustrace journal file", &display_path, error)
            })?;
            fsync(&self.directory).map_err(|error| {
                filesystem_error("sync Rustrace state directory", &self.display_path, error)
            })?;
            Ok(PinnedJournalFile {
                state_directory: self,
                file_name,
                display_path,
                descriptor,
                identity,
                writer,
                sidecars: Vec::new(),
                reserve: None,
                poison: std::sync::Mutex::new(None),
            })
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (session_id, create);
            Err(WorkspaceHashError::UnsupportedPlatform)
        }
    }
}

/// Ownership for bounded inspection of incomplete startup; never opens SQLite.
pub struct PinnedStateInspection {
    state: PinnedStateDirectory,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    writer: WriterOwnership,
}

impl PinnedStateInspection {
    /// End ownership after all owned inspection/publication has finished.
    /// A failed unlock is returned and leaves this authority unusable.
    pub fn release_ownership(&mut self) -> Result<(), WorkspaceHashError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.writer.release()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Err(WorkspaceHashError::UnsupportedPlatform)
        }
    }

    /// Sync and publish an artifact while retaining cooperative ownership.
    pub fn publish_artifact(
        &self,
        name: &str,
        bytes: &[u8],
        replace: bool,
    ) -> Result<(), WorkspaceHashError> {
        self.state
            .publish_artifact(name, bytes, replace, || self.verify())
    }

    pub fn verify(&self) -> Result<(), WorkspaceHashError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.state.verify()?;
            self.writer.verify(&self.state)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Err(WorkspaceHashError::UnsupportedPlatform)
        }
    }

    pub fn read_artifact(&self, name: &str, maximum: usize) -> Result<Vec<u8>, WorkspaceHashError> {
        self.verify()?;
        let bytes = self.state.read_artifact(name, maximum)?;
        self.verify()?;
        Ok(bytes)
    }

    /// Stream exact regular-file hashes, including partial reserve/publication files.
    /// The writer lock itself is excluded; no original file is modified.
    pub fn inventory(
        &self,
        maximum_bytes: u64,
        maximum_files: usize,
    ) -> Result<Vec<(String, u64, Hash)>, WorkspaceHashError> {
        self.verify()?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let fail = |reason| {
                filesystem_error(
                    "inspect preserved startup",
                    &self.state.display_path,
                    reason,
                )
            };
            let mut result = Vec::new();
            let mut total = 0_u64;
            for entry in
                std::fs::read_dir(&self.state.display_path).map_err(|e| fail(e.to_string()))?
            {
                let entry = entry.map_err(|e| fail(e.to_string()))?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| fail("non-UTF-8 state filename".into()))?;
                if name == "writer.lock" {
                    continue;
                }
                if name.len() > 128
                    || name.chars().any(char::is_control)
                    || result.len() >= maximum_files
                {
                    return Err(fail(
                        "state inventory exceeds bounded inspection; original preserved".into(),
                    ));
                }
                let pinned = self.state.pin_file(&name, false)?;
                let length =
                    u64::try_from(pinned.identity.st_size).map_err(|e| fail(e.to_string()))?;
                total = total
                    .checked_add(length)
                    .ok_or_else(|| fail("state byte count overflow".into()))?;
                if total > maximum_bytes {
                    return Err(fail(
                        "state bytes exceed inspection budget; original preserved".into(),
                    ));
                }
                let mut file =
                    File::from(dup(&pinned.descriptor).map_err(|e| fail(e.to_string()))?);
                let mut reader = file.by_ref().take(length.saturating_add(1));
                let mut hasher = blake3::Hasher::new();
                let mut buffer = [0_u8; 64 * 1024];
                let mut read = 0_u64;
                loop {
                    let count = reader.read(&mut buffer).map_err(|e| fail(e.to_string()))?;
                    if count == 0 {
                        break;
                    }
                    read += count as u64;
                    hasher.update(&buffer[..count]);
                }
                let after = fstat(&pinned.descriptor).map_err(|e| fail(e.to_string()))?;
                pinned.verify(&self.state)?;
                if read != length || !same_snapshot(&pinned.identity, &after) {
                    return Err(fail(
                        "state changed during inspection; original preserved".into(),
                    ));
                }
                result.push((
                    name,
                    length,
                    Hash::from_bytes(*hasher.finalize().as_bytes()),
                ));
            }
            result.sort_by(|a, b| a.0.cmp(&b.0));
            self.verify()?;
            Ok(result)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (maximum_bytes, maximum_files);
            Err(WorkspaceHashError::UnsupportedPlatform)
        }
    }
}

/// Atomically created journal inode retained with its parent directory authority.
pub struct PinnedJournalFile {
    poison: std::sync::Mutex<Option<String>>,
    state_directory: PinnedStateDirectory,
    file_name: String,
    display_path: PathBuf,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    descriptor: OwnedFd,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    identity: Stat,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    writer: WriterOwnership,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    sidecars: Vec<PinnedStateFile>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    reserve: Option<PinnedStateFile>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct PinnedStateFile {
    name: String,
    descriptor: OwnedFd,
    identity: Stat,
}

/// Only a successfully acquired writer lock has this guard. Explicit unlock
/// ends ownership even when an unrelated pre-exec child retains the open file
/// description. Generic state files and failed acquisitions must never unlock.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct WriterOwnership {
    file: PinnedStateFile,
    path: PathBuf,
    released: Option<Result<(), Errno>>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl WriterOwnership {
    fn verify(&self, state: &PinnedStateDirectory) -> Result<(), WorkspaceHashError> {
        if self.released.is_some() {
            return Err(filesystem_error(
                "verify workspace writer ownership",
                &self.path,
                "ownership release was already attempted",
            ));
        }
        self.file.verify(state)
    }

    fn release(&mut self) -> Result<(), WorkspaceHashError> {
        self.release_with(|descriptor| flock(descriptor, FlockOperation::Unlock))
    }

    fn release_with(
        &mut self,
        unlock: impl FnOnce(&OwnedFd) -> Result<(), Errno>,
    ) -> Result<(), WorkspaceHashError> {
        // Record the first outcome: no retries, including from Drop, and no
        // successful completion after a known failure. The descriptor still
        // closes during teardown. Implicit Drop cannot report errors and
        // preserves the existing recovery-on-next-open contract.
        self.released
            .get_or_insert_with(|| unlock(&self.file.descriptor))
            .map_err(|error| {
                filesystem_error("release workspace writer ownership", &self.path, error)
            })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for WriterOwnership {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl PinnedStateFile {
    fn verify(&self, state: &PinnedStateDirectory) -> Result<(), WorkspaceHashError> {
        let path = state.display_path.join(&self.name);
        let linked = statat(
            &state.directory,
            self.name.as_str(),
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|error| filesystem_error("verify controlled state file", &path, error))?;
        let retained = fstat(&self.descriptor)
            .map_err(|error| filesystem_error("inspect retained state file", &path, error))?;
        if !same_identity(&self.identity, &linked) || !same_identity(&self.identity, &retained) {
            return Err(WorkspaceHashError::WorkspaceChanged {
                path,
                change: WorkspaceChange::EntryReplaced,
            });
        }
        Ok(())
    }
}

impl fmt::Debug for PinnedJournalFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PinnedJournalFile")
            .field("display_path", &self.display_path)
            .finish_non_exhaustive()
    }
}

impl PinnedJournalFile {
    /// A close-on-exec duplicate of the held writer-lock descriptor. A command
    /// process that inherits it shares the lock, so the workspace stays locked
    /// until every such process exits, even if the session process dies.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn duplicate_writer_lock(&self) -> std::io::Result<OwnedFd> {
        self.writer.file.descriptor.try_clone()
    }

    /// End ownership without an explicit unlock: the lock lasts until the last
    /// descriptor sharing it closes, so surviving command processes keep it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn keep_writer_lock_for_children(&mut self) {
        self.writer.released.get_or_insert(Ok(()));
    }

    /// End ownership only after every journal worker has drained and joined,
    /// and all owner-dependent persistence and verification have finished.
    /// A failed unlock is returned and leaves this authority unusable.
    pub fn release_ownership(&mut self) -> Result<(), WorkspaceHashError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.writer.release()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Err(WorkspaceHashError::UnsupportedPlatform)
        }
    }

    /// Materialize an incompressible recovery/export reserve, or verify it on resume.
    pub fn secure_reserve(&mut self, bytes: u64, create: bool) -> Result<(), WorkspaceHashError> {
        self.verify()?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use std::io::Write;
            let state = &self.state_directory;
            if create {
                if self.available_storage_bytes()? < bytes.saturating_mul(2) {
                    return Err(filesystem_error(
                        "secure reserve",
                        &self.display_path,
                        "insufficient storage headroom",
                    ));
                }
                let descriptor = openat(
                    &state.directory,
                    "reserve.bin",
                    OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::WRONLY
                        | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                )
                .map_err(|e| filesystem_error("create reserve", &self.display_path, e))?;
                let mut file = File::from(descriptor);
                let mut chunk = vec![0; 1024 * 1024];
                let mut remaining = bytes;
                while remaining > 0 {
                    getrandom::fill(&mut chunk)
                        .map_err(|e| filesystem_error("fill reserve", &self.display_path, e))?;
                    let count = remaining.min(chunk.len() as u64) as usize;
                    file.write_all(&chunk[..count])
                        .map_err(|e| filesystem_error("write reserve", &self.display_path, e))?;
                    remaining -= count as u64;
                }
                file.sync_all()
                    .map_err(|e| filesystem_error("sync reserve", &self.display_path, e))?;
                fsync(&state.directory).map_err(|e| {
                    filesystem_error("sync reserve directory", &self.display_path, e)
                })?;
            }
            let reserve = state.pin_file("reserve.bin", false)?;
            let stat = fstat(&reserve.descriptor)
                .map_err(|e| filesystem_error("inspect reserve", &self.display_path, e))?;
            if stat.st_size < 0 || stat.st_size as u64 != bytes {
                return Err(filesystem_error(
                    "verify reserve size",
                    &self.display_path,
                    "reserve size mismatch",
                ));
            }
            self.reserve = Some(reserve);
            self.verify()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (bytes, create);
            Err(WorkspaceHashError::UnsupportedPlatform)
        }
    }

    /// Read a bounded artifact while retaining journal ownership and identity checks.
    pub fn read_artifact(&self, name: &str, maximum: usize) -> Result<Vec<u8>, WorkspaceHashError> {
        self.verify()?;
        let bytes = self.state_directory.read_artifact(name, maximum)?;
        self.verify()?;
        Ok(bytes)
    }

    /// Sync and publish an artifact while retaining cooperative ownership.
    pub fn publish_artifact(
        &self,
        name: &str,
        bytes: &[u8],
        replace: bool,
    ) -> Result<(), WorkspaceHashError> {
        self.state_directory
            .publish_artifact(name, bytes, replace, || self.verify())
    }

    pub fn available_storage_bytes(&self) -> Result<u64, WorkspaceHashError> {
        self.verify()?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let stat = rustix::fs::fstatvfs(&self.state_directory.directory)
                .map_err(|e| filesystem_error("query storage headroom", &self.display_path, e))?;
            Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Err(WorkspaceHashError::UnsupportedPlatform)
        }
    }

    pub fn display_path(&self) -> &Path {
        &self.display_path
    }

    /// Freeze the two sidecar identities after SQLite's first WAL commit.
    /// SQLite owns their contents; Rustrace only retains their identities.
    pub fn pin_live_sidecars(&mut self) -> Result<(), WorkspaceHashError> {
        self.verify()?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            if self.sidecars.is_empty() {
                let wal = self
                    .state_directory
                    .pin_file(&format!("{}-wal", self.file_name), false)?;
                let shm = self
                    .state_directory
                    .pin_file(&format!("{}-shm", self.file_name), false)?;
                self.sidecars = vec![wal, shm];
            }
        }
        self.verify()
    }

    pub fn verify(&self) -> Result<(), WorkspaceHashError> {
        let mut poison = self
            .poison
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(reason) = poison.as_ref() {
            return Err(filesystem_error(
                "verify poisoned journal authority",
                &self.display_path,
                reason,
            ));
        }
        let result = self.verify_chain();
        if let Err(error) = &result {
            *poison = Some(error.to_string());
        }
        result
    }

    fn verify_chain(&self) -> Result<(), WorkspaceHashError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.state_directory.verify()?;
            self.writer.verify(&self.state_directory)?;
            if let Some(reserve) = &self.reserve {
                reserve.verify(&self.state_directory)?;
            }
            for sidecar in &self.sidecars {
                sidecar.verify(&self.state_directory)?;
            }
            if !self.sidecars.is_empty() {
                let rollback = format!("{}-journal", self.file_name);
                match statat(
                    &self.state_directory.directory,
                    rollback.as_str(),
                    AtFlags::SYMLINK_NOFOLLOW,
                ) {
                    Err(Errno::NOENT) => {}
                    Ok(_) => {
                        return Err(WorkspaceHashError::WorkspaceChanged {
                            path: self.state_directory.display_path.join(rollback),
                            change: WorkspaceChange::EntryReplaced,
                        });
                    }
                    Err(error) => {
                        return Err(filesystem_error(
                            "inspect unexpected rollback journal",
                            &self.display_path,
                            error,
                        ));
                    }
                }
            }
            let linked = statat(
                &self.state_directory.directory,
                self.file_name.as_str(),
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(|error| {
                filesystem_error("verify Rustrace journal file", &self.display_path, error)
            })?;
            let opened = fstat(&self.descriptor).map_err(|error| {
                filesystem_error(
                    "inspect retained Rustrace journal file",
                    &self.display_path,
                    error,
                )
            })?;
            if !same_identity(&self.identity, &linked) || !same_identity(&self.identity, &opened) {
                return Err(WorkspaceHashError::WorkspaceChanged {
                    path: self.display_path.clone(),
                    change: WorkspaceChange::EntryReplaced,
                });
            }
            Ok(())
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Err(WorkspaceHashError::UnsupportedPlatform)
        }
    }
}

/// Hashes validated, canonical workspace paths and their exact bytes.
///
/// Callers supply only entries that belong in the tree. This pure API does not
/// apply filesystem exclusions. Entries are byte-sorted by canonical path and
/// the fixed project limits are always enforced.
pub fn hash_entries<'path, 'contents, I>(entries: I) -> Result<Hash, WorkspaceHashError>
where
    I: IntoIterator<Item = (&'path WorkspacePath, &'contents [u8])>,
{
    let mut collected = Vec::with_capacity(MAX_WORKSPACE_FILES);
    let mut total_bytes = 0_u64;
    for (path, contents) in entries {
        if collected.len() == MAX_WORKSPACE_FILES {
            return Err(WorkspaceHashError::FileCountLimitExceeded {
                attempted: MAX_WORKSPACE_FILES + 1,
                limit: MAX_WORKSPACE_FILES,
            });
        }

        let size = u64::try_from(contents.len()).unwrap_or(u64::MAX);
        if size > MAX_WORKSPACE_FILE_BYTES {
            return Err(WorkspaceHashError::FileSizeLimitExceeded {
                path: path.clone(),
                actual: size,
                limit: MAX_WORKSPACE_FILE_BYTES,
            });
        }
        total_bytes =
            total_bytes
                .checked_add(size)
                .ok_or(WorkspaceHashError::TotalSizeLimitExceeded {
                    attempted: u64::MAX,
                    limit: MAX_WORKSPACE_TOTAL_BYTES,
                })?;
        if total_bytes > MAX_WORKSPACE_TOTAL_BYTES {
            return Err(WorkspaceHashError::TotalSizeLimitExceeded {
                attempted: total_bytes,
                limit: MAX_WORKSPACE_TOTAL_BYTES,
            });
        }

        collected.push(EntryRef { path, contents });
    }

    collected.sort_unstable_by(|left, right| {
        left.path
            .as_str()
            .as_bytes()
            .cmp(right.path.as_str().as_bytes())
    });
    for adjacent in collected.windows(2) {
        if adjacent[0].path == adjacent[1].path {
            return Err(WorkspaceHashError::DuplicatePath {
                path: adjacent[0].path.clone(),
            });
        }
    }

    let mut hasher = blake3::Hasher::new();
    encode_v1(&collected, |bytes| {
        hasher.update(bytes);
    });
    Ok(Hash::from_bytes(*hasher.finalize().as_bytes()))
}

#[derive(Clone, Copy)]
struct EntryRef<'path, 'contents> {
    path: &'path WorkspacePath,
    contents: &'contents [u8],
}

fn encode_v1(entries: &[EntryRef<'_, '_>], mut write: impl FnMut(&[u8])) {
    write(V1_PREFIX);
    let entry_count =
        u32::try_from(entries.len()).expect("workspace file limit fits in the v1 count field");
    write(&entry_count.to_be_bytes());

    for entry in entries {
        let path = entry.path.as_str().as_bytes();
        let path_length =
            u32::try_from(path.len()).expect("WorkspacePath length fits in the v1 path field");
        let content_length = u64::try_from(entry.contents.len())
            .expect("supported platforms have at most 64-bit address spaces");
        write(&path_length.to_be_bytes());
        write(path);
        write(&content_length.to_be_bytes());
        write(entry.contents);
    }
}

/// Safely enumerates and hashes the regular files beneath `root`.
///
/// Directory descriptors and no-follow operations prevent symlink traversal.
/// Any symlink or unsupported filesystem object encountered outside an excluded
/// directory is an error.
pub fn hash_workspace(root: &Path) -> Result<Hash, WorkspaceHashError> {
    let entries = read_workspace(root)?;
    hash_entries(
        entries
            .iter()
            .map(|(path, contents)| (path, contents.as_slice())),
    )
}

/// Safely reads the regular files beneath `root` into canonical path order.
///
/// This applies the same exclusions, fixed resource limits, no-follow traversal,
/// and final namespace validation as [`hash_workspace`].
pub fn read_workspace(root: &Path) -> Result<BTreeMap<WorkspacePath, Vec<u8>>, WorkspaceHashError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let root = PinnedWorkspaceRoot::open(root)?;
        read_pinned_workspace(&root)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = root;
        Err(WorkspaceHashError::UnsupportedPlatform)
    }
}

/// Reads a workspace using a retained root descriptor and verifies that its
/// canonical pathname still names the same directory before and after.
pub fn read_pinned_workspace(
    root: &PinnedWorkspaceRoot,
) -> Result<BTreeMap<WorkspacePath, Vec<u8>>, WorkspaceHashError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut hooks = NoTraversalHooks;
        read_pinned_workspace_with_hooks(root, root.path(), &mut hooks)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = root;
        Err(WorkspaceHashError::UnsupportedPlatform)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[cfg(test)]
fn hash_workspace_with_hooks(
    root: &Path,
    hooks: &mut impl TraversalHooks,
) -> Result<Hash, WorkspaceHashError> {
    let entries = read_workspace_with_hooks(root, hooks)?;
    hash_entries(
        entries
            .iter()
            .map(|(path, contents)| (path, contents.as_slice())),
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[cfg(test)]
fn read_workspace_with_hooks(
    root: &Path,
    hooks: &mut impl TraversalHooks,
) -> Result<BTreeMap<WorkspacePath, Vec<u8>>, WorkspaceHashError> {
    let pinned = PinnedWorkspaceRoot::open(root)?;
    read_pinned_workspace_with_hooks(&pinned, root, hooks)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_pinned_workspace_with_hooks(
    root: &PinnedWorkspaceRoot,
    hook_root: &Path,
    hooks: &mut impl TraversalHooks,
) -> Result<BTreeMap<WorkspacePath, Vec<u8>>, WorkspaceHashError> {
    root.verify_binding()?;
    let canonical_root = root.path();
    let directory = root.directory();
    let before = fstat(directory).map_err(|error| {
        filesystem_error("inspect pinned workspace root", canonical_root, error)
    })?;

    let mut state = TraversalState::default();
    let root_baseline =
        walk_directory(directory, canonical_root, None, &before, &mut state, hooks)?;

    hooks.before_final_validation(hook_root);
    root.verify_binding()?;
    let final_root_stat = fstat(directory).map_err(|error| {
        filesystem_error("reinspect pinned workspace root", canonical_root, error)
    })?;
    if !same_snapshot(&root_baseline, &final_root_stat) {
        return Err(WorkspaceHashError::WorkspaceChanged {
            path: canonical_root.to_owned(),
            change: WorkspaceChange::DirectoryChangedAfterVisit,
        });
    }
    let mut validation = ValidationState::default();
    validate_namespace(
        directory,
        canonical_root,
        None,
        &final_root_stat,
        &state,
        &mut validation,
    )?;
    require_complete_namespace(canonical_root, &state, &validation)?;

    Ok(state
        .entries
        .into_iter()
        .map(|(path, entry)| (path, entry.contents))
        .collect())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Default)]
struct TraversalState {
    entries: BTreeMap<WorkspacePath, StoredFile>,
    directories: BTreeMap<WorkspacePath, Stat>,
    total_bytes: u64,
    traversed_directories: usize,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct StoredFile {
    contents: Vec<u8>,
    baseline: Stat,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Default)]
struct ValidationState {
    files: BTreeSet<WorkspacePath>,
    directories: BTreeSet<WorkspacePath>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
trait TraversalHooks {
    fn before_final_validation(&mut self, _root: &Path) {}

    fn after_inspect_entry(
        &mut self,
        _path: &WorkspacePath,
        _absolute_path: &Path,
        _file_type: FileType,
    ) {
    }

    fn after_read_file(&mut self, _path: &WorkspacePath, _absolute_path: &Path) {}

    fn after_visit_directory(&mut self, _path: &WorkspacePath, _absolute_path: &Path) {}
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct NoTraversalHooks;

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl TraversalHooks for NoTraversalHooks {}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn walk_directory(
    directory: &OwnedFd,
    absolute_directory: &Path,
    relative_directory: Option<&WorkspacePath>,
    before: &Stat,
    state: &mut TraversalState,
    hooks: &mut impl TraversalHooks,
) -> Result<Stat, WorkspaceHashError> {
    state.traversed_directories = state.traversed_directories.checked_add(1).ok_or(
        WorkspaceHashError::DirectoryCountLimitExceeded {
            attempted: usize::MAX,
            limit: MAX_WORKSPACE_DIRECTORIES,
        },
    )?;
    if state.traversed_directories > MAX_WORKSPACE_DIRECTORIES {
        return Err(WorkspaceHashError::DirectoryCountLimitExceeded {
            attempted: state.traversed_directories,
            limit: MAX_WORKSPACE_DIRECTORIES,
        });
    }

    let mut entries = Dir::read_from(directory)
        .map_err(|error| filesystem_error("read workspace directory", absolute_directory, error))?;

    while let Some(entry) = entries.read() {
        let entry = entry.map_err(|error| {
            filesystem_error("read workspace directory entry", absolute_directory, error)
        })?;
        let raw_name = entry.file_name().to_bytes();
        if matches!(raw_name, b"." | b"..") {
            continue;
        }

        let absolute_path = absolute_directory.join(OsStr::from_bytes(raw_name));
        let path = workspace_path(relative_directory, raw_name, &absolute_path)?;
        let stat = statat(directory, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| filesystem_error("inspect workspace entry", &absolute_path, error))?;
        let file_type = FileType::from_raw_mode(stat.st_mode);
        hooks.after_inspect_entry(&path, &absolute_path, file_type);

        match file_type {
            FileType::Symlink => {
                return Err(WorkspaceHashError::Symlink {
                    path: absolute_path,
                });
            }
            FileType::Directory => {
                if is_excluded_directory(&path) {
                    continue;
                }
                let child = openat(
                    directory,
                    entry.file_name(),
                    directory_open_flags(),
                    Mode::empty(),
                )
                .map_err(|error| open_error("open workspace directory", &absolute_path, error))?;
                let opened = fstat(&child).map_err(|error| {
                    filesystem_error("inspect opened workspace directory", &absolute_path, error)
                })?;
                if FileType::from_raw_mode(opened.st_mode) != FileType::Directory
                    || !same_identity(&stat, &opened)
                {
                    return Err(WorkspaceHashError::WorkspaceChanged {
                        path: absolute_path,
                        change: WorkspaceChange::EntryReplaced,
                    });
                }

                walk_directory(&child, &absolute_path, Some(&path), &opened, state, hooks)?;
            }
            FileType::RegularFile => {
                if !is_excluded_file(&path) {
                    add_regular_file(
                        directory,
                        entry.file_name(),
                        absolute_path,
                        path,
                        &stat,
                        state,
                        hooks,
                    )?;
                }
            }
            other => {
                return Err(WorkspaceHashError::UnsupportedFileType {
                    path: absolute_path,
                    kind: file_type_name(other),
                });
            }
        }
    }

    let after = fstat(directory).map_err(|error| {
        filesystem_error("reinspect workspace directory", absolute_directory, error)
    })?;
    if !same_snapshot(before, &after) {
        return Err(WorkspaceHashError::WorkspaceChanged {
            path: absolute_directory.to_owned(),
            change: WorkspaceChange::DirectoryChanged,
        });
    }
    if let Some(path) = relative_directory {
        if state.directories.insert(path.clone(), after).is_some() {
            return Err(WorkspaceHashError::DuplicatePath { path: path.clone() });
        }
        hooks.after_visit_directory(path, absolute_directory);
    }

    Ok(after)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn add_regular_file(
    directory: &OwnedFd,
    name: &std::ffi::CStr,
    absolute_path: PathBuf,
    path: WorkspacePath,
    discovered: &Stat,
    state: &mut TraversalState,
    hooks: &mut impl TraversalHooks,
) -> Result<(), WorkspaceHashError> {
    if state.entries.len() == MAX_WORKSPACE_FILES {
        return Err(WorkspaceHashError::FileCountLimitExceeded {
            attempted: MAX_WORKSPACE_FILES + 1,
            limit: MAX_WORKSPACE_FILES,
        });
    }
    if state.entries.contains_key(&path) {
        return Err(WorkspaceHashError::DuplicatePath { path });
    }

    let descriptor = openat(directory, name, file_open_flags(), Mode::empty())
        .map_err(|error| open_error("open workspace file", &absolute_path, error))?;
    let opened = fstat(&descriptor).map_err(|error| {
        filesystem_error("inspect opened workspace file", &absolute_path, error)
    })?;
    if FileType::from_raw_mode(opened.st_mode) != FileType::RegularFile
        || !same_identity(discovered, &opened)
    {
        return Err(WorkspaceHashError::UnstableFile {
            path,
            change: FileChange::ReplacedBeforeRead,
        });
    }
    if !same_snapshot(discovered, &opened) {
        return Err(WorkspaceHashError::UnstableFile {
            path,
            change: FileChange::ChangedBeforeRead,
        });
    }

    let mut file = File::from(descriptor);
    let (contents, baseline) = read_stable_file(&mut file, &path, &opened, state.total_bytes)?;
    state.total_bytes += contents.len() as u64;
    state
        .entries
        .insert(path.clone(), StoredFile { contents, baseline });
    hooks.after_read_file(&path, &absolute_path);
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_stable_file(
    file: &mut File,
    path: &WorkspacePath,
    before: &Stat,
    prior_total: u64,
) -> Result<(Vec<u8>, Stat), WorkspaceHashError> {
    read_stable_file_with_hook(file, path, before, prior_total, || {})
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_stable_file_with_hook(
    file: &mut File,
    path: &WorkspacePath,
    before: &Stat,
    prior_total: u64,
    before_read: impl FnOnce(),
) -> Result<(Vec<u8>, Stat), WorkspaceHashError> {
    let expected = u64::try_from(before.st_size).map_err(|_| WorkspaceHashError::UnstableFile {
        path: path.clone(),
        change: FileChange::InvalidSize,
    })?;
    require_sizes(path, expected, prior_total)?;

    let capacity = usize::try_from(expected).expect("per-file limit fits in usize");
    let mut contents = Vec::with_capacity(capacity);
    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    before_read();

    loop {
        let remaining = expected - contents.len() as u64;
        let read_limit = if remaining < READ_BUFFER_BYTES as u64 {
            remaining as usize + 1
        } else {
            READ_BUFFER_BYTES
        };
        let read = file.read(&mut buffer[..read_limit]).map_err(|error| {
            WorkspaceHashError::Filesystem {
                operation: "read workspace file",
                path: PathBuf::from(path.as_str()),
                message: error.to_string(),
            }
        })?;
        if read == 0 {
            break;
        }
        if read as u64 > remaining {
            return Err(WorkspaceHashError::UnstableFile {
                path: path.clone(),
                change: FileChange::Grew {
                    expected,
                    actual: expected + 1,
                },
            });
        }
        contents.extend_from_slice(&buffer[..read]);
        let actual = contents.len() as u64;
        require_sizes(path, actual, prior_total)?;
    }

    let actual = contents.len() as u64;
    if actual < expected {
        return Err(WorkspaceHashError::UnstableFile {
            path: path.clone(),
            change: FileChange::Truncated { expected, actual },
        });
    }

    let after = fstat(&*file).map_err(|error| {
        filesystem_error("reinspect workspace file", Path::new(path.as_str()), error)
    })?;
    if !same_snapshot(before, &after) {
        return Err(WorkspaceHashError::UnstableFile {
            path: path.clone(),
            change: FileChange::ChangedDuringRead,
        });
    }

    Ok((contents, after))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn validate_namespace(
    directory: &OwnedFd,
    absolute_directory: &Path,
    relative_directory: Option<&WorkspacePath>,
    before: &Stat,
    expected: &TraversalState,
    validation: &mut ValidationState,
) -> Result<(), WorkspaceHashError> {
    let mut entries = Dir::read_from(directory).map_err(|error| {
        filesystem_error(
            "read workspace directory during final validation",
            absolute_directory,
            error,
        )
    })?;

    while let Some(entry) = entries.read() {
        let entry = entry.map_err(|error| {
            filesystem_error(
                "read workspace directory entry during final validation",
                absolute_directory,
                error,
            )
        })?;
        let raw_name = entry.file_name().to_bytes();
        if matches!(raw_name, b"." | b"..") {
            continue;
        }

        let absolute_path = absolute_directory.join(OsStr::from_bytes(raw_name));
        let path = workspace_path(relative_directory, raw_name, &absolute_path)?;
        let stat =
            statat(directory, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW).map_err(|error| {
                filesystem_error(
                    "inspect workspace entry during final validation",
                    &absolute_path,
                    error,
                )
            })?;
        let file_type = FileType::from_raw_mode(stat.st_mode);

        match file_type {
            FileType::Symlink => {
                return Err(WorkspaceHashError::Symlink {
                    path: absolute_path,
                });
            }
            FileType::Directory => {
                if is_excluded_directory(&path) {
                    continue;
                }
                let baseline = expected
                    .directories
                    .get(&path)
                    .ok_or_else(|| namespace_changed(&absolute_path))?;
                if !same_snapshot(baseline, &stat) {
                    return Err(WorkspaceHashError::WorkspaceChanged {
                        path: absolute_path,
                        change: WorkspaceChange::DirectoryChangedAfterVisit,
                    });
                }
                if !validation.directories.insert(path.clone()) {
                    return Err(WorkspaceHashError::DuplicatePath { path });
                }

                let child = openat(
                    directory,
                    entry.file_name(),
                    directory_open_flags(),
                    Mode::empty(),
                )
                .map_err(|error| {
                    open_error(
                        "open workspace directory during final validation",
                        &absolute_path,
                        error,
                    )
                })?;
                let opened = fstat(&child).map_err(|error| {
                    filesystem_error(
                        "inspect opened workspace directory during final validation",
                        &absolute_path,
                        error,
                    )
                })?;
                if FileType::from_raw_mode(opened.st_mode) != FileType::Directory
                    || !same_snapshot(&stat, &opened)
                {
                    return Err(WorkspaceHashError::WorkspaceChanged {
                        path: absolute_path,
                        change: WorkspaceChange::EntryReplaced,
                    });
                }

                validate_namespace(
                    &child,
                    &absolute_path,
                    Some(&path),
                    &opened,
                    expected,
                    validation,
                )?;
            }
            FileType::RegularFile => {
                if is_excluded_file(&path) {
                    continue;
                }
                let baseline = expected
                    .entries
                    .get(&path)
                    .ok_or_else(|| namespace_changed(&absolute_path))?;
                if !same_snapshot(&baseline.baseline, &stat) {
                    return Err(WorkspaceHashError::UnstableFile {
                        path,
                        change: FileChange::ChangedAfterRead,
                    });
                }
                if !validation.files.insert(path.clone()) {
                    return Err(WorkspaceHashError::DuplicatePath { path });
                }

                let descriptor = openat(
                    directory,
                    entry.file_name(),
                    file_open_flags(),
                    Mode::empty(),
                )
                .map_err(|error| {
                    open_error(
                        "open workspace file during final validation",
                        &absolute_path,
                        error,
                    )
                })?;
                let opened = fstat(&descriptor).map_err(|error| {
                    filesystem_error(
                        "inspect opened workspace file during final validation",
                        &absolute_path,
                        error,
                    )
                })?;
                if FileType::from_raw_mode(opened.st_mode) != FileType::RegularFile
                    || !same_identity(&stat, &opened)
                {
                    return Err(WorkspaceHashError::UnstableFile {
                        path,
                        change: FileChange::ReplacedBeforeRead,
                    });
                }
                if !same_snapshot(&baseline.baseline, &opened) {
                    return Err(WorkspaceHashError::UnstableFile {
                        path,
                        change: FileChange::ChangedAfterRead,
                    });
                }
            }
            other => {
                return Err(WorkspaceHashError::UnsupportedFileType {
                    path: absolute_path,
                    kind: file_type_name(other),
                });
            }
        }
    }

    let after = fstat(directory).map_err(|error| {
        filesystem_error(
            "reinspect workspace directory during final validation",
            absolute_directory,
            error,
        )
    })?;
    if !same_snapshot(before, &after) {
        return Err(WorkspaceHashError::WorkspaceChanged {
            path: absolute_directory.to_owned(),
            change: WorkspaceChange::DirectoryChanged,
        });
    }

    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn require_complete_namespace(
    canonical_root: &Path,
    expected: &TraversalState,
    validation: &ValidationState,
) -> Result<(), WorkspaceHashError> {
    if let Some(path) = expected
        .directories
        .keys()
        .find(|path| !validation.directories.contains(*path))
    {
        return Err(namespace_changed(&canonical_root.join(path.as_str())));
    }
    if let Some(path) = expected
        .entries
        .keys()
        .find(|path| !validation.files.contains(*path))
    {
        return Err(namespace_changed(&canonical_root.join(path.as_str())));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn namespace_changed(path: &Path) -> WorkspaceHashError {
    WorkspaceHashError::WorkspaceChanged {
        path: path.to_owned(),
        change: WorkspaceChange::NamespaceChanged,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn root_binding_changed(path: &Path) -> WorkspaceHashError {
    WorkspaceHashError::WorkspaceChanged {
        path: path.to_owned(),
        change: WorkspaceChange::RootBindingChanged,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn require_sizes(
    path: &WorkspacePath,
    file_bytes: u64,
    prior_total: u64,
) -> Result<(), WorkspaceHashError> {
    if file_bytes > MAX_WORKSPACE_FILE_BYTES {
        return Err(WorkspaceHashError::FileSizeLimitExceeded {
            path: path.clone(),
            actual: file_bytes,
            limit: MAX_WORKSPACE_FILE_BYTES,
        });
    }
    let attempted =
        prior_total
            .checked_add(file_bytes)
            .ok_or(WorkspaceHashError::TotalSizeLimitExceeded {
                attempted: u64::MAX,
                limit: MAX_WORKSPACE_TOTAL_BYTES,
            })?;
    if attempted > MAX_WORKSPACE_TOTAL_BYTES {
        return Err(WorkspaceHashError::TotalSizeLimitExceeded {
            attempted,
            limit: MAX_WORKSPACE_TOTAL_BYTES,
        });
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn workspace_path(
    parent: Option<&WorkspacePath>,
    raw_name: &[u8],
    absolute_path: &Path,
) -> Result<WorkspacePath, WorkspaceHashError> {
    let component =
        std::str::from_utf8(raw_name).map_err(|_| WorkspaceHashError::UnrepresentablePath {
            path: absolute_path.to_owned(),
            reason: "a path component is not valid UTF-8",
        })?;
    if component.contains('\\') {
        return Err(WorkspaceHashError::UnrepresentablePath {
            path: absolute_path.to_owned(),
            reason: "a path component contains `\\`, which is a canonical separator",
        });
    }

    let relative = match parent {
        Some(parent) => format!("{}/{component}", parent.as_str()),
        None => component.to_owned(),
    };
    WorkspacePath::new(relative).map_err(|source| WorkspaceHashError::InvalidPath {
        path: absolute_path.to_owned(),
        source,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_excluded_directory(path: &WorkspacePath) -> bool {
    !path.as_str().contains('/') && matches!(path.as_str(), "target" | ".rustrace")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_excluded_file(path: &WorkspacePath) -> bool {
    let name = path.as_str().rsplit('/').next().unwrap_or(path.as_str());
    matches!(name, ".DS_Store" | "Thumbs.db" | "desktop.ini") || is_editor_temporary(name)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_editor_temporary(name: &str) -> bool {
    name.strip_prefix(".rustrace-editor-")
        .and_then(|value| value.strip_suffix(".tmp"))
        .is_some_and(|nonce| {
            nonce.len() == 32
                && nonce
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn directory_open_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn file_open_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn same_identity(left: &Stat, right: &Stat) -> bool {
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn same_snapshot(left: &Stat, right: &Stat) -> bool {
    same_identity(left, right)
        && left.st_mode == right.st_mode
        && left.st_nlink == right.st_nlink
        && left.st_size == right.st_size
        && left.st_mtime == right.st_mtime
        && left.st_mtime_nsec == right.st_mtime_nsec
        && left.st_ctime == right.st_ctime
        && left.st_ctime_nsec == right.st_ctime_nsec
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_error(operation: &'static str, path: &Path, error: Errno) -> WorkspaceHashError {
    if error == Errno::LOOP {
        WorkspaceHashError::Symlink {
            path: path.to_owned(),
        }
    } else {
        filesystem_error(operation, path, error)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn filesystem_error(
    operation: &'static str,
    path: &Path,
    error: impl fmt::Display,
) -> WorkspaceHashError {
    WorkspaceHashError::Filesystem {
        operation,
        path: path.to_owned(),
        message: error.to_string(),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn file_type_name(file_type: FileType) -> &'static str {
    match file_type {
        FileType::RegularFile => "regular file",
        FileType::Directory => "directory",
        FileType::Symlink => "symlink",
        FileType::Fifo => "FIFO",
        FileType::Socket => "socket",
        FileType::CharacterDevice => "character device",
        FileType::BlockDevice => "block device",
        FileType::Unknown => "unknown filesystem object",
    }
}

/// An obvious file change observed around a bounded read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileChange {
    InvalidSize,
    ReplacedBeforeRead,
    ChangedBeforeRead,
    Grew { expected: u64, actual: u64 },
    Truncated { expected: u64, actual: u64 },
    ChangedDuringRead,
    ChangedAfterRead,
}

impl fmt::Display for FileChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSize => formatter.write_str("reported an invalid negative size"),
            Self::ReplacedBeforeRead => formatter.write_str("was replaced before it was read"),
            Self::ChangedBeforeRead => formatter.write_str("changed before it was read"),
            Self::Grew { expected, actual } => write!(
                formatter,
                "grew while being read (opened at {expected} bytes, read at least {actual})"
            ),
            Self::Truncated { expected, actual } => write!(
                formatter,
                "was truncated while being read (opened at {expected} bytes, read {actual})"
            ),
            Self::ChangedDuringRead => formatter.write_str("changed while it was being read"),
            Self::ChangedAfterRead => formatter.write_str("changed after it was read"),
        }
    }
}

/// An obvious directory-tree change observed during traversal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceChange {
    RootBindingChanged,
    NamespaceChanged,
    EntryReplaced,
    DirectoryChanged,
    DirectoryChangedAfterVisit,
}

impl fmt::Display for WorkspaceChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootBindingChanged => {
                formatter.write_str("the supplied root resolved to a different directory")
            }
            Self::NamespaceChanged => {
                formatter.write_str("the included workspace namespace changed")
            }
            Self::EntryReplaced => formatter.write_str("an entry was replaced during traversal"),
            Self::DirectoryChanged => formatter.write_str("the directory changed during traversal"),
            Self::DirectoryChangedAfterVisit => {
                formatter.write_str("the directory changed after it was visited")
            }
        }
    }
}

#[derive(Debug)]
pub enum WorkspaceHashError {
    UnsupportedPlatform,
    InvalidRoot {
        path: PathBuf,
        source: std::io::Error,
    },
    RootNotDirectory {
        path: PathBuf,
    },
    InvalidPath {
        path: PathBuf,
        source: WorkspacePathError,
    },
    UnrepresentablePath {
        path: PathBuf,
        reason: &'static str,
    },
    DuplicatePath {
        path: WorkspacePath,
    },
    Symlink {
        path: PathBuf,
    },
    UnsupportedFileType {
        path: PathBuf,
        kind: &'static str,
    },
    FileCountLimitExceeded {
        attempted: usize,
        limit: usize,
    },
    DirectoryCountLimitExceeded {
        attempted: usize,
        limit: usize,
    },
    FileSizeLimitExceeded {
        path: WorkspacePath,
        actual: u64,
        limit: u64,
    },
    TotalSizeLimitExceeded {
        attempted: u64,
        limit: u64,
    },
    UnstableFile {
        path: WorkspacePath,
        change: FileChange,
    },
    WorkspaceChanged {
        path: PathBuf,
        change: WorkspaceChange,
    },
    Filesystem {
        operation: &'static str,
        path: PathBuf,
        message: String,
    },
}

impl fmt::Display for WorkspaceHashError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => formatter.write_str(
                "workspace hashing is supported only on Linux, macOS, and Linux-based WSL",
            ),
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
            Self::InvalidPath { path, source } => write!(
                formatter,
                "workspace entry `{}` is not a canonical WorkspacePath: {source}",
                path.display()
            ),
            Self::UnrepresentablePath { path, reason } => write!(
                formatter,
                "workspace entry `{}` cannot be represented canonically: {reason}",
                path.display()
            ),
            Self::DuplicatePath { path } => {
                write!(
                    formatter,
                    "workspace contains duplicate canonical path `{path}`"
                )
            }
            Self::Symlink { path } => write!(
                formatter,
                "workspace entry `{}` is a symlink; symlinks are not supported",
                path.display()
            ),
            Self::UnsupportedFileType { path, kind } => write!(
                formatter,
                "workspace entry `{}` is an unsupported {kind}",
                path.display()
            ),
            Self::FileCountLimitExceeded { attempted, limit } => write!(
                formatter,
                "workspace contains at least {attempted} included files; maximum is {limit}"
            ),
            Self::DirectoryCountLimitExceeded { attempted, limit } => write!(
                formatter,
                "workspace contains at least {attempted} traversed directories; maximum is {limit}"
            ),
            Self::FileSizeLimitExceeded {
                path,
                actual,
                limit,
            } => write!(
                formatter,
                "workspace file `{path}` is {actual} bytes; maximum is {limit} bytes"
            ),
            Self::TotalSizeLimitExceeded { attempted, limit } => write!(
                formatter,
                "workspace included files total at least {attempted} bytes; maximum is {limit} bytes"
            ),
            Self::UnstableFile { path, change } => {
                write!(formatter, "workspace file `{path}` {change}; retry hashing")
            }
            Self::WorkspaceChanged { path, change } => write!(
                formatter,
                "workspace at `{}` changed during hashing ({change}); retry hashing",
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

impl Error for WorkspaceHashError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidRoot { source, .. } => Some(source),
            Self::InvalidPath { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use std::fs::{self, File, OpenOptions, Permissions};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    #[test]
    fn failed_post_lock_verification_releases_a_shared_writer_description() {
        let temp = TempTree::new();
        let root = PinnedWorkspaceRoot::open(temp.path()).unwrap();
        let state = root.open_state_directory().unwrap();
        let linked = temp.path().join(".rustrace/writer.lock");
        let retained = temp.path().join(".rustrace/retained.lock");
        let mut inherited = None;
        let result = state.lock_writer_with(|writer| {
            // Same open-file-description retention as fork, isolated from
            // process timing. Exercise the actual fallible verification path.
            inherited = Some(dup(&writer.descriptor).unwrap());
            fs::rename(&linked, &retained).unwrap();
        });
        assert!(result.is_err());
        fs::rename(&retained, &linked).unwrap();
        let next = state.lock_writer();
        assert!(
            next.is_ok(),
            "failed post-lock verification retained writer ownership"
        );
        drop(inherited);
    }

    #[test]
    fn writer_unlock_failure_is_latched_and_never_reports_clean_release() {
        // Run the descriptor-lifetime control in a process containing no
        // unrelated test threads. This makes the final reacquisition depend
        // only on the references created by the control itself.
        let output = Command::new(std::env::current_exe().unwrap())
            .env("RUSTRACE_WRITER_UNLOCK_CONTROL", "child")
            .args([
                "hash::tests::writer_unlock_failure_is_latched_and_never_reports_clean_release_control",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "isolated writer control failed: {}\n{stdout}\n{stderr}",
            output.status
        );
        assert_eq!(
            stdout
                .lines()
                .filter(|line| *line == "WRITER_UNLOCK_CONTROL_COMPLETED")
                .count(),
            1,
            "isolated writer control did not execute exactly once:\n{stdout}\n{stderr}"
        );
    }

    #[test]
    #[ignore = "invoked only by the isolated writer unlock parent test"]
    fn writer_unlock_failure_is_latched_and_never_reports_clean_release_control() {
        assert_eq!(
            std::env::var("RUSTRACE_WRITER_UNLOCK_CONTROL").as_deref(),
            Ok("child"),
            "writer lifetime control must run only in its owned child process"
        );
        let temp = TempTree::new();
        let root = PinnedWorkspaceRoot::open(temp.path()).unwrap();
        let state = root.open_state_directory().unwrap();
        let mut writer = state.lock_writer().unwrap();
        let inherited = dup(&writer.file.descriptor).unwrap();
        let error = writer.release_with(|_| Err(Errno::IO)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("release workspace writer ownership")
        );
        assert!(writer.verify(&state).is_err());
        assert!(
            writer
                .release_with(|_| panic!("must not retry unlock"))
                .is_err()
        );
        assert!(state.lock_writer().is_err(), "injected unlock did not run");
        drop(writer);
        // The known failure is latched; Drop cannot report or retry it. The
        // retained duplicate models an inherited pre-exec reference, so the
        // lock remains held until that reference itself retires.
        assert!(state.lock_writer().is_err());
        drop(inherited);
        state.lock_writer().unwrap();
        println!("WRITER_UNLOCK_CONTROL_COMPLETED");
    }

    #[test]
    fn explicit_writer_release_invalidates_retained_authority() {
        let temp = TempTree::new();
        let root = PinnedWorkspaceRoot::open(temp.path()).unwrap();
        let mut owner = root
            .open_state_directory()
            .unwrap()
            .lock_for_inspection()
            .unwrap();
        let inherited = dup(&owner.writer.file.descriptor).unwrap();
        owner.release_ownership().unwrap();
        assert!(owner.verify().is_err());
        assert!(owner.publish_artifact("forbidden", b"no", false).is_err());
        let next = root
            .open_state_directory()
            .unwrap()
            .lock_for_inspection()
            .unwrap();
        // Retiring an already released owner must not disturb the new owner.
        drop(owner);
        drop(inherited);
        next.verify().unwrap();
        assert!(
            root.open_state_directory()
                .unwrap()
                .lock_for_inspection()
                .is_err()
        );
    }

    #[test]
    fn regular_file_open_flags_are_nonblocking_and_never_acquire_a_tty() {
        let flags = file_open_flags();
        assert!(flags.contains(OFlags::NONBLOCK));
        assert!(flags.contains(OFlags::NOCTTY));
    }

    #[test]
    fn final_sweep_rejects_mutation_of_an_already_read_file() {
        let temp = TempTree::new();
        fs::write(temp.path().join("first.rs"), b"old").unwrap();
        fs::write(temp.path().join("second.rs"), b"old").unwrap();
        let mut hooks = MutateEarlierFile::default();

        let error = hash_workspace_with_hooks(temp.path(), &mut hooks).unwrap_err();

        assert!(hooks.mutated);
        assert!(matches!(
            error,
            WorkspaceHashError::UnstableFile {
                change: FileChange::ChangedAfterRead,
                ..
            }
        ));
    }

    #[test]
    fn final_sweep_rejects_replacement_in_an_already_visited_directory() {
        let temp = TempTree::new();
        fs::create_dir(temp.path().join("one")).unwrap();
        fs::create_dir(temp.path().join("two")).unwrap();
        fs::write(temp.path().join("one/file.rs"), b"old").unwrap();
        fs::write(temp.path().join("two/file.rs"), b"old").unwrap();
        let mut hooks = ReplaceInEarlierDirectory::default();

        let error = hash_workspace_with_hooks(temp.path(), &mut hooks).unwrap_err();

        assert!(hooks.mutated);
        assert!(matches!(
            error,
            WorkspaceHashError::WorkspaceChanged {
                change: WorkspaceChange::DirectoryChangedAfterVisit,
                ..
            }
        ));
    }

    #[test]
    fn regular_to_fifo_swap_before_open_fails_without_blocking() {
        let flags = file_open_flags();
        assert!(flags.contains(OFlags::NONBLOCK));
        assert!(flags.contains(OFlags::NOCTTY));

        let temp = TempTree::new();
        fs::write(temp.path().join("victim.rs"), b"regular").unwrap();
        let mut hooks = ReplaceRegularWithFifo::default();

        let error = hash_workspace_with_hooks(temp.path(), &mut hooks).unwrap_err();

        assert!(hooks.mutated);
        assert!(matches!(
            error,
            WorkspaceHashError::UnstableFile {
                change: FileChange::ReplacedBeforeRead,
                ..
            }
        ));
    }

    #[test]
    fn final_validation_rejects_root_symlink_retargeting() {
        let temp = TempTree::new();
        let first = temp.path().join("first-target");
        let second = temp.path().join("second-target");
        let root = temp.path().join("workspace-link");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        fs::write(first.join("file.rs"), b"same").unwrap();
        fs::write(second.join("file.rs"), b"same").unwrap();
        std::os::unix::fs::symlink(&first, &root).unwrap();
        let mut hooks = RetargetRootSymlink {
            new_target: second,
            mutated: false,
        };

        let error = hash_workspace_with_hooks(&root, &mut hooks).unwrap_err();

        assert!(hooks.mutated);
        assert!(matches!(
            error,
            WorkspaceHashError::WorkspaceChanged {
                change: WorkspaceChange::RootBindingChanged,
                ..
            }
        ));
    }

    #[test]
    fn final_validation_rejects_direct_root_replacement() {
        let temp = TempTree::new();
        let root = temp.path().join("workspace");
        let retired = temp.path().join("retired-workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file.rs"), b"same").unwrap();
        let mut hooks = ReplaceDirectRoot {
            retired,
            mutated: false,
        };

        let error = hash_workspace_with_hooks(&root, &mut hooks).unwrap_err();

        assert!(hooks.mutated);
        assert!(matches!(
            error,
            WorkspaceHashError::WorkspaceChanged {
                change: WorkspaceChange::RootBindingChanged,
                ..
            }
        ));
    }

    #[test]
    fn final_validation_rejects_included_namespace_additions_and_deletions() {
        for mutation in [
            NamespaceMutation::AddFile,
            NamespaceMutation::DeleteFile,
            NamespaceMutation::AddDirectory,
            NamespaceMutation::DeleteDirectory,
        ] {
            let temp = TempTree::new();
            let nested = temp.path().join("nested");
            fs::create_dir(&nested).unwrap();
            fs::create_dir(nested.join("empty")).unwrap();
            fs::write(nested.join("existing.rs"), b"same").unwrap();
            let mut hooks = MutateNamespace {
                mutation,
                mutated: false,
            };

            let error = hash_workspace_with_hooks(temp.path(), &mut hooks).unwrap_err();

            assert!(hooks.mutated);
            assert!(matches!(error, WorkspaceHashError::WorkspaceChanged { .. }));
        }
    }

    #[derive(Default)]
    struct MutateEarlierFile {
        first_read: Option<PathBuf>,
        mutated: bool,
    }

    impl TraversalHooks for MutateEarlierFile {
        fn after_inspect_entry(
            &mut self,
            _path: &WorkspacePath,
            absolute_path: &Path,
            _file_type: FileType,
        ) {
            let Some(first_read) = self.first_read.as_ref() else {
                return;
            };
            if !self.mutated && absolute_path != first_read {
                fs::write(first_read, b"new").unwrap();
                self.mutated = true;
            }
        }

        fn after_read_file(&mut self, _path: &WorkspacePath, absolute_path: &Path) {
            if self.first_read.is_none() {
                self.first_read = Some(absolute_path.to_owned());
            }
        }
    }

    #[derive(Default)]
    struct ReplaceInEarlierDirectory {
        first_visited: Option<PathBuf>,
        mutated: bool,
    }

    impl TraversalHooks for ReplaceInEarlierDirectory {
        fn after_inspect_entry(
            &mut self,
            _path: &WorkspacePath,
            absolute_path: &Path,
            _file_type: FileType,
        ) {
            let Some(first_visited) = self.first_visited.as_ref() else {
                return;
            };
            if !self.mutated && !absolute_path.starts_with(first_visited) {
                let file = first_visited.join("file.rs");
                fs::remove_file(&file).unwrap();
                fs::write(file, b"new").unwrap();
                self.mutated = true;
            }
        }

        fn after_visit_directory(&mut self, _path: &WorkspacePath, absolute_path: &Path) {
            if self.first_visited.is_none() {
                self.first_visited = Some(absolute_path.to_owned());
            }
        }
    }

    #[derive(Default)]
    struct ReplaceRegularWithFifo {
        mutated: bool,
    }

    struct RetargetRootSymlink {
        new_target: PathBuf,
        mutated: bool,
    }

    impl TraversalHooks for RetargetRootSymlink {
        fn before_final_validation(&mut self, root: &Path) {
            fs::remove_file(root).unwrap();
            std::os::unix::fs::symlink(&self.new_target, root).unwrap();
            self.mutated = true;
        }
    }

    struct ReplaceDirectRoot {
        retired: PathBuf,
        mutated: bool,
    }

    #[derive(Clone, Copy)]
    enum NamespaceMutation {
        AddFile,
        DeleteFile,
        AddDirectory,
        DeleteDirectory,
    }

    struct MutateNamespace {
        mutation: NamespaceMutation,
        mutated: bool,
    }

    impl TraversalHooks for MutateNamespace {
        fn before_final_validation(&mut self, root: &Path) {
            let nested = root.join("nested");
            match self.mutation {
                NamespaceMutation::AddFile => {
                    fs::write(nested.join("added.rs"), b"same").unwrap();
                }
                NamespaceMutation::DeleteFile => {
                    fs::remove_file(nested.join("existing.rs")).unwrap();
                }
                NamespaceMutation::AddDirectory => {
                    fs::create_dir(nested.join("added")).unwrap();
                }
                NamespaceMutation::DeleteDirectory => {
                    fs::remove_dir(nested.join("empty")).unwrap();
                }
            }
            self.mutated = true;
        }
    }

    impl TraversalHooks for ReplaceDirectRoot {
        fn before_final_validation(&mut self, root: &Path) {
            fs::rename(root, &self.retired).unwrap();
            fs::create_dir(root).unwrap();
            fs::write(root.join("file.rs"), b"same").unwrap();
            self.mutated = true;
        }
    }

    impl TraversalHooks for ReplaceRegularWithFifo {
        fn after_inspect_entry(
            &mut self,
            path: &WorkspacePath,
            absolute_path: &Path,
            file_type: FileType,
        ) {
            if !self.mutated && path.as_str() == "victim.rs" && file_type == FileType::RegularFile {
                fs::remove_file(absolute_path).unwrap();
                let status = Command::new("mkfifo").arg(absolute_path).status().unwrap();
                assert!(status.success());
                self.mutated = true;
            }
        }
    }

    #[test]
    fn detects_growth_truncation_and_same_length_change() {
        let temp = TempFile::new(b"abc");
        let path = WorkspacePath::new("file.rs").unwrap();

        let (mut file, before) = temp.open_snapshot();
        let error = read_stable_file_with_hook(&mut file, &path, &before, 0, || {
            OpenOptions::new()
                .append(true)
                .open(temp.path())
                .unwrap()
                .write_all(b"d")
                .unwrap();
        })
        .unwrap_err();
        assert!(matches!(
            error,
            WorkspaceHashError::UnstableFile {
                change: FileChange::Grew { .. },
                ..
            }
        ));

        fs::write(temp.path(), b"abc").unwrap();
        let (mut file, before) = temp.open_snapshot();
        let error = read_stable_file_with_hook(&mut file, &path, &before, 0, || {
            OpenOptions::new()
                .write(true)
                .open(temp.path())
                .unwrap()
                .set_len(1)
                .unwrap();
        })
        .unwrap_err();
        assert!(matches!(
            error,
            WorkspaceHashError::UnstableFile {
                change: FileChange::Truncated { .. },
                ..
            }
        ));

        fs::write(temp.path(), b"abc").unwrap();
        fs::set_permissions(temp.path(), Permissions::from_mode(0o644)).unwrap();
        let (mut file, before) = temp.open_snapshot();
        let error = read_stable_file_with_hook(&mut file, &path, &before, 0, || {
            fs::write(temp.path(), b"xyz").unwrap();
            fs::set_permissions(temp.path(), Permissions::from_mode(0o600)).unwrap();
        })
        .unwrap_err();
        assert!(matches!(
            error,
            WorkspaceHashError::UnstableFile {
                change: FileChange::ChangedDuringRead,
                ..
            }
        ));
    }

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TempTree(PathBuf);

    impl TempTree {
        fn new() -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rustrace-t1-3-snapshot-test-{}-{id}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                panic!("failed to remove {}: {error}", self.0.display());
            }
        }
    }

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(contents: &[u8]) -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rustrace-t1-3-mutation-test-{}-{id}",
                std::process::id()
            ));
            fs::write(&path, contents).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn open_snapshot(&self) -> (File, Stat) {
            let descriptor = open(self.path(), file_open_flags(), Mode::empty()).unwrap();
            let snapshot = fstat(&descriptor).unwrap();
            (File::from(descriptor), snapshot)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_file(&self.0)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                panic!("failed to remove {}: {error}", self.0.display());
            }
        }
    }
}

fn validate_artifact_name(name: &str) -> Result<(), WorkspaceHashError> {
    if name == "writer.lock"
        || name == "reserve.bin"
        || name.ends_with(".sqlite")
        || name.ends_with("-wal")
        || name.ends_with("-shm")
        || name.ends_with("-journal")
        || name.starts_with(".artifact-")
        || name.is_empty()
        || name.len() > 128
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(filesystem_error(
            "validate artifact name",
            Path::new(name),
            "expected one bounded ASCII filename",
        ));
    }
    Ok(())
}
