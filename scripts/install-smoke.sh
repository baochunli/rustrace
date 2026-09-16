#!/bin/sh
# Exercise the installer using only scratch Rust/app homes and a local release.
# Run from a clean checkout. No Cargo or build cache is restored in CI.
set -eu
repository_root=$(pwd -P)
smoke_root=$repository_root/.tmp/install-smoke
[ ! -e "$smoke_root" ] || { echo "Smoke directory already exists: $smoke_root" >&2; exit 1; }
mkdir -p "$smoke_root/tools" "$smoke_root/home" "$smoke_root/target"
# setup-python supplies tomllib even on ubuntu-22.04. Keep just that executable
# from the incoming PATH, never the runner's Rust launchers or Cargo caches.
ln -s "$(command -v python3)" "$smoke_root/tools/python3"
HOME=$smoke_root/home
CARGO_HOME=$smoke_root/cargo
RUSTUP_HOME=$smoke_root/rustup
CARGO_TARGET_DIR=$smoke_root/target
XDG_CONFIG_HOME=$smoke_root/config
XDG_STATE_HOME=$smoke_root/state
PATH=$smoke_root/tools:/usr/bin:/bin:/usr/sbin:/sbin
export HOME CARGO_HOME RUSTUP_HOME CARGO_TARGET_DIR XDG_CONFIG_HOME XDG_STATE_HOME PATH
unset CARGO_INSTALL_ROOT RUSTRACE_INSTALL_DIR RUSTUP_TOOLCHAIN ENV BASH_ENV TMPDIR
if command -v rustup || command -v cargo || command -v rustc; then
    echo 'Preinstalled Rust is still on the smoke PATH.' >&2
    exit 1
fi
printf 'Fresh HOME: %s\nFresh Rust homes: %s %s\n' "$HOME" "$CARGO_HOME" "$RUSTUP_HOME"
start=$(date +%s)
# Documented rustup one-liner, with unattended flags: install the manager only.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --default-toolchain none
PATH=$CARGO_HOME/bin:$PATH
export PATH
export RUSTUP_AUTO_INSTALL=0

version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')
tag=v$version
commit=$(git rev-parse HEAD)
# Cargo --git --tag requires a ref. Never move an existing release tag.
if git show-ref --verify --quiet "refs/tags/$tag"; then
    [ "$(git rev-parse "$tag^{commit}")" = "$commit" ] || {
        echo "Local tag $tag points at a different commit." >&2; exit 1;
    }
else
    git tag "$tag" "$commit"
    trap 'git tag -d "$tag" >/dev/null' 0
fi
case $(uname -s) in
    Darwin) target=aarch64-apple-darwin;;
    Linux) target=x86_64-unknown-linux-gnu;;
    *) echo 'Unsupported smoke host.' >&2; exit 1;;
esac
# Seed fixture metadata from the checked-out source constants; this is not
# release build proof. Compare it to the real installed binary below.
metadata=$smoke_root/dist/$target
mkdir -p "$metadata"
python3 - "$metadata/version.txt" "$version" "$commit" "$target" <<'PY'
import pathlib
import re
import sys

def constant(path, name):
    return re.search(rf'pub const {name}: u32 = ([0-9]+);', pathlib.Path(path).read_text())[1]

path, version, commit, target = sys.argv[1:]
event = constant('crates/model/src/event.rs', 'FORMAT_VERSION_V1')
package = constant('crates/model/src/rprov.rs', 'RPROV_FORMAT_VERSION_V1')
assignment = constant('crates/model/src/assignment.rs', 'SUPPORTED_FORMAT_VERSION')
pathlib.Path(path).write_text(f'rustrace {version}\nbuild commit: {commit}\n'
                            f'event format: {event}\npackage format: {package}\n'
                            f'assignment format: {assignment}\ntarget: {target}\n')
PY
./scripts/manifest.sh "$tag" "$metadata" > "$smoke_root/latest.json"
cp "$smoke_root/latest.json" "$smoke_root/canonical.json"
RUSTRACE_SOURCE_REPOSITORY=$(python3 -c 'import pathlib; print(pathlib.Path.cwd().as_uri())')
RUSTRACE_MANIFEST_URL=$(python3 -c 'import pathlib, sys; print(pathlib.Path(sys.argv[1]).as_uri())' "$smoke_root/latest.json")
export RUSTRACE_SOURCE_REPOSITORY RUSTRACE_MANIFEST_URL
python3 - "$smoke_root/latest.json" "$RUSTRACE_SOURCE_REPOSITORY" <<'PY'
import json
import pathlib
import sys
path = pathlib.Path(sys.argv[1])
data = json.loads(path.read_text())
data['source']['repository'] = sys.argv[2]
path.write_text(json.dumps(data, sort_keys=True, indent=2) + '\n')
PY
install_start=$(date +%s)
./scripts/install.sh
install_end=$(date +%s)
rustrace --version --verbose > "$smoke_root/installed-version.txt"
cat "$smoke_root/installed-version.txt"
# Includes the checked-out commit, host target, and all three format versions.
cmp "$metadata/version.txt" "$smoke_root/installed-version.txt"
{
    printf '## Clean-machine source installation (%s)\n\n' "$(date -u +%F)"
    # shellcheck disable=SC2016 # Markdown backticks are literal.
    printf 'Commit: `%s`; version: `%s`; target: `%s`.\n\n' "$commit" "$version" "$target"
    printf 'Wall time (rustup + installer): %s seconds.\n' "$((install_end - start))"
    printf 'Installer wall time: %s seconds.\n\n' "$((install_end - install_start))"
    printf 'Disk use after installation (before test builds):\n\n```text\n'
    du -sh "$CARGO_HOME" "$CARGO_TARGET_DIR"
    printf '```\n\n'
} >> "${GITHUB_STEP_SUMMARY:-$smoke_root/summary.md}"
# update has a fixed public endpoint. Redirect only this request through real
# curl to the canonical local fixture; keep production URL validation intact.
cp "$smoke_root/canonical.json" "$smoke_root/latest.json"
SMOKE_REAL_CURL=$(command -v curl)
SMOKE_REQUEST_LOG=$smoke_root/update-request.txt
export SMOKE_REAL_CURL SMOKE_REQUEST_LOG
cat > "$smoke_root/tools/curl" <<'CURL'
#!/bin/sh
set -eu
for arg do last=$arg; done
[ "$last" = 'https://github.com/baochunli/rustrace/releases/latest/download/latest.json' ] || exit 91
printf '%s\n' "$last" > "$SMOKE_REQUEST_LOG"
exec "$SMOKE_REAL_CURL" --disable -fsSL --max-time 20 "$RUSTRACE_MANIFEST_URL"
CURL
chmod +x "$smoke_root/tools/curl"
rustrace update --check > "$smoke_root/update-check.txt"
cat "$smoke_root/update-check.txt"
grep -Fx "Rustrace $version is up to date." "$smoke_root/update-check.txt"
[ -s "$SMOKE_REQUEST_LOG" ]
rm "$smoke_root/tools/curl"
printf 'Fixture update check: up to date.\n' >> "${GITHUB_STEP_SUMMARY:-$smoke_root/summary.md}"
# Each selected implementation runs the complete suite, including adversarial
# fallback cases without Python on the installer PATH. Missing awk variants skip.
python3 tests/support/install_sh_tests.py SelectedAwkTests
