"""Drive every T10.11 editing convenience through the real 80x24 work TUI."""

import fcntl
import io
import json
import os
import pty
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
MANIFEST = b'''format_version = 1
course_id = "course"
assignment_id = "editing-conveniences"
assignment_version = "v1"
title = "Editing conveniences"
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
EXPECTED = b"// fn main() {\n//     ()\n//     if {\n//     }\n// }"


def package_at(root):
    package = os.path.join(root, "assignment.rta")
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in [
            ("assignment.toml", MANIFEST),
            ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
            ("starter/main.rs", b""),
        ]:
            info = tarfile.TarInfo(name)
            info.size = len(contents)
            info.mode = 0o600
            archive.addfile(info, io.BytesIO(contents))
    data = buffer.getvalue()
    while data.endswith(bytes(512)):
        data = data[:-512]
    with open(package, "wb") as file:
        file.write(data + bytes(1024))
    return package


def journal_events(workspace):
    state = os.path.join(workspace, ".rustrace")
    with open(os.path.join(state, "session.json"), "rb") as file:
        session_id = json.load(file)["session_id"]
    connection = sqlite3.connect(os.path.join(state, session_id + ".sqlite"))
    try:
        rows = connection.execute(
            "SELECT payload FROM events ORDER BY sequence"
        ).fetchall()
    finally:
        connection.close()
    return [json.loads(raw) for (raw,) in rows]


with tempfile.TemporaryDirectory(prefix="rustrace-editing-conveniences-pty-") as root:
    package = package_at(root)
    workspace = os.path.join(root, "assignment.work")
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
        [sys.executable, "-c", wrapper, BINARY, "work", package],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        preexec_fn=child,
        env={**os.environ, "TERM": "xterm-256color"},
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
            assert len(transcript) <= 2 * 1024 * 1024

    try:
        deadline = time.monotonic() + 20
        while b" files" not in transcript and time.monotonic() < deadline:
            drain(0.1)
        assert b" files" in transcript, repr(bytes(transcript))
        os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.
        drain(0.2)

        keys = [
            b"fn main() ",
            b"{",                 # auto-close {}
            b"\r",                # indent between the braces
            b"(", b")",          # auto-close and over-type
            b"\r",                # copy the current indentation
            b"[", b"\x7f",       # remove an empty automatic pair
            b"if ", b"{",        # create an inner automatic pair
            b"\x1b[3~",           # remove its closer
            b"\r", b"}",         # indent after {, then dedent the closer
            b"\x01",              # Ctrl-A selects every line
            b"\x1b[47;5u",        # CSI-u Ctrl-/
        ]
        for key in keys:
            os.write(master, key)
            drain(0.12)

        deadline = time.monotonic() + 10
        while (not os.path.isfile(os.path.join(workspace, "main.rs"))
               or open(os.path.join(workspace, "main.rs"), "rb").read() != EXPECTED):
            assert time.monotonic() < deadline, "autosave did not persist editing conveniences"
            drain(0.05)
        os.write(master, b"\x11")

        deadline = time.monotonic() + 20
        while process.poll() is None and time.monotonic() < deadline:
            drain(0.1)
        assert process.poll() is not None, repr(bytes(transcript[-5000:]))
        drain(0.2)
        assert process.returncode == 0, repr(bytes(transcript[-5000:]))
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

    with open(os.path.join(workspace, "main.rs"), "rb") as file:
        assert file.read() == EXPECTED

    events = journal_events(workspace)
    transactions = [
        event["event"]["payload"]
        for event in events
        if event["event"]["type"] == "file_edited"
    ]
    assert transactions
    assert all(transaction["origin"] == "keyboard" for transaction in transactions)
    assert any(
        len(transaction["edits"]) == 1
        and transaction["edits"][0]["inserted_text"] == "\n    \n"
        for transaction in transactions
    ), "Enter between {} did not stay one exact transaction"
    assert any(
        len(transaction["edits"]) == 1
        and transaction["edits"][0]["inserted_text"] == "()"
        for transaction in transactions
    ), "automatic pair insertion missing"
    assert not any(
        len(transaction["edits"]) == 1
        and transaction["edits"][0]["inserted_text"] == ")"
        for transaction in transactions
    ), "over-type inserted a second closer"
    assert any(
        len(transaction["edits"]) == 1
        and transaction["edits"][0]["inserted_text"] == ""
        and transaction["edits"][0]["end_byte"]
        - transaction["edits"][0]["start_byte"] == 2
        for transaction in transactions
    ), "empty-pair Backspace was not atomic"
    assert any(
        len(transaction["edits"]) == 1
        and transaction["edits"][0]["inserted_text"] == "    }"
        and transaction["edits"][0]["end_byte"]
        - transaction["edits"][0]["start_byte"] == 8
        for transaction in transactions
    ), "closer dedent did not replace one indentation unit"
    assert any(
        len(transaction["edits"]) == 5
        and all(edit["inserted_text"] == "// " for edit in transaction["edits"])
        for transaction in transactions
    ), "Ctrl-/ selection toggle was not one transaction"

    inspected = subprocess.run(
        [BINARY, "work", package, "--inspect"],
        capture_output=True,
        cwd=root,
        timeout=20,
    )
    assert inspected.returncode == 0, inspected.stdout + inspected.stderr
    assert json.dumps(EXPECTED.decode()).encode() in inspected.stdout, inspected.stdout

print("80x24 editing conveniences, exact transactions, replay inspect, and terminal cleanup passed")
