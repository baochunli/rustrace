# Rustrace editor core

`rustrace-editor` owns the production document buffer and transaction path.
The Rope is private; keyboard input, paste, undo, redo, and programmatic edits
all commit through `EditorBuffer::apply_transaction`.

## Position conversion

`buffer.positions(expected_version)?` borrows the current document's Rope and
rejects stale versions. Use the view's typed `ByteOffset`, `ScalarIndex`,
`Utf16Position`, and `VisualPosition` conversions. Byte/scalar mapping preserves
every exact UTF-8 boundary; it never rounds a mid-codepoint byte. The view cannot
be used across a buffer mutation. Request owners must additionally check
session/document identity, replacement, cursor context, and server restart;
numeric conversion does not establish those identities.

Only UTF-16 LSP positions are supported, matching the spike's advertised encoding
and LSP's default. Protocol lines use CRLF, CR, or LF. Terminators have no interior
position: line content end points before the terminator and the next line's zero
points after it. UTF-16 surrogate interiors and integers above `2^31 - 1` fail.
For exact edit conversion, past-end lines/columns fail rather than adopting the
protocol's general past-end character clamping rule.

Visual lines retain Ropey's Unicode line-break rules. Columns use grapheme
widths and four-cell tab stops from the shared safe-display policy, before
viewport scrolling or terminal pane placement. Controls, standalone invisible
clusters, and oversized graphemes therefore occupy bounded visible expansions.
Exact conversions reject grapheme interiors and every interior cell of a tab,
wide glyph, escape, or marker. Scalar/LSP coordinates may still address
combining/emoji interiors that are not visual cursor stops. The existing
interactive vertical cursor movement separately floors to a grapheme and clamps
at line end, using the same shared widths.

Byte/scalar conversion uses Rope indices without allocations. LSP conversion
streams the prefix without copying the document; it cannot use Rope line counts
because LSP has fewer line-break kinds. Visual conversion visits one editor line,
borrowing contiguous storage or materializing that line across Rope chunks.
No conversion modifies text, selection, version, or undo/replay state.

## Transaction rules

- Every offset, including selection endpoints, is a UTF-8 byte offset.
- Edit ranges refer to the immutable `version_before` snapshot.
- Edits are sorted by `(start_byte, end_byte)`, may touch, and may not overlap.
- Applying edits from last to first preserves every pre-snapshot offset. Equal
  zero-width ranges therefore produce inserted text in vector order.
- Undo text larger than one edit is split at UTF-8 boundaries into bounded
  equal-start edits. The last chunk removes any replacement text; preceding
  zero-width chunks prepend in source order under reverse application, which
  reconstructs the deleted bytes exactly.
- A transaction advances the version by exactly one. Invalid and no-op changes
  leave content, version, selection, history, and effects unchanged.
- A normal edit commits only when both it and its future undo/redo entries fit
  the transaction codec. This makes every accepted transaction emit-safe and
  every stored history entry usable.

## Document hash

The document hash is BLAKE3 over the ASCII domain
`rustrace.document.utf8.v1\0` followed immediately by the exact UTF-8 document
bytes. Paths, timestamps, platform metadata, and Unicode normalization are not
included.

## Effects

After a successful atomic commit, the editor calls the provenance,
Tree-sitter, LSP `didChange`, and replay adapter hooks with the same immutable
`EditorTransaction` reference. Each hook is called exactly once when hooks do
not panic. These hooks are typed integration boundaries only; the journal and
language-server process are intentionally outside this crate and phase.

`replay_transactions` is the narrow Phase 1 helper for verifying an in-memory
transaction stream. It stages each recorded `selection_before` and then uses
the normal strict transaction gateway. `ReplayDocument` is the headless Phase
2 boundary used after checkpoints: it restores version and selection, shares
the production transaction validator, and deliberately retains no undo/redo
history because checkpoints do not persist that history. Durable journals,
workspace checkpoints, and workspace lifecycle replay remain outside this
crate.

`encode_transaction` emits compact JSON through a counting and bounded writer,
using the model's shared `MAX_FILE_EDITED_TRANSACTION_BYTES` limit. That limit
reserves the maximum canonical framing and metadata of any valid v1
`FileEdited` envelope, so every accepted transaction remains persistable
inside the one-MiB event limit. The encoder then applies the model's lexical
JSON preflight to the canonical bytes. Every committed transaction passes that
same bounded encoder and preflight before content, history, or effects change.
This includes the 512-KiB raw-token ceiling after JSON escaping, not only the
decoded inserted-text limit.
Locally built edits preflight before constructing the post-edit document; the
unknown post-edit hash uses a wire-size-equivalent fixed-width hash placeholder
and is replaced before the final authoritative validation.
Serialized transactions must enter through `decode_transaction`, which checks
the identical raw limit plus lexical nesting, escaped and decoded string,
container, and value limits before its private typed deserializer allocates
the transaction.
