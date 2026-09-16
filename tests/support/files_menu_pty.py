"""Delete a dirty file through the 80x24 files-panel context menu."""

import fcntl
import glob
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


binary = sys.argv[1]
manifest = b'''format_version = 1
course_id = "course"
assignment_id = "files-menu"
assignment_version = "v1"
title = "Files menu PTY"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["src/**/*.rs", "Cargo.toml"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
'''


def read_available(fd, transcript):
    while select.select([fd], [], [], 0.02)[0]:
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            return
        if not chunk:
            return
        transcript.extend(chunk)


def wait_for(predicate, fd, transcript, message, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        read_available(fd, transcript)
        if predicate():
            return
        time.sleep(0.02)
    raise AssertionError(message + ": " + repr(bytes(transcript[-4000:])))


def starts_with(path, prefix):
    try:
        with open(path, "rb") as file:
            return file.read().startswith(prefix)
    except FileNotFoundError:
        return False


def rendered_screen(transcript):
    screen = [[" "] * 80 for _ in range(24)]
    row = column = 0
    text = transcript.decode("utf-8", "replace")
    for part in re.split(r"(\x1b\[[0-?]*[ -/]*[@-~])", text):
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
            elif 0 <= row < 24 and 0 <= column < 80:
                screen[row][column] = character
                column += 1
    return screen


with tempfile.TemporaryDirectory(prefix="rustrace-files-menu-pty-") as root:
    package = os.path.join(root, "assignment.rta")
    with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
        for name, content in [
            ("assignment.toml", manifest),
            ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
            ("starter/src/a.rs", b"fn a() {}\n"),
            ("starter/src/b.rs", b"fn b() {}\n"),
        ]:
            info = tarfile.TarInfo(name)
            info.size = len(content)
            info.mode = 0o600
            archive.addfile(info, io.BytesIO(content))
    with open(package, "rb") as package_file:
        data = package_file.read()
    while data.endswith(bytes(512)):
        data = data[:-512]
    with open(package, "wb") as package_file:
        package_file.write(data + bytes(1024))

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))

    def child():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    process = subprocess.Popen(
        [binary, "work", package],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        preexec_fn=child,
        env={**os.environ, "TERM": "xterm-256color"},
        cwd=root,
    )
    transcript = bytearray()
    try:
        wait_for(
            lambda: b" files" in transcript and b"src/b.rs" in transcript,
            master,
            transcript,
            "student shell did not render both files at 80x24",
            20,
        )
        database = glob.glob(
            os.path.join(root, "assignment.work", ".rustrace", "*.sqlite")
        )[0]

        def events():
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
                return [
                    json.loads(row[0])
                    for row in connection.execute(
                        "SELECT payload FROM events ORDER BY sequence"
                    )
                ]

        # Open the second file and let one edit autosave. This resets the session's
        # two-second save clock so the next edit stays dirty while the menu opens.
        os.write(master, b"\x1b[<0;4;4M\x1b[<0;4;4mX")
        wait_for(
            lambda: starts_with(
                os.path.join(root, "assignment.work", "src", "b.rs"), b"X"
            ),
            master,
            transcript,
            "first edit did not autosave",
        )
        edit_count = len(events())
        os.write(master, b"Y")
        wait_for(
            lambda: len(events()) > edit_count
            and events()[-1]["event"]["type"] == "file_edited",
            master,
            transcript,
            "second file did not become dirty",
        )
        focus_count = len(events())
        os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")
        wait_for(
            lambda: len(events()) > focus_count
            and events()[-1]["event"]["type"] == "file_focused",
            master,
            transcript,
            "first file did not become active",
        )
        before_menu = len(events())

        os.write(master, b"\x1b[<2;4;4M")
        wait_for(
            lambda: "delete…" in "\n".join("".join(row) for row in rendered_screen(transcript)),
            master,
            transcript,
            "files context menu did not open on the selected row",
        )
        screen = rendered_screen(transcript)
        delete_row = next(index for index, row in enumerate(screen) if "delete…" in "".join(row))
        delete_column = "".join(screen[delete_row]).index("delete…")
        os.write(
            master,
            f"\x1b[<0;{delete_column + 1};{delete_row + 1}M".encode(),
        )
        wait_for(
            lambda: "Delete src/b.rs?" in "\n".join("".join(row) for row in rendered_screen(transcript)),
            master,
            transcript,
            "delete menu entry did not open the existing confirmation",
        )
        os.write(master, b"\r")
        deleted = os.path.join(root, "assignment.work", "src", "b.rs")
        wait_for(
            lambda: not os.path.exists(deleted),
            master,
            transcript,
            "confirmed file was not removed from disk",
        )

        delta = events()[before_menu:]
        assert [event["event"]["type"] for event in delta] == ["file_deleted"], delta
        assert "mouse" not in json.dumps(delta).lower(), delta

        os.write(master, b"\x11")
        wait_for(
            lambda: process.poll() is not None,
            master,
            transcript,
            "student shell did not exit",
        )
        assert process.returncode == 0, repr(bytes(transcript))
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
        os.close(master)
        os.close(slave)

print("Files menu PTY deleted one dirty file with one FileDeleted event and no mouse event")
