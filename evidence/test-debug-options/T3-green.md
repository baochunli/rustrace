# T3 implementation evidence

Base: `d9d4e21` (accepted T1/T2/T5 merged base).

## Red baseline before production edits

Using task-owned `.t3-target/` and `.t3-tmp/`:

```text
env TMPDIR="$PWD/.t3-tmp" CARGO_TARGET_DIR="$PWD/.t3-target" cargo test --no-fail-fast --test cargo_policy --test console_session
```

Exit 101, expected: `cargo_policy` 12 passed / 3 failed and
`console_session` 7 passed / 1 failed. The failures were the accepted T2 gaps:
unsupported bounded Test forms, old help text, and the production parser
rejecting the Test tail before launch.

```text
env TMPDIR="$PWD/.t3-tmp" CARGO_TARGET_DIR="$PWD/.t3-target" cargo test -p rustrace-model --test controlled_command
```

Exit 101, expected: 20 passed / 1 failed. The new canonical recorded Test tail
was rejected as `literal non-Run console route`.

## Green validation

```text
cargo fmt --all
```

Exit 0.

```text
env TMPDIR="$PWD/.t3-tmp" CARGO_TARGET_DIR="$PWD/.t3-target" cargo test --no-fail-fast --test cargo_policy --test console_session
```

Exit 0: `cargo_policy` 15 passed and `console_session` 8 passed.

```text
env TMPDIR="$PWD/.t3-tmp" CARGO_TARGET_DIR="$PWD/.t3-target" cargo test -p rustrace-model --test controlled_command
```

Exit 0: 21 passed.

```text
env TMPDIR="$PWD/.t3-tmp" CARGO_TARGET_DIR="$PWD/.t3-target" cargo check --workspace --all-targets
```

Exit 0. No schema, `.rta`, version, dependency, configuration, or test changes
were made. Task-owned build and temporary artifacts remain retained for
integration validation.

## First follow-up without retained failure output

The merged-tree integration run later reported `console_session` as 7 passed /
1 failed, but at that time its raw output and retained fixture were unavailable,
including the failing test name. The original implementer resumed from
corrected base `1dd8546` and could not reproduce the failure.

One fresh task-owned run passed all 8 tests:

```text
env TMPDIR="$PWD/.t3-tmp" CARGO_TARGET_DIR="$PWD/.t3-target" cargo test --test console_session -- --nocapture
```

The exact affected integration command then passed both targets:

```text
env TMPDIR="$PWD/.t3-tmp" CARGO_TARGET_DIR="$PWD/.t3-target" cargo test --no-fail-fast --test cargo_policy --test console_session
```

Result: `cargo_policy` 15 passed and `console_session` 8 passed.

A bounded repetition ran the console-session target 10 more times with default
test parallelism:

```text
for attempt in {1..10}; do
  env TMPDIR="$PWD/.t3-tmp" CARGO_TARGET_DIR="$PWD/.t3-target" cargo test -q --test console_session || exit $?
done
```

All 10 runs passed, for 80 passed / 0 failed. Combined with the fresh and exact
integration reruns, the target passed 12 consecutive times. The feature parent
and all pre-existing control parents passed in every run, so the missing failure
could not be classified at that point. Those observations alone did not justify
a production or test change.

Raw follow-up logs remain retained at:

- `.t3-tmp/console-session-followup-1.log`
- `.t3-tmp/integration-command-followup.log`
- `.t3-tmp/console-session-repeat-10.log`

## Diagnosed path-spelling assumption

A later merged-tree run at `073a557` retained
`/tmp/rustrace-test-options.knhWFz/t3-integration-root.log`. It identified
`production_console_test_options_parent` as the failure. The recorded argv
correctly preserved the rustup launcher path supplied through `PATH` with its
`/tmp` spelling, while the test incorrectly expected that element under the
canonicalized `/private/tmp` fixture root. The resolved Cargo path and the
remaining exact argv were already correct.

The original implementer reproduced the focused failure before editing:

```text
env TMPDIR=/tmp/rustrace-test-options.knhWFz/t3/.t3-tmp CARGO_TARGET_DIR=/tmp/rustrace-test-options.knhWFz/t3/.t3-target cargo test --test console_session production_console_test_options_parent -- --exact --nocapture
```

Exit 101: 0 passed / 1 failed at the exact argv assertion with only the rustup
`/tmp` versus `/private/tmp` spelling mismatch. Raw output is retained at
`.t3-tmp/console-session-tmp-spelling-red.log`.

The fixture now saves the supplied launcher path before canonicalizing the
workspace root. The assertion still compares the complete recorded argv
exactly; it expects the original launcher spelling and the separately resolved,
canonical Cargo path. Production recording and validation are unchanged.

After `cargo fmt --all`, the focused parent passed 1/1 with each spelling:

```text
env TMPDIR=/tmp/rustrace-test-options.knhWFz/t3/.t3-tmp CARGO_TARGET_DIR=/tmp/rustrace-test-options.knhWFz/t3/.t3-target cargo test --test console_session production_console_test_options_parent -- --exact --nocapture
env TMPDIR=/private/tmp/rustrace-test-options.knhWFz/t3/.t3-tmp CARGO_TARGET_DIR=/private/tmp/rustrace-test-options.knhWFz/t3/.t3-target cargo test --test console_session production_console_test_options_parent -- --exact --nocapture
```

The complete target also passed with the integration spelling:

```text
env TMPDIR=/tmp/rustrace-test-options.knhWFz/t3/.t3-tmp CARGO_TARGET_DIR=/tmp/rustrace-test-options.knhWFz/t3/.t3-target cargo test --test console_session
```

Result: 8 passed / 0 failed. Green logs remain retained at:

- `.t3-tmp/console-session-tmp-spelling-green.log`
- `.t3-tmp/console-session-private-tmp-spelling-green.log`
- `.t3-tmp/console-session-integration-spelling-green.log`
