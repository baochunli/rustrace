"""T8.7 real 80x24 production composition driver."""

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
import termios
import time

from test_home import isolate
isolate()


BINARY = str(pathlib.Path(sys.argv[1]).resolve())
ROOT = pathlib.Path(sys.argv[2]).resolve()
MODE = sys.argv[3]
WARNING = "Paste blocked:"
SENTINEL = b"T87_REJECTED_CLIPBOARD_SENTINEL"
MANIFEST = b'''format_version = 1
course_id = "course"
assignment_id = "t8-7"
assignment_version = "v1"
title = "T8.7 integrated production"
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


def package_at(path):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in [
            ("assignment.toml", MANIFEST),
            ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
            ("starter/a.rs", b"source"),
            ("starter/b.rs", b"destination"),
        ]:
            info = tarfile.TarInfo(name)
            info.size = len(contents)
            info.mode = 0o600
            archive.addfile(info, io.BytesIO(contents))
    data = stream.getvalue()
    while data.endswith(bytes(512)):
        data = data[:-512]
    path.write_bytes(data + bytes(1024))


def rendered_screen(data):
    screen = [[" "] * 80 for _ in range(24)]
    row = column = 0
    for part in re.split(r"(\x1b\[[0-?]*[ -/]*[@-~])", data.decode("utf-8", "replace")):
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
                row = min(23, row + amount)
            elif code == "C":
                column = min(79, column + amount)
            elif code == "D":
                column = max(0, column - amount)
            elif code == "J" and values[0] == 2:
                screen = [[" "] * 80 for _ in range(24)]
            elif code == "K" and 0 <= row < 24:
                screen[row][column:] = [" "] * (80 - column)
            continue
        for character in part:
            if character == "\r":
                column = 0
            elif character == "\n":
                row = min(23, row + 1)
            elif character >= " ":
                if 0 <= row < 24 and 0 <= column < 80:
                    screen[row][column] = character
                column += 1
    return "\n".join("".join(line) for line in screen)


def command(*args, expected=0):
    result = subprocess.run([BINARY, *map(str, args)], capture_output=True, timeout=30)
    assert result.returncode == expected, (args, result.returncode, result.stdout, result.stderr)
    return result.stdout + result.stderr


def scan_files(root, needle):
    for path in root.rglob("*"):
        assert needle.decode() not in str(path), "rejected clipboard payload retained in path " + str(path)
        if not path.is_file() or path.stat().st_size > 40 * 1024 * 1024:
            continue
        with path.open("rb") as stream:
            tail = b""
            while chunk := stream.read(65536):
                combined = tail + chunk
                assert needle not in combined, "rejected clipboard payload retained in " + str(path)
                tail = combined[-len(needle):]


def exercise(name, setup, incoming=None, internal=False, p2=False):
    case = ROOT / name
    case.mkdir()
    package = case / "assignment.rta"
    package_at(package)
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    before = termios.tcgetattr(slave)

    def child_setup():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    wrapper = (
        "import subprocess,termios,sys; before=termios.tcgetattr(0); "
        "result=subprocess.run(sys.argv[1:]); "
        "assert termios.tcgetattr(0)==before, 'raw mode leaked'; "
        "print('TERMINAL_RESTORED',flush=True); sys.exit(result.returncode)"
    )
    process = subprocess.Popen(
        [sys.executable, "-c", wrapper, BINARY, "work", str(package)],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        cwd=case,
        env={**os.environ, "TERM": "xterm-256color", "RUSTUP_AUTO_INSTALL": "0"},
        preexec_fn=child_setup,
    )
    transcript = bytearray()
    policy_screen = None

    def pump_until(predicate, label, seconds=20):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([master], [], [], 0.025)[0]:
                try:
                    transcript.extend(os.read(master, 65536))
                except OSError:
                    pass
            assert len(transcript) <= 2 * 1024 * 1024
            if predicate():
                return
        raise AssertionError(name + ": timed out waiting for " + label)

    def send(value):
        os.write(master, value)
        until = time.monotonic() + 0.12
        pump_until(lambda: time.monotonic() >= until, "input settle", 2)

    try:
        pump_until(lambda: b" files" in transcript, "production editor")
        os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.
        for keys in setup:
            send(keys)
        if p2:
            send(b"\x1a")
            send(b"\x19")
            (case / "assignment.work/a.rs").write_bytes(b"P2_EXTERNAL_SENTINEL")
            send(b"\x13")
            pump_until(lambda: "External changes are not accepted" in rendered_screen(transcript), "P2 warning")
            policy_screen = rendered_screen(transcript)
            pump_until(
                lambda: list((case / "assignment.work/.rustrace").glob("restored-*.json")),
                "P2 restoration receipt",
            )
            send(b"\x13")
            send(b"\x1a\x19")
        else:
            send(incoming or b"\x1b[200~" + SENTINEL + b"\x1b[201~")
            pump_until(
                lambda: (WARNING in rendered_screen(transcript)) != internal,
                "clipboard policy result",
            )
            policy_screen = rendered_screen(transcript)
            assert SENTINEL.decode() not in rendered_screen(transcript)
        if internal:
            send(b"\x1a\x19")
            pump_until(
                lambda: (case / "assignment.work/b.rs").read_bytes() == b"source",
                "internal paste autosave",
            )
        if name.startswith("search"):
            send(b"\x1b")
        elif name.startswith(("create", "rename")):
            send(b"\x1b")
        elif name.startswith("tree"):
            send(b"\x1bOQ")
        elif name.startswith("confirmation"):
            send(b"n")
        send(b"\x11")
        pump_until(
            lambda: process.poll() is not None
            or "Discard unsaved buffer changes?" in rendered_screen(transcript),
            "quit outcome",
        )
        if process.poll() is None:
            send(b"y")
        pump_until(lambda: process.poll() is not None, "normal quit")
        assert process.returncode == 0
        assert b"TERMINAL_RESTORED" in transcript
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
        (case / "work.pty").write_bytes(transcript)
        if policy_screen is not None:
            (case / "policy-screen.txt").write_text(policy_screen + "\n")

    work = case / "assignment.work"
    inspect = command("work", package, "--inspect")
    (case / "inspect.txt").write_bytes(inspect)
    metadata = json.loads((work / ".rustrace/session.json").read_bytes())
    connection = sqlite3.connect(work / ".rustrace" / (metadata["session_id"] + ".sqlite"))
    rows = connection.execute("SELECT payload FROM events ORDER BY sequence").fetchall()
    connection.close()
    events = [json.loads(raw) for (raw,) in rows]
    rejections = [event for event in events if event["event"]["type"] == "paste_rejected"]
    assert len(rejections) == (0 if internal or p2 else 1)
    for event in rejections:
        assert set(event["event"]["payload"]) == {"reason", "channel"}
        assert len(json.dumps(event, separators=(",", ":")).encode()) <= 1024
    if internal:
        copied = [event for event in events if event["event"]["type"] == "clipboard_copied"]
        pasted = [event for event in events if event["event"]["type"] == "internal_paste"]
        assert len(copied) == len(pasted) == 1
        assert pasted[0]["event"]["payload"]["source"] == {
            "session_id": copied[0]["session_id"],
            "sequence": copied[0]["sequence"],
            "event_hash": copied[0]["event_hash"],
        }
    if p2:
        origins = [event["event"]["payload"]["origin"] for event in events
                   if event["event"]["type"] == "file_edited"]
        assert origins == ["keyboard", "undo", "redo"]
        assert any(event["event"]["type"] == "external_observation" for event in events)
        assert not rejections
        assert (work / "a.rs").read_bytes() == b"!source"

    bundle = case / "submission.zip"
    submit = command("submit", work, "--student-id", "student-1", "--output", bundle)
    verify = command("verify", bundle)
    scan = command("scan", case)
    (case / "submit.txt").write_bytes(submit)
    (case / "verify.txt").write_bytes(verify)
    (case / "scan.txt").write_bytes(scan)
    assert b"Package structure        OK" in verify
    assert b"Replay                   OK" in verify
    assert b"submission.zip" in scan
    scan_files(work, SENTINEL)
    assert SENTINEL not in bundle.read_bytes()
    return {
        "name": name,
        "bundle": str(bundle),
        "session": metadata["session_id"],
        "events": len(events),
        "paste_rejections": len(rejections),
        "internal_pastes": sum(event["event"]["type"] == "internal_paste" for event in events),
        "external_observations": sum(event["event"]["type"] == "external_observation" for event in events),
    }


def run_ingress_red():
    result = exercise("search-red", [b"\x06"])
    print(json.dumps(result, sort_keys=True))


def run_suite():
    results = []
    for name, setup in [
        ("editor", []),
        ("equal-text", []),
        ("missing-live-source", []),
        ("search", [b"\x06"]),
        ("create", [b"\x1bOQ", b"n"]),
        ("rename", [b"\x1bOQ", b"r"]),
        ("tree", [b"\x1bOQ"]),
        ("confirmation", [b"!", b"\x17"]),
    ]:
        incoming = (b"\x1b[200~source\x1b[201~" if name == "equal-text"
                    else b"\x16" if name == "missing-live-source" else None)
        results.append(exercise(name, setup, incoming=incoming))
    for cut in (False, True):
        setup = [b"\x01", b"\x18" if cut else b"\x03"]
        if cut:
            setup += [b"\x1a", b"\x19"]
        setup += [b"\x17"]
        if cut:
            setup += [b"y"]
        setup += [b"\x01"]
        results.append(exercise("cut-flow" if cut else "copy-flow", setup, incoming=b"\x16", internal=True))
    results.append(exercise("p2-separate", [b"!"], p2=True))
    (ROOT / "summary.json").write_text(json.dumps(results, indent=2, sort_keys=True))
    print("T8.7 production clipboard/P2/verify/scan suite passed")


if MODE == "ingress-red":
    run_ingress_red()
elif MODE == "suite":
    run_suite()
else:
    raise AssertionError("unknown T8.7 mode: " + MODE)
