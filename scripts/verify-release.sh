#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 DIST/TARGET" >&2
    exit 2
fi

artifact_dir=${1%/}
target=${artifact_dir##*/}
case "$target" in
    aarch64-apple-darwin | x86_64-unknown-linux-gnu) ;;
    *)
        echo "unsupported release target directory: $target" >&2
        exit 1
        ;;
esac

binary="$artifact_dir/rustrace"
checksum_file="$artifact_dir/rustrace.sha256"
toolchain_file="$artifact_dir/toolchain.txt"
version_file="$artifact_dir/version.txt"

for required_file in "$binary" "$checksum_file" "$toolchain_file" "$version_file"; do
    if [ ! -f "$required_file" ]; then
        echo "missing release artifact: $required_file" >&2
        exit 1
    fi
done
if [ ! -x "$binary" ]; then
    echo "release binary is not executable: $binary" >&2
    exit 1
fi

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        echo "no SHA-256 utility found (need sha256sum or shasum)" >&2
        exit 1
    fi
}

expected_checksum=$(awk 'NR == 1 { print $1 }' "$checksum_file")
expected_checksum_line="$expected_checksum  rustrace"
if [ "$(cat "$checksum_file")" != "$expected_checksum_line" ]; then
    echo "invalid checksum file format: $checksum_file" >&2
    exit 1
fi
actual_checksum=$(sha256 "$binary")
if [ "$actual_checksum" != "$expected_checksum" ]; then
    echo "release binary checksum mismatch" >&2
    exit 1
fi

if ! grep -Eq '^rustc 1\.98\.1( |$)' "$toolchain_file"; then
    echo "toolchain metadata does not report rustc 1.98.1" >&2
    exit 1
fi
if ! grep -Fqx "host: $target" "$toolchain_file"; then
    echo "toolchain metadata target does not match $target" >&2
    exit 1
fi

actual_version=$(mktemp "${TMPDIR:-/tmp}/rustrace-version.XXXXXX")
trap 'rm -f "$actual_version"' EXIT HUP INT TERM
"$binary" --version --verbose >"$actual_version"
if ! cmp -s "$actual_version" "$version_file"; then
    echo "recorded verbose version output does not match the binary" >&2
    exit 1
fi
for required_line in \
    "event format: 1" \
    "package format: 1" \
    "assignment format: 2" \
    "target: $target"
do
    if ! grep -Fqx "$required_line" "$version_file"; then
        echo "verbose version output is missing: $required_line" >&2
        exit 1
    fi
done

echo "verified release artifacts for $target"
