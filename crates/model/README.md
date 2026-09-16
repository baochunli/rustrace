# Rustrace model

This crate owns the persisted event vocabulary for Rustrace.

`document_hash` is the shared content-hash primitive used by editor
transactions and checkpoint document metadata. It hashes the exact UTF-8 bytes
as `BLAKE3("rustrace.document.utf8.v1" || 00 || bytes)` without normalization.

## Event envelope format version 1

The canonical representation is compact UTF-8 JSON produced by
encode_envelope. Envelope fields are emitted in declaration order. Events use
an adjacent type and payload representation with snake_case variant names.
Identifiers are strings and hashes are exactly 64 lowercase hexadecimal
characters representing 32 bytes. UTC wall-clock values use RFC 3339. The wall
clock is contextual; replay uses sequence and monotonic_millis.

## Event hash chain

Version 1 pins the `blake3` crate at 1.8.7 and defines each event hash as:

```text
e_i = compact canonical UTF-8 JSON of:
      format_version, session_id, sequence, monotonic_millis,
      wall_clock_utc, event

h_i = BLAKE3(exact 32-byte h_(i-1) || u64_be(length(e_i)) || e_i)
```

The JSON field order shown above is part of the contract. `event` retains the
variant, payload, nested-field, and array ordering of the existing version 1
canonical envelope codec. The byte length is checked and encoded as exactly
eight unsigned big-endian bytes. Sequence 1 uses `Hash::zero()` as `h_0`.
Neither `previous_event_hash` nor `event_hash` appears in `e_i`; changing the
values supplied in those envelope fields cannot create circular hashing or
change the canonical hash material.

`encode_event_hash_material` exposes the bounded `e_i` bytes for verification
and golden fixtures. `compute_event_hash` takes the authoritative previous
hash explicitly. `EventEnvelope::seal` sets both chain fields, computes the
hash, and confirms that the resulting full envelope remains persistable by the
bounded version 1 codec.

The pinned golden vector is:

```text
h_0:
0000000000000000000000000000000000000000000000000000000000000000

length(e_i): 193
u64 big-endian length bytes: 00000000000000c1

e_i:
{"format_version":1,"session_id":"session-01","sequence":7,"monotonic_millis":25,"wall_clock_utc":"2026-01-02T03:04:05Z","event":{"type":"file_focused","payload":{"document_id":"document-01"}}}

h_i:
ce8cd99a855aad67f76af4c68d4387788394565a56680b75f8d9897708d2afcd
```

### Envelope fields

| Field | Wire type |
| --- | --- |
| format_version | u32; exactly 1 |
| session_id | SessionId string |
| sequence | u64; starts at 1 |
| monotonic_millis | u64 |
| wall_clock_utc | RFC 3339 string or null |
| previous_event_hash | Hash string |
| event_hash | Hash string |
| event | adjacent-tagged Event object |

### Event tags and payload fields

Field order shown below is canonical. A question mark means nullable and square
brackets mean an array.

| Event type tag | Payload fields, in wire order |
| --- | --- |
| session_started | client_version: string; starter_workspace_hash: Hash |
| session_resumed | last_sequence: u64 |
| session_ended | final_workspace_hash: Hash |
| file_created | document_id: DocumentId; path: WorkspacePath string; contents: string; content_hash: Hash |
| file_deleted | document_id: DocumentId; path: WorkspacePath string; previous_hash: Hash |
| file_renamed | document_id: DocumentId; old_path: WorkspacePath string; new_path: WorkspacePath string |
| file_focused | document_id: DocumentId |
| file_edited | document_id: DocumentId; version_before: u64; version_after: u64; origin: EditOrigin; edits: TextEdit[]; selection_before: SelectionState; selection_after: SelectionState; hash_before: Hash; hash_after: Hash |
| selection_changed | document_id: DocumentId; anchor_byte: u64; active_byte: u64 |
| viewport_changed | document_id: DocumentId; top_line: u64; horizontal_column: u64 |
| cargo_command_started | command_id: CommandId; program: string; arguments: string[]; working_directory: WorkspaceDirectory string |
| cargo_diagnostic | command_id: CommandId; document_id: DocumentId?; severity: DiagnosticSeverity; code: string?; message: string; range: TextRange? |
| cargo_output | command_id: CommandId; stream: OutputStream; output: string |
| cargo_command_finished | command_id: CommandId; exit_code: i32?; success: bool |
| lsp_completion_requested | document_id: DocumentId; document_version: u64; position_byte: u64 |
| lsp_completion_accepted | document_id: DocumentId; document_version: u64; label: string; primary_edit: TextEdit; additional_edits: TextEdit[] |
| lsp_code_action_applied | title: string; kind: string?; document_ids: DocumentId[] |
| workspace_checkpoint | workspace_hash: Hash; documents: DocumentHash[] |
| external_file_change | path: WorkspacePath string; previous_contents: string?; new_contents: string?; previous_hash: Hash?; new_hash: Hash? |
| submission_finalized | final_workspace_hash: Hash; event_count: u64; clean: bool; warnings: string[] |

Nested wire objects are:

| Type | Fields, in wire order |
| --- | --- |
| TextEdit | start_byte: u64; end_byte: u64; inserted_text: string |
| SelectionState | anchor_byte: u64; active_byte: u64 |
| TextRange | start_line: u64; start_column: u64; end_line: u64; end_column: u64 |
| DocumentHash | document_id: DocumentId; hash: Hash |

EditOrigin values are keyboard, paste, undo, redo, completion,
additional_completion_edit, formatter, code_action, file_reload,
external_change, and unknown.
DiagnosticSeverity values are error, warning, information, and hint.
OutputStream values are stdout and stderr.

FileEdited is the shared model-level EditorTransaction type used by the editor
crate. There is no conversion layer that can omit origin, selection, edit, or
hash data.

`MAX_FILE_EDITED_TRANSACTION_BYTES` is the shared canonical JSON budget for a
transaction. It subtracts the exact worst-case v1 `FileEdited` envelope
overhead from the one-MiB envelope limit: maximum safe SessionId bytes, u64
sequence width, bounded monotonic width, Chrono's longest UTC representation,
two fixed-width hashes, and JSON/event framing. Identifiers cannot contain
JSON-escaped characters. The budget is deliberately independent of actual
metadata so an accepted editor transaction fits every valid envelope.

Callers must use decode_envelope at persistence and import boundaries. It
performs a bounded lexical pass before Serde allocates the decoded object
graph. The pass validates JSON syntax and rejects duplicate decoded keys in
every object. Its limits are:

- encoded envelope: 1 MiB;
- nesting: 16;
- members in each object or items in each array: 1,024;
- values across the document: 8,192;
- decoded keys and raw key tokens: 4 KiB;
- decoded string values: 256 KiB;
- raw string tokens: 512 KiB.

Event and EventEnvelope deliberately implement serialization but not Serde
deserialization. decode_envelope is the only supported untrusted decode path,
so raw byte, lexical, version, and semantic validation cannot be bypassed.
Canonical encoding first serializes through a capped counting sink. It
allocates the exact output size only after proving that the representation is
at most 1 MiB.

Decoded v1 events are then validated recursively with field-specific limits:

- identifiers: 128 bytes;
- ordinary strings: 4 KiB;
- workspace paths: 1 KiB;
- inserted/file text: 256 KiB;
- command output: 256 KiB;
- vectors: 1,024 items.

`WorkspacePath` preserves the v1 wire type as a JSON string while making an
invalid path unrepresentable in the Rust model. Canonical domain and wire paths
use `/` separators; backslash aliases are rejected rather than rewritten.
Native platform paths must be converted explicitly at the filesystem boundary.
Unicode is required to already be NFC; construction rejects non-NFC spelling
rather than silently selecting a canonically equivalent filesystem name. Paths
are nonempty and relative, with no drive, root, UNC, or verbatim prefix, null
byte, empty component, `.`, or `..`. Canonical paths are limited to 1,024
UTF-8 bytes, 255 bytes per component, and 64 components. Serde deserialization
invokes the same constructor, so persisted events cannot bypass these rules.

`WorkspaceDirectory` is used only for command working directories. Its `.`
wire value represents the workspace root exactly; every other value must be a
valid `WorkspacePath`. Forms such as `./`, `./src`, `src/.`, and `..` remain
invalid.

Sequences start at one and document versions advance exactly once per edit.
Edits are ordered by start and end offsets, may touch but cannot overlap, and
equal-offset insertions retain vector order. Selection direction is preserved
with anchor and active byte offsets. Snapshot-dependent UTF-8 boundaries,
selection bounds, and hash contents remain editor mutation checks. Monotonic
milliseconds are bounded to the signed 64-bit range so later duration
conversions remain lossless.

DecodePolicy::RejectUnsupported returns typed errors for future envelope
versions and unknown event variants. DecodePolicy::SkipUnsupported returns
DecodeOutcome::Skipped with only envelope metadata and a reason; it never
constructs an Event for data the v1 decoder does not understand.

This unkeyed chain detects internal inconsistency and accidental corruption.
It provides no authenticity or attestation: a client that controls all event
content can fabricate a new self-consistent chain.

`inserted_text_counts` derives Unicode scalar and LF-delimited line counts from
exact inserted text without serialized metadata. Paste provenance records bounded
origin metadata and source links without proving authorship.
