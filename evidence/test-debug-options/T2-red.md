# T2 red regression evidence

Base: `fdb7197` (accepted T1 contract). The tests compile against existing public
APIs. Red results are missing behavior in the parser/preparer/session/model,
not missing helpers or malformed fixtures.

Formatting and whitespace checks completed successfully with:

```text
cargo fmt --all
git diff --check
```

## Root policy and production session

Command (exit 101, intentional):

```text
env TMPDIR=/private/tmp/rustrace-test-options.knhWFz/t2/.t2-tmp CARGO_TARGET_DIR=/private/tmp/rustrace-test-options.knhWFz/t2/.t2-target cargo test --no-fail-fast --test cargo_policy --test console_session
```

`cargo_policy`: 12 passed, 3 failed.

- `console_test_grammar_preserves_every_bounded_filter_and_output_form`: the
  first new form, `cargo test legal_moves`, returns `UnsupportedCommand`.
- `console_test_forms_prepare_the_exact_literal_tail_after_controller_locking`:
  parsing the first filtered Test form returns `UnsupportedCommand`, so exact
  prepared argv cannot yet be observed.
- `rejection_names_the_complete_student_allowlist_without_internal_ids`: the
  current help still advertises only bare `cargo test`, not the bounded syntax
  and three output options.

Passing controls include the existing bare/menu/action/environment behavior,
the full invalid console Test table, direct-construction rejection (wrong
action, malformed filters, extra args, and redirection), and the unchanged
instructor-command restriction.

`console_session`: 7 passed, 1 failed.

- `production_console_test_options_parent`: its child fails at
  `start_console_command(...).unwrap()` because the production parser rejects
  `cargo test tests::legal_moves -- --show-output` before launch.

All existing console/session parent tests pass. The new child compiles and the
failure is the unsupported command, before its assertions for completion,
Closed/Console route, exact recorded argv, output, and finish evidence.

## Model event validation

Command (exit 101, intentional):

```text
env TMPDIR=/private/tmp/rustrace-test-options.knhWFz/t2/.t2-tmp CARGO_TARGET_DIR=/private/tmp/rustrace-test-options.knhWFz/t2/.t2-target cargo test -p rustrace-model --test controlled_command
```

Result: 20 passed, 1 failed.

- `console_test_evidence_accepts_and_round_trips_every_canonical_tail`: the
  first canonical new recorded tail, `["--locked", "legal_moves"]`, is rejected
  as `invalid controlled command evidence: literal non-Run console route`.

Passing controls include historical Test argv (`--frozen`, structured
`--message-format=json --locked`, and natural `--locked`), malformed-tail and
257-byte-filter rejection, Closed/Console route restrictions, rejection of Test
filters/options on Build/Check/Run/Clippy/Doc, and all prior model regressions.

## Retained task artifacts

- `.t2-target/` (about 1.4 GiB)
- `.t2-tmp/` (about 257 MiB), including the intentionally retained red session
  fixtures `rustrace-console-test-options-30450/`,
  `rustrace-console-test-options-30992/`, and
  `rustrace-console-test-options-32781/`, and
  `rustrace-console-test-options-33858/`

These remain in the T2 checkout for root-orchestrator validation and cleanup.
