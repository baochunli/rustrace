#!/usr/bin/env python3
"""Run format 3 packaged cases and console Runs end to end in a real workspace.

The F4 picker lists every case with a `.expected`, including `quiet` and
`touch`, which have no `.in`, and shows the selected case's arguments, input,
fixture files and result. It runs the `echo` case (its arguments, hashed
input, and the fixture working directory), then Run all across the three
cases; `touch` adds a file to the fixture folder, which the reopened picker
reports as changed. The console runs `cargo run -- ARG...` from the fixture
folder, a changed fixture tree shows in the picker before a run and as a
warning toast, and a workspace `.cargo` is refused.
"""
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
import stat
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
            print("rustup 1.28.1 (format 3 fixture)")
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
        print(f"{program} 1.98.1 (format 3 fixture)")
        if program == "rustc":
            print("host: aarch64-apple-darwin\nrelease: 1.98.1")
        return
    if program == "rust-analyzer":
        raise SystemExit(1)

    assert program == "cargo", (program, args)
    cwd = pathlib.Path.cwd()
    if args == ["check", "--message-format=json", "--locked"]:
        os.write(1, b'{"reason":"build-finished","success":true}\n')
        return
    cargo_args, program_args = args, []
    if "--" in args:
        split = args.index("--")
        cargo_args, program_args = args[:split], args[split + 1:]
    assert cargo_args[:1] == ["run"], args
    at = cargo_args.index("--manifest-path")
    manifest = cargo_args[at + 1]
    assert cargo_args[:at] + cargo_args[at + 2:] == ["run", "--locked"], args
    workspace = (cwd / manifest).parent.resolve()
    assert manifest == f"../../{workspace.name}/Cargo.toml", manifest
    assert os.environ["CARGO_BUILD_BUILD_DIR"] == f"../../{workspace.name}/target"
    target = workspace / "target"
    target.mkdir(exist_ok=True)
    mode = os.fstat(0).st_mode
    kind = "pipe" if stat.S_ISFIFO(mode) else "file" if stat.S_ISREG(mode) else "closed"
    if program_args[:1] == ["--console"]:
        # Typed console Runs read the prompt, which never closes by itself.
        kind = "console"
    with (target / "runs.jsonl").open("a") as log:
        log.write(json.dumps({"cwd": str(cwd), "args": program_args, "stdin": kind}) + "\n")
    value = b"" if kind == "console" else sys.stdin.buffer.read()
    data = (cwd / "data.txt").read_bytes()
    if "--touch" in program_args:
        # A program that writes into the folder it runs in changes the
        # fixtures for every later run.
        (cwd / "touched.txt").write_bytes(b"touched\n")
    os.write(1, (
        f"cwd={cwd.parent.name}/{cwd.name}\nargs={'|'.join(program_args)}\nstdin={kind}:"
    ).encode() + value + b"\ndata=" + data)
    time.sleep(0.2)


if pathlib.Path(__file__).name != "format3_run_pty.py":
    fake_tool()
    raise SystemExit(0)


from test_home import isolate
isolate()

binary = str(pathlib.Path(sys.argv[1]).resolve())
root = pathlib.Path(tempfile.mkdtemp(prefix="rustrace-format3-run-")).resolve()
bin_dir = root / "bin"
bin_dir.mkdir()
tool_bytes = pathlib.Path(__file__).read_bytes()
for name in ["rustup", "rustc", "cargo", "rustdoc", "rust-analyzer"]:
    tool = bin_dir / name
    tool.write_bytes(tool_bytes)
    tool.chmod(0o755)

manifest = b'''format_version = 3
course_id = "course"
assignment_id = "format3-run"
assignment_version = "v1"
title = "Format 3 run PTY"
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
data = b"fixture data\n"
expected = b"cwd=lab.test-cases/files\nargs=-i|two words\nstdin=pipe:alpha\n\ndata=" + data
entries = [
    ("assignment.toml", manifest),
    ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
    ("starter/main.rs", b"fn main() {}\n"),
    ("test-cases/echo.args", b"-i\ntwo words\n"),
    ("test-cases/echo.in", b"alpha\n"),
    ("test-cases/echo.expected", expected),
    ("test-cases/quiet.args", b"--quiet\n"),
    ("test-cases/quiet.expected", b"cwd=lab.test-cases/files\nargs=--quiet\nstdin=closed:\ndata=" + data),
    ("test-cases/touch.args", b"--touch\n"),
    ("test-cases/touch.expected", b"cwd=lab.test-cases/files\nargs=--touch\nstdin=closed:\ndata=" + data),
    ("test-cases/files/data.txt", data),
]
package = root / "lab.rta"
with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
    for name, contents in entries:
        info = tarfile.TarInfo(name)
        info.size = len(contents)
        info.mode = 0o600
        info.uid = info.gid = info.mtime = 0
        archive.addfile(info, io.BytesIO(contents))
package_bytes = package.read_bytes()
while package_bytes.endswith(bytes(512)):
    package_bytes = package_bytes[:-512]
package.write_bytes(package_bytes + bytes(1024))

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
        "RUSTUP_AUTO_INSTALL": "0",
    },
)
transcript = bytearray()
deadline = time.monotonic() + 60


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
    assert len(transcript) < 4 * 1024 * 1024, "PTY transcript exceeded fixture cap"
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


def normalized_since(offset):
    """Visible text emitted after `offset`, with cursor movement as spaces."""
    return b" ".join(
        re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b" ", bytes(transcript[offset:])).split()
    ).decode("utf-8", "replace")


def toast_text_since(offset):
    """Like normalized_since, with box borders removed so wrapped toast
    lines read as one sentence."""
    return " ".join(normalized_since(offset).replace("│", " ").split())


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


def runs():
    path = workspace / "target/runs.jsonl"
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines()]


def type_console(line):
    send(line.encode() + b"\r")


def wait_console_idle(label):
    """A running console Run would read the next typed line as its stdin."""
    wait_for(
        lambda: "esc close  ↵ run/send" in rendered_screen()
        and "ctrl-c stop" not in rendered_screen(),
        label,
    )


success = False
workspace = root / "lab.work"
folder = root / "lab.test-cases"
files = folder / "files"
try:
    wait_for(lambda: " files" in rendered_screen(), "workspace")
    assert (folder / ".rustrace-cases.json").is_file()
    assert (files / "data.txt").read_bytes() == data

    # F4 lists every case, shows what the selected `echo` runs with, and
    # runs it: its arguments, its hashed input through a pipe, and the
    # fixture folder as the working directory.
    send(b"\x1b[14~")
    wait_for(
        lambda: all(
            value in rendered_screen()
            for value in [
                "Test cases", "echo", "quiet", "touch", "Run all",
                "Arguments  «-i» «two words»",
                "Input      echo.in (6 bytes)",
                "Runs in    lab.test-cases/files (1 file)",
                "Files      data.txt",
                "Result     not run yet",
            ]
        ),
        "picker with the echo detail",
    )
    assert "changed from the package" not in rendered_screen()
    send(b"\r")
    wait_for(
        lambda: len(runs()) == 1
        and " TEST CASES " in rendered_screen()
        and "Result     PASS" in rendered_screen(),
        "echo PASS in the reopened picker",
    )
    assert runs()[0] == {"cwd": str(files), "args": ["-i", "two words"], "stdin": "pipe"}, runs()

    # A case without `.in` runs with standard input closed.
    send(b"\x1b[B")
    wait_for(
        lambda: "Arguments  «--quiet»" in rendered_screen()
        and "Input      no input (stdin closed)" in rendered_screen(),
        "quiet detail",
    )

    # Run all runs the three cases in order with the same arguments, closed
    # stdin and fixture folder. `touch` adds a file there, so the reopened
    # picker says the fixtures changed; removing it and refreshing clears that.
    send(b"\x1b[B\x1b[B")
    wait_for(
        lambda: "Results    3 cases: 1 PASS, 0 FAIL, 0 ERROR, 2 not run yet" in rendered_screen(),
        "Run all tally",
    )
    send(b"\r")
    wait_for(
        lambda: len(runs()) == 4
        and " TEST CASES " in rendered_screen()
        and "Results    3 cases: 3 PASS, 0 FAIL, 0 ERROR, 0 not run yet" in rendered_screen()
        and "changed from the package; runs with them will not verify" in rendered_screen()
        and "Runs in    lab.test-cases/files (2 files)" in rendered_screen(),
        "Run all PASS with the touched fixtures",
    )
    assert runs()[1:] == [
        {"cwd": str(files), "args": ["-i", "two words"], "stdin": "pipe"},
        {"cwd": str(files), "args": ["--quiet"], "stdin": "closed"},
        {"cwd": str(files), "args": ["--touch"], "stdin": "closed"},
    ], runs()
    (files / "touched.txt").unlink()
    send(b"r")
    wait_for(
        lambda: "Runs in    lab.test-cases/files (1 file)" in rendered_screen()
        and "changed from the package" not in rendered_screen(),
        "refreshed fixtures match the package again",
    )
    send(b"\x1b")
    wait_for(lambda: " TEST CASES " not in rendered_screen(), "picker closed")
    time.sleep(0.3)

    # Console Runs start in the fixture folder with literal arguments.
    send(b"\x1b[20~")
    wait_for(lambda: " CONSOLE " in rendered_screen(), "console")
    # Nothing expands arguments: `*` and `$` reach the program as typed.
    type_console("cargo run -- --console a*b $HOME")
    wait_for(
        lambda: len(runs()) == 5
        and "cwd=lab.test-cases/files" in rendered_screen()
        and "args=--console|a*b|$HOME" in rendered_screen(),
        "console Run output",
    )
    assert runs()[4] == {
        "cwd": str(files), "args": ["--console", "a*b", "$HOME"], "stdin": "console"
    }, runs()
    wait_console_idle("first console Run finished")
    type_console("cargo run -- 'quoted'")
    wait_for(lambda: "console command rejected" in normalized_since(0), "quoted refusal")
    # A rejected line stays at the prompt for editing; clear it.
    send(b"\x7f" * len("cargo run -- 'quoted'"))
    wait_for(lambda: "> ▏" in rendered_screen(), "empty prompt")
    send(b"\x1b")
    wait_for(lambda: " CONSOLE " not in rendered_screen(), "console closed")
    time.sleep(0.3)

    # A changed fixture tree shows in the picker before a run, still runs,
    # and the student sees why its result will not verify.
    (files / "data.txt").write_bytes(b"student edit\n")
    offset = len(transcript)
    send(b"\x1b[14~")
    wait_for(
        lambda: " TEST CASES " in rendered_screen()
        and "Arguments  «-i» «two words»" in rendered_screen()
        and "changed from the package; a run with them will not verify" in rendered_screen(),
        "picker reopened on echo with the changed-fixtures warning",
    )
    send(b"\r")
    wait_for(
        lambda: len(runs()) == 6
        and " TEST CASES " in rendered_screen()
        and "FAIL line 5" in rendered_screen()
        and "Result     FAIL at line 5" in rendered_screen()
        and 'expected (17 bytes) "data=fixture data"' in rendered_screen()
        and 'got (17 bytes) "data=student edit"' in rendered_screen()
        and "● warning" in toast_text_since(offset)
        and "warning: the files in lab.test-cases/files differ from the assignment package; "
        "the case runs with them as they are and will not verify against the package"
        in toast_text_since(offset),
        "changed-fixture warning and FAIL",
    )
    assert "● action failed" not in normalized_since(offset)
    send(b"\x1b")
    wait_for(lambda: " TEST CASES " not in rendered_screen(), "picker closed again")
    time.sleep(0.3)
    offset = len(transcript)
    send(b"\x1b[20~")
    wait_for(lambda: " CONSOLE " in rendered_screen(), "console again")
    type_console("cargo run -- --console x")
    wait_for(
        lambda: len(runs()) == 7
        and "data=student edit" in rendered_screen()
        and "● warning" in toast_text_since(offset)
        and "differ from the assignment package; your program runs with them as they are"
        in toast_text_since(offset),
        "console changed-fixture warning",
    )
    wait_console_idle("changed-fixture console Run finished")
    (files / "data.txt").write_bytes(data)

    # A workspace `.cargo` would configure F7 commands but not a Run from the
    # fixture folder, so the Run is refused before it starts.
    (workspace / ".cargo").mkdir()
    offset = len(transcript)
    type_console("cargo run -- --console y")
    wait_for(
        lambda: "● action failed" in toast_text_since(offset)
        and "remove `.cargo` from the workspace" in toast_text_since(offset),
        "workspace .cargo refusal",
    )
    time.sleep(0.3)
    assert len(runs()) == 7, runs()
    send(b"\x7f" * len("cargo run -- --console y"))
    wait_for(lambda: "> ▏" in rendered_screen(), "empty prompt again")
    send(b"\x1b")
    wait_for(lambda: " CONSOLE " not in rendered_screen(), "console closed again")
    time.sleep(0.3)

    # F7 Run is refused the same way and says why, not just "unavailable".
    offset = len(transcript)
    send(b"\x1b[18~")
    wait_for(lambda: " MENU " in rendered_screen(), "command menu for refused Run")
    send(b"\x1b[B\r")
    wait_for(
        lambda: "● action failed" in toast_text_since(offset)
        and "console command rejected: remove `.cargo` from the workspace"
        in toast_text_since(offset),
        "menu Run .cargo refusal",
    )
    assert "Command unavailable" not in toast_text_since(offset)
    time.sleep(0.3)
    assert len(runs()) == 7, runs()
    (workspace / ".cargo").rmdir()
    send(b"\x1b")
    wait_for(lambda: " CONSOLE " not in rendered_screen(), "console closed after refusal")
    time.sleep(0.3)

    # F7 Run is `cargo run` typed in the console, so it starts in the fixture
    # folder too and reads the console prompt; Ctrl-C stops it.
    send(b"\x1b[18~")
    wait_for(lambda: " MENU " in rendered_screen(), "command menu")
    send(b"\x1b[B\r")
    wait_for(lambda: len(runs()) == 8 and " CONSOLE " in rendered_screen(), "menu Run")
    assert runs()[7] == {"cwd": str(files), "args": [], "stdin": "pipe"}, runs()
    wait_for(lambda: "ctrl-c stop" in rendered_screen(), "running menu Run")
    send(b"\x03")
    wait_console_idle("menu Run stopped")
    send(b"\x1b")
    wait_for(lambda: " CONSOLE " not in rendered_screen(), "console closed after menu Run")
    time.sleep(0.2)

    send(b"\x11")
    while process.poll() is None and time.monotonic() < deadline:
        read_once()
    assert process.poll() is not None, "editor did not quit"
    while read_once(0.01):
        pass
    assert process.returncode == 0, repr(bytes(transcript[-8000:]))
    assert b"TERMINAL_RESTORED" in transcript, "terminal restoration marker missing"

    metadata = json.loads((workspace / ".rustrace/session.json").read_bytes())
    packaged = metadata["test_case_fixtures_hash"]
    database = workspace / ".rustrace" / f"{metadata['session_id']}.sqlite"
    with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
        events = [
            json.loads(payload)["event"]
            for (payload,) in connection.execute("SELECT payload FROM events ORDER BY sequence")
        ]
    starts = [
        event["payload"] for event in events
        if event["type"] == "controlled_command_started" and "console" in event["payload"]
    ]
    assert len(starts) == 8, starts
    fixtures = {"kind": "fixtures", "fixtures_blake3": packaged}
    echo_route = {
        "stdin": {"kind": "file", "path": "echo.in"},
        "stdout": {"kind": "console"},
        "args": ["-i", "two words"],
        "working_directory": fixtures,
        "test_case": "echo",
    }
    assert starts[0]["console"] == echo_route, starts[0]
    assert starts[0]["argv"][4:] == [
        "run", "--locked", "--manifest-path", "../../lab.work/Cargo.toml", "--", "-i", "two words"
    ], starts[0]["argv"]
    # Run all: echo, then the two cases without `.in`, with stdin closed.
    assert starts[1]["console"] == echo_route, starts[1]
    for start, name in [(starts[2], "quiet"), (starts[3], "touch")]:
        assert start["console"] == {
            "stdin": {"kind": "closed"},
            "stdout": {"kind": "console"},
            "args": [f"--{name}"],
            "working_directory": fixtures,
            "test_case": name,
        }, start
        assert start["argv"][4:] == [
            "run", "--locked", "--manifest-path", "../../lab.work/Cargo.toml", "--", f"--{name}"
        ], start["argv"]
    assert starts[4]["console"] == {
        "stdin": {"kind": "submitted"},
        "stdout": {"kind": "console"},
        "args": ["--console", "a*b", "$HOME"],
        "working_directory": fixtures,
    }, starts[4]
    changed = starts[5]["console"]["working_directory"]["fixtures_blake3"]
    assert changed != packaged
    assert starts[6]["console"]["working_directory"]["fixtures_blake3"] == changed
    assert starts[7]["console"] == {
        "stdin": {"kind": "submitted"},
        "stdout": {"kind": "console"},
        "working_directory": fixtures,
    }, starts[7]
    assert starts[7]["argv"][4:] == [
        "run", "--locked", "--manifest-path", "../../lab.work/Cargo.toml"
    ], starts[7]["argv"]
    comparisons = [event["payload"] for event in events if event["type"] == "test_case_compared"]
    assert [comparison["outcome"]["kind"] for comparison in comparisons] == [
        "pass", "pass", "pass", "pass", "mismatch"
    ]
    assert [comparison["case"] for comparison in comparisons] == [
        "echo", "echo", "quiet", "touch", "echo"
    ]
    for comparison in comparisons[:4]:
        assert comparison["invocation"]["fixtures_blake3"] == packaged, comparison
    assert comparisons[0]["invocation"]["stdin"]["kind"] == "file"
    assert comparisons[2]["invocation"]["stdin"] == {"kind": "closed"}
    assert comparisons[3]["invocation"]["stdin"] == {"kind": "closed"}
    assert comparisons[4]["invocation"]["fixtures_blake3"] == changed

    success = True
    print(
        "format 3 run PTY: picker listing and detail, a case with arguments and fixtures, "
        "closed-stdin cases, Run all, console arguments, changed-fixture indicator and "
        "warnings, typed and menu .cargo refusals, menu Run, and provenance passed"
    )
finally:
    (root / "format3-run-transcript.bin").write_bytes(transcript)
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
    os.close(master)
    os.close(slave)
    if success and os.environ.get("RUSTRACE_FORMAT3_PTY_RETAIN") != "1":
        import shutil

        shutil.rmtree(root)
