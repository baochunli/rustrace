import fcntl
import glob
import io
import json
import os
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

from test_home import isolate
isolate()


binary = sys.argv[1]
manifest = b'''format_version = 1
course_id = "course"
assignment_id = "mouse"
assignment_version = "v1"
title = "Mouse PTY"
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
source = "\n".join(f"line {line:02}" for line in range(40)).encode()


def read_available(fd, transcript):
    while select.select([fd], [], [], 0.02)[0]:
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            return
        if not chunk:
            return
        transcript.extend(chunk)


def wait_for(predicate, fd, transcript, message, timeout=5):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        read_available(fd, transcript)
        if predicate():
            return
        time.sleep(0.02)
    raise AssertionError(message + ": " + repr(bytes(transcript[-4000:])))


def rendered_screen(transcript):
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
            elif 0 <= row < 24 and 0 <= column < 80:
                screen[row][column] = character
                column += 1
    return screen


def pane_geometry(transcript):
    screen = rendered_screen(transcript)
    width = sidebar_width(transcript)
    if width is None:
        return None
    divider_rows = [
        row
        for row in range(24)
        if screen[row][width - 1] == "├" and screen[row][width:] == ["─"] * (80 - width)
    ]
    if len(divider_rows) != 1:
        return None
    divider = divider_rows[0]
    if not "".join(screen[divider + 1][width:]).startswith("output"):
        return None
    return divider - 1, 23 - divider - 1


def sidebar_width(transcript):
    screen = rendered_screen(transcript)
    candidates = [
        column
        for column in range(80)
        if sum(screen[row][column] in ("│", "├") for row in range(24)) >= 23
    ]
    return candidates[0] + 1 if len(candidates) == 1 else None


with tempfile.TemporaryDirectory(prefix="rustrace-mouse-pty-") as root:
    package = os.path.join(root, "assignment.rta")
    with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
        for name, content in [
            ("assignment.toml", manifest),
            ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
            ("starter/a.rs", source),
            ("starter/b.rs", b"second file\n"),
        ]:
            info = tarfile.TarInfo(name)
            info.size = len(content)
            info.mode = 0o600
            archive.addfile(info, io.BytesIO(content))
    with open(package, "rb") as package_file:
        data = package_file.read()
    while data.endswith(bytes(512)):
        data = data[:-512]
    with open(package, "wb") as package_file:
        package_file.write(data + bytes(1024))

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    before = termios.tcgetattr(slave)

    def child():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    wrapper = (
        "import subprocess,termios,sys; "
        "before=termios.tcgetattr(0); "
        "result=subprocess.run(sys.argv[1:]); "
        "assert termios.tcgetattr(0)==before, 'raw mode leaked'; "
        "print('TERMINAL_RESTORED', flush=True); "
        "sys.exit(result.returncode)"
    )
    process = subprocess.Popen(
        [sys.executable, "-c", wrapper, binary, "work", package],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        preexec_fn=child,
        env={**os.environ, "TERM": "xterm-256color"},
        cwd=root,
    )
    transcript = bytearray()
    database = None
    try:
        wait_for(
            lambda: b" files" in transcript and b"a.rs" in transcript,
            master,
            transcript,
            "student shell did not render at 80x24",
            20,
        )
        wait_for(
            lambda: bool(glob.glob(os.path.join(root, "assignment.work", ".rustrace", "*.sqlite"))),
            master,
            transcript,
            "journal database did not appear",
        )
        database = glob.glob(
            os.path.join(root, "assignment.work", ".rustrace", "*.sqlite")
        )[0]

        def events():
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
                return [row[0] for row in connection.execute("SELECT payload FROM events ORDER BY sequence")]

        os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.
        wait_for(lambda: "line 00" in "\n".join("".join(row) for row in rendered_screen(transcript)), master, transcript, "source did not render")
        initial_count = len(events())
        wait_for(
            lambda: pane_geometry(transcript) == (15, 6),
            master,
            transcript,
            "initial editor/output pane heights were not stable at 15/6",
        )
        before_resize = len(events())
        assert sidebar_width(transcript) == 26
        os.write(
            master,
            b"\x1b[<0;26;2M"
            b"\x1b[<32;22;2M\x1b[<32;18;2M"
            b"\x1b[<0;18;2m",
        )
        wait_for(
            lambda: sidebar_width(transcript) == 18,
            master,
            transcript,
            "leftward sidebar drag did not clamp the width to 18",
        )
        os.write(
            master,
            b"\x1b[<0;18;2M"
            b"\x1b[<32;22;2M\x1b[<32;26;2M"
            b"\x1b[<0;26;2m",
        )
        wait_for(
            lambda: sidebar_width(transcript) == 26,
            master,
            transcript,
            "sidebar drag did not retain the restored session width",
        )
        assert len(events()) == before_resize, "sidebar resize changed provenance"
        os.write(
            master,
            b"\x1b[<0;30;17M"
            b"\x1b[<32;30;14M\x1b[<32;30;10M"
            b"\x1b[<0;30;10m",
        )
        wait_for(
            lambda: pane_geometry(transcript) == (8, 13),
            master,
            transcript,
            "upward divider drag did not resize panes to 8/13",
        )
        os.write(
            master,
            b"\x1b[<0;30;10M"
            b"\x1b[<32;30;18M\x1b[<32;30;22M"
            b"\x1b[<0;30;22m",
        )
        wait_for(
            lambda: pane_geometry(transcript) == (17, 4),
            master,
            transcript,
            "downward divider drag did not clamp panes to 17/4",
        )
        assert len(events()) == before_resize, "pane resize changed provenance"

        frames_before_motion = transcript.count(b"\x1b[?2026h")
        os.write(master, b"".join(
            f"\x1b[<35;{30 + index % 3};3M".encode() for index in range(200)
        ))
        time.sleep(0.35)
        read_available(master, transcript)
        assert len(events()) == initial_count, "idle mouse motion changed provenance"
        assert transcript.count(b"\x1b[?2026h") - frames_before_motion <= 5, (
            "idle mouse motion caused render churn"
        )

        # Click in source, then drag a bounded selection across the same row.
        os.write(master, b"\x1b[<0;30;2M\x1b[<0;30;2m")
        wait_for(
            lambda: len(events()) > initial_count,
            master,
            transcript,
            "editor click did not record its resulting selection",
        )
        os.write(
            master,
            b"\x1b[<0;28;2M"
            b"\x1b[<32;29;2M\x1b[<32;30;2M\x1b[<32;31;2M\x1b[<32;32;2M"
            b"\x1b[<0;32;2m",
        )
        wait_for(
            lambda: len(events()) >= initial_count + 3,
            master,
            transcript,
            "editor drag did not extend selection",
        )
        time.sleep(0.1)
        read_available(master, transcript)
        drag_records = [
            json.loads(payload)["event"]
            for payload in events()[initial_count + 1:]
            if json.loads(payload)["event"]["type"] == "selection_changed"
        ]
        assert len(drag_records) <= 5, "five-cell drag emitted too many selection states"
        assert drag_records, "drag emitted no resulting selection state"
        final_selection = drag_records[-1]["payload"]
        assert (final_selection["anchor_byte"], final_selection["active_byte"]) == (1, 5), (
            "queued drag did not retain its final cell: " + repr(final_selection)
        )
        os.write(master, b"\x03")  # Ctrl-C preserves the selected internal bytes.
        wait_for(
            lambda: any(b'"type":"clipboard_copied"' in payload for payload in events()),
            master,
            transcript,
            "keyboard copy did not establish internal clipboard authority",
        )

        before_wheel = len(events())
        os.write(master, b"\x1b[<65;30;5M\x1b[<64;30;5M")
        time.sleep(0.25)
        read_available(master, transcript)
        assert len(events()) == before_wheel, "wheel scrolling changed provenance"

        # Click b.rs below Cargo.toml and a.rs, then the a.rs tab pill.
        os.write(master, b"\x1b[<0;4;4M\x1b[<0;4;4m")
        wait_for(
            lambda: sum(b'"type":"file_focused"' in payload for payload in events()) >= 2,
            master,
            transcript,
            "sidebar file click did not activate b.rs",
        )
        os.write(master, b"\x01")  # Select the destination for replacement.
        os.write(master, b"\x1b[<2;34;4M")  # Right-button Down in editor source.
        wait_for(
            lambda: all(label in "\n".join("".join(row) for row in rendered_screen(transcript))
                        for label in ("Cut", "Copy", "Paste")),
            master,
            transcript,
            "editor context menu did not render at the click",
        )
        menu_screen = rendered_screen(transcript)
        paste_row = next(index for index, row in enumerate(menu_screen)
                         if "Paste" in "".join(row))
        paste_column = "".join(menu_screen[paste_row]).index("Paste")
        os.write(
            master,
            f"\x1b[<0;{paste_column + 1};{paste_row + 1}M".encode(),
        )
        wait_for(
            lambda: open(os.path.join(root, "assignment.work", "b.rs"), "rb").read()
                    == b"ine ",
            master,
            transcript,
            "left-down Paste did not apply the internal clipboard bytes",
            10,
        )
        context_events = [json.loads(payload) for payload in events()]
        copied = [event for event in context_events
                  if event["event"]["type"] == "clipboard_copied"]
        pasted = [event for event in context_events
                  if event["event"]["type"] == "internal_paste"]
        assert len(copied) == len(pasted) == 1, "context paste journal event missing"
        assert pasted[0]["event"]["payload"]["source"] == {
            "session_id": copied[0]["session_id"],
            "sequence": copied[0]["sequence"],
            "event_hash": copied[0]["event_hash"],
        }
        tab_column = "".join(rendered_screen(transcript)[0]).index("a.rs") + 1
        os.write(master, f"\x1b[<0;{tab_column};1M\x1b[<0;{tab_column};1m".encode())
        wait_for(
            lambda: sum(b'"type":"file_focused"' in payload for payload in events()) >= 3,
            master,
            transcript,
            "tab click did not reactivate a.rs",
        )

        encoded = b"\n".join(events()).lower()
        for forbidden in [b"mouse", b"click", b"scroll", b"coordinate", b"button"]:
            assert forbidden not in encoded, "mouse metadata entered provenance: " + repr(forbidden)

        os.write(master, b"\x11")
        wait_for(
            lambda: process.poll() is not None,
            master,
            transcript,
            "student shell did not exit",
            10,
        )
        read_available(master, transcript)
        assert process.returncode == 0, repr(bytes(transcript))
        assert b"TERMINAL_RESTORED" in transcript
        for mode in [1000, 1002, 1003, 1015, 1006]:
            assert f"\x1b[?{mode}h".encode() in transcript, f"mouse mode {mode} was not enabled"
            assert f"\x1b[?{mode}l".encode() in transcript, f"mouse mode {mode} was not disabled"
        assert b"\x1b[?1049h" in transcript and b"\x1b[?1049l" in transcript
        assert b"\x1b[?2004h" in transcript and b"\x1b[?2004l" in transcript
        assert open(os.path.join(root, "assignment.work", "a.rs"), "rb").read() == source
        assert open(os.path.join(root, "assignment.work", "b.rs"), "rb").read() == b"ine "
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

print("Mouse PTY pane resize, context paste, click, drag, wheel, sidebar, tabs, provenance, and capture cleanup passed")
