"""T10.28 80x24 compiler tint, click-to-output, and provenance proof."""
import fcntl
import io
import json
import os
import pathlib
import pty
import re
import select
import shutil
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


WIDTH = 80
HEIGHT = 24
binary = str(pathlib.Path(sys.argv[1]).resolve())
root = pathlib.Path(tempfile.mkdtemp(prefix="rustrace-diagnostic-tints-pty-"))
fixture = pathlib.Path(__file__).with_name("command_rustup.py").read_bytes()
bin_dir = root / "bin"
(bin_dir / "v1").mkdir(parents=True)
for target in [bin_dir / "rustup"] + [bin_dir / "v1" / name for name in
        ["rustc", "cargo", "rustdoc", "rust-analyzer", "cargo-clippy", "cargo-fmt", "rustfmt"]]:
    target.write_bytes(fixture)
    target.chmod(0o755)

manifest = b'''format_version = 1
course_id = "course"
assignment_id = "diagnostic-tints"
assignment_version = "v1"
title = "Diagnostic tints"
toolchain = "fixture"
edition = "2024"
allowed_paths = ["*.rs", "Cargo.lock", "Cargo.toml"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
'''
source = b"warning\nerror\n"
package = root / "assignment.rta"
with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
    for name, content in [
        ("assignment.toml", manifest),
        ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
        ("starter/main.rs", source),
        ("starter/Cargo.lock", b"lock fixture"),
    ]:
        info = tarfile.TarInfo(name)
        info.size = len(content)
        info.mode = 0o600
        archive.addfile(info, io.BytesIO(content))
data = package.read_bytes()
while data.endswith(bytes(512)):
    data = data[:-512]
package.write_bytes(data + bytes(1024))

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", HEIGHT, WIDTH, 0, 0))


def own_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


wrapper = (
    "import subprocess,termios,sys; before=termios.tcgetattr(0); "
    "result=subprocess.run(sys.argv[1:]); "
    "assert termios.tcgetattr(0)==before, 'raw mode leaked'; "
    "print('TERMINAL_RESTORED', flush=True); sys.exit(result.returncode)"
)
child_env = {key: value for key, value in os.environ.items() if key != "NO_COLOR"}
child_env.update({
    "PATH": str(bin_dir) + os.pathsep + os.environ["PATH"],
    "TERM": "xterm-256color",
    "COLORTERM": "",
})
process = subprocess.Popen(
    [sys.executable, "-c", wrapper, binary, "work", str(package)],
    stdin=slave,
    stdout=slave,
    stderr=slave,
    preexec_fn=own_terminal,
    cwd=root,
    env=child_env,
)
transcript = bytearray()


def read_available():
    while select.select([master], [], [], 0.02)[0]:
        try:
            chunk = os.read(master, 65536)
        except OSError:
            return
        if not chunk:
            return
        transcript.extend(chunk)


def wait_for(predicate, message, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        read_available()
        if predicate():
            return
        time.sleep(0.02)
    raise AssertionError(message + ": " + repr(bytes(transcript[-6000:])))


def terminal_screen():
    cells = [[{"char": " ", "fg": None, "bg": None, "bold": False}
              for _ in range(WIDTH)] for _ in range(HEIGHT)]
    row = column = 0
    fg = bg = None
    bold = False
    pattern = r"(\x1b\[[0-?]*[ -/]*[@-~])"
    for part in re.split(pattern, transcript.decode("utf-8", "replace")):
        if part.startswith("\x1b["):
            if part == "\x1b[?1049h":
                cells = [[{"char": " ", "fg": None, "bg": None, "bold": False}
                          for _ in range(WIDTH)] for _ in range(HEIGHT)]
                row = column = 0
                fg = bg = None
                bold = False
                continue
            if part[2:3] == "?":
                continue
            code = part[-1]
            raw = part[2:-1]
            values = [int(value or "0") for value in raw.split(";")] if raw else [0]
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
                cells = [[{"char": " ", "fg": None, "bg": None, "bold": False}
                          for _ in range(WIDTH)] for _ in range(HEIGHT)]
            elif code == "K" and 0 <= row < HEIGHT:
                start = 0 if values[0] in (1, 2) else column
                end = column + 1 if values[0] == 1 else WIDTH
                for x in range(start, min(end, WIDTH)):
                    cells[row][x] = {"char": " ", "fg": fg, "bg": bg, "bold": bold}
            elif code == "m":
                index = 0
                while index < len(values):
                    value = values[index]
                    if value == 0:
                        fg = bg = None
                        bold = False
                    elif value == 1:
                        bold = True
                    elif value == 22:
                        bold = False
                    elif value == 39:
                        fg = None
                    elif value == 49:
                        bg = None
                    elif 30 <= value <= 37 or 90 <= value <= 97:
                        fg = value
                    elif 40 <= value <= 47 or 100 <= value <= 107:
                        bg = value
                    elif value in (38, 48) and index + 2 < len(values) and values[index + 1] == 5:
                        if value == 38:
                            fg = ("indexed", values[index + 2])
                        else:
                            bg = ("indexed", values[index + 2])
                        index += 2
                    elif value in (38, 48) and index + 4 < len(values) and values[index + 1] == 2:
                        colour = tuple(values[index + 2:index + 5])
                        if value == 38:
                            fg = colour
                        else:
                            bg = colour
                        index += 4
                    index += 1
            continue
        for character in part:
            if character == "\r":
                column = 0
            elif character == "\n":
                row += 1
            elif character >= " ":
                if 0 <= row < HEIGHT and 0 <= column < WIDTH:
                    cells[row][column] = {
                        "char": character, "fg": fg, "bg": bg, "bold": bold,
                    }
                column += 1
    return cells


def text_rows(cells):
    return ["".join(cell["char"] for cell in row) for row in cells]


def event_payloads(database):
    with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
        return [json.loads(raw)["event"]
                for (raw,) in connection.execute("SELECT payload FROM events ORDER BY sequence")]


def selected_diagnostic_visible(text, caret):
    cells = terminal_screen()
    for row in cells:
        rendered = "".join(cell["char"] for cell in row)
        first = next((cell for cell in row[26:] if cell["char"] != " "), None)
        if text in rendered and first is not None and first["fg"] == ("indexed", 4):
            return caret in "\n".join(text_rows(cells))
    return False


success = False
try:
    wait_for(lambda: b" files" in transcript, "student shell did not render")
    os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.
    work = root / "assignment.work"
    (work / "target").mkdir(exist_ok=True)
    (work / "target/runner-fixture.json").write_text(
        json.dumps({"mode": "diagnostic_tints", "exit": 101})
    )
    os.write(master, b"\x1b[18~\r")
    activity = work / ".rustrace/command-activity.json"
    wait_for(
        lambda: activity.exists() and not json.loads(activity.read_bytes())["active"],
        "controlled Check did not finish",
    )
    database = next((work / ".rustrace").glob("*.sqlite"))
    cells = terminal_screen()
    rows = text_rows(cells)
    warning_row = next(index for index, row in enumerate(rows) if row[26:].startswith("warning"))
    error_row = next(index for index, row in enumerate(rows) if row[26:].startswith("error"))
    assert rows[warning_row][26:33] == "warning", rows[warning_row]
    assert rows[error_row][26:31] == "error", rows[error_row]
    assert all(cells[warning_row][x]["bg"] == ("indexed", 3)
               for x in range(26, WIDTH))
    assert all(cells[error_row][x]["bg"] == ("indexed", 1)
               for x in range(26, WIDTH))

    baseline = event_payloads(database)
    os.write(master, f"\x1b[<0;30;{warning_row + 1}M".encode())
    wait_for(
        lambda: selected_diagnostic_visible("warning[W0001]", "Ln 1, Col 4"),
        "warning click did not reveal an accent-marked output row",
    )
    after_warning = event_payloads(database)
    assert [event["type"] for event in after_warning[len(baseline):]] == ["selection_changed"]

    os.write(master, f"\x1b[<0;29;{error_row + 1}M".encode())
    wait_for(
        lambda: selected_diagnostic_visible("error[E0308]", "Ln 2, Col 3"),
        "error click did not reveal an accent-marked output row",
    )
    final_events = event_payloads(database)
    assert [event["type"] for event in final_events[len(after_warning):]] == ["selection_changed"]
    encoded = json.dumps(final_events[len(baseline):]).lower()
    for forbidden in ["mouse", "click", "coordinate", "button"]:
        assert forbidden not in encoded, forbidden

    os.write(master, b"\x11")
    wait_for(lambda: process.poll() is not None, "student shell did not exit")
    read_available()
    assert process.returncode == 0, repr(bytes(transcript[-6000:]))
    assert b"TERMINAL_RESTORED" in transcript
    success = True
finally:
    (root / "transcript.bin").write_bytes(transcript)
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
    os.close(master)
    os.close(slave)
    if success:
        shutil.rmtree(root)
    else:
        print("FAILED fixture preserved:", root, file=sys.stderr)

print("80x24 diagnostic tints, click-to-output, and provenance passed")
