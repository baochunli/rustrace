# Rustrace

Rustrace is a local-first Rust assignment environment that records editor and
tool activity so you can create a reviewable coursework submission. It runs on
your computer and does not upload your work.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/baochunli/rustrace/main/scripts/install.sh | sh
```

Install rustup and native compiler/linker tools first. The installer builds
from source with Rust 1.98.1, installing that toolchain if absent; compilation
takes a few minutes. If prompted, open a new terminal before running Rustrace.

Cargo alternative (replace `vX.Y.Z` with the latest release tag from
[GitHub Releases](https://github.com/baochunli/rustrace/releases)):

```sh
cargo +1.98.1 install --git https://github.com/baochunli/rustrace --tag vX.Y.Z rustrace --locked
```

Or, from a checked-out repository:

```sh
cargo install --path . --locked
```

Read the [student guide](docs/student-guide.md) to get started and the
[installation guide](docs/installation.md) for supported setup options.

Quit Rustrace, then run `rustrace update` to build the latest release. Restart
Rustrace after success. `rustrace update --check` checks without compiling.
Automatic daily checks are on by default; choose Automatic checks: On/Off in
the F7 menu to change the preference. See [Privacy](docs/privacy.md#update-checks)
for the GitHub metadata request and local state; no assignment data is sent.

To uninstall, run `cargo uninstall rustrace`; see the installation guide for
custom roots, receipt removal, and the optional PATH cleanup.

Licensed under the [MIT License](LICENSE).
