use rustrace_journal::{
    CheckpointError, CheckpointFile, CheckpointSnapshot, MAX_CHECKPOINT_DOCUMENTS, OpenDocument,
    StoredCheckpoint, decode_checkpoint, encode_checkpoint,
};
use rustrace_model::{
    CommandFinished, CommandId, CommandOutput, CommandStarted, CompletionAccepted,
    CompletionRequested, Diagnostic, DiagnosticSeverity, DocumentId, EditOrigin, EditorTransaction,
    Event, EventEnvelope, ExternalFileChange, FORMAT_VERSION_V1, FileCreated, FileDeleted,
    FileFocused, FileRenamed, Hash, MAX_INSERTED_TEXT_BYTES, OutputStream, SelectionChanged,
    SelectionState, SessionEnded, SessionId, SessionStarted, SubmissionFinalized, TextEdit,
    ViewportChanged, WorkspaceDirectory, WorkspacePath, document_hash,
};
use rustrace_replay::{MAX_ACTIVE_CARGO_COMMANDS, ReplayEngine, ReplayError};
use rustrace_workspace::hash::{MAX_WORKSPACE_FILE_BYTES, MAX_WORKSPACE_FILES, hash_entries};

fn session_id(value: &str) -> SessionId {
    SessionId::new(value).unwrap()
}

fn document_id(value: &str) -> DocumentId {
    DocumentId::new(value).unwrap()
}

fn path(value: &str) -> WorkspacePath {
    WorkspacePath::new(value).unwrap()
}

fn file(path_value: &str, contents: &[u8]) -> CheckpointFile {
    CheckpointFile {
        path: path(path_value),
        contents: contents.to_vec(),
    }
}

fn document(id: &str, path_value: &str, version: u64, selection: SelectionState) -> OpenDocument {
    OpenDocument {
        document_id: document_id(id),
        path: path(path_value),
        selection,
        version,
    }
}

fn checkpoint(
    session_id: &SessionId,
    sequence: u64,
    files: Vec<CheckpointFile>,
    active_document: Option<DocumentId>,
    documents: Vec<OpenDocument>,
    previous_event_hash: Hash,
) -> StoredCheckpoint {
    let snapshot = CheckpointSnapshot::new(
        session_id.clone(),
        sequence,
        files,
        active_document,
        documents,
    )
    .unwrap();
    let owning_event = envelope(
        session_id,
        sequence,
        previous_event_hash,
        Event::WorkspaceCheckpoint(snapshot.event_payload()),
    );
    StoredCheckpoint {
        owning_event,
        snapshot,
    }
}

fn envelope(
    session_id: &SessionId,
    sequence: u64,
    previous_event_hash: Hash,
    event: Event,
) -> EventEnvelope {
    EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: session_id.clone(),
        sequence,
        monotonic_millis: sequence * 10,
        wall_clock_utc: None,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event,
    }
    .seal(previous_event_hash)
    .unwrap()
}

fn initial_checkpoint(session_id: &SessionId) -> StoredCheckpoint {
    checkpoint(
        session_id,
        1,
        vec![
            file("binary.dat", &[0xff, 0x00]),
            file("src/lib.rs", b"abc"),
        ],
        Some(document_id("doc-lib")),
        vec![document(
            "doc-lib",
            "src/lib.rs",
            0,
            SelectionState::caret(0),
        )],
        Hash::zero(),
    )
}

fn edit_transaction(
    id: &str,
    before: &str,
    after: &str,
    version_before: u64,
    selection_before: SelectionState,
    selection_after: SelectionState,
    edit: TextEdit,
) -> EditorTransaction {
    EditorTransaction {
        document_id: document_id(id),
        version_before,
        version_after: version_before + 1,
        origin: EditOrigin::Keyboard,
        edits: vec![edit],
        selection_before,
        selection_after,
        hash_before: document_hash(before),
        hash_after: document_hash(after),
    }
}

fn apply_event(
    replay: &mut ReplayEngine,
    session_id: &SessionId,
    sequence: u64,
    previous_hash: &mut Hash,
    event: Event,
) -> EventEnvelope {
    let envelope = envelope(session_id, sequence, *previous_hash, event);
    replay.apply(&envelope).unwrap();
    *previous_hash = envelope.event_hash;
    envelope
}

#[test]
fn restores_a_validated_checkpoint_without_touching_the_filesystem() {
    let session = session_id("restore");
    let checkpoint = initial_checkpoint(&session);
    let replay = ReplayEngine::from_initial_checkpoint(checkpoint).unwrap();
    let state = replay.workspace_state();

    assert_eq!(state.file(&path("binary.dat")), Some(&[0xff, 0x00][..]));
    assert_eq!(state.file(&path("src/lib.rs")), Some(&b"abc"[..]));
    assert_eq!(state.active_document(), Some(&document_id("doc-lib")));
    let document = state.document(&document_id("doc-lib")).unwrap();
    assert_eq!(document.path(), &path("src/lib.rs"));
    assert_eq!(document.text(), "abc");
    assert_eq!(document.version(), 0);
    assert_eq!(document.selection(), SelectionState::caret(0));
    assert_eq!(state.workspace_hash(), replay.current_workspace_hash());
}

#[test]
fn complete_replay_and_checkpoint_seek_produce_the_same_exact_state() {
    let session = session_id("seek");
    let initial = initial_checkpoint(&session);
    let mut full = ReplayEngine::from_initial_checkpoint(initial).unwrap();
    let mut previous = full.last_event_hash();

    apply_event(
        &mut full,
        &session,
        2,
        &mut previous,
        Event::SelectionChanged(SelectionChanged {
            document_id: document_id("doc-lib"),
            anchor_byte: 2,
            active_byte: 2,
        }),
    );
    apply_event(
        &mut full,
        &session,
        3,
        &mut previous,
        Event::FileEdited(edit_transaction(
            "doc-lib",
            "abc",
            "ab🦀c",
            0,
            SelectionState::caret(2),
            SelectionState::caret(6),
            TextEdit {
                start_byte: 2,
                end_byte: 2,
                inserted_text: "🦀".to_owned(),
            },
        )),
    );
    apply_event(
        &mut full,
        &session,
        4,
        &mut previous,
        Event::FileCreated(FileCreated {
            document_id: document_id("doc-new"),
            path: path("src/new.rs"),
            contents: "new".to_owned(),
            content_hash: document_hash("new"),
        }),
    );
    apply_event(
        &mut full,
        &session,
        5,
        &mut previous,
        Event::FileFocused(FileFocused {
            document_id: document_id("doc-new"),
        }),
    );
    apply_event(
        &mut full,
        &session,
        6,
        &mut previous,
        Event::FileRenamed(FileRenamed {
            document_id: document_id("doc-new"),
            old_path: path("src/new.rs"),
            new_path: path("src/renamed.rs"),
        }),
    );
    apply_event(
        &mut full,
        &session,
        7,
        &mut previous,
        Event::ExternalFileChange(ExternalFileChange {
            path: path("notes.txt"),
            previous_contents: None,
            new_contents: Some("external".to_owned()),
            previous_hash: None,
            new_hash: Some(document_hash("external")),
        }),
    );

    let later = checkpoint(
        &session,
        8,
        vec![
            file("binary.dat", &[0xff, 0x00]),
            file("notes.txt", b"external"),
            file("src/lib.rs", "ab🦀c".as_bytes()),
            file("src/renamed.rs", b"new"),
        ],
        Some(document_id("doc-new")),
        vec![
            document("doc-lib", "src/lib.rs", 1, SelectionState::caret(6)),
            document("doc-new", "src/renamed.rs", 0, SelectionState::caret(0)),
        ],
        previous,
    );
    full.apply(&later.owning_event).unwrap();
    previous = later.owning_event.event_hash;
    let cursor = full.certify_checkpoint_cursor(&later).unwrap();
    let certificate = full.certify_checkpoint(later.clone()).unwrap();

    let delete = envelope(
        &session,
        9,
        previous,
        Event::FileDeleted(FileDeleted {
            document_id: document_id("doc-new"),
            path: path("src/renamed.rs"),
            previous_hash: document_hash("new"),
        }),
    );
    full.apply(&delete).unwrap();
    let final_hash = full.current_workspace_hash();
    let finalized = envelope(
        &session,
        10,
        delete.event_hash,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: final_hash,
            event_count: 10,
            clean: true,
            warnings: vec![],
        }),
    );
    full.apply(&finalized).unwrap();

    let mut seek = ReplayEngine::from_checkpoint(certificate);
    seek.apply(&delete).unwrap();
    seek.apply(&finalized).unwrap();
    let mut compact_seek = ReplayEngine::from_checkpoint_cursor(cursor, later).unwrap();
    compact_seek.apply(&delete).unwrap();
    compact_seek.apply(&finalized).unwrap();

    assert_eq!(seek.workspace_state(), full.workspace_state());
    assert_eq!(seek.last_event_hash(), full.last_event_hash());
    assert_eq!(seek.next_sequence(), full.next_sequence());
    assert!(seek.is_finalized());
    assert_eq!(compact_seek.workspace_state(), full.workspace_state());
    assert_eq!(compact_seek.last_event_hash(), full.last_event_hash());
    assert_eq!(compact_seek.next_sequence(), full.next_sequence());
    assert!(compact_seek.is_finalized());
}

#[test]
fn post_checkpoint_undo_is_validated_without_unavailable_editor_history() {
    let session = session_id("undo-seek");
    let initial = checkpoint(
        &session,
        1,
        vec![file("src/lib.rs", b"abcd")],
        Some(document_id("doc")),
        vec![document("doc", "src/lib.rs", 4, SelectionState::caret(4))],
        Hash::zero(),
    );
    let mut prefix = ReplayEngine::from_initial_checkpoint(initial).unwrap();
    let later = checkpoint(
        &session,
        2,
        vec![file("src/lib.rs", b"abcd")],
        Some(document_id("doc")),
        vec![document("doc", "src/lib.rs", 4, SelectionState::caret(4))],
        prefix.last_event_hash(),
    );
    prefix.apply(&later.owning_event).unwrap();
    let certificate = prefix.certify_checkpoint(later).unwrap();
    let mut replay = ReplayEngine::from_checkpoint(certificate);
    let mut transaction = edit_transaction(
        "doc",
        "abcd",
        "abc",
        4,
        SelectionState::caret(4),
        SelectionState::caret(3),
        TextEdit {
            start_byte: 3,
            end_byte: 4,
            inserted_text: String::new(),
        },
    );
    transaction.origin = EditOrigin::Undo;
    let event = envelope(
        &session,
        3,
        replay.last_event_hash(),
        Event::FileEdited(transaction),
    );

    replay.apply(&event).unwrap();

    assert_eq!(
        replay
            .workspace_state()
            .document(&document_id("doc"))
            .unwrap()
            .text(),
        "abc"
    );
}

#[test]
fn rejects_corrupt_or_wrongly_owned_checkpoints_with_typed_errors() {
    let session = session_id("bad-checkpoint");
    let mut corrupt_bytes = encode_checkpoint(&initial_checkpoint(&session).snapshot).unwrap();
    let last = corrupt_bytes.len() - 1;
    corrupt_bytes[last] ^= 1;
    assert!(matches!(
        decode_checkpoint(&corrupt_bytes),
        Err(CheckpointError::IntegrityMismatch)
    ));

    let mut wrong_owner = initial_checkpoint(&session);
    wrong_owner.owning_event.session_id = session_id("other");
    assert!(matches!(
        ReplayEngine::from_initial_checkpoint(wrong_owner),
        Err(ReplayError::Checkpoint(_))
    ));

    let mut wrong_hash = initial_checkpoint(&session);
    wrong_hash.owning_event.event_hash = Hash::zero();
    assert!(matches!(
        ReplayEngine::from_initial_checkpoint(wrong_hash),
        Err(ReplayError::EventHashMismatch { .. })
    ));
}

#[test]
fn rejects_wrong_session_sequence_and_chain_without_advancing() {
    let session = session_id("ordering");
    let mut replay = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
    let event = Event::FileFocused(FileFocused {
        document_id: document_id("doc-lib"),
    });

    let wrong_session = envelope(
        &session_id("other"),
        2,
        replay.last_event_hash(),
        event.clone(),
    );
    assert!(matches!(
        replay.apply(&wrong_session),
        Err(ReplayError::WrongSession { .. })
    ));
    let wrong_sequence = envelope(&session, 3, replay.last_event_hash(), event.clone());
    assert!(matches!(
        replay.apply(&wrong_sequence),
        Err(ReplayError::UnexpectedSequence { .. })
    ));
    let wrong_previous = envelope(&session, 2, Hash::from_bytes([9; 32]), event.clone());
    assert!(matches!(
        replay.apply(&wrong_previous),
        Err(ReplayError::PreviousEventHashMismatch { .. })
    ));

    let valid = envelope(&session, 2, replay.last_event_hash(), event);
    replay.apply(&valid).unwrap();
    assert_eq!(replay.next_sequence(), 3);
}

#[test]
fn invalid_utf8_edit_range_is_typed_and_atomic() {
    let session = session_id("utf8-range");
    let checkpoint = checkpoint(
        &session,
        1,
        vec![file("src/lib.rs", "a🦀b".as_bytes())],
        Some(document_id("doc")),
        vec![document("doc", "src/lib.rs", 0, SelectionState::caret(0))],
        Hash::zero(),
    );
    let mut replay = ReplayEngine::from_initial_checkpoint(checkpoint).unwrap();
    let transaction = edit_transaction(
        "doc",
        "a🦀b",
        "axb",
        0,
        SelectionState::caret(0),
        SelectionState::caret(2),
        TextEdit {
            start_byte: 2,
            end_byte: 5,
            inserted_text: "x".to_owned(),
        },
    );
    let invalid = envelope(
        &session,
        2,
        replay.last_event_hash(),
        Event::FileEdited(transaction),
    );

    assert!(matches!(
        replay.apply(&invalid),
        Err(ReplayError::Transaction { .. })
    ));
    assert_eq!(
        replay.workspace_state().file(&path("src/lib.rs")),
        Some("a🦀b".as_bytes())
    );
    assert_eq!(replay.next_sequence(), 2);
}

#[test]
fn version_selection_and_document_hash_mismatches_are_typed() {
    let session = session_id("transaction-mismatch");
    for mutation in 0..4 {
        let mut replay =
            ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
        let mut transaction = edit_transaction(
            "doc-lib",
            "abc",
            "abcd",
            0,
            SelectionState::caret(0),
            SelectionState::caret(4),
            TextEdit {
                start_byte: 3,
                end_byte: 3,
                inserted_text: "d".to_owned(),
            },
        );
        match mutation {
            0 => {
                transaction.version_before = 2;
                transaction.version_after = 3;
            }
            1 => transaction.selection_before = SelectionState::caret(1),
            2 => transaction.hash_before = Hash::from_bytes([2; 32]),
            3 => transaction.hash_after = Hash::from_bytes([3; 32]),
            _ => unreachable!(),
        }
        let invalid = envelope(
            &session,
            2,
            replay.last_event_hash(),
            Event::FileEdited(transaction),
        );
        assert!(matches!(
            replay.apply(&invalid),
            Err(ReplayError::Transaction { .. })
        ));
        assert_eq!(replay.next_sequence(), 2);
    }
}

#[test]
fn duplicate_and_missing_file_lifecycle_events_are_rejected() {
    let session = session_id("lifecycle");
    let cases = [
        Event::FileCreated(FileCreated {
            document_id: document_id("new-id"),
            path: path("src/lib.rs"),
            contents: String::new(),
            content_hash: document_hash(""),
        }),
        Event::FileCreated(FileCreated {
            document_id: document_id("doc-lib"),
            path: path("other.rs"),
            contents: String::new(),
            content_hash: document_hash(""),
        }),
        Event::FileDeleted(FileDeleted {
            document_id: document_id("missing"),
            path: path("missing.rs"),
            previous_hash: document_hash(""),
        }),
        Event::FileRenamed(FileRenamed {
            document_id: document_id("missing"),
            old_path: path("missing.rs"),
            new_path: path("new.rs"),
        }),
    ];

    for event in cases {
        let mut replay =
            ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
        let event = envelope(&session, 2, replay.last_event_hash(), event);
        assert!(matches!(
            replay.apply(&event),
            Err(ReplayError::FileLifecycle { .. })
        ));
        assert_eq!(replay.next_sequence(), 2);
    }
}

#[test]
fn external_change_validates_option_pairs_contents_hashes_and_lifecycle() {
    let session = session_id("external");
    let invalid_events = [
        ExternalFileChange {
            path: path("outside.txt"),
            previous_contents: None,
            new_contents: Some("new".to_owned()),
            previous_hash: Some(document_hash("old")),
            new_hash: Some(document_hash("new")),
        },
        ExternalFileChange {
            path: path("src/lib.rs"),
            previous_contents: Some("wrong".to_owned()),
            new_contents: Some("new".to_owned()),
            previous_hash: Some(document_hash("wrong")),
            new_hash: Some(document_hash("new")),
        },
        ExternalFileChange {
            path: path("outside.txt"),
            previous_contents: None,
            new_contents: None,
            previous_hash: None,
            new_hash: None,
        },
    ];

    for change in invalid_events {
        let mut replay =
            ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
        let event = envelope(
            &session,
            2,
            replay.last_event_hash(),
            Event::ExternalFileChange(change),
        );
        assert!(matches!(
            replay.apply(&event),
            Err(ReplayError::ExternalFileChange { .. })
        ));
    }
}

#[test]
fn checkpoint_and_final_workspace_hash_mismatches_are_rejected() {
    let session = session_id("final-hash");
    let mut replay = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
    let wrong = Hash::from_bytes([8; 32]);
    let checkpoint_event = envelope(
        &session,
        2,
        replay.last_event_hash(),
        Event::WorkspaceCheckpoint(rustrace_model::WorkspaceCheckpoint {
            workspace_hash: wrong,
            documents: vec![],
        }),
    );
    assert!(matches!(
        replay.apply(&checkpoint_event),
        Err(ReplayError::CheckpointStateMismatch { .. })
    ));

    let finalized = envelope(
        &session,
        2,
        replay.last_event_hash(),
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: wrong,
            event_count: 2,
            clean: false,
            warnings: vec!["mismatch".to_owned()],
        }),
    );
    assert!(matches!(
        replay.apply(&finalized),
        Err(ReplayError::FinalWorkspaceHashMismatch { .. })
    ));
    assert!(matches!(
        replay.verify_final_workspace_hash(wrong),
        Err(ReplayError::FinalWorkspaceHashMismatch { .. })
    ));
}

#[test]
fn resulting_workspace_limits_are_enforced_before_commit() {
    let session = session_id("result-limit");
    let initial_text = "x".repeat(MAX_WORKSPACE_FILE_BYTES as usize);
    let checkpoint = checkpoint(
        &session,
        1,
        vec![file("src/lib.rs", initial_text.as_bytes())],
        Some(document_id("doc")),
        vec![document("doc", "src/lib.rs", 0, SelectionState::caret(0))],
        Hash::zero(),
    );
    let mut replay = ReplayEngine::from_initial_checkpoint(checkpoint).unwrap();
    let transaction = edit_transaction(
        "doc",
        &initial_text,
        &format!("y{initial_text}"),
        0,
        SelectionState::caret(0),
        SelectionState::caret(1),
        TextEdit {
            start_byte: 0,
            end_byte: 0,
            inserted_text: "y".to_owned(),
        },
    );
    let event = envelope(
        &session,
        2,
        replay.last_event_hash(),
        Event::FileEdited(transaction),
    );

    assert!(matches!(
        replay.apply(&event),
        Err(ReplayError::WorkspaceLimit { .. })
    ));
    assert_eq!(
        replay.workspace_state().file(&path("src/lib.rs")),
        Some(initial_text.as_bytes())
    );
}

#[test]
fn workspace_file_count_limit_is_enforced_before_create() {
    let session = session_id("count-limit");
    let files = (0..MAX_WORKSPACE_FILES)
        .map(|index| file(&format!("{index:03}.txt"), b""))
        .collect();
    let checkpoint = checkpoint(&session, 1, files, None, vec![], Hash::zero());
    let mut replay = ReplayEngine::from_initial_checkpoint(checkpoint).unwrap();
    let event = envelope(
        &session,
        2,
        replay.last_event_hash(),
        Event::FileCreated(FileCreated {
            document_id: document_id("overflow"),
            path: path("overflow.txt"),
            contents: String::new(),
            content_hash: document_hash(""),
        }),
    );

    assert!(matches!(
        replay.apply(&event),
        Err(ReplayError::WorkspaceLimit { .. })
    ));
    assert_eq!(replay.workspace_state().files().len(), MAX_WORKSPACE_FILES);
}

#[test]
fn retained_open_document_count_is_bounded_after_external_deletes() {
    let session = session_id("document-count-limit");
    let files: Vec<_> = (0..MAX_CHECKPOINT_DOCUMENTS)
        .map(|index| file(&format!("{index:03}.txt"), b""))
        .collect();
    let documents: Vec<_> = (0..MAX_CHECKPOINT_DOCUMENTS)
        .map(|index| {
            document(
                &format!("doc-{index:03}"),
                &format!("{index:03}.txt"),
                0,
                SelectionState::caret(0),
            )
        })
        .collect();
    let checkpoint = checkpoint(&session, 1, files, None, documents, Hash::zero());
    let mut replay = ReplayEngine::from_initial_checkpoint(checkpoint).unwrap();
    let mut previous = replay.last_event_hash();
    for index in 0..MAX_CHECKPOINT_DOCUMENTS {
        apply_event(
            &mut replay,
            &session,
            index as u64 + 2,
            &mut previous,
            Event::ExternalFileChange(ExternalFileChange {
                path: path(&format!("{index:03}.txt")),
                previous_contents: Some(String::new()),
                new_contents: None,
                previous_hash: Some(document_hash("")),
                new_hash: None,
            }),
        );
    }
    let attempted_sequence = MAX_CHECKPOINT_DOCUMENTS as u64 + 2;
    let event = envelope(
        &session,
        attempted_sequence,
        previous,
        Event::FileCreated(FileCreated {
            document_id: document_id("overflow"),
            path: path("overflow.txt"),
            contents: String::new(),
            content_hash: document_hash(""),
        }),
    );

    assert!(matches!(
        replay.apply(&event),
        Err(ReplayError::OpenDocumentCountLimit {
            attempted: 257,
            maximum: 256
        })
    ));
    assert_eq!(
        replay.workspace_state().documents().len(),
        MAX_CHECKPOINT_DOCUMENTS
    );
    assert!(replay.workspace_state().files().is_empty());
    assert_eq!(replay.next_sequence(), attempted_sequence);
}

#[test]
fn retained_open_document_bytes_are_bounded_after_external_delete() {
    let session = session_id("document-bytes-limit");
    let contents = vec![b'x'; MAX_INSERTED_TEXT_BYTES];
    let files: Vec<_> = (0..40)
        .map(|index| file(&format!("{index:02}.txt"), &contents))
        .collect();
    let documents: Vec<_> = (0..40)
        .map(|index| {
            document(
                &format!("doc-{index:02}"),
                &format!("{index:02}.txt"),
                0,
                SelectionState::caret(0),
            )
        })
        .collect();
    let checkpoint = checkpoint(&session, 1, files, None, documents, Hash::zero());
    let mut replay = ReplayEngine::from_initial_checkpoint(checkpoint).unwrap();
    let deleted_text = String::from_utf8(contents).unwrap();
    let deleted = envelope(
        &session,
        2,
        replay.last_event_hash(),
        Event::ExternalFileChange(ExternalFileChange {
            path: path("00.txt"),
            previous_contents: Some(deleted_text.clone()),
            new_contents: None,
            previous_hash: Some(document_hash(&deleted_text)),
            new_hash: None,
        }),
    );
    replay.apply(&deleted).unwrap();
    let event = envelope(
        &session,
        3,
        deleted.event_hash,
        Event::FileCreated(FileCreated {
            document_id: document_id("overflow"),
            path: path("overflow.txt"),
            contents: "x".to_owned(),
            content_hash: document_hash("x"),
        }),
    );

    assert!(matches!(
        replay.apply(&event),
        Err(ReplayError::OpenDocumentBytesLimit { .. })
    ));
    assert_eq!(replay.next_sequence(), 3);
    assert!(
        replay
            .workspace_state()
            .file(&path("overflow.txt"))
            .is_none()
    );
}

#[test]
fn session_started_cannot_follow_the_replay_checkpoint() {
    let session = session_id("late-session-start");
    let mut replay = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
    let event = envelope(
        &session,
        2,
        replay.last_event_hash(),
        Event::SessionStarted(SessionStarted {
            client_version: "0.1.0".to_owned(),
            starter_workspace_hash: replay.current_workspace_hash(),
        }),
    );

    assert!(matches!(
        replay.apply(&event),
        Err(ReplayError::SessionLifecycle {
            event: "session_started",
            ..
        })
    ));
    assert_eq!(replay.next_sequence(), 2);
}

#[test]
fn external_document_divergence_must_be_resolved_before_checkpointing() {
    let session = session_id("external-reload");
    let mut replay = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
    let external = envelope(
        &session,
        2,
        replay.last_event_hash(),
        Event::ExternalFileChange(ExternalFileChange {
            path: path("src/lib.rs"),
            previous_contents: Some("abc".to_owned()),
            new_contents: Some("axc".to_owned()),
            previous_hash: Some(document_hash("abc")),
            new_hash: Some(document_hash("axc")),
        }),
    );
    replay.apply(&external).unwrap();
    assert_eq!(
        replay.workspace_state().file(&path("src/lib.rs")),
        Some(&b"axc"[..])
    );
    assert_eq!(
        replay
            .workspace_state()
            .document(&document_id("doc-lib"))
            .unwrap()
            .text(),
        "abc"
    );

    let invalid_checkpoint = envelope(
        &session,
        3,
        external.event_hash,
        Event::WorkspaceCheckpoint(rustrace_model::WorkspaceCheckpoint {
            workspace_hash: replay.current_workspace_hash(),
            documents: vec![rustrace_model::DocumentHash {
                document_id: document_id("doc-lib"),
                hash: document_hash("abc"),
            }],
        }),
    );
    assert!(matches!(
        replay.apply(&invalid_checkpoint),
        Err(ReplayError::CheckpointStateMismatch {
            field: "documents.path",
            ..
        })
    ));

    let mut reload = edit_transaction(
        "doc-lib",
        "abc",
        "axc",
        0,
        SelectionState::caret(0),
        SelectionState::caret(0),
        TextEdit {
            start_byte: 1,
            end_byte: 2,
            inserted_text: "x".to_owned(),
        },
    );
    reload.origin = EditOrigin::FileReload;
    let reload = envelope(&session, 3, external.event_hash, Event::FileEdited(reload));
    replay.apply(&reload).unwrap();
    let checkpoint = checkpoint(
        &session,
        4,
        vec![
            file("binary.dat", &[0xff, 0x00]),
            file("src/lib.rs", b"axc"),
        ],
        Some(document_id("doc-lib")),
        vec![document(
            "doc-lib",
            "src/lib.rs",
            1,
            SelectionState::caret(0),
        )],
        reload.event_hash,
    );
    replay.apply(&checkpoint.owning_event).unwrap();
}

#[test]
fn no_op_transactions_are_rejected_without_advancing() {
    let session = session_id("no-op");
    let mut replay = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
    let transaction = edit_transaction(
        "doc-lib",
        "abc",
        "abc",
        0,
        SelectionState::caret(0),
        SelectionState::caret(0),
        TextEdit {
            start_byte: 0,
            end_byte: 0,
            inserted_text: String::new(),
        },
    );
    let event = envelope(
        &session,
        2,
        replay.last_event_hash(),
        Event::FileEdited(transaction),
    );

    assert!(matches!(
        replay.apply(&event),
        Err(ReplayError::NoOpTransaction { .. })
    ));
    assert_eq!(replay.next_sequence(), 2);
    assert_eq!(
        replay.workspace_state().file(&path("src/lib.rs")),
        Some(&b"abc"[..])
    );
}

#[test]
fn terminal_events_reject_a_following_event() {
    let session = session_id("terminal");
    let mut replay = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
    let finalized = envelope(
        &session,
        2,
        replay.last_event_hash(),
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: replay.current_workspace_hash(),
            event_count: 2,
            clean: true,
            warnings: vec![],
        }),
    );
    replay.apply(&finalized).unwrap();
    let after = envelope(
        &session,
        3,
        finalized.event_hash,
        Event::FileFocused(FileFocused {
            document_id: document_id("doc-lib"),
        }),
    );

    assert!(matches!(
        replay.apply(&after),
        Err(ReplayError::EventAfterTerminal { sequence: 3 })
    ));
    assert_eq!(replay.next_sequence(), 3);
}

#[test]
fn non_mutating_events_are_explicitly_validated_and_never_executed() {
    let session = session_id("non-mutating");
    let mut replay = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
    let initial_state = replay.workspace_state().clone();
    let mut previous = replay.last_event_hash();
    let command_id = CommandId::new("cargo-check").unwrap();
    let events = vec![
        Event::ViewportChanged(ViewportChanged {
            document_id: document_id("doc-lib"),
            top_line: 0,
            horizontal_column: 0,
        }),
        Event::CargoCommandStarted(CommandStarted {
            command_id: command_id.clone(),
            program: "cargo".to_owned(),
            arguments: vec!["check".to_owned()],
            working_directory: WorkspaceDirectory::new(".").unwrap(),
        }),
        Event::CargoDiagnostic(Diagnostic {
            command_id: command_id.clone(),
            document_id: Some(document_id("doc-lib")),
            severity: DiagnosticSeverity::Warning,
            code: None,
            message: "warning".to_owned(),
            range: None,
        }),
        Event::CargoOutput(CommandOutput {
            command_id: command_id.clone(),
            stream: OutputStream::Stdout,
            output: "checked".to_owned(),
        }),
        Event::CargoCommandFinished(CommandFinished {
            command_id,
            exit_code: Some(0),
            success: true,
        }),
        Event::LspCompletionRequested(CompletionRequested {
            document_id: document_id("doc-lib"),
            document_version: 0,
            position_byte: 1,
        }),
        Event::LspCompletionAccepted(CompletionAccepted {
            document_id: document_id("doc-lib"),
            document_version: 0,
            label: "item".to_owned(),
            primary_edit: TextEdit {
                start_byte: 1,
                end_byte: 1,
                inserted_text: "x".to_owned(),
            },
            additional_edits: vec![],
        }),
    ];

    for (index, event) in events.into_iter().enumerate() {
        apply_event(
            &mut replay,
            &session,
            index as u64 + 2,
            &mut previous,
            event,
        );
    }

    assert_eq!(replay.workspace_state(), &initial_state);
}

#[test]
fn prospective_lsp_ranges_and_versions_are_checked() {
    let session = session_id("lsp-validation");
    let invalid = [
        Event::LspCompletionRequested(CompletionRequested {
            document_id: document_id("doc-lib"),
            document_version: 7,
            position_byte: 0,
        }),
        Event::LspCompletionAccepted(CompletionAccepted {
            document_id: document_id("doc-lib"),
            document_version: 0,
            label: "bad".to_owned(),
            primary_edit: TextEdit {
                start_byte: 9,
                end_byte: 9,
                inserted_text: String::new(),
            },
            additional_edits: vec![],
        }),
    ];

    for event in invalid {
        let mut replay =
            ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
        let event = envelope(&session, 2, replay.last_event_hash(), event);
        assert!(matches!(
            replay.apply(&event),
            Err(ReplayError::NonMutatingEvent { .. })
        ));
    }
}

#[test]
fn workspace_hash_matches_the_authoritative_pure_hasher() {
    let session = session_id("hash-api");
    let replay = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
    let expected = hash_entries([
        (&path("binary.dat"), &[0xff, 0x00][..]),
        (&path("src/lib.rs"), &b"abc"[..]),
    ])
    .unwrap();

    assert_eq!(replay.current_workspace_hash(), expected);
}

#[test]
fn clean_finalization_rejects_changed_and_deleted_open_files_atomically() {
    for new_contents in [Some("external"), None] {
        let session = session_id(if new_contents.is_some() {
            "clean-divergent"
        } else {
            "clean-missing"
        });
        let mut replay =
            ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
        let external = envelope(
            &session,
            2,
            replay.last_event_hash(),
            Event::ExternalFileChange(ExternalFileChange {
                path: path("src/lib.rs"),
                previous_contents: Some("abc".to_owned()),
                new_contents: new_contents.map(str::to_owned),
                previous_hash: Some(document_hash("abc")),
                new_hash: new_contents.map(document_hash),
            }),
        );
        replay.apply(&external).unwrap();
        let before_state = replay.workspace_state().clone();
        let before_sequence = replay.next_sequence();
        let before_hash = replay.last_event_hash();
        let finalized = envelope(
            &session,
            3,
            before_hash,
            Event::SubmissionFinalized(SubmissionFinalized {
                final_workspace_hash: replay.current_workspace_hash(),
                event_count: 3,
                clean: true,
                warnings: vec![],
            }),
        );

        assert!(matches!(
            replay.apply(&finalized),
            Err(ReplayError::WorkspaceCoherence { .. })
        ));
        assert_eq!(replay.workspace_state(), &before_state);
        assert_eq!(replay.next_sequence(), before_sequence);
        assert_eq!(replay.last_event_hash(), before_hash);
        assert!(!replay.is_terminal());
    }
}

#[test]
fn clean_finalization_checks_non_active_documents_but_unclean_allows_recovery() {
    let session = session_id("clean-non-active");
    let initial = checkpoint(
        &session,
        1,
        vec![file("active.rs", b"same"), file("background.rs", b"same")],
        Some(document_id("active")),
        vec![
            document("active", "active.rs", 0, SelectionState::caret(0)),
            document("background", "background.rs", 0, SelectionState::caret(0)),
        ],
        Hash::zero(),
    );
    let mut replay = ReplayEngine::from_initial_checkpoint(initial).unwrap();
    let external = envelope(
        &session,
        2,
        replay.last_event_hash(),
        Event::ExternalFileChange(ExternalFileChange {
            path: path("background.rs"),
            previous_contents: Some("same".to_owned()),
            new_contents: Some("changed".to_owned()),
            previous_hash: Some(document_hash("same")),
            new_hash: Some(document_hash("changed")),
        }),
    );
    replay.apply(&external).unwrap();
    let clean = envelope(
        &session,
        3,
        external.event_hash,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: replay.current_workspace_hash(),
            event_count: 3,
            clean: true,
            warnings: vec![],
        }),
    );
    assert!(matches!(
        replay.apply(&clean),
        Err(ReplayError::WorkspaceCoherence { .. })
    ));
    assert_eq!(replay.next_sequence(), 3);

    let wrong_count = envelope(
        &session,
        3,
        external.event_hash,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: replay.current_workspace_hash(),
            event_count: 4,
            clean: false,
            warnings: vec![],
        }),
    );
    assert!(matches!(
        replay.apply(&wrong_count),
        Err(ReplayError::NonMutatingEvent {
            event: "submission_finalized",
            ..
        })
    ));
    assert_eq!(replay.next_sequence(), 3);

    let unclean = envelope(
        &session,
        3,
        external.event_hash,
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: replay.current_workspace_hash(),
            event_count: 3,
            clean: false,
            warnings: vec!["external change unresolved".to_owned()],
        }),
    );
    replay.apply(&unclean).unwrap();
    assert!(replay.is_finalized());
}

#[test]
fn ordinary_edits_reject_divergent_and_deleted_raw_files_atomically() {
    let ordinary_origins = [
        EditOrigin::Keyboard,
        EditOrigin::Paste,
        EditOrigin::Undo,
        EditOrigin::Redo,
        EditOrigin::Completion,
        EditOrigin::AdditionalCompletionEdit,
        EditOrigin::CodeAction,
        EditOrigin::ExternalChange,
        EditOrigin::Unknown,
    ];
    for new_contents in [Some("external"), None] {
        for origin in ordinary_origins {
            let session = session_id("ordinary-divergence");
            let mut replay =
                ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
            let external = envelope(
                &session,
                2,
                replay.last_event_hash(),
                Event::ExternalFileChange(ExternalFileChange {
                    path: path("src/lib.rs"),
                    previous_contents: Some("abc".to_owned()),
                    new_contents: new_contents.map(str::to_owned),
                    previous_hash: Some(document_hash("abc")),
                    new_hash: new_contents.map(document_hash),
                }),
            );
            replay.apply(&external).unwrap();
            let before_state = replay.workspace_state().clone();
            let before_hash = replay.last_event_hash();
            let mut transaction = edit_transaction(
                "doc-lib",
                "abc",
                "abcd",
                0,
                SelectionState::caret(0),
                SelectionState::caret(4),
                TextEdit {
                    start_byte: 3,
                    end_byte: 3,
                    inserted_text: "d".to_owned(),
                },
            );
            transaction.origin = origin;
            let edit = envelope(&session, 3, before_hash, Event::FileEdited(transaction));

            assert!(
                matches!(
                    replay.apply(&edit),
                    Err(ReplayError::WorkspaceCoherence { .. })
                ),
                "origin {origin:?}"
            );
            assert_eq!(replay.workspace_state(), &before_state, "origin {origin:?}");
            assert_eq!(replay.next_sequence(), 3, "origin {origin:?}");
            assert_eq!(replay.last_event_hash(), before_hash, "origin {origin:?}");
        }
    }
}

#[test]
fn file_reload_must_exactly_reproduce_existing_raw_bytes() {
    let origin = EditOrigin::FileReload;
    let session = session_id("explicit-reload");
    let mut replay = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
    let external = envelope(
        &session,
        2,
        replay.last_event_hash(),
        Event::ExternalFileChange(ExternalFileChange {
            path: path("src/lib.rs"),
            previous_contents: Some("abc".to_owned()),
            new_contents: Some("axc".to_owned()),
            previous_hash: Some(document_hash("abc")),
            new_hash: Some(document_hash("axc")),
        }),
    );
    replay.apply(&external).unwrap();
    let before_state = replay.workspace_state().clone();
    let before_hash = replay.last_event_hash();
    let raw_workspace_hash = replay.current_workspace_hash();
    let mut wrong = edit_transaction(
        "doc-lib",
        "abc",
        "ayc",
        0,
        SelectionState::caret(0),
        SelectionState::caret(0),
        TextEdit {
            start_byte: 1,
            end_byte: 2,
            inserted_text: "y".to_owned(),
        },
    );
    wrong.origin = origin;
    let wrong = envelope(&session, 3, external.event_hash, Event::FileEdited(wrong));
    assert!(matches!(
        replay.apply(&wrong),
        Err(ReplayError::WorkspaceCoherence { .. })
    ));
    assert_eq!(replay.workspace_state(), &before_state);
    assert_eq!(replay.next_sequence(), 3);
    assert_eq!(replay.last_event_hash(), before_hash);
    assert!(!replay.is_terminal());

    let mut reload = edit_transaction(
        "doc-lib",
        "abc",
        "axc",
        0,
        SelectionState::caret(0),
        SelectionState::caret(0),
        TextEdit {
            start_byte: 1,
            end_byte: 2,
            inserted_text: "x".to_owned(),
        },
    );
    reload.origin = origin;
    let reload = envelope(&session, 3, external.event_hash, Event::FileEdited(reload));
    replay.apply(&reload).unwrap();
    assert_eq!(replay.current_workspace_hash(), raw_workspace_hash);
    assert_eq!(
        replay.workspace_state().file(&path("src/lib.rs")),
        Some(&b"axc"[..])
    );
    assert_eq!(
        replay
            .workspace_state()
            .document(&document_id("doc-lib"))
            .unwrap()
            .text(),
        "axc"
    );
    let document = replay
        .workspace_state()
        .document(&document_id("doc-lib"))
        .unwrap();
    assert_eq!(document.version(), 1);
    assert_eq!(document.selection(), SelectionState::caret(0));
}

#[test]
fn file_reload_reconciles_an_inactive_document_without_changing_focus() {
    let session = session_id("inactive-reload");
    let initial = checkpoint(
        &session,
        1,
        vec![file("active.rs", b"abc"), file("inactive.rs", b"abc")],
        Some(document_id("active")),
        vec![
            document("active", "active.rs", 0, SelectionState::caret(0)),
            document("inactive", "inactive.rs", 0, SelectionState::caret(0)),
        ],
        Hash::zero(),
    );
    let mut replay = ReplayEngine::from_initial_checkpoint(initial).unwrap();
    let external = envelope(
        &session,
        2,
        replay.last_event_hash(),
        Event::ExternalFileChange(ExternalFileChange {
            path: path("inactive.rs"),
            previous_contents: Some("abc".to_owned()),
            new_contents: Some("axc".to_owned()),
            previous_hash: Some(document_hash("abc")),
            new_hash: Some(document_hash("axc")),
        }),
    );
    replay.apply(&external).unwrap();
    let raw_workspace_hash = replay.current_workspace_hash();
    let mut reload = edit_transaction(
        "inactive",
        "abc",
        "axc",
        0,
        SelectionState::caret(0),
        SelectionState::caret(2),
        TextEdit {
            start_byte: 1,
            end_byte: 2,
            inserted_text: "x".to_owned(),
        },
    );
    reload.origin = EditOrigin::FileReload;
    let reload = envelope(&session, 3, external.event_hash, Event::FileEdited(reload));
    replay.apply(&reload).unwrap();

    assert_eq!(replay.current_workspace_hash(), raw_workspace_hash);
    assert_eq!(
        replay.workspace_state().active_document(),
        Some(&document_id("active"))
    );
    let inactive = replay
        .workspace_state()
        .document(&document_id("inactive"))
        .unwrap();
    assert_eq!(inactive.text(), "axc");
    assert_eq!(inactive.version(), 1);
    assert_eq!(inactive.selection(), SelectionState::caret(2));
}

#[test]
fn external_recreation_with_matching_bytes_restores_edit_coherence() {
    let session = session_id("external-recreate");
    let mut replay = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
    let deleted = envelope(
        &session,
        2,
        replay.last_event_hash(),
        Event::ExternalFileChange(ExternalFileChange {
            path: path("src/lib.rs"),
            previous_contents: Some("abc".to_owned()),
            new_contents: None,
            previous_hash: Some(document_hash("abc")),
            new_hash: None,
        }),
    );
    replay.apply(&deleted).unwrap();
    let recreated = envelope(
        &session,
        3,
        deleted.event_hash,
        Event::ExternalFileChange(ExternalFileChange {
            path: path("src/lib.rs"),
            previous_contents: None,
            new_contents: Some("abc".to_owned()),
            previous_hash: None,
            new_hash: Some(document_hash("abc")),
        }),
    );
    replay.apply(&recreated).unwrap();
    let edit = envelope(
        &session,
        4,
        recreated.event_hash,
        Event::FileEdited(edit_transaction(
            "doc-lib",
            "abc",
            "abcd",
            0,
            SelectionState::caret(0),
            SelectionState::caret(4),
            TextEdit {
                start_byte: 3,
                end_byte: 3,
                inserted_text: "d".to_owned(),
            },
        )),
    );
    replay.apply(&edit).unwrap();

    assert_eq!(
        replay.workspace_state().file(&path("src/lib.rs")),
        Some(&b"abcd"[..])
    );
}

#[test]
fn reload_cannot_resolve_a_missing_raw_file() {
    let session = session_id("reload-missing");
    let mut replay = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
    let external = envelope(
        &session,
        2,
        replay.last_event_hash(),
        Event::ExternalFileChange(ExternalFileChange {
            path: path("src/lib.rs"),
            previous_contents: Some("abc".to_owned()),
            new_contents: None,
            previous_hash: Some(document_hash("abc")),
            new_hash: None,
        }),
    );
    replay.apply(&external).unwrap();
    let before_state = replay.workspace_state().clone();
    let before_hash = replay.last_event_hash();
    let mut reload = edit_transaction(
        "doc-lib",
        "abc",
        "axc",
        0,
        SelectionState::caret(0),
        SelectionState::caret(0),
        TextEdit {
            start_byte: 1,
            end_byte: 2,
            inserted_text: "x".to_owned(),
        },
    );
    reload.origin = EditOrigin::FileReload;
    let reload = envelope(&session, 3, external.event_hash, Event::FileEdited(reload));

    assert!(matches!(
        replay.apply(&reload),
        Err(ReplayError::WorkspaceCoherence { .. })
    ));
    assert_eq!(replay.workspace_state(), &before_state);
    assert_eq!(replay.next_sequence(), 3);
    assert_eq!(replay.last_event_hash(), before_hash);
    assert!(!replay.is_terminal());
    assert!(replay.workspace_state().file(&path("src/lib.rs")).is_none());
}

fn cargo_start(id: &CommandId) -> Event {
    Event::CargoCommandStarted(CommandStarted {
        command_id: id.clone(),
        program: "cargo".to_owned(),
        arguments: vec!["check".to_owned()],
        working_directory: WorkspaceDirectory::new(".").unwrap(),
    })
}

#[test]
fn cargo_lifecycle_rejects_duplicate_and_unknown_events_atomically() {
    let session = session_id("cargo-lifecycle");
    let command = CommandId::new("command").unwrap();
    let unknown = CommandId::new("unknown").unwrap();
    let mut replay = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
    let started = envelope(&session, 2, replay.last_event_hash(), cargo_start(&command));
    replay.apply(&started).unwrap();

    let invalid = [
        (cargo_start(&command), true),
        (
            Event::CargoDiagnostic(Diagnostic {
                command_id: unknown.clone(),
                document_id: None,
                severity: DiagnosticSeverity::Error,
                code: None,
                message: "unknown".to_owned(),
                range: None,
            }),
            false,
        ),
        (
            Event::CargoOutput(CommandOutput {
                command_id: unknown.clone(),
                stream: OutputStream::Stderr,
                output: "unknown".to_owned(),
            }),
            false,
        ),
        (
            Event::CargoCommandFinished(CommandFinished {
                command_id: unknown,
                exit_code: None,
                success: false,
            }),
            false,
        ),
    ];
    for (event, duplicate_start) in invalid {
        let before_state = replay.workspace_state().clone();
        let before_hash = replay.last_event_hash();
        let rejected = envelope(&session, 3, before_hash, event);
        let error = replay.apply(&rejected).unwrap_err();
        if duplicate_start {
            assert!(matches!(
                error,
                ReplayError::CargoCommandAlreadyActive { .. }
            ));
        } else {
            assert!(matches!(error, ReplayError::CargoCommandNotActive { .. }));
        }
        assert_eq!(replay.workspace_state(), &before_state);
        assert_eq!(replay.next_sequence(), 3);
        assert_eq!(replay.last_event_hash(), before_hash);
    }

    let finished = envelope(
        &session,
        3,
        started.event_hash,
        Event::CargoCommandFinished(CommandFinished {
            command_id: command.clone(),
            exit_code: Some(0),
            success: true,
        }),
    );
    replay.apply(&finished).unwrap();
    let after_finish = [
        Event::CargoDiagnostic(Diagnostic {
            command_id: command.clone(),
            document_id: None,
            severity: DiagnosticSeverity::Warning,
            code: None,
            message: "late".to_owned(),
            range: None,
        }),
        Event::CargoOutput(CommandOutput {
            command_id: command.clone(),
            stream: OutputStream::Stdout,
            output: "late".to_owned(),
        }),
        Event::CargoCommandFinished(CommandFinished {
            command_id: command.clone(),
            exit_code: Some(0),
            success: true,
        }),
    ];
    for event in after_finish {
        let rejected = envelope(&session, 4, finished.event_hash, event);
        assert!(matches!(
            replay.apply(&rejected),
            Err(ReplayError::CargoCommandNotActive { .. })
        ));
        assert_eq!(replay.next_sequence(), 4);
        assert_eq!(replay.last_event_hash(), finished.event_hash);
    }
    let restarted = envelope(&session, 4, finished.event_hash, cargo_start(&command));
    replay.apply(&restarted).unwrap();
    assert_eq!(replay.next_sequence(), 5);
}

#[test]
fn cargo_in_flight_commands_are_bounded_before_insertion() {
    let session = session_id("cargo-bound");
    let mut replay = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
    let mut previous = replay.last_event_hash();
    for index in 0..MAX_ACTIVE_CARGO_COMMANDS {
        let command = CommandId::new(format!("command-{index:04}")).unwrap();
        apply_event(
            &mut replay,
            &session,
            index as u64 + 2,
            &mut previous,
            cargo_start(&command),
        );
    }
    let overflow = envelope(
        &session,
        MAX_ACTIVE_CARGO_COMMANDS as u64 + 2,
        previous,
        cargo_start(&CommandId::new("overflow").unwrap()),
    );

    assert!(matches!(
        replay.apply(&overflow),
        Err(ReplayError::ActiveCargoCommandLimit {
            attempted,
            maximum,
        }) if attempted == MAX_ACTIVE_CARGO_COMMANDS + 1
            && maximum == MAX_ACTIVE_CARGO_COMMANDS
    ));
    assert_eq!(replay.next_sequence(), MAX_ACTIVE_CARGO_COMMANDS as u64 + 2);
    assert_eq!(replay.last_event_hash(), previous);

    let overflow_finish = envelope(
        &session,
        MAX_ACTIVE_CARGO_COMMANDS as u64 + 2,
        previous,
        Event::CargoCommandFinished(CommandFinished {
            command_id: CommandId::new("overflow").unwrap(),
            exit_code: Some(0),
            success: true,
        }),
    );
    assert!(matches!(
        replay.apply(&overflow_finish),
        Err(ReplayError::CargoCommandNotActive { .. })
    ));
    let first = CommandId::new("command-0000").unwrap();
    let finished = envelope(
        &session,
        MAX_ACTIVE_CARGO_COMMANDS as u64 + 2,
        previous,
        Event::CargoCommandFinished(CommandFinished {
            command_id: first,
            exit_code: Some(0),
            success: true,
        }),
    );
    replay.apply(&finished).unwrap();
    let replacement = envelope(
        &session,
        MAX_ACTIVE_CARGO_COMMANDS as u64 + 3,
        finished.event_hash,
        cargo_start(&CommandId::new("replacement").unwrap()),
    );
    replay.apply(&replacement).unwrap();
}

#[test]
fn terminal_events_reject_active_cargo_commands_atomically() {
    for finalize_submission in [false, true] {
        let session = session_id(if finalize_submission {
            "cargo-submit-terminal"
        } else {
            "cargo-session-terminal"
        });
        let command = CommandId::new("active-at-terminal").unwrap();
        let mut replay =
            ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
        let started = envelope(&session, 2, replay.last_event_hash(), cargo_start(&command));
        replay.apply(&started).unwrap();
        let terminal_event = if finalize_submission {
            Event::SubmissionFinalized(SubmissionFinalized {
                final_workspace_hash: replay.current_workspace_hash(),
                event_count: 3,
                clean: false,
                warnings: vec!["recovery".to_owned()],
            })
        } else {
            Event::SessionEnded(SessionEnded {
                final_workspace_hash: replay.current_workspace_hash(),
            })
        };
        let terminal = envelope(&session, 3, started.event_hash, terminal_event);
        let before_state = replay.workspace_state().clone();

        assert!(matches!(
            replay.apply(&terminal),
            Err(ReplayError::ActiveCargoCommandsAtTerminal { count: 1, .. })
        ));
        assert_eq!(replay.workspace_state(), &before_state);
        assert_eq!(replay.next_sequence(), 3);
        assert_eq!(replay.last_event_hash(), started.event_hash);
        assert!(!replay.is_terminal());

        let finished = envelope(
            &session,
            3,
            started.event_hash,
            Event::CargoCommandFinished(CommandFinished {
                command_id: command,
                exit_code: Some(0),
                success: true,
            }),
        );
        replay.apply(&finished).unwrap();
        let terminal_event = if finalize_submission {
            Event::SubmissionFinalized(SubmissionFinalized {
                final_workspace_hash: replay.current_workspace_hash(),
                event_count: 4,
                clean: false,
                warnings: vec!["recovery".to_owned()],
            })
        } else {
            Event::SessionEnded(SessionEnded {
                final_workspace_hash: replay.current_workspace_hash(),
            })
        };
        let terminal = envelope(&session, 4, finished.event_hash, terminal_event);
        replay.apply(&terminal).unwrap();
        assert!(replay.is_terminal());
    }
}

#[test]
fn arbitrary_later_checkpoint_cannot_be_used_as_an_initial_anchor() {
    let session = session_id("arbitrary-anchor");
    let arbitrary = checkpoint(
        &session,
        7,
        vec![file("src/lib.rs", b"abcd")],
        Some(document_id("doc")),
        vec![document("doc", "src/lib.rs", 4, SelectionState::caret(4))],
        Hash::from_bytes([7; 32]),
    );

    assert!(matches!(
        ReplayEngine::from_initial_checkpoint(arbitrary),
        Err(ReplayError::InitialCheckpointNotGenesis { sequence: 7 })
    ));

    let nonzero_genesis = checkpoint(
        &session,
        1,
        vec![file("src/lib.rs", b"abc")],
        None,
        vec![],
        Hash::from_bytes([1; 32]),
    );
    assert!(matches!(
        ReplayEngine::from_initial_checkpoint(nonzero_genesis),
        Err(ReplayError::PreviousEventHashMismatch { sequence: 1, .. })
    ));
}

#[test]
fn checkpoint_certification_requires_the_exact_reached_full_state() {
    let session = session_id("exact-certificate");
    let files = || vec![file("one.rs", b"same"), file("two.rs", b"same")];
    let initial = checkpoint(
        &session,
        1,
        files(),
        Some(document_id("one")),
        vec![
            document("one", "one.rs", 0, SelectionState::caret(0)),
            document("two", "two.rs", 0, SelectionState::caret(0)),
        ],
        Hash::zero(),
    );
    let mut replay = ReplayEngine::from_initial_checkpoint(initial).unwrap();
    let exact = checkpoint(
        &session,
        2,
        files(),
        Some(document_id("one")),
        vec![
            document("one", "one.rs", 0, SelectionState::caret(0)),
            document("two", "two.rs", 0, SelectionState::caret(0)),
        ],
        replay.last_event_hash(),
    );

    assert!(matches!(
        replay.certify_checkpoint(exact.clone()),
        Err(ReplayError::CheckpointCertification { .. })
    ));
    assert_eq!(replay.next_sequence(), 2);
    replay.apply(&exact.owning_event).unwrap();
    let before_state = replay.workspace_state().clone();
    let before_sequence = replay.next_sequence();
    let before_hash = replay.last_event_hash();
    replay.validate_checkpoint(&exact).unwrap();
    assert_eq!(replay.workspace_state(), &before_state);
    assert_eq!(replay.next_sequence(), before_sequence);
    assert_eq!(replay.last_event_hash(), before_hash);
    let variants = [
        checkpoint(
            &session,
            2,
            files(),
            Some(document_id("two")),
            vec![
                document("one", "one.rs", 0, SelectionState::caret(0)),
                document("two", "two.rs", 0, SelectionState::caret(0)),
            ],
            exact.owning_event.previous_event_hash,
        ),
        checkpoint(
            &session,
            2,
            files(),
            Some(document_id("one")),
            vec![
                document("one", "one.rs", 7, SelectionState::caret(0)),
                document("two", "two.rs", 0, SelectionState::caret(0)),
            ],
            exact.owning_event.previous_event_hash,
        ),
        checkpoint(
            &session,
            2,
            files(),
            Some(document_id("one")),
            vec![
                document("one", "one.rs", 0, SelectionState::caret(2)),
                document("two", "two.rs", 0, SelectionState::caret(0)),
            ],
            exact.owning_event.previous_event_hash,
        ),
        checkpoint(
            &session,
            2,
            files(),
            Some(document_id("one")),
            vec![
                document("one", "two.rs", 0, SelectionState::caret(0)),
                document("two", "one.rs", 0, SelectionState::caret(0)),
            ],
            exact.owning_event.previous_event_hash,
        ),
    ];

    for variant in variants {
        assert_eq!(variant.owning_event, exact.owning_event);
        assert!(matches!(
            replay.validate_checkpoint(&variant),
            Err(ReplayError::CheckpointCertification { .. })
        ));
        assert!(matches!(
            replay.certify_checkpoint(variant),
            Err(ReplayError::CheckpointCertification { .. })
        ));
        assert_eq!(replay.workspace_state(), &before_state);
        assert_eq!(replay.next_sequence(), before_sequence);
        assert_eq!(replay.last_event_hash(), before_hash);
    }

    let other_session = session_id("other-certificate-session");
    let wrong_session = checkpoint(
        &other_session,
        2,
        files(),
        Some(document_id("one")),
        vec![
            document("one", "one.rs", 0, SelectionState::caret(0)),
            document("two", "two.rs", 0, SelectionState::caret(0)),
        ],
        exact.owning_event.previous_event_hash,
    );
    assert!(matches!(
        replay.certify_checkpoint(wrong_session),
        Err(ReplayError::WrongSession { .. })
    ));
    let wrong_sequence = checkpoint(
        &session,
        3,
        files(),
        Some(document_id("one")),
        vec![
            document("one", "one.rs", 0, SelectionState::caret(0)),
            document("two", "two.rs", 0, SelectionState::caret(0)),
        ],
        exact.owning_event.event_hash,
    );
    assert!(matches!(
        replay.certify_checkpoint(wrong_sequence),
        Err(ReplayError::CheckpointCertification { .. })
    ));
    let wrong_chain = checkpoint(
        &session,
        2,
        files(),
        Some(document_id("one")),
        vec![
            document("one", "one.rs", 0, SelectionState::caret(0)),
            document("two", "two.rs", 0, SelectionState::caret(0)),
        ],
        Hash::from_bytes([9; 32]),
    );
    assert!(matches!(
        replay.certify_checkpoint(wrong_chain),
        Err(ReplayError::CheckpointCertification { .. })
    ));
    assert_eq!(replay.workspace_state(), &before_state);
    assert_eq!(replay.next_sequence(), before_sequence);
    assert_eq!(replay.last_event_hash(), before_hash);

    let certificate = replay.certify_checkpoint(exact).unwrap();
    let seek = ReplayEngine::from_checkpoint(certificate);
    assert_eq!(seek.workspace_state(), replay.workspace_state());
    assert_eq!(seek.next_sequence(), replay.next_sequence());
    assert_eq!(seek.last_event_hash(), replay.last_event_hash());
}

#[test]
fn checkpoint_certification_is_rejected_after_a_terminal_event() {
    let session = session_id("terminal-certificate");
    let mut replay = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
    let later = checkpoint(
        &session,
        2,
        vec![
            file("binary.dat", &[0xff, 0x00]),
            file("src/lib.rs", b"abc"),
        ],
        Some(document_id("doc-lib")),
        vec![document(
            "doc-lib",
            "src/lib.rs",
            0,
            SelectionState::caret(0),
        )],
        replay.last_event_hash(),
    );
    replay.apply(&later.owning_event).unwrap();
    let finalized = envelope(
        &session,
        3,
        replay.last_event_hash(),
        Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: replay.current_workspace_hash(),
            event_count: 3,
            clean: true,
            warnings: vec![],
        }),
    );
    replay.apply(&finalized).unwrap();
    let before_state = replay.workspace_state().clone();
    let before_sequence = replay.next_sequence();
    let before_hash = replay.last_event_hash();

    assert!(matches!(
        replay.certify_checkpoint(later),
        Err(ReplayError::CheckpointCertification { .. })
    ));
    assert_eq!(replay.workspace_state(), &before_state);
    assert_eq!(replay.next_sequence(), before_sequence);
    assert_eq!(replay.last_event_hash(), before_hash);
    assert!(replay.is_finalized());
}

#[test]
fn certified_seek_preserves_in_flight_cargo_lifecycle() {
    let session = session_id("cargo-certificate");
    let command = CommandId::new("in-flight").unwrap();
    let mut full = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&session)).unwrap();
    let started = envelope(&session, 2, full.last_event_hash(), cargo_start(&command));
    full.apply(&started).unwrap();
    let later = checkpoint(
        &session,
        3,
        vec![
            file("binary.dat", &[0xff, 0x00]),
            file("src/lib.rs", b"abc"),
        ],
        Some(document_id("doc-lib")),
        vec![document(
            "doc-lib",
            "src/lib.rs",
            0,
            SelectionState::caret(0),
        )],
        started.event_hash,
    );
    full.apply(&later.owning_event).unwrap();
    let certificate = full.certify_checkpoint(later).unwrap();
    let output = envelope(
        &session,
        4,
        full.last_event_hash(),
        Event::CargoOutput(CommandOutput {
            command_id: command.clone(),
            stream: OutputStream::Stdout,
            output: "still running".to_owned(),
        }),
    );
    full.apply(&output).unwrap();
    let finished = envelope(
        &session,
        5,
        output.event_hash,
        Event::CargoCommandFinished(CommandFinished {
            command_id: command,
            exit_code: Some(0),
            success: true,
        }),
    );
    full.apply(&finished).unwrap();

    let mut seek = ReplayEngine::from_checkpoint(certificate);
    seek.apply(&output).unwrap();
    seek.apply(&finished).unwrap();
    assert_eq!(seek.workspace_state(), full.workspace_state());
    assert_eq!(seek.next_sequence(), full.next_sequence());
    assert_eq!(seek.last_event_hash(), full.last_event_hash());
}

#[test]
fn observation_preserves_dirty_logical_bytes_and_rejects_false_logical_link() {
    let id = session_id("observation");
    let initial = initial_checkpoint(&id);
    let mut replay = ReplayEngine::from_initial_checkpoint(initial.clone()).unwrap();
    let observation = rustrace_model::ExternalObservation {
        path: path("src/lib.rs"),
        saved_hash: Some(rustrace_model::observation_hash(b"saved A")),
        logical_hash: Some(rustrace_model::observation_hash(b"abc")),
        observed_hash: Some(rustrace_model::observation_hash(b"observed C")),
        evidence_hash: Hash::from_bytes([7; 32]),
    };
    let mut wrong = observation.clone();
    wrong.logical_hash = wrong.saved_hash;
    assert!(
        replay
            .apply(&envelope(
                &id,
                2,
                initial.owning_event.event_hash,
                Event::ExternalObservation(wrong)
            ))
            .is_err()
    );
    replay
        .apply(&envelope(
            &id,
            2,
            initial.owning_event.event_hash,
            Event::ExternalObservation(observation),
        ))
        .unwrap();
    assert_eq!(
        replay.workspace_state().file(&path("src/lib.rs")),
        Some(b"abc".as_slice())
    );
}

#[test]
fn observation_creation_deletion_and_own_save_do_not_mutate_replay() {
    use rustrace_model::{ExternalObservation, observation_hash};
    let id = session_id("observation-presence");
    let mut replay = ReplayEngine::from_initial_checkpoint(initial_checkpoint(&id)).unwrap();
    let original = replay.workspace_state().clone();
    for observation in [
        ExternalObservation {
            path: path("created.rs"),
            saved_hash: None,
            logical_hash: None,
            observed_hash: Some(observation_hash(b"new")),
            evidence_hash: Hash::from_bytes([1; 32]),
        },
        ExternalObservation {
            path: path("src/lib.rs"),
            saved_hash: Some(observation_hash(b"abc")),
            logical_hash: Some(observation_hash(b"abc")),
            observed_hash: None,
            evidence_hash: Hash::from_bytes([2; 32]),
        },
        ExternalObservation {
            path: path("src/lib.rs"),
            saved_hash: Some(observation_hash(b"abc")),
            logical_hash: Some(observation_hash(b"abc")),
            observed_hash: Some(observation_hash(b"abc")),
            evidence_hash: Hash::from_bytes([3; 32]),
        },
    ] {
        let event = envelope(
            &id,
            replay.next_sequence(),
            replay.last_event_hash(),
            Event::ExternalObservation(observation),
        );
        let bytes = rustrace_model::encode_envelope(&event).unwrap();
        assert_eq!(
            rustrace_model::decode_envelope(
                &bytes,
                rustrace_model::DecodePolicy::RejectUnsupported
            )
            .unwrap(),
            rustrace_model::DecodeOutcome::Decoded(event.clone())
        );
        replay.apply(&event).unwrap();
        assert_eq!(replay.workspace_state(), &original);
    }
}
