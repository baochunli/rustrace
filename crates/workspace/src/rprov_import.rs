//! Hostile-input importer for standalone `.rprov` files and fixed LMS ZIPs.
//!
//! Imported bytes stay in private owned spools. The importer never extracts an
//! archive path, follows a package-supplied link, or executes package content.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::PathBuf;

use flate2::bufread::DeflateDecoder;
use rustrace_model::{
    MAX_RPROV_ARCHIVE_ENTRIES, MAX_RPROV_EXPANDED_BYTES, MAX_RPROV_MANIFEST_BYTES,
    MAX_RPROV_RECORD_PAYLOAD_BYTES, MAX_RPROV_STORED_BYTES, RPROV_CONTAINER_HEADER_BYTES,
    RPROV_RECORD_HEADER_BYTES, RprovArchiveEntry, RprovArchiveEntryType, RprovEntryKind,
    RprovError, RprovInventoryEntry, RprovManifest, WorkspacePath, decode_rprov_container_header,
    decode_rprov_manifest, decode_rprov_record_header, review_rprov_event_stream_reader,
    validate_rprov_event_stream_reader, validate_rprov_layout, validate_rprov_payload,
};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

use crate::hash::{MAX_WORKSPACE_FILE_BYTES, MAX_WORKSPACE_FILES, MAX_WORKSPACE_TOTAL_BYTES};

const COPY_BUFFER_BYTES: usize = 8 * 1024;
const LMS_RPROV_PATH: &str = "session.rprov";
const ZIP_LOCAL_HEADER_BYTES: u64 = 30;
const ZIP_CENTRAL_HEADER_BYTES: u64 = 46;
const ZIP_END_BYTES: u64 = 22;
const ZIP_LOCAL_MAGIC: u32 = 0x0403_4b50;
const ZIP_CENTRAL_MAGIC: u32 = 0x0201_4b50;
const ZIP_END_MAGIC: u32 = 0x0605_4b50;
const ZIP_FLAG_UTF8: u16 = 1 << 11;
const ZIP_METHOD_STORED: u16 = 0;
const ZIP_METHOD_DEFLATED: u16 = 8;
const ZIP_DOS_VOLUME_LABEL: u32 = 0x08;
const ZIP_DOS_DIRECTORY: u32 = 0x10;
const UNIX_FILE_TYPE_MASK: u32 = 0o170000;
const UNIX_REGULAR_FILE: u32 = 0o100000;
const UNIX_DIRECTORY: u32 = 0o040000;

/// The physical wrapper accepted by the importer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportedPackageKind {
    StandaloneRprov,
    LmsZip,
}

/// One exact record in the imported `.rprov` container.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedRprovEntry {
    pub path: String,
    pub byte_length: u64,
    pub kind: Option<RprovEntryKind>,
}

/// One regular file from the latest outer submitted source tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedSourceFile {
    pub path: WorkspacePath,
    pub byte_length: u64,
}

/// A narrow evidence-only defect retained for read-only review.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportedRprovIssue {
    MissingExternalEvidence {
        segment: u32,
        sequence: u64,
        detail: String,
    },
    ExternalEvidencePayloadDigest {
        entry: String,
    },
    EventStreamValidation {
        error: RprovError,
    },
}

#[derive(Clone, Copy, Debug)]
struct StoredRange {
    offset: u64,
    length: u64,
}

/// A bounded exact-byte reader tied to an owned private spool.
#[derive(Debug)]
pub struct ImportedEntryReader {
    file: File,
    next: u64,
    remaining: u64,
}

impl ImportedEntryReader {
    fn new(file: &File, range: StoredRange) -> Result<Self, RprovImportError> {
        Ok(Self {
            file: file.try_clone().map_err(|source| RprovImportError::Io {
                context: "private package spool",
                source,
            })?,
            next: range.offset,
            remaining: range.length,
        })
    }
}

impl Read for ImportedEntryReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buffer.is_empty() {
            return Ok(0);
        }
        let wanted = usize::try_from(self.remaining.min(buffer.len() as u64)).unwrap();
        let read = positional_read(&self.file, &mut buffer[..wanted], self.next)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "private package spool ended inside an entry",
            ));
        }
        self.next += read as u64;
        self.remaining -= read as u64;
        Ok(read)
    }
}

/// A structurally validated package and its owned exact-byte spools.
#[derive(Debug)]
pub struct ImportedRprov {
    kind: ImportedPackageKind,
    manifest: RprovManifest,
    entries: Vec<ImportedRprovEntry>,
    entry_ranges: BTreeMap<String, StoredRange>,
    inner_spool: PrivateSpool,
    outer_source_files: Vec<ImportedSourceFile>,
    outer_source_ranges: BTreeMap<WorkspacePath, StoredRange>,
    outer_source_spool: Option<PrivateSpool>,
    issues: Vec<ImportedRprovIssue>,
}

impl ImportedRprov {
    pub fn kind(&self) -> ImportedPackageKind {
        self.kind
    }

    pub fn manifest(&self) -> &RprovManifest {
        &self.manifest
    }

    pub fn entries(&self) -> &[ImportedRprovEntry] {
        &self.entries
    }

    pub fn outer_source_files(&self) -> &[ImportedSourceFile] {
        &self.outer_source_files
    }

    pub fn issues(&self) -> &[ImportedRprovIssue] {
        &self.issues
    }

    pub fn open_entry(&self, path: &str) -> Result<ImportedEntryReader, RprovImportError> {
        let range = self
            .entry_ranges
            .get(path)
            .copied()
            .ok_or(RprovImportError::MissingEntry)?;
        ImportedEntryReader::new(&self.inner_spool.file, range)
    }

    /// Opens one checked subrange of an imported entry without rescanning or
    /// exposing the private spool path.
    pub fn open_entry_range(
        &self,
        path: &str,
        offset: u64,
        length: u64,
    ) -> Result<ImportedEntryReader, RprovImportError> {
        let entry = self
            .entry_ranges
            .get(path)
            .copied()
            .ok_or(RprovImportError::MissingEntry)?;
        offset
            .checked_add(length)
            .filter(|end| *end <= entry.length)
            .ok_or(RprovImportError::InvalidRprov {
                field: "entry range",
            })?;
        let start = entry
            .offset
            .checked_add(offset)
            .ok_or(RprovImportError::InvalidRprov {
                field: "entry range",
            })?;
        ImportedEntryReader::new(
            &self.inner_spool.file,
            StoredRange {
                offset: start,
                length,
            },
        )
    }

    pub fn open_outer_source(
        &self,
        path: &WorkspacePath,
    ) -> Result<ImportedEntryReader, RprovImportError> {
        let range = self
            .outer_source_ranges
            .get(path)
            .copied()
            .ok_or(RprovImportError::MissingEntry)?;
        let spool = self
            .outer_source_spool
            .as_ref()
            .ok_or(RprovImportError::MissingEntry)?;
        ImportedEntryReader::new(&spool.file, range)
    }
}

#[derive(Debug)]
pub enum RprovImportError {
    Io {
        context: &'static str,
        source: io::Error,
    },
    Model(RprovError),
    InvalidRprov {
        field: &'static str,
    },
    PayloadDigest {
        entry: String,
        kind: RprovEntryKind,
    },
    Truncated {
        layer: &'static str,
    },
    InvalidOuterArchive {
        detail: &'static str,
    },
    UnsafeOuterPath,
    UnsupportedOuterFeature,
    LimitExceeded {
        field: &'static str,
        actual: u64,
        maximum: u64,
    },
    MissingEntry,
}

impl fmt::Display for RprovImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => {
                write!(formatter, "failed to read {context}: {}", source.kind())
            }
            Self::Model(source) => write!(formatter, "invalid .rprov structure: {source}"),
            Self::InvalidRprov { field } => {
                write!(formatter, "invalid .rprov structure in {field}")
            }
            Self::PayloadDigest { entry, .. } => {
                write!(formatter, "invalid .rprov payload digest for {entry}")
            }
            Self::Truncated { layer } => write!(formatter, "truncated {layer}"),
            Self::InvalidOuterArchive { detail } => {
                write!(formatter, "invalid outer LMS ZIP: {detail}")
            }
            Self::UnsafeOuterPath => formatter.write_str("unsafe outer LMS ZIP path"),
            Self::UnsupportedOuterFeature => {
                formatter.write_str("unsupported outer LMS ZIP feature")
            }
            Self::LimitExceeded {
                field,
                actual,
                maximum,
            } => write!(formatter, "{field} is {actual}; maximum is {maximum}"),
            Self::MissingEntry => formatter.write_str("package entry is unavailable"),
        }
    }
}

impl std::error::Error for RprovImportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Model(source) => Some(source),
            _ => None,
        }
    }
}

impl From<RprovError> for RprovImportError {
    fn from(source: RprovError) -> Self {
        match source {
            RprovError::InvalidManifest { .. } => Self::InvalidRprov {
                field: "manifest.json",
            },
            RprovError::InvalidField { field, .. } => Self::InvalidRprov { field },
            source => Self::Model(source),
        }
    }
}

/// Imports hostile bytes without extracting or executing package content.
pub fn import_rprov<R: Read>(source: R) -> Result<ImportedRprov, RprovImportError> {
    import_rprov_with_mode(source, false)
}

/// Imports a package for read-only review while retaining bounded package
/// state for external-evidence defects that do not invalidate the event chain.
pub fn import_rprov_for_review<R: Read>(source: R) -> Result<ImportedRprov, RprovImportError> {
    import_rprov_with_mode(source, true)
}

fn import_rprov_with_mode<R: Read>(
    mut source: R,
    retain_evidence_issues: bool,
) -> Result<ImportedRprov, RprovImportError> {
    let mut prefix = [0_u8; 4];
    read_exact_input(&mut source, &mut prefix, "package prefix")?;
    if prefix == *b"RUST" {
        import_standalone(source, prefix, retain_evidence_issues)
    } else if prefix == ZIP_LOCAL_MAGIC.to_le_bytes() {
        import_outer_zip(source, prefix, retain_evidence_issues)
    } else {
        Err(RprovImportError::InvalidOuterArchive {
            detail: "input is neither a standalone .rprov nor the fixed LMS ZIP",
        })
    }
}

fn import_standalone<R: Read>(
    source: R,
    prefix: [u8; 4],
    retain_evidence_issues: bool,
) -> Result<ImportedRprov, RprovImportError> {
    let mut spool = PrivateSpool::create()?;
    let parsed = parse_rprov_stream(
        source,
        Some(prefix),
        &mut spool,
        true,
        0,
        retain_evidence_issues,
    )?;
    Ok(ImportedRprov {
        kind: ImportedPackageKind::StandaloneRprov,
        manifest: parsed.manifest,
        entries: parsed.entries,
        entry_ranges: parsed.ranges,
        inner_spool: spool,
        outer_source_files: vec![],
        outer_source_ranges: BTreeMap::new(),
        outer_source_spool: None,
        issues: parsed.issues,
    })
}

fn import_outer_zip<R: Read>(
    source: R,
    prefix: [u8; 4],
    retain_evidence_issues: bool,
) -> Result<ImportedRprov, RprovImportError> {
    let mut archive_spool = PrivateSpool::create()?;
    spool_outer_archive(source, prefix, &mut archive_spool)?;
    let archive_length = spool_length(&archive_spool.file)?;
    let directory = parse_outer_directory(&archive_spool.file, archive_length)?;
    if directory.entries.len() >= MAX_RPROV_ARCHIVE_ENTRIES {
        return Err(RprovImportError::LimitExceeded {
            field: "aggregate outer and inner entries",
            actual: directory.entries.len() as u64 + 1,
            maximum: MAX_RPROV_ARCHIVE_ENTRIES as u64,
        });
    }

    let mut inner_spool = PrivateSpool::create()?;
    let mut source_spool = PrivateSpool::create()?;
    let mut source_ranges = BTreeMap::new();
    let mut source_files = Vec::new();
    for entry in &directory.entries {
        match &entry.kind {
            OuterEntryKind::Rprov => {
                copy_outer_entry(&archive_spool.file, entry, &mut inner_spool.file)?;
            }
            OuterEntryKind::SourceFile(path) => {
                let offset = spool_length(&source_spool.file)?;
                copy_outer_entry(&archive_spool.file, entry, &mut source_spool.file)?;
                source_ranges.insert(
                    path.clone(),
                    StoredRange {
                        offset,
                        length: entry.expanded_bytes,
                    },
                );
                source_files.push(ImportedSourceFile {
                    path: path.clone(),
                    byte_length: entry.expanded_bytes,
                });
            }
            OuterEntryKind::Directory(_) => {}
        }
    }

    let inner_length = spool_length(&inner_spool.file)?;
    let inner_reader = ImportedEntryReader::new(
        &inner_spool.file,
        StoredRange {
            offset: 0,
            length: inner_length,
        },
    )?;
    let parsed = parse_rprov_stream(
        inner_reader,
        None,
        &mut inner_spool,
        false,
        directory.entries.len(),
        retain_evidence_issues,
    )?;

    source_files.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    Ok(ImportedRprov {
        kind: ImportedPackageKind::LmsZip,
        manifest: parsed.manifest,
        entries: parsed.entries,
        entry_ranges: parsed.ranges,
        inner_spool,
        outer_source_files: source_files,
        outer_source_ranges: source_ranges,
        outer_source_spool: Some(source_spool),
        issues: parsed.issues,
    })
}

#[derive(Debug)]
struct ParsedRprov {
    manifest: RprovManifest,
    entries: Vec<ImportedRprovEntry>,
    ranges: BTreeMap<String, StoredRange>,
    issues: Vec<ImportedRprovIssue>,
}

fn parse_rprov_stream<R: Read>(
    source: R,
    prefix: Option<[u8; 4]>,
    spool: &mut PrivateSpool,
    copy_to_spool: bool,
    aggregate_entry_base: usize,
    retain_evidence_issues: bool,
) -> Result<ParsedRprov, RprovImportError> {
    let initial_position = prefix.map_or(0, |_| 4);
    if let Some(prefix) = prefix
        && copy_to_spool
    {
        spool
            .file
            .write_all(&prefix)
            .map_err(|source| RprovImportError::Io {
                context: "private package spool",
                source,
            })?;
    }
    let mut input = ImportReader {
        source,
        spool: copy_to_spool.then_some(&mut spool.file),
        position: initial_position,
    };

    let mut header_bytes = [0_u8; RPROV_CONTAINER_HEADER_BYTES];
    if let Some(prefix) = prefix {
        header_bytes[..4].copy_from_slice(&prefix);
        read_exact_import(
            &mut input,
            &mut header_bytes[4..12],
            "the .rprov version prefix",
        )?;
        reject_unknown_container_version(&header_bytes[..12])?;
        read_exact_import(&mut input, &mut header_bytes[12..], "the .rprov header")?;
    } else {
        read_exact_import(
            &mut input,
            &mut header_bytes[..12],
            "the .rprov version prefix",
        )?;
        reject_unknown_container_version(&header_bytes[..12])?;
        read_exact_import(&mut input, &mut header_bytes[12..], "the .rprov header")?;
    }
    let header = decode_rprov_container_header(&header_bytes)?;
    let expected_file_bytes = header
        .stored_records_bytes
        .checked_add(RPROV_CONTAINER_HEADER_BYTES as u64)
        .ok_or(RprovImportError::Model(RprovError::ArithmeticOverflow {
            field: "stored container bytes",
        }))?;

    let entry_capacity =
        usize::try_from(header.entry_count).map_err(|_| RprovImportError::LimitExceeded {
            field: "archive entries",
            actual: u64::from(header.entry_count),
            maximum: MAX_RPROV_ARCHIVE_ENTRIES as u64,
        })?;
    let aggregate_entries = aggregate_entry_base.checked_add(entry_capacity).ok_or(
        RprovImportError::LimitExceeded {
            field: "aggregate outer and inner entries",
            actual: u64::MAX,
            maximum: MAX_RPROV_ARCHIVE_ENTRIES as u64,
        },
    )?;
    if aggregate_entries > MAX_RPROV_ARCHIVE_ENTRIES {
        return Err(RprovImportError::LimitExceeded {
            field: "aggregate outer and inner entries",
            actual: aggregate_entries as u64,
            maximum: MAX_RPROV_ARCHIVE_ENTRIES as u64,
        });
    }
    let mut entries = Vec::with_capacity(entry_capacity);
    let mut ranges = BTreeMap::new();
    let mut manifest_bytes = None;
    let mut manifest = None;
    let mut accounted_records = 0_u64;
    let mut issues = Vec::new();

    for index in 0..entry_capacity {
        let mut record_bytes = [0_u8; RPROV_RECORD_HEADER_BYTES];
        read_exact_import(&mut input, &mut record_bytes, "a .rprov record header")?;
        let record = decode_rprov_record_header(&record_bytes)?;
        let record_total = (RPROV_RECORD_HEADER_BYTES as u64)
            .checked_add(u64::from(record.path_bytes))
            .and_then(|total| total.checked_add(record.payload_bytes))
            .ok_or(RprovImportError::Model(RprovError::ArithmeticOverflow {
                field: "record bytes",
            }))?;
        accounted_records =
            accounted_records
                .checked_add(record_total)
                .ok_or(RprovImportError::Model(RprovError::ArithmeticOverflow {
                    field: "record bytes",
                }))?;
        if accounted_records > header.stored_records_bytes {
            return Err(RprovImportError::Model(RprovError::InvalidField {
                field: "records bytes",
                detail: "record claims exceed the container total".to_owned(),
            }));
        }

        let mut path_bytes = [0_u8; rustrace_model::MAX_RPROV_ARCHIVE_PATH_BYTES];
        let path_length = usize::from(record.path_bytes);
        read_exact_import(
            &mut input,
            &mut path_bytes[..path_length],
            "a .rprov record path",
        )?;
        let path = std::str::from_utf8(&path_bytes[..path_length])
            .map_err(|_| {
                RprovImportError::Model(RprovError::InvalidField {
                    field: "archive path",
                    detail: "must be valid ASCII".to_owned(),
                })
            })?
            .to_owned();

        if index == 0 {
            if path != "manifest.json" {
                return Err(RprovImportError::Model(RprovError::InvalidField {
                    field: "manifest record",
                    detail: "must be the first record".to_owned(),
                }));
            }
            if record.payload_bytes > MAX_RPROV_MANIFEST_BYTES as u64 {
                return Err(RprovImportError::Model(RprovError::ManifestTooLarge {
                    actual: usize::try_from(record.payload_bytes).unwrap_or(usize::MAX),
                    maximum: MAX_RPROV_MANIFEST_BYTES,
                }));
            }
            let payload_offset = input.position;
            let payload_length = usize::try_from(record.payload_bytes).map_err(|_| {
                RprovImportError::Model(RprovError::ManifestTooLarge {
                    actual: usize::MAX,
                    maximum: MAX_RPROV_MANIFEST_BYTES,
                })
            })?;
            let mut bytes = vec![0_u8; payload_length];
            read_exact_import(&mut input, &mut bytes, "manifest.json")?;
            let decoded = decode_rprov_manifest(&bytes)?;
            if decoded.inventory.len() + 1 != entry_capacity {
                return Err(RprovImportError::Model(RprovError::InvalidField {
                    field: "entry_count",
                    detail: "does not equal manifest inventory plus manifest.json".to_owned(),
                }));
            }
            ranges.insert(
                path.clone(),
                StoredRange {
                    offset: payload_offset,
                    length: record.payload_bytes,
                },
            );
            entries.push(ImportedRprovEntry {
                path,
                byte_length: record.payload_bytes,
                kind: None,
            });
            manifest_bytes = Some(bytes);
            manifest = Some(decoded);
            continue;
        }

        let declaration = manifest
            .as_ref()
            .and_then(|manifest| manifest.inventory.get(index - 1))
            .ok_or(RprovImportError::Model(RprovError::InvalidField {
                field: "payload record",
                detail: "has no matching inventory declaration".to_owned(),
            }))?;
        if declaration.path != path || declaration.byte_length != record.payload_bytes {
            return Err(RprovImportError::Model(RprovError::InvalidField {
                field: "payload record",
                detail: "path or length disagrees with manifest inventory".to_owned(),
            }));
        }
        let payload_offset = input.position;
        let captured = match copy_inner_payload(&mut input, declaration) {
            Ok(captured) => captured,
            Err(RprovImportError::PayloadDigest {
                entry,
                kind: RprovEntryKind::ExternalRecoveryEvidence,
            }) if retain_evidence_issues => {
                issues.push(ImportedRprovIssue::ExternalEvidencePayloadDigest { entry });
                None
            }
            Err(error) => return Err(error),
        };
        if let Some(bytes) = captured.as_deref() {
            validate_rprov_payload(declaration, bytes)?;
        }
        ranges.insert(
            path.clone(),
            StoredRange {
                offset: payload_offset,
                length: record.payload_bytes,
            },
        );
        entries.push(ImportedRprovEntry {
            path,
            byte_length: record.payload_bytes,
            kind: Some(declaration.kind),
        });
    }

    if accounted_records != header.stored_records_bytes || input.position != expected_file_bytes {
        return Err(RprovImportError::Model(RprovError::InvalidField {
            field: "records bytes",
            detail: "enumerated framing does not equal the container total".to_owned(),
        }));
    }
    let mut trailing = [0_u8; 1];
    if read_import(&mut input, &mut trailing, "the end of the .rprov container")? != 0 {
        return Err(RprovImportError::Model(RprovError::InvalidField {
            field: "trailing bytes",
            detail: "version 1 has no trailer".to_owned(),
        }));
    }
    drop(input);

    let manifest = manifest.ok_or(RprovImportError::Model(RprovError::Truncated {
        layer: "manifest record",
    }))?;
    let manifest_bytes = manifest_bytes.unwrap();
    let layout_entries = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| RprovArchiveEntry {
            path: entry.path.clone(),
            entry_type: RprovArchiveEntryType::RegularFile,
            byte_length: entry.byte_length,
            blake3: index
                .checked_sub(1)
                .map(|inventory_index| manifest.inventory[inventory_index].blake3),
        })
        .collect::<Vec<_>>();
    validate_rprov_layout(&header, &manifest_bytes, &manifest, &layout_entries)?;
    for segment in &manifest.segments {
        let range = ranges
            .get(&segment.events.entry)
            .copied()
            .ok_or(RprovImportError::MissingEntry)?;
        let reader = ImportedEntryReader::new(&spool.file, range)?;
        if retain_evidence_issues {
            let review = review_rprov_event_stream_reader(&manifest, segment, reader)?;
            let (missing_external_evidence, remaining_error) = review.into_errors();
            if let Some(RprovError::EventIntegrity {
                kind: rustrace_model::RprovEventIntegrityKind::MissingExternalEvidence,
                segment,
                sequence,
                detail,
            }) = missing_external_evidence
            {
                issues.push(ImportedRprovIssue::MissingExternalEvidence {
                    segment,
                    sequence,
                    detail,
                });
            }
            if let Some(error) = remaining_error {
                issues.push(ImportedRprovIssue::EventStreamValidation { error });
            }
        } else {
            validate_rprov_event_stream_reader(&manifest, segment, reader)?;
        }
    }
    Ok(ParsedRprov {
        manifest,
        entries,
        ranges,
        issues,
    })
}

fn reject_unknown_container_version(prefix: &[u8]) -> Result<(), RprovImportError> {
    if let Err(error) = decode_rprov_container_header(prefix)
        && matches!(error, RprovError::UnsupportedFormatVersion { .. })
    {
        return Err(error.into());
    }
    Ok(())
}

fn copy_inner_payload<R: Read>(
    input: &mut ImportReader<'_, R>,
    declaration: &RprovInventoryEntry,
) -> Result<Option<Vec<u8>>, RprovImportError> {
    let capture = matches!(
        declaration.kind,
        RprovEntryKind::Checkpoint | RprovEntryKind::RuntimeMetadata
    );
    let capture_length = if capture {
        Some(usize::try_from(declaration.byte_length).map_err(|_| {
            RprovImportError::LimitExceeded {
                field: "captured structural payload bytes",
                actual: declaration.byte_length,
                maximum: MAX_RPROV_RECORD_PAYLOAD_BYTES,
            }
        })?)
    } else {
        None
    };
    let mut captured = capture_length.map(Vec::with_capacity);
    let mut hasher = blake3::Hasher::new();
    let mut remaining = declaration.byte_length;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(COPY_BUFFER_BYTES as u64)).unwrap();
        read_exact_import(input, &mut buffer[..wanted], "a .rprov payload")?;
        hasher.update(&buffer[..wanted]);
        if let Some(bytes) = &mut captured {
            bytes.extend_from_slice(&buffer[..wanted]);
        }
        remaining -= wanted as u64;
    }
    if rustrace_model::Hash::from_bytes(*hasher.finalize().as_bytes()) != declaration.blake3 {
        return Err(RprovImportError::PayloadDigest {
            entry: declaration.path.clone(),
            kind: declaration.kind,
        });
    }
    Ok(captured)
}

struct ImportReader<'a, R> {
    source: R,
    spool: Option<&'a mut File>,
    position: u64,
}

impl<R: Read> Read for ImportReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.source.read(buffer)?;
        if let Some(spool) = &mut self.spool {
            spool.write_all(&buffer[..read])?;
        }
        self.position = self
            .position
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("package input position overflow"))?;
        Ok(read)
    }
}

fn read_exact_import<R: Read>(
    input: &mut ImportReader<'_, R>,
    buffer: &mut [u8],
    layer: &'static str,
) -> Result<(), RprovImportError> {
    let mut offset = 0;
    while offset < buffer.len() {
        let read = read_import(input, &mut buffer[offset..], layer)?;
        if read == 0 {
            return Err(RprovImportError::Truncated { layer });
        }
        offset += read;
    }
    Ok(())
}

fn read_import<R: Read>(
    input: &mut ImportReader<'_, R>,
    buffer: &mut [u8],
    layer: &'static str,
) -> Result<usize, RprovImportError> {
    loop {
        match input.read(buffer) {
            Ok(read) => return Ok(read),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(RprovImportError::Io {
                    context: layer,
                    source: io::Error::from(error.kind()),
                });
            }
        }
    }
}

#[derive(Debug)]
struct OuterDirectory {
    entries: Vec<OuterEntry>,
}

#[derive(Clone, Debug)]
struct OuterEntry {
    kind: OuterEntryKind,
    version_needed: u16,
    flags: u16,
    method: u16,
    modified_time: u16,
    modified_date: u16,
    crc32: u32,
    compressed_bytes: u64,
    expanded_bytes: u64,
    local_offset: u64,
    data_offset: u64,
    raw_name: Vec<u8>,
}

#[derive(Clone, Debug)]
enum OuterEntryKind {
    Rprov,
    SourceFile(WorkspacePath),
    Directory(WorkspacePath),
}

fn spool_outer_archive<R: Read>(
    mut source: R,
    prefix: [u8; 4],
    spool: &mut PrivateSpool,
) -> Result<(), RprovImportError> {
    spool
        .file
        .write_all(&prefix)
        .map_err(|source| RprovImportError::Io {
            context: "private outer ZIP spool",
            source,
        })?;
    let mut total = prefix.len() as u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let allowance = MAX_RPROV_STORED_BYTES
            .checked_sub(total)
            .and_then(|remaining| remaining.checked_add(1))
            .unwrap_or(1);
        let wanted = usize::try_from(allowance.min(COPY_BUFFER_BYTES as u64)).unwrap();
        let read = read_some_input(&mut source, &mut buffer[..wanted], "outer LMS ZIP")?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or(RprovImportError::LimitExceeded {
                field: "outer ZIP stored bytes",
                actual: u64::MAX,
                maximum: MAX_RPROV_STORED_BYTES,
            })?;
        if total > MAX_RPROV_STORED_BYTES {
            return Err(RprovImportError::LimitExceeded {
                field: "outer ZIP stored bytes",
                actual: total,
                maximum: MAX_RPROV_STORED_BYTES,
            });
        }
        spool
            .file
            .write_all(&buffer[..read])
            .map_err(|source| RprovImportError::Io {
                context: "private outer ZIP spool",
                source,
            })?;
    }
    Ok(())
}

fn parse_outer_directory(
    archive: &File,
    archive_length: u64,
) -> Result<OuterDirectory, RprovImportError> {
    if archive_length < ZIP_END_BYTES {
        return Err(RprovImportError::Truncated {
            layer: "outer LMS ZIP end record",
        });
    }
    let end_offset = archive_length - ZIP_END_BYTES;
    let mut end = [0_u8; ZIP_END_BYTES as usize];
    read_at_exact(archive, end_offset, &mut end, "outer LMS ZIP end record")?;
    if u32_at(&end, 0) != ZIP_END_MAGIC {
        return Err(RprovImportError::InvalidOuterArchive {
            detail: "the fixed wrapper requires one exact end record and no comment or trailer",
        });
    }
    if u16_at(&end, 4) != 0
        || u16_at(&end, 6) != 0
        || u16_at(&end, 8) != u16_at(&end, 10)
        || u16_at(&end, 20) != 0
    {
        return Err(RprovImportError::UnsupportedOuterFeature);
    }
    let entry_count = usize::from(u16_at(&end, 10));
    if entry_count == 0 || entry_count > MAX_RPROV_ARCHIVE_ENTRIES {
        return Err(RprovImportError::LimitExceeded {
            field: "outer ZIP entries",
            actual: entry_count as u64,
            maximum: MAX_RPROV_ARCHIVE_ENTRIES as u64,
        });
    }
    let central_size = u64::from(u32_at(&end, 12));
    let central_offset = u64::from(u32_at(&end, 16));
    if central_offset
        .checked_add(central_size)
        .filter(|end| *end == end_offset)
        .is_none()
    {
        return Err(RprovImportError::InvalidOuterArchive {
            detail: "central directory bounds do not match the exact archive",
        });
    }

    let mut cursor = central_offset;
    let mut entries = Vec::with_capacity(entry_count);
    let mut exact_names = HashSet::with_capacity(entry_count);
    let mut alias_names = HashSet::with_capacity(entry_count);
    let mut source_file_aliases = HashSet::new();
    let mut rprov_count = 0_usize;
    let mut source_files = 0_usize;
    let mut source_bytes = 0_u64;
    let mut expanded_bytes = 0_u64;
    let mut compressed_bytes = 0_u64;

    for _ in 0..entry_count {
        let mut header = [0_u8; ZIP_CENTRAL_HEADER_BYTES as usize];
        read_at_exact(archive, cursor, &mut header, "outer ZIP central entry")?;
        if u32_at(&header, 0) != ZIP_CENTRAL_MAGIC {
            return Err(RprovImportError::InvalidOuterArchive {
                detail: "invalid central entry signature",
            });
        }
        let made_by_system = u16_at(&header, 4) >> 8;
        let version_needed = u16_at(&header, 6);
        let flags = u16_at(&header, 8);
        let method = u16_at(&header, 10);
        let modified_time = u16_at(&header, 12);
        let modified_date = u16_at(&header, 14);
        let crc32 = u32_at(&header, 16);
        let compressed = u64::from(u32_at(&header, 20));
        let expanded = u64::from(u32_at(&header, 24));
        let name_length = usize::from(u16_at(&header, 28));
        let extra_length = usize::from(u16_at(&header, 30));
        let comment_length = usize::from(u16_at(&header, 32));
        let starting_disk = u16_at(&header, 34);
        let internal_attributes = u16_at(&header, 36);
        let external_attributes = u32_at(&header, 38);
        let local_offset = u64::from(u32_at(&header, 42));
        if !matches!(made_by_system, 0 | 3)
            || !matches!(
                (method, version_needed),
                (ZIP_METHOD_STORED, 10 | 20) | (ZIP_METHOD_DEFLATED, 20)
            )
            || flags & !ZIP_FLAG_UTF8 != 0
            || !matches!(method, ZIP_METHOD_STORED | ZIP_METHOD_DEFLATED)
            || name_length == 0
            || name_length > rustrace_model::MAX_WORKSPACE_PATH_BYTES + 1
            || extra_length != 0
            || comment_length != 0
            || starting_disk != 0
            || internal_attributes != 0
            || external_attributes & ZIP_DOS_VOLUME_LABEL != 0
        {
            return Err(RprovImportError::UnsupportedOuterFeature);
        }
        cursor = cursor.checked_add(ZIP_CENTRAL_HEADER_BYTES).ok_or(
            RprovImportError::InvalidOuterArchive {
                detail: "central directory offset overflow",
            },
        )?;
        let mut raw_name = vec![0_u8; name_length];
        read_at_exact(archive, cursor, &mut raw_name, "outer ZIP entry name")?;
        cursor = cursor.checked_add(name_length as u64).ok_or(
            RprovImportError::InvalidOuterArchive {
                detail: "central directory name overflow",
            },
        )?;
        let name = std::str::from_utf8(&raw_name).map_err(|_| RprovImportError::UnsafeOuterPath)?;
        if !name.is_ascii() && flags & ZIP_FLAG_UTF8 == 0 {
            return Err(RprovImportError::UnsafeOuterPath);
        }
        if !exact_names.insert(name.to_owned()) {
            return Err(RprovImportError::UnsafeOuterPath);
        }

        let mode_type = (external_attributes >> 16) & UNIX_FILE_TYPE_MASK;
        let directory_name = name.ends_with('/');
        let dos_directory = external_attributes & ZIP_DOS_DIRECTORY != 0;
        let kind = if directory_name {
            if !matches!(mode_type, 0 | UNIX_DIRECTORY)
                || (made_by_system == 0 && !dos_directory)
                || compressed != 0
                || expanded != 0
                || crc32 != 0
                || method != ZIP_METHOD_STORED
            {
                return Err(RprovImportError::UnsupportedOuterFeature);
            }
            let stripped = name.strip_suffix('/').unwrap();
            let path = source_path(stripped)?;
            OuterEntryKind::Directory(path)
        } else {
            if !matches!(mode_type, 0 | UNIX_REGULAR_FILE) || dos_directory {
                return Err(RprovImportError::UnsupportedOuterFeature);
            }
            if name == LMS_RPROV_PATH {
                rprov_count += 1;
                if expanded > MAX_RPROV_STORED_BYTES {
                    return Err(RprovImportError::LimitExceeded {
                        field: "outer .rprov expanded bytes",
                        actual: expanded,
                        maximum: MAX_RPROV_STORED_BYTES,
                    });
                }
                OuterEntryKind::Rprov
            } else {
                reject_nested_archive_name(name)?;
                let path = source_path(name)?;
                source_files =
                    source_files
                        .checked_add(1)
                        .ok_or(RprovImportError::LimitExceeded {
                            field: "outer source files",
                            actual: u64::MAX,
                            maximum: MAX_WORKSPACE_FILES as u64,
                        })?;
                if source_files > MAX_WORKSPACE_FILES {
                    return Err(RprovImportError::LimitExceeded {
                        field: "outer source files",
                        actual: source_files as u64,
                        maximum: MAX_WORKSPACE_FILES as u64,
                    });
                }
                if expanded > MAX_WORKSPACE_FILE_BYTES {
                    return Err(RprovImportError::LimitExceeded {
                        field: "outer source file bytes",
                        actual: expanded,
                        maximum: MAX_WORKSPACE_FILE_BYTES,
                    });
                }
                source_bytes = checked_outer_add(
                    "outer source bytes",
                    source_bytes,
                    expanded,
                    MAX_WORKSPACE_TOTAL_BYTES,
                )?;
                source_file_aliases.insert(alias_key(path.as_str()));
                OuterEntryKind::SourceFile(path)
            }
        };
        let alias_path = match &kind {
            OuterEntryKind::Rprov => LMS_RPROV_PATH,
            OuterEntryKind::SourceFile(path) | OuterEntryKind::Directory(path) => path.as_str(),
        };
        if !alias_names.insert(alias_key(alias_path)) {
            return Err(RprovImportError::UnsafeOuterPath);
        }
        expanded_bytes = checked_outer_add(
            "outer ZIP expanded bytes",
            expanded_bytes,
            expanded,
            MAX_RPROV_EXPANDED_BYTES,
        )?;
        compressed_bytes = checked_outer_add(
            "outer ZIP declared compressed bytes",
            compressed_bytes,
            compressed,
            MAX_RPROV_STORED_BYTES,
        )?;
        entries.push(OuterEntry {
            kind,
            version_needed,
            flags,
            method,
            modified_time,
            modified_date,
            crc32,
            compressed_bytes: compressed,
            expanded_bytes: expanded,
            local_offset,
            data_offset: 0,
            raw_name,
        });
    }
    if cursor != end_offset || rprov_count != 1 {
        return Err(RprovImportError::InvalidOuterArchive {
            detail: "the fixed wrapper requires exactly one root session.rprov",
        });
    }
    reject_file_prefix_aliases(&source_file_aliases, &alias_names)?;
    validate_local_zip_entries(archive, central_offset, &mut entries)?;
    Ok(OuterDirectory { entries })
}

fn source_path(name: &str) -> Result<WorkspacePath, RprovImportError> {
    reject_nested_archive_name(name)?;
    WorkspacePath::new(name).map_err(|_| RprovImportError::UnsafeOuterPath)
}

fn reject_nested_archive_name(name: &str) -> Result<(), RprovImportError> {
    if name.split('/').any(|component| {
        let lower = component.to_ascii_lowercase();
        lower.ends_with(".zip") || lower.ends_with(".rprov")
    }) {
        return Err(RprovImportError::UnsupportedOuterFeature);
    }
    Ok(())
}

fn alias_key(path: &str) -> String {
    path.nfd().case_fold().nfd().collect()
}

fn reject_file_prefix_aliases(
    file_aliases: &HashSet<String>,
    all_aliases: &HashSet<String>,
) -> Result<(), RprovImportError> {
    for path in all_aliases {
        let mut prefix = String::new();
        let mut components = path.split('/').peekable();
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                break;
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if file_aliases.contains(&prefix) {
                return Err(RprovImportError::UnsafeOuterPath);
            }
        }
    }
    Ok(())
}

fn validate_local_zip_entries(
    archive: &File,
    central_offset: u64,
    entries: &mut [OuterEntry],
) -> Result<(), RprovImportError> {
    let mut order = (0..entries.len()).collect::<Vec<_>>();
    order.sort_unstable_by_key(|index| entries[*index].local_offset);
    let mut expected_offset = 0_u64;
    for index in order {
        let entry = &mut entries[index];
        if entry.local_offset != expected_offset {
            return Err(RprovImportError::InvalidOuterArchive {
                detail: "local entries are overlapping, reordered, or contain gaps",
            });
        }
        let mut header = [0_u8; ZIP_LOCAL_HEADER_BYTES as usize];
        read_at_exact(
            archive,
            entry.local_offset,
            &mut header,
            "outer ZIP local entry",
        )?;
        if u32_at(&header, 0) != ZIP_LOCAL_MAGIC
            || u16_at(&header, 4) != entry.version_needed
            || u16_at(&header, 6) != entry.flags
            || u16_at(&header, 8) != entry.method
            || u16_at(&header, 10) != entry.modified_time
            || u16_at(&header, 12) != entry.modified_date
            || u32_at(&header, 14) != entry.crc32
            || u64::from(u32_at(&header, 18)) != entry.compressed_bytes
            || u64::from(u32_at(&header, 22)) != entry.expanded_bytes
            || usize::from(u16_at(&header, 26)) != entry.raw_name.len()
            || u16_at(&header, 28) != 0
        {
            return Err(RprovImportError::InvalidOuterArchive {
                detail: "local and central entry metadata disagree",
            });
        }
        let name_offset = entry
            .local_offset
            .checked_add(ZIP_LOCAL_HEADER_BYTES)
            .ok_or(RprovImportError::InvalidOuterArchive {
                detail: "local entry offset overflow",
            })?;
        let mut local_name = vec![0_u8; entry.raw_name.len()];
        read_at_exact(
            archive,
            name_offset,
            &mut local_name,
            "outer ZIP local entry name",
        )?;
        if local_name != entry.raw_name {
            return Err(RprovImportError::InvalidOuterArchive {
                detail: "local and central entry names disagree",
            });
        }
        entry.data_offset = name_offset.checked_add(entry.raw_name.len() as u64).ok_or(
            RprovImportError::InvalidOuterArchive {
                detail: "local entry data offset overflow",
            },
        )?;
        expected_offset = entry
            .data_offset
            .checked_add(entry.compressed_bytes)
            .filter(|end| *end <= central_offset)
            .ok_or(RprovImportError::InvalidOuterArchive {
                detail: "entry data extends into the central directory",
            })?;
    }
    if expected_offset != central_offset {
        return Err(RprovImportError::InvalidOuterArchive {
            detail: "local entry data does not end at the central directory",
        });
    }
    Ok(())
}

fn copy_outer_entry(
    archive: &File,
    entry: &OuterEntry,
    target: &mut File,
) -> Result<(), RprovImportError> {
    let range = ImportedEntryReader::new(
        archive,
        StoredRange {
            offset: entry.data_offset,
            length: entry.compressed_bytes,
        },
    )?;
    match entry.method {
        ZIP_METHOD_STORED => {
            if entry.compressed_bytes != entry.expanded_bytes {
                return Err(RprovImportError::InvalidOuterArchive {
                    detail: "stored entry sizes disagree",
                });
            }
            copy_decoded(range, target, entry)
        }
        ZIP_METHOD_DEFLATED => {
            let mut decoder =
                DeflateDecoder::new(BufReader::with_capacity(COPY_BUFFER_BYTES, range));
            copy_decoded(&mut decoder, target, entry)?;
            if decoder.total_in() != entry.compressed_bytes
                || decoder.total_out() != entry.expanded_bytes
            {
                return Err(RprovImportError::InvalidOuterArchive {
                    detail: "deflated entry did not consume and produce its exact declared sizes",
                });
            }
            Ok(())
        }
        _ => Err(RprovImportError::UnsupportedOuterFeature),
    }
}

fn copy_decoded<R: Read>(
    mut decoded: R,
    target: &mut File,
    entry: &OuterEntry,
) -> Result<(), RprovImportError> {
    let mut crc = crc32fast::Hasher::new();
    let mut written = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let remaining = entry.expanded_bytes.saturating_sub(written);
        let wanted =
            usize::try_from(remaining.saturating_add(1).min(COPY_BUFFER_BYTES as u64)).unwrap();
        let read = loop {
            match decoded.read(&mut buffer[..wanted]) {
                Ok(read) => break read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    return Err(RprovImportError::InvalidOuterArchive {
                        detail: "compressed entry data is corrupt or truncated",
                    });
                }
            }
        };
        if read == 0 {
            break;
        }
        let attempted =
            written
                .checked_add(read as u64)
                .ok_or(RprovImportError::LimitExceeded {
                    field: "outer entry expanded bytes",
                    actual: u64::MAX,
                    maximum: entry.expanded_bytes,
                })?;
        if attempted > entry.expanded_bytes {
            return Err(RprovImportError::LimitExceeded {
                field: "outer entry expanded bytes",
                actual: attempted,
                maximum: entry.expanded_bytes,
            });
        }
        target
            .write_all(&buffer[..read])
            .map_err(|source| RprovImportError::Io {
                context: "private imported-content spool",
                source,
            })?;
        crc.update(&buffer[..read]);
        written = attempted;
    }
    if written != entry.expanded_bytes || crc.finalize() != entry.crc32 {
        return Err(RprovImportError::InvalidOuterArchive {
            detail: "entry length or CRC32 disagrees with the directory",
        });
    }
    Ok(())
}

fn checked_outer_add(
    field: &'static str,
    current: u64,
    added: u64,
    maximum: u64,
) -> Result<u64, RprovImportError> {
    let attempted = current
        .checked_add(added)
        .ok_or(RprovImportError::LimitExceeded {
            field,
            actual: u64::MAX,
            maximum,
        })?;
    if attempted > maximum {
        return Err(RprovImportError::LimitExceeded {
            field,
            actual: attempted,
            maximum,
        });
    }
    Ok(attempted)
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_at_exact(
    file: &File,
    offset: u64,
    buffer: &mut [u8],
    layer: &'static str,
) -> Result<(), RprovImportError> {
    let mut read_total = 0;
    while read_total < buffer.len() {
        let read = positional_read(file, &mut buffer[read_total..], offset + read_total as u64)
            .map_err(|source| RprovImportError::Io {
                context: layer,
                source,
            })?;
        if read == 0 {
            return Err(RprovImportError::Truncated { layer });
        }
        read_total += read;
    }
    Ok(())
}

#[cfg(unix)]
fn positional_read(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buffer, offset)
}

#[cfg(windows)]
fn positional_read(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buffer, offset)
}

#[cfg(not(any(unix, windows)))]
fn positional_read(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::io::{Seek, SeekFrom};
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(offset))?;
    file.read(buffer)
}

fn read_exact_input<R: Read>(
    source: &mut R,
    buffer: &mut [u8],
    layer: &'static str,
) -> Result<(), RprovImportError> {
    let mut offset = 0;
    while offset < buffer.len() {
        let read = read_some_input(source, &mut buffer[offset..], layer)?;
        if read == 0 {
            return Err(RprovImportError::Truncated { layer });
        }
        offset += read;
    }
    Ok(())
}

fn read_some_input<R: Read>(
    source: &mut R,
    buffer: &mut [u8],
    layer: &'static str,
) -> Result<usize, RprovImportError> {
    loop {
        match source.read(buffer) {
            Ok(read) => return Ok(read),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(RprovImportError::Io {
                    context: layer,
                    source: io::Error::from(error.kind()),
                });
            }
        }
    }
}

#[derive(Debug)]
struct PrivateSpool {
    file: File,
    cleanup_path: Option<PathBuf>,
}

impl PrivateSpool {
    fn create() -> Result<Self, RprovImportError> {
        for _ in 0..16 {
            let mut nonce = [0_u8; 16];
            getrandom::fill(&mut nonce).map_err(|error| RprovImportError::Io {
                context: "private spool entropy",
                source: io::Error::other(error.to_string()),
            })?;
            let name = format!(
                ".rustrace-rprov-{}",
                nonce
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
            let path = std::env::temp_dir().join(name);
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    #[cfg(unix)]
                    {
                        if let Err(source) = fs::remove_file(&path) {
                            drop(file);
                            let _ = fs::remove_file(&path);
                            return Err(RprovImportError::Io {
                                context: "unlink private package spool",
                                source,
                            });
                        }
                        return Ok(Self {
                            file,
                            cleanup_path: None,
                        });
                    }
                    #[cfg(not(unix))]
                    {
                        return Ok(Self {
                            file,
                            cleanup_path: Some(path),
                        });
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(RprovImportError::Io {
                        context: "create private package spool",
                        source,
                    });
                }
            }
        }
        Err(RprovImportError::Io {
            context: "create private package spool",
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not obtain a unique private spool name",
            ),
        })
    }
}

impl Drop for PrivateSpool {
    fn drop(&mut self) {
        if let Some(path) = &self.cleanup_path {
            let _ = fs::remove_file(path);
        }
    }
}

fn spool_length(file: &File) -> Result<u64, RprovImportError> {
    file.metadata()
        .map(|metadata| metadata.len())
        .map_err(|source| RprovImportError::Io {
            context: "private package spool length",
            source,
        })
}
