#!/usr/bin/env python3
"""Run the packaged test-case picker end to end in a real v2 workspace."""
import errno
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


def fake_tool():
    program = pathlib.Path(__file__).name
    args = sys.argv[1:]
    if program == "rustup":
        if args == ["--version"]:
            print("rustup 1.28.1 (test-case picker fixture)")
        elif args == ["toolchain", "list"]:
            print("fixture (default)")
        elif args == ["show", "active-toolchain"]:
            print("fixture (environment override)")
        elif len(args) == 4 and args[:3] == ["which", "--toolchain", "fixture"]:
            print(pathlib.Path(__file__).resolve().parent / args[3])
        elif len(args) >= 3 and args[:2] == ["run", "fixture"]:
            os.execv(args[2], args[2:])
        else:
            raise AssertionError(f"unsupported rustup invocation: {args!r}")
        return

    if args in (["-vV"], ["-V"], ["--version"]):
        print(f"{program} 1.98.1 (test-case picker fixture)")
        if program == "rustc":
            print("host: aarch64-apple-darwin\nrelease: 1.98.1")
        return
    if program == "rust-analyzer":
        raise SystemExit(1)

    assert program == "cargo", (program, args)
    target = pathlib.Path.cwd() / "target"
    target.mkdir(exist_ok=True)
    if args == ["check", "--message-format=json", "--locked"]:
        (target / "check-ran").write_text("ready")
        os.write(1, b'{"reason":"build-finished","success":true}\n')
        time.sleep(0.2)
        raise SystemExit(0)
    assert args == ["run", "--locked"], args
    data = sys.stdin.buffer.read()
    if data == b"pass\n":
        name = "01-pass"
        output = b"OK\n"
    elif data == b"fail\n":
        name = "02-fail"
        output = b"\xff"
    else:
        raise AssertionError(f"unexpected packaged stdin: {data!r}")
    order_path = target / "test-case-order.txt"
    invocation = (
        len(order_path.read_text(encoding="ascii").splitlines())
        if order_path.exists()
        else 0
    )
    os.write(1, output)
    with order_path.open("a", encoding="ascii") as order:
        order.write(name + "\n")
        order.flush()
    if invocation == 2:
        (target / "cancel-ready").write_text("output written\n", encoding="ascii")
        while True:
            time.sleep(1)
    time.sleep(0.2)


if pathlib.Path(__file__).name != "test_case_picker_pty.py":
    fake_tool()
    raise SystemExit(0)


from test_home import isolate
isolate()

binary = str(pathlib.Path(sys.argv[1]).resolve())
root = pathlib.Path(tempfile.mkdtemp(prefix="rustrace-test-case-picker-"))
bin_dir = root / "bin"
bin_dir.mkdir()
tool_bytes = pathlib.Path(__file__).read_bytes()
for name in ["rustup", "rustc", "cargo", "rustdoc", "rust-analyzer"]:
    tool = bin_dir / name
    tool.write_bytes(tool_bytes)
    tool.chmod(0o755)

manifest = b'''format_version = 2
course_id = "course"
assignment_id = "test-case-picker"
assignment_version = "v1"
title = "Test case picker PTY"
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
entries = [
    ("assignment.toml", manifest),
    ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
    ("starter/main.rs", b"fn main() {}\n"),
    ("test-cases/01-pass.in", b"pass\n"),
    ("test-cases/01-pass.expected", b"OK\n"),
    ("test-cases/02-fail.in", b"fail\n"),
    ("test-cases/02-fail.expected", b"\x1b"),
]
package = root / "assignment.rta"
with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
    for name, contents in entries:
        info = tarfile.TarInfo(name)
        info.size = len(contents)
        info.mode = 0o600
        info.uid = info.gid = info.mtime = 0
        archive.addfile(info, io.BytesIO(contents))
data = package.read_bytes()
while data.endswith(bytes(512)):
    data = data[:-512]
package.write_bytes(data + bytes(1024))

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
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
    [sys.executable, "-c", wrapper, binary, "work", str(package)],
    stdin=slave,
    stdout=slave,
    stderr=slave,
    cwd=root,
    preexec_fn=child_setup,
    env={
        **os.environ,
        "PATH": str(bin_dir) + os.pathsep + os.environ["PATH"],
        "TERM": "xterm-256color",
        "XDG_CONFIG_HOME": str(root / "xdg"),
        "RUSTUP_AUTO_INSTALL": "0",
    },
)
transcript = bytearray()
deadline = time.monotonic() + 45


def read_once(timeout=0.05):
    if not select.select([master], [], [], timeout)[0]:
        return False
    try:
        chunk = os.read(master, 65536)
    except OSError as error:
        if error.errno == errno.EIO:
            return False
        raise
    transcript.extend(chunk)
    assert len(transcript) < 2 * 1024 * 1024, "PTY transcript exceeded fixture cap"
    return bool(chunk)


def rendered_screen():
    screen = [[" "] * 120 for _ in range(40)]
    row = column = 0
    text = transcript.decode("utf-8", "replace")
    for part in re.split(r"(\x1b\[[0-?]*[ -/]*[@-~])", text):
        if part.startswith("\x1b["):
            if part[2:3] == "?":
                if part == "\x1b[?1049h":
                    screen = [[" "] * 120 for _ in range(40)]
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
                screen = [[" "] * 120 for _ in range(40)]
            elif code == "K" and 0 <= row < 40:
                start = 0 if values[0] in (1, 2) else column
                end = column + 1 if values[0] == 1 else 120
                screen[row][start:end] = [" "] * (end - start)
            continue
        for character in part:
            if character == "\r":
                column = 0
            elif character == "\n":
                row += 1
            elif character >= " ":
                if 0 <= row < 40 and 0 <= column < 120:
                    screen[row][column] = character
                column += 1
    return "\n".join("".join(line) for line in screen)


def wait_for(predicate, label):
    while time.monotonic() < deadline:
        read_once()
        if predicate():
            return
        if process.poll() is not None:
            break
    raise AssertionError(f"timed out waiting for {label}\n{rendered_screen()}")


def send(data):
    assert process.poll() is None
    os.write(master, data)


success = False
try:
    wait_for(lambda: " files" in rendered_screen(), "workspace")
    workspace = root / "assignment.work"
    cases = root / "test-cases"
    assert (cases / "01-pass.in").read_bytes() == b"pass\n"
    assert (cases / "02-fail.expected").read_bytes() == b"\x1b"

    send(b"\x1b[14~")
    wait_for(
        lambda: all(
            value in rendered_screen()
            for value in ["Test cases", "01-pass", "02-fail", "Run all"]
        ),
        "packaged case modal",
    )
    send(b"\x1b[A\r")
    order_path = workspace / "target/test-case-order.txt"
    wait_for(
        lambda: order_path.exists() and "01-pass" in order_path.read_text(),
        "first serialized case",
    )
    assert " TEST CASES " not in rendered_screen(), "modal stayed open during command"
    send(b"\x1bOP\x1b[18~\x1b[20~")  # F1/F7/F9 are inert during the sequence.
    time.sleep(0.05)
    while read_once(0.01):
        pass
    assert not any(
        modal in rendered_screen() for modal in [" KEYBINDS ", " MENU ", " CONSOLE "]
    ), rendered_screen()
    wait_for(
        lambda: order_path.read_text().splitlines() == ["01-pass", "02-fail"]
        and " TEST CASES " in rendered_screen()
        and "PASS" in rendered_screen()
        and "FAIL line 1" in rendered_screen(),
        "serial pass/fail results and reopened modal",
    )
    screen = rendered_screen()
    assert " ERROR " not in screen, screen

    send(b"\x1b")
    wait_for(
        lambda: " TEST CASES " not in rendered_screen()
        and "Test case 02-fail: FAIL at line 1" in rendered_screen(),
        "comparison summary after command output",
    )
    screen = rendered_screen()
    assert 'expected (1 bytes) "\\u{1b}"' in screen, screen
    assert 'got (1 bytes) "\\xff"' in screen, screen
    assert "\ufffd" not in screen, screen
    assert "Test case 01-pass: PASS" in screen, screen

    # Existing cancellation stops the remaining queue and reopens the picker.
    send(b"\x1b[14~\x1b[A\r")
    cancel_ready = workspace / "target/cancel-ready"
    wait_for(
        lambda: cancel_ready.exists()
        and order_path.read_text().splitlines() == ["01-pass", "02-fail", "01-pass"]
        and "OK" in rendered_screen(),
        "cancellable Run all output",
    )
    send(b"\x1b")
    wait_for(
        lambda: " TEST CASES " in rendered_screen()
        and "ERROR" in rendered_screen()
        and order_path.read_text().splitlines() == ["01-pass", "02-fail", "01-pass"],
        "cancelled queue result and reopened modal",
    )
    time.sleep(0.3)
    assert order_path.read_text().splitlines() == ["01-pass", "02-fail", "01-pass"]
    send(b"\x1b")
    wait_for(lambda: " TEST CASES " not in rendered_screen(), "cancelled modal close")
    time.sleep(0.1)

    # Open Test cases through the menu from Console, run one, then restore Console.
    send(b"\x1b[20~")
    wait_for(lambda: " CONSOLE " in rendered_screen(), "console origin")
    send(b"\x1b[18~" + b"\x1b[B" * 9 + b"\r")
    wait_for(lambda: " TEST CASES " in rendered_screen(), "menu Test cases entry")
    send(b"\r")
    wait_for(
        lambda: order_path.read_text().splitlines()
        == ["01-pass", "02-fail", "01-pass", "01-pass"]
        and " TEST CASES " in rendered_screen(),
        "single menu-selected case",
    )
    send(b"\x1b")
    wait_for(
        lambda: " TEST CASES " not in rendered_screen() and " CONSOLE " in rendered_screen(),
        "console origin restoration",
    )
    send(b"\x1b")
    wait_for(lambda: " CONSOLE " not in rendered_screen(), "return from console")
    time.sleep(0.1)

    # A later ordinary command owns the output pane and retires comparison lines.
    send(b"\x1b[18~\r")
    check_marker = workspace / "target/check-ran"
    wait_for(
        lambda: check_marker.exists()
        and "Test case 01-pass: PASS" not in rendered_screen()
        and "Test case 02-fail:" not in rendered_screen(),
        "later command output ownership",
    )
    time.sleep(0.3)
    while read_once(0.01):
        pass

    send(b"\x11")
    while process.poll() is None and time.monotonic() < deadline:
        read_once()
    assert process.poll() is not None, "editor did not quit"
    while read_once(0.01):
        pass
    assert process.returncode == 0, repr(bytes(transcript[-8000:]))
    assert b"TERMINAL_RESTORED" in transcript, "terminal restoration marker missing"
    assert b"\xff" not in transcript, "raw program byte reached terminal"

    metadata = json.loads((workspace / ".rustrace/session.json").read_bytes())
    database = workspace / ".rustrace" / f"{metadata['session_id']}.sqlite"
    with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
        events = [
            json.loads(payload)["event"]
            for (payload,) in connection.execute("SELECT payload FROM events ORDER BY sequence")
        ]
    event_types = [event["type"] for event in events]
    assert event_types.count("controlled_command_started") == 5, event_types
    assert event_types.count("controlled_command_finished") == 5, event_types
    assert event_types.count("controlled_command_output") == 5, event_types
    comparisons = [event for event in events if event["type"] == "test_case_compared"]
    assert len(comparisons) == 4, comparisons
    assert [event["payload"]["outcome"]["kind"] for event in comparisons] == [
        "pass", "mismatch", "error", "pass"
    ], comparisons
    mismatch = comparisons[1]["payload"]["outcome"]
    assert mismatch == {
        "kind": "mismatch", "line": 1, "expected_len": 1, "actual_len": 1
    }, mismatch
    assert comparisons[2]["payload"]["outcome"] == {
        "kind": "error", "reason": "terminated"
    }, comparisons[2]
    for comparison in comparisons:
        payload = comparison["payload"]
        assert payload["case"] in ["01-pass", "02-fail"], payload
        assert set(payload) == {
            "command_id", "case", "expected_blake3", "actual_blake3", "outcome"
        }, payload
        at = events.index(comparison)
        previous = events[at - 1]
        assert previous["type"] == "controlled_command_finished", previous
        assert previous["payload"]["command_id"] == payload["command_id"], (previous, payload)
    assert comparisons[0]["payload"]["expected_blake3"] == comparisons[0]["payload"]["actual_blake3"]
    assert comparisons[1]["payload"]["expected_blake3"] != comparisons[1]["payload"]["actual_blake3"]
    assert comparisons[3]["payload"]["expected_blake3"] == comparisons[3]["payload"]["actual_blake3"]

    success = True
    print("test-case picker PTY: run-all, cancel, menu, focus, output ownership, and provenance passed")
finally:
    (root / "test-case-picker-transcript.bin").write_bytes(transcript)
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
    os.close(master)
    os.close(slave)
    if success and os.environ.get("RUSTRACE_TEST_CASE_PTY_RETAIN") != "1":
        import shutil

        shutil.rmtree(root)
