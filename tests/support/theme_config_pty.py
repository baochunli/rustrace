#!/usr/bin/env python3
"""Assert configured 24-bit palette colours in the real 80x24 work TUI."""

import fcntl
import io
import os
import pathlib
import pty
import re
import select
import signal
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
WIDTH = 80
HEIGHT = 24
MANIFEST = b'''format_version = 1
course_id = "demo"
assignment_id = "theme"
assignment_version = "v1"
title = "Theme PTY"
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
SOURCE = b'fn main() { let message = "custom"; }\n'
COLORS = {
    "mauve": (1, 2, 3),
    "green": (4, 5, 6),
    "overlay0": (7, 8, 9),
    "accent": (10, 11, 12),
    "panel_bg": (13, 14, 15),
    "tab_active_bg": (16, 17, 18),
    "tab_active_fg": (19, 20, 21),
}


def package_at(root):
    package = root / "assignment.rta"
    with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in [("assignment.toml", MANIFEST), ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'), ("starter/main.rs", SOURCE)]:
            info = tarfile.TarInfo(name)
            info.size = len(contents)
            info.mode = 0o600
            archive.addfile(info, io.BytesIO(contents))
    data = package.read_bytes()
    while data.endswith(bytes(512)):
        data = data[:-512]
    package.write_bytes(data + bytes(1024))
    return package


def screen(transcript):
    grid = [[(" ", None, None) for _ in range(WIDTH)] for _ in range(HEIGHT)]
    row = 0
    column = 0
    foreground = None
    background = None
    text = bytes(transcript).decode("utf-8", "replace")
    index = 0
    while index < len(text):
        character = text[index]
        if character == "\x1b" and index + 1 < len(text) and text[index + 1] == "[":
            end = index + 2
            while end < len(text) and not ("@" <= text[end] <= "~"):
                end += 1
            if end >= len(text):
                break
            raw = text[index + 2:end].lstrip("?")
            values = [int(value) if value.isdigit() else 0 for value in raw.split(";")] if raw else []
            command = text[end]
            if command in ("H", "f"):
                row = max(0, (values[0] if values else 1) - 1)
                column = max(0, (values[1] if len(values) > 1 else 1) - 1)
            elif command == "J" and values and values[0] in (2, 3):
                grid = [[(" ", None, None) for _ in range(WIDTH)] for _ in range(HEIGHT)]
            elif command == "C":
                column += values[0] if values else 1
            elif command == "G":
                column = max(0, (values[0] if values else 1) - 1)
            elif command == "d":
                row = max(0, (values[0] if values else 1) - 1)
            elif command == "m":
                position = 0
                while position < len(values or [0]):
                    value = (values or [0])[position]
                    if value == 0:
                        foreground = None
                        background = None
                    elif value == 39:
                        foreground = None
                    elif value == 49:
                        background = None
                    elif value in (38, 48) and position + 4 < len(values) and values[position + 1] == 2:
                        color = tuple(values[position + 2:position + 5])
                        if value == 38:
                            foreground = color
                        else:
                            background = color
                        position += 4
                    position += 1
            index = end + 1
            continue
        if character == "\r":
            column = 0
        elif character == "\n":
            row = min(HEIGHT - 1, row + 1)
        elif character >= " ":
            if 0 <= row < HEIGHT and 0 <= column < WIDTH:
                grid[row][column] = (character, foreground, background)
            column += 1
        index += 1
    return grid


def locate(grid, needle):
    for row, cells in enumerate(grid):
        rendered = "".join(character for character, _, _ in cells)
        column = rendered.find(needle)
        if column >= 0:
            return row, column
    raise AssertionError(f"missing {needle!r} in reconstructed screen")


with tempfile.TemporaryDirectory(prefix="rustrace-theme-") as root_text:
    root = pathlib.Path(root_text)
    package = package_at(root)
    config = root / "config/rustrace"
    config.mkdir(parents=True)
    (config / "config.toml").write_text(
        '''[theme]
name = "catppuccin"
auto_switch = false

[theme.custom]
mauve = "#010203"
green = "#040506"
overlay0 = "#070809"
accent = "#0a0b0c"
panel_bg = "#0d0e0f"
tab_active_bg = "#101112"
tab_active_fg = "#131415"
'''
    )
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", HEIGHT, WIDTH, 0, 0))

    def child():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    environment = {
        **os.environ,
        "TERM": "xterm-256color",
        "COLORTERM": "truecolor",
        "XDG_CONFIG_HOME": str(root / "config"),
    }
    environment.pop("NO_COLOR", None)
    process = subprocess.Popen(
        [BINARY, "work", str(package)],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        preexec_fn=child,
        env=environment,
        cwd=root,
    )
    transcript = bytearray()

    def drain(seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([master], [], [], 0.025)[0]:
                try:
                    transcript.extend(os.read(master, 65536))
                except OSError:
                    return
            assert len(transcript) <= 2 * 1024 * 1024, "capture limit exceeded"

    def redraw():
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", HEIGHT, WIDTH - 1, 0, 0))
        os.kill(process.pid, signal.SIGWINCH)
        drain(0.2)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", HEIGHT, WIDTH, 0, 0))
        os.kill(process.pid, signal.SIGWINCH)
        drain(0.4)

    try:
        deadline = time.monotonic() + 20
        while b" files" not in transcript and time.monotonic() < deadline:
            drain(0.1)
        assert b" files" in transcript, repr(bytes(transcript[-5000:]))
        os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.
        drain(0.2)
        redraw()
        grid = screen(transcript)
        source_row, source_column = locate(grid, "fn main()")
        string_row, string_column = locate(grid, '"custom"')
        output_row, output_column = locate(grid, "output")
        tab_row, tab_column = locate(grid, "main.rs")
        assert grid[source_row][source_column][1] == COLORS["mauve"], (
            grid[source_row],
            [
                code
                for code in re.findall(rb"\x1b\[[0-9;:]*m", transcript)
                if b"38" in code or b"48" in code
            ][-80:],
        )
        assert grid[string_row][string_column + 1][1] == COLORS["green"], grid[string_row]
        assert grid[output_row][output_column][1] == COLORS["overlay0"], grid[output_row]
        assert grid[tab_row][tab_column][1:] == (
            COLORS["tab_active_fg"],
            COLORS["tab_active_bg"],
        )

        os.write(master, b"\x1bOP")
        drain(0.4)
        redraw()
        overlay = screen(transcript)
        keybind_row, keybind_column = locate(overlay, "keybinds")
        assert overlay[keybind_row][keybind_column][2] == COLORS["panel_bg"]
        border_cells = [
            cell
            for row in overlay
            for cell in row
            if cell[0] in "┌┐└┘─│" and cell[1] == COLORS["accent"]
        ]
        assert border_cells, "custom overlay frame colour was not rendered"

        os.write(master, b"\x1b")
        drain(0.2)
        os.write(master, b"\x11")
        deadline = time.monotonic() + 5
        while process.poll() is None and time.monotonic() < deadline:
            drain(0.1)
        assert process.poll() is not None, repr(bytes(transcript[-5000:]))
        assert process.returncode == 0, repr(bytes(transcript[-5000:]))
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        os.close(master)
        os.close(slave)

print("Custom theme editor, tabs, output, and overlay colours passed")
