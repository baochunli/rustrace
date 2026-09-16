"""Hostile bytes remain in an isolated fixture/capture, never our terminal."""
import fcntl
import io
import os
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

from pty_screen import rendered_screen

from test_home import isolate
isolate()

binary = sys.argv[1]
source = ("A\x1b]52;c;Y2xpcA==\x07\n"
          "B\x1b]0;hostile-title\x1b\\\n"
          "C\x1b[999;999H\x1bPpayload\x1b\\\n"
          "D\u009b31m\u202espoof\x00\n"
          "E東京 e\u0301 👩🏽‍🔬\tend\n").encode()
manifest = b'''format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Safe Display PTY"
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

with tempfile.TemporaryDirectory(prefix="rustrace-safe-display-") as root:
    package = os.path.join(root, "assignment.rta")
    with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
        for name, content in [("assignment.toml", manifest), ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'), ("starter/main.rs", source)]:
            entry = tarfile.TarInfo(name)
            entry.size = len(content)
            entry.mode = 0o600
            archive.addfile(entry, io.BytesIO(content))
    with open(package, "rb") as file:
        data = file.read()
    while data.endswith(bytes(512)):
        data = data[:-512]
    with open(package, "wb") as file:
        file.write(data + bytes(1024))

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
    before = termios.tcgetattr(slave)

    def child():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    # Query the terminal before its controlling session exits. On macOS the
    # parent's slave fd may return ENOTTY once the session leader has exited.
    wrapper = ("import subprocess,termios,sys; before=termios.tcgetattr(0); "
               "r=subprocess.run(sys.argv[1:]); "
               "assert termios.tcgetattr(0)==before, 'raw mode leaked'; "
               "print('TERMINAL_RESTORED', flush=True); sys.exit(r.returncode)")
    proc = subprocess.Popen(
        [sys.executable, "-c", wrapper, binary, "work", package], stdin=slave, stdout=slave, stderr=slave,
        preexec_fn=child, env={**os.environ, "TERM": "xterm-256color"}, cwd=root,
    )
    transcript = bytearray()
    deadline = time.monotonic() + 25
    quit_sent = False
    source_selected = False
    try:
        while proc.poll() is None and time.monotonic() < deadline:
            if select.select([master], [], [], .05)[0]:
                transcript.extend(os.read(master, 65536))
            screen = rendered_screen(transcript, rows=32, columns=120)
            if not source_selected and " files" in screen:
                os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")
                source_selected = True
            if not quit_sent and " files" in screen and "hostile-title" in screen:
                os.write(master, b"\x11")
                quit_sent = True
            assert len(transcript) < 2 * 1024 * 1024, "capture limit exceeded"
        assert proc.poll() is not None, "CLI timed out: " + repr(bytes(transcript[:5000]))
        while select.select([master], [], [], .05)[0]:
            chunk = os.read(master, 65536)
            if not chunk:
                break
            transcript.extend(chunk)
        assert proc.returncode == 0, repr(bytes(transcript))
        assert quit_sent, "source never rendered"
        assert b"TERMINAL_RESTORED" in transcript, "raw terminal mode leaked"
        assert b"\x1b[?1049l" in transcript and b"\x1b[?2004l" in transcript

        # Only backend CSI framing is allowed. Check every escape and position,
        # then inspect printable payload independently of renderer style splits.
        framing = re.compile(rb"\x1b\[([0-9;?]*)([A-Za-z])")
        for match in framing.finditer(transcript):
            params, final = match.groups()
            if final == b"H":
                row, column = map(int, params.split(b";"))
                assert 1 <= row <= 32 and 1 <= column <= 120, "cursor escaped screen"
            else:
                assert final in [b"m", b"h", b"l", b"J", b"c", b"u"], "unexpected backend command"
                if final == b"c":
                    assert params == b"", "unexpected device-attributes query"
                if final == b"u":
                    assert params == b"?", "unexpected keyboard-enhancement query"
                if final in [b"h", b"l"]:
                    assert params in [b"?1049", b"?2004", b"?25", b"?2026",
                                      b"?1000", b"?1002", b"?1003", b"?1015", b"?1006"]
        text = framing.sub(b"", transcript).decode("utf-8")
        assert not any(ord(c) < 32 and c not in "\r\n\t" or 127 <= ord(c) <= 159 for c in text), "raw terminal control"
        assert "\u202e" not in text, "raw bidi control"
        screen = rendered_screen(transcript, rows=32, columns=120)
        for literal in [r"\u{1b}]52;c;Y2xpcA==\u{7}", r"\u{1b}]0;hostile-title",
                        r"\u{1b}[999;999H", r"\u{9b}31m\u{202e}spoof\u{0}",
                        "東京 e\u0301 👩🏽‍🔬"]:
            assert literal in screen, "missing visible source representation: " + repr(literal)
        with open(os.path.join(root, "assignment.work", "main.rs"), "rb") as file:
            assert file.read() == source, "display changed source bytes"
    finally:
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
        try:
            termios.tcsetattr(slave, termios.TCSANOW, before)
        except termios.error:
            pass  # The child-side assertion above verifies restoration.
        os.close(master)
        os.close(slave)
print("Safe PTY source, trusted framing, exact file bytes and terminal restoration passed")
