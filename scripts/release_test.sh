#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
test_root=$(mktemp -d "${TMPDIR:-/tmp}/rustrace-release-test.XXXXXX")
trap 'rm -rf "$test_root"' EXIT HUP INT TERM

version=$(python3 - "$repository_root/Cargo.toml" <<'PYVERSION'
import sys
import tomllib
with open(sys.argv[1], "rb") as manifest:
    print(tomllib.load(manifest)["workspace"]["package"]["version"])
PYVERSION
)
target=aarch64-apple-darwin
artifact_dir="$test_root/$target"
mkdir -p "$artifact_dir"

write_fixture_binary() {
    cat >"$artifact_dir/rustrace" <<EOF
#!/bin/sh
printf '%s\n' \\
    'rustrace $version' \\
    'build commit: 0123456789abcdef' \\
    'event format: 1' \\
    'package format: 1' \\
    'assignment format: 2' \\
    'target: $target'
EOF
    chmod +x "$artifact_dir/rustrace"
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

write_fixture_binary
"$artifact_dir/rustrace" --version --verbose >"$artifact_dir/version.txt"
cat >"$artifact_dir/toolchain.txt" <<EOF
rustup: rustup 1.29.0
rustc -vV:
rustc 1.98.1 (fixture 2026-01-01)
host: $target
cargo: cargo 1.98.1 (fixture 2026-01-01)
EOF
printf '%s  rustrace\n' "$(sha256 "$artifact_dir/rustrace")" \
    >"$artifact_dir/rustrace.sha256"

"$repository_root/scripts/verify-release.sh" "$artifact_dir"

printf '\n# corruption\n' >>"$artifact_dir/rustrace"
if "$repository_root/scripts/verify-release.sh" "$artifact_dir" >/dev/null 2>&1; then
    echo "release verifier accepted a checksum mismatch" >&2
    exit 1
fi

write_fixture_binary
printf '%s  rustrace\n' "$(sha256 "$artifact_dir/rustrace")" \
    >"$artifact_dir/rustrace.sha256"
sed 's/target: aarch64-apple-darwin/target: x86_64-unknown-linux-gnu/' \
    "$artifact_dir/version.txt" >"$artifact_dir/version.invalid"
mv "$artifact_dir/version.invalid" "$artifact_dir/version.txt"
if "$repository_root/scripts/verify-release.sh" "$artifact_dir" >/dev/null 2>&1; then
    echo "release verifier accepted mismatched verbose version metadata" >&2
    exit 1
fi

echo "release verifier behavioral tests passed"
