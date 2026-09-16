"""Exercise the real production paste decoder before every modal/focus route."""
import fcntl
import io
import json
import os
import pty
import select
import signal
import sqlite3
import struct
import subprocess
import sys
import tarfile
import tempfile
import termios
import time

from pty_screen import rendered_screen

from test_home import isolate
isolate()


MANIFEST = b'''format_version = 1
course_id = "course"
assignment_id = "clipboard"
assignment_version = "v1"
title = "Clipboard ingress"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["*.rs", "Cargo.toml"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
'''
SENTINEL = b"REJECTED_CLIPBOARD_PTY_SENTINEL"


def package_at(root, include_src=False):
    package = os.path.join(root, "assignment.rta")
    buffer = io.BytesIO()
    manifest = MANIFEST
    sources = [
        ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
        ("starter/a.rs", b"A"),
        ("starter/b.rs", b"B"),
    ]
    if include_src:
        manifest = manifest.replace(
            b'allowed_paths = ["*.rs", "Cargo.toml"]',
            b'allowed_paths = ["*.rs", "src/*.rs", "Cargo.toml"]',
        )
        sources.append(("starter/src/lib.rs", b"// src fixture\n"))
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in [("assignment.toml", manifest), *sources]:
            info = tarfile.TarInfo(name)
            info.size = len(contents)
            info.mode = 0o600
            archive.addfile(info, io.BytesIO(contents))
    data = buffer.getvalue()
    while data.endswith(bytes(512)):
        data = data[:-512]
    with open(package, "wb") as file:
        file.write(data + bytes(1024))
    return package


def exercise(binary, mode, setup, incoming=None):
    internal_flow = mode in ("copy-delete-paste", "cut-delete-paste")
    with tempfile.TemporaryDirectory(prefix="rustrace-clipboard-pty-") as root:
        package = package_at(root)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 140, 0, 0))
        before = termios.tcgetattr(slave)

        def child():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        wrapper = (
            "import subprocess,termios,sys; before=termios.tcgetattr(0); "
            "r=subprocess.run(sys.argv[1:]); "
            "assert termios.tcgetattr(0)==before, 'raw mode leaked'; "
            "print('TERMINAL_RESTORED', flush=True); sys.exit(r.returncode)"
        )
        proc = subprocess.Popen(
            [sys.executable, "-c", wrapper, binary, "work", package],
            stdin=slave, stdout=slave, stderr=slave, preexec_fn=child,
            env={**os.environ, "TERM": "xterm-256color"}, cwd=root,
        )
        transcript = bytearray()
        keyboard_probe_answered = False

        def drain_until(predicate, seconds=10):
            nonlocal keyboard_probe_answered
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                if select.select([master], [], [], 0.025)[0]:
                    transcript.extend(os.read(master, 65536))
                if not keyboard_probe_answered and b"\x1b[?u\x1b[c" in transcript:
                    os.write(master, b"\x1b[?1;0c")
                    keyboard_probe_answered = True
                assert len(transcript) <= 2 * 1024 * 1024, "terminal output exceeded bound"
                if predicate():
                    return
            raise AssertionError(mode + ": timed out waiting for production UI")

        def settle():
            until = time.monotonic() + 0.15
            drain_until(lambda: time.monotonic() >= until, 2)

        try:
            drain_until(lambda: " files" in rendered_screen(transcript))
            os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.
            settle()
            for keys in setup:
                os.write(master, keys)
                settle()
            route = mode.removesuffix("-internal")
            markers = {
                "search": "find and replace", "create": "new file",
                "rename": "rename file", "files-menu": "new file…",
                "confirmation": "Delete a.rs?",
            }
            if route in markers:
                screen = rendered_screen(transcript)
                assert markers[route] in screen, \
                    mode + ": fixture did not reach intended route\n" + screen
            os.write(master, incoming or b"\x1b[200~" + SENTINEL + b"\x1b[201~")
            settle()
            warned = "Paste blocked:" in rendered_screen(transcript)
            leaked = b"REJECTED_CLIPBOARD" in transcript
            if internal_flow:
                for keys in [b"\x1a", b"\x19"]:  # undo, redo; autosave is command-silent
                    os.write(master, keys)
                    settle()
                work = os.path.join(root, "assignment.work")
                drain_until(
                    lambda: open(os.path.join(work, "b.rs"), "rb").read() == b"A",
                    10,
                )
            exit_keys = b"n\x11y" if route == "confirmation" else b"\x11"
            if route in ("search", "files-menu"):
                os.write(master, b"\x1b")
                settle()
            os.write(master, exit_keys)
            drain_until(lambda: proc.poll() is not None)
            settle()
            assert proc.returncode == 0, mode + ": production CLI failed"
            assert b"TERMINAL_RESTORED" in transcript, mode + ": raw mode leaked"
            assert b"\x1b[?2004l" in transcript and b"\x1b[?1049l" in transcript
            assert warned != internal_flow, mode + ": incorrect paste policy warning"
            assert not leaked, mode + ": rejected content leaked to terminal"
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait()
            try:
                termios.tcsetattr(slave, termios.TCSANOW, before)
            except termios.error:
                pass  # The child terminal guard is checked above before hangup.
            os.close(master)
            os.close(slave)

        work = os.path.join(root, "assignment.work")
        state = os.path.join(work, ".rustrace")
        with open(os.path.join(state, "session.json"), "rb") as file:
            metadata = json.load(file)
        connection = sqlite3.connect(os.path.join(state, metadata["session_id"] + ".sqlite"))
        rows = connection.execute("SELECT payload FROM events ORDER BY sequence").fetchall()
        connection.close()
        attempts = []
        events = []
        for (raw,) in rows:
            assert SENTINEL not in raw, mode + ": rejected content leaked to journal"
            event = json.loads(raw)
            events.append(event)
            if event["event"]["type"] == "paste_rejected":
                attempts.append(event)
                assert len(raw) <= 1024
                assert set(event["event"]["payload"]) == {"reason", "channel"}
        assert len(attempts) == (0 if internal_flow else 1), mode + ": incorrect rejection metadata"
        if internal_flow:
            copies = [event for event in events if event["event"]["type"] == "clipboard_copied"]
            pastes = [event for event in events if event["event"]["type"] == "internal_paste"]
            assert len(copies) == len(pastes) == 1, mode + ": exact copy/paste evidence missing"
            assert pastes[0]["event"]["payload"]["source"] == {
                "session_id": copies[0]["session_id"],
                "sequence": copies[0]["sequence"],
                "event_hash": copies[0]["event_hash"],
            }
        inspect = subprocess.run(
            [binary, "work", package, "--inspect"], capture_output=True, timeout=20
        )
        assert inspect.returncode == 0, mode + ": persisted replay failed"
        assert SENTINEL not in inspect.stdout + inspect.stderr
        expected_files = [("b.rs", b"A")] if internal_flow else [("a.rs", b"A"), ("b.rs", b"B")]
        expected_files.insert(0, ("Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'))
        assert sorted(name for name in os.listdir(work) if name != ".rustrace") == [name for name, _ in expected_files]
        for name, expected in expected_files:
            with open(os.path.join(work, name), "rb") as file:
                assert file.read() == expected, mode + ": rejection changed disk"
        for name in os.listdir(state):
            with open(os.path.join(state, name), "rb") as file:
                # Bounded chunks also cover the reserved file without allocating it.
                tail = b""
                while chunk := file.read(65536):
                    combined = tail + chunk
                    assert SENTINEL not in combined, mode + ": rejected content retained in artifact"
                    tail = combined[-len(SENTINEL):]


if __name__ == "__main__":
    for cut in [False, True]:
        setup = [b"\x01", b"\x18" if cut else b"\x03"]
        if cut:
            setup += [b"\x1a", b"\x19"]  # cut undo/redo retains the pre-cut slot
        setup += [b"\x17"]  # delete source
        if cut:
            setup += [b"y"]  # confirm dirty source deletion
        setup += [b"\x01"]  # replace destination selection
        exercise(sys.argv[1], "cut-delete-paste" if cut else "copy-delete-paste", setup, b"\x16")
    cases = [
        ("editor", []),
        ("search", [b"\x06"]),
        ("create", [b"\x1b[<2;4;1M", b"\x1b[<0;5;5M"]),
        ("rename", [b"\x1b[<2;4;3M", b"\x1b[<0;5;5M"]),
        ("files-menu", [b"\x1b[<2;4;3M"]),
        ("confirmation", [b"!", b"\x17"]),
    ]
    for mode, setup in cases:
        exercise(sys.argv[1], mode, setup)
        if mode != "editor":
            # Even a valid internal slot cannot edit source behind another
            # focus/modal target. The shortcut must be warned and recorded.
            exercise(sys.argv[1], mode + "-internal", [b"\x01", b"\x03"] + setup, b"\x16")
    print("Production copy/cut/delete/paste/undo/redo/save/replay and all modal/focus rejection routes passed")
