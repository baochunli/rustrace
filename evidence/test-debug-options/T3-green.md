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
