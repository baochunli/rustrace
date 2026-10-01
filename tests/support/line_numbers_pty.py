#!/usr/bin/env python3
"""Line numbers in the real 80x24 work TUI: on by default, Ctrl-L hides and
shows them, clicks map through the gutter, the console keeps Ctrl-L, the key
never edits the buffer, and the choice survives a restart."""

import fcntl
import io
import json
import os
import pathlib
import pty
import select
import sqlite3
import signal
import struct
import subprocess
import sys
import tarfile
import tempfile
import termios
import time

from pty_process import wait_for_pty_exit
from pty_screen import rendered_screen
from test_home import isolate
isolate()


BINARY = str(pathlib.Path(sys.argv[1]).resolve())
WIDTH = 80
HEIGHT = 24
EDITOR_X = 26
MANIFEST = b'''format_version = 1
course_id = "demo"
assignment_id = "numbers"
assignment_version = "v1"
title = "Line numbers PTY"
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
SOURCE = b'fn main() {\n    let first = 1;\n    let second = 2;\n}\n'
CTRL_L = b"\x0c"
EMPTY_CONSOLE_LINE = "> ▏"  # Prompt and line cursor.
CTRL_Q = b"\x11"
F9 = b"\x1b[20~"
ESC = b"\x1b"
PREFERENCES = pathlib.Path(os.environ["XDG_STATE_HOME"]) / "rustrace/editor-preferences.json"


def package_at(root):
    package = root / "assignment.rta"
    with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in [
            ("assignment.toml", MANIFEST),
            ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
            ("starter/main.rs", SOURCE),
        ]:
            info = tarfile.TarInfo(name)
            info.size = len(contents)
            info.mode = 0o600
            archive.addfile(info, io.BytesIO(contents))
    data = package.read_bytes()
    while data.endswith(bytes(512)):
        data = data[:-512]
    package.write_bytes(data + bytes(1024))
    return package


def click(column, row):
    """A left click at zero-based screen cell (column, row)."""
    return f"\x1b[<0;{column + 1};{row + 1}M\x1b[<0;{column + 1};{row + 1}m".encode()


def stored_choice():
    if not PREFERENCES.exists():
        return None
    return json.loads(PREFERENCES.read_text())["line_numbers"]


class Session:
    def __init__(self, root, package):
        self.master, self.slave = pty.openpty()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", HEIGHT, WIDTH, 0, 0))

        def child():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        self.process = subprocess.Popen(
            [BINARY, "work", str(package)],
            stdin=self.slave,
            stdout=self.slave,
            stderr=self.slave,
            preexec_fn=child,
            env={**os.environ, "TERM": "xterm-256color"},
            cwd=root,
        )
        self.transcript = bytearray()
        self.root = root
        self.last_toggle = 0.0

    def drain(self, seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([self.master], [], [], 0.025)[0]:
                try:
                    self.transcript.extend(os.read(self.master, 65536))
                except OSError:
                    return
            assert len(self.transcript) <= 2 * 1024 * 1024, "capture limit exceeded"

    def screen(self):
        return rendered_screen(bytes(self.transcript), rows=HEIGHT, columns=WIDTH).split("\n")

    def wait_for(self, predicate, what, seconds=20):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            self.drain(0.1)
            if predicate(self.screen()):
                return
        raise AssertionError(f"timed out waiting for {what}:\n" + "\n".join(self.screen()))

    def send(self, data, seconds=0.4):
        os.write(self.master, data)
        self.drain(seconds)

    def journal(self):
        """Every recorded event payload, in order."""
        databases = list((self.root / "assignment.work/.rustrace").glob("*.sqlite"))
        assert len(databases) == 1, databases
        with sqlite3.connect(f"file:{databases[0]}?mode=ro", uri=True) as connection:
            return [row[0] for row in
                    connection.execute("SELECT payload FROM events ORDER BY sequence")]

    def toggle(self, rows, choice, presses=1):
        """Ctrl-L (held for `presses` repeats) flips the gutter once and records nothing."""
        # Presses closer than Rustrace's repeat window count as one held key.
        time.sleep(max(0.0, self.last_toggle + 0.7 - time.monotonic()))
        before = self.journal()
        self.send(CTRL_L * presses, 0.1)
        self.last_toggle = time.monotonic()
        self.expect_rows(rows, f"line numbers {'shown' if rows is NUMBERED else 'hidden'}")
        self.expect_choice(choice)
        self.drain(0.3)
        assert self.journal() == before, "a line-number toggle entered the journal"

    def editor_rows(self, count=5, screen=None):
        screen = screen or self.screen()
        return [line[EDITOR_X:].rstrip() for line in screen[1:1 + count]]

    def expect_rows(self, rows, what):
        self.wait_for(lambda screen: self.editor_rows(len(rows), screen) == rows, what, 10)

    def expect_caret(self, position):
        self.wait_for(
            lambda screen: position in screen[0], f"caret at {position}", 10
        )

    def expect_choice(self, choice):
        deadline = time.monotonic() + 10
        while stored_choice() is not choice and time.monotonic() < deadline:
            self.drain(0.05)
        assert stored_choice() is choice, (stored_choice(), choice)

    def quit(self):
        os.write(self.master, CTRL_Q)
        wait_for_pty_exit(self.process, self.drain, 10)
        assert self.process.returncode == 0, repr(bytes(self.transcript[-5000:]))

    def close(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait()
        os.close(self.master)
        os.close(self.slave)


NUMBERED = [" 1 fn main() {", " 2     let first = 1;", " 3     let second = 2;", " 4 }", " 5"]
PLAIN = ["fn main() {", "    let first = 1;", "    let second = 2;", "}", ""]


def open_source(session):
    session.wait_for(lambda screen: " files" in screen[0], "the workspace")
    session.send(click(3, 2))  # main.rs, below Cargo.toml.
    session.wait_for(
        lambda screen: any("fn main() {" in line for line in screen[1:6]), "main.rs"
    )


with tempfile.TemporaryDirectory(prefix="rustrace-line-numbers-") as root_text:
    root = pathlib.Path(root_text)
    package = package_at(root)

    first = Session(root, package)
    try:
        open_source(first)
        assert stored_choice() is None, "a new install wrote a preference before any toggle"
        first.expect_rows(NUMBERED, "the default gutter")

        # A gutter click lands at the start of that line; a text click maps past the gutter.
        first.send(click(EDITOR_X + 1, 3))
        first.expect_caret("Ln 3, Col 1")
        first.send(click(EDITOR_X + 3 + 4, 2))
        first.expect_caret("Ln 2, Col 5")

        first.toggle(PLAIN, False)
        first.send(click(EDITOR_X + 4, 3))
        first.expect_caret("Ln 3, Col 5")

        # The console keeps Ctrl-L: it neither toggles nor types into the line.
        first.send(F9)
        first.wait_for(
            lambda screen: any(line[EDITOR_X:].rstrip() == EMPTY_CONSOLE_LINE for line in screen[10:]),
            "the empty console line",
        )
        time.sleep(max(0.0, first.last_toggle + 0.7 - time.monotonic()))
        first.send(CTRL_L, 1.0)
        console = first.screen()
        assert stored_choice() is False, "Ctrl-L in the console toggled line numbers"
        assert first.editor_rows(screen=console) == PLAIN, console
        assert any(line[EDITOR_X:].rstrip() == EMPTY_CONSOLE_LINE for line in console[10:]), \
            "Ctrl-L reached the console line:\n" + "\n".join(console)
        first.send(ESC)
        first.expect_rows(PLAIN, "plain rows")

        first.toggle(NUMBERED, True)
        first.toggle(PLAIN, False)
        # A held Ctrl-L arrives as repeated presses and still toggles once.
        first.toggle(NUMBERED, True, presses=6)
        first.toggle(PLAIN, False, presses=6)
        first.quit()
    finally:
        first.close()

    workspaces = list(root.glob("*.work"))
    assert len(workspaces) == 1, workspaces
    assert (workspaces[0] / "main.rs").read_bytes() == SOURCE, "Ctrl-L reached the buffer"

    second = Session(root, package)
    try:
        open_source(second)
        second.expect_rows(PLAIN, "plain rows")
        second.toggle(NUMBERED, True)
        second.quit()
    finally:
        second.close()

    third = Session(root, package)
    try:
        open_source(third)
        third.expect_rows(NUMBERED, "numbered rows")
        third.quit()
    finally:
        third.close()

    assert (workspaces[0] / "main.rs").read_bytes() == SOURCE

print("Line numbers default on, toggle with Ctrl-L, map clicks, and persist across restarts")
