#!/usr/bin/env python3
"""Drive T10.18 Command and Control navigation through real 80x24 PTYs."""

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
SOURCE = b"one two\r\nthree four\r\nfive six"
COMMAND_EXPECTED = b"one two\r\nthree four\r\n"
CONTROL_EXPECTED = b"one two\r\nthree four\r\nfive "
MANIFEST = b'''format_version = 1
course_id = "course"
assignment_id = "navigation-chords"
assignment_version = "v1"
title = "Navigation chords"
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
    package = root / "assignment.rta"
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in [
            ("assignment.toml", MANIFEST),
            ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
            ("starter/main.rs", SOURCE),
        ]:
            info = tarfile.TarInfo(name)
            info.size = len(contents)
            info.mode = 0o600
            archive.addfile(info, io.BytesIO(contents))
    data = buffer.getvalue()
    while data.endswith(bytes(512)):
        data = data[:-512]
    package.write_bytes(data + bytes(1024))
    return package


def rendered_screen(data):
    screen = [[" "] * 80 for _ in range(24)]
    row = column = 0
    for part in re.split(
        r"(\x1b\[[0-?]*[ -/]*[@-~])", data.decode("utf-8", "replace")
    ):
        if part.startswith("\x1b["):
            if part[2:3] in "?><":
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
    return "\n".join("".join(line) for line in screen)


def transactions(workspace):
    state = workspace / ".rustrace"
    metadata = json.loads((state / "session.json").read_bytes())
    with sqlite3.connect(state / (metadata["session_id"] + ".sqlite")) as connection:
        rows = connection.execute("SELECT payload FROM events ORDER BY sequence").fetchall()
    return [
        event["event"]["payload"]
        for (raw,) in rows
        if (event := json.loads(raw))["event"]["type"] == "file_edited"
    ]


def exercise(root, command_mode):
    package = package_at(root)
    config = root / "config" / "rustrace"
    config.mkdir(parents=True)
    modifier = "command" if command_mode else "control"
    (config / "config.toml").write_text(f'modifier = "{modifier}"\n')
    workspace = root / "assignment.work"

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
        env={
            **os.environ,
            "TERM": "xterm-256color",
            "XDG_CONFIG_HOME": str(root / "config"),
            "RUSTUP_AUTO_INSTALL": "0",
        },
        cwd=root,
    )
    transcript = bytearray()
    answered_probe = False

    def drain():
        nonlocal answered_probe
        if select.select([master], [], [], 0.025)[0]:
            try:
                transcript.extend(os.read(master, 65536))
            except OSError:
                pass
        if not answered_probe and b"\x1b[?u\x1b[c" in transcript:
            response = b"\x1b[?0u\x1b[?1;0c" if command_mode else b"\x1b[?1;0c"
            os.write(master, response)
            answered_probe = True
        assert len(transcript) <= 2 * 1024 * 1024

    def wait_for(predicate, description, seconds=20):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            drain()
            if predicate():
                return
            if process.poll() is not None:
                break
        raise AssertionError(
            f"timed out waiting for {description}: {bytes(transcript[-5000:])!r}"
        )

    def wait_for_exit(description, seconds=20):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            drain()
            returncode = process.poll()
            if returncode is not None:
                return returncode
        raise AssertionError(
            f"timed out waiting for {description}: {bytes(transcript[-5000:])!r}"
        )

    def send(sequence, line, column):
        os.write(master, sequence)
        marker = f"Ln {line}, Col {column}"
        wait_for(lambda: marker in rendered_screen(transcript), marker)

    try:
        wait_for(
            lambda: answered_probe
            and b" files" in transcript
            and (workspace / "main.rs").is_file(),
            "workspace editor",
            30,
        )
        os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.
        wait_for(lambda: "Ln 1, Col 1" in rendered_screen(transcript), "initial caret")

        if command_mode:
            wait_for(lambda: b"\x1b[>5u" in transcript, "kitty keyboard push")
            # Arrows retain their CSI final bytes under the negotiated flags;
            # disambiguated Backspace uses CSI-u. Modifier values are base + 1.
            send(b"\x1b[1;9C", 1, 8)    # Super-Right
            send(b"\x1b[1;9D", 1, 1)    # Super-Left
            send(b"\x1b[1;3C", 1, 4)    # Alt-Right
            send(b"\x1b[1;3D", 1, 1)    # Alt-Left
            send(b"\x1b[1;9B", 3, 9)    # Super-Down
            send(b"\x1b[1;10A", 1, 1)   # Shift-Super-Up
            assert (workspace / "main.rs").read_bytes() == SOURCE
            send(b"\x1b[1;9B", 3, 9)
            send(b"\x1b[1;4D", 3, 6)    # Shift-Alt-Left
            os.write(master, b"\x1b[127;3u")  # Alt-Backspace
            wait_for(
                lambda: (workspace / "main.rs").read_bytes()
                == b"one two\r\nthree four\r\nfive ",
                "Option-Backspace bytes",
            )
            wait_for(lambda: "Ln 3, Col 6" in rendered_screen(transcript), "word delete caret")
            os.write(master, b"\x1b[127;9u")  # Super-Backspace
            expected = COMMAND_EXPECTED
            expected_edits = [(26, 29), (21, 26)]
        else:
            assert b"\x1b[>5u" not in transcript
            send(b"\x1b[1;5C", 1, 4)    # Ctrl-Right
            send(b"\x1b[1;6C", 1, 8)    # Shift-Ctrl-Right
            send(b"\x1b[1;5D", 1, 5)    # Ctrl-Left
            send(b"\x1b[1;5H", 1, 1)    # Ctrl-Home
            send(b"\x1b[1;6F", 3, 9)    # Shift-Ctrl-End
            send(b"\x1b[1;5H", 1, 1)
            send(b"\x1b[1;5F", 3, 9)    # Ctrl-End
            os.write(master, b"\x1b[127;5u")  # Ctrl-Backspace
            expected = CONTROL_EXPECTED
            expected_edits = [(26, 29)]

        wait_for(lambda: (workspace / "main.rs").read_bytes() == expected, "autosaved bytes")
        final_column = 1 if command_mode else 6
        wait_for(
            lambda: f"Ln 3, Col {final_column}" in rendered_screen(transcript),
            "final caret",
        )
        if not command_mode:
            send(b"\x1b[1;5D", 3, 1)
            assert (workspace / "main.rs").read_bytes() == expected

        edits = transactions(workspace)
        assert len(edits) == len(expected_edits), edits
        assert all(transaction["origin"] == "keyboard" for transaction in edits)
        assert [
            (transaction["edits"][0]["start_byte"], transaction["edits"][0]["end_byte"])
            for transaction in edits
        ] == expected_edits
        assert all(
            len(transaction["edits"]) == 1
            and transaction["edits"][0]["inserted_text"] == ""
            for transaction in edits
        )

        os.write(master, b"\x11")
        returncode = wait_for_exit("clean exit", 30)
        while select.select([master], [], [], 0.025)[0]:
            length_before = len(transcript)
            drain()
            if len(transcript) == length_before:
                break
        assert returncode == 0, bytes(transcript[-5000:])
        assert b"TERMINAL_RESTORED" in transcript
        assert b"\x1b[?2004l" in transcript and b"\x1b[?1049l" in transcript
        if command_mode:
            assert transcript.count(b"\x1b[<1u") == 1
        else:
            assert b"\x1b[<1u" not in transcript
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

    inspected = subprocess.run(
        [BINARY, "work", str(package), "--inspect"],
        capture_output=True,
        cwd=root,
        timeout=20,
    )
    assert inspected.returncode == 0, inspected.stdout + inspected.stderr
    assert json.dumps(expected.decode()).encode() in inspected.stdout, inspected.stdout


with tempfile.TemporaryDirectory(prefix="rustrace-navigation-chords-") as root_text:
    root = pathlib.Path(root_text)
    (root / "command").mkdir()
    (root / "control").mkdir()
    exercise(root / "command", command_mode=True)
    exercise(root / "control", command_mode=False)

print("Command and Control navigation chords, carets, bytes, replay, and cleanup passed")
