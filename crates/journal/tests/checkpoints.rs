use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier, Mutex},
    thread,
};

use rusqlite::{Connection, params};
use rustrace_journal::{
    ChainMismatch, CheckpointCorruption, CheckpointError, CheckpointFile, CheckpointSnapshot,
    Corruption, Journal, JournalError, MAX_CHECKPOINT_EXPANDED_BYTES, MAX_CHECKPOINT_FILE_BYTES,
    MAX_CHECKPOINT_FILES, MAX_CHECKPOINT_TOTAL_FILE_BYTES, OpenDocument, StoredCheckpoint,
    decode_checkpoint, encode_checkpoint,
};
use rustrace_model::{
    DocumentHash, DocumentId, Event, EventEnvelope, FORMAT_VERSION_V1, FileFocused, Hash,
    SelectionState, SessionId, WorkspaceCheckpoint, WorkspacePath, encode_envelope,
};

struct TempDatabase {
    directory: PathBuf,
    path: PathBuf,
}

impl TempDatabase {
    fn new(name: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "rustrace-checkpoint-{name}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("journal.sqlite3");
        Self { directory, path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDatabase {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn session_id(value: &str) -> SessionId {
    SessionId::new(value).unwrap()
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

fn document(id: &str, path_value: &str, selection: SelectionState) -> OpenDocument {
    OpenDocument {
        document_id: DocumentId::new(id).unwrap(),
        path: path(path_value),
        selection,
        version: 7,
    }
}

fn snapshot(id: &SessionId, sequence: u64, marker: &str) -> CheckpointSnapshot {
    CheckpointSnapshot::new(
        id.clone(),
        sequence,
        vec![
            file(
                "src/lib.rs",
                format!("pub fn value() {{ {marker} }}\n").as_bytes(),
            ),
            file("Cargo.toml", b"[package]\nname = \"demo\"\n[workspace]\n"),
        ],
        Some(DocumentId::new("doc-lib").unwrap()),
        vec![document("doc-lib", "src/lib.rs", SelectionState::caret(4))],
    )
    .unwrap()
}

fn create_journal(name: &str, id: &SessionId) -> (TempDatabase, Journal) {
    let temp = TempDatabase::new(name);
    let mut journal = Journal::create(temp.path()).unwrap();
    journal.create_or_resume_session(id).unwrap();
    (temp, journal)
}

fn focused_event(
    id: &SessionId,
    sequence: u64,
    previous: Hash,
    document_id: &str,
) -> EventEnvelope {
    EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: id.clone(),
        sequence,
        monotonic_millis: sequence * 10,
        wall_clock_utc: None,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event: Event::FileFocused(FileFocused {
            document_id: DocumentId::new(document_id).unwrap(),
        }),
    }
    .seal(previous)
    .unwrap()
}

#[test]
fn checkpoint_encoding_is_deterministic_and_round_trips_canonically() {
    let id = session_id("codec");
    let marker = "a".repeat(4096);
    let forward = snapshot(&id, 1, &marker);
    let reverse = CheckpointSnapshot::new(
        id,
        1,
        forward.files().iter().cloned().rev().collect(),
        forward.active_document().cloned(),
        vec![document("doc-lib", "src/lib.rs", SelectionState::caret(4))],
    )
    .unwrap();

    let first = encode_checkpoint(&forward).unwrap();
    let second = encode_checkpoint(&forward).unwrap();
    assert_eq!(first, second);
    assert_eq!(first, encode_checkpoint(&reverse).unwrap());
    assert!(first.len() < forward.total_file_bytes());
    assert_eq!(decode_checkpoint(&first).unwrap(), forward);
}

#[test]
fn checkpoint_constructor_enforces_exact_workspace_limits_and_one_over() {
    let id = session_id("limits");
    let one_mebibyte = vec![b'x'; MAX_CHECKPOINT_FILE_BYTES];
    assert!(
        CheckpointSnapshot::new(
            id.clone(),
            1,
            vec![file("max.bin", &one_mebibyte)],
            None,
            vec![]
        )
        .is_ok()
    );
    let oversized = vec![b'x'; MAX_CHECKPOINT_FILE_BYTES + 1];
    assert!(matches!(
        CheckpointSnapshot::new(
            id.clone(),
            1,
            vec![file("too-large.bin", &oversized)],
            None,
            vec![]
        ),
        Err(CheckpointError::FileTooLarge { .. })
    ));

    let files: Vec<_> = (0..MAX_CHECKPOINT_FILES)
        .map(|index| file(&format!("{index:03}.rs"), b""))
        .collect();
    assert!(CheckpointSnapshot::new(id.clone(), 1, files.clone(), None, vec![]).is_ok());
    let mut too_many = files;
    too_many.push(file("overflow.rs", b""));
    assert!(matches!(
        CheckpointSnapshot::new(id.clone(), 1, too_many, None, vec![]),
        Err(CheckpointError::TooManyFiles { .. })
    ));

    let chunk = vec![0_u8; MAX_CHECKPOINT_FILE_BYTES];
    let exact: Vec<_> = (0..(MAX_CHECKPOINT_TOTAL_FILE_BYTES / MAX_CHECKPOINT_FILE_BYTES))
        .map(|index| file(&format!("total-{index}.bin"), &chunk))
        .collect();
    assert!(CheckpointSnapshot::new(id.clone(), 1, exact.clone(), None, vec![]).is_ok());
    let mut over_total = exact;
    over_total.push(file("one-over.bin", b"x"));
    assert!(matches!(
        CheckpointSnapshot::new(id, 1, over_total, None, vec![]),
        Err(CheckpointError::WorkspaceTooLarge { .. })
    ));
}

#[test]
fn checkpoint_rejects_invalid_document_relationships_and_selections() {
    let id = session_id("invalid-documents");
    assert!(matches!(
        CheckpointSnapshot::new(
            id.clone(),
            1,
            vec![file("src/lib.rs", b"abc")],
            None,
            vec![document("missing", "missing.rs", SelectionState::caret(0))],
        ),
        Err(CheckpointError::DocumentFileMissing { .. })
    ));
    assert!(matches!(
        CheckpointSnapshot::new(
            id.clone(),
            1,
            vec![file("src/lib.rs", "🦀".as_bytes())],
            None,
            vec![document("doc", "src/lib.rs", SelectionState::caret(1))],
        ),
        Err(CheckpointError::SelectionNotCharBoundary { .. })
    ));
    assert!(matches!(
        CheckpointSnapshot::new(
            id.clone(),
            1,
            vec![file("binary", &[0xff])],
            None,
            vec![document("doc", "binary", SelectionState::caret(0))],
        ),
        Err(CheckpointError::DocumentNotUtf8 { .. })
    ));
    assert!(matches!(
        CheckpointSnapshot::new(
            id,
            1,
            vec![file("src/lib.rs", b"abc")],
            Some(DocumentId::new("closed").unwrap()),
            vec![],
        ),
        Err(CheckpointError::ActiveDocumentNotOpen { .. })
    ));
}

#[test]
fn compressed_corruption_and_trailing_data_are_rejected() {
    let id = session_id("corrupt-codec");
    let mut encoded = encode_checkpoint(&snapshot(&id, 1, "corrupt")).unwrap();
    let middle = encoded.len() / 2;
    encoded[middle] ^= 1;
    assert!(matches!(
        decode_checkpoint(&encoded),
        Err(CheckpointError::IntegrityMismatch)
    ));

    let mut trailing = encode_checkpoint(&snapshot(&id, 1, "trailing")).unwrap();
    trailing.push(0);
    assert!(matches!(
        decode_checkpoint(&trailing),
        Err(CheckpointError::TrailingData { .. })
    ));
}

#[test]
fn declared_expansion_over_the_limit_is_rejected_before_decompression() {
    let id = session_id("bomb");
    let mut encoded = encode_checkpoint(&snapshot(&id, 1, "bomb")).unwrap();
    encoded[13..21].copy_from_slice(
        &(u64::try_from(MAX_CHECKPOINT_EXPANDED_BYTES).unwrap() + 1).to_be_bytes(),
    );
    let digest_start = encoded.len() - Hash::LENGTH;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rustrace.checkpoint.compressed.v1\0");
    hasher.update(&encoded[..digest_start]);
    let digest = *hasher.finalize().as_bytes();
    encoded[digest_start..].copy_from_slice(&digest);

    assert!(matches!(
        decode_checkpoint(&encoded),
        Err(CheckpointError::ExpandedTooLarge { .. })
    ));
}

#[test]
fn initial_and_later_checkpoints_load_and_seek_at_or_before_boundaries() {
    let id = session_id("seek");
    let (_temp, mut journal) = create_journal("seek", &id);
    let initial = snapshot(&id, 1, "initial");
    let initial_event = journal.append_checkpoint(&id, 10, None, &initial).unwrap();
    let middle = focused_event(&id, 2, initial_event.event_hash, "doc-lib");
    journal.append_event(&id, &middle).unwrap();
    let later = snapshot(&id, 3, "later");
    let later_event = journal.append_checkpoint(&id, 30, None, &later).unwrap();

    assert_eq!(
        journal.load_checkpoint(&id, 1).unwrap().unwrap().snapshot,
        initial
    );
    assert_eq!(journal.load_checkpoint(&id, 2).unwrap(), None);
    assert_eq!(
        journal.load_checkpoint(&id, 3).unwrap().unwrap().snapshot,
        later
    );
    assert_eq!(
        journal.latest_checkpoint_at_or_before(&id, 0).unwrap(),
        None
    );
    assert_eq!(
        journal
            .latest_checkpoint_at_or_before(&id, 1)
            .unwrap()
            .unwrap()
            .owning_event,
        initial_event
    );
    assert_eq!(
        journal
            .latest_checkpoint_at_or_before(&id, 2)
            .unwrap()
            .unwrap()
            .snapshot
            .event_sequence(),
        1
    );
    assert_eq!(
        journal
            .latest_checkpoint_at_or_before(&id, 3)
            .unwrap()
            .unwrap()
            .owning_event,
        later_event
    );

    let listed = journal.list_checkpoints(&id, 1, 8).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].snapshot.event_sequence(), 1);
    assert_eq!(listed[1].snapshot.event_sequence(), 3);
    assert_eq!(journal.read_events(&id, 2, 8).unwrap().len(), 2);
    assert_eq!(journal.verify_session_chain(&id).unwrap().event_count, 3);
    assert_eq!(
        journal
            .verify_session_checkpoints(&id)
            .unwrap()
            .checkpoint_count,
        2
    );
}

#[test]
fn checkpoint_event_and_payload_commit_atomically_and_remain_chain_compatible() {
    let id = session_id("atomic-happy");
    let (_temp, mut journal) = create_journal("atomic-happy", &id);
    let stored = snapshot(&id, 1, "atomic");
    let event = journal.append_checkpoint(&id, 10, None, &stored).unwrap();

    assert!(matches!(event.event, Event::WorkspaceCheckpoint(_)));
    assert_eq!(event.sequence, stored.event_sequence());
    assert_eq!(
        journal.verify_session_chain(&id).unwrap().final_hash,
        event.event_hash
    );
    assert_eq!(
        journal
            .verify_session_checkpoints(&id)
            .unwrap()
            .checkpoint_count,
        1
    );
}

#[test]
fn plain_event_append_rejects_checkpoint_events_without_a_payload() {
    let id = session_id("atomic-only");
    let (_temp, mut journal) = create_journal("atomic-only", &id);
    let snapshot = snapshot(&id, 1, "atomic-only");
    let event = EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: id.clone(),
        sequence: 1,
        monotonic_millis: 10,
        wall_clock_utc: None,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event: Event::WorkspaceCheckpoint(snapshot.event_payload()),
    }
    .seal(Hash::zero())
    .unwrap();

    assert!(matches!(
        journal.append_event(&id, &event),
        Err(JournalError::CheckpointRequiresAtomicAppend)
    ));
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 1);
}

#[test]
fn append_rejects_snapshot_session_and_sequence_mismatches_without_mutation() {
    let id = session_id("expected-session");
    let (_temp, mut journal) = create_journal("mismatched-snapshot", &id);
    let wrong_session = snapshot(&session_id("other-session"), 1, "wrong-session");
    assert!(matches!(
        journal.append_checkpoint(&id, 10, None, &wrong_session),
        Err(JournalError::WrongSession { .. })
    ));
    let wrong_sequence = snapshot(&id, 2, "wrong-sequence");
    assert!(matches!(
        journal.append_checkpoint(&id, 10, None, &wrong_sequence),
        Err(JournalError::UnexpectedSequence {
            expected: 1,
            actual: 2,
            ..
        })
    ));
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 1);
    assert!(journal.read_events(&id, 1, 1).unwrap().is_empty());
}

#[test]
fn missing_payload_and_payload_owned_by_wrong_event_type_are_rejected() {
    let missing_id = session_id("missing-payload");
    let (missing_temp, mut missing) = create_journal("missing-payload", &missing_id);
    missing
        .append_checkpoint(&missing_id, 10, None, &snapshot(&missing_id, 1, "missing"))
        .unwrap();
    Connection::open(missing_temp.path())
        .unwrap()
        .execute(
            "DELETE FROM checkpoints WHERE session_id = ?1 AND sequence = 1",
            [missing_id.as_str()],
        )
        .unwrap();
    assert!(matches!(
        missing.load_checkpoint(&missing_id, 1),
        Err(JournalError::CorruptStorage(Corruption::Checkpoint {
            kind: CheckpointCorruption::MissingPayload,
            ..
        }))
    ));

    let wrong_id = session_id("wrong-owner");
    let (wrong_temp, mut wrong) = create_journal("wrong-owner", &wrong_id);
    let focused = focused_event(&wrong_id, 1, Hash::zero(), "doc");
    wrong.append_event(&wrong_id, &focused).unwrap();
    let encoded = encode_checkpoint(&snapshot(&wrong_id, 1, "wrong")).unwrap();
    Connection::open(wrong_temp.path())
        .unwrap()
        .execute(
            "INSERT INTO checkpoints (session_id, sequence, payload) VALUES (?1, 1, ?2)",
            params![wrong_id.as_str(), encoded],
        )
        .unwrap();
    assert!(matches!(
        wrong.load_checkpoint(&wrong_id, 1),
        Err(JournalError::CorruptStorage(Corruption::Checkpoint {
            kind: CheckpointCorruption::WrongOwningEventType,
            ..
        }))
    ));
}

#[test]
fn owner_workspace_and_document_hash_mismatches_are_rejected() {
    for mismatch in ["workspace", "document"] {
        let id = session_id(&format!("owner-{mismatch}"));
        let (temp, mut journal) = create_journal(&format!("owner-{mismatch}"), &id);
        let stored = snapshot(&id, 1, mismatch);
        let original = journal.append_checkpoint(&id, 10, None, &stored).unwrap();
        let Event::WorkspaceCheckpoint(mut owner) = original.event.clone() else {
            unreachable!()
        };
        if mismatch == "workspace" {
            owner.workspace_hash = Hash::from_bytes([0x91; Hash::LENGTH]);
        } else {
            owner.documents[0].hash = Hash::from_bytes([0x92; Hash::LENGTH]);
        }
        let replacement = EventEnvelope {
            event: Event::WorkspaceCheckpoint(owner),
            event_hash: Hash::zero(),
            previous_event_hash: Hash::zero(),
            ..original
        }
        .seal(Hash::zero())
        .unwrap();
        let payload = encode_envelope(&replacement).unwrap();
        Connection::open(temp.path())
            .unwrap()
            .execute(
                "UPDATE events SET event_hash = ?1, payload = ?2 WHERE session_id = ?3 AND sequence = 1",
                params![replacement.event_hash.as_bytes().as_slice(), payload, id.as_str()],
            )
            .unwrap();

        assert!(matches!(
            journal.load_checkpoint(&id, 1),
            Err(JournalError::CorruptStorage(Corruption::Checkpoint {
                kind: CheckpointCorruption::OwnerMismatch { .. },
                ..
            }))
        ));
    }
}

#[test]
fn corrupted_checkpoint_payload_and_out_of_domain_rows_are_typed_errors() {
    let corrupt_id = session_id("corrupt-payload");
    let (corrupt_temp, mut corrupt) = create_journal("corrupt-payload", &corrupt_id);
    corrupt
        .append_checkpoint(&corrupt_id, 10, None, &snapshot(&corrupt_id, 1, "payload"))
        .unwrap();
    let connection = Connection::open(corrupt_temp.path()).unwrap();
    let mut payload: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM checkpoints WHERE session_id = ?1 AND sequence = 1",
            [corrupt_id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    let middle = payload.len() / 2;
    payload[middle] ^= 1;
    connection
        .execute(
            "UPDATE checkpoints SET payload = ?1 WHERE session_id = ?2 AND sequence = 1",
            params![payload, corrupt_id.as_str()],
        )
        .unwrap();
    assert!(matches!(
        corrupt.load_checkpoint(&corrupt_id, 1),
        Err(JournalError::CorruptStorage(Corruption::Checkpoint {
            kind: CheckpointCorruption::MalformedPayload { .. },
            ..
        }))
    ));

    let invalid_id = session_id("invalid-row");
    let (invalid_temp, mut invalid) = create_journal("invalid-row", &invalid_id);
    let connection = Connection::open(invalid_temp.path()).unwrap();
    connection
        .pragma_update(None, "foreign_keys", "OFF")
        .unwrap();
    connection
        .execute(
            "INSERT INTO checkpoints (session_id, sequence, payload) VALUES (?1, -1, X'00')",
            [invalid_id.as_str()],
        )
        .unwrap();
    assert!(matches!(
        invalid.verify_session_checkpoints(&invalid_id),
        Err(JournalError::CorruptStorage(Corruption::Checkpoint {
            kind: CheckpointCorruption::InvalidColumn { field: "sequence" },
            ..
        }))
    ));
}

#[test]
fn orphan_checkpoint_and_extra_payload_are_rejected_by_explicit_verification() {
    let id = session_id("orphan");
    let (temp, mut journal) = create_journal("orphan", &id);
    journal
        .append_checkpoint(&id, 10, None, &snapshot(&id, 1, "orphan"))
        .unwrap();
    let connection = Connection::open(temp.path()).unwrap();
    connection
        .pragma_update(None, "foreign_keys", "OFF")
        .unwrap();
    connection
        .execute("DELETE FROM events WHERE session_id = ?1", [id.as_str()])
        .unwrap();
    connection
        .execute(
            "UPDATE sessions SET next_sequence = 1 WHERE session_id = ?1",
            [id.as_str()],
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        journal.verify_session_checkpoints(&id),
        Err(JournalError::CorruptStorage(Corruption::Checkpoint {
            kind: CheckpointCorruption::OrphanPayload,
            ..
        }))
    ));
}

#[test]
fn verification_rejects_a_valid_checkpoint_pair_beyond_the_authoritative_tail() {
    let id = session_id("checkpoint-beyond-tail");
    let (temp, mut journal) = create_journal("checkpoint-beyond-tail", &id);
    journal
        .append_checkpoint(&id, 10, None, &snapshot(&id, 1, "tail"))
        .unwrap();
    Connection::open(temp.path())
        .unwrap()
        .execute(
            "UPDATE sessions SET next_sequence = 1 WHERE session_id = ?1",
            [id.as_str()],
        )
        .unwrap();

    for result in [
        journal.load_checkpoint(&id, 1).map(|_| ()),
        journal.verify_session_checkpoints(&id).map(|_| ()),
    ] {
        assert!(matches!(
            result,
            Err(JournalError::CorruptStorage(Corruption::Checkpoint {
                sequence: Some(1),
                kind: CheckpointCorruption::SequenceOutsideSessionTail { next_sequence: 1 },
                ..
            }))
        ));
    }
}

#[test]
fn verification_finds_a_missing_payload_beyond_the_authoritative_tail() {
    let id = session_id("missing-checkpoint-beyond-tail");
    let (temp, mut journal) = create_journal("missing-checkpoint-beyond-tail", &id);
    journal
        .append_checkpoint(&id, 10, None, &snapshot(&id, 1, "missing-tail"))
        .unwrap();
    Connection::open(temp.path())
        .unwrap()
        .execute_batch(&format!(
            "DELETE FROM checkpoints WHERE session_id = '{}';\n\
             UPDATE sessions SET next_sequence = 1 WHERE session_id = '{}';",
            id.as_str(),
            id.as_str()
        ))
        .unwrap();

    assert!(matches!(
        journal.verify_session_checkpoints(&id),
        Err(JournalError::CorruptStorage(Corruption::Checkpoint {
            sequence: Some(1),
            kind: CheckpointCorruption::MissingPayload,
            ..
        }))
    ));
}

#[test]
fn checkpoint_load_recomputes_the_owning_event_hash() {
    let id = session_id("owner-event-hash");
    let (temp, mut journal) = create_journal("owner-event-hash", &id);
    journal
        .append_checkpoint(&id, 10, None, &snapshot(&id, 1, "hash"))
        .unwrap();
    let connection = Connection::open(temp.path()).unwrap();
    let mut event: EventEnvelope = journal.read_events(&id, 1, 1).unwrap().remove(0);
    event.event_hash = Hash::from_bytes([0x7a; Hash::LENGTH]);
    connection
        .execute(
            "UPDATE events SET event_hash = ?1, payload = ?2 \
             WHERE session_id = ?3 AND sequence = 1",
            params![
                event.event_hash.as_bytes().as_slice(),
                encode_envelope(&event).unwrap(),
                id.as_str()
            ],
        )
        .unwrap();

    for result in [
        journal.load_checkpoint(&id, 1).map(|_| ()),
        journal.latest_checkpoint_at_or_before(&id, 1).map(|_| ()),
        journal.list_checkpoints(&id, 1, 1).map(|_| ()),
        journal.verify_session_checkpoints(&id).map(|_| ()),
    ] {
        assert!(matches!(
            result,
            Err(JournalError::Chain {
                sequence: 1,
                kind: ChainMismatch::EventHash { .. },
                ..
            })
        ));
    }
}

#[test]
fn checkpoint_verification_pages_metadata_and_rejects_the_sqlite_domain_boundary() {
    let id = session_id("verification-pages");
    let (temp, mut journal) = create_journal("verification-pages", &id);
    for sequence in 1..=9 {
        journal
            .append_checkpoint(
                &id,
                sequence * 10,
                None,
                &snapshot(&id, sequence, &sequence.to_string()),
            )
            .unwrap();
    }
    assert_eq!(
        journal
            .verify_session_checkpoints(&id)
            .unwrap()
            .checkpoint_count,
        9
    );

    let connection = Connection::open(temp.path()).unwrap();
    connection
        .pragma_update(None, "foreign_keys", "OFF")
        .unwrap();
    connection
        .execute(
            "INSERT INTO checkpoints (session_id, sequence, payload) \
             VALUES (?1, ?2, zeroblob(1))",
            params![id.as_str(), i64::MAX],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        journal.verify_session_checkpoints(&id),
        Err(JournalError::CorruptStorage(Corruption::Checkpoint {
            sequence: Some(sequence),
            kind: CheckpointCorruption::InvalidColumn { field: "sequence" },
            ..
        })) if sequence == i64::MAX as u64
    ));
}

#[test]
fn concurrent_checkpoint_appends_cannot_fork_the_chain() {
    let temp = TempDatabase::new("concurrent");
    let id = session_id("concurrent");
    let mut initial = Journal::create(temp.path()).unwrap();
    initial.create_or_resume_session(&id).unwrap();
    drop(initial);

    let barrier = Arc::new(Barrier::new(2));
    let results = Arc::new(Mutex::new(Vec::new()));
    let workers: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|marker| {
            let path = temp.path().to_owned();
            let id = id.clone();
            let barrier = Arc::clone(&barrier);
            let results = Arc::clone(&results);
            thread::spawn(move || {
                let mut journal = Journal::open(path).unwrap();
                let snapshot = snapshot(&id, 1, marker);
                barrier.wait();
                results
                    .lock()
                    .unwrap()
                    .push(journal.append_checkpoint(&id, 10, None, &snapshot));
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let results = results.lock().unwrap();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(JournalError::UnexpectedSequence { .. })))
            .count(),
        1
    );
}

fn _assert_stored_checkpoint_is_public(_: StoredCheckpoint) {}

fn _assert_owner_wire_type(_: WorkspaceCheckpoint, _: DocumentHash) {}
