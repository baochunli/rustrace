//! The `UNOFFICIAL_CLIENT` advisory that packages recorded by this test
//! binary carry.
//!
//! Test sessions record this build's identity. `build.rs` marks it `-dirty`
//! when the source tree has uncommitted changes, and a clean checkout records
//! a released-build shape, so whether such a package carries the advisory
//! depends on how the test binary was built. Tests add exactly this to the
//! advisories they expect rather than filtering the kind out, so any other
//! advisory still fails them.
#![allow(dead_code)]

use rustrace::review_flags::{AdvisoryFlagKind, is_clean_released_build};

/// `[UNOFFICIAL_CLIENT]` for a package recorded by this build, or nothing
/// when this build has a clean released-build identity.
pub fn client_advisories() -> Vec<AdvisoryFlagKind> {
    let version = rustrace::version::version_metadata();
    if is_clean_released_build(version.client_version, version.build_identity) {
        Vec::new()
    } else {
        vec![AdvisoryFlagKind::UnofficialClient]
    }
}

/// The advisories a package recorded by this build carries in one attempt
/// whose other advisories are `others`: `UNOFFICIAL_CLIENT` first when this
/// build is not a clean release, then `others` in order.
pub fn with_client_advisory(others: &[AdvisoryFlagKind]) -> Vec<AdvisoryFlagKind> {
    let mut expected = client_advisories();
    expected.extend_from_slice(others);
    expected
}

/// Scan's `advisories` value for one package whose advisory kinds are
/// `kinds`: `NAME: COUNT` per kind in vocabulary order, joined by `; `.
pub fn scan_advisory_column(kinds: &[AdvisoryFlagKind]) -> String {
    AdvisoryFlagKind::ALL
        .into_iter()
        .filter_map(|kind| {
            let count = kinds.iter().filter(|candidate| **candidate == kind).count();
            (count > 0).then(|| format!("{}: {count}", kind.name()))
        })
        .collect::<Vec<_>>()
        .join("; ")
}
