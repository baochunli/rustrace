#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
toolchain=1.98.1
export RUSTUP_AUTO_INSTALL=0

rustc_identity=$(rustup run "$toolchain" rustc -vV)
target=$(printf '%s\n' "$rustc_identity" | awk '/^host: / { print $2 }')
case "$target" in
    aarch64-apple-darwin | x86_64-unknown-linux-gnu) ;;
    *)
        echo "unsupported release host target: ${target:-unknown}" >&2
        exit 1
        ;;
esac

cargo_target_dir=${CARGO_TARGET_DIR:-"$repository_root/target"}
artifact_dir="$repository_root/dist/$target"
mkdir -p "$artifact_dir"
rm -f \
    "$artifact_dir/rustrace" \
    "$artifact_dir/rustrace.sha256" \
    "$artifact_dir/toolchain.txt" \
    "$artifact_dir/version.txt"

rustup run "$toolchain" cargo build \
    --manifest-path "$repository_root/Cargo.toml" \
    --locked \
    --release \
    --target "$target" \
    --bin rustrace

install -m 0755 "$cargo_target_dir/$target/release/rustrace" \
    "$artifact_dir/rustrace"

{
    printf 'rustup: '
    rustup --version
    printf 'rustc -vV:\n%s\n' "$rustc_identity"
    printf 'cargo: '
    rustup run "$toolchain" cargo --version --verbose
} >"$artifact_dir/toolchain.txt"

"$artifact_dir/rustrace" --version --verbose >"$artifact_dir/version.txt"

if command -v sha256sum >/dev/null 2>&1; then
    checksum=$(sha256sum "$artifact_dir/rustrace" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    checksum=$(shasum -a 256 "$artifact_dir/rustrace" | awk '{print $1}')
else
    echo "no SHA-256 utility found (need sha256sum or shasum)" >&2
    exit 1
fi
printf '%s  rustrace\n' "$checksum" >"$artifact_dir/rustrace.sha256"

"$repository_root/scripts/verify-release.sh" "$artifact_dir"
echo "release artifacts written to $artifact_dir"
