//! Descriptor-relative workspace file mutations.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustrace_model::WorkspacePath;

use crate::hash::{
    MAX_WORKSPACE_DIRECTORIES, MAX_WORKSPACE_FILE_BYTES, MAX_WORKSPACE_FILES, PinnedWorkspaceRoot,
    WorkspaceHashError,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::fs::File;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::Write;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::fd::OwnedFd;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, RenameFlags, Stat, fstat, fsync, openat, renameat_with,
    statat, unlinkat,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::io::Errno;

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
const FILE_OPEN_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::NOCTTY)
    .union(OFlags::CLOEXEC);
#[cfg(any(target_os = "linux", target_os = "macos"))]
const FILE_WRITE_OPEN_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::NOCTTY)
    .union(OFlags::CLOEXEC);
#[cfg(any(target_os = "linux", target_os = "macos"))]
const MAX_EXTERNAL_DIRECTORY_ENTRIES: usize = MAX_WORKSPACE_DIRECTORIES + MAX_WORKSPACE_FILES;
#[cfg(any(target_os = "linux", target_os = "macos"))]
const FILE_MODE: Mode = Mode::RUSR
    .union(Mode::WUSR)
    .union(Mode::RGRP)
    .union(Mode::ROTH);
#[cfg(any(target_os = "linux", target_os = "macos"))]
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

/// Descriptor identity retained with an already-open ordinary external file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegularFileIdentity {
    device: u64,
    inode: u64,
}

/// An already-open, single-link ordinary file beneath a pinned root.
#[derive(Debug)]
pub struct OpenedRegularFile {
    file: File,
    identity: RegularFileIdentity,
}

impl OpenedRegularFile {
    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    pub fn identity(&self) -> RegularFileIdentity {
        self.identity
    }

    pub fn into_file(self) -> File {
        self.file
    }
}

/// Opens an existing ordinary file read-only without following any component.
pub fn open_external_regular_file_read_in(
    root: &PinnedWorkspaceRoot,
    path: &WorkspacePath,
) -> Result<OpenedRegularFile, WorkspaceMutationError> {
    open_external_regular_file_in(root, path, FILE_OPEN_FLAGS, "open external input file")
}

/// Opens an existing ordinary file for writing without truncating it.
pub fn open_external_regular_file_write_in(
    root: &PinnedWorkspaceRoot,
    path: &WorkspacePath,
) -> Result<OpenedRegularFile, WorkspaceMutationError> {
    open_external_regular_file_in(
        root,
        path,
        FILE_WRITE_OPEN_FLAGS,
        "open external output file",
    )
}

/// Checks whether an output path is an existing unaliased ordinary file.
/// Missing leaves are accepted only beneath an already-open safe parent.
pub fn external_regular_file_exists_in(
    root: &PinnedWorkspaceRoot,
    path: &WorkspacePath,
) -> Result<bool, WorkspaceMutationError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        verify_mutation_root(root)?;
        let opened = OpenedRoot::from_pinned(root);
        let parents = open_parent_chain(&opened, path)?;
        let parent = directory_chain_leaf(opened.directory, &parents);
        match statat(parent, file_name(path), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => {
                require_unaliased_regular(path, &stat)?;
                verify_mutation_root(root)?;
                Ok(true)
            }
            Err(Errno::NOENT) => {
                verify_mutation_root(root)?;
                Ok(false)
            }
            Err(error) => Err(filesystem_error(
                "inspect external output path",
                opened.absolute(path),
                error,
            )),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (root, path);
        Err(WorkspaceMutationError::UnsupportedPlatform)
    }
}

/// Creates one new ordinary file without replacing an existing entry.
pub fn create_external_regular_file_in(
    root: &PinnedWorkspaceRoot,
    path: &WorkspacePath,
) -> Result<OpenedRegularFile, WorkspaceMutationError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        verify_mutation_root(root)?;
        let opened = OpenedRoot::from_pinned(root);
        let parents = open_parent_chain(&opened, path)?;
        let parent = directory_chain_leaf(opened.directory, &parents);
        let absolute = opened.absolute(path);
        let descriptor =
            openat(parent, file_name(path), FILE_CREATE_FLAGS, FILE_MODE).map_err(|error| {
                match error {
                    Errno::EXIST => WorkspaceMutationError::PathCollision { path: path.clone() },
                    Errno::LOOP => WorkspaceMutationError::Symlink { path: path.clone() },
                    _ => filesystem_error("create external output file", &absolute, error),
                }
            })?;
        let stat = fstat(&descriptor)
            .map_err(|error| filesystem_error("inspect external output file", &absolute, error))?;
        require_unaliased_regular(path, &stat)?;
        verify_mutation_root(root)?;
        Ok(opened_regular_file(descriptor, &stat))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (root, path);
        Err(WorkspaceMutationError::UnsupportedPlatform)
    }
}

/// Removes a regular file only while it is still the file created by the caller.
///
/// This is intended for rollback after [`create_external_regular_file_in`]. A
/// missing or replaced path is left untouched and returns `Ok(false)`.
pub fn remove_created_external_regular_file_in(
    root: &PinnedWorkspaceRoot,
    path: &WorkspacePath,
    expected: RegularFileIdentity,
) -> Result<bool, WorkspaceMutationError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        verify_mutation_root(root)?;
        let opened = OpenedRoot::from_pinned(root);
        let parents = open_parent_chain(&opened, path)?;
        let parent = directory_chain_leaf(opened.directory, &parents);
        let name = file_name(path);
        let absolute = opened.absolute(path);
        let (_source, discovered) = match open_regular_target(parent, name, path, &absolute) {
            Ok(opened) => opened,
            Err(WorkspaceMutationError::MissingTarget { .. }) => return Ok(false),
            Err(error) => return Err(error),
        };
        if regular_file_identity(&discovered) != expected {
            return Ok(false);
        }

        let temp_name = allocate_temporary_name(parent, &absolute)?;
        renameat_with(
            parent,
            name,
            parent,
            temp_name.as_str(),
            RenameFlags::NOREPLACE,
        )
        .map_err(|error| match error {
            Errno::EXIST | Errno::NOENT => {
                WorkspaceMutationError::PathChanged { path: path.clone() }
            }
            Errno::NOSYS | Errno::NOTSUP | Errno::OPNOTSUPP | Errno::INVAL => {
                WorkspaceMutationError::AtomicRemoveUnavailable { path: path.clone() }
            }
            _ => filesystem_error(
                "capture created external file for removal",
                &absolute,
                error,
            ),
        })?;

        let captured = inspect_regular_target(parent, temp_name.as_str(), path, &absolute)?;
        if !same_identity(&discovered, &captured) || regular_file_identity(&captured) != expected {
            restore_rename(parent, temp_name.as_str(), parent, name, path, &absolute)?;
            return Ok(false);
        }
        unlinkat(parent, temp_name.as_str(), AtFlags::empty()).map_err(|error| {
            repair_error("commit created external file removal", &absolute, error)
        })?;
        fsync(parent)
            .map_err(|error| filesystem_error("sync external file parent", &absolute, error))?;
        verify_mutation_root(root)?;
        Ok(true)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (root, path, expected);
        Err(WorkspaceMutationError::UnsupportedPlatform)
    }
}

/// Lists only bounded, single-link ordinary paths. It never opens file content.
pub fn list_external_regular_files_in(
    root: &PinnedWorkspaceRoot,
) -> Result<Vec<WorkspacePath>, WorkspaceMutationError> {
    list_external_regular_files_in_with_limit(root, MAX_WORKSPACE_FILES)
}

/// Lists ordinary external paths with a caller-owned bounded file allowance.
pub fn list_external_regular_files_in_with_limit(
    root: &PinnedWorkspaceRoot,
    maximum_files: usize,
) -> Result<Vec<WorkspacePath>, WorkspaceMutationError> {
    list_external_regular_files_in_with_filter(root, maximum_files, |_| true)
}

/// Lists a bounded projection of ordinary external paths while still scanning
/// no more than the fixed external-directory entry allowance.
pub fn list_external_regular_files_in_with_filter<F>(
    root: &PinnedWorkspaceRoot,
    maximum_files: usize,
    include: F,
) -> Result<Vec<WorkspacePath>, WorkspaceMutationError>
where
    F: Fn(&WorkspacePath) -> bool,
{
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let maximum_files = maximum_files.min(MAX_EXTERNAL_DIRECTORY_ENTRIES);
        verify_mutation_root(root)?;
        let mut paths = Vec::new();
        let mut directories = 0_usize;
        let mut entries = 0_usize;
        list_external_directory(
            root.directory(),
            root.path(),
            None,
            &mut paths,
            &mut directories,
            &mut entries,
            maximum_files,
            &include,
        )?;
        paths.sort();
        verify_mutation_root(root)?;
        Ok(paths)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (root, maximum_files, include);
        Err(WorkspaceMutationError::UnsupportedPlatform)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
trait MutationHooks {
    fn publish_rename(
        &mut self,
        old_parent: &OwnedFd,
        old_name: &str,
        new_parent: &OwnedFd,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<(), Errno> {
        renameat_with(old_parent, old_name, new_parent, new_name, flags)
    }

    fn sync_created(&mut self, descriptor: &OwnedFd, _phase: &'static str) -> Result<(), Errno> {
        fsync(descriptor)
    }

    fn before_leaf_commit(
        &mut self,
        _operation: &'static str,
        _path: &WorkspacePath,
        _absolute: &Path,
    ) {
    }

    fn before_leaf_repair(
        &mut self,
        _operation: &'static str,
        _path: &WorkspacePath,
        _absolute: &Path,
    ) {
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct NoMutationHooks;

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl MutationHooks for NoMutationHooks {}

/// Creates an empty regular file without replacing an existing entry.
///
/// Every existing parent is opened relative to the canonical workspace root
/// with no-follow semantics. Parent directories are not created implicitly.
pub fn create_workspace_file(
    root: &Path,
    path: &WorkspacePath,
) -> Result<(), WorkspaceMutationError> {
    let root = PinnedWorkspaceRoot::open(root).map_err(map_root_error)?;
    create_workspace_file_in(&root, path)
}

/// Creates a file relative to a retained workspace-root authority.
///
/// Callers recording intent first should use `preflight_workspace_destination_in`
/// before recording. Publication still checks independently and never clobbers.
pub fn create_workspace_file_in(
    root: &PinnedWorkspaceRoot,
    path: &WorkspacePath,
) -> Result<(), WorkspaceMutationError> {
    create_workspace_file_in_with_hooks(root, path, &mut NoMutationHooks)
}

fn create_workspace_file_in_with_hooks(
    root: &PinnedWorkspaceRoot,
    path: &WorkspacePath,
    hooks: &mut impl MutationHooks,
) -> Result<(), WorkspaceMutationError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        verify_mutation_root(root)?;
        let root = OpenedRoot::from_pinned(root);
        let parents = open_parent_chain(&root, path)?;
        let parent = directory_chain_leaf(root.directory, &parents);
        let name = file_name(path);
        let absolute = root.absolute(path);

        match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if file_type(&stat) == FileType::Symlink => {
                return Err(WorkspaceMutationError::Symlink { path: path.clone() });
            }
            Ok(_) => {
                return Err(WorkspaceMutationError::PathCollision { path: path.clone() });
            }
            Err(Errno::NOENT) => {}
            Err(error) => return Err(filesystem_error("inspect create target", absolute, error)),
        }

        let created =
            openat(parent, name, FILE_CREATE_FLAGS, FILE_MODE).map_err(|error| match error {
                Errno::EXIST => WorkspaceMutationError::PathCollision { path: path.clone() },
                Errno::LOOP => WorkspaceMutationError::Symlink { path: path.clone() },
                _ => filesystem_error("create workspace file", &absolute, error),
            })?;
        let retained = fstat(&created)
            .map_err(|error| filesystem_error("inspect created descriptor", &absolute, error))?;
        hooks
            .sync_created(&created, "file")
            .map_err(|error| filesystem_error("sync created file", &absolute, error))?;
        hooks
            .sync_created(parent, "parent")
            .map_err(|error| filesystem_error("sync created parent", &absolute, error))?;
        let published = inspect_regular_target(parent, name, path, &absolute)?;
        if !same_identity(&retained, &published) {
            return Err(WorkspaceMutationError::PathChanged { path: path.clone() });
        }
        verify_mutation_root(root.pinned)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (root, path);
        Err(WorkspaceMutationError::UnsupportedPlatform)
    }
}

/// Checks existing no-follow parents and an absent destination without mutation.
/// Host-equivalent names are checked by the filesystem, not string folding.
/// This is preflight only: it does not reserve the name against later changes.
pub fn preflight_workspace_destination_in(
    root: &PinnedWorkspaceRoot,
    path: &WorkspacePath,
) -> Result<(), WorkspaceMutationError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        verify_mutation_root(root)?;
        let opened = OpenedRoot::from_pinned(root);
        let parents = open_parent_chain(&opened, path)?;
        let parent = directory_chain_leaf(opened.directory, &parents);
        match statat(parent, file_name(path), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if file_type(&stat) == FileType::Symlink => {
                return Err(WorkspaceMutationError::Symlink { path: path.clone() });
            }
            Ok(_) => return Err(WorkspaceMutationError::PathCollision { path: path.clone() }),
            Err(Errno::NOENT) => {}
            Err(error) => {
                return Err(filesystem_error(
                    "preflight destination",
                    opened.absolute(path),
                    error,
                ));
            }
        }
        verify_mutation_root(root)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (root, path);
        Err(WorkspaceMutationError::UnsupportedPlatform)
    }
}

/// Atomically replaces an existing regular workspace file.
pub fn write_workspace_file(
    root: &Path,
    path: &WorkspacePath,
    contents: &[u8],
) -> Result<(), WorkspaceMutationError> {
    let root = PinnedWorkspaceRoot::open(root).map_err(map_root_error)?;
    write_workspace_file_in(&root, path, contents)
}

/// Atomically replaces a file relative to a retained workspace-root authority.
pub fn write_workspace_file_in(
    root: &PinnedWorkspaceRoot,
    path: &WorkspacePath,
    contents: &[u8],
) -> Result<(), WorkspaceMutationError> {
    let mut hooks = NoMutationHooks;
    write_workspace_file_in_with_hooks(root, path, contents, &mut hooks)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_workspace_file_in_with_hooks(
    root: &PinnedWorkspaceRoot,
    path: &WorkspacePath,
    contents: &[u8],
    hooks: &mut impl MutationHooks,
) -> Result<(), WorkspaceMutationError> {
    let actual = u64::try_from(contents.len()).unwrap_or(u64::MAX);
    if actual > MAX_WORKSPACE_FILE_BYTES {
        return Err(WorkspaceMutationError::FileSizeLimitExceeded {
            path: path.clone(),
            actual,
            limit: MAX_WORKSPACE_FILE_BYTES,
        });
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        verify_mutation_root(root)?;
        let root = OpenedRoot::from_pinned(root);
        let parents = open_parent_chain(&root, path)?;
        let parent = directory_chain_leaf(root.directory, &parents);
        let name = file_name(path);
        let absolute = root.absolute(path);
        let (_source, discovered) = open_regular_target(parent, name, path, &absolute)?;
        let (temp_name, descriptor) = create_temporary_file(parent, &absolute)?;
        let mut file = File::from(descriptor);
        if let Err(source) = file.write_all(contents).and_then(|()| file.sync_all()) {
            let _ = unlinkat(parent, temp_name.as_str(), AtFlags::empty());
            return Err(WorkspaceMutationError::Io {
                operation: "write temporary workspace file",
                path: absolute,
                source,
            });
        }
        let replacement = fstat(&file).map_err(|error| {
            let cleanup = unlinkat(parent, temp_name.as_str(), AtFlags::empty());
            repair_or_filesystem_error(
                cleanup,
                "inspect temporary workspace file",
                &absolute,
                error,
            )
        })?;
        hooks.before_leaf_commit("write", path, &absolute);
        if let Err(error) = hooks.publish_rename(
            parent,
            temp_name.as_str(),
            parent,
            name,
            RenameFlags::EXCHANGE,
        ) {
            return match unlinkat(parent, temp_name.as_str(), AtFlags::empty()) {
                Ok(()) => Err(match error {
                    Errno::NOSYS | Errno::NOTSUP | Errno::OPNOTSUPP | Errno::INVAL => {
                        WorkspaceMutationError::AtomicReplaceUnavailable { path: path.clone() }
                    }
                    Errno::NOENT => WorkspaceMutationError::PathChanged { path: path.clone() },
                    _ => filesystem_error("exchange workspace file", absolute, error),
                }),
                Err(cleanup) => Err(repair_error(
                    "clean up an uncommitted replacement",
                    absolute,
                    cleanup,
                )),
            };
        }

        let swapped_out = inspect_regular_target(parent, temp_name.as_str(), path, &absolute)?;
        let installed = inspect_regular_target(parent, name, path, &absolute)?;
        if !same_identity(&replacement, &installed) {
            return Err(repair_error(
                "verify the installed replacement",
                absolute,
                "the destination changed after the atomic exchange",
            ));
        }
        if !same_identity(&discovered, &swapped_out) {
            restore_exchange(parent, name, temp_name.as_str(), path, &absolute)?;
            drop(file);
            unlinkat(parent, temp_name.as_str(), AtFlags::empty()).map_err(|error| {
                repair_error("clean up a rejected replacement", &absolute, error)
            })?;
            fsync(parent).map_err(|error| {
                repair_error("sync a rejected replacement repair", &absolute, error)
            })?;
            return Err(WorkspaceMutationError::PathChanged { path: path.clone() });
        }

        drop(file);
        unlinkat(parent, temp_name.as_str(), AtFlags::empty()).map_err(|error| {
            repair_error("remove the replaced workspace file", &absolute, error)
        })?;
        fsync(parent)
            .map_err(|error| filesystem_error("sync workspace parent", &absolute, error))?;
        verify_mutation_root(root.pinned)?;
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (root, path, contents);
        Err(WorkspaceMutationError::UnsupportedPlatform)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn create_temporary_file(
    parent: &OwnedFd,
    target: &Path,
) -> Result<(String, OwnedFd), WorkspaceMutationError> {
    for _ in 0..16 {
        let nonce = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            ".rustrace-editor-{:016x}{nonce:016x}.tmp",
            u64::from(std::process::id())
        );
        match openat(parent, name.as_str(), FILE_CREATE_FLAGS, FILE_MODE) {
            Ok(descriptor) => return Ok((name, descriptor)),
            Err(Errno::EXIST) => continue,
            Err(error) => {
                return Err(filesystem_error(
                    "create temporary workspace file",
                    target.to_owned(),
                    error,
                ));
            }
        }
    }
    Err(WorkspaceMutationError::Io {
        operation: "create unique temporary workspace file",
        path: target.to_owned(),
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "temporary filename attempts exhausted",
        ),
    })
}

/// Removes an existing regular file without following symlinks.
pub fn remove_workspace_file(
    root: &Path,
    path: &WorkspacePath,
) -> Result<(), WorkspaceMutationError> {
    let root = PinnedWorkspaceRoot::open(root).map_err(map_root_error)?;
    remove_workspace_file_in(&root, path)
}

/// Removes a file relative to a retained workspace-root authority.
pub fn remove_workspace_file_in(
    root: &PinnedWorkspaceRoot,
    path: &WorkspacePath,
) -> Result<(), WorkspaceMutationError> {
    let mut hooks = NoMutationHooks;
    remove_workspace_file_in_with_hooks(root, path, &mut hooks)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn remove_workspace_file_in_with_hooks(
    root: &PinnedWorkspaceRoot,
    path: &WorkspacePath,
    hooks: &mut impl MutationHooks,
) -> Result<(), WorkspaceMutationError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        verify_mutation_root(root)?;
        let root = OpenedRoot::from_pinned(root);
        let parents = open_parent_chain(&root, path)?;
        let parent = directory_chain_leaf(root.directory, &parents);
        let name = file_name(path);
        let absolute = root.absolute(path);
        let (_source, discovered) = open_regular_target(parent, name, path, &absolute)?;
        let temp_name = allocate_temporary_name(parent, &absolute)?;
        hooks.before_leaf_commit("remove", path, &absolute);
        hooks
            .publish_rename(
                parent,
                name,
                parent,
                temp_name.as_str(),
                RenameFlags::NOREPLACE,
            )
            .map_err(|error| match error {
                Errno::EXIST | Errno::NOENT => {
                    WorkspaceMutationError::PathChanged { path: path.clone() }
                }
                Errno::NOSYS | Errno::NOTSUP | Errno::OPNOTSUPP | Errno::INVAL => {
                    WorkspaceMutationError::AtomicRemoveUnavailable { path: path.clone() }
                }
                _ => filesystem_error("capture workspace file for removal", &absolute, error),
            })?;

        let captured = inspect_regular_target(parent, temp_name.as_str(), path, &absolute)?;
        if !same_identity(&discovered, &captured) {
            hooks.before_leaf_repair("remove", path, &absolute);
            restore_rename(parent, temp_name.as_str(), parent, name, path, &absolute)?;
            return Err(WorkspaceMutationError::PathChanged { path: path.clone() });
        }

        unlinkat(parent, temp_name.as_str(), AtFlags::empty())
            .map_err(|error| repair_error("commit workspace file removal", &absolute, error))?;
        fsync(parent)
            .map_err(|error| filesystem_error("sync workspace parent", &absolute, error))?;
        verify_mutation_root(root.pinned)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (root, path);
        Err(WorkspaceMutationError::UnsupportedPlatform)
    }
}

/// Atomically renames a regular file without replacing any destination entry.
pub fn rename_workspace_file(
    root: &Path,
    old_path: &WorkspacePath,
    new_path: &WorkspacePath,
) -> Result<(), WorkspaceMutationError> {
    let root = PinnedWorkspaceRoot::open(root).map_err(map_root_error)?;
    rename_workspace_file_in(&root, old_path, new_path)
}

/// Renames a file relative to a retained workspace-root authority.
pub fn rename_workspace_file_in(
    root: &PinnedWorkspaceRoot,
    old_path: &WorkspacePath,
    new_path: &WorkspacePath,
) -> Result<(), WorkspaceMutationError> {
    let mut hooks = NoMutationHooks;
    rename_workspace_file_in_with_hooks(root, old_path, new_path, &mut hooks)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rename_workspace_file_in_with_hooks(
    root: &PinnedWorkspaceRoot,
    old_path: &WorkspacePath,
    new_path: &WorkspacePath,
    hooks: &mut impl MutationHooks,
) -> Result<(), WorkspaceMutationError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        verify_mutation_root(root)?;
        let root = OpenedRoot::from_pinned(root);
        let old_parents = open_parent_chain(&root, old_path)?;
        let new_parents = open_parent_chain(&root, new_path)?;
        let old_parent = directory_chain_leaf(root.directory, &old_parents);
        let new_parent = directory_chain_leaf(root.directory, &new_parents);
        let old_name = file_name(old_path);
        let new_name = file_name(new_path);
        let old_absolute = root.absolute(old_path);
        let new_absolute = root.absolute(new_path);

        let (_source, discovered) =
            open_regular_target(old_parent, old_name, old_path, &old_absolute)?;
        match statat(new_parent, new_name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if file_type(&stat) == FileType::Symlink => {
                return Err(WorkspaceMutationError::Symlink {
                    path: new_path.clone(),
                });
            }
            Ok(_) => {
                return Err(WorkspaceMutationError::PathCollision {
                    path: new_path.clone(),
                });
            }
            Err(Errno::NOENT) => {}
            Err(error) => {
                return Err(filesystem_error(
                    "inspect rename destination",
                    new_absolute,
                    error,
                ));
            }
        }

        hooks.before_leaf_commit("rename", old_path, &old_absolute);
        hooks
            .publish_rename(
                old_parent,
                old_name,
                new_parent,
                new_name,
                RenameFlags::NOREPLACE,
            )
            .map_err(|error| match error {
                Errno::EXIST => WorkspaceMutationError::PathCollision {
                    path: new_path.clone(),
                },
                Errno::NOENT => WorkspaceMutationError::PathChanged {
                    path: old_path.clone(),
                },
                Errno::NOSYS | Errno::NOTSUP | Errno::OPNOTSUPP | Errno::INVAL => {
                    WorkspaceMutationError::AtomicRenameUnavailable {
                        old_path: old_path.clone(),
                        new_path: new_path.clone(),
                    }
                }
                _ => filesystem_error("rename workspace file", &old_absolute, error),
            })?;

        let captured = inspect_regular_target(new_parent, new_name, new_path, &new_absolute)?;
        if !same_identity(&discovered, &captured) {
            restore_rename(
                new_parent,
                new_name,
                old_parent,
                old_name,
                old_path,
                &old_absolute,
            )?;
            return Err(WorkspaceMutationError::PathChanged {
                path: old_path.clone(),
            });
        }

        fsync(old_parent)
            .map_err(|error| filesystem_error("sync rename source parent", &old_absolute, error))?;
        fsync(new_parent).map_err(|error| {
            filesystem_error("sync rename destination parent", &new_absolute, error)
        })?;
        verify_mutation_root(root.pinned)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (root, old_path, new_path);
        Err(WorkspaceMutationError::UnsupportedPlatform)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct OpenedRoot<'root> {
    canonical: &'root Path,
    directory: &'root OwnedFd,
    pinned: &'root PinnedWorkspaceRoot,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl<'root> OpenedRoot<'root> {
    fn from_pinned(root: &'root PinnedWorkspaceRoot) -> Self {
        Self {
            canonical: root.path(),
            directory: root.directory(),
            pinned: root,
        }
    }

    fn absolute(&self, path: &WorkspacePath) -> PathBuf {
        self.canonical.join(path.as_str())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_parent_chain(
    root: &OpenedRoot,
    path: &WorkspacePath,
) -> Result<Vec<OwnedFd>, WorkspaceMutationError> {
    let components = path.components().collect::<Vec<_>>();
    let mut chain = Vec::with_capacity(components.len().saturating_sub(1));
    for (index, component) in components[..components.len() - 1].iter().enumerate() {
        let logical = WorkspacePath::new(components[..=index].join("/"))
            .expect("a prefix of a WorkspacePath remains valid");
        let absolute = root.absolute(&logical);
        let parent = directory_chain_leaf(root.directory, &chain);
        let discovered = match statat(parent, *component, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => {
                return Err(WorkspaceMutationError::MissingParent { path: logical });
            }
            Err(error) => {
                return Err(filesystem_error(
                    "inspect workspace parent directory",
                    absolute,
                    error,
                ));
            }
        };
        match file_type(&discovered) {
            FileType::Symlink => return Err(WorkspaceMutationError::Symlink { path: logical }),
            FileType::Directory => {}
            kind => {
                return Err(WorkspaceMutationError::NotDirectory {
                    path: logical,
                    kind: file_type_name(kind),
                });
            }
        }

        let directory =
            openat(parent, *component, DIRECTORY_OPEN_FLAGS, Mode::empty()).map_err(|error| {
                match error {
                    Errno::NOENT => WorkspaceMutationError::MissingParent {
                        path: logical.clone(),
                    },
                    Errno::LOOP => WorkspaceMutationError::Symlink {
                        path: logical.clone(),
                    },
                    Errno::NOTDIR => WorkspaceMutationError::NotDirectory {
                        path: logical.clone(),
                        kind: "non-directory filesystem object",
                    },
                    _ => filesystem_error("open workspace parent directory", &absolute, error),
                }
            })?;
        let opened = fstat(&directory).map_err(|error| {
            filesystem_error(
                "inspect opened workspace parent directory",
                &absolute,
                error,
            )
        })?;
        if file_type(&opened) != FileType::Directory || !same_identity(&discovered, &opened) {
            return Err(WorkspaceMutationError::PathChanged { path: logical });
        }
        chain.push(directory);
    }
    Ok(chain)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_external_regular_file_in(
    root: &PinnedWorkspaceRoot,
    path: &WorkspacePath,
    flags: OFlags,
    operation: &'static str,
) -> Result<OpenedRegularFile, WorkspaceMutationError> {
    verify_mutation_root(root)?;
    let opened = OpenedRoot::from_pinned(root);
    let parents = open_parent_chain(&opened, path)?;
    let parent = directory_chain_leaf(opened.directory, &parents);
    let absolute = opened.absolute(path);
    let discovered = inspect_regular_target(parent, file_name(path), path, &absolute)?;
    require_unaliased_regular(path, &discovered)?;
    let descriptor = openat(parent, file_name(path), flags, Mode::empty())
        .map_err(|error| map_target_open_error(operation, path, &absolute, error))?;
    let retained = fstat(&descriptor)
        .map_err(|error| filesystem_error("inspect opened external file", &absolute, error))?;
    if !same_identity(&discovered, &retained) {
        return Err(WorkspaceMutationError::PathChanged { path: path.clone() });
    }
    require_unaliased_regular(path, &retained)?;
    verify_mutation_root(root)?;
    Ok(opened_regular_file(descriptor, &retained))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn require_unaliased_regular(
    path: &WorkspacePath,
    stat: &Stat,
) -> Result<(), WorkspaceMutationError> {
    if file_type(stat) != FileType::RegularFile {
        return Err(WorkspaceMutationError::NotRegularFile {
            path: path.clone(),
            kind: file_type_name(file_type(stat)),
        });
    }
    if stat.st_nlink != 1 {
        return Err(WorkspaceMutationError::MultipleLinks { path: path.clone() });
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn opened_regular_file(descriptor: OwnedFd, stat: &Stat) -> OpenedRegularFile {
    OpenedRegularFile {
        file: File::from(descriptor),
        identity: regular_file_identity(stat),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn regular_file_identity(stat: &Stat) -> RegularFileIdentity {
    RegularFileIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn list_external_directory(
    directory: &OwnedFd,
    absolute_directory: &Path,
    relative_directory: Option<&WorkspacePath>,
    paths: &mut Vec<WorkspacePath>,
    directories: &mut usize,
    entries_seen: &mut usize,
    maximum_files: usize,
    include: &impl Fn(&WorkspacePath) -> bool,
) -> Result<(), WorkspaceMutationError> {
    *directories = directories.saturating_add(1);
    if *directories > MAX_WORKSPACE_DIRECTORIES {
        return Err(WorkspaceMutationError::ListingLimitExceeded {
            kind: "directory",
            limit: MAX_WORKSPACE_DIRECTORIES,
        });
    }
    let mut entries = Dir::read_from(directory)
        .map_err(|error| filesystem_error("read external directory", absolute_directory, error))?;
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(|error| {
            filesystem_error("read external directory entry", absolute_directory, error)
        })?;
        let raw_name = entry.file_name().to_bytes();
        if matches!(raw_name, b"." | b"..") {
            continue;
        }
        *entries_seen = entries_seen.saturating_add(1);
        if *entries_seen > MAX_EXTERNAL_DIRECTORY_ENTRIES {
            return Err(WorkspaceMutationError::ListingLimitExceeded {
                kind: "entry",
                limit: MAX_EXTERNAL_DIRECTORY_ENTRIES,
            });
        }
        let absolute = absolute_directory.join(OsStr::from_bytes(raw_name));
        let Ok(name) = std::str::from_utf8(raw_name) else {
            continue;
        };
        let candidate = match relative_directory {
            Some(parent) => format!("{}/{name}", parent.as_str()),
            None => name.to_owned(),
        };
        let Ok(path) = WorkspacePath::new(candidate) else {
            continue;
        };
        let stat = statat(directory, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| filesystem_error("inspect external entry", &absolute, error))?;
        match file_type(&stat) {
            FileType::Directory => {
                let child = openat(
                    directory,
                    entry.file_name(),
                    DIRECTORY_OPEN_FLAGS,
                    Mode::empty(),
                )
                .map_err(|error| filesystem_error("open external directory", &absolute, error))?;
                let retained = fstat(&child).map_err(|error| {
                    filesystem_error("inspect opened external directory", &absolute, error)
                })?;
                if file_type(&retained) != FileType::Directory || !same_identity(&stat, &retained) {
                    return Err(WorkspaceMutationError::PathChanged { path });
                }
                list_external_directory(
                    &child,
                    &absolute,
                    Some(&path),
                    paths,
                    directories,
                    entries_seen,
                    maximum_files,
                    include,
                )?;
            }
            FileType::RegularFile if stat.st_nlink == 1 && include(&path) => {
                if paths.len() == maximum_files {
                    return Err(WorkspaceMutationError::ListingLimitExceeded {
                        kind: "file",
                        limit: maximum_files,
                    });
                }
                paths.push(path);
            }
            _ => {
                // Unsafe, special and aliased entries are deliberately absent
                // from the read-only projection and are never opened here.
            }
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn inspect_regular_target(
    parent: &OwnedFd,
    name: &str,
    path: &WorkspacePath,
    absolute: &Path,
) -> Result<Stat, WorkspaceMutationError> {
    let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|error| match error {
        Errno::NOENT => WorkspaceMutationError::MissingTarget { path: path.clone() },
        _ => filesystem_error("inspect workspace file", absolute, error),
    })?;
    match file_type(&stat) {
        FileType::RegularFile => Ok(stat),
        FileType::Symlink => Err(WorkspaceMutationError::Symlink { path: path.clone() }),
        kind => Err(WorkspaceMutationError::NotRegularFile {
            path: path.clone(),
            kind: file_type_name(kind),
        }),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_regular_target(
    parent: &OwnedFd,
    name: &str,
    path: &WorkspacePath,
    absolute: &Path,
) -> Result<(OwnedFd, Stat), WorkspaceMutationError> {
    let discovered = inspect_regular_target(parent, name, path, absolute)?;
    let descriptor = openat(parent, name, FILE_OPEN_FLAGS, Mode::empty())
        .map_err(|error| map_target_open_error("open workspace file", path, absolute, error))?;
    let opened = fstat(&descriptor)
        .map_err(|error| filesystem_error("inspect opened workspace file", absolute, error))?;
    if file_type(&opened) != FileType::RegularFile || !same_identity(&discovered, &opened) {
        return Err(WorkspaceMutationError::PathChanged { path: path.clone() });
    }
    Ok((descriptor, opened))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn allocate_temporary_name(
    parent: &OwnedFd,
    target: &Path,
) -> Result<String, WorkspaceMutationError> {
    let (name, descriptor) = create_temporary_file(parent, target)?;
    drop(descriptor);
    unlinkat(parent, name.as_str(), AtFlags::empty())
        .map_err(|error| repair_error("reserve a temporary workspace path", target, error))?;
    Ok(name)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn restore_exchange(
    parent: &OwnedFd,
    destination: &str,
    captured: &str,
    path: &WorkspacePath,
    absolute: &Path,
) -> Result<(), WorkspaceMutationError> {
    renameat_with(parent, destination, parent, captured, RenameFlags::EXCHANGE)
        .map_err(|error| repair_error("restore a raced workspace replacement", absolute, error))?;
    inspect_regular_target(parent, destination, path, absolute).map_err(|error| {
        repair_error(
            "verify a raced workspace replacement repair",
            absolute,
            error,
        )
    })?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn restore_rename(
    captured_parent: &OwnedFd,
    captured_name: &str,
    destination_parent: &OwnedFd,
    destination_name: &str,
    path: &WorkspacePath,
    absolute: &Path,
) -> Result<(), WorkspaceMutationError> {
    renameat_with(
        captured_parent,
        captured_name,
        destination_parent,
        destination_name,
        RenameFlags::NOREPLACE,
    )
    .map_err(|error| repair_error("restore a raced workspace mutation", absolute, error))?;
    inspect_regular_target(destination_parent, destination_name, path, absolute).map_err(
        |error| repair_error("verify a raced workspace mutation repair", absolute, error),
    )?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn map_target_open_error(
    operation: &'static str,
    path: &WorkspacePath,
    absolute: &Path,
    error: Errno,
) -> WorkspaceMutationError {
    match error {
        Errno::NOENT => WorkspaceMutationError::MissingTarget { path: path.clone() },
        Errno::LOOP => WorkspaceMutationError::Symlink { path: path.clone() },
        _ => filesystem_error(operation, absolute, error),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn file_name(path: &WorkspacePath) -> &str {
    path.as_str()
        .rsplit('/')
        .next()
        .expect("WorkspacePath is nonempty")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn directory_chain_leaf<'a>(root: &'a OwnedFd, chain: &'a [OwnedFd]) -> &'a OwnedFd {
    chain.last().unwrap_or(root)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn file_type(stat: &Stat) -> FileType {
    FileType::from_raw_mode(stat.st_mode)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn same_identity(left: &Stat, right: &Stat) -> bool {
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn filesystem_error(
    operation: &'static str,
    path: impl Into<PathBuf>,
    error: impl fmt::Display,
) -> WorkspaceMutationError {
    WorkspaceMutationError::Filesystem {
        operation,
        path: path.into(),
        message: error.to_string(),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn repair_error(
    operation: &'static str,
    path: impl Into<PathBuf>,
    error: impl fmt::Display,
) -> WorkspaceMutationError {
    WorkspaceMutationError::RepairFailed {
        operation,
        path: path.into(),
        detail: error.to_string(),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn repair_or_filesystem_error(
    cleanup: rustix::io::Result<()>,
    operation: &'static str,
    path: &Path,
    error: impl fmt::Display,
) -> WorkspaceMutationError {
    match cleanup {
        Ok(()) => filesystem_error(operation, path, error),
        Err(cleanup) => repair_error(
            operation,
            path,
            format!("{error}; cleanup failed: {cleanup}"),
        ),
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

fn map_root_error(error: WorkspaceHashError) -> WorkspaceMutationError {
    let path = match &error {
        WorkspaceHashError::InvalidRoot { path, .. }
        | WorkspaceHashError::RootNotDirectory { path }
        | WorkspaceHashError::WorkspaceChanged { path, .. }
        | WorkspaceHashError::Filesystem { path, .. }
        | WorkspaceHashError::InvalidPath { path, .. }
        | WorkspaceHashError::UnrepresentablePath { path, .. }
        | WorkspaceHashError::Symlink { path }
        | WorkspaceHashError::UnsupportedFileType { path, .. } => path.clone(),
        _ => PathBuf::new(),
    };
    WorkspaceMutationError::RootBindingChanged {
        path,
        detail: error.to_string(),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn verify_mutation_root(root: &PinnedWorkspaceRoot) -> Result<(), WorkspaceMutationError> {
    root.verify_binding().map_err(map_root_error)
}

/// A typed failure from a descriptor-relative workspace mutation.
#[derive(Debug)]
pub enum WorkspaceMutationError {
    UnsupportedPlatform,
    InvalidRoot {
        path: PathBuf,
        source: std::io::Error,
    },
    RootNotDirectory {
        path: PathBuf,
    },
    RootBindingChanged {
        path: PathBuf,
        detail: String,
    },
    MissingParent {
        path: WorkspacePath,
    },
    MissingTarget {
        path: WorkspacePath,
    },
    PathCollision {
        path: WorkspacePath,
    },
    Symlink {
        path: WorkspacePath,
    },
    NotDirectory {
        path: WorkspacePath,
        kind: &'static str,
    },
    NotRegularFile {
        path: WorkspacePath,
        kind: &'static str,
    },
    MultipleLinks {
        path: WorkspacePath,
    },
    ListingLimitExceeded {
        kind: &'static str,
        limit: usize,
    },
    PathChanged {
        path: WorkspacePath,
    },
    FileSizeLimitExceeded {
        path: WorkspacePath,
        actual: u64,
        limit: u64,
    },
    AtomicRenameUnavailable {
        old_path: WorkspacePath,
        new_path: WorkspacePath,
    },
    AtomicReplaceUnavailable {
        path: WorkspacePath,
    },
    AtomicRemoveUnavailable {
        path: WorkspacePath,
    },
    RepairFailed {
        operation: &'static str,
        path: PathBuf,
        detail: String,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    Filesystem {
        operation: &'static str,
        path: PathBuf,
        message: String,
    },
}

impl fmt::Display for WorkspaceMutationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => formatter.write_str(
                "workspace mutations are supported only on Linux, macOS, and Linux-based WSL",
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
            Self::RootBindingChanged { path, detail } => write!(
                formatter,
                "workspace root `{}` is no longer bound to the opened directory: {detail}",
                path.display()
            ),
            Self::MissingParent { path } => {
                write!(
                    formatter,
                    "workspace parent directory `{path}` does not exist"
                )
            }
            Self::MissingTarget { path } => {
                write!(formatter, "workspace file `{path}` does not exist")
            }
            Self::PathCollision { path } => {
                write!(formatter, "workspace path `{path}` already exists")
            }
            Self::Symlink { path } => write!(
                formatter,
                "workspace path `{path}` is a symlink; symlinks are not supported"
            ),
            Self::NotDirectory { path, kind } => {
                write!(
                    formatter,
                    "workspace parent `{path}` is a {kind}, not a directory"
                )
            }
            Self::NotRegularFile { path, kind } => {
                write!(
                    formatter,
                    "workspace path `{path}` is a {kind}, not a regular file"
                )
            }
            Self::MultipleLinks { path } => write!(
                formatter,
                "workspace path `{path}` has multiple hard links and is not safe for external routing"
            ),
            Self::ListingLimitExceeded { kind, limit } => write!(
                formatter,
                "external {kind} listing exceeds the {limit}-entry limit"
            ),
            Self::PathChanged { path } => {
                write!(
                    formatter,
                    "workspace path `{path}` changed during the operation"
                )
            }
            Self::FileSizeLimitExceeded {
                path,
                actual,
                limit,
            } => write!(
                formatter,
                "workspace file `{path}` would be {actual} bytes; maximum is {limit} bytes"
            ),
            Self::AtomicRenameUnavailable { old_path, new_path } => write!(
                formatter,
                "cannot atomically rename `{old_path}` to `{new_path}` without replacement on this filesystem; preserve the workspace and journal, quit, and use a supported local filesystem after reconciliation"
            ),
            Self::AtomicReplaceUnavailable { path } => write!(
                formatter,
                "cannot atomically replace `{path}` conditionally on this filesystem; preserve the workspace and journal, quit, and use a supported local filesystem after reconciliation"
            ),
            Self::AtomicRemoveUnavailable { path } => write!(
                formatter,
                "cannot atomically remove `{path}` conditionally on this filesystem; preserve the workspace and journal, quit, and use a supported local filesystem after reconciliation"
            ),
            Self::RepairFailed {
                operation,
                path,
                detail,
            } => write!(
                formatter,
                "failed to {operation} `{}`; workspace consistency is uncertain: {detail}",
                path.display()
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "failed to {operation} `{}`: {source}",
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

impl Error for WorkspaceMutationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidRoot { source, .. } | Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use rustrace_model::WorkspacePath;

    use super::{
        MutationHooks, PinnedWorkspaceRoot, WorkspaceMutationError,
        remove_workspace_file_in_with_hooks, rename_workspace_file_in_with_hooks,
        write_workspace_file_in_with_hooks,
    };

    static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(0);

    struct UnsupportedRename(super::Errno, Vec<super::RenameFlags>);

    impl MutationHooks for UnsupportedRename {
        fn publish_rename(
            &mut self,
            _old_parent: &super::OwnedFd,
            _old_name: &str,
            _new_parent: &super::OwnedFd,
            _new_name: &str,
            flags: super::RenameFlags,
        ) -> Result<(), super::Errno> {
            self.1.push(flags);
            Err(self.0)
        }
    }

    #[test]
    fn unsupported_publication_preserves_files_and_explains_remediation() {
        for code in [
            super::Errno::NOSYS,
            super::Errno::NOTSUP,
            super::Errno::OPNOTSUPP,
            super::Errno::INVAL,
        ] {
            for operation in ["write", "remove", "rename"] {
                let temp = TempRoot::new();
                let path = workspace_path("old.rs");
                fs::write(temp.path().join("old.rs"), b"preserve").unwrap();
                let root = PinnedWorkspaceRoot::open(temp.path()).unwrap();
                let mut hooks = UnsupportedRename(code, vec![]);
                let result = match operation {
                    "write" => write_workspace_file_in_with_hooks(&root, &path, b"new", &mut hooks),
                    "remove" => remove_workspace_file_in_with_hooks(&root, &path, &mut hooks),
                    "rename" => rename_workspace_file_in_with_hooks(
                        &root,
                        &path,
                        &workspace_path("new.rs"),
                        &mut hooks,
                    ),
                    _ => unreachable!(),
                };
                let error = result.unwrap_err();
                assert!(matches!(
                    (&error, operation),
                    (
                        WorkspaceMutationError::AtomicReplaceUnavailable { .. },
                        "write"
                    ) | (
                        WorkspaceMutationError::AtomicRemoveUnavailable { .. },
                        "remove"
                    ) | (
                        WorkspaceMutationError::AtomicRenameUnavailable { .. },
                        "rename"
                    )
                ));
                assert_eq!(
                    hooks.1,
                    [if operation == "write" {
                        super::RenameFlags::EXCHANGE
                    } else {
                        super::RenameFlags::NOREPLACE
                    }]
                );
                assert_eq!(fs::read(temp.path().join("old.rs")).unwrap(), b"preserve");
                assert!(!temp.path().join("new.rs").exists());
                assert_no_temporary_artifacts(temp.path());
                assert!(
                    error.to_string().contains("supported local filesystem"),
                    "{error}"
                );
                assert!(error.to_string().contains("preserve"), "{error}");
            }
        }
    }

    struct CreateSync {
        phases: Vec<&'static str>,
        fail: Option<&'static str>,
        substitute: Option<PathBuf>,
    }

    impl MutationHooks for CreateSync {
        fn sync_created(
            &mut self,
            descriptor: &super::OwnedFd,
            phase: &'static str,
        ) -> Result<(), super::Errno> {
            self.phases.push(phase);
            let stat = super::fstat(descriptor)?;
            assert_eq!(
                super::file_type(&stat),
                if phase == "file" {
                    super::FileType::RegularFile
                } else {
                    super::FileType::Directory
                }
            );
            if self.fail == Some(phase) {
                return Err(super::Errno::IO);
            }
            super::fsync(descriptor)?;
            if phase == "parent"
                && let Some(path) = self.substitute.take()
            {
                fs::rename(&path, path.with_extension("retained")).unwrap();
                fs::write(path, b"replacement").unwrap();
            }
            Ok(())
        }
    }

    #[test]
    fn create_syncs_retained_file_before_parent_and_acknowledgment() {
        let temp = TempRoot::new();
        let root = PinnedWorkspaceRoot::open(temp.path()).unwrap();
        let mut hooks = CreateSync {
            phases: vec![],
            fail: None,
            substitute: None,
        };
        super::create_workspace_file_in_with_hooks(&root, &workspace_path("new.rs"), &mut hooks)
            .unwrap();
        assert_eq!(hooks.phases, ["file", "parent"]);
        assert_eq!(fs::read(temp.path().join("new.rs")).unwrap(), b"");
        assert_no_temporary_artifacts(temp.path());
    }

    #[test]
    fn create_sync_errors_preserve_published_evidence_and_return_failure() {
        for phase in ["file", "parent"] {
            let temp = TempRoot::new();
            let root = PinnedWorkspaceRoot::open(temp.path()).unwrap();
            let mut hooks = CreateSync {
                phases: vec![],
                fail: Some(phase),
                substitute: None,
            };
            let error = super::create_workspace_file_in_with_hooks(
                &root,
                &workspace_path("new.rs"),
                &mut hooks,
            )
            .unwrap_err();
            assert!(error.to_string().contains("sync created"));
            assert_eq!(hooks.phases.last(), Some(&phase));
            assert_eq!(fs::read(temp.path().join("new.rs")).unwrap(), b"");
            assert_no_temporary_artifacts(temp.path());
        }
    }

    #[test]
    fn create_verifies_retained_leaf_after_sync() {
        let temp = TempRoot::new();
        let root = PinnedWorkspaceRoot::open(temp.path()).unwrap();
        let absolute = temp.path().join("new.rs");
        let mut hooks = CreateSync {
            phases: vec![],
            fail: None,
            substitute: Some(absolute.clone()),
        };
        assert!(matches!(
            super::create_workspace_file_in_with_hooks(
                &root,
                &workspace_path("new.rs"),
                &mut hooks
            ),
            Err(WorkspaceMutationError::PathChanged { .. })
        ));
        assert_eq!(fs::read(absolute).unwrap(), b"replacement");
    }

    struct SubstituteLeaf {
        replacement: &'static [u8],
        triggered: bool,
    }

    struct BlockRemoveRepair {
        triggered: bool,
    }

    impl MutationHooks for BlockRemoveRepair {
        fn before_leaf_commit(
            &mut self,
            _operation: &'static str,
            _path: &WorkspacePath,
            absolute: &Path,
        ) {
            fs::remove_file(absolute).expect("remove the validated leaf");
            fs::write(absolute, b"replacement").expect("install a substituted leaf");
        }

        fn before_leaf_repair(
            &mut self,
            _operation: &'static str,
            _path: &WorkspacePath,
            absolute: &Path,
        ) {
            fs::write(absolute, b"repair blocker").expect("block no-replace restoration");
            self.triggered = true;
        }
    }

    impl MutationHooks for SubstituteLeaf {
        fn before_leaf_commit(
            &mut self,
            _operation: &'static str,
            _path: &WorkspacePath,
            absolute: &Path,
        ) {
            fs::remove_file(absolute).expect("remove the validated leaf");
            fs::write(absolute, self.replacement).expect("install a substituted leaf");
            self.triggered = true;
        }
    }

    #[test]
    fn write_rejects_a_leaf_substituted_at_the_commit_boundary() {
        let root = TempRoot::new();
        let path = workspace_path("src/main.rs");
        let absolute = root.path().join(path.as_str());
        fs::create_dir_all(absolute.parent().expect("file has a parent")).unwrap();
        fs::write(&absolute, b"validated").unwrap();
        let pinned = PinnedWorkspaceRoot::open(root.path()).unwrap();
        let mut hooks = SubstituteLeaf {
            replacement: b"replacement",
            triggered: false,
        };

        let result =
            write_workspace_file_in_with_hooks(&pinned, &path, b"new contents", &mut hooks);

        assert!(hooks.triggered);
        assert!(matches!(
            result,
            Err(WorkspaceMutationError::PathChanged { path: changed }) if changed == path
        ));
        assert_eq!(fs::read(&absolute).unwrap(), b"replacement");
        assert_no_temporary_artifacts(root.path());
    }

    #[test]
    fn remove_rejects_a_leaf_substituted_at_the_commit_boundary() {
        let root = TempRoot::new();
        let path = workspace_path("src/main.rs");
        let absolute = root.path().join(path.as_str());
        fs::create_dir_all(absolute.parent().expect("file has a parent")).unwrap();
        fs::write(&absolute, b"validated").unwrap();
        let pinned = PinnedWorkspaceRoot::open(root.path()).unwrap();
        let mut hooks = SubstituteLeaf {
            replacement: b"replacement",
            triggered: false,
        };

        let result = remove_workspace_file_in_with_hooks(&pinned, &path, &mut hooks);

        assert!(hooks.triggered);
        assert!(matches!(
            result,
            Err(WorkspaceMutationError::PathChanged { path: changed }) if changed == path
        ));
        assert_eq!(fs::read(&absolute).unwrap(), b"replacement");
        assert_no_temporary_artifacts(root.path());
    }

    #[test]
    fn rename_rejects_a_leaf_substituted_at_the_commit_boundary() {
        let root = TempRoot::new();
        let old_path = workspace_path("src/main.rs");
        let new_path = workspace_path("src/lib.rs");
        let old_absolute = root.path().join(old_path.as_str());
        let new_absolute = root.path().join(new_path.as_str());
        fs::create_dir_all(old_absolute.parent().expect("file has a parent")).unwrap();
        fs::write(&old_absolute, b"validated").unwrap();
        let pinned = PinnedWorkspaceRoot::open(root.path()).unwrap();
        let mut hooks = SubstituteLeaf {
            replacement: b"replacement",
            triggered: false,
        };

        let result = rename_workspace_file_in_with_hooks(&pinned, &old_path, &new_path, &mut hooks);

        assert!(hooks.triggered);
        assert!(matches!(
            result,
            Err(WorkspaceMutationError::PathChanged { path: changed }) if changed == old_path
        ));
        assert_eq!(fs::read(&old_absolute).unwrap(), b"replacement");
        assert!(!new_absolute.exists());
        assert_no_temporary_artifacts(root.path());
    }

    #[test]
    fn failed_leaf_repair_is_explicit_and_retains_recovery_evidence() {
        let root = TempRoot::new();
        let path = workspace_path("src/main.rs");
        let absolute = root.path().join(path.as_str());
        fs::create_dir_all(absolute.parent().expect("file has a parent")).unwrap();
        fs::write(&absolute, b"validated").unwrap();
        let pinned = PinnedWorkspaceRoot::open(root.path()).unwrap();
        let mut hooks = BlockRemoveRepair { triggered: false };

        let result = remove_workspace_file_in_with_hooks(&pinned, &path, &mut hooks);

        assert!(hooks.triggered);
        assert!(matches!(
            result,
            Err(WorkspaceMutationError::RepairFailed { .. })
        ));
        assert_eq!(fs::read(&absolute).unwrap(), b"repair blocker");
        let evidence = temporary_artifacts(root.path());
        assert_eq!(evidence.len(), 1);
        assert_eq!(fs::read(&evidence[0]).unwrap(), b"replacement");
        fs::remove_file(&evidence[0]).unwrap();
        assert_no_temporary_artifacts(root.path());
    }

    fn workspace_path(path: &str) -> WorkspacePath {
        WorkspacePath::new(path).expect("test path is valid")
    }

    fn assert_no_temporary_artifacts(root: &Path) {
        assert!(
            temporary_artifacts(root).is_empty(),
            "temporary workspace mutation artifact remained"
        );
    }

    fn temporary_artifacts(root: &Path) -> Vec<PathBuf> {
        let mut artifacts = Vec::new();
        let mut pending = vec![root.to_owned()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    pending.push(entry.path());
                } else if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".rustrace-editor-")
                {
                    artifacts.push(entry.path());
                }
            }
        }
        artifacts.sort();
        artifacts
    }

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let nonce = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rustrace-workspace-mutation-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create isolated test root");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove isolated test root");
        }
    }
}
