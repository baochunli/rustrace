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

## Follow-up after the merged-tree failure

The merged-tree integration run later reported `console_session` as 7 passed /
1 failed, but its raw output and retained fixture were unavailable, including
the failing test name. The original implementer resumed from corrected base
`1dd8546` and could not reproduce the failure.

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
and all pre-existing control parents passed in every run, so the missing
original failure cannot be classified as feature-specific or pre-existing.
No production or test change was justified.

Raw follow-up logs remain retained at:

- `.t3-tmp/console-session-followup-1.log`
- `.t3-tmp/integration-command-followup.log`
- `.t3-tmp/console-session-repeat-10.log`
