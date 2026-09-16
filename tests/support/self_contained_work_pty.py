"""Run real menu Check beneath a foreign Cargo workspace, then reject its unsafe twin."""
import fcntl
import io
import json
import os
from pathlib import Path
import pty
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

BINARY = str(Path(sys.argv[1]).resolve())
MANIFEST = b'''format_version = 2
course_id = "course"
assignment_id = "isolated"
assignment_version = "v1"
title = "Self-contained package"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["Cargo.toml", "Cargo.lock", "src/*.rs"]
[commands]
check = ["cargo", "check", "--locked", "--offline"]
test = ["cargo", "test", "--locked", "--offline"]
run = ["cargo", "run", "--locked", "--offline"]
clippy = ["cargo", "clippy", "--locked", "--offline"]
format = ["cargo", "fmt"]
'''
CARGO = b'''[package]
name = "isolated"
version = "0.1.0"
edition = "2024"

[workspace]
'''
LOCK = b'''version = 4

[[package]]
name = "isolated"
version = "0.1.0"
'''
MESSAGE = b"assignment starter must be a self-contained package: add an empty [workspace] table to starter/Cargo.toml"


def write_package(path, cargo):
    entries = [
        ("assignment.toml", MANIFEST),
        ("starter/Cargo.lock", LOCK),
        ("starter/Cargo.toml", cargo),
        ("starter/src/main.rs", b"fn main() {}\n"),
        ("test-cases/sample.expected", b""),
        ("test-cases/sample.in", b""),
    ]
    with tarfile.open(path, "w", format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in entries:
            entry = tarfile.TarInfo(name)
            entry.size = len(contents)
            entry.mode = 0o644
            archive.addfile(entry, io.BytesIO(contents))
    data = path.read_bytes()
    while data.endswith(bytes(512)):
        data = data[:-512]
    path.write_bytes(data + bytes(1024))


def check_in_pty(package, environment):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
    before = termios.tcgetattr(slave)

    def child_setup():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    wrapper = (
        "import subprocess,sys,termios; before=termios.tcgetattr(0); "
        "result=subprocess.run(sys.argv[1:]); "
        "assert termios.tcgetattr(0)==before, 'terminal mode leaked'; "
        "print('TERMINAL_RESTORED',flush=True); sys.exit(result.returncode)"
    )
    process = subprocess.Popen(
        [sys.executable, "-c", wrapper, BINARY, "work", str(package)], stdin=slave, stdout=slave, stderr=slave,
        cwd=package.parent, env=environment, preexec_fn=child_setup,
    )
    transcript = bytearray()
    menu_sent = check_sent = quit_sent = False
    capture = None
    deadline = time.monotonic() + 60
    try:
        while process.poll() is None and time.monotonic() < deadline:
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))
            screen = rendered_screen(transcript, rows=32, columns=120)
            if not menu_sent and " files" in screen:
                os.write(master, b"\x1b[18~")  # F7 command menu.
                menu_sent = True
            elif menu_sent and not check_sent and " MENU " in screen and "Check" in screen:
                os.write(master, b"\r")
                check_sent = True
            captures = list(package.with_suffix(".work").glob(".rustrace/command-*-capture.json"))
            if captures and capture is None:
                try:
                    capture = json.loads(captures[0].read_bytes())
                except (FileNotFoundError, json.JSONDecodeError):
                    pass
            activity = package.with_suffix(".work") / ".rustrace" / "command-activity.json"
            try:
                inactive = json.loads(activity.read_bytes())["active"] is False
            except (FileNotFoundError, json.JSONDecodeError):
                inactive = False
            if capture is not None and inactive and not quit_sent:
                os.write(master, b"\x11")
                quit_sent = True
            assert len(transcript) < 2 * 1024 * 1024, "PTY transcript exceeded bound"
        assert process.poll() is not None, "PTY timed out: " + repr(bytes(transcript[-5000:]))
        while select.select([master], [], [], 0.05)[0]:
            chunk = os.read(master, 65536)
            if not chunk:
                break
            transcript.extend(chunk)
        assert process.returncode == 0, repr(bytes(transcript[-5000:]))
        assert menu_sent and check_sent and quit_sent, repr(bytes(transcript[-5000:]))
        assert capture["execution"]["outcome"] == {"kind": "exited", "code": 0}, capture
        assert capture["diagnostics"]["identity"]["action"] == "check", capture
        assert capture["diagnostics"]["outcome"] == "success", capture
        assert capture["diagnostics"]["issues"] == [], capture
        assert capture["diagnostics"]["diagnostics"] == [], capture
        assert all(b"believes it's in a workspace" not in bytes.fromhex(chunk["bytes_hex"]) for chunk in capture["capture"]), capture
        assert b"believes it's in a workspace" not in transcript, repr(bytes(transcript))
        assert b"\x1b[?1049l" in transcript, "terminal cleanup missing"
        assert b"TERMINAL_RESTORED" in transcript, "terminal mode leaked"
        print("foreign workspace menu Check: exit 0, zero diagnostics, terminal restored")
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


with tempfile.TemporaryDirectory(prefix="rustrace-self-contained-") as directory:
    root = Path(directory)
    environment = {**os.environ, "TERM": "xterm-256color", "XDG_CONFIG_HOME": str(root / "xdg"), "RUSTUP_AUTO_INSTALL": "0"}
    environment.pop("TMPDIR", None)
    (root / "Cargo.toml").write_text('[workspace]\nmembers = ["other"]\n')
    (root / "other" / "src").mkdir(parents=True)
    (root / "other" / "Cargo.toml").write_text('[package]\nname = "other"\nversion = "0.1.0"\nedition = "2024"\n')
    (root / "other" / "src" / "lib.rs").write_text("")
    nested = root / "assignments"
    nested.mkdir()
    package = nested / "assignment.rta"
    write_package(package, CARGO)
    check_in_pty(package, environment)
    unsafe = nested / "missing-workspace.rta"
    write_package(unsafe, CARGO.replace(b"\n[workspace]\n", b"\n"))
    result = subprocess.run([BINARY, "work", str(unsafe)], cwd=nested, env=environment, capture_output=True, timeout=30)
    assert result.returncode != 0, result.stdout + result.stderr
    assert MESSAGE in result.stdout + result.stderr, result.stdout + result.stderr
    assert b"Toolchain:" not in result.stdout + result.stderr, "invalid starter reached tool discovery"
    assert not unsafe.with_suffix(".work").exists(), "invalid starter was published"
    assert not list(nested.glob("missing-workspace.work.*")), "invalid extraction left staging artifacts"
    assert b"believes it's in a workspace" not in result.stdout + result.stderr
    print("same v2 archive without [workspace]: rejected at extraction with required remedy")
