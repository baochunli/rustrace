#!/bin/sh
set -eu
if [ "$#" -ne 1 ]; then
    echo "usage: $0 vX.Y.Z" >&2
    exit 2
fi
repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
# Test/support override; production defaults to the root workspace manifest.
cargo_manifest=${RUSTRACE_CARGO_MANIFEST:-"$repository_root/Cargo.toml"}
python3 - "$cargo_manifest" "$1" <<'PY'
import pathlib
import re
import sys
import tomllib

version = tomllib.loads(pathlib.Path(sys.argv[1]).read_text())["workspace"]["package"]["version"]
tag = sys.argv[2]
if not re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag) or tag != "v" + version:
    sys.exit(f"release tag {tag!r} must be the stable workspace version v{version}")
print(f"verified release tag {tag}")
PY
