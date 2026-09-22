# Privacy and recorded data

This document describes Rustrace format version 1 and the current production
recorder. Provenance can contain source history and other sensitive course work.
Use `rustrace privacy WORKSPACE` before submission to inspect the identities,
ordered attempts, time spans, category counts, and byte totals that the current
workspace would contribute.

## Privacy statement

Rustrace records source and editor history, commands and their output,
diagnostics, tool metadata, and bounded disk-change evidence generated or
observed while you work in the assignment TUI. Source history includes code that
you later edit or delete. A revised submission includes every recorded attempt
back to the original starter, including prior deleted code, tool output, and
required recovery evidence. Blocked clipboard attempts retain only a bounded
reason and input-channel label, never the rejected content. When you copy or cut
inside Rustrace, it sends that already-recorded selection to your terminal as a
one-way system clipboard write. Rustrace never reads the system clipboard or
records other operating-system clipboard contents. It does not record webcam or
screen images, activity in other applications, or keystroke-timing biometrics.
Pointer activity, click coordinates, buttons, click counts, and scrolling are
not recorded. A mouse selection records only the same resulting document byte
range that keyboard selection records.

Cargo commands may contact `crates.io` or other endpoints selected by retained
Cargo configuration. Downloaded dependencies and their build scripts execute on
the student's machine under the retained Cargo configuration and may have local
side effects. Rustrace does not record registry requests, responses,
credentials, downloaded cache contents, or general network activity. It does
record the command/output evidence and the exact accepted `Cargo.toml` and
`Cargo.lock` edits produced by console `cargo add` and `cargo remove` commands
and the Update dependencies action.

The data is used for grading only. There is no research use of the recorded data.
Only the course's TAs and the instructor may review the data.
All recorded data is permanently deleted after the term's grades are released.

Rustrace has no server and does not upload a bundle. The student prepares a
local bundle and manually uploads it through Quercus. The access and
deletion statements above are course policy; the Rustrace client cannot enforce
or verify copies, backups, or deletion on staff machines or in Quercus. The
bundle is not encrypted and can contain sensitive recorded source and activity;
institutional and local controls must protect its storage, transfer, access,
retention, and deletion.

## Update checks

Automatic update checks are on by default. When you run a validated
`rustrace work`, Rustrace makes at most one attempt per 24 hours, before
creating or resuming your recording session. It requests only
[latest.json](https://github.com/baochunli/rustrace/releases/latest/download/latest.json).

The GET itself and ordinary IP/request metadata go to GitHub.

No assignment content, student ID, tool output, provenance, or telemetry is sent.
An automatic request has a one-second total deadline and no retries; a failed
request leaves the cached status usable and does not block your work.
`rustrace update --check` requests the same metadata explicitly, with a
20-second bound, even when automatic checks are off. `rustrace update` makes
the same explicit metadata request before deciding whether to compile a source
update, including when it can only print a manual installation remedy. Cargo
contacts the source repository and dependency endpoints to build an update;
this can take a few minutes. These explicit operations send no assignment data
or telemetry. The successful update rewrites the local installation receipt;
that receipt and update status stay outside assignment provenance.

Rustrace keeps the preference, attempt/success timestamps, next eligible time,
and validated release identity in
`$XDG_STATE_HOME/rustrace/update-state.json`, or
`~/.local/state/rustrace/update-state.json` when `XDG_STATE_HOME` is unset.
Keep this state directory outside your assignments. Update state is separate
from `config.toml`; it does not enter provenance, the journal or submission.

Choose Automatic checks: On/Off in the F7 menu to persist the preference for
the next launch. It is saved immediately; update settings are not in
`config.toml`.

Changing the preference makes no request. The Update Rustrace menu panel reads
only cached state and the installation receipt; it never checks or installs
during a session.
Help, `--version`, replay, status, submit, verify, scan, privacy, and ordinary
`doctor` make no update request; `doctor` reports only cached release status.

## What is recorded

Every event envelope carries a format version, session ID, one-based sequence,
monotonic milliseconds, an optional UTC wall-clock field, and chain hashes. The
monotonic value orders events and measures time only within one attempt. The
wall-clock field is contextual only and is never trusted as elapsed time; gaps
between attempts are unknown. The current production session and finalizer set
the optional wall-clock field to `null`, so they do not currently collect event
wall-clock timestamps. The schema and current writes are in
[`crates/model/src/event.rs`](../crates/model/src/event.rs) and
[`src/session.rs`](../src/session.rs).

The complete version 1 event vocabulary is:

- Controlled tool execution: `controlled_command_started`,
  `controlled_command_output`, and `controlled_command_finished` record the
  selected action, exact bounded argument vector, selected toolchain and tool
  versions, before/after workspace links, resource limits, output bytes, and
  outcome. The environment summary records only which fixed environment names
  are retained and never their values. Console routing is also recorded when
  present. Console Test filters and output-option spellings are part of the
  recorded argument vector. `--no-capture` and its `--nocapture` alias disable
  the Rust test harness's capture, not Rustrace's recording; `--show-output`
  exposes successful-test output after tests finish. Rustrace records output
  emitted by the process, not prints kept inside the harness. Stdout and stderr
  share an 8 MiB per-command cap within a 64 MiB session budget, or lower
  deployment budgets and the remaining session allowance. These flags do not
  change output limits, deadlines, or cancellation. The live console's 256 KiB
  tail is separate from recorded output. See
  [`crates/model/src/command.rs`](../crates/model/src/command.rs).
- Test-case comparison: `test_case_compared` records the command ID, case name, expected BLAKE3 digest, optional actual BLAKE3 digest, and typed outcome. The
  outcome is pass, mismatch with a one-based line and expected/actual line-byte
  lengths, or error with a closed reason. No raw test input, expected output, or actual output bytes are stored in this event. Actual stdout remains in the bounded controlled-command output records described above. See
  [`crates/model/src/command.rs`](../crates/model/src/command.rs).
  Its exact retained fields are `command_id`, `case`, `expected_blake3`,
  `actual_blake3`, and `outcome`. `actual_blake3` is `null` when stdout capture
  is unavailable. `case` is 1–64 ASCII bytes using letters, digits, `-`, or `_`.
  Mismatch lines are positive and at most 1,048,577; expected line lengths are
  at most 1 MiB and actual line lengths at most 8 MiB, excluding LF. Error
  reasons are exactly `launch_failed`, `nonzero_exit`, `terminated`,
  `capture_truncated`, `capture_unavailable`, `capture_read_failed`,
  `expected_unreadable`, or `expected_oversized`.
  Preceding LF bytes plus the expected line length must also fit the 1 MiB
  expected-file limit.
- Session lifecycle: `session_started`, `session_resumed`, and `session_ended`
  record the client/starter identity, resume position, and final workspace hash.
  See [`crates/model/src/event.rs`](../crates/model/src/event.rs).
- Files and editor history: `file_created` records the path and initial text;
  `file_deleted` records the document, path, and previous hash;
  `file_renamed` and `file_focused` record their document/path facts; and
  `file_edited` records each transaction's origin, inserted text, replacement
  byte ranges, before/after selection, versions, and hashes. Deleted or replaced
  text remains reconstructible from the preceding recorded state, so source
  history includes deleted code even though an edit stores the inserted text
  rather than duplicating the removed text. See
  [`crates/model/src/event.rs`](../crates/model/src/event.rs).
  Format uses the `Formatter` origin. Successful dependency actions use the
  `DependencyTool` origin exactly once per changed manifest or lockfile
  document; replay accepts it only within the matching controlled Add, Remove
  or Update command.
- Internal clipboard policy: `clipboard_copied` records a source event,
  document/path/version/hash, and selected byte range; `internal_paste` records
  that source link and the resulting editor transaction; `paste_rejected`
  records only one fixed reason and observed input channel. A blocked attempt
  stores no text, text hash, size, preview, or application attribution. Internal
  source bytes are already part of recorded workspace history and are derived
  during replay. After a successful internal copy or cut, Rustrace mirrors those
  exact source bytes to the system clipboard with a one-way terminal write. It
  never queries or reads the OS clipboard. See
  [`crates/model/src/event.rs`](../crates/model/src/event.rs).
- TUI state: `selection_changed` records document and anchor/active byte
  offsets, whether the selection came from keyboard or mouse navigation.
  `viewport_changed` remains part of the version 1 schema vocabulary, but the
  production TUI does not emit it: wheel, scrollbar, and other viewport-only
  scrolling are not provenance. These are document positions, not pointer
  coordinates or screen images. See
  [`crates/model/src/event.rs`](../crates/model/src/event.rs).
- Legacy Cargo evidence: `cargo_command_started` records the program, argument
  vector, and workspace-relative working directory; `cargo_diagnostic` records
  severity, the required message and optional code, document, and range;
  `cargo_output` records stdout or stderr text; and `cargo_command_finished`
  records exit code and success.
  See [`crates/model/src/event.rs`](../crates/model/src/event.rs).
- Editor-assisted changes: every automatic completion request after a typing
  pause and every explicit Ctrl-Space request records
  `lsp_completion_requested` with the document, version, and cursor.
  `lsp_completion_accepted` records its label and text edits; showing,
  navigating, or dismissing the popup records nothing. `lsp_code_action_applied`
  records the title, optional kind, and affected documents. The resulting source
  mutation is separately recorded as an editor transaction. See
  [`crates/model/src/event.rs`](../crates/model/src/event.rs).
- Checkpoint and finalization facts: `workspace_checkpoint` binds document and
  workspace hashes to a complete checkpoint payload, and
  `submission_finalized` records the final workspace hash, event count, clean
  state, and warnings. See
  [`crates/model/src/event.rs`](../crates/model/src/event.rs).
- Disk-change and recovery evidence: `external_file_change` records the
  workspace path and available before/after contents and hashes;
  `external_observation` binds saved/logical/observed hashes to a separately
  retained bounded evidence artifact; and `recovery_recorded` binds that
  artifact to the selected recovery decision. This disk-change evidence is
  a separate recorded category and is never populated with rejected clipboard
  content. See
  [`crates/model/src/event.rs`](../crates/model/src/event.rs).

The package also carries the student-entered student ID, course/assignment
identity and assignment manifest, the optional version 2 packaged-suite hash,
original starter files, exact event streams, full checkpoints, runtime metadata,
and referenced recovery evidence. Runtime
metadata includes the selected toolchain, probe commands, bounded probe output,
statuses, and remediation text. Its schema is in
[`src/toolchain.rs`](../src/toolchain.rs). Rustrace validates the package
inventory and byte totals against fixed version 1 format limits.

Each revised bundle is self-contained: it includes all prior recorded attempts
from the original starter in order, including reconstructible deleted code,
tool output, full checkpoints, runtime metadata, and required recovery evidence.
It does not nest earlier ZIP files.

## What is not recorded

Rustrace does not capture webcam video, microphone audio, screenshots, screen
video, window contents, or general activity outside the assignment TUI. It does
not monitor other applications or browser activity. It does not read or retain
OS clipboard contents from other applications. It writes only the
already-recorded selection from a successful internal copy or cut. A rejected
paste records only the bounded metadata named above. It does not record raw
key-down/key-up events, key pressure, dwell/flight measurements, or a
typing-biometric profile. Ordinary editor transactions still have event-level
monotonic times for ordering and within-attempt replay.
It also does not record registry traffic, network destinations, Cargo
credentials, or fetched dependency cache contents.

Course policy permits an external debugger on artifacts built by Rustrace;
there is no integrated debugger. Keep source edits inside Rustrace, finish the
Rustrace build before debugging its artifact, and avoid concurrent rebuilding.
Stop the debugger and its running program before rebuilding or submitting.
External debugger commands and output are not recorded. Program execution
under the debugger can still write files; permission to debug does not imply
a read-only run or recorded execution history. The
[student debugger workflow](student-guide.md#use-an-external-debugger) explains
which workspace artifacts to use.

Disk changes to assignment files can be noticed when the TUI reconciles its
workspace. Rustrace records the observed file facts and bounded recovery
evidence, not the outside application or person that caused the change.

## What `rustrace privacy` reports

For a finalized workspace, `rustrace privacy WORKSPACE` validates the immutable
finalization receipt and derives its report from receipt-bound payloads. It
shows student, course, assignment, starter, and latest-session identity; ordered
attempt IDs, counts, and segment-local time spans; initial workspace and
assignment-manifest sizes; exact event counts by every kind above and encoded
event bytes; inserted and deleted Unicode-scalar/text-byte totals; command,
output, diagnostic, external-observation, blocked-paste, checkpoint, runtime
metadata, and recovery-evidence counts and byte totals.

For an unfinished workspace, the command labels the result as a preview and
derives the same categories from the verified durable journal/checkpoint prefix.
It takes cooperative inspection ownership and reads a bounded private scratch
copy of SQLite journal files so the live WAL sidecars are not changed. The
implementation is
[`src/session/privacy.rs`](../src/session/privacy.rs).

The report describes what a clean finalization would carry; finalizing an
unfinished attempt adds a final checkpoint and terminal event. It does not
claim an LMS upload or submission time.

## Retention, local cleanup, and linked attempts

While an attempt remains linked to a prior attempt, its ancestor journals,
checkpoints, captures, metadata, evidence, and receipts are required to create a
future clean self-contained bundle. Normal
`rustrace cleanup WORKSPACE --confirm` preserves provenance. The explicit
`rustrace cleanup WORKSPACE --destroy-provenance --confirm` mode destroys the
workspace's local provenance; doing so can prevent a linked descendant from
creating a clean complete-history export. The implemented checks and deletion
surface are in
[`src/session/retention.rs`](../src/session/retention.rs).

Students may remove local copies by using that explicit destructive cleanup and
by deleting their workspace and generated bundle files. This does not delete a
copy already uploaded or transferred elsewhere.

After the term's grades are released, course staff will permanently delete all
recorded data submitted to the course website under the course policy.
