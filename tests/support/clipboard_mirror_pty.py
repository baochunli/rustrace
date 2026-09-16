#!/usr/bin/env python3
"""Exercise OSC 52 mirroring and provenance-preserving matching paste at 80x24."""
import fcntl
import io
import json
import os
import pathlib
import pty
import re
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

from test_home import isolate
isolate()


BINARY = str(pathlib.Path(sys.argv[1]).resolve())
SOURCE = b"mirror text"
FOREIGN = b"FOREIGN_MIRROR_PASTE"
OSC52 = b"\x1b]52;c;bWlycm9yIHRleHQ=\x07"
MANIFEST = b'''format_version = 1
course_id = "course"
assignment_id = "clipboard-mirror"
assignment_version = "v1"
title = "Clipboard mirror"
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


def write_package(root):
    package = root / "assignment.rta"
    with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in [("assignment.toml", MANIFEST), ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'), ("starter/main.rs", SOURCE)]:
            entry = tarfile.TarInfo(name)
            entry.size = len(contents)
            entry.mode = 0o600
            archive.addfile(entry, io.BytesIO(contents))
    data = package.read_bytes()
    while data.endswith(bytes(512)):
        data = data[:-512]
    package.write_bytes(data + bytes(1024))
    return package


def rendered_screen(data):
    screen = [[" "] * 80 for _ in range(24)]
    row = column = 0
    for part in re.split(r"(\x1b\[[0-?]*[ -/]*[@-~])", data.decode("utf-8", "replace")):
        if part.startswith("\x1b["):
            if part[2:3] == "?":
                continue
            code = part[-1]
            values = [int(value or "0") for value in part[2:-1].split(";")]
            amount = values[0] or 1
            if code in ("H", "f"):
                row = amount - 1
                column = (values[1] or 1) - 1 if len(values) > 1 else 0
            elif code == "G":
                column = amount - 1
            elif code == "A":
                row = max(0, row - amount)
            elif code == "B":
                row += amount
            elif code == "C":
                column += amount
            elif code == "D":
                column = max(0, column - amount)
            elif code == "J" and values[0] == 2:
                screen = [[" "] * 80 for _ in range(24)]
            elif code == "K" and 0 <= row < 24:
                start = 0 if values[0] in (1, 2) else column
                end = column + 1 if values[0] == 1 else 80
                screen[row][start:end] = [" "] * (end - start)
            continue
        for character in part:
            if character == "\r":
                column = 0
            elif character == "\n":
                row += 1
            elif character >= " ":
                if 0 <= row < 24 and 0 <= column < 80:
                    screen[row][column] = character
                column += 1
    return "\n".join("".join(line) for line in screen)


def main():
    with tempfile.TemporaryDirectory(prefix="rustrace-clipboard-mirror-pty-") as temp:
        root = pathlib.Path(temp)
        package = write_package(root)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
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
        process = subprocess.Popen(
            [sys.executable, "-c", wrapper, BINARY, "work", str(package)],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            preexec_fn=child,
            env={**os.environ, "TERM": "xterm-256color", "RUSTUP_AUTO_INSTALL": "0"},
            cwd=root,
        )
        transcript = bytearray()
        probe_answered = False

        def wait_for(predicate, message, seconds=10):
            nonlocal probe_answered
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                if select.select([master], [], [], 0.025)[0]:
                    transcript.extend(os.read(master, 65536))
                if not probe_answered and b"\x1b[?u\x1b[c" in transcript:
                    os.write(master, b"\x1b[?1;0c")
                    probe_answered = True
                assert len(transcript) < 2 * 1024 * 1024, "terminal output exceeded bound"
                if predicate():
                    return
            raise AssertionError(message + ": " + repr(bytes(transcript[-4000:])))

        def settle():
            end = time.monotonic() + 0.2
            wait_for(lambda: time.monotonic() >= end, "UI did not settle", 2)

        try:
            wait_for(lambda: b" files" in transcript, "production UI did not start")
            os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.
            os.write(master, b"\x01\x03")
            wait_for(lambda: OSC52 in transcript, "exact OSC 52 mirror was not emitted")
            assert b"\x1b]52;c;?" not in transcript, "OSC 52 query form was emitted"

            os.write(master, b"!\x01\x1b[200~" + SOURCE + b"\x1b[201~")
            settle()
            os.write(master, b"\x1b[200~" + FOREIGN + b"\x1b[201~")
            wait_for(
                lambda: "Paste blocked:" in rendered_screen(transcript),
                "foreign paste did not show the P4 warning",
            )
            os.write(master, b"\x11")
            wait_for(lambda: process.poll() is not None, "production CLI did not quit")
            assert process.returncode == 0, repr(bytes(transcript[-4000:]))
            assert b"TERMINAL_RESTORED" in transcript
            assert b"\x1b[?2004l" in transcript and b"\x1b[?1049l" in transcript
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            try:
                termios.tcsetattr(slave, termios.TCSANOW, before)
            except termios.error:
                pass
            os.close(master)
            os.close(slave)

        work = root / "assignment.work"
        assert (work / "main.rs").read_bytes() == SOURCE, "matching or foreign paste changed final bytes"
        metadata = json.loads((work / ".rustrace" / "session.json").read_bytes())
        connection = sqlite3.connect(work / ".rustrace" / (metadata["session_id"] + ".sqlite"))
        rows = connection.execute("SELECT payload FROM events ORDER BY sequence").fetchall()
        connection.close()
        events = [json.loads(raw)["event"] for (raw,) in rows]
        copies = [event for event in events if event["type"] == "clipboard_copied"]
        pastes = [event for event in events if event["type"] == "internal_paste"]
        rejected = [event for event in events if event["type"] == "paste_rejected"]
        assert len(copies) == len(pastes) == len(rejected) == 1
        assert pastes[0]["payload"]["source"] == {
            "session_id": metadata["session_id"],
            "sequence": next(
                json.loads(raw)["sequence"]
                for (raw,) in rows
                if json.loads(raw)["event"]["type"] == "clipboard_copied"
            ),
            "event_hash": next(
                json.loads(raw)["event_hash"]
                for (raw,) in rows
                if json.loads(raw)["event"]["type"] == "clipboard_copied"
            ),
        }
        assert rejected[0]["payload"] == {
            "reason": "external_input",
            "channel": "terminal_bracketed",
        }
        encoded = b"".join(raw for (raw,) in rows)
        assert FOREIGN not in encoded, "foreign paste leaked into the journal"

        inspect = subprocess.run(
            [BINARY, "work", str(package), "--inspect"], capture_output=True, timeout=20
        )
        assert inspect.returncode == 0, inspect.stdout + inspect.stderr
        print("80x24 OSC 52 mirror, matching paste provenance, rejection, replay and cleanup passed")


if __name__ == "__main__":
    main()
