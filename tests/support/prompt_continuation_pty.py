"""Actual CLI prompt editing/reentry after rejection; adapted from review4."""
import fcntl
import hashlib
import json
import os
import pathlib
import pty
import select
import shutil
import sqlite3
import struct
import subprocess
import sys
import tempfile
import termios
import time

sys.dont_write_bytecode = True
from clipboard_pty import package_at, rendered_screen
from pty_process import wait_for_pty_exit

from test_home import isolate
isolate()

binary = pathlib.Path(sys.argv[1]).resolve()
base = pathlib.Path(sys.argv[2]) if len(sys.argv) > 2 else pathlib.Path(
    tempfile.mkdtemp(prefix="rustrace-prompt-continuation-")
)
base.mkdir(parents=True, exist_ok=True)
# Preserve the actual first CLI before any test failure or subsequent rebuild.
archived = base / "rustrace-cli"
assert not archived.exists()
shutil.copy2(binary, archived)
shutil.copy2(__file__, base / "prompt_continuation_pty.py")
print(json.dumps(dict(evidence=str(base), binary=str(binary),
                      sha256=hashlib.sha256(archived.read_bytes()).hexdigest())), flush=True)
notice = ["External changes are not accepted.", "Rustrace preserves recovery evidence.",
          "Restoring current contents, including unsaved edits."]
layouts = [(external, 24, mode) for external in [False, True]
           for mode in ["search", "create", "rename"]]
layouts += [(False, 36, "search"), (True, 36, "search")]
results = []
for entry in [False, True]:
    for external, height, mode in layouts:
        case = f"entry-{entry}-p2-{external}-{height}-{mode}"
        root = base / case
        root.mkdir()
        package = package_at(str(root), include_src=True)
        master, slave = pty.openpty()
        width = 80 if height == 24 else 140
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))

        def child():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        wrapper = "import subprocess,termios,sys; before=termios.tcgetattr(0); r=subprocess.run(sys.argv[1:]); assert termios.tcgetattr(0)==before; print('TERMINAL_RESTORED',flush=True); sys.exit(r.returncode)"
        proc = subprocess.Popen([sys.executable, "-c", wrapper, str(archived), "work", package],
            stdin=slave, stdout=slave, stderr=slave, preexec_fn=child, cwd=root,
            env={**os.environ, "TERM": "xterm-256color", "RUSTUP_AUTO_INSTALL": "0"})
        data = bytearray()

        def process_snapshot(stage):
            rows = subprocess.check_output(
                ["ps", "-axo", "pid=,ppid=,pgid=,lstart=,comm="], text=True
            ).splitlines()
            owned = []
            for row in rows:
                fields = row.split()
                if int(fields[0]) == proc.pid or int(fields[1]) == proc.pid:
                    try:
                        sid = os.getsid(int(fields[0]))
                    except ProcessLookupError:
                        sid = None
                    owned.append(dict(process=row.strip(), sid=sid))
            (root / (stage + "-processes.json")).write_text(json.dumps(owned, indent=2))

        def pump(seconds):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                if select.select([master], [], [], 0.02)[0]:
                    data.extend(os.read(master, 65536))
                assert len(data) < 2 * 1024 * 1024

        def wait_for(text):
            deadline = time.monotonic() + 20
            while text not in rendered_screen(data) and time.monotonic() < deadline:
                pump(.05)
            assert text in rendered_screen(data), f"{case}: missing setup {text}"

        def send(keys):
            os.write(master, keys)
            pump(.25)

        def snapshot(name):
            screen = rendered_screen(data)
            (root / (name + ".txt")).write_text(screen)
            return screen

        started = time.monotonic()
        try:
            wait_for(" files")
            send(b"\x1b[<0;4;3M\x1b[<0;4;3m")
            process_snapshot("recording")
            work = root / "assignment.work"
            if external:
                (work / "a.rs").write_text("external C")
                send(b"\x13")
                wait_for(notice[0])
                activity = work / ".rustrace/command-activity.json"
                deadline = time.monotonic() + 20
                while (not activity.is_file()
                       or json.loads(activity.read_text())["active"]):
                    assert time.monotonic() < deadline, f"{case}: save Check stayed active"
                    pump(.05)
            opening = {
                "search": b"\x06",
                "create": b"\x1b[<2;4;1M\x1b[<0;5;5M",
                "rename": b"\x1b[<2;4;3M\x1b[<0;5;5M",
            }[mode]
            prompt = {"search": "find and replace", "create": "new file",
                       "rename": "rename file"}[mode]
            query = "A" if mode == "search" else "fresh.rs"
            send(opening)
            wait_for(prompt)
            if mode == "rename":
                send(b"\x7f" * len("a.rs"))
            send(query.encode())
            before = snapshot("before-rejection")
            assert prompt in before and query in before
            if entry:
                # Reenter from the original focus after cancelling a prior prompt.
                send(b"\x1b")
            send(b"\x1b[200~REVIEW5_REJECTED_PROMPT_SENTINEL\x1b[201~")
            warned = snapshot("after-rejection")
            assert "Paste blocked:" in warned
            assert not external or (" ERROR " in warned and "External" in warned)
            assert (work / "a.rs").read_bytes() == b"A"
            assert (work / "b.rs").read_bytes() == b"B"
            entry_visible = True
            if entry:
                send(opening)
                entry_visible = prompt in snapshot("after-reentry")
                if mode == "rename":
                    send(b"\x7f" * len("a.rs"))
                send(query.encode())
            send(b"x")
            typed = snapshot("after-typing")
            typed_query = f"src/{query}x▏" if mode == "create" else f" {query}x▏"
            typing_visible = prompt in typed and typed_query in typed
            send(b"\x7f")
            backed = snapshot("after-backspace")
            backed_query = f"src/{query}▏" if mode == "create" else f" {query}▏"
            backspace_visible = (
                prompt in backed and backed_query in backed and typed_query not in backed
            )
            assert not external or (" ERROR " in backed and "External" in backed)
            send(b"\r")
            submitted = snapshot("after-submit")
            if mode == "search":
                applied = "match found" in submitted
            elif mode == "create":
                applied = (work / "src/fresh.rs").is_file() and (work / "a.rs").read_bytes() == b"A"
            else:
                applied = (work / "fresh.rs").read_bytes() == b"A" and not (work / "a.rs").exists()
            assert applied, f"{case}: prompt submission did not execute"
            if mode == "search":
                send(b"\x1b")
            send(b"\x11")
            wait_for_pty_exit(proc, pump, 15)
            pump(.1)
            assert proc.returncode == 0 and b"TERMINAL_RESTORED" in data
            assert b"\x1b[?1049l" in data and b"\x1b[?2004l" in data
            state = work / ".rustrace"
            metadata = json.loads((state / "session.json").read_text())
            with sqlite3.connect(state / (metadata["session_id"] + ".sqlite")) as db:
                raw = [r[0] for r in db.execute("SELECT payload FROM events ORDER BY sequence")]
            rejected = [json.loads(r) for r in raw if json.loads(r)["event"]["type"] == "paste_rejected"]
            assert len(rejected) == 1
            assert b"REVIEW5_REJECTED_PROMPT_SENTINEL" not in data
            assert all(b"REVIEW5_REJECTED_PROMPT_SENTINEL" not in r for r in raw)
            record = dict(case=case, entry_visible=entry_visible, typing_visible=typing_visible,
                          backspace_visible=backspace_visible, submitted=applied, rejections=1,
                          terminal_restored=True, seconds=round(time.monotonic()-started, 3),
                          build=metadata["build_identity"])
            results.append(record)
            print(json.dumps(record), flush=True)
        finally:
            (root / "terminal.bin").write_bytes(data)
            if proc.poll() is None:
                process_snapshot("before-cleanup")
                send(b"\x11")
                wait_for_pty_exit(proc, pump, 15)
            os.close(master)
            os.close(slave)
(base / "results.json").write_text(json.dumps(results, indent=2) + "\n")
assert all(r["entry_visible"] and r["typing_visible"] and r["backspace_visible"]
           for r in results), "Ordinary prompt entry/edit remains hidden after paste rejection"
