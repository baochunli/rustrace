use crate::Hash;

/// Domain prefix for the pinned BLAKE3 document hash.
///
/// The digest input is this byte string followed immediately by the exact
/// document UTF-8 bytes. No path, timestamp, platform metadata, or Unicode
/// normalization participates in the digest.
pub const DOCUMENT_HASH_DOMAIN: &[u8] = b"rustrace.document.utf8.v1\0";

/// Returns the deterministic digest of the exact UTF-8 document bytes.
pub fn document_hash(text: &str) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOCUMENT_HASH_DOMAIN);
    hasher.update(text.as_bytes());
    Hash::from_bytes(*hasher.finalize().as_bytes())
}

/// Digest of an exact observed file view, including non-UTF-8 managed files.
/// This is a separate domain from editor document hashes and workspace trees.
pub fn observation_hash(bytes: &[u8]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rustrace.observation.bytes.v1\0");
    hasher.update(bytes);
    Hash::from_bytes(*hasher.finalize().as_bytes())
}
