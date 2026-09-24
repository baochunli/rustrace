#!/usr/bin/env python3
"""One bounded 80x24 production fixture for the embedded Cargo console."""
import errno
import fcntl
import io
import json
import os
import pathlib
import pty
import re
import select
import shutil
import sqlite3
import struct
import subprocess
import sys
import tarfile
import tempfile
import termios
import time


def fake_tool():
    """Act as every pinned fixture tool after this file is copied into bin/."""
    program = pathlib.Path(__file__).name
    root = pathlib.Path.cwd()
    args = sys.argv[1:]
    if program == "rustup":
        assert os.environ["RUSTUP_AUTO_INSTALL"] == "0"
        if args == ["--version"]:
            assert os.environ["RUSTUP_TOOLCHAIN"] == "rustrace-discovery-bootstrap"
            print("rustup 1.28.1 (console fixture)")
        elif args == ["toolchain", "list"]:
            print("fixture (default)")
        elif args == ["show", "active-toolchain"]:
            assert os.environ["RUSTUP_TOOLCHAIN"] == "fixture"
            print("fixture (environment override)")
        elif len(args) == 4 and args[:3] == ["which", "--toolchain", "fixture"]:
            tool = pathlib.Path(__file__).resolve().parent / "v1" / args[3]
            assert tool.is_file(), tool
            print(tool)
        elif len(args) >= 3 and args[:2] == ["run", "fixture"]:
            os.execv(args[2], args[2:])
        else:
            raise AssertionError(f"unsupported rustup invocation: {args!r}")
        return

    if args in (["-vV"], ["-V"], ["--version"], ["clippy", "-V"], ["fmt", "--version"]):
        name = {"cargo-clippy": "clippy", "cargo-fmt": "rustfmt"}.get(program, program)
        phase_path = root.parent / "t10-35-command-phase"
        if (
            program == "rustdoc"
            and args == ["--version"]
            and phase_path.exists()
            and phase_path.read_text() == "launch-failure"
        ):
            (pathlib.Path(__file__).resolve().parent.parent / "rustup").unlink()
        print(f"{name} 1.98.1 (console fixture)")
        if program == "rustc":
            print("host: aarch64-apple-darwin\nrelease: 1.98.1")
        return

    # The production editor tolerates an unavailable language service. Keeping
    # this probe finite avoids adding an LSP simulator to a console fixture.
    if program == "rust-analyzer":
        raise SystemExit(1)

    if program == "cargo-fmt":
        assert args == ["fmt"], args
        fixture_root = pathlib.Path(__file__).resolve().parents[2]
        phase_path = fixture_root / "t10-35-command-phase"
        assert phase_path.read_text() == "format-cancel"
        (fixture_root / "menu-format-running").write_text("running")
        while True:
            time.sleep(60)

    assert program == "cargo", (program, args)
    if args == ["check", "--message-format=json", "--locked"]:
        if (root.parent / "lifecycle-notices-challenge").exists():
            target = root / "target"
            target.mkdir(exist_ok=True)
            phase = (root.parent / "t10-35-command-phase").read_text()
            if phase == "nonzero":
                (target / "menu-nonzero-running").write_text("running")
                deadline = time.monotonic() + 10
                while not (target / "release-menu-nonzero").exists() and time.monotonic() < deadline:
                    time.sleep(.005)
                assert (target / "release-menu-nonzero").exists()
                print('{"reason":"build-finished","success":false}')
                os.write(2, b"CHECK_NONZERO_OUTPUT\n")
                raise SystemExit(101)
            assert phase == "cancel", phase
            (target / "menu-cancel-running").write_text("running")
            while True:
                time.sleep(60)
        print('{"reason":"build-finished","success":true}')
        os.write(2, b"CHECK_OUTPUT\n")
        return
    if (root.parent / "natural-output-challenge").exists():
        assert args in [[action, "--locked"] for action in ["doc", "check", "run"]], args
        action = args[0]
        target = root / "target"
        target.mkdir(exist_ok=True)
        os.write(2, f"    Finished {action} fixture\n".encode())
        if action == "doc":
            os.write(2, b"   Generated target/doc/index.html\n")
        if action == "run":
            os.write(1, b"    indented program line\n")
        (target / f"natural-{action}").write_text("finished")
        return
    assert args == ["run", "--locked"], args
    assert os.environ["RUSTUP_AUTO_INSTALL"] == "0"
    assert os.environ["RUSTUP_TOOLCHAIN"] == "fixture"
    assert "CARGO_NET_OFFLINE" not in os.environ
    assert os.environ["RUSTC_WRAPPER"] == ""
    assert os.environ["RUSTC_WORKSPACE_WRAPPER"] == ""
    assert os.environ["CARGO_ENCODED_RUSTFLAGS"] == ""
    assert "RUSTFLAGS" not in os.environ
    assert "RUSTUP_LOG" not in os.environ

    target = root / "target"
    target.mkdir(exist_ok=True)
    identity = {
        "pid": os.getpid(),
        "pgid": os.getpgid(0),
        "sid": os.getsid(0),
        "argv": args,
        "cwd": str(root),
    }
    (target / "console-invocation.json").write_text(json.dumps(identity))

    until = time.monotonic() + 2
    while not (target / "console-descendant.json").exists() and time.monotonic() < until:
        time.sleep(.005)
    assert (target / "console-descendant.json").exists()
    if (root.parent / "rolling-output-challenge").exists():
        for index in range(5000):
            os.write(1, f"ROLLING-{index:05d}-safe-output-line\n".encode())
    os.write(1, b"console-ready\n")
    submitted = bytearray()
    while True:
        block = os.read(0, 8192)
        assert block, "console stdin closed before submitted line"
        submitted.extend(block)
        if b"\n" in submitted:
            break
    (target / "console-stdin.bin").write_bytes(submitted)
    (target / "console-line.json").write_text(json.dumps({"bytes": len(submitted)}))
    os.write(1, b"console-stdin-observed\nunsafe:\xff\0\x1b]52;c;fixture\x07\n")
    os.write(2, b"console-stderr:\xfe\n")
    while True:
        time.sleep(60)


if pathlib.Path(__file__).name != "console_pty.py":
    fake_tool()
    raise SystemExit(0)


from test_home import isolate
isolate()

binary = str(pathlib.Path(sys.argv[1]).resolve())
challenge = sys.argv[2] if len(sys.argv) > 2 else "baseline"
assert challenge in {
    "baseline",
    "natural-output",
    "rolling-output",
    "maximum-test-cases",
    "workspace-confirmations",
    "delete-confirmations",
    "lifecycle-notices",
    "hangup",
}
root = pathlib.Path(tempfile.mkdtemp(prefix="rustrace-console-pty-")).resolve()
if challenge == "lifecycle-notices":
    (root / "lifecycle-notices-challenge").write_text("enabled")
    (root / "t10-35-command-phase").write_text("nonzero")
tool_bytes = pathlib.Path(__file__).read_bytes()
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

manifest = b'''format_version = 1
course_id = "course"
assignment_id = "console-pty"
assignment_version = "v1"
title = "Console PTY"
toolchain = "fixture"
edition = "2024"
allowed_paths = ["*.rs", "src/*.rs", "src/*/*.rs", "Cargo.lock", "Cargo.toml"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
'''
initial_source = b"A"
initial_lock = b"console fixture lock"
package = root / "assignment.rta"
with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
    for name, content in [
        ("assignment.toml", manifest),
        ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
        ("starter/main.rs", initial_source),
        ("starter/Cargo.lock", initial_lock),
        ("starter/src/lib.rs", b"pub fn helper() {}\n"),
        ("starter/src/util/existing.rs", b"pub fn existing() {}\n"),
    ]:
        info = tarfile.TarInfo(name)
        info.size = len(content)
        info.mode = 0o600
        archive.addfile(info, io.BytesIO(content))
data = package.read_bytes()
while data.endswith(bytes(512)):
    data = data[:-512]
package.write_bytes(data + bytes(1024))

cases = root / "test-cases"
cases.mkdir()
case_one_input = b"case one input\n"
case_one_expected = b"case one expected\n"
if challenge == "maximum-test-cases":
    for index in range(256):
        name = f"case-{index:03d}"
        (cases / f"{name}.in").write_bytes(f"input {index}\n".encode())
        (cases / f"{name}.expected").write_bytes(f"expected {index}\n".encode())
else:
    (cases / "case-one.in").write_bytes(case_one_input)
    (cases / "case-one.expected").write_bytes(case_one_expected)
case_two_input = b"case two input\n"
case_two_expected = b"case two expected\n"
work = root / "assignment.work"
if challenge == "rolling-output":
    (root / "rolling-output-challenge").write_bytes(b"1")

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))


def own_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


wrapper = r'''
import json, os, pathlib, signal, subprocess, sys, termios, threading, time

def assert_absent(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return
    raise AssertionError(f"fixture process still exists after editor exit: {pid}")

before = termios.tcgetattr(0)
work = pathlib.Path(sys.argv[1])
challenge = sys.argv[2]
reaper_status = []
reaper_thread = None
if challenge not in {"workspace-confirmations", "delete-confirmations", "natural-output"}:
    # Keep the owned group member live through the scenario, but give it a
    # parent already blocked in waitpid. Darwin otherwise exposes the killed
    # member briefly as an EPERM-reporting zombie during the group-absence poll.
    reaper_pid = os.fork()
    if reaper_pid == 0:
        helper_pid = os.fork()
        if helper_pid == 0:
            invocation_path = work / "target/console-invocation.json"
            deadline = time.monotonic() + 20
            while not invocation_path.exists() and time.monotonic() < deadline:
                time.sleep(.005)
            if not invocation_path.exists():
                os._exit(91)
            leader = json.loads(invocation_path.read_bytes())
            os.setpgid(0, leader["pgid"])
            identity = {
                "pid": os.getpid(),
                "ppid": os.getppid(),
                "pgid": os.getpgid(0),
                "sid": os.getsid(0),
            }
            (work / "target/console-descendant.json").write_text(json.dumps(identity))
            while True:
                time.sleep(60)

        _, status = os.waitpid(helper_pid, 0)
        if os.WIFSIGNALED(status) and os.WTERMSIG(status) == signal.SIGKILL:
            os._exit(0)
        os._exit(92)

    def reap_reaper():
        _, status = os.waitpid(reaper_pid, 0)
        reaper_status.append(status)

    reaper_thread = threading.Thread(target=reap_reaper, daemon=True)
    reaper_thread.start()
if challenge == "hangup":
    # Like a shell that does not forward SIGHUP: only this session leader is
    # signalled when the terminal closes, so Rustrace must see the hangup itself.
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    result = subprocess.run(sys.argv[3:], preexec_fn=lambda: signal.signal(signal.SIGHUP, signal.SIG_DFL))
else:
    result = subprocess.run(sys.argv[3:])
if reaper_thread is not None:
    reaper_thread.join(timeout=3)
    assert not reaper_thread.is_alive(), "owned process-group helper was not reaped"
    assert len(reaper_status) == 1, reaper_status
    assert os.WIFEXITED(reaper_status[0]), reaper_status
    assert os.WEXITSTATUS(reaper_status[0]) == 0, reaper_status
assert termios.tcgetattr(0) == before, "raw terminal settings leaked"
if challenge in {"workspace-confirmations", "delete-confirmations", "natural-output"}:
    activity_path = work / ".rustrace/command-activity.json"
    if activity_path.exists():
        activity = json.loads(activity_path.read_bytes())
        assert not activity["active"], activity
    assert not (work / "target/console-invocation.json").exists()
    assert not (work / "target/console-descendant.json").exists()
else:
    activity = json.loads((work / ".rustrace/command-activity.json").read_bytes())
    assert not activity["active"], activity
    leader = json.loads((work / "target/console-invocation.json").read_bytes())
    descendant = json.loads((work / "target/console-descendant.json").read_bytes())
    assert_absent(leader["pid"])
    assert_absent(descendant["pid"])
    try:
        os.killpg(leader["pgid"], 0)
    except ProcessLookupError:
        pass
    else:
        raise AssertionError(f"fixture process group still exists: {leader['pgid']}")
print("TERMINAL_RESTORED_AFTER_COMMAND_REAP", flush=True)
sys.exit(result.returncode)
'''
proc = subprocess.Popen(
    [
        sys.executable,
        "-c",
        wrapper,
        str(work),
        challenge,
        binary,
        "work",
        str(package),
    ],
    stdin=slave,
    stdout=slave,
    stderr=slave,
    preexec_fn=own_terminal,
    cwd=root,
    env={
        **os.environ,
        "PATH": str(bin_dir) + os.pathsep + os.environ["PATH"],
        "TERM": "xterm-256color",
    },
)
transcript = bytearray()
success = False


def rendered_screen():
    """Reconstruct only the small cursor-addressing subset Ratatui emits."""
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
            elif character >= " ":
                if 0 <= row < 24 and 0 <= column < 80:
                    screen[row][column] = character
                column += 1
    return "\n".join("".join(line) for line in screen)


def read_once(timeout=.05):
    if not select.select([master], [], [], timeout)[0]:
        return False
    try:
        chunk = os.read(master, 65536)
    except OSError as error:
        if error.errno == errno.EIO:
            return False
        raise
    if chunk:
        transcript.extend(chunk)
        assert len(transcript) < 2 * 1024 * 1024, "terminal output exceeded fixture cap"
        return True
    return False


def wait_for(predicate, description, seconds=12):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        read_once()
        if predicate():
            return
        if proc.poll() is not None:
            break
    raise AssertionError(f"timed out waiting for {description}\n{rendered_screen()}")


def wait_screen(value, seconds=12):
    wait_for(lambda: value in rendered_screen(), repr(value), seconds)


def pane_header_row_column():
    line = next(line for line in rendered_screen().splitlines() if "│console" in line)
    return line.index("console")


def pane_header_row(label):
    marker = f"│{label}"
    return next(
        (
            index
            for index, line in enumerate(rendered_screen().splitlines())
            if marker in line
        ),
        None,
    )


def normalized_transcript():
    """Visible text emitted through cursor-addressed terminal updates."""
    return b" ".join(
        re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b" ", bytes(transcript)).split()
    ).decode("utf-8", "replace")


def assert_no_toast():
    visible = normalized_transcript()
    for title in [
        "● notice",
        "● action failed",
        "● complete",
        "● warning",
        "● paste blocked",
        "● recovery required",
    ]:
        assert title not in visible, visible


def send(keys):
    assert proc.poll() is None, "editor exited before fixture input"
    os.write(master, keys)


def process_exists(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


try:
    wait_screen(" files", 25)
    os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.

    if challenge == "natural-output":
        (root / "natural-output-challenge").write_text("enabled")
        send(b"\x1b[20~")
        wait_screen("│console")
        for action in ["doc", "check", "run"]:
            send(f"cargo {action}\r".encode())
            wait_for(lambda: (work / f"target/natural-{action}").exists()
                     and not json.loads((work / ".rustrace/command-activity.json").read_bytes())["active"],
                     f"natural {action} completion")
            wait_screen(f"Finished {action} fixture")
            screen = rendered_screen()
            assert '{"reason":' not in screen, screen
            assert b'{"reason":' not in transcript, bytes(transcript)
            lines = screen.splitlines()
            finished = next(line for line in lines if f"Finished {action} fixture" in line)
            assert finished.index("Finished") == pane_header_row_column(), screen
            if action == "run":
                program = next(line for line in lines if "indented program line" in line)
                assert program.index("indented program line") == pane_header_row_column() + 4, screen
        send(b"\x1b")
        wait_screen("│output")
        send(b"\x11")
        deadline = time.monotonic() + 15
        while proc.poll() is None and time.monotonic() < deadline:
            read_once(.01)
        assert proc.poll() == 0, rendered_screen()
        success = True
        print("80x24 natural Doc/Check output flush-left; indented Run output preserved")
        raise SystemExit(0)

    if challenge == "lifecycle-notices":
        send(b"\x1b[18~\r")  # F7, default Check, Enter.
        wait_for(
            lambda: (work / "target/menu-nonzero-running").exists()
            and "Command: running (Esc cancel)" in rendered_screen(),
            "menu Check running state",
        )
        assert_no_toast()
        (work / "target/release-menu-nonzero").write_text("release")
        wait_for(
            lambda: (work / ".rustrace/command-activity.json").exists()
            and not json.loads(
                (work / ".rustrace/command-activity.json").read_bytes()
            )["active"]
            and "CHECK_NONZERO_OUTPUT" in rendered_screen()
            and " ERROR " in rendered_screen()
            and "Command: nonzero exit" in rendered_screen(),
            "menu Check nonzero result",
        )
        assert_no_toast()

        send(b"\x1b[20~cargo run\r")
        invocation_path = work / "target/console-invocation.json"
        descendant_path = work / "target/console-descendant.json"
        wait_for(
            lambda: invocation_path.exists()
            and descendant_path.exists()
            and " CONSOLE " in rendered_screen()
            and "ctrl-c stop  esc stop+close  ↵ send" in rendered_screen()
            and "stdin>" in rendered_screen(),
            "console running state and mode bar",
        )
        assert_no_toast()

        send(b"\x04\x1b[99;9u\x1b[100;9u")  # Ctrl-D, Cmd-C and Cmd-D are inert.
        time.sleep(.15)
        while read_once(.01):
            pass
        assert json.loads((work / ".rustrace/command-activity.json").read_bytes())["active"]
        assert "│console" in rendered_screen()
        assert "stdin> ▏" in rendered_screen(), rendered_screen()
        send(b"\x1b")
        wait_for(
            lambda: not json.loads((work / ".rustrace/command-activity.json").read_bytes())["active"]
            and "│output" in rendered_screen()
            and " CONSOLE " not in rendered_screen()
            and " ERROR " in rendered_screen()
            and "Console: stopped" in rendered_screen(),
            "Esc cancelled command and closed console",
        )
        captures = [json.loads(path.read_bytes()) for path in (work / ".rustrace").glob("command-*-capture.json")]
        assert any(capture["execution"]["outcome"].get("reason") == "cancelled" for capture in captures), captures
        assert_no_toast()

        (root / "t10-35-command-phase").write_text("cancel")
        send(b"\x1b[18~\r")
        wait_for(
            lambda: (work / "target/menu-cancel-running").exists()
            and "Command: running (Esc cancel)" in rendered_screen(),
            "cancellable menu Check",
        )
        assert_no_toast()
        send(b"\x1b")
        wait_for(
            lambda: not json.loads(
                (work / ".rustrace/command-activity.json").read_bytes()
            )["active"]
            and " ERROR " in rendered_screen()
            and "Command: cancelled; capture recorded" in rendered_screen(),
            "cancelled menu Check result",
        )
        assert_no_toast()

        (root / "t10-35-command-phase").write_text("format-cancel")
        send(b"\x1b[18~\x1b[B\x1b[B\x1b[B\r")
        wait_for(
            lambda: (root / "menu-format-running").exists()
            and "Command: running (Esc cancel)" in rendered_screen(),
            "cancellable menu Format",
        )
        assert_no_toast()
        send(b"\x1b")
        wait_for(
            lambda: not json.loads(
                (work / ".rustrace/command-activity.json").read_bytes()
            )["active"]
            and " ERROR " in rendered_screen()
            and "Format: failed/cancelled; returned changes" in rendered_screen(),
            "cancelled menu Format result",
        )
        assert_no_toast()

        (root / "t10-35-command-phase").write_text("launch-failure")
        send(b"\x1b[18~\r")
        wait_for(
            lambda: not json.loads(
                (work / ".rustrace/command-activity.json").read_bytes()
            )["active"]
            and " ERROR " in rendered_screen()
            and "Command: launch failed; evidence recorded" in rendered_screen(),
            "menu Check launch failure result",
        )
        assert_no_toast()

        send(b"\x11")
        deadline = time.monotonic() + 15
        while proc.poll() is None and time.monotonic() < deadline:
            read_once(.01)
        assert proc.poll() is not None, "lifecycle notice fixture did not quit"
        while read_once(.01):
            pass
        assert proc.returncode == 0, repr(bytes(transcript[-8000:]))
        assert b"TERMINAL_RESTORED_AFTER_COMMAND_REAP" in transcript
        assert_no_toast()
        captures = list((work / ".rustrace").glob("command-*-capture.json"))
        assert len(captures) == 5, captures
        success = True
        print("80x24 command lifecycle and failure outcomes rendered without notices")
        raise SystemExit(0)

    if challenge == "workspace-confirmations":
        # Reset the autosave clock, then queue the edit, console focus, console
        # line, menu navigation, and Quit activation inside that two-second
        # window so the confirmation is deterministic.
        send(b"\x13")
        wait_for(
            lambda: len(list((work / ".rustrace").glob("command-*-capture.json"))) == 1
            and not json.loads(
                (work / ".rustrace/command-activity.json").read_bytes()
            )["active"],
            "initial save-triggered Check completion",
        )
        send(b"Z\x1b[20~Q\x1b[18~" + b"\x1b[B" * 11 + b"\r")
        wait_screen("Discard unsaved buffer changes?")
        assert (work / "main.rs").read_bytes() == initial_source

        send(b"\x1b")
        wait_for(
            lambda: "Discard unsaved buffer changes?" not in rendered_screen()
            and " CONSOLE " in rendered_screen()
            and "> Q" in rendered_screen(),
            "cancelled dirty Quit restoring console focus and line",
        )
        assert proc.poll() is None
        assert not (work / "target/console-invocation.json").exists()

        # Save once to reset the clock, dirty the file again, then confirm the
        # second request. Enter must confirm Quit rather than execute Q.
        send(b"\x1b[<0;31;3M\x1b[<0;31;3m\x13")
        wait_for(
            lambda: len(list((work / ".rustrace").glob("command-*-capture.json"))) == 2
            and not json.loads(
                (work / ".rustrace/command-activity.json").read_bytes()
            )["active"],
            "second save-triggered Check completion",
        )
        send(b"Y\x1b[20~\x1b[18~" + b"\x1b[B" * 11 + b"\r")
        wait_screen("Discard unsaved buffer changes?")
        send(b"\r")

        deadline = time.monotonic() + 12
        while proc.poll() is None and time.monotonic() < deadline:
            read_once()
        assert proc.poll() is not None, "confirmation fixture did not quit"
        while read_once(.01):
            pass
        assert proc.returncode == 0, repr(bytes(transcript[-8000:]))
        assert not (work / "target/console-invocation.json").exists()
        assert b"TERMINAL_RESTORED_AFTER_COMMAND_REAP" in transcript
        success = True
        print("80x24 console workspace confirmation precedence passed")
        raise SystemExit(0)

    if challenge == "delete-confirmations":
        # Reset the autosave clock, then dirty the selected file and enter the
        # console with an input line that Ctrl-W must not consume or execute.
        send(b"\x13")
        wait_for(
            lambda: len(list((work / ".rustrace").glob("command-*-capture.json"))) == 1
            and not json.loads(
                (work / ".rustrace/command-activity.json").read_bytes()
            )["active"],
            "initial save-triggered Check completion",
        )
        send(b"Z\x1b[20~Q\x17")
        wait_screen("Delete main.rs?")
        assert "> Q" in rendered_screen()
        assert (work / "main.rs").exists()

        send(b"n")
        wait_for(
            lambda: "Delete main.rs?" not in rendered_screen()
            and " CONSOLE " in rendered_screen()
            and "> Q" in rendered_screen(),
            "cancelled delete restoring console focus and line",
        )
        assert (work / "main.rs").exists()

        send(b"\x17")
        wait_screen("Delete main.rs?")
        assert "> Q" in rendered_screen()
        send(b"y")
        wait_for(
            lambda: "Delete main.rs?" not in rendered_screen()
            and " CONSOLE " in rendered_screen()
            and "> Q" in rendered_screen()
            and not (work / "main.rs").exists(),
            "confirmed delete restoring console focus and line",
        )
        assert not (work / "target/console-invocation.json").exists()

        metadata = json.loads((work / ".rustrace/session.json").read_bytes())
        database = work / ".rustrace" / f"{metadata['session_id']}.sqlite"
        with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
            events = [json.loads(row[0]) for row in connection.execute(
                "SELECT payload FROM events ORDER BY sequence"
            )]
        deleted = [event for event in events if event["event"]["type"] == "file_deleted"]
        assert len(deleted) == 1, deleted
        assert deleted[0]["event"]["payload"]["path"] == "main.rs", deleted[0]
        assert "mouse" not in json.dumps(events).lower(), events

        send(b"\x11")
        deadline = time.monotonic() + 12
        while proc.poll() is None and time.monotonic() < deadline:
            read_once()
        assert proc.poll() is not None, "delete confirmation fixture did not quit"
        while read_once(.01):
            pass
        assert proc.returncode == 0, repr(bytes(transcript[-8000:]))
        assert b"TERMINAL_RESTORED_AFTER_COMMAND_REAP" in transcript
        success = True
        print("80x24 console Ctrl-W delete confirmation precedence passed")
        raise SystemExit(0)

    if challenge == "baseline":
        # Keep the console pane open while focus moves through editor, console,
        # file tree activation, and back to the console. Keyboard input and
        # cursor/mode rendering must follow focus, not pane visibility.
        send(b"\x1b[20~")
        wait_screen(" CONSOLE ")
        send(
            b"\x1b[<0;30;14M"
            b"\x1b[<32;30;18M"
            b"\x1b[<0;30;18m"
        )
        wait_for(
            lambda: pane_header_row("console") == 18,
            "five-row console resize",
        )
        console_header_row = 18
        send(b"Q")
        wait_screen("> Q")

        # Cargo menu actions return to output/editor focus and preserve the
        # hidden console state and shared pane height for the next F9.
        send(b"\x1b[<0;22;24M\x1b[<0;22;24m")
        wait_screen(" MENU ")
        send(b"\r")
        wait_for(
            lambda: "│output" in rendered_screen()
            and "CHECK_OUTPUT" in rendered_screen()
            and " CONSOLE " not in rendered_screen(),
            "menu Check output with editor focus",
        )
        output_header_row = pane_header_row("output")
        assert output_header_row == console_header_row
        send(b"Z")
        wait_for(
            lambda: (work / "main.rs").read_bytes() == b"ZA",
            "editor focus after menu Check",
        )
        send(b"\x1b[20~")
        wait_for(
            lambda: " CONSOLE " in rendered_screen()
            and "> Q" in rendered_screen()
            and pane_header_row("console") == console_header_row,
            "console state and height restored after menu Check",
        )

        send(b"\x1b[<0;31;3M\x1b[<0;31;3m")
        wait_for(
            lambda: "│console" in rendered_screen()
            and " CONSOLE " not in rendered_screen()
            and "> ▏" not in rendered_screen(),
            "editor focus with the console pane still open",
        )
        assert (work / "main.rs").read_bytes() == b"ZA"

        send(b"\x1b[<0;31;19M\x1b[<0;31;19m")
        wait_for(
            lambda: " CONSOLE " in rendered_screen()
            and "│console" in rendered_screen(),
            "console focus without closing the pane",
        )
        wait_screen("> Q")
        assert (work / "main.rs").read_bytes() == b"ZA"

        # A file modal opened over console focus owns the keyboard and returns
        # to the byte-identical console line when submitted or cancelled.
        send(b"\x1b[<0;2;24M\x1b[<0;2;24m")
        wait_screen("new file")
        wait_screen("src/▏")
        send(b"\x7fcreated.rs")
        wait_screen("src/created.rs")
        assert not (work / "src/created.rs").exists()
        send(b"\r")
        wait_for(
            lambda: (work / "src/created.rs").exists(),
            "new file from console-focused modal",
        )
        metadata = json.loads((work / ".rustrace/session.json").read_bytes())
        database = work / ".rustrace" / f"{metadata['session_id']}.sqlite"
        with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
            events = [json.loads(row[0]) for row in connection.execute(
                "SELECT payload FROM events ORDER BY sequence"
            )]
        created = [event for event in events if event["event"]["type"] == "file_created"]
        assert created[-1]["event"]["payload"]["path"] == "src/created.rs", created[-1]
        wait_for(
            lambda: " CONSOLE " in rendered_screen() and "> Q" in rendered_screen(),
            "console focus and line restored after create",
        )

        send(b"\x1b[<0;2;24M\x1b[<0;2;24m")
        wait_screen("src/▏")
        send(b"util/mod.rs\r")
        wait_for(
            lambda: (work / "src/util/mod.rs").exists(),
            "nested new-file remainder",
        )
        wait_for(
            lambda: " CONSOLE " in rendered_screen() and "> Q" in rendered_screen(),
            "console focus and line restored after nested create",
        )

        # Rename through the files menu while the console remains open; its
        # prefilled field, not the console line, receives the keys.
        send(b"\x1b[<2;4;7M")
        wait_screen("rename…")
        menu_lines = rendered_screen().splitlines()
        rename_row = next(
            index for index, line in enumerate(menu_lines) if "rename…" in line
        )
        rename_column = menu_lines[rename_row].index("rename…")
        send(f"\x1b[<0;{rename_column + 1};{rename_row + 1}M".encode())
        wait_screen("rename file")
        send(b"\x7f" * len("src/util/mod.rs") + b"src/renamed.rs")
        wait_screen("src/renamed.rs")
        send(b"\r")
        wait_for(
            lambda: (work / "src/renamed.rs").exists()
            and not (work / "src/util/mod.rs").exists(),
            "rename modal submission",
        )

        send(b"\x1b[<0;31;19M\x1b[<0;31;19m")
        wait_for(
            lambda: " CONSOLE " in rendered_screen() and "> Q" in rendered_screen(),
            "console line remained byte-identical after rename",
        )
        send(b"\x1b[<0;2;24M\x1b[<0;2;24m")
        wait_screen("new file")
        send(b"cancelled.rs\x1b")
        wait_for(
            lambda: " CONSOLE " in rendered_screen()
            and "> Q" in rendered_screen()
            and "new file" not in rendered_screen(),
            "Esc closed the file modal and restored console focus",
        )
        assert not (work / "src/cancelled.rs").exists()

        main_row = next(
            index + 1
            for index, line in enumerate(rendered_screen().splitlines())
            if "main.rs" in line[:26]
        )
        send(f"\x1b[<0;4;{main_row}M\x1b[<0;4;{main_row}m".encode())
        wait_for(
            lambda: "│console" in rendered_screen()
            and " CONSOLE " not in rendered_screen(),
            "file activation returning focus to the editor",
        )
        send(b"\x1b[<0;31;19M\x1b[<0;31;19m")
        wait_screen(" CONSOLE ")
        send(b"\x7f\x1b")
        wait_screen("│output")
        send(b"\x1a")
        wait_for(
            lambda: (work / "main.rs").read_bytes() == initial_source,
            "focus regression cleanup save",
        )

    send(b"\x1b[14~")  # F4: fixed sibling Test cases.
    wait_screen(" TEST CASES ")
    if challenge == "maximum-test-cases":
        for _ in range(255):
            send(b"\x1b[B")
            time.sleep(.002)
        wait_for(
            lambda: "case-255" in rendered_screen() and "Run all" in rendered_screen(),
            "late selected test case and Run all row",
            15,
        )
        for entry in cases.iterdir():
            entry.unlink()
        (cases / "case-one.in").write_bytes(case_one_input)
        (cases / "case-one.expected").write_bytes(case_one_expected)
        send(b"r")
        wait_for(
            lambda: "case-one" in rendered_screen() and "Run all" in rendered_screen(),
            "refreshed paired case and Run all row",
        )
    else:
        wait_for(
            lambda: "case-one" in rendered_screen() and "Run all" in rendered_screen(),
            "paired case and Run all row",
        )

    (cases / "case-one.in").unlink()
    (cases / "case-one.expected").unlink()
    (cases / "case-two.in").write_bytes(case_two_input)
    (cases / "case-two.expected").write_bytes(case_two_expected)
    send(b"r")
    wait_for(
        lambda: "case-two" in rendered_screen() and "case-one" not in rendered_screen(),
        "refreshed replacement case pair",
    )
    send(b"\x1b")
    wait_for(lambda: " TEST CASES " not in rendered_screen(), "closed test-case modal")

    paste = b"PASTE_MUST_NOT_PERSIST"
    send(b"\x1b[200~" + paste + b"\x1b[201~")
    wait_screen("Paste blocked:")

    send(b"\x1b[20~")  # F9: embedded console.
    wait_screen("│console")
    wait_screen("> ▏")
    command_console_header_row = pane_header_row("console")
    send(b"cargo run\r")
    invocation_path = work / "target/console-invocation.json"
    descendant_path = work / "target/console-descendant.json"
    wait_for(
        lambda: invocation_path.exists() and descendant_path.exists(),
        "console child and descendant",
    )
    wait_screen("stdin>")

    submitted = b"SUBMITTED_STDIN_MUST_STAY_PRIVATE"
    for character in submitted:
        send(bytes([character]))
        time.sleep(.01)
    wait_screen(submitted.decode())
    send(b"\r")
    time.sleep(.1)
    while read_once(.01):
        pass
    send(b"\x04\x1b[99;9u\x1b[100;9u")  # Ctrl-D, Cmd-C and Cmd-D are inert.
    wait_for(lambda: (work / "target/console-line.json").exists(), "submitted console line")
    assert json.loads((work / ".rustrace/command-activity.json").read_bytes())["active"]
    wait_screen("console-stderr:\\xfe")
    if challenge == "baseline":
        # The console follows the newest output; PgUp reads older rows while
        # the command runs, and PgDn resumes following.
        send(b"\x1b[5~")
        wait_screen("console-ready")
        wait_screen("↓ ")
        wait_screen(" more line")
        send(b"\x1b[6~")
        wait_for(
            lambda: "↓" not in rendered_screen()
            and "console-stderr:\\xfe" in rendered_screen(),
            "console follows newest output again",
        )
    else:
        wait_screen("unsafe:\\xff\\u{0}\\u{1b}]52;c;fixture\\u{7}")
    assert (work / "target/console-stdin.bin").read_bytes() == submitted + b"\n"

    leader = json.loads(invocation_path.read_bytes())
    descendant = json.loads(descendant_path.read_bytes())
    assert leader["pid"] == leader["pgid"]
    assert descendant["pgid"] == leader["pgid"]
    assert process_exists(leader["pid"]) and process_exists(descendant["pid"])

    send(b"\x1b[18~")  # F7: menu while the console command owns execution.
    wait_screen(" MENU ")
    send(b"\r")  # Check is skipped by the existing owner-busy policy.
    wait_for(
        lambda: "│output" in rendered_screen()
        and "Command unavailable" in rendered_screen()
        and " CONSOLE " not in rendered_screen(),
        "busy menu Check switched to output with owner feedback",
    )
    assert process_exists(leader["pid"]) and process_exists(descendant["pid"])
    send(b"\x1b[20~")
    wait_for(
        lambda: "│console" in rendered_screen()
        and "console-stderr:\\xfe" in rendered_screen()
        and pane_header_row("console") == command_console_header_row,
        "console state and height restored after busy menu Check",
    )
    send(b"\x1b[18~\r")  # Busy menu Check switches to output while Run remains active.
    wait_screen("│output")
    send(b"Z")  # Must be ignored by the active-command source barrier.
    time.sleep(.15)
    while read_once(.01):
        pass
    assert (work / "main.rs").read_bytes() == initial_source
    send(b"\x1b[20~")
    wait_screen("│console")
    assert b"\x1b[?1049l" not in transcript

    if challenge == "hangup":
        # The terminal emulator dies mid-command: Rustrace gets SIGHUP and must
        # reap the program and clear its activity marker before exiting.
        rustrace_pids = [
            int(pid)
            for pid in subprocess.run(
                ["pgrep", "-f", f"^{binary} work {package}"],
                capture_output=True,
                text=True,
            ).stdout.split()
        ]
        assert rustrace_pids, "running Rustrace process"
        # A terminal emulator holds only the master side.
        os.close(slave)
        os.close(master)
        deadline = time.monotonic() + 15
        while any(map(process_exists, rustrace_pids)) and time.monotonic() < deadline:
            time.sleep(.05)
        survivors = [pid for pid in rustrace_pids if process_exists(pid)]
        if survivors:
            state = subprocess.run(["ps", "-o", "pid,ppid,stat,etime,command", "-p",
                                    ",".join(map(str, survivors))],
                                   capture_output=True, text=True).stdout
            if shutil.which("sample"):
                subprocess.run(["sample", str(survivors[0]), "1", "-file",
                                str(root / "survivor-sample.txt")], capture_output=True)
            raise AssertionError(f"Rustrace survived SIGHUP:\n{state}\nsample: {root}/survivor-sample.txt")
        assert not process_exists(leader["pid"]), "program outlived the hangup"
        assert not process_exists(descendant["pid"]), "descendant outlived the hangup"
        assert not json.loads((work / ".rustrace/command-activity.json").read_bytes())["active"]

        # A new terminal resumes the session and quits normally.
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        transcript.clear()
        proc = subprocess.Popen(
            [binary, "work", str(package), "--resume"],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            preexec_fn=own_terminal,
            cwd=root,
            env={
                **os.environ,
                "PATH": str(bin_dir) + os.pathsep + os.environ["PATH"],
                "TERM": "xterm-256color",
            },
        )
        wait_screen("F1 keybinds")
        send(b"\x11")
        deadline = time.monotonic() + 15
        while proc.poll() is None and time.monotonic() < deadline:
            read_once(.01)
        assert proc.returncode == 0, repr(bytes(transcript[-4000:]))
        success = True
        print("80x24 console hangup reaped the command and resumed")
        raise SystemExit(0)

    captures_before_quit = set(
        (work / ".rustrace").glob("command-*-capture.json")
    )
    assert len(captures_before_quit) == (1 if challenge == "baseline" else 0), (
        captures_before_quit
    )
    activity_before_quit = json.loads(
        (work / ".rustrace/command-activity.json").read_bytes()
    )
    assert activity_before_quit["active"] is True, activity_before_quit

    quit_started = time.monotonic()
    send(b"\x11")  # Ctrl-Q: cancel, reap, then restore the terminal.
    restoration_observation = None
    deadline = time.monotonic() + 15
    while proc.poll() is None and time.monotonic() < deadline:
        read_once(.01)
        if restoration_observation is None and b"\x1b[?1049l" in transcript:
            activity = json.loads((work / ".rustrace/command-activity.json").read_bytes())
            restoration_observation = {
                "active": activity["active"],
                "leader": process_exists(leader["pid"]),
                "descendant": process_exists(descendant["pid"]),
                "elapsed": time.monotonic() - quit_started,
            }
    assert proc.poll() is not None, "console PTY fixture hung during Ctrl-Q"
    while read_once(.01):
        pass

    assert proc.returncode == 0, repr(bytes(transcript[-8000:]))
    assert restoration_observation is not None
    assert restoration_observation["active"] is False, restoration_observation
    assert restoration_observation["leader"] is False, restoration_observation
    assert restoration_observation["descendant"] is False, restoration_observation
    assert restoration_observation["elapsed"] < 3.5, restoration_observation
    assert b"\x1b[?2004l" in transcript and b"\x1b[?1049l" in transcript
    restored = b"TERMINAL_RESTORED_AFTER_COMMAND_REAP"
    assert restored in transcript
    assert transcript.index(b"\x1b[?1049l") < transcript.index(restored)
    assert b"\x1b]52;c;fixture" not in transcript, "raw console OSC reached the terminal"
    assert b"unsafe:\xff" not in transcript, "raw invalid UTF-8 reached the terminal"
    assert paste not in transcript
    assert (work / "main.rs").read_bytes() == initial_source
    assert (work / "Cargo.lock").read_bytes() == initial_lock
    assert (cases / "case-two.in").read_bytes() == case_two_input
    assert (cases / "case-two.expected").read_bytes() == case_two_expected
    assert not json.loads((work / ".rustrace/command-activity.json").read_bytes())["active"]

    captures = set((work / ".rustrace").glob("command-*-capture.json"))
    assert len(captures) == (2 if challenge == "baseline" else 1), captures
    quit_captures = captures - captures_before_quit
    assert len(quit_captures) == 1, (captures_before_quit, captures)
    capture = json.loads(quit_captures.pop().read_bytes())
    assert capture["execution"]["outcome"]["reason"] == "quit", capture
    assert capture["execution"]["cleanup_confirmed"] is True, capture
    for artifact in (work / ".rustrace").iterdir():
        if artifact.is_file() and artifact.stat().st_size <= 34 * 1024 * 1024:
            contents = artifact.read_bytes()
            assert paste not in contents, artifact
            assert submitted not in contents, artifact

    success = True
    print(
        "80x24 console/test-cases/input/paste/source-lockout/quit cleanup passed; "
        f"restored after {restoration_observation['elapsed']:.3f}s"
    )
finally:
    (root / "console-transcript.bin").write_bytes(transcript)
    try:
        if proc.poll() is None:
            try:
                os.write(master, b"\x11")
            except OSError:
                pass
            cleanup_until = time.monotonic() + 4
            while proc.poll() is None and time.monotonic() < cleanup_until:
                read_once()
        if proc.poll() is None:
            os.killpg(proc.pid, 9)
        if (work / "target/console-invocation.json").exists():
            owned_group = json.loads(
                (work / "target/console-invocation.json").read_bytes()
            )["pgid"]
            try:
                os.killpg(owned_group, 9)
            except ProcessLookupError:
                pass
    finally:
        (root / "console-transcript.bin").write_bytes(transcript)
        os.close(master)
        os.close(slave)
    if success and not os.environ.get("RUSTRACE_CONSOLE_PTY_RETAIN"):
        shutil.rmtree(root)
    elif success:
        print("RETAINED_FIXTURE:", root)
    else:
        print("FAILED fixture preserved:", root, file=sys.stderr)
