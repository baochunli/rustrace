"""Replace 1,025 matches in one bounded transaction through the real 80x24 TUI."""

import fcntl
import io
import json
import os
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


BINARY = sys.argv[1]
MATCHES = 1025
INITIAL = b"a " * MATCHES + b"\n"
EXPECTED = b"b " * MATCHES + b"\n"
assert len(INITIAL) == 2051
MANIFEST = b'''format_version = 1
course_id = "course"
assignment_id = "find-replace-limit"
assignment_version = "v1"
title = "Find replace limit"
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


def package_at(root):
    package = os.path.join(root, "assignment.rta")
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in [
            ("assignment.toml", MANIFEST),
            ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
            ("starter/main.rs", INITIAL),
        ]:
            info = tarfile.TarInfo(name)
            info.size = len(contents)
            info.mode = 0o600
            archive.addfile(info, io.BytesIO(contents))
    data = stream.getvalue()
    while data.endswith(bytes(512)):
        data = data[:-512]
    with open(package, "wb") as output:
        output.write(data + bytes(1024))
    return package


def rendered_screen(data):
    screen = [[" "] * 80 for _ in range(24)]
    row = column = 0
    for part in re.split(r"(\x1b\[[0-?]*[ -/]*[@-~])", data.decode("utf-8", "replace")):
        if part.startswith("\x1b["):
            if part[2:3] == "?":
                if part == "\x1b[?1049h":
                    screen = [[" "] * 80 for _ in range(24)]
                    row = column = 0
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
    return ["".join(line) for line in screen]


def journal_events(workspace):
    state = os.path.join(workspace, ".rustrace")
    with open(os.path.join(state, "session.json"), "rb") as source:
        session_id = json.load(source)["session_id"]
    with sqlite3.connect(os.path.join(state, session_id + ".sqlite")) as connection:
        rows = connection.execute("SELECT payload FROM events ORDER BY sequence").fetchall()
    return [json.loads(raw) for (raw,) in rows]


with tempfile.TemporaryDirectory(prefix="rustrace-find-replace-limit-") as root:
    package = package_at(root)
    workspace = os.path.join(root, "assignment.work")
    source = os.path.join(workspace, "main.rs")
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    before = termios.tcgetattr(slave)

    def child():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    wrapper = (
        "import subprocess,termios,sys; before=termios.tcgetattr(0); "
        "result=subprocess.run(sys.argv[1:]); "
        "assert termios.tcgetattr(0)==before, 'raw mode leaked'; "
        "print('TERMINAL_RESTORED', flush=True); sys.exit(result.returncode)"
    )
    process = subprocess.Popen(
        [sys.executable, "-c", wrapper, BINARY, "work", package],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        preexec_fn=child,
        env={**os.environ, "TERM": "xterm-256color", "RUSTUP_AUTO_INSTALL": "0"},
        cwd=root,
    )
    transcript = bytearray()

    def drain(seconds=0.05):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([master], [], [], 0.02)[0]:
                try:
                    transcript.extend(os.read(master, 65536))
                except OSError:
                    return
            assert len(transcript) <= 2 * 1024 * 1024

    def wait_for(predicate, label, timeout=20):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            drain()
            if predicate():
                return
            assert process.poll() is None, f"work exited before {label}"
        raise AssertionError(f"timed out waiting for {label}: {bytes(transcript[-5000:])!r}")

    def screen():
        return rendered_screen(transcript)

    def send(keys):
        os.write(master, keys)
        drain(0.15)

    def click(wanted):
        for row, line in enumerate(screen()):
            column = line.find(wanted)
            if column >= 0:
                send(f"\x1b[<0;{column + 2};{row + 1}M".encode())
                return
        raise AssertionError(f"missing {wanted!r} in\n" + "\n".join(screen()))

    try:
        wait_for(lambda: any(" files" in line for line in screen()), "student shell")
        send(b"\x1b[<0;4;3M\x1b[<0;4;3m")
        send(b"\x06")
        wait_for(lambda: any("find and replace" in line for line in screen()), "find panel")
        send(b"a\tb")
        wait_for(lambda: any("1 of 1025" in line for line in screen()), "match count")
        click(" replace all ")
        wait_for(lambda: any("replaced 1025" in line for line in screen()), "replace all")
        assert not any("maximum is 1024" in line for line in screen())
        send(b"\x1b")
        wait_for(lambda: not any("find and replace" in line for line in screen()), "closed panel")
        send(b"\x13")
        wait_for(
            lambda: os.path.isfile(source) and open(source, "rb").read() == EXPECTED,
            "saved replacement bytes",
        )
        send(b"\x11")
        wait_for(lambda: process.poll() is not None, "normal exit")
        drain(0.2)
        assert process.returncode == 0, bytes(transcript[-5000:])
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

    assert open(source, "rb").read() == EXPECTED
    edits = [
        event["event"]["payload"]
        for event in journal_events(workspace)
        if event["event"]["type"] == "file_edited"
    ]
    assert len(edits) == 1, edits
    assert edits[0]["origin"] == "keyboard"
    assert len(edits[0]["edits"]) == 1

print("80x24 2,051-byte/1,025-match replace-all stayed one bounded Keyboard transaction")
