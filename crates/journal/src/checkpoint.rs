//! Canonical, compressed full-workspace checkpoints.

use std::{error::Error, fmt, io::Write};

use flate2::{Compression, Decompress, FlushDecompress, Status, write::ZlibEncoder};
use rustrace_model::{
    DocumentHash, DocumentId, Event, EventEnvelope, Hash, MAX_IDENTIFIER_BYTES,
    MAX_WORKSPACE_PATH_BYTES, SelectionState, SessionId, WorkspaceCheckpoint, WorkspacePath,
    document_hash,
};
use rustrace_workspace::hash::{
    MAX_WORKSPACE_FILE_BYTES, MAX_WORKSPACE_FILES, MAX_WORKSPACE_TOTAL_BYTES, hash_entries,
};

pub const CHECKPOINT_FORMAT_VERSION_V1: u32 = 1;
pub const MAX_CHECKPOINT_FILES: usize = MAX_WORKSPACE_FILES;
pub const MAX_CHECKPOINT_FILE_BYTES: usize = MAX_WORKSPACE_FILE_BYTES as usize;
pub const MAX_CHECKPOINT_TOTAL_FILE_BYTES: usize = MAX_WORKSPACE_TOTAL_BYTES as usize;
pub const MAX_CHECKPOINT_DOCUMENTS: usize = MAX_CHECKPOINT_FILES;
pub const MAX_CHECKPOINT_SEQUENCE: u64 = i64::MAX as u64 - 1;

const OUTER_MAGIC: &[u8; 8] = b"RUSTCPK\0";
const SNAPSHOT_MAGIC: &[u8; 8] = b"RUSTSNP\0";
const COMPRESSION_ZLIB: u8 = 1;
const COMPRESSION_LEVEL: u32 = 6;
const INTEGRITY_DOMAIN: &[u8] = b"rustrace.checkpoint.compressed.v1\0";
const OUTER_HEADER_BYTES: usize = 8 + 4 + 1 + 8 + 8;
const OUTER_TRAILER_BYTES: usize = Hash::LENGTH;
const SNAPSHOT_FIXED_BYTES: usize = 8 + 4 + 4 + MAX_IDENTIFIER_BYTES + 8 + Hash::LENGTH + 4 + 1 + 4;
const MAX_FILE_RECORD_OVERHEAD: usize = 4 + MAX_WORKSPACE_PATH_BYTES + 8;
const MAX_ACTIVE_DOCUMENT_BYTES: usize = 4 + MAX_IDENTIFIER_BYTES;
const MAX_DOCUMENT_RECORD_BYTES: usize =
    4 + MAX_IDENTIFIER_BYTES + 4 + MAX_WORKSPACE_PATH_BYTES + 8 + 8 + 8 + Hash::LENGTH;

/// Largest accepted canonical uncompressed snapshot.
pub const MAX_CHECKPOINT_EXPANDED_BYTES: usize = SNAPSHOT_FIXED_BYTES
    + MAX_ACTIVE_DOCUMENT_BYTES
    + MAX_CHECKPOINT_TOTAL_FILE_BYTES
    + MAX_CHECKPOINT_FILES * MAX_FILE_RECORD_OVERHEAD
    + MAX_CHECKPOINT_DOCUMENTS * MAX_DOCUMENT_RECORD_BYTES;
/// Largest accepted zlib stream. The margin exceeds DEFLATE's bounded framing
/// overhead for a maximum-size canonical snapshot.
pub const MAX_CHECKPOINT_COMPRESSED_BYTES: usize = MAX_CHECKPOINT_EXPANDED_BYTES + 64 * 1024;
/// Largest complete checkpoint BLOB, including framing and integrity digest.
pub const MAX_CHECKPOINT_ENCODED_BYTES: usize =
    OUTER_HEADER_BYTES + MAX_CHECKPOINT_COMPRESSED_BYTES + OUTER_TRAILER_BYTES;

#[cfg(test)]
thread_local! {
    static CHECKPOINT_PRE_SORT_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointFile {
    pub path: WorkspacePath,
    pub contents: Vec<u8>,
}

/// Caller-supplied metadata for one open document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenDocument {
    pub document_id: DocumentId,
    pub path: WorkspacePath,
    pub selection: SelectionState,
    pub version: u64,
}

/// Owned point-in-time workspace input for background checkpoint creation.
/// The journal writer assigns its event sequence in FIFO order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointInput {
    pub session_id: SessionId,
    pub files: Vec<CheckpointFile>,
    pub active_document: Option<DocumentId>,
    pub documents: Vec<OpenDocument>,
}

/// Validated open-document metadata persisted in a checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointDocument {
    pub document_id: DocumentId,
    pub path: WorkspacePath,
    pub selection: SelectionState,
    pub version: u64,
    pub content_hash: Hash,
}

/// A complete immutable workspace state at the owning event sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointSnapshot {
    format_version: u32,
    session_id: SessionId,
    event_sequence: u64,
    workspace_hash: Hash,
    files: Vec<CheckpointFile>,
    active_document: Option<DocumentId>,
    documents: Vec<CheckpointDocument>,
}

impl CheckpointSnapshot {
    pub fn from_input(
        input: CheckpointInput,
        event_sequence: u64,
    ) -> Result<Self, CheckpointError> {
        Self::new(
            input.session_id,
            event_sequence,
            input.files,
            input.active_document,
            input.documents,
        )
    }

    pub fn new(
        session_id: SessionId,
        event_sequence: u64,
        mut files: Vec<CheckpointFile>,
        active_document: Option<DocumentId>,
        mut documents: Vec<OpenDocument>,
    ) -> Result<Self, CheckpointError> {
        validate_input_counts(event_sequence, files.len(), documents.len())?;
        #[cfg(test)]
        CHECKPOINT_PRE_SORT_COUNT.with(|count| count.set(count.get() + 1));
        files.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        #[cfg(test)]
        CHECKPOINT_PRE_SORT_COUNT.with(|count| count.set(count.get() + 1));
        documents.sort_unstable_by(|left, right| left.document_id.cmp(&right.document_id));
        validate_input(event_sequence, &files, active_document.as_ref(), &documents)?;

        let workspace_hash = hash_entries(
            files
                .iter()
                .map(|file| (&file.path, file.contents.as_slice())),
        )
        .map_err(|error| CheckpointError::WorkspaceHashing {
            detail: error.to_string(),
        })?;
        let mut persisted_documents = Vec::with_capacity(documents.len());
        for document in documents {
            let contents = find_file(&files, &document.path).ok_or_else(|| {
                CheckpointError::DocumentFileMissing {
                    document_id: document.document_id.clone(),
                    path: document.path.clone(),
                }
            })?;
            let text =
                std::str::from_utf8(contents).map_err(|_| CheckpointError::DocumentNotUtf8 {
                    document_id: document.document_id.clone(),
                    path: document.path.clone(),
                })?;
            persisted_documents.push(CheckpointDocument {
                document_id: document.document_id,
                path: document.path,
                selection: document.selection,
                version: document.version,
                content_hash: document_hash(text),
            });
        }

        let snapshot = Self {
            format_version: CHECKPOINT_FORMAT_VERSION_V1,
            session_id,
            event_sequence,
            workspace_hash,
            files,
            active_document,
            documents: persisted_documents,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub const fn format_version(&self) -> u32 {
        self.format_version
    }

    pub const fn event_sequence(&self) -> u64 {
        self.event_sequence
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub const fn workspace_hash(&self) -> Hash {
        self.workspace_hash
    }

    pub fn files(&self) -> &[CheckpointFile] {
        &self.files
    }

    pub fn active_document(&self) -> Option<&DocumentId> {
        self.active_document.as_ref()
    }

    pub fn documents(&self) -> &[CheckpointDocument] {
        &self.documents
    }

    pub fn total_file_bytes(&self) -> usize {
        self.files.iter().map(|file| file.contents.len()).sum()
    }

    pub fn event_payload(&self) -> WorkspaceCheckpoint {
        WorkspaceCheckpoint {
            workspace_hash: self.workspace_hash,
            documents: self
                .documents
                .iter()
                .map(|document| DocumentHash {
                    document_id: document.document_id.clone(),
                    hash: document.content_hash,
                })
                .collect(),
        }
    }

    /// Recomputes all path/content/document invariants.
    pub fn validate(&self) -> Result<(), CheckpointError> {
        if self.format_version != CHECKPOINT_FORMAT_VERSION_V1 {
            return Err(CheckpointError::UnsupportedVersion {
                found: self.format_version,
                supported: CHECKPOINT_FORMAT_VERSION_V1,
            });
        }
        validate_persisted(
            self.event_sequence,
            &self.files,
            self.active_document.as_ref(),
            &self.documents,
        )?;
        let workspace_hash = hash_entries(
            self.files
                .iter()
                .map(|file| (&file.path, file.contents.as_slice())),
        )
        .map_err(|error| CheckpointError::WorkspaceHashing {
            detail: error.to_string(),
        })?;
        if workspace_hash != self.workspace_hash {
            return Err(CheckpointError::WorkspaceHashMismatch {
                expected: workspace_hash,
                actual: self.workspace_hash,
            });
        }
        Ok(())
    }

    /// Cross-checks the snapshot against its canonical owning event.
    pub fn verify_owner(&self, envelope: &EventEnvelope) -> Result<(), CheckpointError> {
        self.validate()?;
        if envelope.session_id != self.session_id {
            return Err(CheckpointError::OwnerMismatch {
                field: "session_id",
            });
        }
        if envelope.sequence != self.event_sequence {
            return Err(CheckpointError::OwnerMismatch { field: "sequence" });
        }
        let Event::WorkspaceCheckpoint(owner) = &envelope.event else {
            return Err(CheckpointError::OwnerMismatch {
                field: "event.type",
            });
        };
        if owner.workspace_hash != self.workspace_hash {
            return Err(CheckpointError::OwnerMismatch {
                field: "workspace_hash",
            });
        }
        if owner.documents != self.event_payload().documents {
            return Err(CheckpointError::OwnerMismatch { field: "documents" });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckpointError {
    UnsupportedVersion {
        found: u32,
        supported: u32,
    },
    InvalidSequence {
        actual: u64,
    },
    TooManyFiles {
        actual: usize,
        maximum: usize,
    },
    FileTooLarge {
        path: WorkspacePath,
        actual: usize,
        maximum: usize,
    },
    WorkspaceTooLarge {
        actual: usize,
        maximum: usize,
    },
    DuplicateFilePath {
        path: WorkspacePath,
    },
    TooManyDocuments {
        actual: usize,
        maximum: usize,
    },
    DuplicateDocumentId {
        document_id: DocumentId,
    },
    DuplicateDocumentPath {
        path: WorkspacePath,
    },
    ActiveDocumentNotOpen {
        document_id: DocumentId,
    },
    DocumentFileMissing {
        document_id: DocumentId,
        path: WorkspacePath,
    },
    DocumentNotUtf8 {
        document_id: DocumentId,
        path: WorkspacePath,
    },
    SelectionOutOfBounds {
        document_id: DocumentId,
        offset: u64,
        length: usize,
    },
    SelectionNotCharBoundary {
        document_id: DocumentId,
        offset: u64,
    },
    DocumentHashMismatch {
        document_id: DocumentId,
        expected: Hash,
        actual: Hash,
    },
    WorkspaceHashMismatch {
        expected: Hash,
        actual: Hash,
    },
    WorkspaceHashing {
        detail: String,
    },
    EncodedTooLarge {
        actual: usize,
        maximum: usize,
    },
    ExpandedTooLarge {
        actual: u64,
        maximum: usize,
    },
    CompressedTooLarge {
        actual: u64,
        maximum: usize,
    },
    Truncated,
    InvalidMagic {
        layer: &'static str,
    },
    UnsupportedCompression {
        found: u8,
    },
    LengthOverflow,
    TrailingData {
        actual: usize,
        expected: usize,
    },
    IntegrityMismatch,
    Decompression {
        detail: String,
    },
    CompressedTrailingData {
        actual: u64,
        expected: usize,
    },
    ExpandedLengthMismatch {
        actual: u64,
        expected: usize,
    },
    Malformed {
        field: &'static str,
        detail: String,
    },
    NonCanonicalEncoding,
    Compression {
        detail: String,
    },
    OwnerMismatch {
        field: &'static str,
    },
}

impl fmt::Display for CheckpointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "checkpoint format version {found} is unsupported; supported version is {supported}"
            ),
            Self::InvalidSequence { actual } => write!(
                formatter,
                "checkpoint sequence {actual} is outside 1..={MAX_CHECKPOINT_SEQUENCE}"
            ),
            Self::TooManyFiles { actual, maximum } => {
                write!(
                    formatter,
                    "checkpoint has {actual} files; maximum is {maximum}"
                )
            }
            Self::FileTooLarge {
                path,
                actual,
                maximum,
            } => write!(
                formatter,
                "checkpoint file {path} is {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::WorkspaceTooLarge { actual, maximum } => write!(
                formatter,
                "checkpoint file contents total {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::DuplicateFilePath { path } => {
                write!(formatter, "checkpoint contains duplicate file path {path}")
            }
            Self::TooManyDocuments { actual, maximum } => write!(
                formatter,
                "checkpoint has {actual} open documents; maximum is {maximum}"
            ),
            Self::DuplicateDocumentId { document_id } => {
                write!(
                    formatter,
                    "checkpoint contains duplicate document id {document_id}"
                )
            }
            Self::DuplicateDocumentPath { path } => {
                write!(formatter, "checkpoint opens path {path} more than once")
            }
            Self::ActiveDocumentNotOpen { document_id } => write!(
                formatter,
                "active document {document_id} is not an open checkpoint document"
            ),
            Self::DocumentFileMissing { document_id, path } => write!(
                formatter,
                "open document {document_id} references missing checkpoint file {path}"
            ),
            Self::DocumentNotUtf8 { document_id, path } => write!(
                formatter,
                "open document {document_id} at {path} is not UTF-8"
            ),
            Self::SelectionOutOfBounds {
                document_id,
                offset,
                length,
            } => write!(
                formatter,
                "document {document_id} selection offset {offset} exceeds byte length {length}"
            ),
            Self::SelectionNotCharBoundary {
                document_id,
                offset,
            } => write!(
                formatter,
                "document {document_id} selection offset {offset} is not a UTF-8 boundary"
            ),
            Self::DocumentHashMismatch {
                document_id,
                expected,
                actual,
            } => write!(
                formatter,
                "document {document_id} hash is {actual}; recomputed hash is {expected}"
            ),
            Self::WorkspaceHashMismatch { expected, actual } => write!(
                formatter,
                "workspace hash is {actual}; recomputed hash is {expected}"
            ),
            Self::WorkspaceHashing { detail } => {
                write!(formatter, "cannot hash checkpoint workspace: {detail}")
            }
            Self::EncodedTooLarge { actual, maximum } => write!(
                formatter,
                "encoded checkpoint is {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::ExpandedTooLarge { actual, maximum } => write!(
                formatter,
                "expanded checkpoint declares {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::CompressedTooLarge { actual, maximum } => write!(
                formatter,
                "compressed checkpoint declares {actual} bytes; maximum is {maximum} bytes"
            ),
            Self::Truncated => formatter.write_str("checkpoint payload is truncated"),
            Self::InvalidMagic { layer } => write!(formatter, "invalid {layer} checkpoint magic"),
            Self::UnsupportedCompression { found } => {
                write!(formatter, "unsupported checkpoint compression id {found}")
            }
            Self::LengthOverflow => formatter.write_str("checkpoint length arithmetic overflowed"),
            Self::TrailingData { actual, expected } => write!(
                formatter,
                "checkpoint has trailing data: {actual} bytes, expected {expected}"
            ),
            Self::IntegrityMismatch => {
                formatter.write_str("compressed checkpoint integrity digest does not match")
            }
            Self::Decompression { detail } => {
                write!(formatter, "checkpoint decompression failed: {detail}")
            }
            Self::CompressedTrailingData { actual, expected } => write!(
                formatter,
                "zlib stream consumed {actual} bytes; framed body has {expected} bytes"
            ),
            Self::ExpandedLengthMismatch { actual, expected } => write!(
                formatter,
                "checkpoint expanded to {actual} bytes; declared length is {expected}"
            ),
            Self::Malformed { field, detail } => {
                write!(formatter, "malformed checkpoint {field}: {detail}")
            }
            Self::NonCanonicalEncoding => {
                formatter.write_str("checkpoint payload is not canonical encoding")
            }
            Self::Compression { detail } => {
                write!(formatter, "checkpoint compression failed: {detail}")
            }
            Self::OwnerMismatch { field } => {
                write!(
                    formatter,
                    "checkpoint disagrees with owning event field {field}"
                )
            }
        }
    }
}

impl Error for CheckpointError {}

pub fn encode_checkpoint(snapshot: &CheckpointSnapshot) -> Result<Vec<u8>, CheckpointError> {
    snapshot.validate()?;
    let expanded = encode_snapshot(snapshot)?;
    frame_expanded(&expanded)
}

fn frame_expanded(expanded: &[u8]) -> Result<Vec<u8>, CheckpointError> {
    if expanded.len() > MAX_CHECKPOINT_EXPANDED_BYTES {
        return Err(CheckpointError::ExpandedTooLarge {
            actual: expanded.len() as u64,
            maximum: MAX_CHECKPOINT_EXPANDED_BYTES,
        });
    }

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(COMPRESSION_LEVEL));
    encoder
        .write_all(expanded)
        .map_err(|error| CheckpointError::Compression {
            detail: error.to_string(),
        })?;
    let compressed = encoder
        .finish()
        .map_err(|error| CheckpointError::Compression {
            detail: error.to_string(),
        })?;
    if compressed.len() > MAX_CHECKPOINT_COMPRESSED_BYTES {
        return Err(CheckpointError::CompressedTooLarge {
            actual: compressed.len() as u64,
            maximum: MAX_CHECKPOINT_COMPRESSED_BYTES,
        });
    }

    let capacity = OUTER_HEADER_BYTES
        .checked_add(compressed.len())
        .and_then(|length| length.checked_add(OUTER_TRAILER_BYTES))
        .ok_or(CheckpointError::LengthOverflow)?;
    if capacity > MAX_CHECKPOINT_ENCODED_BYTES {
        return Err(CheckpointError::EncodedTooLarge {
            actual: capacity,
            maximum: MAX_CHECKPOINT_ENCODED_BYTES,
        });
    }
    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(OUTER_MAGIC);
    put_u32(&mut encoded, CHECKPOINT_FORMAT_VERSION_V1);
    encoded.push(COMPRESSION_ZLIB);
    put_u64(
        &mut encoded,
        u64::try_from(expanded.len()).map_err(|_| CheckpointError::LengthOverflow)?,
    );
    put_u64(
        &mut encoded,
        u64::try_from(compressed.len()).map_err(|_| CheckpointError::LengthOverflow)?,
    );
    encoded.extend_from_slice(&compressed);
    let digest = integrity_digest(&encoded);
    encoded.extend_from_slice(digest.as_bytes());
    Ok(encoded)
}

pub fn decode_checkpoint(encoded: &[u8]) -> Result<CheckpointSnapshot, CheckpointError> {
    if encoded.len() > MAX_CHECKPOINT_ENCODED_BYTES {
        return Err(CheckpointError::EncodedTooLarge {
            actual: encoded.len(),
            maximum: MAX_CHECKPOINT_ENCODED_BYTES,
        });
    }
    if encoded.len() < OUTER_HEADER_BYTES + OUTER_TRAILER_BYTES {
        return Err(CheckpointError::Truncated);
    }
    if &encoded[..8] != OUTER_MAGIC {
        return Err(CheckpointError::InvalidMagic { layer: "outer" });
    }
    let version = u32::from_be_bytes(
        encoded[8..12]
            .try_into()
            .map_err(|_| CheckpointError::Truncated)?,
    );
    if version != CHECKPOINT_FORMAT_VERSION_V1 {
        return Err(CheckpointError::UnsupportedVersion {
            found: version,
            supported: CHECKPOINT_FORMAT_VERSION_V1,
        });
    }
    let compression = encoded[12];
    if compression != COMPRESSION_ZLIB {
        return Err(CheckpointError::UnsupportedCompression { found: compression });
    }
    let expanded_u64 = u64::from_be_bytes(
        encoded[13..21]
            .try_into()
            .map_err(|_| CheckpointError::Truncated)?,
    );
    if expanded_u64 > MAX_CHECKPOINT_EXPANDED_BYTES as u64 {
        return Err(CheckpointError::ExpandedTooLarge {
            actual: expanded_u64,
            maximum: MAX_CHECKPOINT_EXPANDED_BYTES,
        });
    }
    let expanded_len =
        usize::try_from(expanded_u64).map_err(|_| CheckpointError::ExpandedTooLarge {
            actual: expanded_u64,
            maximum: MAX_CHECKPOINT_EXPANDED_BYTES,
        })?;
    let compressed_u64 = u64::from_be_bytes(
        encoded[21..29]
            .try_into()
            .map_err(|_| CheckpointError::Truncated)?,
    );
    if compressed_u64 > MAX_CHECKPOINT_COMPRESSED_BYTES as u64 {
        return Err(CheckpointError::CompressedTooLarge {
            actual: compressed_u64,
            maximum: MAX_CHECKPOINT_COMPRESSED_BYTES,
        });
    }
    let compressed_len =
        usize::try_from(compressed_u64).map_err(|_| CheckpointError::CompressedTooLarge {
            actual: compressed_u64,
            maximum: MAX_CHECKPOINT_COMPRESSED_BYTES,
        })?;
    let expected_len = OUTER_HEADER_BYTES
        .checked_add(compressed_len)
        .and_then(|length| length.checked_add(OUTER_TRAILER_BYTES))
        .ok_or(CheckpointError::LengthOverflow)?;
    if encoded.len() < expected_len {
        return Err(CheckpointError::Truncated);
    }
    if encoded.len() > expected_len {
        return Err(CheckpointError::TrailingData {
            actual: encoded.len(),
            expected: expected_len,
        });
    }
    let digest_start = expected_len - OUTER_TRAILER_BYTES;
    let expected_digest = integrity_digest(&encoded[..digest_start]);
    if encoded[digest_start..] != expected_digest.as_bytes()[..] {
        return Err(CheckpointError::IntegrityMismatch);
    }

    let compressed = &encoded[OUTER_HEADER_BYTES..digest_start];
    let mut expanded = vec![0_u8; expanded_len];
    let mut decompressor = Decompress::new(true);
    let status = decompressor
        .decompress(compressed, &mut expanded, FlushDecompress::Finish)
        .map_err(|error| CheckpointError::Decompression {
            detail: error.to_string(),
        })?;
    if status != Status::StreamEnd {
        return Err(CheckpointError::Decompression {
            detail: "zlib stream did not reach its checked end within the declared output length"
                .to_owned(),
        });
    }
    if decompressor.total_in() != compressed_u64 {
        return Err(CheckpointError::CompressedTrailingData {
            actual: decompressor.total_in(),
            expected: compressed_len,
        });
    }
    if decompressor.total_out() != expanded_u64 {
        return Err(CheckpointError::ExpandedLengthMismatch {
            actual: decompressor.total_out(),
            expected: expanded_len,
        });
    }

    let snapshot = decode_snapshot(&expanded)?;
    let canonical = encode_checkpoint(&snapshot)?;
    if canonical != encoded {
        return Err(CheckpointError::NonCanonicalEncoding);
    }
    Ok(snapshot)
}

fn encode_snapshot(snapshot: &CheckpointSnapshot) -> Result<Vec<u8>, CheckpointError> {
    let mut encoded = Vec::with_capacity(snapshot.total_file_bytes().saturating_add(4096));
    encoded.extend_from_slice(SNAPSHOT_MAGIC);
    put_u32(&mut encoded, snapshot.format_version);
    put_bytes_u32(&mut encoded, snapshot.session_id.as_str().as_bytes())?;
    put_u64(&mut encoded, snapshot.event_sequence);
    encoded.extend_from_slice(snapshot.workspace_hash.as_bytes());
    put_len_u32(&mut encoded, snapshot.files.len())?;
    for file in &snapshot.files {
        put_bytes_u32(&mut encoded, file.path.as_str().as_bytes())?;
        put_len_u64(&mut encoded, file.contents.len())?;
        encoded.extend_from_slice(&file.contents);
    }
    match &snapshot.active_document {
        Some(document_id) => {
            encoded.push(1);
            put_bytes_u32(&mut encoded, document_id.as_str().as_bytes())?;
        }
        None => encoded.push(0),
    }
    put_len_u32(&mut encoded, snapshot.documents.len())?;
    for document in &snapshot.documents {
        put_bytes_u32(&mut encoded, document.document_id.as_str().as_bytes())?;
        put_bytes_u32(&mut encoded, document.path.as_str().as_bytes())?;
        put_u64(&mut encoded, document.version);
        put_u64(&mut encoded, document.selection.anchor_byte);
        put_u64(&mut encoded, document.selection.active_byte);
        encoded.extend_from_slice(document.content_hash.as_bytes());
    }
    Ok(encoded)
}

fn decode_snapshot(encoded: &[u8]) -> Result<CheckpointSnapshot, CheckpointError> {
    let mut reader = SliceReader::new(encoded);
    if reader.take(8)? != SNAPSHOT_MAGIC {
        return Err(CheckpointError::InvalidMagic { layer: "snapshot" });
    }
    let format_version = reader.u32()?;
    if format_version != CHECKPOINT_FORMAT_VERSION_V1 {
        return Err(CheckpointError::UnsupportedVersion {
            found: format_version,
            supported: CHECKPOINT_FORMAT_VERSION_V1,
        });
    }
    let session_id = reader.session_id()?;
    let event_sequence = reader.u64()?;
    let workspace_hash = Hash::from_bytes(reader.array()?);
    let file_count = reader.bounded_count("file_count", MAX_CHECKPOINT_FILES)?;
    let mut files = Vec::with_capacity(file_count);
    let mut total_file_bytes = 0usize;
    for _ in 0..file_count {
        let path = reader.workspace_path()?;
        let contents_len = reader.bounded_u64_len("file.contents", MAX_CHECKPOINT_FILE_BYTES)?;
        total_file_bytes = total_file_bytes
            .checked_add(contents_len)
            .ok_or(CheckpointError::LengthOverflow)?;
        if total_file_bytes > MAX_CHECKPOINT_TOTAL_FILE_BYTES {
            return Err(CheckpointError::WorkspaceTooLarge {
                actual: total_file_bytes,
                maximum: MAX_CHECKPOINT_TOTAL_FILE_BYTES,
            });
        }
        let contents = reader.take(contents_len)?.to_vec();
        files.push(CheckpointFile { path, contents });
    }
    let active_document = match reader.byte()? {
        0 => None,
        1 => Some(reader.document_id()?),
        value => {
            return Err(CheckpointError::Malformed {
                field: "active_document.tag",
                detail: format!("expected 0 or 1, found {value}"),
            });
        }
    };
    let document_count = reader.bounded_count("document_count", MAX_CHECKPOINT_DOCUMENTS)?;
    let mut documents = Vec::with_capacity(document_count);
    for _ in 0..document_count {
        documents.push(CheckpointDocument {
            document_id: reader.document_id()?,
            path: reader.workspace_path()?,
            version: reader.u64()?,
            selection: SelectionState::new(reader.u64()?, reader.u64()?),
            content_hash: Hash::from_bytes(reader.array()?),
        });
    }
    if reader.remaining() != 0 {
        return Err(CheckpointError::Malformed {
            field: "snapshot",
            detail: format!("{} trailing expanded bytes", reader.remaining()),
        });
    }
    let snapshot = CheckpointSnapshot {
        format_version,
        session_id,
        event_sequence,
        workspace_hash,
        files,
        active_document,
        documents,
    };
    snapshot.validate()?;
    if encode_snapshot(&snapshot)? != encoded {
        return Err(CheckpointError::NonCanonicalEncoding);
    }
    Ok(snapshot)
}

fn validate_input(
    event_sequence: u64,
    files: &[CheckpointFile],
    active_document: Option<&DocumentId>,
    documents: &[OpenDocument],
) -> Result<(), CheckpointError> {
    validate_files(event_sequence, files)?;
    if documents.len() > MAX_CHECKPOINT_DOCUMENTS {
        return Err(CheckpointError::TooManyDocuments {
            actual: documents.len(),
            maximum: MAX_CHECKPOINT_DOCUMENTS,
        });
    }
    validate_document_order(
        documents
            .iter()
            .map(|document| (&document.document_id, &document.path)),
    )?;
    validate_active(
        active_document,
        documents.iter().map(|document| &document.document_id),
    )?;
    for document in documents {
        validate_document_state(
            files,
            &document.document_id,
            &document.path,
            document.selection,
            None,
        )?;
    }
    Ok(())
}

fn validate_input_counts(
    event_sequence: u64,
    file_count: usize,
    document_count: usize,
) -> Result<(), CheckpointError> {
    if event_sequence == 0 || event_sequence > MAX_CHECKPOINT_SEQUENCE {
        return Err(CheckpointError::InvalidSequence {
            actual: event_sequence,
        });
    }
    if file_count > MAX_CHECKPOINT_FILES {
        return Err(CheckpointError::TooManyFiles {
            actual: file_count,
            maximum: MAX_CHECKPOINT_FILES,
        });
    }
    if document_count > MAX_CHECKPOINT_DOCUMENTS {
        return Err(CheckpointError::TooManyDocuments {
            actual: document_count,
            maximum: MAX_CHECKPOINT_DOCUMENTS,
        });
    }
    Ok(())
}

fn validate_persisted(
    event_sequence: u64,
    files: &[CheckpointFile],
    active_document: Option<&DocumentId>,
    documents: &[CheckpointDocument],
) -> Result<(), CheckpointError> {
    validate_files(event_sequence, files)?;
    if documents.len() > MAX_CHECKPOINT_DOCUMENTS {
        return Err(CheckpointError::TooManyDocuments {
            actual: documents.len(),
            maximum: MAX_CHECKPOINT_DOCUMENTS,
        });
    }
    validate_document_order(
        documents
            .iter()
            .map(|document| (&document.document_id, &document.path)),
    )?;
    validate_active(
        active_document,
        documents.iter().map(|document| &document.document_id),
    )?;
    for document in documents {
        validate_document_state(
            files,
            &document.document_id,
            &document.path,
            document.selection,
            Some(document.content_hash),
        )?;
    }
    Ok(())
}

fn validate_files(event_sequence: u64, files: &[CheckpointFile]) -> Result<(), CheckpointError> {
    if event_sequence == 0 || event_sequence > MAX_CHECKPOINT_SEQUENCE {
        return Err(CheckpointError::InvalidSequence {
            actual: event_sequence,
        });
    }
    if files.len() > MAX_CHECKPOINT_FILES {
        return Err(CheckpointError::TooManyFiles {
            actual: files.len(),
            maximum: MAX_CHECKPOINT_FILES,
        });
    }
    let mut total = 0usize;
    let mut previous: Option<&WorkspacePath> = None;
    for file in files {
        if let Some(previous) = previous {
            if previous == &file.path {
                return Err(CheckpointError::DuplicateFilePath {
                    path: file.path.clone(),
                });
            }
            if previous > &file.path {
                return Err(CheckpointError::Malformed {
                    field: "files",
                    detail: "file paths are not in canonical byte order".to_owned(),
                });
            }
        }
        if file.contents.len() > MAX_CHECKPOINT_FILE_BYTES {
            return Err(CheckpointError::FileTooLarge {
                path: file.path.clone(),
                actual: file.contents.len(),
                maximum: MAX_CHECKPOINT_FILE_BYTES,
            });
        }
        total = total
            .checked_add(file.contents.len())
            .ok_or(CheckpointError::LengthOverflow)?;
        if total > MAX_CHECKPOINT_TOTAL_FILE_BYTES {
            return Err(CheckpointError::WorkspaceTooLarge {
                actual: total,
                maximum: MAX_CHECKPOINT_TOTAL_FILE_BYTES,
            });
        }
        previous = Some(&file.path);
    }
    Ok(())
}

fn validate_document_order<'a>(
    documents: impl IntoIterator<Item = (&'a DocumentId, &'a WorkspacePath)>,
) -> Result<(), CheckpointError> {
    let mut previous_id: Option<&DocumentId> = None;
    let mut paths = std::collections::BTreeSet::new();
    for (document_id, path) in documents {
        if let Some(previous) = previous_id {
            if previous == document_id {
                return Err(CheckpointError::DuplicateDocumentId {
                    document_id: document_id.clone(),
                });
            }
            if previous > document_id {
                return Err(CheckpointError::Malformed {
                    field: "documents",
                    detail: "document ids are not in canonical byte order".to_owned(),
                });
            }
        }
        if !paths.insert(path) {
            return Err(CheckpointError::DuplicateDocumentPath { path: path.clone() });
        }
        previous_id = Some(document_id);
    }
    Ok(())
}

fn validate_active<'a>(
    active_document: Option<&DocumentId>,
    documents: impl IntoIterator<Item = &'a DocumentId>,
) -> Result<(), CheckpointError> {
    if let Some(active_document) = active_document
        && !documents.into_iter().any(|id| id == active_document)
    {
        return Err(CheckpointError::ActiveDocumentNotOpen {
            document_id: active_document.clone(),
        });
    }
    Ok(())
}

fn validate_document_state(
    files: &[CheckpointFile],
    document_id: &DocumentId,
    path: &WorkspacePath,
    selection: SelectionState,
    persisted_hash: Option<Hash>,
) -> Result<(), CheckpointError> {
    let contents = find_file(files, path).ok_or_else(|| CheckpointError::DocumentFileMissing {
        document_id: document_id.clone(),
        path: path.clone(),
    })?;
    let text = std::str::from_utf8(contents).map_err(|_| CheckpointError::DocumentNotUtf8 {
        document_id: document_id.clone(),
        path: path.clone(),
    })?;
    for offset in [selection.anchor_byte, selection.active_byte] {
        let Ok(offset_usize) = usize::try_from(offset) else {
            return Err(CheckpointError::SelectionOutOfBounds {
                document_id: document_id.clone(),
                offset,
                length: text.len(),
            });
        };
        if offset_usize > text.len() {
            return Err(CheckpointError::SelectionOutOfBounds {
                document_id: document_id.clone(),
                offset,
                length: text.len(),
            });
        }
        if !text.is_char_boundary(offset_usize) {
            return Err(CheckpointError::SelectionNotCharBoundary {
                document_id: document_id.clone(),
                offset,
            });
        }
    }
    if let Some(actual) = persisted_hash {
        let expected = document_hash(text);
        if actual != expected {
            return Err(CheckpointError::DocumentHashMismatch {
                document_id: document_id.clone(),
                expected,
                actual,
            });
        }
    }
    Ok(())
}

fn find_file<'a>(files: &'a [CheckpointFile], path: &WorkspacePath) -> Option<&'a [u8]> {
    files
        .binary_search_by(|file| file.path.cmp(path))
        .ok()
        .map(|index| files[index].contents.as_slice())
}

fn integrity_digest(bytes: &[u8]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(INTEGRITY_DOMAIN);
    hasher.update(bytes);
    Hash::from_bytes(*hasher.finalize().as_bytes())
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn put_len_u32(output: &mut Vec<u8>, value: usize) -> Result<(), CheckpointError> {
    put_u32(
        output,
        u32::try_from(value).map_err(|_| CheckpointError::LengthOverflow)?,
    );
    Ok(())
}

fn put_len_u64(output: &mut Vec<u8>, value: usize) -> Result<(), CheckpointError> {
    put_u64(
        output,
        u64::try_from(value).map_err(|_| CheckpointError::LengthOverflow)?,
    );
    Ok(())
}

fn put_bytes_u32(output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), CheckpointError> {
    put_len_u32(output, bytes.len())?;
    output.extend_from_slice(bytes);
    Ok(())
}

struct SliceReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> SliceReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.position)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], CheckpointError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(CheckpointError::LengthOverflow)?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or(CheckpointError::Truncated)?;
        self.position = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, CheckpointError> {
        Ok(self.take(1)?[0])
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], CheckpointError> {
        self.take(N)?
            .try_into()
            .map_err(|_| CheckpointError::Truncated)
    }

    fn u32(&mut self) -> Result<u32, CheckpointError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, CheckpointError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn bounded_count(
        &mut self,
        field: &'static str,
        maximum: usize,
    ) -> Result<usize, CheckpointError> {
        let count = usize::try_from(self.u32()?).map_err(|_| CheckpointError::Malformed {
            field,
            detail: "count does not fit in usize".to_owned(),
        })?;
        if count > maximum {
            return Err(CheckpointError::Malformed {
                field,
                detail: format!("count {count} exceeds maximum {maximum}"),
            });
        }
        Ok(count)
    }

    fn bounded_u64_len(
        &mut self,
        field: &'static str,
        maximum: usize,
    ) -> Result<usize, CheckpointError> {
        let raw = self.u64()?;
        let length = usize::try_from(raw).map_err(|_| CheckpointError::Malformed {
            field,
            detail: format!("length {raw} does not fit in usize"),
        })?;
        if length > maximum {
            return Err(CheckpointError::Malformed {
                field,
                detail: format!("length {length} exceeds maximum {maximum}"),
            });
        }
        Ok(length)
    }

    fn bounded_u32_bytes(
        &mut self,
        field: &'static str,
        maximum: usize,
    ) -> Result<&'a [u8], CheckpointError> {
        let raw = self.u32()?;
        let length = usize::try_from(raw).map_err(|_| CheckpointError::Malformed {
            field,
            detail: format!("length {raw} does not fit in usize"),
        })?;
        if length == 0 || length > maximum {
            return Err(CheckpointError::Malformed {
                field,
                detail: format!("length {length} is outside 1..={maximum}"),
            });
        }
        self.take(length)
    }

    fn workspace_path(&mut self) -> Result<WorkspacePath, CheckpointError> {
        let bytes = self.bounded_u32_bytes("path", MAX_WORKSPACE_PATH_BYTES)?;
        let text = std::str::from_utf8(bytes).map_err(|_| CheckpointError::Malformed {
            field: "path",
            detail: "path is not UTF-8".to_owned(),
        })?;
        WorkspacePath::new(text).map_err(|error| CheckpointError::Malformed {
            field: "path",
            detail: error.to_string(),
        })
    }

    fn document_id(&mut self) -> Result<DocumentId, CheckpointError> {
        let bytes = self.bounded_u32_bytes("document_id", MAX_IDENTIFIER_BYTES)?;
        let text = std::str::from_utf8(bytes).map_err(|_| CheckpointError::Malformed {
            field: "document_id",
            detail: "identifier is not UTF-8".to_owned(),
        })?;
        DocumentId::new(text).map_err(|error| CheckpointError::Malformed {
            field: "document_id",
            detail: error.to_string(),
        })
    }

    fn session_id(&mut self) -> Result<SessionId, CheckpointError> {
        let bytes = self.bounded_u32_bytes("session_id", MAX_IDENTIFIER_BYTES)?;
        let text = std::str::from_utf8(bytes).map_err(|_| CheckpointError::Malformed {
            field: "session_id",
            detail: "identifier is not UTF-8".to_owned(),
        })?;
        SessionId::new(text).map_err(|error| CheckpointError::Malformed {
            field: "session_id",
            detail: error.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> CheckpointSnapshot {
        let document_id = DocumentId::new("doc-lib").unwrap();
        CheckpointSnapshot::new(
            SessionId::new("codec-internals").unwrap(),
            1,
            vec![
                CheckpointFile {
                    path: WorkspacePath::new("src/lib.rs").unwrap(),
                    contents: b"pub fn value() {}\n".to_vec(),
                },
                CheckpointFile {
                    path: WorkspacePath::new("Cargo.toml").unwrap(),
                    contents: b"[package]\nname = \"demo\"\n[workspace]\n".to_vec(),
                },
            ],
            Some(document_id.clone()),
            vec![OpenDocument {
                document_id,
                path: WorkspacePath::new("src/lib.rs").unwrap(),
                selection: SelectionState::caret(4),
                version: 3,
            }],
        )
        .unwrap()
    }

    #[test]
    fn over_limit_counts_are_rejected_before_sorting() {
        let files = (0..=MAX_CHECKPOINT_FILES)
            .map(|index| CheckpointFile {
                path: WorkspacePath::new(format!("{index:03}.rs")).unwrap(),
                contents: vec![],
            })
            .collect();
        CHECKPOINT_PRE_SORT_COUNT.with(|count| count.set(0));
        assert!(matches!(
            CheckpointSnapshot::new(
                SessionId::new("too-many-files").unwrap(),
                1,
                files,
                None,
                vec![]
            ),
            Err(CheckpointError::TooManyFiles { .. })
        ));
        CHECKPOINT_PRE_SORT_COUNT.with(|count| assert_eq!(count.get(), 0));

        let path = WorkspacePath::new("only.rs").unwrap();
        let documents = (0..=MAX_CHECKPOINT_DOCUMENTS)
            .map(|index| OpenDocument {
                document_id: DocumentId::new(format!("doc-{index:03}")).unwrap(),
                path: path.clone(),
                selection: SelectionState::caret(0),
                version: 0,
            })
            .collect();
        CHECKPOINT_PRE_SORT_COUNT.with(|count| count.set(0));
        assert!(matches!(
            CheckpointSnapshot::new(
                SessionId::new("too-many-documents").unwrap(),
                1,
                vec![CheckpointFile {
                    path,
                    contents: vec![]
                }],
                None,
                documents
            ),
            Err(CheckpointError::TooManyDocuments { .. })
        ));
        CHECKPOINT_PRE_SORT_COUNT.with(|count| assert_eq!(count.get(), 0));
    }

    fn find(bytes: &[u8], needle: &[u8]) -> usize {
        bytes
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap()
    }

    #[test]
    fn decoded_file_content_path_and_document_metadata_are_revalidated() {
        let base = encode_snapshot(&snapshot()).unwrap();

        let mut content = base.clone();
        let content_index = find(&content, b"name = \"demo\"");
        content[content_index] ^= 1;
        assert!(matches!(
            decode_checkpoint(&frame_expanded(&content).unwrap()),
            Err(CheckpointError::WorkspaceHashMismatch { .. })
        ));

        let mut path = base.clone();
        let path_index = find(&path, b"src/lib.rs");
        path[path_index..path_index + 10].copy_from_slice(b"/rc/lib.rs");
        assert!(matches!(
            decode_checkpoint(&frame_expanded(&path).unwrap()),
            Err(CheckpointError::Malformed { field: "path", .. })
        ));

        let mut selection = base.clone();
        let document_index = selection
            .windows(b"doc-lib".len())
            .rposition(|window| window == b"doc-lib")
            .unwrap();
        let anchor_index = document_index + b"doc-lib".len() + 4 + b"src/lib.rs".len() + 8;
        selection[anchor_index..anchor_index + 8].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(matches!(
            decode_checkpoint(&frame_expanded(&selection).unwrap()),
            Err(CheckpointError::SelectionOutOfBounds { .. })
        ));

        let mut document_hash = base;
        let last = document_hash.len() - 1;
        document_hash[last] ^= 1;
        assert!(matches!(
            decode_checkpoint(&frame_expanded(&document_hash).unwrap()),
            Err(CheckpointError::DocumentHashMismatch { .. })
        ));
    }

    #[test]
    fn malformed_and_noncanonical_zlib_streams_are_rejected() {
        let expanded = encode_snapshot(&snapshot()).unwrap();
        let malformed = frame_compressed(expanded.len(), vec![0; 32]);
        assert!(matches!(
            decode_checkpoint(&malformed),
            Err(CheckpointError::Decompression { .. })
        ));

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(1));
        encoder.write_all(&expanded).unwrap();
        let noncanonical = frame_compressed(expanded.len(), encoder.finish().unwrap());
        assert!(matches!(
            decode_checkpoint(&noncanonical),
            Err(CheckpointError::NonCanonicalEncoding)
        ));

        let mut first = ZlibEncoder::new(Vec::new(), Compression::new(COMPRESSION_LEVEL));
        first.write_all(&expanded).unwrap();
        let mut concatenated = first.finish().unwrap();
        let mut second = ZlibEncoder::new(Vec::new(), Compression::new(COMPRESSION_LEVEL));
        second.write_all(b"extra frame").unwrap();
        concatenated.extend_from_slice(&second.finish().unwrap());
        let trailing_frame = frame_compressed(expanded.len(), concatenated);
        assert!(matches!(
            decode_checkpoint(&trailing_frame),
            Err(CheckpointError::CompressedTrailingData { .. })
        ));
    }

    #[test]
    fn open_document_count_accepts_the_exact_limit_and_rejects_one_over() {
        let files: Vec<_> = (0..MAX_CHECKPOINT_DOCUMENTS)
            .map(|index| CheckpointFile {
                path: WorkspacePath::new(format!("{index:03}.rs")).unwrap(),
                contents: Vec::new(),
            })
            .collect();
        let documents: Vec<_> = (0..MAX_CHECKPOINT_DOCUMENTS)
            .map(|index| OpenDocument {
                document_id: DocumentId::new(format!("doc-{index:03}")).unwrap(),
                path: WorkspacePath::new(format!("{index:03}.rs")).unwrap(),
                selection: SelectionState::caret(0),
                version: 0,
            })
            .collect();
        let mut exact = CheckpointSnapshot::new(
            SessionId::new("document-limit").unwrap(),
            1,
            files,
            None,
            documents,
        )
        .unwrap();
        assert_eq!(exact.documents().len(), MAX_CHECKPOINT_DOCUMENTS);
        exact.documents.push(exact.documents[0].clone());
        assert!(matches!(
            exact.validate(),
            Err(CheckpointError::TooManyDocuments { actual, maximum })
                if actual == MAX_CHECKPOINT_DOCUMENTS + 1 && maximum == MAX_CHECKPOINT_DOCUMENTS
        ));
    }

    #[test]
    fn checkpoint_sequence_accepts_exact_domain_boundaries() {
        let id = SessionId::new("sequence-limits").unwrap();
        assert!(matches!(
            CheckpointSnapshot::new(id.clone(), 0, vec![], None, vec![]),
            Err(CheckpointError::InvalidSequence { actual: 0 })
        ));
        assert!(
            CheckpointSnapshot::new(id.clone(), MAX_CHECKPOINT_SEQUENCE, vec![], None, vec![])
                .is_ok()
        );
        assert!(matches!(
            CheckpointSnapshot::new(
                id,
                MAX_CHECKPOINT_SEQUENCE + 1,
                vec![],
                None,
                vec![]
            ),
            Err(CheckpointError::InvalidSequence { actual })
                if actual == MAX_CHECKPOINT_SEQUENCE + 1
        ));
    }

    #[test]
    fn outer_declared_and_complete_size_boundaries_are_checked_before_allocation() {
        let base = encode_checkpoint(&snapshot()).unwrap();

        let mut expanded_exact = base.clone();
        expanded_exact[13..21]
            .copy_from_slice(&(MAX_CHECKPOINT_EXPANDED_BYTES as u64).to_be_bytes());
        rewrite_integrity_digest(&mut expanded_exact);
        assert!(matches!(
            decode_checkpoint(&expanded_exact),
            Err(CheckpointError::ExpandedLengthMismatch { .. })
        ));
        let mut expanded_over = base.clone();
        expanded_over[13..21]
            .copy_from_slice(&((MAX_CHECKPOINT_EXPANDED_BYTES as u64) + 1).to_be_bytes());
        assert!(matches!(
            decode_checkpoint(&expanded_over),
            Err(CheckpointError::ExpandedTooLarge { .. })
        ));

        let mut compressed_exact = base.clone();
        compressed_exact[21..29]
            .copy_from_slice(&(MAX_CHECKPOINT_COMPRESSED_BYTES as u64).to_be_bytes());
        assert!(matches!(
            decode_checkpoint(&compressed_exact),
            Err(CheckpointError::Truncated)
        ));
        let mut compressed_over = base;
        compressed_over[21..29]
            .copy_from_slice(&((MAX_CHECKPOINT_COMPRESSED_BYTES as u64) + 1).to_be_bytes());
        assert!(matches!(
            decode_checkpoint(&compressed_over),
            Err(CheckpointError::CompressedTooLarge { .. })
        ));

        let mut encoded_exact = vec![0_u8; MAX_CHECKPOINT_ENCODED_BYTES];
        encoded_exact[..8].copy_from_slice(OUTER_MAGIC);
        encoded_exact[8..12].copy_from_slice(&CHECKPOINT_FORMAT_VERSION_V1.to_be_bytes());
        encoded_exact[12] = COMPRESSION_ZLIB;
        encoded_exact[21..29]
            .copy_from_slice(&(MAX_CHECKPOINT_COMPRESSED_BYTES as u64).to_be_bytes());
        assert!(matches!(
            decode_checkpoint(&encoded_exact),
            Err(CheckpointError::IntegrityMismatch)
        ));
        encoded_exact.push(0);
        assert!(matches!(
            decode_checkpoint(&encoded_exact),
            Err(CheckpointError::EncodedTooLarge { .. })
        ));
    }

    #[test]
    fn path_length_accepts_1024_bytes_and_rejects_one_over_when_decoded() {
        let exact_path = [
            "a".repeat(255),
            "b".repeat(255),
            "c".repeat(255),
            "d".repeat(254),
            "e".to_owned(),
        ]
        .join("/");
        assert_eq!(exact_path.len(), MAX_WORKSPACE_PATH_BYTES);
        let exact = CheckpointSnapshot::new(
            SessionId::new("path-limit").unwrap(),
            1,
            vec![CheckpointFile {
                path: WorkspacePath::new(&exact_path).unwrap(),
                contents: vec![],
            }],
            None,
            vec![],
        )
        .unwrap();
        assert_eq!(
            decode_checkpoint(&encode_checkpoint(&exact).unwrap()).unwrap(),
            exact
        );

        let mut expanded = encode_snapshot(&exact).unwrap();
        let path_start = find(&expanded, exact_path.as_bytes());
        expanded[path_start - 4..path_start]
            .copy_from_slice(&((MAX_WORKSPACE_PATH_BYTES as u32) + 1).to_be_bytes());
        expanded.insert(path_start + exact_path.len(), b'f');
        assert!(matches!(
            decode_checkpoint(&frame_expanded(&expanded).unwrap()),
            Err(CheckpointError::Malformed { field: "path", .. })
        ));
    }

    #[test]
    fn versions_codec_and_representative_truncations_are_rejected() {
        let encoded = encode_checkpoint(&snapshot()).unwrap();
        let mut outer_version = encoded.clone();
        outer_version[8..12].copy_from_slice(&2_u32.to_be_bytes());
        assert!(matches!(
            decode_checkpoint(&outer_version),
            Err(CheckpointError::UnsupportedVersion { found: 2, .. })
        ));
        let mut codec = encoded.clone();
        codec[12] = 2;
        assert!(matches!(
            decode_checkpoint(&codec),
            Err(CheckpointError::UnsupportedCompression { found: 2 })
        ));
        let mut expanded = encode_snapshot(&snapshot()).unwrap();
        expanded[8..12].copy_from_slice(&2_u32.to_be_bytes());
        assert!(matches!(
            decode_checkpoint(&frame_expanded(&expanded).unwrap()),
            Err(CheckpointError::UnsupportedVersion { found: 2, .. })
        ));

        for length in [
            0,
            1,
            7,
            8,
            12,
            28,
            encoded.len() - Hash::LENGTH,
            encoded.len() - 1,
        ] {
            assert!(
                decode_checkpoint(&encoded[..length]).is_err(),
                "length {length}"
            );
        }
    }

    #[test]
    fn decoded_duplicate_and_out_of_order_records_are_rejected() {
        let mut duplicate_files = snapshot();
        duplicate_files.files[1].path = duplicate_files.files[0].path.clone();
        assert!(matches!(
            decode_checkpoint(
                &frame_expanded(&encode_snapshot(&duplicate_files).unwrap()).unwrap()
            ),
            Err(CheckpointError::DuplicateFilePath { .. })
        ));
        let mut out_of_order_files = snapshot();
        out_of_order_files.files.swap(0, 1);
        assert!(matches!(
            decode_checkpoint(
                &frame_expanded(&encode_snapshot(&out_of_order_files).unwrap()).unwrap()
            ),
            Err(CheckpointError::Malformed { field: "files", .. })
        ));

        let mut documents = CheckpointSnapshot::new(
            SessionId::new("document-order").unwrap(),
            1,
            vec![
                CheckpointFile {
                    path: WorkspacePath::new("a.rs").unwrap(),
                    contents: vec![],
                },
                CheckpointFile {
                    path: WorkspacePath::new("b.rs").unwrap(),
                    contents: vec![],
                },
            ],
            None,
            vec![
                OpenDocument {
                    document_id: DocumentId::new("a").unwrap(),
                    path: WorkspacePath::new("a.rs").unwrap(),
                    selection: SelectionState::caret(0),
                    version: 0,
                },
                OpenDocument {
                    document_id: DocumentId::new("b").unwrap(),
                    path: WorkspacePath::new("b.rs").unwrap(),
                    selection: SelectionState::caret(0),
                    version: 0,
                },
            ],
        )
        .unwrap();
        documents.documents[1].document_id = documents.documents[0].document_id.clone();
        assert!(matches!(
            decode_checkpoint(&frame_expanded(&encode_snapshot(&documents).unwrap()).unwrap()),
            Err(CheckpointError::DuplicateDocumentId { .. })
        ));
        let mut out_of_order_documents = CheckpointSnapshot::new(
            SessionId::new("document-order-2").unwrap(),
            1,
            vec![
                CheckpointFile {
                    path: WorkspacePath::new("a.rs").unwrap(),
                    contents: vec![],
                },
                CheckpointFile {
                    path: WorkspacePath::new("b.rs").unwrap(),
                    contents: vec![],
                },
            ],
            None,
            vec![
                OpenDocument {
                    document_id: DocumentId::new("a").unwrap(),
                    path: WorkspacePath::new("a.rs").unwrap(),
                    selection: SelectionState::caret(0),
                    version: 0,
                },
                OpenDocument {
                    document_id: DocumentId::new("b").unwrap(),
                    path: WorkspacePath::new("b.rs").unwrap(),
                    selection: SelectionState::caret(0),
                    version: 0,
                },
            ],
        )
        .unwrap();
        out_of_order_documents.documents.swap(0, 1);
        assert!(matches!(
            decode_checkpoint(
                &frame_expanded(&encode_snapshot(&out_of_order_documents).unwrap()).unwrap()
            ),
            Err(CheckpointError::Malformed {
                field: "documents",
                ..
            })
        ));
    }

    #[test]
    fn documented_checkpoint_limits_match_the_format_constants() {
        assert_eq!(MAX_CHECKPOINT_EXPANDED_BYTES, 11_062_597);
        assert_eq!(MAX_CHECKPOINT_COMPRESSED_BYTES, 11_128_133);
        assert_eq!(MAX_CHECKPOINT_ENCODED_BYTES, 11_128_194);
    }

    fn frame_compressed(expanded_len: usize, compressed: Vec<u8>) -> Vec<u8> {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(OUTER_MAGIC);
        put_u32(&mut encoded, CHECKPOINT_FORMAT_VERSION_V1);
        encoded.push(COMPRESSION_ZLIB);
        put_u64(&mut encoded, u64::try_from(expanded_len).unwrap());
        put_u64(&mut encoded, u64::try_from(compressed.len()).unwrap());
        encoded.extend_from_slice(&compressed);
        let digest = integrity_digest(&encoded);
        encoded.extend_from_slice(digest.as_bytes());
        encoded
    }

    fn rewrite_integrity_digest(encoded: &mut [u8]) {
        let digest_start = encoded.len() - OUTER_TRAILER_BYTES;
        let digest = integrity_digest(&encoded[..digest_start]);
        encoded[digest_start..].copy_from_slice(digest.as_bytes());
    }
}
