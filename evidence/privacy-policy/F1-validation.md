# F1 validation

Task: F1, `depends_on: []`. Branch: `task/privacy-policy-f1`.
Base: `baab60261e2be5b5b854d5af9850ff8cc42e4775`.

## Root cause and correction

`tests/privacy_cli.rs:452` compares each `POLICY_LINES` entry with an exact
source line. `docs/privacy.md` joined the reviewer-access and permanent-deletion
sentences on one line. Both policies were present; this was source-formatting
drift, unrelated to nextest process isolation.

Replace the space between those sentences with one newline. All wording and the
rendered Markdown paragraph are retained: no blank line or hard break was added.
No tests, code, dependencies, or version files changed. The original checkout's
uncommitted 0.1.2 version bump was not touched.

## Commands and results

Commands ran from `/private/tmp/rustrace-privacy-fix.lg4PIj/f1`.
First create the local resource directories:

```sh
mkdir -p .f1-target .f1-tmp
```

Before editing the document:

```sh
CARGO_TARGET_DIR="$PWD/.f1-target" TMPDIR="$PWD/.f1-tmp" cargo nextest run --test privacy_cli privacy_document_tracks_the_complete_event_schema_and_fixed_policy > .f1-tmp/focused-red.log 2>&1
```

Exit 100: 1 failed, 0 passed, 5 skipped. The assertion at
`tests/privacy_cli.rs:452:9` reported:
`missing "Only the course's TAs and the instructor may review the data."`

After the one-newline correction:

```sh
CARGO_TARGET_DIR="$PWD/.f1-target" TMPDIR="$PWD/.f1-tmp" cargo nextest run --test privacy_cli privacy_document_tracks_the_complete_event_schema_and_fixed_policy > .f1-tmp/focused-green.log 2>&1
CARGO_TARGET_DIR="$PWD/.f1-target" TMPDIR="$PWD/.f1-tmp" cargo nextest run --test privacy_cli > .f1-tmp/privacy-cli.log 2>&1
git diff --check > .f1-tmp/whitespace.log 2>&1
git diff --cached --check >> .f1-tmp/whitespace.log 2>&1
```

Results:

- Focused test: exit 0; 1 passed, 5 skipped.
- Complete `privacy_cli` target: exit 0; 6 passed, 0 skipped.
- Working-tree and staged whitespace checks: exit 0, no output.

This validates the requested privacy target only, not the full nextest suite.
The existing tests remain unchanged; no formatting tests were added.

## Retained review resources

`.f1-target/` contains build artifacts. `.f1-tmp/` contains the three test logs,
their corresponding `.exit` files, the whitespace log, and the commit message.
These local resources are left in place and excluded from the commit.
Only `docs/privacy.md` and this evidence file are committed. No push or merge
is performed; integration and subsequent validation belong to the root task.

## Independent acceptance

Reviewer `rp-f1-review` (GPT-5.6 Sol, xhigh reasoning, through Herdr) reviewed
`3627d15400cb8ba39b76d1ff86ddec37ca904853` against
`baab60261e2be5b5b854d5af9850ff8cc42e4775`: PASS, no findings at any priority.
Policy wording/rendering and all tests remain unchanged; only the required
source-line layout is restored. The original checkout's uncommitted version
bump is outside this fix. The user requested direct integration into latest
main without a PR; root validates the integrated privacy suite before push.

## Main integration validation

After updating local main from the merged PR (`baab602`) and integrating the
reviewed fix at `0bb0638`, `cargo nextest run --test privacy_cli` passed all
6 tests (0 skipped), including the originally failing documentation test.
The user's uncommitted 0.1.2 Cargo.toml/Cargo.lock changes were preserved
byte-for-byte and were not staged or committed. Whitespace validation passed.
This records the affected target, not a rerun of the entire nextest suite.
