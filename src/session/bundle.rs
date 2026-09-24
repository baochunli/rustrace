//! Deterministic publication of one finalized session as an LMS ZIP.

use super::{
    FinalizationReceipt, FinalizationStatus, IncompleteFinalization, METADATA_LIMIT,
    ProductionSession, Result, ResumeChoice, process_probe,
};
use rustrace_model::{
    Hash, RPROV_FORMAT_VERSION_V1, RPROV_RECORD_HEADER_BYTES, RprovContainerHeader, RprovManifest,
    RprovPackageState, RprovRecordHeader, RprovRecordType, encode_rprov_container_header,
    encode_rprov_manifest, encode_rprov_record_header, rprov_raw_blake3,
};
use rustrace_workspace::{
    hash::{PinnedWorkspaceRoot, hash_entries},
    rprov_import::{ImportedPackageKind, ImportedRprov, import_rprov},
};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const ZIP_LOCAL_MAGIC: u32 = 0x0403_4b50;
const ZIP_CENTRAL_MAGIC: u32 = 0x0201_4b50;
const ZIP_END_MAGIC: u32 = 0x0605_4b50;
const ZIP_VERSION: u16 = 20;
const ZIP_MADE_BY_UNIX: u16 = (3 << 8) | ZIP_VERSION;
const ZIP_FLAG_UTF8: u16 = 1 << 11;
const ZIP_METHOD_STORED: u16 = 0;
const ZIP_FIXED_TIME: u16 = 0;
const ZIP_FIXED_DATE: u16 = (1 << 5) | 1;
const ZIP_REGULAR_MODE: u32 = 0o100644 << 16;
const COPY_BUFFER_BYTES: usize = 8 * 1024;
const RPROV_ENTRY: &str = "session.rprov";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleOutput {
    pub path: PathBuf,
    pub blake3: Hash,
}

/// Writes a complete deterministic archive and installs it without replacing
/// an existing destination.
pub fn create_bundle(receipt: &FinalizationReceipt, destination: &Path) -> Result<BundleOutput> {
    let bundle = create_bundle_inner(receipt, destination, |_| Ok(()))?;
    if let Err(error) = super::retention::record_successful_export(receipt, &bundle) {
        let rollback = fs::remove_file(&bundle.path);
        return Err(format!(
            "bundle export record could not be published: {error}; bundle rollback {}",
            if rollback.is_ok() {
                "succeeded"
            } else {
                "failed; inspect the destination and retry status"
            }
        )
        .into());
    }
    Ok(bundle)
}

fn create_bundle_inner(
    receipt: &FinalizationReceipt,
    destination: &Path,
    before_install: impl FnOnce(&Path) -> Result<()>,
) -> Result<BundleOutput> {
    if !matches!(
        receipt.manifest().package_state,
        RprovPackageState::CleanFinalized
    ) {
        return Err("only a clean finalized receipt can produce an LMS bundle".into());
    }
    receipt.manifest().validate()?;
    let source_hash = hash_entries(
        receipt
            .final_workspace()
            .iter()
            .map(|(path, bytes)| (path, bytes.as_slice())),
    )?;
    if receipt.manifest().final_tree_hash.known() != Some(&source_hash) {
        return Err("immutable final workspace hash differs from the finalized manifest".into());
    }

    create_bundle_from_parts(
        receipt.final_workspace(),
        receipt.manifest(),
        destination,
        |entry| receipt.read_payload(entry),
        before_install,
    )
}

fn create_incomplete_bundle(
    workspace: &Path,
    incomplete: &IncompleteFinalization,
    destination: &Path,
) -> Result<BundleOutput> {
    let manifest = incomplete
        .manifest()
        .ok_or("incomplete recovery capture has no valid package manifest")?;
    if !matches!(
        manifest.package_state,
        RprovPackageState::RecoveryIncomplete { .. }
    ) {
        return Err("recovery export manifest is not marked incomplete".into());
    }
    manifest.validate()?;
    let final_workspace = incomplete
        .final_workspace()
        .ok_or("incomplete recovery capture has no immutable workspace boundary")?;
    let bundle = create_bundle_from_parts(
        final_workspace,
        manifest,
        destination,
        |entry| incomplete.read_payload(entry),
        |_| Ok(()),
    )?;
    if let Err(error) = super::retention::record_incomplete_export(workspace, incomplete, &bundle) {
        let rollback = fs::remove_file(&bundle.path);
        return Err(format!(
            "incomplete export record could not be published: {error}; bundle rollback {}",
            if rollback.is_ok() {
                "succeeded"
            } else {
                "failed; inspect the destination and retry status"
            }
        )
        .into());
    }
    Ok(bundle)
}

fn create_bundle_from_parts(
    workspace: &std::collections::BTreeMap<rustrace_model::WorkspacePath, Vec<u8>>,
    manifest: &RprovManifest,
    destination: &Path,
    read_payload: impl FnMut(&str) -> Result<Vec<u8>>,
    before_install: impl FnOnce(&Path) -> Result<()>,
) -> Result<BundleOutput> {
    manifest.validate()?;
    let source_hash = hash_entries(
        workspace
            .iter()
            .map(|(path, bytes)| (path, bytes.as_slice())),
    )?;

    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if destination.file_name().is_none() {
        return Err("bundle destination must name a file".into());
    }
    let temporary_tag = blake3::hash(destination.as_os_str().as_encoded_bytes())
        .to_hex()
        .to_string();
    cleanup_stale_temporaries(parent, &temporary_tag)?;

    let mut rprov_temp = TemporaryFile::create(parent, &temporary_tag)?;
    write_rprov(manifest, read_payload, rprov_temp.writer()?)?;
    rprov_temp.sync()?;

    let mut archive_temp = TemporaryFile::create(parent, &temporary_tag)?;
    write_zip(workspace, rprov_temp.file_mut()?, archive_temp.writer()?)?;
    archive_temp.sync()?;

    archive_temp.rewind()?;
    let imported = import_rprov(BufReader::new(archive_temp.file_mut()?))?;
    if imported.kind() != ImportedPackageKind::LmsZip {
        return Err("generated archive was not recognized as the LMS ZIP".into());
    }
    let imported_source_hash = hash_imported_outer_source(&imported)?;
    if imported_source_hash != source_hash || imported.manifest() != manifest {
        return Err("generated outer source does not match the finalized manifest".into());
    }
    drop(imported);

    process_probe("archive-before-install");
    before_install(archive_temp.path())?;
    let archive_hash = hash_file(archive_temp.file_mut()?)?;
    archive_temp.install_no_clobber(destination)?;
    Ok(BundleOutput {
        path: destination.to_path_buf(),
        blake3: archive_hash,
    })
}

#[cfg(test)]
pub(super) fn create_bundle_after_replacing_temporary(
    receipt: &FinalizationReceipt,
    destination: &Path,
    replacement: &Path,
) -> Result<BundleOutput> {
    create_bundle_inner(receipt, destination, |temporary| {
        fs::remove_file(temporary)?;
        fs::copy(replacement, temporary)?;
        Ok(())
    })
}

/// Computes the accepted workspace hash over the exact outer source retained
/// by the hostile-input importer. T6.4 can compare this with `final_tree_hash`.
pub fn hash_imported_outer_source(package: &ImportedRprov) -> Result<Hash> {
    if package.kind() != ImportedPackageKind::LmsZip {
        return Err("submitted source comparison is unavailable for standalone .rprov".into());
    }
    let mut files = Vec::with_capacity(package.outer_source_files().len());
    for declaration in package.outer_source_files() {
        let mut bytes = Vec::new();
        package
            .open_outer_source(&declaration.path)?
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != declaration.byte_length {
            return Err("imported outer source length changed while reading".into());
        }
        files.push((declaration.path.clone(), bytes));
    }
    Ok(hash_entries(
        files.iter().map(|(path, bytes)| (path, bytes.as_slice())),
    )?)
}

pub fn submit_finalized_workspace(
    workspace: &Path,
    student_id: &str,
    destination: Option<&Path>,
) -> Result<BundleOutput> {
    submit_workspace(workspace, student_id, destination, false).map(|(bundle, _)| bundle)
}

fn submit_workspace(
    workspace: &Path,
    student_id: &str,
    destination: Option<&Path>,
    allow_incomplete: bool,
) -> Result<(BundleOutput, Option<(String, String)>)> {
    validate_student_id(student_id)?;
    let mut status = ProductionSession::recover_finalization_if_started(workspace)
        .map_err(submit_resume_error)?;
    if status.is_none() {
        let session = (|| -> Result<ProductionSession> {
            let pinned = PinnedWorkspaceRoot::open(workspace)?;
            let mut inspection = pinned.open_state_directory()?.lock_for_inspection()?;
            let manifest = inspection.read_artifact("manifest.toml", METADATA_LIMIT)?;
            inspection.release_ownership()?;
            drop(inspection);
            ProductionSession::resume(workspace, &manifest, ResumeChoice::Resume)
        })()
        .map_err(submit_resume_error)?;
        status = Some(match session.finalize(student_id) {
            Ok(receipt) => FinalizationStatus::Finalized(Box::new(receipt)),
            Err(finalize_error) => match ProductionSession::recover_finalization(workspace) {
                Ok(incomplete @ FinalizationStatus::Incomplete(_)) => incomplete,
                Ok(FinalizationStatus::Finalized(_)) | Err(_) => return Err(finalize_error),
            },
        });
    }

    let status = status.ok_or("finalization status is unavailable")?;
    let (recorded_student_id, assignment_id) = match &status {
        FinalizationStatus::Finalized(receipt) => (
            receipt.manifest().student_id.clone(),
            receipt.manifest().assignment_id.clone(),
        ),
        FinalizationStatus::Incomplete(incomplete) => {
            let Some(manifest) = incomplete.manifest() else {
                return Err(format!(
                    "{}: {}; no bundle was created",
                    incomplete.label, incomplete.reason
                )
                .into());
            };
            manifest.validate()?;
            if !allow_incomplete {
                return Err(format!(
                    "{}: {}; no bundle was created; rerun with --allow-incomplete to create a visibly marked incomplete recovery export",
                    incomplete.label, incomplete.reason
                )
                .into());
            }
            (manifest.student_id.clone(), manifest.assignment_id.clone())
        }
    };
    if recorded_student_id != student_id {
        return Err(format!(
            "supplied student ID differs from finalized receipt student ID {}",
            recorded_student_id
        )
        .into());
    }
    let destination = destination.map_or_else(
        || {
            workspace
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .join(format!("{recorded_student_id}-{assignment_id}.zip"))
        },
        Path::to_path_buf,
    );
    match status {
        FinalizationStatus::Finalized(receipt) => {
            create_bundle(&receipt, &destination).map(|bundle| (bundle, None))
        }
        FinalizationStatus::Incomplete(incomplete) => {
            let label = incomplete.label.clone();
            let reason = incomplete.reason.clone();
            create_incomplete_bundle(workspace, &incomplete, &destination)
                .map(|bundle| (bundle, Some((label, reason))))
        }
    }
}

fn submit_resume_error(error: impl std::fmt::Display) -> String {
    format!(
        "startup/session cannot resume: {error}; original preserved. Your code and recorded history are intact: fix the cause above and run the same command again; --inspect shows the preserved views. If it still cannot resume, start a new workspace with `rustrace work ASSIGNMENT.rta --workspace NEW.work` and tell your course staff; this workspace stays preserved for inspection"
    )
}

pub fn run_submit(args: &[String], output: &mut impl Write) -> Result<()> {
    let workspace = args.first().ok_or(
        "Usage: rustrace submit WORKSPACE --student-id ID [--allow-incomplete] [--output PATH]",
    )?;
    let mut student_id = None;
    let mut destination = None;
    let mut allow_incomplete = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--student-id" if student_id.is_none() => {
                index += 1;
                student_id = Some(
                    args.get(index)
                        .ok_or("--student-id requires an identifier")?
                        .as_str(),
                );
            }
            "--output" if destination.is_none() => {
                index += 1;
                destination = Some(Path::new(
                    args.get(index).ok_or("--output requires a path")?,
                ));
            }
            "--allow-incomplete" if !allow_incomplete => allow_incomplete = true,
            _ => {
                return Err("Usage: rustrace submit WORKSPACE --student-id ID [--allow-incomplete] [--output PATH]".into());
            }
        }
        index += 1;
    }
    let student_id = student_id.ok_or(
        "Usage: rustrace submit WORKSPACE --student-id ID [--allow-incomplete] [--output PATH]",
    )?;
    let (bundle, incomplete) = submit_workspace(
        Path::new(workspace),
        student_id,
        destination,
        allow_incomplete,
    )?;
    let is_incomplete = incomplete.is_some();
    if let Some((label, reason)) = incomplete {
        writeln!(output, "INCOMPLETE RECOVERY EXPORT")?;
        writeln!(output, "{label}: {}", crate::display::label(&reason, 4096))?;
    }
    writeln!(
        output,
        "{}  {} ({})",
        bundle.blake3,
        crate::display::label_fmt(format_args!("{}", bundle.path.display()), 4096),
        if is_incomplete {
            "local incomplete recovery artifact only; this does not pass clean verification or mean a successful LMS hand-in"
        } else {
            "local artifact only; this does not mean a successful LMS hand-in"
        }
    )?;
    Ok(())
}

fn validate_student_id(student_id: &str) -> Result<()> {
    if student_id.is_empty() || student_id.len() > rustrace_model::MAX_IDENTIFIER_BYTES {
        return Err(format!(
            "student ID must contain 1..={} bytes",
            rustrace_model::MAX_IDENTIFIER_BYTES
        )
        .into());
    }
    if !student_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("student ID must use ASCII letters, digits, '-', '_' or '.'".into());
    }
    Ok(())
}

fn write_rprov(
    manifest: &RprovManifest,
    mut read_payload: impl FnMut(&str) -> Result<Vec<u8>>,
    output: &mut File,
) -> Result<()> {
    let manifest_bytes = encode_rprov_manifest(manifest)?;
    let mut stored_records_bytes =
        framed_record_bytes("manifest.json", manifest_bytes.len() as u64)?;
    for entry in &manifest.inventory {
        stored_records_bytes = stored_records_bytes
            .checked_add(framed_record_bytes(&entry.path, entry.byte_length)?)
            .ok_or(".rprov record-byte total overflow")?;
    }
    let entry_count = u32::try_from(manifest.inventory.len() + 1)?;
    let header = RprovContainerHeader {
        format_version: RPROV_FORMAT_VERSION_V1,
        entry_count,
        stored_records_bytes,
        expanded_records_bytes: stored_records_bytes,
    };
    output.write_all(&encode_rprov_container_header(&header)?)?;
    write_rprov_record(output, "manifest.json", &manifest_bytes)?;
    for declaration in &manifest.inventory {
        let payload = read_payload(&declaration.path)?;
        if payload.len() as u64 != declaration.byte_length
            || rprov_raw_blake3(&payload) != declaration.blake3
        {
            return Err(format!(
                "receipt payload {} differs from its finalized inventory",
                declaration.path
            )
            .into());
        }
        write_rprov_record(output, &declaration.path, &payload)?;
    }
    Ok(())
}

fn framed_record_bytes(path: &str, payload_bytes: u64) -> Result<u64> {
    Ok((RPROV_RECORD_HEADER_BYTES as u64)
        .checked_add(path.len() as u64)
        .and_then(|bytes| bytes.checked_add(payload_bytes))
        .ok_or(".rprov record-byte total overflow")?)
}

fn write_rprov_record(output: &mut File, path: &str, payload: &[u8]) -> Result<()> {
    let header = RprovRecordHeader {
        path_bytes: u16::try_from(path.len())?,
        entry_type: RprovRecordType::RegularFile,
        payload_bytes: payload.len() as u64,
    };
    output.write_all(&encode_rprov_record_header(&header)?)?;
    output.write_all(path.as_bytes())?;
    output.write_all(payload)?;
    Ok(())
}

#[derive(Clone, Debug)]
enum ZipSource {
    Workspace(rustrace_model::WorkspacePath),
    Rprov,
}

#[derive(Clone, Debug)]
struct ZipEntry {
    name: String,
    source: ZipSource,
    crc32: u32,
    byte_length: u32,
    local_offset: u32,
}

fn write_zip(
    workspace: &std::collections::BTreeMap<rustrace_model::WorkspacePath, Vec<u8>>,
    rprov: &mut File,
    output: &mut File,
) -> Result<()> {
    let rprov_length = u32::try_from(rprov.metadata()?.len())?;
    let mut entries = workspace
        .iter()
        .map(|(path, bytes)| {
            Ok(ZipEntry {
                name: path.as_str().to_owned(),
                source: ZipSource::Workspace(path.clone()),
                crc32: crc32fast::hash(bytes),
                byte_length: u32::try_from(bytes.len())?,
                local_offset: 0,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    entries.push(ZipEntry {
        name: RPROV_ENTRY.to_owned(),
        source: ZipSource::Rprov,
        crc32: crc32_file(rprov)?,
        byte_length: rprov_length,
        local_offset: 0,
    });
    entries.sort_unstable_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));

    let mut offset = 0_u64;
    for entry in &mut entries {
        entry.local_offset = u32::try_from(offset)?;
        let name = entry.name.as_bytes();
        let name_length = u16::try_from(name.len())?;
        let mut header = [0_u8; 30];
        put_u32(&mut header, 0, ZIP_LOCAL_MAGIC);
        put_u16(&mut header, 4, ZIP_VERSION);
        put_u16(&mut header, 6, ZIP_FLAG_UTF8);
        put_u16(&mut header, 8, ZIP_METHOD_STORED);
        put_u16(&mut header, 10, ZIP_FIXED_TIME);
        put_u16(&mut header, 12, ZIP_FIXED_DATE);
        put_u32(&mut header, 14, entry.crc32);
        put_u32(&mut header, 18, entry.byte_length);
        put_u32(&mut header, 22, entry.byte_length);
        put_u16(&mut header, 26, name_length);
        output.write_all(&header)?;
        output.write_all(name)?;
        match &entry.source {
            ZipSource::Workspace(path) => output.write_all(&workspace[path])?,
            ZipSource::Rprov => copy_file(rprov, output)?,
        }
        offset = offset
            .checked_add(30 + name.len() as u64 + u64::from(entry.byte_length))
            .ok_or("ZIP local-entry offset overflow")?;
    }

    let central_offset = offset;
    for entry in &entries {
        let name = entry.name.as_bytes();
        let name_length = u16::try_from(name.len())?;
        let mut header = [0_u8; 46];
        put_u32(&mut header, 0, ZIP_CENTRAL_MAGIC);
        put_u16(&mut header, 4, ZIP_MADE_BY_UNIX);
        put_u16(&mut header, 6, ZIP_VERSION);
        put_u16(&mut header, 8, ZIP_FLAG_UTF8);
        put_u16(&mut header, 10, ZIP_METHOD_STORED);
        put_u16(&mut header, 12, ZIP_FIXED_TIME);
        put_u16(&mut header, 14, ZIP_FIXED_DATE);
        put_u32(&mut header, 16, entry.crc32);
        put_u32(&mut header, 20, entry.byte_length);
        put_u32(&mut header, 24, entry.byte_length);
        put_u16(&mut header, 28, name_length);
        put_u32(&mut header, 38, ZIP_REGULAR_MODE);
        put_u32(&mut header, 42, entry.local_offset);
        output.write_all(&header)?;
        output.write_all(name)?;
        offset = offset
            .checked_add(46 + name.len() as u64)
            .ok_or("ZIP central-directory offset overflow")?;
    }

    let central_size = offset
        .checked_sub(central_offset)
        .ok_or("ZIP central-directory size underflow")?;
    let entry_count = u16::try_from(entries.len())?;
    let mut end = [0_u8; 22];
    put_u32(&mut end, 0, ZIP_END_MAGIC);
    put_u16(&mut end, 8, entry_count);
    put_u16(&mut end, 10, entry_count);
    put_u32(&mut end, 12, u32::try_from(central_size)?);
    put_u32(&mut end, 16, u32::try_from(central_offset)?);
    output.write_all(&end)?;
    Ok(())
}

fn put_u16(target: &mut [u8], offset: usize, value: u16) {
    target[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(target: &mut [u8], offset: usize, value: u32) {
    target[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn copy_file(source: &mut File, output: &mut File) -> Result<()> {
    source.seek(SeekFrom::Start(0))?;
    let mut source = BufReader::new(source);
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        output.write_all(&buffer[..read])?;
    }
}

fn crc32_file(source: &mut File) -> Result<u32> {
    source.seek(SeekFrom::Start(0))?;
    let mut source = BufReader::new(source);
    let mut hasher = crc32fast::Hasher::new();
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            return Ok(hasher.finalize());
        }
        hasher.update(&buffer[..read]);
    }
}

fn hash_file(source: &mut File) -> Result<Hash> {
    source.seek(SeekFrom::Start(0))?;
    let mut source = BufReader::new(source);
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            return Ok(Hash::from_bytes(*hasher.finalize().as_bytes()));
        }
        hasher.update(&buffer[..read]);
    }
}

struct TemporaryFile {
    path: Option<PathBuf>,
    file: Option<File>,
}

impl TemporaryFile {
    fn create(parent: &Path, tag: &str) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..64 {
            let counter = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                ".rustrace-submit-{}-{counter}-{tag}.tmp",
                std::process::id()
            ));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        path: Some(path),
                        file: Some(file),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err("could not create a unique bundle temporary file".into())
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("temporary path is retained")
    }

    fn writer(&mut self) -> Result<&mut File> {
        self.file_mut()
    }

    fn file_mut(&mut self) -> Result<&mut File> {
        self.file
            .as_mut()
            .ok_or_else(|| "temporary file descriptor is unavailable".into())
    }

    fn rewind(&mut self) -> Result<()> {
        self.file_mut()?.seek(SeekFrom::Start(0))?;
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        self.file
            .as_ref()
            .ok_or("temporary file descriptor is unavailable")?
            .sync_all()?;
        Ok(())
    }

    fn install_no_clobber(mut self, destination: &Path) -> Result<()> {
        let path = self.path();
        if let Err(error) = fs::hard_link(path, destination) {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                return Err(format!(
                    "bundle destination already exists and was not replaced: {}",
                    destination.display()
                )
                .into());
            }
            return Err(format!(
                "could not atomically install bundle at {}: {error}",
                destination.display()
            )
            .into());
        }
        if let Err(error) = self.verify_installed_identity(destination) {
            let rollback = fs::remove_file(destination);
            return Err(format!(
                "bundle installation identity check failed ({error}); destination rollback {}",
                if rollback.is_ok() {
                    "succeeded"
                } else {
                    "failed"
                }
            )
            .into());
        }
        if let Err(error) = fs::remove_file(path) {
            let rollback = fs::remove_file(destination);
            return Err(format!(
                "bundle temporary-link cleanup failed ({error}); destination rollback {}",
                if rollback.is_ok() {
                    "succeeded"
                } else {
                    "failed"
                }
            )
            .into());
        }
        self.path = None;
        Ok(())
    }

    #[cfg(unix)]
    fn verify_installed_identity(&self, destination: &Path) -> Result<()> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

        let installed = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(destination)?;
        let source = self
            .file
            .as_ref()
            .ok_or("temporary file descriptor is unavailable")?
            .metadata()?;
        let installed = installed.metadata()?;
        if !source.file_type().is_file()
            || !installed.file_type().is_file()
            || source.dev() != installed.dev()
            || source.ino() != installed.ino()
        {
            return Err("installed file does not match the validated temporary descriptor".into());
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn verify_installed_identity(&self, _destination: &Path) -> Result<()> {
        Err("bundle installation identity checks are unsupported on this platform".into())
    }
}

fn cleanup_stale_temporaries(parent: &Path, tag: &str) -> Result<()> {
    let suffix = format!("-{tag}.tmp");
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.ends_with(&suffix) || !super::retention::is_stale_submit_temporary(name) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_file() {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}
