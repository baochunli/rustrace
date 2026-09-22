# T4 real Cargo integration validation

Current base: `c12cf48d92a2181565a39c981729770e7e2a3692`.
Behavioral-red base: `d9d4e21`.

## Scope

Added one dependency-free, format-v1 `.rta` integration fixture in
`tests/real_cargo_test_options.rs`. It runs only installed toolchain `1.98.1`
through `ProductionSession`; no production code, schema, dependency, package
format, version, tag, or release action changed.

The regression covers filter-only, bare/unfiltered, zero-match, all three
output spellings, and a controlled failing assertion. It checks live output,
replayed recorded bytes, exact executed argv, Closed/Console routing, normal
process exits, complete bounded capture, unchanged source/lockfile and `.rta`
bytes, finalization/export, reference verification, and replayed final source
and command history.

## Behavioral red

The final test file was copied unchanged to the task-owned detached checkout:

```text
git worktree add --detach /tmp/rustrace-test-options.knhWFz/t4-red d9d4e21
env TMPDIR=/tmp/rustrace-test-options.knhWFz/t4-red/.t4-tmp \
  CARGO_TARGET_DIR=/tmp/rustrace-test-options.knhWFz/t4-red/.t4-target \
  cargo test --test real_cargo_test_options -- \
  --exact real_cargo_console_test_options_verify_and_replay --nocapture
```

Expected exit 101 after successful compilation: 0 passed / 1 failed. The
first real ProductionSession command, `cargo test alpha_visible`, failed at
`start_console_command` with `UnsupportedCommand`. This is the pre-T3 behavior
gap, not a missing helper or compilation failure. Exact final-file equality
between the red and green checkout was checked with `cmp` before this run.

Raw qualifying log:
`/tmp/rustrace-test-options.knhWFz/t4-red/.t4-tmp/T4-red.log`.

A preliminary, non-qualifying authoring run found a test-only map-key type
mismatch before execution. It was corrected before the qualifying behavioral
red; its log remains at
`/tmp/rustrace-test-options.knhWFz/t4-red/.t4-tmp/T4-red-attempt1-compile.log`.

## Focused green

```text
env TMPDIR=/tmp/rustrace-test-options.knhWFz/t4/.t4-tmp \
  CARGO_TARGET_DIR=/tmp/rustrace-test-options.knhWFz/t4/.t4-target \
  cargo test --test real_cargo_test_options -- \
  --exact real_cargo_console_test_options_verify_and_replay --nocapture
```

Exit 0: 1 passed / 0 failed. The final tightened run completed in 3.27 seconds.
It executed seven real Cargo commands, finalized and exported the submission,
verified it against the original `.rta`, and replayed all command evidence.

Raw log: `.t4-tmp/T4-focused-green.log`.

## Compatibility, verification, and replay

```text
env TMPDIR=/tmp/rustrace-test-options.knhWFz/t4/.t4-tmp \
  CARGO_TARGET_DIR=/tmp/rustrace-test-options.knhWFz/t4/.t4-target \
  cargo test --no-fail-fast \
  --test cargo_policy --test console_session --test controlled_cargo \
  --test verify_cli --test replay_tui
```

Exit 0:

- `cargo_policy`: 15 passed
- `console_session`: 8 passed
- `controlled_cargo`: 2 passed
- `replay_tui`: 14 passed
- `verify_cli`: 21 passed

This preserves the existing menu, historical console, real-Cargo, verifier,
and replay checks rather than duplicating them. Raw log:
`.t4-tmp/T4-compatibility.log`.

```text
env TMPDIR=/tmp/rustrace-test-options.knhWFz/t4/.t4-tmp \
  CARGO_TARGET_DIR=/tmp/rustrace-test-options.knhWFz/t4/.t4-target \
  cargo test -p rustrace-replay --test controlled_command
```

Exit 0: 19 passed / 0 failed. Raw log:
`.t4-tmp/T4-replay-compatibility.log`.

## Formatting and compilation

```text
cargo fmt --all -- --check
env TMPDIR=/tmp/rustrace-test-options.knhWFz/t4/.t4-tmp \
  CARGO_TARGET_DIR=/tmp/rustrace-test-options.knhWFz/t4/.t4-target \
  cargo check --workspace --all-targets
git diff --check
```

All exited 0. The final assertion-only tightening was compiled by the focused
green run and followed by another successful formatting and whitespace check.
Logs: `.t4-tmp/T4-final-checks.log` and
`.t4-tmp/T4-final-post-tightening.log`.

## Retained task resources

- Current build cache: `/tmp/rustrace-test-options.knhWFz/t4/.t4-target/`
- Current logs/temp: `/tmp/rustrace-test-options.knhWFz/t4/.t4-tmp/`
- Detached red checkout: `/tmp/rustrace-test-options.knhWFz/t4-red/`
- Red build cache: `/tmp/rustrace-test-options.knhWFz/t4-red/.t4-target/`
- Red logs/temp: `/tmp/rustrace-test-options.knhWFz/t4-red/.t4-tmp/`

On this host `/tmp` resolves to `/private/tmp`; both spellings identify the
same retained resources. They are intentionally retained for orchestrator
review and cleanup after integration acceptance.

## Remaining risk

The test intentionally requires the installed pinned toolchain and fails if it
is unavailable. Its Cargo/libtest assertions use stable marker and result
substrings rather than progress ordering or full output text. Older readers
still reject the expanded Test argv as documented in the T1 contract; no
compatibility rule was weakened here.
