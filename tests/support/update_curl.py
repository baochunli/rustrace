"""Fake curl endpoint: record literal requests, serve local bytes, fail offline/hang."""
import hashlib
import json
import os
import pathlib
import sys
import time

root = pathlib.Path(__file__).parent
mode = (root / "mode").read_text()
request = {"argv": sys.argv[1:], "time": time.time(), "pid": os.getpid()}
with (root / "requests.jsonl").open("a") as log:
    log.write(json.dumps(request) + "\n")
assert not (root / "session-active").exists(), "request after session started"
if (root / "before-session.json").exists():
    expected = json.loads((root / "before-session.json").read_text())
    workspace = pathlib.Path(expected["workspace"])
    current = {str(p.relative_to(workspace)): hashlib.sha256(p.read_bytes()).hexdigest()
               for p in workspace.joinpath(".rustrace").rglob("*") if p.is_file()}
    assert current == expected["files"], "request after production session created/resumed"
if mode == "hang":
    time.sleep(5)
elif mode == "offline":
    print("fixture offline", file=sys.stderr)
    sys.exit(22)
else:
    sys.stdout.buffer.write((root / "latest.json").read_bytes())
