# Bounded console Test options: release readiness

Prepared from accepted T1–T5 at `3f9cd34`. This note is ready for maintainer
review; it does not announce a published release. The maintainer chooses and
records the first compatible release version, merges, and performs release
actions manually. No version bump, tag, publication, or deployment is part of
this documentation change.

## Release notes

The manual Cargo console accepts one optional test filter and one optional
output option:

```text
cargo test [FILTER] [-- OUTPUT_OPTION]
```

Brackets indicate optional groups. `FILTER` is one literal token of 1–256 ASCII
bytes from `[A-Za-z0-9_:-]`, with no leading `-`. It matches a substring of the
full test name, including module paths. A valid filter can match zero tests;
check the reported count. `OUTPUT_OPTION` is exactly one of `--nocapture`,
`--no-capture`, or `--show-output`, following the literal `--` separator and
appearing last. For example:

```text
cargo test
cargo test tests::legal_moves
cargo test -- --no-capture
cargo test tests::legal_moves -- --nocapture
cargo test tests::legal_moves -- --show-output
```

With Rust's standard test harness, `--no-capture` and its deprecated alias
`--nocapture` expose prints during execution; parallel output can interleave.
`--show-output` displays captured successful-test output after tests finish.
Behavior depends on the harness. These options do not disable Rustrace's
recording or change output budgets, deadlines, or cancellation. Rustrace
records emitted output and exact executed argv, preserving the option spelling
and inserting its own `--locked` before the filter and separator.

There is no general argument forwarding: multiple filters/options, Unicode
filters, `--exact`, other Cargo/libtest flags, and Test redirection remain
unsupported. Shell syntax remains rejected and the whole-line limit remains
4096 bytes. Bare console Test, menu Test, other actions, assignment command
policy, and packaged input/output cases keep their existing behavior. See the
[contract](../plans/test-debug-options.md) and
[student console guide](../student-guide.md#cargo-console-stdin-and-test-cases).

There is no integrated debugger. The approved
[external debugger workflow](../student-guide.md#use-an-external-debugger)
permits a separate debugger on artifacts already built by Rustrace. Keep source
edits inside Rustrace, finish the build first, disable debugger build steps,
and avoid concurrent Rustrace commands. Stop the debugger and its program
before saving source, rebuilding, or submitting. External debugger commands
and output are unrecorded; program execution can still write files. Later file
observations do not reconstruct that debugging session. See
[privacy boundaries](../privacy.md#what-is-not-recorded).

## Compatibility and reader-first rollout

Existing `.rta` packages remain unchanged: no repacking, migration, schema/event
version change, or capability metadata is required. Updated readers continue
to accept historical recordings and existing menu/console forms.

Older readers reject recordings containing the expanded Test argv because
their strict command validation does not recognize those forms, even though
the event structure is unchanged. This is a reader compatibility failure,
not evidence of corrupt student work. Do not strip arguments or weaken
verification to bypass it.

Staff must update every grader, verifier, and replay installation before
students can receive the writer update and submit new recordings. Inventory
the actual course installations; this note assumes no installation paths.
If staff need a candidate build before public release, use the reviewed source
revision and record it. Pushing a release tag triggers automatic publication,
so complete the reader gate before that step can expose the student update.

## Manual maintainer checklist

- [ ] **Maintainer:** obtain final PR/integration review, choose the first
  compatible release version, and add it to the release announcement. Merge
  manually after acceptance; this note does not claim merge or release completion.
- [ ] **Staff deployment owner:** inventory and upgrade all grader/verifier/replay
  installations, recording the installed version or candidate revision. Exercise
  a submission with an expanded Test command against its unchanged reference
  `.rta`; require verification and replay to preserve argv, output, and final
  source. Also check an older recording. Confirm the reader gate to the maintainer.
- [ ] **Maintainer:** review the committed validation below. If the release
  candidate changes executable behavior, rerun the affected checks below and
  record results; complete the normal checks in the
  [release procedure](../update-release.md) on the chosen candidate.
- [ ] **Maintainer:** after the reader gate, perform the chosen version bump,
  release checks, tag, and publication manually using that procedure. Publish
  these compatibility notes with the selected version. No such actions were
  performed for T6.
- [ ] **Course/student rollout owner:** distribute or announce the student update
  only after staff confirmation. Point students to the console and debugger
  guides above; check a representative new submission through deployed readers
  before relying on the feature for coursework.

## Validation and limits

The [T4 acceptance record](../../evidence/test-debug-options/T4.md) reports
**101 affected tests passing at `cf4b2b7`**: policy 15, console session 8,
controlled Cargo 2, new end-to-end 1, replay UI 14, verifier 21, model command
evidence 21, and replay command evidence 19. Workspace all-targets compilation,
formatting, and whitespace checks also passed. This is not a claim that the
full workspace test suite ran or that a release was published.

The [T2 red evidence](../../evidence/test-debug-options/T2-red.md) demonstrates
unsupported-command and recorded-argv failures through existing public APIs;
[T2 acceptance](../../evidence/test-debug-options/T2.md) and
[T3 green evidence](../../evidence/test-debug-options/T3-green.md) document
their resolution. The T3 integration issue was a test fixture's `/tmp` versus
`/private/tmp` launcher-path expectation. The fixture now preserves the supplied
launcher spelling while still asserting exact full argv; production behavior
was unchanged. [T3 acceptance](../../evidence/test-debug-options/T3.md) records
successful validation with the reproducing path spelling and closes the issue;
it is not a lingering flaky production failure.

[T4 red/green evidence](../../evidence/test-debug-options/T4-validation.md)
uses the identical final regression on the pre-implementation revision for a
behavioral red, then seven real Cargo commands through `ProductionSession` for
green. It covers filtering, zero matches, all three output spellings, successful
and failing tests, live/recorded output, exact argv/routes, bounded capture,
unchanged fixture source/lockfile/package bytes, and finalize/export/verify/replay.
The fixture requires installed Rust `1.98.1` and fails if unavailable. It does
not establish behavior for every harness, toolchain, or deployment.
[T1](../../evidence/test-debug-options/T1.md) and
[T5](../../evidence/test-debug-options/T5.md) record contract and student/privacy
documentation acceptance.

Commands for an affected-check rerun from the release candidate checkout
(use maintainer-selected build/temp directories if isolation is needed):

```sh
cargo test --no-fail-fast --test cargo_policy --test console_session --test controlled_cargo --test real_cargo_test_options --test replay_tui --test verify_cli
cargo test -p rustrace-model --test controlled_command
cargo test -p rustrace-replay --test controlled_command
cargo check --workspace --all-targets
cargo fmt --all -- --check
git diff --check
```

T6 is writing-only: inspect documentation links and `git diff --check`; no
prose-only tests are added and the recorded behavioral suites are not rerun.
