# rustrace-replay

`rustrace-replay` deterministically reconstructs a bounded workspace from a
verified journal checkpoint and contiguous v1 events. It has no terminal or
rendering dependency and performs no filesystem, process, or async operation.

```rust
let mut replay = ReplayEngine::from_initial_checkpoint(checkpoint)?;
replay.apply(&event)?;
let state = replay.workspace_state();
```

`from_initial_checkpoint` accepts only the sequence-one checkpoint with the
genesis zero predecessor. A raw `StoredCheckpoint` is deliberately not a later
seek authority: its wire owner does not bind document paths, versions,
selections, the active document, or derived replay lifecycle state.

To create a later seek point, continuous replay first applies the checkpoint
owner and then certifies its full persisted snapshot:

```rust
replay.apply(&stored.owning_event)?;
let certified = replay.certify_checkpoint(stored)?;
let mut seek = ReplayEngine::from_checkpoint(certified);
```

The certificate is opaque, process-local, and constructible only by the replay
engine. Certification requires exact raw files and complete open-document state
and captures the chain cursor and bounded in-flight Cargo command set. This
preserves suffix behavior without changing the checkpoint wire format.

Replay keeps exact raw workspace bytes separate from open UTF-8 document
buffers. This represents an external file change before the editor reloads or
resolves it. Ordinary edits cannot overwrite a divergent or missing raw file.
A `FileReload` transaction reconciles a divergent document only when its result
exactly reproduces the recorded raw bytes. When the document and replayed raw
bytes are coherent, the same origin represents a deliberate reload of the
persisted baseline and updates both through the transaction stream.

A checkpoint event and a `clean=true` submission finalization require every
active or inactive open document to agree with its workspace file. An explicit
`clean=false` recovery finalization may retain divergent or missing files, but
must still match the exact final raw-workspace hash and event count.

Every applied event is checked for session, sequence, previous hash, canonical
bounded encoding, and recomputed event hash. Workspace mutations are prepared
and hashed before commit. File count, per-file size, total workspace size,
transaction size, string/vector counts, checkpoint limits, retained open
documents, and active Cargo commands remain bounded by authoritative existing
limits. Cargo events validate a start-to-output/diagnostic-to-finish lifecycle
as inert data and are never executed. Terminal events require every active
command to have a matching finish. A command ID may be reused only after its
previous lifecycle finishes; replay retains active IDs rather than an
unbounded history of completed commands.

## Trust boundary

Replay reconstructs state and checks internal consistency. It never reads or
executes a student's workspace, runs recorded Cargo commands, compiles code, or
invokes language-server operations. Recorded command and LSP events are data
only.

The event chain and ordinary hashes are not authenticity or attestation. A
party able to fabricate the entire record can generate a new self-consistent
chain and checkpoints that pass replay.

## Property replay coverage

The integration property suite uses exactly pinned `proptest` 1.11.0. That is
the current maintained release and its Rust 1.85 minimum is below Rustrace's
Rust 1.98 floor. Only the `std` feature is enabled; process forking, wall-clock
timeouts, and unrelated bit-set support are excluded.

Normal workspace tests run 2,048 deterministic exact-replay cases and 2,048
deterministic tamper cases. These are fresh cases on every run; persisted
regressions are replayed in addition to those 4,096 cases. The runners use the
ChaCha algorithm with the literal 32-byte seeds
`b"rustrace-replay-property-seed-v1"` and
`b"rustrace-tamper-property-seed-v1"`. Case count, generated size, local and
global rejects, flat-map regeneration, and shrink iterations are explicitly
capped. Minimized failures are retained under `proptest-regressions/` and
replayed automatically before fresh cases in later local and CI runs.

Every generated case starts with a fixed required-operation spine and adds at
most 24 statefully interpreted primitive actions. A separate focused test runs
two fixed replay scenarios and four tamper selectors against each, for eight
focused tamper checks. Together these retain coverage for empty text, BOF and
EOF, newline and CRLF content, UTF-8 boundaries, combining and emoji graphemes,
reversed selections, rename chains, delete/recreate, and undo/redo exhaustion.

On an Apple M1 Max MacBook Pro with 10 cores and 64 GB RAM, macOS 15.7.9, a
warm debug build with dependencies already compiled measured 27.24 seconds
test time (27.30 seconds wall) for the complete corrected property target,
including both persisted cases. The corrected properties were also measured
separately: exact replay took 26.87 seconds test time (27.62 seconds wall), and
tamper validation took 19.85 seconds test time (19.91 seconds wall). Formal
review measured the full warm workspace at 50.51 seconds on Rust 1.98.0 and
56.00 seconds on stable. Similar hardware should budget about 30 seconds for
the property target; CI load, available cores, and cache state can change wall
time substantially.

### Scope limits

The generator deliberately creates small valid checkpoints and typed event
streams rather than arbitrary JSON or limit-exceeding inputs. Its initial
workspace has one to three open UTF-8 documents and may have one closed binary
file. Text comes from a finite corpus of ASCII, newline, CRLF, multibyte,
combining, CJK, Arabic, ZWJ, skin-tone, and regional-indicator tokens. Valid
selection and edit offsets are chosen from current UTF-8 character boundaries;
grapheme-aware deletion and undo/redo are delegated to the production editor.
External divergence/reload, Cargo and LSP lifecycles, unsupported formats, and
resource-limit rejection remain in their focused suites and are not randomized
by this Task 2.5 generator.

Tampering selects a recorded event and covers a stale outer event hash,
rehashed event-type-specific semantic contradictions, a rehashed out-of-bounds
edit range, or a rehashed transaction before-hash. It validates rejection and
no partial state advancement at that event, then proves the original event can
still apply. It does not rechain the remaining suffix or claim that a fully
self-consistent fabricated journal must fail: event hashes provide integrity,
not authenticity, as described above.

### Behavioral mutation evidence

The checked-in regression entries were created by actual failing Proptest runs,
not written by hand. For exact replay, a controlled temporary production mutant
moved file bytes on rename but retained the document's old path. This command:

```text
cargo +1.98.0 test -p rustrace-replay --test replay_properties generated_journals_replay_exactly_from_initial_and_later_checkpoints -- --exact --nocapture
```

failed because sequence 14 could not perform the second rename: document
`doc-lifecycle` remained at `generated/lifecycle-a.txt`, not the recorded
`generated/lifecycle-b.txt`. Proptest persisted seed
`cc 2d011440041edaa31659a095e89cb8967fbbecf9da4ca7209174c5f82afa5e0a`,
shrunk to `ScenarioSeed { initial: 0, actions: [] }`. After restoring the
production rename, the same command replayed the persisted case, ran 2,048
fresh cases, and passed.

For tamper validation, a second controlled temporary production mutant omitted
the recomputed event-hash comparison. This command:

```text
cargo +1.98.0 test -p rustrace-replay --test replay_properties generated_event_tampering_is_rejected_atomically -- --exact --nocapture
```

failed with `tampered event unexpectedly passed validation: ()`. Proptest
persisted seed
`cc ee721b52c79d30f0b2946addd9e32e92e8b29252d686c8605857a3ed88068eb1`,
shrunk to `(ScenarioSeed { initial: 0, actions: [] }, 0, 12)`: event selector
zero and tamper mode zero. After restoring event-hash validation, the same
command replayed the persisted case, ran 2,048 fresh cases, and passed. Both
mutants were fully reverted; no production behavior differs from the reviewed
base.
