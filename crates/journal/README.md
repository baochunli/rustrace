# Rustrace journal

`rustrace-journal` is the synchronous, local SQLite boundary for provenance
events. It stores complete canonical `EventEnvelope` bytes from
`rustrace-model`; it does not aggregate event contents or expose its SQLite
connection.

## Schema version 1

Schema creation is one `IMMEDIATE` transaction and publishes
`PRAGMA user_version = 1` in that transaction. An empty version-0 database is
migrated to version 1. The version decision and emptiness check happen after
the transaction holds the writer lock, so concurrent initializers converge on
version 1. A version-0 database with application tables is rejected as
unversioned, and a newer version is rejected before any persistent SQLite
setting is changed.

- `sessions` owns `session_id`, the exact `next_sequence`, and the ended flag.
- `events` has `PRIMARY KEY (session_id, sequence)` and a foreign key to
  `sessions`. Each row keeps the format version, both envelope hash columns,
  and the complete canonical envelope BLOB.
- `checkpoints` stores the bounded compressed v1 snapshot and is keyed to its
  owning `WorkspaceCheckpoint` event.
- `documents` remains reserved for a future live-document projection and
  belongs to a session. Authoritative checkpoint document metadata is inside
  the checkpoint BLOB and owning event.
- `metadata` is reserved for bounded journal/package metadata.

The existing schema-v1 checkpoint table required no migration for T2.3. There
is no migration framework: future versions add explicit `user_version` steps
here.

Version 1 is wholly owned by this crate. Opening requires the exact version-1
table definitions (including primary keys, foreign keys, checks, `STRICT`, and
`WITHOUT ROWID` where specified), the one SQLite-created primary-key index,
and no other tables, indexes, views, or triggers. This deliberately rejects
lookalike schemas and database objects that could change write semantics.

## Connection and durability policy

Every connection explicitly enables and verifies:

- `foreign_keys = ON`;
- a 5,000 ms busy timeout;
- `synchronous = FULL`;
- `SQLITE_LIMIT_LENGTH = 11,132,290` bytes (the maximum checkpoint BLOB plus
  4 KiB of bounded row overhead);
- `journal_mode = WAL` for file databases.

SQLite cannot use WAL for a private `:memory:` database, so
`Journal::open_in_memory` requires and verifies `journal_mode = memory` while
retaining the other settings. File-backed open fails if WAL cannot be enabled;
silently weakening concurrency or durability is not accepted.

`append_event` first runs the model's bounded canonical encoder. It then opens
an `IMMEDIATE` transaction, checks session ownership, ended state, exact next
sequence, and the current exact schema under the held writer lock. Sequence 1
must link to `Hash::zero()`. Later events must link to the authoritative prior
row's exact 32-byte `event_hash`, read only through bounded scalar metadata.
The journal recomputes the new BLAKE3 event hash from canonical model material
and requires an exact match before any insert or state update. It then inserts
the canonical BLOB and advances `next_sequence` in the same transaction.
Session creation and ending perform the same schema check before their DML.
Before commit, append rereads the exact inserted row and final session state,
so a missing or modified row causes rollback. It returns only after the
`synchronous=FULL` commit succeeds. The `IMMEDIATE` writer lock serializes
competing writers, so two appends cannot fork or commit the same sequence.
Accordingly, the journal has a zero-event application buffering window:
successfully returned appends are committed durably. An append whose call is
still in flight when the process or machine stops may be present in full or
absent in full, but is never exposed partially; SQLite rolls an incomplete
transaction back on reopen. Durability still depends on the operating system
and storage hardware honoring SQLite's synchronization requests.

Ending a session is an immediate transaction and is idempotent. Ended sessions
cannot append and are not resumed. `create_or_resume_session` resumes the sole
unfinished session regardless of the proposed new ID. No unfinished session
creates the proposed ID; multiple unfinished sessions return an explicit
ambiguity error.

## Checkpoints

`CheckpointSnapshot` contains the complete small workspace, session and event
sequence, active/open document state, canonical workspace hash, and recomputed
document hashes. Version 1 sets exact limits for paths, counts, and encoded sizes.
The compact canonical binary snapshot is compressed with pinned
`flate2` 1.1.10 zlib level 6 and wrapped in length framing plus a domain-
separated BLAKE3 digest. Decode is bounded before allocation, fully consumes one
stream, revalidates all relationships and hashes, and requires byte-identical
canonical re-encoding.

`append_checkpoint` creates and seals its event against the authoritative tail,
then inserts the event, checkpoint payload, and next-sequence update in one
`IMMEDIATE`, `synchronous=FULL` transaction. It rereads both inserted rows
before commit. An error leaves neither half committed. `append_event` rejects
`WorkspaceCheckpoint`, preventing supported callers from bypassing the atomic
API.

Exact load, ascending bounded list, and latest-at-or-before APIs return both the
owning event and verified snapshot after recomputing the owner's event hash
against its authoritative previous hash. `verify_session_checkpoints` walks
ordered keyset pages of at most eight metadata rows in one stable read
transaction, keeps one checkpoint payload live at a time, scans actual event
rows even beyond a corrupted session tail, and rejects out-of-tail sequences,
missing payloads, orphans, wrong owning event types, malformed or oversized
BLOBs, invalid DB domains, and session/sequence/workspace/document/hash
disagreement. T2.4 can restore any returned snapshot and continue reading at
`event_sequence + 1`.

`CheckpointPolicy` implements the 30–60 second interval, positive edit-event
threshold, either-limit activity decision, and unconditional before-Cargo,
after-Cargo, and finalization triggers as a pure function.
`CheckpointScheduleState` keeps a saturating edit count, elapsed baseline, and
pending/retry state. Queueing records the immutable snapshot's capture time and
covered edit count and binds them to the accepted writer attempt. Only that
attempt's completion or stopped-worker error can resolve pending state;
persisted success retains later edits and measures the next interval from
capture, while failure preserves all edits for retry.
The bounded `JournalWriter` serializes owned ordinary-event and checkpoint jobs
through one capacity-1–16 FIFO and assigns their authoritative sequences in
queue order. Its try-submit APIs return `QueueFull`/`Closed` with the owned job;
identity-space exhaustion is likewise typed and returns the job. Success means
queued, not persisted. Every accepted job receives a non-reused, process-global
attempt identity echoed by its receipt and all outcomes, so writers cannot
acknowledge one another's work. Per-job receipts report durable success/failure.
Snapshot work and storage stay off the submitting path.
Explicit shutdown drains, joins, and reports errors/panics; Drop closes and
detaches without blocking.

## Validation and limits

Opening a supported database validates required schema objects, runs
`quick_check(1)`, checks foreign keys, and verifies that each session describes
one contiguous event prefix. These potentially database-wide checks happen at
the open boundary, never on every append.

Event reads are paginated and ordered. A call returns at most 1,024 events and
at most 16 MiB of encoded payload. In one read transaction and snapshot, the
first query projects only scalar metadata plus a 129-byte session prefix and
33-byte hash prefixes. It checks SQLite types and lengths, passes every bounded
session value through `SessionId::new`, and rejects payloads above the model's
1 MiB envelope limit. Only then does a second query project that row's payload.
The connection-wide SQLite length limit is an additional backstop against an
oversized checkpoint cell or row. Event metadata still enforces the event's
smaller 1 MiB limit before its BLOB is read. The model's bounded decoder then
validates the payload.
Stored session, sequence, format version, previous hash, and event hash columns
must equal the decoded envelope, and re-encoding must reproduce the exact
bytes. Malformed, oversized, non-canonical, inconsistent, physically corrupt,
truncated, or incomplete storage returns typed errors rather than panicking.

`verify_session_chain` verifies a complete session in one SQLite read
transaction and therefore one stable snapshot. It walks at most 1,024 bounded
metadata records per page, fetches and discards one at-most-1-MiB payload at a
time, and never collects the full journal. Verification checks sequences
1 through N, genesis, previous links, recomputed hashes, bounded canonical
model decoding, stored-column agreement, and the declared session tail. Its
summary contains the exact event count and final hash; an empty session returns
count zero and `Hash::zero()`.

Full verification is explicit. Append performs only the locked tail checks
needed to prevent a new inconsistency and does not rescan history. Callers
should run `verify_session_chain` at complete-history integrity boundaries,
including submission finalization and untrusted provenance inspection.

SQLite sequences are limited to `i64::MAX - 1`, leaving a representable
`next_sequence` after every accepted append. Session IDs retain the model's
128-byte bound. Public callers cannot access the raw connection, execute
unchecked SQL, or bypass the model codec.

## Trust and Phase 2 ownership boundaries

The event chain and checkpoint digests/hashes are internal consistency and
corruption checks only. A complete newly fabricated self-consistent chain and
checkpoint set verifies successfully by design. There is no secret, signature,
trusted hardware, or attestation, and these checks do not prove who produced a
history or whether a client was modified. T2.4 owns deterministic headless
replay. This crate has no async runtime, encryption, signing, delta store,
content aggregation, terminal rendering, or replay behavior.

The crash test uses a real temporary database and isolated child processes. A
child exits immediately after a successful commit to prove it survives without
Rust destructors; a second child exits after SQL insertion but before the
transaction can advance or commit, proving the partial append disappears and
the session resumes at the same sequence.
