use super::*;
use crate::session::finalization::{FinalizationInterruption, checked_aggregate_event_count};
use rustrace_journal::Journal;
use rustrace_model::{
    Event, MAX_RPROV_EVENTS, RprovEventStreamCompleteness, RprovPackageState, RprovRecoveryGap,
    RprovUnavailableAssurance, validate_rprov_event_stream,
};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "finalization"
assignment_version = "v1"
title = "Finalization"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

const MANIFEST_V2: &[u8] = br#"format_version = 2
course_id = "course"
assignment_id = "finalization"
assignment_version = "v1"
title = "Finalization"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

struct Fixture(PathBuf);

impl Fixture {
    fn new(prefix: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rustrace-{prefix}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(fs::canonicalize(root).unwrap())
    }

    fn started(prefix: &str) -> (Self, ProductionSession) {
        let fixture = Self::new(prefix);
        fs::write(fixture.0.join("main.rs"), "A").unwrap();
        let session = ProductionSession::start(&fixture.0, MANIFEST).unwrap();
        (fixture, session)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn finalized(status: FinalizationStatus) -> FinalizationReceipt {
    match status {
        FinalizationStatus::Finalized(receipt) => *receipt,
        FinalizationStatus::Incomplete(incomplete) => {
            panic!("expected finalized receipt, got {incomplete:?}")
        }
    }
}

#[test]
fn packaged_suite_identity_survives_revision_and_interrupted_recovery() {
    let suite_hash = Hash::from_bytes([42; Hash::LENGTH]);
    let parent = Fixture::new("suite-identity-parent");
    fs::write(parent.0.join("main.rs"), "A").unwrap();
    let parent_session =
        ProductionSession::start_linked(&parent.0, MANIFEST_V2, None, Some(suite_hash)).unwrap();
    let parent_receipt = parent_session.finalize("student-1").unwrap();
    assert_eq!(
        parent_receipt.manifest().test_case_suite_hash,
        Some(suite_hash)
    );

    let child = Fixture::new("suite-identity-child");
    for (path, bytes) in parent_receipt.final_workspace() {
        fs::write(child.0.join(path.as_str()), bytes).unwrap();
    }
    let child_session = ProductionSession::start_revision(&parent.0, &child.0, MANIFEST_V2)
        .expect("revision inherits the validated suite identity");
    assert_eq!(
        child_session.metadata().test_case_suite_hash,
        Some(suite_hash)
    );
    let child_receipt = child_session.finalize("student-1").unwrap();
    assert_eq!(
        child_receipt.manifest().test_case_suite_hash,
        Some(suite_hash)
    );

    let interrupted = Fixture::new("suite-identity-recovery");
    fs::write(interrupted.0.join("main.rs"), "A").unwrap();
    let session =
        ProductionSession::start_linked(&interrupted.0, MANIFEST_V2, None, Some(suite_hash))
            .unwrap();
    assert!(
        session
            .finalize_interrupted("student-1", FinalizationInterruption::AfterCapture)
            .is_err()
    );
    let recovered = finalized(ProductionSession::recover_finalization(&interrupted.0).unwrap());
    assert_eq!(recovered.manifest().test_case_suite_hash, Some(suite_hash));
}

fn terminal_count(root: &Path, id: &SessionId) -> (u64, usize, bool) {
    let path = root.join(".rustrace").join(format!("{id}.sqlite"));
    let mut journal = Journal::open_read_only_no_follow(&path).unwrap();
    let chain = journal.verify_session_chain(id).unwrap();
    let mut terminal = 0;
    for first in (1..=chain.event_count).step_by(MAX_EVENTS_PER_READ) {
        terminal += journal
            .read_events(id, first, MAX_EVENTS_PER_READ)
            .unwrap()
            .iter()
            .filter(|event| matches!(event.event, Event::SubmissionFinalized(_)))
            .count();
    }
    let ended = journal.inspect_session(id).unwrap().ended;
    (chain.event_count, terminal, ended)
}

fn state_artifact(root: &Path, name: &str) -> PathBuf {
    root.join(".rustrace").join(name)
}

fn state_bytes(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(root.join(".rustrace"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let metadata = entry.metadata().unwrap();
            assert!(metadata.is_file(), "unexpected state entry {name}");
            (name, fs::read(entry.path()).unwrap())
        })
        .collect()
}

fn assert_state_bytes_unchanged(root: &Path, before: &BTreeMap<String, Vec<u8>>) {
    let after = state_bytes(root);
    let changed = before
        .keys()
        .chain(after.keys())
        .filter(|name| before.get(*name) != after.get(*name))
        .collect::<BTreeSet<_>>();
    assert!(changed.is_empty(), "changed state entries: {changed:?}");
}

fn tamper_binding(root: &Path, names: &[&str], field: &str) {
    for name in names {
        let path = state_artifact(root, name);
        let mut marker: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let replacement = marker["binding"]["terminal_event_hash"].clone();
        marker["binding"][field] = replacement;
        fs::write(path, serde_json::to_vec(&marker).unwrap()).unwrap();
    }
}

fn assert_exportable_missing_ancestry(
    status: FinalizationStatus,
    expected: &[u8],
) -> IncompleteFinalization {
    let FinalizationStatus::Incomplete(incomplete) = status else {
        panic!("invalid ancestry must not recover as finalized");
    };
    assert!(incomplete.capture_available);
    let manifest = incomplete
        .manifest()
        .expect("the required marked recovery export has no durable capture");
    let RprovPackageState::RecoveryIncomplete {
        unavailable_assurances,
        gaps,
    } = &manifest.package_state
    else {
        panic!("invalid ancestry must use the accepted recovery state");
    };
    assert!(unavailable_assurances.contains(&RprovUnavailableAssurance::CompleteAncestry));
    assert!(unavailable_assurances.contains(&RprovUnavailableAssurance::CompleteEventStream));
    assert!(unavailable_assurances.contains(&RprovUnavailableAssurance::CleanFinalization));
    assert!(matches!(
        gaps.as_slice(),
        [RprovRecoveryGap::MissingAncestry { before_session_id }]
            if before_session_id == &manifest.latest_session_id
    ));
    let segment = manifest.segments.last().unwrap();
    assert_eq!(
        segment.events.completeness,
        RprovEventStreamCompleteness::PrefixOnly
    );
    let event_bytes = incomplete.read_payload(&segment.events.entry).unwrap();
    validate_rprov_event_stream(manifest, segment, &event_bytes).unwrap();
    assert!(!incomplete.payloads().is_empty());
    assert_eq!(
        incomplete.final_workspace().unwrap()[&WorkspacePath::new("main.rs").unwrap()],
        expected
    );
    incomplete
}

#[test]
fn ordinary_finalization_is_exact_replayable_and_immutable_after_live_changes() {
    let (fixture, mut session) = Fixture::started("finalization-ordinary");
    let id = session.session_id().clone();
    session.execute(EditorCommand::Insert('B')).unwrap();
    let before = session.health().unwrap().events;

    let receipt = session.finalize("student-1").unwrap();
    assert!(matches!(
        receipt.manifest().package_state,
        RprovPackageState::CleanFinalized
    ));
    assert_eq!(receipt.manifest().aggregate_event_count, before + 2);
    assert_eq!(
        receipt.manifest().segments[0].inclusive_event_count,
        before + 2
    );
    assert_eq!(
        receipt.final_workspace()[&WorkspacePath::new("main.rs").unwrap()],
        b"BA"
    );
    let segment = &receipt.manifest().segments[0];
    assert_eq!(
        segment.events.completeness,
        RprovEventStreamCompleteness::Complete
    );
    let events = receipt.read_payload(&segment.events.entry).unwrap();
    validate_rprov_event_stream(receipt.manifest(), segment, &events).unwrap();
    assert_eq!(terminal_count(&fixture.0, &id), (before + 2, 1, true));

    fs::write(fixture.0.join("main.rs"), "mutable-after-finalize").unwrap();
    let recovered = finalized(ProductionSession::recover_finalization(&fixture.0).unwrap());
    assert_eq!(recovered.final_workspace(), receipt.final_workspace());
    assert_eq!(terminal_count(&fixture.0, &id), (before + 2, 1, true));
    assert!(ProductionSession::resume(&fixture.0, MANIFEST, ResumeChoice::Resume).is_err());
}

#[test]
fn clean_receipt_retains_required_external_evidence_and_internal_source_links() {
    let (fixture, mut session) = Fixture::started("finalization-evidence");
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Copy).unwrap();
    session
        .execute(EditorCommand::Move {
            movement: rustrace_editor::Movement::DocumentEnd,
            selecting: false,
        })
        .unwrap();
    session.execute(EditorCommand::Paste).unwrap();
    fs::write(fixture.0.join("main.rs"), "unmanaged-change").unwrap();
    session.save_all().unwrap();

    let receipt = session.finalize("student-1").unwrap();
    let segment = &receipt.manifest().segments[0];
    assert_eq!(segment.source_links.len(), 1);
    assert!(!segment.evidence.is_empty());
    for evidence in &segment.evidence {
        assert_eq!(
            receipt.read_payload(&evidence.entry).unwrap().len() as u64,
            evidence.byte_length
        );
    }
}

#[test]
fn revision_is_a_separate_session_bound_to_the_exact_parent_snapshot() {
    let (parent, mut first) = Fixture::started("finalization-parent");
    first.execute(EditorCommand::Insert('B')).unwrap();
    let first_receipt = first.finalize("student-1").unwrap();
    let parent_id = first_receipt.manifest().latest_session_id.clone();
    let parent_count = first_receipt.manifest().aggregate_event_count;

    let child = Fixture::new("finalization-child");
    for (path, bytes) in first_receipt.final_workspace() {
        fs::write(child.0.join(path.as_str()), bytes).unwrap();
    }
    let mut second = ProductionSession::start_revision(&parent.0, &child.0, MANIFEST).unwrap();
    assert_ne!(second.session_id(), &parent_id);
    second.execute(EditorCommand::Insert('C')).unwrap();
    let second_receipt = second.finalize("student-1").unwrap();

    assert_eq!(second_receipt.manifest().segments.len(), 2);
    let first_segment = &second_receipt.manifest().segments[0];
    let second_segment = &second_receipt.manifest().segments[1];
    let link = second_segment.parent.as_ref().unwrap();
    assert_eq!(link.session_id, first_segment.session_id);
    assert_eq!(
        Some(&link.terminal_event_hash),
        first_segment.terminal_event_hash.known()
    );
    assert_eq!(
        Some(&link.final_tree_hash),
        first_segment.final_tree_hash.known()
    );
    assert_eq!(second_segment.initial_tree_hash, link.final_tree_hash);
    assert_eq!(
        second_receipt.manifest().aggregate_event_count,
        parent_count + second_segment.inclusive_event_count
    );
    assert_eq!(terminal_count(&parent.0, &parent_id).1, 1);
}

#[test]
fn prepared_capture_recovers_the_same_snapshot_without_rereading_live_source() {
    let (fixture, mut session) = Fixture::started("finalization-before-terminal");
    session.execute(EditorCommand::Insert('B')).unwrap();
    assert!(
        session
            .finalize_interrupted("student-1", FinalizationInterruption::AfterCapture)
            .is_err()
    );
    assert!(ProductionSession::resume(&fixture.0, MANIFEST, ResumeChoice::Resume).is_err());
    fs::write(fixture.0.join("main.rs"), "changed-after-capture").unwrap();

    let receipt = finalized(ProductionSession::recover_finalization(&fixture.0).unwrap());
    assert_eq!(
        receipt.final_workspace()[&WorkspacePath::new("main.rs").unwrap()],
        b"BA"
    );
    let id = receipt.manifest().latest_session_id.clone();
    assert_eq!(terminal_count(&fixture.0, &id).1, 1);
}

#[test]
fn status_reports_prepared_finalization_without_changing_any_state_bytes() {
    let (fixture, session) = Fixture::started("status-prepared-read-only");
    assert!(
        session
            .finalize_interrupted("student-1", FinalizationInterruption::AfterCapture)
            .is_err()
    );
    let before = state_bytes(&fixture.0);
    let mut output = Vec::new();

    super::retention::run_status(&[fixture.0.to_string_lossy().into_owned()], &mut output).unwrap();

    assert_state_bytes_unchanged(&fixture.0, &before);
    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains(
            "finalization prepared, not completed; run `rustrace submit` to complete or inspect"
        ),
        "{output}"
    );
}

#[test]
fn status_reports_a_finalized_revision_without_reconstructing_renamed_parent() {
    let (parent, first) = Fixture::started("status-renamed-parent");
    let parent_receipt = first.finalize("student-1").unwrap();
    let child = Fixture::new("status-renamed-parent-child");
    for (path, bytes) in parent_receipt.final_workspace() {
        fs::write(child.0.join(path.as_str()), bytes).unwrap();
    }
    let second = ProductionSession::start_revision(&parent.0, &child.0, MANIFEST).unwrap();
    let child_id = second.session_id().clone();
    second.finalize("student-1").unwrap();
    let renamed_parent = parent.0.with_extension("temporarily-renamed");
    fs::rename(&parent.0, &renamed_parent).unwrap();
    let before = state_bytes(&child.0);
    let mut output = Vec::new();

    let status =
        super::retention::run_status(&[child.0.to_string_lossy().into_owned()], &mut output);
    let after = state_bytes(&child.0);
    fs::rename(&renamed_parent, &parent.0).unwrap();

    status.unwrap();
    let changed = before
        .keys()
        .chain(after.keys())
        .filter(|name| before.get(*name) != after.get(*name))
        .collect::<BTreeSet<_>>();
    assert!(changed.is_empty(), "changed state entries: {changed:?}");
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("FINALIZED IMMUTABLE SNAPSHOT"), "{output}");
    assert!(output.contains(child_id.as_str()), "{output}");
}

#[test]
fn post_terminal_interruptions_reconstruct_one_receipt_without_a_second_append() {
    for stage in [
        FinalizationInterruption::AfterTerminal,
        FinalizationInterruption::BeforeReceipt,
    ] {
        let (fixture, session) = Fixture::started("finalization-post-terminal");
        let id = session.session_id().clone();
        assert!(session.finalize_interrupted("student-1", stage).is_err());
        assert_eq!(terminal_count(&fixture.0, &id).1, 1);
        let first = finalized(ProductionSession::recover_finalization(&fixture.0).unwrap());
        let second = finalized(ProductionSession::recover_finalization(&fixture.0).unwrap());
        assert_eq!(
            first.manifest().final_tree_hash,
            second.manifest().final_tree_hash
        );
        assert_eq!(terminal_count(&fixture.0, &id).1, 1);
    }
}

#[test]
fn missing_or_tampered_ancestry_blocks_clean_child_finalization_and_marks_recovery() {
    for remove in [false, true] {
        let (parent, first) = Fixture::started("finalization-tamper-parent");
        let first_receipt = first.finalize("student-1").unwrap();
        let child = Fixture::new("finalization-tamper-child");
        for (path, bytes) in first_receipt.final_workspace() {
            fs::write(child.0.join(path.as_str()), bytes).unwrap();
        }
        let second = ProductionSession::start_revision(&parent.0, &child.0, MANIFEST).unwrap();
        let events_entry = &first_receipt.manifest().segments[0].events.entry;
        let source = first_receipt.payload_path(events_entry).unwrap();
        if remove {
            fs::remove_file(source).unwrap();
        } else {
            fs::write(source, b"tampered").unwrap();
        }

        let error = second.finalize("student-1").unwrap_err().to_string();
        assert!(
            error.contains("ancestry") || error.contains("payload"),
            "{error}"
        );
        let FinalizationStatus::Incomplete(incomplete) =
            ProductionSession::recover_finalization(&child.0).unwrap()
        else {
            panic!("invalid ancestry must not recover as clean");
        };
        assert_eq!(incomplete.label, "INCOMPLETE RECOVERY");
    }
}

#[test]
fn aggregate_count_uses_checked_package_wide_limit() {
    assert_eq!(checked_aggregate_event_count([1, 2, 3]).unwrap(), 6);
    assert!(checked_aggregate_event_count([MAX_RPROV_EVENTS, 1]).is_err());
    assert!(checked_aggregate_event_count([u64::MAX, 1]).is_err());
}

#[test]
fn unconfirmed_mutator_authority_cannot_be_cleanly_finalized() {
    let (fixture, session) = Fixture::started("finalization-mutator");
    let id = session.session_id().clone();
    session.effects.0.borrow_mut().command_active = true;
    let error = session.finalize("student-1").unwrap_err().to_string();
    assert!(
        error.contains("command") || error.contains("mutator"),
        "{error}"
    );
    assert_eq!(terminal_count(&fixture.0, &id).1, 0);
}

#[test]
fn review1_invalid_ancestry_retains_exportable_capture_after_live_source_changes() {
    for remove in [false, true] {
        let (parent, mut first) = Fixture::started("review1-recovery-parent");
        first.execute(EditorCommand::Insert('B')).unwrap();
        let first_receipt = first.finalize("student-1").unwrap();
        let child = Fixture::new("review1-recovery-child");
        for (path, bytes) in first_receipt.final_workspace() {
            fs::write(child.0.join(path.as_str()), bytes).unwrap();
        }
        let mut second = ProductionSession::start_revision(&parent.0, &child.0, MANIFEST).unwrap();
        let child_id = second.session_id().clone();
        second.execute(EditorCommand::Insert('C')).unwrap();
        let source = first_receipt
            .payload_path(&first_receipt.manifest().segments[0].events.entry)
            .unwrap();
        if remove {
            fs::remove_file(source).unwrap();
        } else {
            fs::write(source, b"corrupt-parent-events").unwrap();
        }

        assert!(second.finalize("student-1").is_err());
        fs::write(child.0.join("main.rs"), "mutable-after-failure").unwrap();
        let first_recovery = assert_exportable_missing_ancestry(
            ProductionSession::recover_finalization(&child.0).unwrap(),
            b"CBA",
        );
        let second_recovery = assert_exportable_missing_ancestry(
            ProductionSession::recover_finalization(&child.0).unwrap(),
            b"CBA",
        );
        assert_eq!(
            first_recovery.manifest(),
            second_recovery.manifest(),
            "retry must use only the same durable capture"
        );
        assert_eq!(terminal_count(&child.0, &child_id).1, 0);
    }
}

#[test]
fn review1_over_budget_ancestry_retains_current_capture_without_truncation() {
    let (parent, first) = Fixture::started("review1-budget-parent");
    let first_receipt = first.finalize("student-1").unwrap();
    let child = Fixture::new("review1-budget-child");
    for (path, bytes) in first_receipt.final_workspace() {
        fs::write(child.0.join(path.as_str()), bytes).unwrap();
    }
    let mut second = ProductionSession::start_revision(&parent.0, &child.0, MANIFEST).unwrap();
    let child_id = second.session_id().clone();
    second.execute(EditorCommand::Insert('C')).unwrap();

    assert!(
        second
            .finalize_with_aggregate_limit(
                "student-1",
                first_receipt.manifest().aggregate_event_count,
            )
            .is_err()
    );
    fs::write(child.0.join("main.rs"), "mutable-after-budget").unwrap();
    let FinalizationStatus::Incomplete(incomplete) =
        ProductionSession::recover_finalization(&child.0).unwrap()
    else {
        panic!("over-budget ancestry cannot become clean");
    };
    assert!(incomplete.capture_available);
    assert!(
        incomplete.manifest().is_none(),
        "available over-budget ancestry must not be truncated into a subset manifest"
    );
    assert!(!incomplete.payloads().is_empty());
    assert_eq!(
        incomplete.final_workspace().unwrap()[&WorkspacePath::new("main.rs").unwrap()],
        b"CA"
    );
    assert_eq!(terminal_count(&child.0, &child_id).1, 0);
}

#[test]
fn review1_published_receipt_rejects_binding_tampering() {
    for rewrite_prepared in [false, true] {
        let (fixture, session) = Fixture::started("review1-receipt-binding");
        let id = session.session_id().clone();
        session.finalize("student-1").unwrap();
        let names = if rewrite_prepared {
            &["finalization-receipt.json", "finalization-prepared.json"][..]
        } else {
            &["finalization-receipt.json"][..]
        };
        tamper_binding(&fixture.0, names, "prefix_event_hash");
        let status = ProductionSession::recover_finalization(&fixture.0).unwrap();
        assert!(matches!(status, FinalizationStatus::Incomplete(_)));
        assert_eq!(terminal_count(&fixture.0, &id).1, 1);
    }
}

#[test]
fn review2_abandonment_evidence_is_not_revision_ancestry() {
    let (original, session) = Fixture::started("review2-abandoned-original");
    let original_id = session.session_id().clone();
    session.quit().unwrap();
    fs::write(
        state_artifact(&original.0, &format!("{original_id}.sqlite")),
        b"corrupt retained original",
    )
    .unwrap();

    let fresh = Fixture::new("review2-abandoned-fresh");
    fs::write(fresh.0.join("main.rs"), "A").unwrap();
    let mut replacement = ProductionSession::abandon_into(&original.0, &fresh.0, MANIFEST).unwrap();
    replacement.execute(EditorCommand::Insert('B')).unwrap();

    let receipt = replacement
        .finalize("student-1")
        .expect("accepted abandonment evidence must not be parsed as revision ancestry");
    assert_eq!(receipt.manifest().segments.len(), 1);
    assert!(receipt.manifest().segments[0].parent.is_none());
    assert_eq!(receipt.manifest().segments[0].evidence.len(), 1);
    assert_eq!(
        receipt.final_workspace()[&WorkspacePath::new("main.rs").unwrap()],
        b"BA"
    );
}

#[test]
fn review2_current_capture_survives_later_revision_link_damage() {
    let (parent, mut first) = Fixture::started("review2-link-parent");
    first.execute(EditorCommand::Insert('B')).unwrap();
    let first_receipt = first.finalize("student-1").unwrap();
    let child = Fixture::new("review2-link-child");
    for (path, bytes) in first_receipt.final_workspace() {
        fs::write(child.0.join(path.as_str()), bytes).unwrap();
    }
    let mut second = ProductionSession::start_revision(&parent.0, &child.0, MANIFEST).unwrap();
    second.execute(EditorCommand::Insert('C')).unwrap();
    fs::remove_file(
        first_receipt
            .payload_path(&first_receipt.manifest().segments[0].events.entry)
            .unwrap(),
    )
    .unwrap();
    assert!(second.finalize("student-1").is_err());

    fs::write(state_artifact(&child.0, "parent.json"), b"later damage").unwrap();
    fs::write(child.0.join("main.rs"), "mutable after capture").unwrap();
    assert_exportable_missing_ancestry(
        ProductionSession::recover_finalization(&child.0).unwrap(),
        b"CBA",
    );
}

#[test]
fn review2_markerless_partial_capture_can_resume_and_be_replaced() {
    let (fixture, session) = Fixture::started("review2-partial-capture");
    let collision = state_artifact(
        &fixture.0,
        "finalization-recovery-checkpoint-00000000000000000001.rcpk",
    );
    fs::write(&collision, b"injected later-publication conflict").unwrap();

    assert!(session.finalize("student-1").is_err());
    assert!(state_artifact(&fixture.0, "finalization-recovery-events.jsonl").exists());
    assert!(!state_artifact(&fixture.0, "finalization-recovery-capture.json").exists());
    fs::remove_file(collision).unwrap();

    let mut resumed = ProductionSession::resume(&fixture.0, MANIFEST, ResumeChoice::Resume)
        .expect("marker-less partial publication must not fence mutable recovery");
    resumed.execute(EditorCommand::Insert('B')).unwrap();
    let receipt = resumed
        .finalize("student-1")
        .expect("the next complete capture must replace its prior partial components");
    assert_eq!(
        receipt.final_workspace()[&WorkspacePath::new("main.rs").unwrap()],
        b"BA"
    );
}

#[test]
fn review2_missing_evidence_retains_a_truthful_recovery_export() {
    let (fixture, mut session) = Fixture::started("review2-missing-evidence");
    let id = session.session_id().clone();
    fs::write(fixture.0.join("main.rs"), "external bytes").unwrap();
    session.save_all().unwrap();
    let evidence = session.evidence_paths()[0].clone();
    fs::remove_file(evidence).unwrap();

    assert!(session.finalize("student-1").is_err());
    let FinalizationStatus::Incomplete(incomplete) =
        ProductionSession::recover_finalization(&fixture.0).unwrap()
    else {
        panic!("missing referenced evidence cannot recover as clean");
    };
    assert!(incomplete.capture_available);
    let manifest = incomplete
        .manifest()
        .expect("missing evidence is representable in the accepted recovery model");
    let RprovPackageState::RecoveryIncomplete {
        unavailable_assurances,
        gaps,
    } = &manifest.package_state
    else {
        panic!("missing evidence must remain visibly incomplete");
    };
    assert!(unavailable_assurances.contains(&RprovUnavailableAssurance::ReferencedEvidence));
    assert!(!gaps.is_empty());
    assert!(
        gaps.iter()
            .all(|gap| matches!(gap, RprovRecoveryGap::MissingEvidence { .. }))
    );
    assert!(manifest.segments[0].evidence.is_empty());
    let events = incomplete
        .read_payload(&manifest.segments[0].events.entry)
        .unwrap();
    validate_rprov_event_stream(manifest, &manifest.segments[0], &events).unwrap();
    assert_eq!(
        incomplete.final_workspace().unwrap()[&WorkspacePath::new("main.rs").unwrap()],
        b"A"
    );
    assert_eq!(terminal_count(&fixture.0, &id).1, 0);
}

#[test]
fn review2_actual_aggregate_failure_never_fabricates_missing_ancestry() {
    let (parent, first) = Fixture::started("review2-actual-limit-parent");
    let first_receipt = first.finalize("student-1").unwrap();
    let parent_count = first_receipt.manifest().aggregate_event_count;
    let child = Fixture::new("review2-actual-limit-child");
    for (path, bytes) in first_receipt.final_workspace() {
        fs::write(child.0.join(path.as_str()), bytes).unwrap();
    }
    let mut second = ProductionSession::start_revision(&parent.0, &child.0, MANIFEST).unwrap();
    second.execute(EditorCommand::Insert('C')).unwrap();
    let parent_events = first_receipt
        .payload_path(&first_receipt.manifest().segments[0].events.entry)
        .unwrap()
        .to_path_buf();
    let retained_parent_events = fs::read(&parent_events).unwrap();
    fs::remove_file(&parent_events).unwrap();
    assert!(second.finalize("student-1").is_err());
    fs::write(parent_events, retained_parent_events).unwrap();

    let FinalizationStatus::Incomplete(incomplete) =
        ProductionSession::recover_finalization_with_actual_aggregate_limit(&child.0, parent_count)
            .unwrap()
    else {
        panic!("an injected actual aggregate failure cannot finalize");
    };
    assert!(incomplete.capture_available);
    assert!(
        incomplete.manifest().is_none(),
        "available ancestry that exceeds the actual aggregate check must not be relabeled missing"
    );
    assert_eq!(
        incomplete.final_workspace().unwrap()[&WorkspacePath::new("main.rs").unwrap()],
        b"CA"
    );
}

#[test]
fn review3_pre_capture_revision_link_damage_retains_raw_current_capture() {
    let (parent, mut first) = Fixture::started("review3-link-parent");
    first.execute(EditorCommand::Insert('B')).unwrap();
    let first_receipt = first.finalize("student-1").unwrap();
    let child = Fixture::new("review3-link-child");
    for (path, bytes) in first_receipt.final_workspace() {
        fs::write(child.0.join(path.as_str()), bytes).unwrap();
    }
    let mut second = ProductionSession::start_revision(&parent.0, &child.0, MANIFEST).unwrap();
    let child_id = second.session_id().clone();
    second.execute(EditorCommand::Insert('C')).unwrap();
    fs::write(
        state_artifact(&child.0, "parent.json"),
        b"damaged before capture",
    )
    .unwrap();

    assert!(second.finalize("student-1").is_err());
    fs::write(child.0.join("main.rs"), "mutable after failure").unwrap();
    let FinalizationStatus::Incomplete(incomplete) =
        ProductionSession::recover_finalization(&child.0).unwrap()
    else {
        panic!("uncertifiable revision ancestry cannot recover as clean");
    };
    assert!(incomplete.capture_available);
    assert!(
        incomplete.manifest().is_none(),
        "a damaged seed cannot support invented ancestry or starter facts"
    );
    assert!(!incomplete.payloads().is_empty());
    assert_eq!(
        incomplete.final_workspace().unwrap()[&WorkspacePath::new("main.rs").unwrap()],
        b"CBA"
    );
    assert_eq!(terminal_count(&child.0, &child_id).1, 0);
    assert!(ProductionSession::resume(&child.0, MANIFEST, ResumeChoice::Resume).is_err());
}

#[test]
fn review3_current_only_evidence_aggregate_retains_raw_capture() {
    let fixture = Fixture::new("review3-current-aggregate");
    let mut random = 0x9e37_79b9_7f4a_7c15_u64;
    let original = (0..1024 * 1024)
        .map(|_| {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            b' ' + (random % 95) as u8
        })
        .collect::<Vec<_>>();
    fs::write(fixture.0.join("main.rs"), &original).unwrap();
    let mut session = ProductionSession::start(&fixture.0, MANIFEST).unwrap();
    let id = session.session_id().clone();
    for index in 0_u8..28 {
        let mut external = original.clone();
        external[..8].copy_from_slice(format!("{index:08}").as_bytes());
        fs::write(fixture.0.join("main.rs"), external).unwrap();
        session.save_all().unwrap();
    }

    assert!(session.finalize("student-1").is_err());
    let FinalizationStatus::Incomplete(incomplete) =
        ProductionSession::recover_finalization(&fixture.0).unwrap()
    else {
        panic!("an over-limit current segment cannot recover as clean");
    };
    assert!(
        incomplete.reason.contains("segment evidence bytes")
            && incomplete.reason.contains("maximum is 67108864"),
        "the fixture must reach the actual v1 current-segment aggregate: {}",
        incomplete.reason
    );
    assert!(incomplete.capture_available);
    assert!(
        incomplete.manifest().is_none(),
        "an unrepresentable current manifest must remain a raw capture"
    );
    assert!(!incomplete.payloads().is_empty());
    assert_eq!(
        incomplete.final_workspace().unwrap()[&WorkspacePath::new("main.rs").unwrap()],
        original
    );
    assert_eq!(terminal_count(&fixture.0, &id).1, 0);
}

#[test]
fn review3_missing_parent_evidence_retains_parent_segments_and_exact_gaps() {
    let (parent, mut first) = Fixture::started("review3-parent-evidence");
    first.execute(EditorCommand::Insert('B')).unwrap();
    first.save_all().unwrap();
    fs::write(parent.0.join("main.rs"), "external parent bytes").unwrap();
    first.save_all().unwrap();
    let first_receipt = first.finalize("student-1").unwrap();
    let missing = first_receipt.manifest().segments[0].evidence[0].clone();

    let child = Fixture::new("review3-child-evidence");
    for (path, bytes) in first_receipt.final_workspace() {
        fs::write(child.0.join(path.as_str()), bytes).unwrap();
    }
    let mut second = ProductionSession::start_revision(&parent.0, &child.0, MANIFEST).unwrap();
    let child_id = second.session_id().clone();
    second.execute(EditorCommand::Insert('C')).unwrap();
    fs::remove_file(first_receipt.payload_path(&missing.entry).unwrap()).unwrap();

    assert!(second.finalize("student-1").is_err());
    let FinalizationStatus::Incomplete(incomplete) =
        ProductionSession::recover_finalization(&child.0).unwrap()
    else {
        panic!("missing parent evidence cannot recover as clean");
    };
    assert!(incomplete.capture_available);
    let manifest = incomplete
        .manifest()
        .expect("available parent structure must remain representable");
    let RprovPackageState::RecoveryIncomplete {
        unavailable_assurances,
        gaps,
    } = &manifest.package_state
    else {
        panic!("missing parent evidence must remain visibly incomplete");
    };
    assert_eq!(manifest.segments.len(), 2);
    assert!(!unavailable_assurances.contains(&RprovUnavailableAssurance::CompleteAncestry));
    assert!(unavailable_assurances.contains(&RprovUnavailableAssurance::ReferencedEvidence));
    assert_eq!(gaps.len(), missing.usages.len());
    for (gap, usage) in gaps.iter().zip(&missing.usages) {
        assert!(matches!(
            gap,
            RprovRecoveryGap::MissingEvidence { event, blake3 }
                if event == usage && blake3 == &missing.blake3
        ));
    }
    assert!(manifest.segments[0].evidence.is_empty());
    assert!(
        manifest
            .inventory
            .iter()
            .all(|entry| entry.path != missing.entry)
    );
    assert_eq!(
        incomplete.final_workspace().unwrap()[&WorkspacePath::new("main.rs").unwrap()],
        b"CBA"
    );
    assert_eq!(terminal_count(&child.0, &child_id).1, 0);
}
