# CI Node.js 24 action upgrade

Reviewed implementation: `4ad3cd69cd3384aba7d29c0c757a497ec45d0d42`.
Base: `65de334d0dc1fc1094bc80a53a7095780d3af088`.

Independent Codex gpt-5.6-sol xhigh review passed with no findings above LOW.

- Official action metadata declares Node 20 for upload-artifact v4,
  setup-python v5, and download-artifact v5 (also v6).
- The selected v7 versions declare Node 24; existing checkout v6 does too.
- Artifact names, paths, archive defaults, matching, and merging are preserved.
- Python remains 3.12. Configured GitHub-hosted runners support the actions.
- No runtime-warning suppression flags were introduced.
- `actionlint .github/workflows/release.yml .github/workflows/install-smoke.yml`
  and `git diff --check` passed during implementation and independent review.

This version-only configuration change needs no new mirrored tests or Rust
build. GitHub-hosted workflow execution was not performed during review.

Upstream references: [upload-artifact](https://github.com/actions/upload-artifact/blob/v7/action.yml),
[download-artifact](https://github.com/actions/download-artifact/blob/v7/action.yml),
[setup-python](https://github.com/actions/setup-python/blob/v7/action.yml).
