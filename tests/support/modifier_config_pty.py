#!/usr/bin/env python3
"""Verify plain and kitty-protocol primary-modifier behavior in real PTYs."""
import fcntl
import io
import json
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
DISAMBIGUATE_ESCAPE_CODES = 1
REPORT_ALTERNATE_KEYS = 4
EXPECTED_FLAGS = DISAMBIGUATE_ESCAPE_CODES | REPORT_ALTERNATE_KEYS
EXPECTED_PUSH = b"\x1b[>" + str(EXPECTED_FLAGS).encode() + b"u"
MANIFEST = b'''format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Modifier PTY"
toolchain = "fixture"
edition = "2024"
allowed_paths = ["*.rs", "Cargo.toml"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
'''
STARTER = b"fn main() {}\n"
SHIFTED = b"(A"


def write_package(root):
    package = root / "assignment.rta"
    with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
        for name, content in [
            ("assignment.toml", MANIFEST),
            ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
            ("starter/main.rs", STARTER),
        ]:
            entry = tarfile.TarInfo(name)
            entry.size = len(content)
            entry.mode = 0o600
            archive.addfile(entry, io.BytesIO(content))
    data = package.read_bytes()
    while data.endswith(bytes(512)):
        data = data[:-512]
    package.write_bytes(data + bytes(1024))
    return package


def command_is_inactive(path):
    try:
        return json.loads(path.read_bytes())["active"] is False
    except (FileNotFoundError, json.JSONDecodeError, KeyError):
        return False


def exercise(root, kitty_protocol):
    package = write_package(root)
    config = root / "config" / "rustrace"
    config.mkdir(parents=True)
    (config / "config.toml").write_text('modifier = "command"\n')
    tool_bytes = pathlib.Path(__file__).with_name("command_rustup.py").read_bytes()
    bin_dir = root / "bin"
    (bin_dir / "v1").mkdir(parents=True)
    for target in [bin_dir / "rustup"] + [
        bin_dir / "v1" / name
        for name in [
            "rustc",
            "cargo",
            "rustdoc",
            "rust-analyzer",
            "cargo-clippy",
            "cargo-fmt",
            "rustfmt",
        ]
    ]:
        target.write_bytes(tool_bytes)
        target.chmod(0o755)

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
    environment = {
        **os.environ,
        "TERM": "xterm-256color",
        "XDG_CONFIG_HOME": str(root / "config"),
        "RUSTUP_AUTO_INSTALL": "0",
        "PATH": str(bin_dir) + os.pathsep + os.environ["PATH"],
    }
    process = subprocess.Popen(
        [sys.executable, "-c", wrapper, BINARY, "work", str(package)],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        preexec_fn=child,
        env=environment,
        cwd=root,
    )
    transcript = bytearray()
    answered_probe = False
    shifted_sent = False
    save_sent = False
    quit_sent = False
    deadline = time.monotonic() + 30

    def drain():
        if select.select([master], [], [], 0.05)[0]:
            try:
                transcript.extend(os.read(master, 65536))
            except OSError:
                pass

    try:
        while process.poll() is None and time.monotonic() < deadline:
            drain()
            if not answered_probe and b"\x1b[?u\x1b[c" in transcript:
                response = b"\x1b[?0u\x1b[?1;0c" if kitty_protocol else b"\x1b[?1;0c"
                os.write(master, response)
                answered_probe = True

            pushes = re.findall(rb"\x1b\[>([0-9]+)u", transcript)
            if kitty_protocol and pushes:
                assert pushes == [str(EXPECTED_FLAGS).encode()], (
                    "unexpected keyboard-enhancement pushes: " + repr(pushes)
                )
            if kitty_protocol and pushes and b" files" in transcript and not shifted_sent:
                os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.
                os.write(master, b"\x1b[57:40;2u\x1b[97:65;2u")
                shifted_sent = True

            saved_path = root / "assignment.work" / "main.rs"
            shifted_persisted = saved_path.is_file() and saved_path.read_bytes() == SHIFTED + STARTER
            if kitty_protocol and shifted_sent and shifted_persisted and not save_sent:
                # Super+s is encoded with modifier value 1 + 8. DISAMBIGUATE_ESCAPE_CODES
                # is sufficient for crossterm to preserve the Super modifier.
                os.write(master, b"\x1b[115;9u")
                save_sent = True

            if kitty_protocol:
                save_observed = save_sent and b"File saved" in transcript
                command_finished = command_is_inactive(
                    root / "assignment.work" / ".rustrace" / "command-activity.json"
                )
                if save_observed and command_finished and not quit_sent:
                    os.write(master, b"\x11")
                    quit_sent = True
            elif (
                not quit_sent
                and b"Command modifier unavailable in this terminal" in transcript
            ):
                os.write(master, b"\x11")
                quit_sent = True

            assert len(transcript) < 2 * 1024 * 1024, "capture limit exceeded"

        assert process.poll() is not None, "CLI timed out: " + repr(bytes(transcript[:5000]))
        while select.select([master], [], [], 0.05)[0]:
            before_length = len(transcript)
            drain()
            if len(transcript) == before_length:
                break
        assert process.returncode == 0, repr(bytes(transcript))
        assert answered_probe, "keyboard enhancement probe was not observed"
        assert quit_sent, "normal quit was not sent"

        if kitty_protocol:
            assert shifted_sent, "kitty shifted-key sequences were not sent"
            assert save_sent, "kitty Super+s sequence was not sent"
            assert (root / "assignment.work" / "main.rs").read_bytes() == SHIFTED + STARTER
            pushes = re.findall(rb"\x1b\[>([0-9]+)u", transcript)
            assert pushes == [str(EXPECTED_FLAGS).encode()]
            assert transcript.count(b"\x1b[<1u") == 1, "pushed flags were not popped once"
            assert b"Command modifier unavailable in this terminal" not in transcript
        else:
            checked_transcript = bytes(transcript)
            if os.environ.get("RUSTRACE_TEST_INJECT_ENHANCEMENT_PUSH") == "1":
                checked_transcript += EXPECTED_PUSH
            push = re.search(rb"\x1b\[>([0-9]+)u", checked_transcript)
            assert push is None, (
                "keyboard-enhancement push detected: CSI > "
                + str(int(push.group(1)))
                + " u"
            )
            assert b"\x1b[<1u" not in transcript, "unpushed flags were popped"
            assert (root / "assignment.work" / "main.rs").read_bytes() == STARTER

        assert b"TERMINAL_RESTORED" in transcript, "terminal was not restored"
        assert b"\x1b[?1049l" in transcript and b"\x1b[?2004l" in transcript
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


with tempfile.TemporaryDirectory(prefix="rustrace-modifier-") as root_text:
    root = pathlib.Path(root_text)
    (root / "plain").mkdir()
    (root / "kitty").mkdir()
    exercise(root / "plain", kitty_protocol=False)
    exercise(root / "kitty", kitty_protocol=True)

print("Plain fallback plus kitty shifted characters, Super+s, flags, and restoration passed")
