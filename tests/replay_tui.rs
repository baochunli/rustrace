#[path = "support/test_home.rs"]
mod test_home;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use rustrace::{
    replay_tui::{
        ComparisonMode, DiffContent, DiffLineKind, EvidenceTarget, PasteMarker, ReplayController,
    },
    session::{ProductionSession, create_bundle},
    tui::EditorCommand,
};
use rustrace_editor::Movement;
use rustrace_model::{
    DecodeOutcome, DecodePolicy, Event, PasteInputChannel, PasteRejectionReason,
    RPROV_CONTAINER_HEADER_BYTES, RPROV_RECORD_HEADER_BYTES, SelectionState, decode_envelope,
    decode_rprov_record_header,
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Replay"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["**"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

const REPRESENTATIVE_SOURCE_LINES: usize = 128;
const REPRESENTATIVE_TARGET_LINE: usize = 80;

fn representative_long_source() -> String {
    (0..REPRESENTATIVE_SOURCE_LINES)
        .map(|line| {
            if line == REPRESENTATIVE_TARGET_LINE {
                "let café = \"東京\"; // fixture line 081: replay source viewport and selection target.\n"
                    .to_owned()
            } else {
                format!(
                    "// fixture line {:03}: stable context for replay source viewport and selection coverage.\n",
                    line + 1
                )
            }
        })
        .collect()
}

struct Fixture {
    base: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        Self::with_main_source(name, "A")
    }

    fn with_main_source(name: &str, source: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "rustrace-replay-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = base.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("main.rs"), source).unwrap();
        Self {
            base: fs::canonicalize(base).unwrap(),
            workspace: fs::canonicalize(workspace).unwrap(),
        }
    }

    fn representative_long_file(name: &str) -> (Self, String) {
        let source = representative_long_source();
        (Self::with_main_source(name, &source), source)
    }

    fn start(&self) -> ProductionSession {
        ProductionSession::start(&self.workspace, MANIFEST).unwrap()
    }

    fn bundle(&self, session: ProductionSession, name: &str) -> PathBuf {
        let receipt = session.finalize("student-1").unwrap();
        create_bundle(&receipt, &self.base.join(name)).unwrap().path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn tamper_stored_zip_entry(mut archive: Vec<u8>, wanted: &str, replacement: &[u8]) -> Vec<u8> {
    let end = archive.len() - 22;
    let mut central = u32::from_le_bytes(archive[end + 16..end + 20].try_into().unwrap()) as usize;
    loop {
        let expanded =
            u32::from_le_bytes(archive[central + 24..central + 28].try_into().unwrap()) as usize;
        let name_len =
            u16::from_le_bytes(archive[central + 28..central + 30].try_into().unwrap()) as usize;
        let extra_len =
            u16::from_le_bytes(archive[central + 30..central + 32].try_into().unwrap()) as usize;
        let comment_len =
            u16::from_le_bytes(archive[central + 32..central + 34].try_into().unwrap()) as usize;
        let name = std::str::from_utf8(&archive[central + 46..central + 46 + name_len]).unwrap();
        if name == wanted {
            assert_eq!(replacement.len(), expanded);
            let local = u32::from_le_bytes(archive[central + 42..central + 46].try_into().unwrap())
                as usize;
            let local_name_len =
                u16::from_le_bytes(archive[local + 26..local + 28].try_into().unwrap()) as usize;
            let local_extra_len =
                u16::from_le_bytes(archive[local + 28..local + 30].try_into().unwrap()) as usize;
            let data = local + 30 + local_name_len + local_extra_len;
            archive[data..data + replacement.len()].copy_from_slice(replacement);
            let crc = crc32fast::hash(replacement).to_le_bytes();
            archive[local + 14..local + 18].copy_from_slice(&crc);
            archive[central + 16..central + 20].copy_from_slice(&crc);
            return archive;
        }
        central += 46 + name_len + extra_len + comment_len;
    }
}

fn stored_zip_entry(archive: &[u8], wanted: &str) -> Vec<u8> {
    let mut offset = 0;
    while archive.get(offset..offset + 4) == Some(&0x0403_4b50_u32.to_le_bytes()) {
        let compressed =
            u32::from_le_bytes(archive[offset + 18..offset + 22].try_into().unwrap()) as usize;
        let name_len =
            u16::from_le_bytes(archive[offset + 26..offset + 28].try_into().unwrap()) as usize;
        let extra_len =
            u16::from_le_bytes(archive[offset + 28..offset + 30].try_into().unwrap()) as usize;
        let name_start = offset + 30;
        let data_start = name_start + name_len + extra_len;
        let name = std::str::from_utf8(&archive[name_start..name_start + name_len]).unwrap();
        if name == wanted {
            return archive[data_start..data_start + compressed].to_vec();
        }
        offset = data_start + compressed;
    }
    panic!("missing stored ZIP entry {wanted}");
}

fn corrupt_first_evidence_payload(mut rprov: Vec<u8>) -> Vec<u8> {
    let mut offset = RPROV_CONTAINER_HEADER_BYTES;
    while offset < rprov.len() {
        let header =
            decode_rprov_record_header(&rprov[offset..offset + RPROV_RECORD_HEADER_BYTES]).unwrap();
        let path_start = offset + RPROV_RECORD_HEADER_BYTES;
        let payload_start = path_start + usize::from(header.path_bytes);
        let payload_end = payload_start + header.payload_bytes as usize;
        let path = std::str::from_utf8(&rprov[path_start..payload_start]).unwrap();
        if path.contains("/evidence/") {
            rprov[payload_start] ^= 1;
            return rprov;
        }
        offset = payload_end;
    }
    panic!("missing external-change evidence payload");
}

fn replace_text(session: &mut ProductionSession, text: &str) {
    session.execute(EditorCommand::SelectAll).unwrap();
    insert_text(session, text);
}

fn insert_text(session: &mut ProductionSession, text: &str) {
    for character in text.chars() {
        session.execute(EditorCommand::Insert(character)).unwrap();
    }
}

fn select_visual_range(
    session: &mut ProductionSession,
    line: usize,
    start_column: usize,
    end_column: usize,
) {
    session
        .execute(EditorCommand::MoveTo {
            line,
            column: start_column,
            selecting: false,
        })
        .unwrap();
    session
        .execute(EditorCommand::MoveTo {
            line,
            column: end_column,
            selecting: true,
        })
        .unwrap();
}

fn selected_source<'a>(replay: &'a ReplayController, path: &str) -> &'a [u8] {
    replay
        .selected_event()
        .expect("replay has no selected event")
        .source
        .iter()
        .find(|(candidate, _)| candidate == path)
        .map(|(_, bytes)| bytes.as_slice())
        .unwrap_or_else(|| panic!("selected event has no source for {path}"))
}

#[test]
fn production_long_source_selection_fixture_reconstructs_current_events() {
    let (fixture, source) = Fixture::representative_long_file("source-selection");
    assert_eq!(source.lines().count(), REPRESENTATIVE_SOURCE_LINES);
    assert!((10 * 1024..=12 * 1024).contains(&source.len()));

    let mut session = fixture.start();
    select_visual_range(&mut session, REPRESENTATIVE_TARGET_LINE, 4, 8);
    insert_text(&mut session, "target");
    let bundle = fixture.bundle(session, "source-selection.zip");

    let mut replay = ReplayController::open(&bundle).unwrap();
    assert!(replay.verification_report().is_clean());
    let rows = replay.timeline_rows(replay.event_count());
    let selected_range = rows
        .iter()
        .rfind(|row| row.event_name == "selection changed")
        .map(|row| row.position)
        .expect("fixture did not record its selected range");
    let partial_edits = rows
        .iter()
        .filter(|row| {
            row.position.sequence > selected_range.sequence && row.event_name == "file edited"
        })
        .map(|row| row.position)
        .take(2)
        .collect::<Vec<_>>();
    assert_eq!(partial_edits.len(), 2, "fixture needs two partial edits");

    replay.select(selected_range).unwrap();
    assert_eq!(selected_source(&replay, "main.rs"), source.as_bytes());
    let selected = replay.selected_event().unwrap();
    let DecodeOutcome::Decoded(envelope) =
        decode_envelope(&selected.event_bytes, DecodePolicy::RejectUnsupported).unwrap()
    else {
        panic!("selected production event was skipped");
    };
    assert_eq!(selected.millis, envelope.monotonic_millis);
    let Event::SelectionChanged(selection) = envelope.event else {
        panic!("selected production event was not a selection change");
    };
    let target_line_start = source
        .lines()
        .take(REPRESENTATIVE_TARGET_LINE)
        .map(|line| line.len() + 1)
        .sum::<usize>();
    assert_eq!(selection.anchor_byte, (target_line_start + 4) as u64);
    assert_eq!(selection.active_byte, (target_line_start + 9) as u64);
    let selected_document = selected
        .active_document
        .as_ref()
        .expect("selected event omitted its reconstructed active document");
    assert_eq!(selected_document.document_id, selection.document_id);
    assert_eq!(selected_document.path, "main.rs");
    assert_eq!(
        selected_document.selection,
        SelectionState::new(selection.anchor_byte, selection.active_byte)
    );
    assert!(
        replay.last_seek_applied_events() > 0,
        "a selection-only event incorrectly reused checkpoint projection metadata"
    );

    replay.select(partial_edits[0]).unwrap();
    let first_edit_source = source.replacen("café", "t", 1);
    assert_eq!(
        selected_source(&replay, "main.rs"),
        first_edit_source.as_bytes()
    );
    assert_eq!(
        replay
            .selected_event()
            .unwrap()
            .active_document
            .as_ref()
            .unwrap()
            .selection,
        SelectionState::caret((target_line_start + 5) as u64)
    );

    replay.select(partial_edits[1]).unwrap();
    let second_edit_source = source.replacen("café", "ta", 1);
    assert_eq!(
        selected_source(&replay, "main.rs"),
        second_edit_source.as_bytes()
    );
    assert_eq!(
        replay
            .selected_event()
            .unwrap()
            .active_document
            .as_ref()
            .unwrap()
            .selection,
        SelectionState::caret((target_line_start + 6) as u64)
    );

    let final_position = replay.positions().last().unwrap();
    replay.select(final_position).unwrap();
    let final_source = source.replacen("café", "target", 1);
    assert_eq!(selected_source(&replay, "main.rs"), final_source.as_bytes());
    assert!(replay.cache_bytes() <= 64 * 1024 * 1024);
}

#[test]
fn find_replacement_transactions_reconstruct_exact_final_source() {
    let fixture = Fixture::new("find-replacement");
    fs::write(fixture.workspace.join("main.rs"), "cat\r\ncat 猫 cat\r\n").unwrap();
    let mut session = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
    session
        .execute(EditorCommand::Search("cat".to_owned()))
        .unwrap();
    session
        .execute(EditorCommand::ReplaceCurrent {
            query: "cat".to_owned(),
            replacement: "dog".to_owned(),
        })
        .unwrap();
    session
        .execute(EditorCommand::ReplaceAll {
            query: "cat".to_owned(),
            replacement: "狐".to_owned(),
        })
        .unwrap();
    let bundle = fixture.bundle(session, "find-replacement.zip");

    let mut replay = ReplayController::open(&bundle).unwrap();
    let last = replay.positions().last().unwrap();
    replay.select(last).unwrap();
    let selected = replay.selected_event().unwrap();
    assert_eq!(
        selected
            .source
            .iter()
            .find(|(path, _)| path == "main.rs")
            .map(|(_, bytes)| bytes.as_slice()),
        Some("dog\r\n狐 猫 狐\r\n".as_bytes())
    );
    assert_eq!(
        replay
            .timeline_rows(replay.event_count())
            .iter()
            .filter(|row| row.event_name == "file edited")
            .count(),
        2
    );
}

fn changed_lines(replay: &ReplayController, kind: DiffLineKind) -> Vec<&str> {
    match &replay.diff_view().expect("diff pane is closed").content {
        DiffContent::Lines(lines) => lines
            .iter()
            .filter(|line| line.kind == kind)
            .map(|line| line.text.as_str())
            .collect(),
        DiffContent::Notice(notice) => panic!("unexpected diff notice: {}", notice.text()),
    }
}

#[test]
fn current_to_final_diff_is_exact_and_toggle_preserves_replay_state() {
    let fixture = Fixture::new("current-final-diff");
    fs::write(fixture.workspace.join("main.rs"), "initial\nsame\n").unwrap();
    let mut session = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
    replace_text(&mut session, "current\nsame\n");
    session.capture_boundary().unwrap();
    replace_text(&mut session, "final\nsame\nadded\n");
    let bundle = fixture.bundle(session, "diff.zip");

    let mut replay = ReplayController::open(&bundle).unwrap();
    let current = replay
        .timeline_rows(replay.event_count())
        .into_iter()
        .filter(|row| row.position.segment == 0 && row.event_name == "checkpoint")
        .nth(1)
        .map(|row| row.position)
        .expect("missing intermediate checkpoint");
    replay.select(current).unwrap();
    replay.toggle_play();
    let cache_bytes = replay.cache_bytes();
    let selected = replay.selected_event().unwrap().position;

    replay.toggle_diff("main.rs").unwrap();

    assert_eq!(replay.selected_event().unwrap().position, selected);
    assert_eq!(replay.cache_bytes(), cache_bytes);
    assert!(replay.is_playing());

    assert_eq!(replay.comparison_mode(), ComparisonMode::Final);
    assert_eq!(changed_lines(&replay, DiffLineKind::Deletion), ["current"]);
    assert_eq!(
        changed_lines(&replay, DiffLineKind::Insertion),
        ["final", "added"]
    );
    assert_eq!(replay.diff_view().unwrap().deletions, 1);
    assert_eq!(replay.diff_view().unwrap().insertions, 2);

    replay.toggle_diff("main.rs").unwrap();
    assert!(replay.diff_view().is_none());
    assert_eq!(replay.selected_event().unwrap().position, selected);
    assert_eq!(replay.cache_bytes(), cache_bytes);
    assert!(replay.is_playing());

    replay.toggle_diff("main.rs").unwrap();
    assert!(replay.diff_view().is_some());
    replay.next_event().unwrap();
    assert!(replay.diff_view().is_none(), "seek retained a stale diff");
}

#[test]
fn final_only_file_is_available_as_a_whole_file_insertion() {
    let fixture = Fixture::new("final-only-diff");
    let mut session = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
    session.capture_boundary().unwrap();
    session.create_file("final.rs").unwrap();
    replace_text(&mut session, "added\n");
    let bundle = fixture.bundle(session, "final-only.zip");

    let mut replay = ReplayController::open(&bundle).unwrap();
    let current_position = replay
        .timeline_rows(replay.event_count())
        .into_iter()
        .filter(|row| row.event_name == "checkpoint")
        .nth(1)
        .map(|row| row.position)
        .expect("missing intermediate checkpoint");
    replay.select(current_position).unwrap();

    replay.toggle_diff("main.rs").unwrap();
    assert!(
        replay
            .diff_view()
            .unwrap()
            .files
            .iter()
            .any(|path| path == "final.rs")
    );
    replay.select_diff_file("final.rs").unwrap();
    assert_eq!(changed_lines(&replay, DiffLineKind::Insertion), ["added"]);
    assert_eq!(replay.diff_view().unwrap().deletions, 0);
}

#[test]
fn previous_checkpoint_mode_uses_nearest_checkpoint_and_final_uses_tip_tree() {
    let fixture = Fixture::new("checkpoint-diff");
    let mut parent = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
    replace_text(&mut parent, "P\n");
    parent.capture_boundary().unwrap();
    replace_text(&mut parent, "Q\n");
    let parent_receipt = parent.finalize("student-1").unwrap();
    let child_root = fixture.base.join("child");
    fs::create_dir(&child_root).unwrap();
    for (path, bytes) in parent_receipt.final_workspace() {
        fs::write(child_root.join(path.as_str()), bytes).unwrap();
    }
    let mut child =
        ProductionSession::start_revision(&fixture.workspace, &child_root, MANIFEST).unwrap();
    replace_text(&mut child, "C");
    child.capture_boundary().unwrap();
    child.execute(EditorCommand::Insert('X')).unwrap();
    replace_text(&mut child, "F\n");
    let bundle = fixture.bundle(child, "revision-diff.zip");

    let mut replay = ReplayController::open(&bundle).unwrap();
    let rows = replay.timeline_rows(replay.event_count());
    let parent_checkpoint = rows
        .iter()
        .filter(|row| row.position.segment == 0 && row.event_name == "checkpoint")
        .nth(1)
        .unwrap()
        .position;
    replay.select(parent_checkpoint).unwrap();
    replay.toggle_diff("main.rs").unwrap();
    assert_eq!(changed_lines(&replay, DiffLineKind::Deletion), ["P"]);
    assert_eq!(changed_lines(&replay, DiffLineKind::Insertion), ["F"]);
    replay.toggle_diff("main.rs").unwrap();

    let child_checkpoint = rows
        .iter()
        .filter(|row| row.position.segment == 1 && row.event_name == "checkpoint")
        .nth(1)
        .unwrap()
        .position;
    let child_current = rows
        .iter()
        .find(|row| {
            row.position.segment == 1
                && row.position.sequence > child_checkpoint.sequence
                && row.event_name == "file edited"
        })
        .unwrap()
        .position;
    replay.select(child_current).unwrap();
    replay.toggle_diff("main.rs").unwrap();
    replay.toggle_diff_mode("main.rs").unwrap();

    let view = replay.diff_view().unwrap();
    assert_eq!(view.mode, ComparisonMode::PreviousCheckpoint);
    assert_eq!(
        view.comparison,
        Some(rustrace::replay_tui::ComparisonPoint {
            segment: 1,
            sequence: child_checkpoint.sequence,
        })
    );
    assert_eq!(changed_lines(&replay, DiffLineKind::Deletion), ["C"]);
    assert_eq!(changed_lines(&replay, DiffLineKind::Insertion), ["CX"]);
}

#[test]
fn clean_bundle_validates_then_seeks_from_a_certified_checkpoint() {
    let fixture = Fixture::new("seek");
    let mut session = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
    session.execute(EditorCommand::Insert('B')).unwrap();
    session.capture_boundary().unwrap();
    session.execute(EditorCommand::Insert('C')).unwrap();
    let expected = session.workspace().logical_files().unwrap();
    let bundle = fixture.bundle(session, "submission.zip");

    let mut replay = ReplayController::open(&bundle).unwrap();
    assert!(replay.verification_report().is_clean());
    assert!(replay.timeline_available());
    let final_position = replay.positions().last().unwrap();
    replay.select(final_position).unwrap();

    let selected = replay.selected_event().unwrap();
    let actual = selected
        .source
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect::<Vec<_>>();
    let expected = expected
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    assert!(
        replay.last_seek_applied_events() < final_position.sequence.saturating_sub(1),
        "seek replayed the whole segment instead of restoring a certified checkpoint"
    );
    assert!(replay.cache_bytes() <= 64 * 1024 * 1024);
    assert!(
        replay.cache_bytes() > 0,
        "package-backed seek cache must retain a charged certificate or cursor"
    );
}

#[test]
fn cached_source_projection_tracks_the_active_file_at_the_selected_event() {
    let fixture = Fixture::new("focus");
    fs::write(fixture.workspace.join("second.rs"), "B").unwrap();
    let mut session = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
    let initial_active = session.workspace().active_path().clone();
    session.execute(EditorCommand::NextBuffer).unwrap();
    let expected_active = session.workspace().active_path().clone();
    assert_ne!(expected_active, initial_active);
    let bundle = fixture.bundle(session, "focus.zip");

    let mut replay = ReplayController::open(&bundle).unwrap();
    let focus = replay
        .timeline_rows(replay.event_count())
        .into_iter()
        .find(|row| row.event_name == "file focused")
        .map(|row| row.position)
        .expect("missing focused-file state");
    replay.select(focus).unwrap();

    assert_eq!(
        replay.selected_event().unwrap().active_file.as_deref(),
        Some(expected_active.as_str())
    );
}

#[test]
fn corrupted_bundle_exposes_validation_failure_without_a_timeline() {
    let fixture = Fixture::new("corrupt");
    let session = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
    let bundle = fixture.bundle(session, "submission.zip");
    let mut bytes = fs::read(bundle).unwrap();
    bytes[0] ^= 0xff;
    let corrupt = fixture.base.join("corrupt.zip");
    fs::write(&corrupt, bytes).unwrap();

    let replay = ReplayController::open(&corrupt).unwrap();
    assert!(!replay.verification_report().is_clean());
    assert!(!replay.timeline_available());
    assert!(replay.selected_event().is_none());
}

#[test]
fn linked_attempt_boundary_has_unknown_inter_attempt_time() {
    let fixture = Fixture::new("segments");
    let mut parent = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
    parent.execute(EditorCommand::Insert('B')).unwrap();
    let parent_receipt = parent.finalize("student-1").unwrap();
    let child_root = fixture.base.join("child");
    fs::create_dir(&child_root).unwrap();
    for (path, bytes) in parent_receipt.final_workspace() {
        fs::write(child_root.join(path.as_str()), bytes).unwrap();
    }
    let mut child =
        ProductionSession::start_revision(&fixture.workspace, &child_root, MANIFEST).unwrap();
    child.execute(EditorCommand::Insert('C')).unwrap();
    let bundle = fixture.bundle(child, "revision.zip");

    let mut replay = ReplayController::open(&bundle).unwrap();
    let boundary = replay
        .positions()
        .find(|position| position.segment == 1 && position.sequence == 1)
        .unwrap();
    replay.select(boundary).unwrap();
    assert!(replay.selected_event().unwrap().inter_attempt_time_unknown);
    assert!(
        replay
            .timeline_rows(replay.event_count())
            .iter()
            .any(|row| row.position == boundary && row.attempt_boundary)
    );
}

#[test]
fn paste_markers_distinguish_allowed_internal_and_blocked_metadata_only() {
    let fixture = Fixture::new("paste");
    let mut session = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
    session.execute(EditorCommand::SelectAll).unwrap();
    session.execute(EditorCommand::Copy).unwrap();
    session
        .execute(EditorCommand::Move {
            movement: Movement::DocumentEnd,
            selecting: false,
        })
        .unwrap();
    session.execute(EditorCommand::Paste).unwrap();
    session
        .reject_paste(
            PasteInputChannel::TerminalBracketed,
            PasteRejectionReason::ExternalInput,
        )
        .unwrap();
    let bundle = fixture.bundle(session, "paste.zip");
    let mut replay = ReplayController::open(&bundle).unwrap();

    let positions = replay.positions().collect::<Vec<_>>();
    let mut allowed = None;
    let mut blocked = false;
    for position in positions {
        replay.select(position).unwrap();
        match replay.selected_event().unwrap().paste_marker.clone() {
            Some(PasteMarker::AllowedInternal { source }) => allowed = Some((position, source)),
            Some(PasteMarker::BlockedMetadataOnly) => blocked = true,
            _ => {}
        }
    }
    let (paste, source) = allowed.expect("missing allowed internal-paste marker");
    assert!(blocked, "missing blocked metadata-only marker");
    replay.select(paste).unwrap();
    assert_eq!(
        replay.follow_selected_evidence().unwrap(),
        Some(EvidenceTarget::Event(source))
    );
    assert_eq!(replay.selected_event().unwrap().position, source);
}

#[test]
fn evidence_artifact_link_opens_a_bounded_preview() {
    let fixture = Fixture::new("evidence");
    let mut session = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
    fs::write(fixture.workspace.join("main.rs"), "external bytes").unwrap();
    assert!(session.recheck_external().unwrap());
    let bundle = fixture.bundle(session, "evidence.zip");
    let mut replay = ReplayController::open(&bundle).unwrap();

    let positions = replay.positions().collect::<Vec<_>>();
    let (position, target) = positions
        .into_iter()
        .find_map(|position| {
            replay.select(position).unwrap();
            match replay.selected_event().unwrap().evidence_target.clone() {
                Some(target @ EvidenceTarget::Artifact(_)) => Some((position, target)),
                _ => None,
            }
        })
        .expect("missing evidence artifact link");
    replay.select(position).unwrap();

    assert_eq!(replay.follow_selected_evidence().unwrap(), Some(target));
    let (path, preview) = replay.artifact_preview().expect("missing artifact preview");
    assert!(!path.is_empty());
    assert!(!preview.is_empty());
    assert!(preview.len() <= 64 * 1024);
}

#[cfg(unix)]
#[test]
fn replay_pty_sanitizes_hostile_source_and_restores_an_80x24_terminal() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("pty");
    fs::write(
        fixture.workspace.join("main.rs"),
        "fn main() { /* \u{1b}]52;unsafe\u{7} */ }\n",
    )
    .unwrap();
    let mut session = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
    for character in ['a', 'b', 'c', 'd', 'e', 'f'] {
        session.execute(EditorCommand::Insert(character)).unwrap();
    }
    session.save_all().unwrap();
    let bundle = fixture.bundle(session, "pty.zip");
    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/replay_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .arg(bundle)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn source_follow_and_manual_mode_work_in_real_loop_at_both_layout_sizes() {
    let test_home = test_home::TestHome::new(false);
    let (fixture, _) = Fixture::representative_long_file("source-interaction-pty");
    let mut session = fixture.start();
    select_visual_range(&mut session, REPRESENTATIVE_TARGET_LINE, 4, 8);
    insert_text(&mut session, "target");
    let bundle = fixture.bundle(session, "source-interaction-pty.zip");

    for (width, height) in [(80, 24), (150, 45)] {
        let output = test_home
            .command("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/replay_interaction_pty.py"
            ))
            .arg(env!("CARGO_BIN_EXE_rustrace"))
            .arg(&bundle)
            .arg(width.to_string())
            .arg(height.to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Source replay PTY failed at {width}x{height}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(unix)]
#[test]
fn two_flag_replay_pty_keeps_limitations_and_scrolls_the_flags_view_at_80x24() {
    let test_home = test_home::TestHome::new(false);
    let fixture = Fixture::new("flagged-pty");
    let mut session = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
    fs::write(fixture.workspace.join("main.rs"), "outside").unwrap();
    session.save_all().unwrap();
    let clean = fixture.bundle(session, "clean.zip");
    let flagged = fixture.base.join("flagged.zip");
    let clean_archive = fs::read(clean).unwrap();
    let corrupt_rprov =
        corrupt_first_evidence_payload(stored_zip_entry(&clean_archive, "session.rprov"));
    let archive = tamper_stored_zip_entry(clean_archive, "session.rprov", &corrupt_rprov);
    let mut source = stored_zip_entry(&archive, "main.rs");
    source[0] ^= 1;
    fs::write(
        &flagged,
        tamper_stored_zip_entry(archive, "main.rs", &source),
    )
    .unwrap();

    let output = test_home
        .command("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/replay_flags_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_rustrace"))
        .arg(flagged)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
