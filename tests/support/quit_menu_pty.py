#!/usr/bin/env python3
"""Drive the production Quit menu row by Enter and left mouse Down."""
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


binary = str(pathlib.Path(sys.argv[1]).resolve())
mode = sys.argv[2]
assert mode in {
    "enter_clean",
    "mouse_clean",
    "enter_dirty_cancel_ctrl_confirm",
    "mouse_dirty_confirm",
}

manifest = b'''format_version = 1
course_id = "course"
assignment_id = "quit-menu"
assignment_version = "v1"
title = "Quit menu PTY"
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


def rendered_screen(data):
    screen = [[" "] * 80 for _ in range(24)]
    row = column = 0
    pattern = r"(\x1b\[[0-?]*[ -/]*[@-~])"
    for part in re.split(pattern, data.decode("utf-8", "replace")):
        if part.startswith("\x1b["):
            code = part[-1]
            if part[2:3] == "?":
                continue
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


with tempfile.TemporaryDirectory(prefix="rustrace-quit-menu-") as root_text:
    root = pathlib.Path(root_text)
    package = root / "assignment.rta"
    with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
        for name, content in [
            ("assignment.toml", manifest),
            ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
            ("starter/main.rs", b"fn main() {}\n"),
        ]:
            entry = tarfile.TarInfo(name)
            entry.size = len(content)
            entry.mode = 0o600
            archive.addfile(entry, io.BytesIO(content))
    data = package.read_bytes()
    while data.endswith(bytes(512)):
        data = data[:-512]
    package.write_bytes(data + bytes(1024))

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
    proc = subprocess.Popen(
        [sys.executable, "-c", wrapper, binary, "work", str(package)],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        preexec_fn=child,
        cwd=root,
        env={
            **os.environ,
            "TERM": "xterm-256color",
            "XDG_CONFIG_HOME": str(root / "config"),
            "RUSTUP_AUTO_INSTALL": "0",
        },
    )
    transcript = bytearray()
    probe_answered = False
    menu_opened = False
    quit_activated = False
    confirmation_visible = False
    confirmation_count = 0
    cancel_sent = False
    ctrl_q_sent = False
    confirm_sent = False
    deadline = time.monotonic() + 30
    try:
        while proc.poll() is None and time.monotonic() < deadline:
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))
            assert len(transcript) < 2 * 1024 * 1024, "capture limit exceeded"
            if not probe_answered and b"\x1b[?u\x1b[c" in transcript:
                os.write(master, b"\x1b[?1;0c")
                probe_answered = True

            screen = rendered_screen(transcript)
            if not menu_opened and " files" in screen:
                os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.
                if "dirty" in mode:
                    os.write(master, b"x")
                os.write(master, b"\x1b[18~")
                menu_opened = True
                continue

            if menu_opened and not quit_activated and " MENU " in screen and "Quit" in screen:
                if mode.startswith("enter"):
                    os.write(master, b"\x1b[A\r")
                else:
                    lines = screen.splitlines()
                    quit_row = max(index for index, line in enumerate(lines) if "Quit" in line)
                    quit_column = lines[quit_row].index("Quit")
                    click = f"\x1b[<0;{quit_column + 1};{quit_row + 1}M".encode()
                    os.write(master, click)
                quit_activated = True
                continue

            now_confirming = "Discard unsaved buffer changes?" in screen
            if now_confirming and not confirmation_visible:
                confirmation_count += 1
            confirmation_visible = now_confirming

            if mode == "enter_dirty_cancel_ctrl_confirm":
                if confirmation_count == 1 and not cancel_sent:
                    os.write(master, b"\x1b")
                    cancel_sent = True
                    continue
                if (
                    cancel_sent
                    and not confirmation_visible
                    and "discard cancelled" in screen
                    and not ctrl_q_sent
                ):
                    os.write(master, b"\x11")
                    ctrl_q_sent = True
                    continue
                if confirmation_count == 2 and confirmation_visible and not confirm_sent:
                    os.write(master, b"\r")
                    confirm_sent = True
            elif "dirty" in mode and confirmation_visible and not confirm_sent:
                os.write(master, b"\r")
                confirm_sent = True

        assert proc.poll() is not None, "CLI timed out: " + repr(bytes(transcript[:8000]))
        while select.select([master], [], [], 0.05)[0]:
            chunk = os.read(master, 65536)
            if not chunk:
                break
            transcript.extend(chunk)
        assert proc.returncode == 0, repr(bytes(transcript))
        assert menu_opened and quit_activated, "Quit menu row was not activated"
        if "clean" in mode:
            assert confirmation_count == 0, "clean Quit unexpectedly requested confirmation"
        elif mode == "enter_dirty_cancel_ctrl_confirm":
            assert confirmation_count == 2, "menu and Ctrl-Q did not show the same confirmation"
            assert cancel_sent and ctrl_q_sent and confirm_sent
        else:
            assert confirmation_count == 1 and confirm_sent
        assert b"TERMINAL_RESTORED" in transcript, "terminal was not restored"
    finally:
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
        try:
            termios.tcsetattr(slave, termios.TCSANOW, before)
        except termios.error:
            pass
        os.close(master)
        os.close(slave)

print(f"Quit menu production route passed: {mode}")
