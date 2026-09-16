use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
    thread,
};

use rusqlite::{Connection, params};
use rustrace_journal::{
    ChainMismatch, Corruption, EventCorruption, Journal, JournalError, MAX_EVENTS_PER_READ,
    SessionOpen, VerifiedSessionChain,
};
use rustrace_model::{
    DocumentId, EditOrigin, Event, EventEnvelope, FORMAT_VERSION_V1, FileEdited, FileFocused, Hash,
    MAX_ENVELOPE_BYTES, MAX_INSERTED_TEXT_BYTES, SelectionState, SessionId, TextEdit,
    compute_event_hash, encode_envelope,
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
            "rustrace-hash-chain-{name}-{}-{serial}",
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

fn hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; Hash::LENGTH])
}

fn unsealed_event(session_id: &SessionId, sequence: u64, document: &str) -> EventEnvelope {
    EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: session_id.clone(),
        sequence,
        monotonic_millis: sequence * 10,
        wall_clock_utc: None,
        previous_event_hash: hash(0x44),
        event_hash: hash(0x55),
        event: Event::FileFocused(FileFocused {
            document_id: DocumentId::new(document).unwrap(),
        }),
    }
}

fn sealed_event(
    session_id: &SessionId,
    sequence: u64,
    document: &str,
    previous: Hash,
) -> EventEnvelope {
    unsealed_event(session_id, sequence, document)
        .seal(previous)
        .unwrap()
}

fn append_chain(journal: &mut Journal, session_id: &SessionId, count: usize) -> Vec<EventEnvelope> {
    let mut previous = Hash::zero();
    let mut events = Vec::with_capacity(count);
    for index in 0..count {
        let sequence = u64::try_from(index).unwrap() + 1;
        let event = sealed_event(
            session_id,
            sequence,
            &format!("document-{sequence}"),
            previous,
        );
        journal.append_event(session_id, &event).unwrap();
        previous = event.event_hash;
        events.push(event);
    }
    events
}

fn create_file_journal(name: &str, id: &SessionId) -> (TempDatabase, Journal) {
    let temp = TempDatabase::new(name);
    let mut journal = Journal::create(temp.path()).unwrap();
    assert!(matches!(
        journal.create_or_resume_session(id).unwrap(),
        SessionOpen::Created(_)
    ));
    (temp, journal)
}

fn update_event_row(path: &Path, id: &SessionId, envelope: &EventEnvelope) {
    let payload = encode_envelope(envelope).unwrap();
    let connection = Connection::open(path).unwrap();
    connection
        .execute(
            "UPDATE events SET previous_event_hash = ?1, event_hash = ?2, payload = ?3 \
             WHERE session_id = ?4 AND sequence = ?5",
            params![
                envelope.previous_event_hash.as_bytes().as_slice(),
                envelope.event_hash.as_bytes().as_slice(),
                payload,
                id.as_str(),
                i64::try_from(envelope.sequence).unwrap(),
            ],
        )
        .unwrap();
}

#[test]
fn empty_and_multi_event_sessions_return_exact_verified_summaries() {
    let empty_id = session_id("empty");
    let mut empty = Journal::open_in_memory().unwrap();
    empty.create_or_resume_session(&empty_id).unwrap();
    assert_eq!(
        empty.verify_session_chain(&empty_id).unwrap(),
        VerifiedSessionChain {
            event_count: 0,
            final_hash: Hash::zero(),
        }
    );

    let id = session_id("happy");
    let mut journal = Journal::open_in_memory().unwrap();
    journal.create_or_resume_session(&id).unwrap();
    let events = append_chain(&mut journal, &id, 3);

    assert_eq!(
        journal.verify_session_chain(&id).unwrap(),
        VerifiedSessionChain {
            event_count: 3,
            final_hash: events[2].event_hash,
        }
    );
}

#[test]
fn append_rejects_wrong_chain_values_without_mutation() {
    let genesis_id = session_id("wrong-genesis");
    let mut genesis_journal = Journal::open_in_memory().unwrap();
    genesis_journal
        .create_or_resume_session(&genesis_id)
        .unwrap();
    let mut wrong_genesis = sealed_event(&genesis_id, 1, "genesis", Hash::zero());
    wrong_genesis.previous_event_hash = hash(0x99);
    assert!(matches!(
        genesis_journal.append_event(&genesis_id, &wrong_genesis),
        Err(JournalError::Chain {
            sequence: 1,
            kind: ChainMismatch::GenesisPreviousHash { .. },
            ..
        })
    ));
    assert_eq!(
        genesis_journal
            .inspect_session(&genesis_id)
            .unwrap()
            .next_sequence,
        1
    );
    assert!(
        genesis_journal
            .read_events(&genesis_id, 1, 1)
            .unwrap()
            .is_empty()
    );

    let id = session_id("wrong-tail");
    let mut journal = Journal::open_in_memory().unwrap();
    journal.create_or_resume_session(&id).unwrap();
    let first = sealed_event(&id, 1, "first", Hash::zero());
    journal.append_event(&id, &first).unwrap();

    let mut wrong_previous = sealed_event(&id, 2, "second", first.event_hash);
    wrong_previous.previous_event_hash = hash(0x77);
    assert!(matches!(
        journal.append_event(&id, &wrong_previous),
        Err(JournalError::Chain {
            sequence: 2,
            kind: ChainMismatch::PreviousHash { .. },
            ..
        })
    ));

    let mut wrong_hash = sealed_event(&id, 2, "second", first.event_hash);
    wrong_hash.event_hash = hash(0x88);
    assert!(matches!(
        journal.append_event(&id, &wrong_hash),
        Err(JournalError::Chain {
            sequence: 2,
            kind: ChainMismatch::EventHash { .. },
            ..
        })
    ));
    assert_eq!(journal.inspect_session(&id).unwrap().next_sequence, 2);
    assert_eq!(journal.read_events(&id, 1, 10).unwrap(), vec![first]);
}

#[test]
fn concurrent_distinct_appends_cannot_fork_the_genesis() {
    let temp = TempDatabase::new("concurrent-fork");
    let id = session_id("concurrent-fork");
    let mut initial = Journal::create(temp.path()).unwrap();
    initial.create_or_resume_session(&id).unwrap();
    drop(initial);

    let candidates = [
        sealed_event(&id, 1, "candidate-a", Hash::zero()),
        sealed_event(&id, 1, "candidate-b", Hash::zero()),
    ];
    let candidate_hashes = [candidates[0].event_hash, candidates[1].event_hash];
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = candidates
        .into_iter()
        .map(|candidate| {
            let path = temp.path().to_owned();
            let id = id.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut journal = Journal::open(path).unwrap();
                barrier.wait();
                journal.append_event(&id, &candidate)
            })
        })
        .collect();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(JournalError::UnexpectedSequence { .. })))
            .count(),
        1
    );
    let mut journal = Journal::open(temp.path()).unwrap();
    let verified = journal.verify_session_chain(&id).unwrap();
    assert_eq!(verified.event_count, 1);
    assert!(candidate_hashes.contains(&verified.final_hash));
}

#[test]
fn changed_canonical_event_content_invalidates_verification() {
    let id = session_id("changed-content");
    let (temp, mut journal) = create_file_journal("changed-content", &id);
    let events = append_chain(&mut journal, &id, 2);

    let mut fabricated = events[0].clone();
    fabricated.event = Event::FileFocused(FileFocused {
        document_id: DocumentId::new("fabricated-content").unwrap(),
    });
    let payload = encode_envelope(&fabricated).unwrap();
    let connection = Connection::open(temp.path()).unwrap();
    connection
        .execute(
            "UPDATE events SET payload = ?1 WHERE session_id = ?2 AND sequence = 1",
            params![payload, id.as_str()],
        )
        .unwrap();

    assert!(matches!(
        journal.verify_session_chain(&id),
        Err(JournalError::Chain {
            sequence: 1,
            kind: ChainMismatch::EventHash { .. },
            ..
        })
    ));
}

#[test]
fn deleting_or_swapping_events_invalidates_verification() {
    let deleted_id = session_id("deleted");
    let (deleted_temp, mut deleted_journal) = create_file_journal("deleted", &deleted_id);
    append_chain(&mut deleted_journal, &deleted_id, 3);
    let connection = Connection::open(deleted_temp.path()).unwrap();
    connection
        .execute(
            "DELETE FROM events WHERE session_id = ?1 AND sequence = 2",
            [deleted_id.as_str()],
        )
        .unwrap();
    assert!(matches!(
        deleted_journal.verify_session_chain(&deleted_id),
        Err(JournalError::CorruptStorage(Corruption::Event {
            kind: EventCorruption::SequenceGap {
                expected: 2,
                actual: 3
            },
            ..
        }))
    ));

    let swapped_id = session_id("swapped");
    let (swapped_temp, mut swapped_journal) = create_file_journal("swapped", &swapped_id);
    append_chain(&mut swapped_journal, &swapped_id, 2);
    let connection = Connection::open(swapped_temp.path()).unwrap();
    let first: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM events WHERE session_id = ?1 AND sequence = 1",
            [swapped_id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    let second: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM events WHERE session_id = ?1 AND sequence = 2",
            [swapped_id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "UPDATE events SET payload = ?1 WHERE session_id = ?2 AND sequence = 1",
            params![second, swapped_id.as_str()],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE events SET payload = ?1 WHERE session_id = ?2 AND sequence = 2",
            params![first, swapped_id.as_str()],
        )
        .unwrap();
    assert!(matches!(
        swapped_journal.verify_session_chain(&swapped_id),
        Err(JournalError::CorruptStorage(Corruption::Event {
            kind: EventCorruption::ColumnMismatch { field: "sequence" },
            ..
        }))
    ));
}

#[test]
fn coherent_chain_field_tampering_returns_typed_errors() {
    let genesis_id = session_id("tampered-genesis");
    let (genesis_temp, mut genesis_journal) = create_file_journal("tampered-genesis", &genesis_id);
    let mut genesis = append_chain(&mut genesis_journal, &genesis_id, 1).remove(0);
    genesis.previous_event_hash = hash(0x12);
    genesis.event_hash = compute_event_hash(genesis.previous_event_hash, &genesis).unwrap();
    update_event_row(genesis_temp.path(), &genesis_id, &genesis);
    assert!(matches!(
        genesis_journal.verify_session_chain(&genesis_id),
        Err(JournalError::Chain {
            sequence: 1,
            kind: ChainMismatch::GenesisPreviousHash { .. },
            ..
        })
    ));

    let hash_id = session_id("tampered-hash");
    let (hash_temp, mut hash_journal) = create_file_journal("tampered-hash", &hash_id);
    let mut event = append_chain(&mut hash_journal, &hash_id, 1).remove(0);
    event.event_hash = hash(0x34);
    update_event_row(hash_temp.path(), &hash_id, &event);
    assert!(matches!(
        hash_journal.verify_session_chain(&hash_id),
        Err(JournalError::Chain {
            sequence: 1,
            kind: ChainMismatch::EventHash { .. },
            ..
        })
    ));
}

#[test]
fn newly_fabricated_self_consistent_chain_verifies() {
    let id = session_id("fabricated");
    let mut journal = Journal::open_in_memory().unwrap();
    journal.create_or_resume_session(&id).unwrap();

    let fabricated = append_chain(&mut journal, &id, 4);
    assert_eq!(
        journal.verify_session_chain(&id).unwrap(),
        VerifiedSessionChain {
            event_count: 4,
            final_hash: fabricated[3].event_hash,
        }
    );
}

#[test]
fn verification_crosses_pages_and_accepts_the_maximum_envelope() {
    let paged_id = session_id("paged");
    let mut paged = Journal::open_in_memory().unwrap();
    paged.create_or_resume_session(&paged_id).unwrap();
    let events = append_chain(&mut paged, &paged_id, MAX_EVENTS_PER_READ + 1);
    assert_eq!(
        paged.verify_session_chain(&paged_id).unwrap(),
        VerifiedSessionChain {
            event_count: u64::try_from(MAX_EVENTS_PER_READ).unwrap() + 1,
            final_hash: events.last().unwrap().event_hash,
        }
    );

    let maximum_id = session_id("maximum-envelope");
    let mut maximum = Journal::open_in_memory().unwrap();
    maximum.create_or_resume_session(&maximum_id).unwrap();
    let maximum_text = "x".repeat(MAX_INSERTED_TEXT_BYTES);
    let mut envelope = EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: maximum_id.clone(),
        sequence: 1,
        monotonic_millis: 1,
        wall_clock_utc: None,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event: Event::FileEdited(FileEdited {
            document_id: DocumentId::new("maximum").unwrap(),
            version_before: 0,
            version_after: 1,
            origin: EditOrigin::Paste,
            edits: vec![
                TextEdit {
                    start_byte: 0,
                    end_byte: 0,
                    inserted_text: maximum_text.clone(),
                },
                TextEdit {
                    start_byte: 0,
                    end_byte: 0,
                    inserted_text: maximum_text.clone(),
                },
                TextEdit {
                    start_byte: 0,
                    end_byte: 0,
                    inserted_text: maximum_text,
                },
                TextEdit {
                    start_byte: 0,
                    end_byte: 0,
                    inserted_text: String::new(),
                },
            ],
            selection_before: SelectionState::caret(0),
            selection_after: SelectionState::caret(0),
            hash_before: hash(1),
            hash_after: hash(2),
        }),
    };
    let base_length = encode_envelope(&envelope).unwrap().len();
    let tail_length = MAX_ENVELOPE_BYTES.checked_sub(base_length).unwrap();
    assert!(tail_length <= MAX_INSERTED_TEXT_BYTES);
    let Event::FileEdited(transaction) = &mut envelope.event else {
        unreachable!();
    };
    transaction.edits[3].inserted_text = "y".repeat(tail_length);
    let envelope = envelope.seal(Hash::zero()).unwrap();
    assert_eq!(
        encode_envelope(&envelope).unwrap().len(),
        MAX_ENVELOPE_BYTES
    );

    maximum.append_event(&maximum_id, &envelope).unwrap();
    assert_eq!(
        maximum.verify_session_chain(&maximum_id).unwrap(),
        VerifiedSessionChain {
            event_count: 1,
            final_hash: envelope.event_hash,
        }
    );
}

#[test]
fn verification_rejects_oversized_and_malformed_storage_without_panicking() {
    let oversized_id = session_id("oversized");
    let (oversized_temp, mut oversized_journal) = create_file_journal("oversized", &oversized_id);
    append_chain(&mut oversized_journal, &oversized_id, 1);
    let connection = Connection::open(oversized_temp.path()).unwrap();
    connection
        .execute_batch("PRAGMA ignore_check_constraints = ON;")
        .unwrap();
    connection
        .execute(
            "UPDATE events SET payload = zeroblob(1048577) \
             WHERE session_id = ?1 AND sequence = 1",
            [oversized_id.as_str()],
        )
        .unwrap();
    assert!(matches!(
        oversized_journal.verify_session_chain(&oversized_id),
        Err(JournalError::CorruptStorage(Corruption::Event {
            kind: EventCorruption::OversizedPayload {
                actual: 1_048_577,
                maximum: MAX_ENVELOPE_BYTES
            },
            ..
        }))
    ));

    let malformed_id = session_id("malformed");
    let (malformed_temp, mut malformed_journal) = create_file_journal("malformed", &malformed_id);
    append_chain(&mut malformed_journal, &malformed_id, 1);
    let connection = Connection::open(malformed_temp.path()).unwrap();
    connection
        .execute(
            "UPDATE events SET payload = X'7B' WHERE session_id = ?1 AND sequence = 1",
            [malformed_id.as_str()],
        )
        .unwrap();
    assert!(matches!(
        malformed_journal.verify_session_chain(&malformed_id),
        Err(JournalError::CorruptStorage(Corruption::Event {
            kind: EventCorruption::MalformedPayload { .. },
            ..
        }))
    ));
}
