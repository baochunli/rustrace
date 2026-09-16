#!/bin/sh
# Read verbose metadata from host builds already checked by verify-release.sh.
# Write canonical JSON to stdout, or check a manifest against the same inputs.
set -eu

manifest_to_verify=
if [ "${1:-}" = --verify ]; then
    if [ "$#" -lt 4 ]; then
        echo "usage: $0 --verify LATEST.JSON vX.Y.Z DIST/TARGET..." >&2
        exit 2
    fi
    manifest_to_verify=$2
    shift 2
fi
if [ "$#" -lt 2 ]; then
    echo "usage: $0 vX.Y.Z DIST/TARGET..." >&2
    exit 2
fi
repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tag=$1
shift
RUSTRACE_CARGO_MANIFEST="$repository_root/Cargo.toml" \
    "$repository_root/scripts/check-release-tag.sh" "$tag" >/dev/null
commit=$(git -C "$repository_root" rev-parse HEAD)

python3 - "$repository_root/Cargo.toml" "$tag" "$commit" "$manifest_to_verify" "$@" <<'PY'
import json
import pathlib
import re
import sys
import tomllib


def generate():
    cargo_manifest, tag, commit, manifest_to_verify, *directories = sys.argv[1:]
    version = tomllib.loads(pathlib.Path(cargo_manifest).read_text())["workspace"]["package"]["version"]
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("release commit must be 40 lowercase hex digits")
    formats = None
    seen = set()
    for directory in directories:
        directory = pathlib.Path(directory)
        target = directory.name
        if target not in ("aarch64-apple-darwin", "x86_64-unknown-linux-gnu") or target in seen:
            raise ValueError(f"unsupported or repeated release target: {target}")
        seen.add(target)
        metadata = (directory / "version.txt").read_text()
        match = re.fullmatch(
            r"rustrace ([^\n]+)\nbuild commit: ([0-9a-f]{40})\n"
            r"event format: ([1-9][0-9]*)\npackage format: ([1-9][0-9]*)\n"
            r"assignment format: ([1-9][0-9]*)\ntarget: ([^\n]+)\n", metadata,
        )
        if not match:
            raise ValueError(f"invalid verbose version metadata: {directory}")
        found_version, found_commit, event, package, assignment, found_target = match.groups()
        if (found_version, found_commit, found_target) != (version, commit, target):
            raise ValueError(f"build metadata differs from Cargo version, Git commit or target: {directory}")
        found_formats = (int(event), int(package), int(assignment))
        if formats is not None and formats != found_formats:
            raise ValueError("release builds disagree on event, package or assignment formats")
        formats = found_formats
    event, package, assignment = formats
    data = {
        "schema_version": 1,
        "version": version,
        "tag": tag,
        "commit": commit,
        "event_format": event,
        "package_format": package,
        "assignment_format": assignment,
        "source": {"repository": "https://github.com/baochunli/rustrace", "tag": tag},
        "targets": {},
    }
    encoded = (json.dumps(data, sort_keys=True, indent=2) + "\n").encode()
    if manifest_to_verify:
        if pathlib.Path(manifest_to_verify).read_bytes() != encoded:
            raise ValueError("manifest differs from the canonical release identity")
        print(f"verified source manifest for {tag}")
    else:
        sys.stdout.buffer.write(encoded)


try:
    generate()
except (OSError, ValueError) as error:
    sys.exit(f"release manifest: {error}")
PY
