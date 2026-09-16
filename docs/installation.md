# Installing Rustrace

```sh
curl -fsSL https://raw.githubusercontent.com/baochunli/rustrace/main/scripts/install.sh | sh
```

The installer builds from source on Linux (including WSL 2) and macOS using
Rust 1.98.1. This takes a few minutes. The pilot does not distribute prebuilt
binaries. Release CI verifies macOS Apple Silicon and Linux x86-64.

## Prerequisites and installation

Install rustup 1.28.1 or newer and your system's native compiler/linker tools
first: Xcode Command Line Tools on macOS, or `build-essential` on Debian/Ubuntu.
Debian/Ubuntu also needs `pkg-config`; Fedora uses `gcc`, `gcc-c++`, `make`,
and `pkgconf-pkg-config`. Install these prerequisites with your platform's
package manager. Installation requires curl, Git, and dependency
network access. The installer never installs rustup; when it is missing, it prints the official command:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

If Rust 1.98.1 is missing, the installer announces and runs
`rustup toolchain install 1.98.1 --profile minimal --component clippy,rustfmt`.
It does not change your default toolchain, install rust-analyzer, or use sudo.
You can also run `sh scripts/install.sh` from a checkout; it still installs the
latest published release, rather than the checkout's current source.

The installer validates the release manifest, builds its immutable tag, checks
`rustrace --version --verbose`, and records the successful installation in
`$XDG_STATE_HOME/rustrace/install.json` (default
`~/.local/state/rustrace/install.json`). Cargo manages existing binary replacement.
Rerun the same one-liner to reinstall or install the latest release.

Cargo installs into `~/.cargo/bin` by default. Precedence: `RUSTRACE_INSTALL_DIR`
(passed as Cargo's `--root`), then `CARGO_INSTALL_ROOT`, then `install.root` in
`$CARGO_HOME/config.toml` (or legacy `config`), then `CARGO_HOME`, then
`~/.cargo`. The binary is placed in the root's `bin/` subdirectory. Use the
same root when uninstalling. Configured symlinks retain their logical paths
in receipts and PATH entries.

The POSIX installer reads a single-line, unescaped double-quoted `root = "…"`
inside `[install]` in the global Cargo config. If both filenames exist, Cargo's
legacy `config` takes precedence. Relative config roots resolve against the
parent of the Cargo home directory. For other TOML forms, set
`CARGO_INSTALL_ROOT` explicitly.

When that binary directory is missing from PATH, the installer appends a
marked block to `~/.zshrc`, or `~/.bashrc` and an existing `~/.bash_profile`.
Repeated installs with the same directory do not duplicate the entry. Changing
the install root adds a marked block for the new directory and reports it.
Fish and unknown shells receive a printed instruction. Open a new terminal,
then run `rustrace --version`.

On macOS, Linux, and WSL 2, no platform-specific first-launch steps are
needed for a locally built binary. Open a new terminal if the
installer changed PATH; confirm with `rustrace --version`.

## Cargo alternative

To install a specific release without the script:

```sh
cargo +1.98.1 install --git https://github.com/baochunli/rustrace --tag vX.Y.Z rustrace --locked
```

Replace `vX.Y.Z` with the latest release tag from
[GitHub Releases](https://github.com/baochunli/rustrace/releases). Each release tag
is immutable and matches the root Cargo version. This command builds locally
and needs Rust 1.98.1 installed first; ensure Cargo's binary directory is on PATH.

From a checkout of the release tag, the equivalent local installation is:

```sh
cargo install --path . --locked
```

The package name in the Git command is required because the repository also
contains executable fixture manifests. The root package ships exactly one
binary, `rustrace`, so no `--bin` flag is needed. Native Windows is not a pilot
target; use a supported Linux distribution under WSL 2 with projects stored in
its Linux filesystem.

Release metadata is available at
[latest.json](https://github.com/baochunli/rustrace/releases/latest/download/latest.json)
once a release is published. It identifies the source repository and tag; its
`targets` map is empty. `package_format` is the provenance container version
(`.rprov`, currently 1). `assignment_format` is the highest accepted assignment
package version (`.rta`, currently 2). `event_format` identifies the event
envelope version (currently 1). These fields match `rustrace --version --verbose`.
Together, these fields define the release manifest contract used by installation
and update checks.

## Uninstall

Run `cargo uninstall rustrace` (add `--root <install-root>` for a custom root),
then remove `$XDG_STATE_HOME/rustrace/install.json`, or
`~/.local/state/rustrace/install.json` when XDG_STATE_HOME is unset.
You may remove the marked Rustrace installer block from your shell startup
files. Your assignments and other Rustrace state are retained.

## Updating Rustrace

Quit Rustrace before updating. Run `rustrace update` from a terminal to
install the latest stable release. It makes a fresh release check and, when a
newer version exists, builds from source using Cargo and Rust 1.98.1. This
takes a few minutes; Cargo's progress streams to your terminal. After success,
it prints `Installed X.Y.Z. Restart Rustrace to use it.` Open Rustrace again to
use the new build. Sessions already running keep their recorded build identity.
A failed Cargo build leaves the previous executable unchanged.
If post-install identity validation fails, the new binary is already installed
at the reported path; follow the printed manual remedy to repair it.

The command uses the installer's `$XDG_STATE_HOME/rustrace/install.json`
receipt (default `~/.local/state/rustrace/install.json`) and installs into its
recorded Cargo root. An active workspace session prevents updating from that
workspace, with `Quit Rustrace before updating.`

Cargo-managed copies installed without that receipt, copies with an unknown
installation method, and executables moved away from their receipt path receive
a release-pinned remedy and exit without installing:

```sh
cargo +1.98.1 install --git https://github.com/baochunli/rustrace --tag vX.Y.Z rustrace --locked --force
```

The command prints the latest tag in place of `vX.Y.Z`. Use your original Cargo
install root for this manual remedy. From a checkout, reinstall with
`cargo install --path . --locked --force`.

Run `rustrace update --check` to check without compiling. Automatic daily checks
are on by default and finish before a validated work session starts. See
[Privacy](privacy.md#update-checks) for the request disclosure and menu toggle.

The F7 menu keeps Update dependencies (recorded Cargo dependency edits)
separate from Update Rustrace (cached release information and terminal
instructions). A newer cached release marks the latter `NEW` and the closed
menu footer `● menu`. Automatic checks: On/Off persists immediately for the
next launch; changing it makes no request. Update settings are not in
`config.toml`; the preference and cache live in
`$XDG_STATE_HOME/rustrace/update-state.json` (default
`~/.local/state/rustrace/update-state.json`), outside assignment provenance.
`rustrace doctor` gives a cache-only version advisory with the automatic check
state; an available update is a warning, never a blocker.

## Language server

rust-analyzer is installed separately rather than bundled with Rustrace:

```sh
rustup component add rust-analyzer
```

Language services also require the selected Rust toolchain and its `rust-src`
component:

```sh
rustup component add rust-src
```

The `doctor` command reports a missing rust-analyzer component as a warning,
not a blocker. Recording, editing, Cargo commands, and submission remain
available without language services. The Rust language-service components are
optional for those operations.

## Deferred targets and guarantees

Prebuilt binaries and native Windows support remain deferred. Other source-build
platforms are outside the pilot's supported environment.
Source distribution makes no byte-identical reproducible-build claim.
