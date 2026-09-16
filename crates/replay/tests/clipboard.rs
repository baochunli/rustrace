use rustrace_journal::{CheckpointFile, CheckpointSnapshot, OpenDocument, StoredCheckpoint};
use rustrace_model::*;
use rustrace_replay::ReplayEngine;

const TEXT: &str = "é\r\n🦀";

fn path(value: &str) -> WorkspacePath {
    WorkspacePath::new(value).unwrap()
}

fn id(value: &str) -> DocumentId {
    DocumentId::new(value).unwrap()
}

fn initial(session: &str) -> StoredCheckpoint {
    let session_id = SessionId::new(session).unwrap();
    let snapshot = CheckpointSnapshot::new(
        session_id.clone(),
        1,
        vec![
            CheckpointFile {
                path: path("a.rs"),
                contents: TEXT.as_bytes().to_vec(),
            },
            CheckpointFile {
                path: path("b.rs"),
                contents: Vec::new(),
            },
        ],
        Some(id("source")),
        vec![
            OpenDocument {
                document_id: id("source"),
                path: path("a.rs"),
                version: 0,
                selection: SelectionState::new(0, TEXT.len() as u64),
            },
            OpenDocument {
                document_id: id("destination"),
                path: path("b.rs"),
                version: 0,
                selection: SelectionState::caret(0),
            },
        ],
    )
    .unwrap();
    let owning_event = EventEnvelope {
        format_version: 1,
        session_id,
        sequence: 1,
        monotonic_millis: 0,
        wall_clock_utc: None,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event: Event::WorkspaceCheckpoint(snapshot.event_payload()),
    }
    .seal(Hash::zero())
    .unwrap();
    StoredCheckpoint {
        owning_event,
        snapshot,
    }
}

fn envelope(replay: &ReplayEngine, event: Event) -> EventEnvelope {
    EventEnvelope {
        format_version: 1,
        session_id: replay.session_id().clone(),
        sequence: replay.next_sequence(),
        monotonic_millis: replay.next_sequence(),
        wall_clock_utc: None,
        previous_event_hash: replay.last_event_hash(),
        event_hash: Hash::zero(),
        event,
    }
    .seal(replay.last_event_hash())
    .unwrap()
}

fn source(replay: &ReplayEngine) -> ClipboardSource {
    ClipboardSource {
        prefix: RecordedEventRef {
            session_id: replay.session_id().clone(),
            sequence: replay.next_sequence() - 1,
            event_hash: replay.last_event_hash(),
        },
        document_id: id("source"),
        path: path("a.rs"),
        version: 0,
        content_hash: document_hash(TEXT),
        start_byte: 0,
        end_byte: TEXT.len() as u64,
    }
}

fn copy(replay: &mut ReplayEngine) -> RecordedEventRef {
    let event = envelope(replay, Event::ClipboardCopied(source(replay)));
    replay.apply(&event).unwrap();
    RecordedEventRef {
        session_id: event.session_id,
        sequence: event.sequence,
        event_hash: event.event_hash,
    }
}

fn transaction(origin: EditOrigin, text: &str) -> EditorTransaction {
    EditorTransaction {
        document_id: id("destination"),
        version_before: 0,
        version_after: 1,
        origin,
        edits: vec![TextEdit {
            start_byte: 0,
            end_byte: 0,
            inserted_text: text.to_owned(),
        }],
        selection_before: SelectionState::caret(0),
        selection_after: SelectionState::caret(text.len() as u64),
        hash_before: document_hash(""),
        hash_after: document_hash(text),
    }
}

fn paste(reference: RecordedEventRef) -> Event {
    Event::InternalPaste(InternalPaste {
        source: reference,
        transaction: transaction(EditOrigin::Paste, TEXT),
    })
}

#[test]
fn exact_source_validation_rejects_resealed_identity_version_range_and_prefix_forgery() {
    for field in 0..9 {
        let mut replay = ReplayEngine::from_initial_checkpoint(initial("attempt-a")).unwrap();
        let mut forged = source(&replay);
        match field {
            0 => forged.prefix.session_id = SessionId::new("attempt-b").unwrap(),
            1 => forged.prefix.sequence += 1,
            2 => forged.prefix.event_hash = Hash::zero(),
            3 => forged.document_id = id("destination"),
            4 => forged.path = path("b.rs"),
            5 => forged.version = 1,
            6 => forged.content_hash = Hash::zero(),
            7 => forged.start_byte = 1, // inside é
            8 => forged.end_byte -= 1,  // inside crab
            _ => unreachable!(),
        }
        let event = envelope(&replay, Event::ClipboardCopied(forged));
        let before = replay.workspace_state().clone();
        assert!(replay.apply(&event).is_err(), "forged field {field}");
        assert_eq!(replay.workspace_state(), &before);
        assert_eq!(replay.next_sequence(), 2);
        copy(&mut replay); // Failed evidence must not advance or poison pure replay.
    }
}

#[test]
fn required_paste_link_cannot_be_missing_stale_foreign_or_equal_text_only() {
    for field in 0..6 {
        let mut replay = ReplayEngine::from_initial_checkpoint(initial("attempt-a")).unwrap();
        let mut reference = copy(&mut replay);
        match field {
            0 => reference.session_id = SessionId::new("attempt-b").unwrap(),
            1 => reference.sequence += 1,
            2 => reference.event_hash = Hash::zero(),
            3 => {
                copy(&mut replay);
            }
            4 => {
                let resumed = envelope(
                    &replay,
                    Event::SessionResumed(SessionResumed {
                        last_sequence: replay.next_sequence() - 1,
                    }),
                );
                replay.apply(&resumed).unwrap();
            }
            5 => {
                replay = ReplayEngine::from_initial_checkpoint(initial("attempt-a")).unwrap();
            }
            _ => unreachable!(),
        }
        let event = envelope(&replay, paste(reference));
        let before = replay.workspace_state().clone();
        assert!(replay.apply(&event).is_err(), "invalid source case {field}");
        assert_eq!(replay.workspace_state(), &before);
    }
}

#[test]
fn equal_length_replacement_cannot_change_source_linked_bytes() {
    let mut replay = ReplayEngine::from_initial_checkpoint(initial("attempt-a")).unwrap();
    let reference = copy(&mut replay);
    let forged = envelope(
        &replay,
        Event::InternalPaste(InternalPaste {
            source: reference.clone(),
            transaction: transaction(EditOrigin::Paste, "ê\r\n🦀"),
        }),
    );
    let before = replay.workspace_state().clone();
    assert!(replay.apply(&forged).is_err());
    assert_eq!(replay.workspace_state(), &before);
    let valid = envelope(&replay, paste(reference));
    replay.apply(&valid).unwrap();
    assert_eq!(
        replay.workspace_state().file(&path("b.rs")).unwrap(),
        TEXT.as_bytes()
    );
    assert_eq!(
        replay
            .workspace_state()
            .document(&id("destination"))
            .unwrap()
            .version(),
        1
    );
}

#[test]
fn deleted_source_is_retained_by_continuously_certified_checkpoint_seek() {
    let mut replay = ReplayEngine::from_initial_checkpoint(initial("attempt-a")).unwrap();
    let reference = copy(&mut replay);
    let deleted = envelope(
        &replay,
        Event::FileDeleted(FileDeleted {
            document_id: id("source"),
            path: path("a.rs"),
            previous_hash: document_hash(TEXT),
        }),
    );
    replay.apply(&deleted).unwrap();
    let state = replay.workspace_state();
    let snapshot = CheckpointSnapshot::new(
        replay.session_id().clone(),
        replay.next_sequence(),
        state
            .files()
            .iter()
            .map(|(path, contents)| CheckpointFile {
                path: path.clone(),
                contents: contents.clone(),
            })
            .collect(),
        state.active_document().cloned(),
        state
            .documents()
            .values()
            .map(|document| OpenDocument {
                document_id: document.document_id().clone(),
                path: document.path().clone(),
                version: document.version(),
                selection: document.selection(),
            })
            .collect(),
    )
    .unwrap();
    let owning_event = envelope(
        &replay,
        Event::WorkspaceCheckpoint(snapshot.event_payload()),
    );
    replay.apply(&owning_event).unwrap();
    let certified = replay
        .certify_checkpoint(StoredCheckpoint {
            owning_event,
            snapshot,
        })
        .unwrap();
    let mut restored = ReplayEngine::from_checkpoint(certified);
    let event = envelope(&restored, paste(reference));
    restored.apply(&event).unwrap();
    assert_eq!(
        restored.workspace_state().file(&path("b.rs")).unwrap(),
        TEXT.as_bytes()
    );
    assert!(restored.workspace_state().file(&path("a.rs")).is_none());
}

#[test]
fn historical_paste_and_unknown_are_not_retroactively_certified_or_rejected() {
    for origin in [EditOrigin::Paste, EditOrigin::Unknown] {
        let mut replay = ReplayEngine::from_initial_checkpoint(initial("historical")).unwrap();
        let event = envelope(
            &replay,
            Event::FileEdited(transaction(origin, "historical input")),
        );
        let bytes = encode_envelope(&event).unwrap();
        let DecodeOutcome::Decoded(decoded) =
            decode_envelope(&bytes, DecodePolicy::RejectUnsupported).unwrap()
        else {
            panic!("historical event skipped")
        };
        assert_eq!(encode_envelope(&decoded).unwrap(), bytes);
        replay.apply(&decoded).unwrap();
        assert_eq!(
            replay.workspace_state().file(&path("b.rs")).unwrap(),
            b"historical input"
        );
    }
}

#[test]
fn segment_scoped_links_remain_distinct_at_the_same_original_sequence() {
    // T6 will package these unchanged segments and validate parent/child links.
    // The core must already distinguish references without global renumbering.
    let mut first = ReplayEngine::from_initial_checkpoint(initial("attempt-a")).unwrap();
    let mut second = ReplayEngine::from_initial_checkpoint(initial("attempt-b")).unwrap();
    let first_ref = copy(&mut first);
    let second_ref = copy(&mut second);
    assert_eq!(first_ref.sequence, second_ref.sequence);
    let wrong = envelope(&second, paste(first_ref.clone()));
    assert!(second.apply(&wrong).is_err());
    let first_paste = envelope(&first, paste(first_ref));
    let second_paste = envelope(&second, paste(second_ref));
    first.apply(&first_paste).unwrap();
    second.apply(&second_paste).unwrap();
    assert_eq!(first.workspace_state(), second.workspace_state());
}

#[test]
fn terminal_closure_expires_replay_source_evidence_cache() {
    for finalized in [false, true] {
        let mut replay = ReplayEngine::from_initial_checkpoint(initial("attempt-a")).unwrap();
        let reference = copy(&mut replay);
        let event = if finalized {
            Event::SubmissionFinalized(SubmissionFinalized {
                final_workspace_hash: replay.current_workspace_hash(),
                event_count: replay.next_sequence(),
                clean: true,
                warnings: Vec::new(),
            })
        } else {
            Event::SessionEnded(SessionEnded {
                final_workspace_hash: replay.current_workspace_hash(),
            })
        };
        replay.apply(&envelope(&replay, event)).unwrap();
        assert!(replay.clipboard_text(&reference).is_err());
    }
}
