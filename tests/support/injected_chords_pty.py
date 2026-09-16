#!/usr/bin/env python3
"""Drive T10.27 injected-text translations through real 80x24 PTYs."""

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

from test_home import isolate
isolate()


BINARY = str(pathlib.Path(sys.argv[1]).resolve())
SOURCE = "alpha beta\r\n  café gamma\r\nthird delta".encode()
TRANSLATED = b"\r\n\r\nthird "
MANIFEST = b'''format_version = 1
course_id = "course"
assignment_id = "injected-chords"
assignment_version = "v1"
title = "Injected chords"
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
GHOSTTY_CONFIGURED = r'''keybind = super+arrow_left=text:\x01
keybind = super+arrow_right=text:\x05
keybind = super+backspace=text:\x15
keybind = super+k=text:\x0b
keybind = alt+arrow_left=esc:b
keybind = alt+arrow_right=esc:f
'''
GHOSTTY_DEFAULTS = r'''keybind = super+arrow_left=text:\x01
keybind = super+arrow_right=text:\x05
keybind = super+backspace=text:\x15
keybind = super+k=clear_screen
keybind = alt+arrow_left=esc:b
keybind = alt+arrow_right=esc:f
'''
GHOSTTY_UNBOUND = '''keybind = super+arrow_left=unbind
keybind = super+arrow_right=unbind
keybind = super+backspace=unbind
keybind = super+k=unbind
keybind = alt+arrow_left=unbind
keybind = alt+arrow_right=unbind
'''
GHOSTTY_UNAVAILABLE = ""


def package_at(root):
    package = root / "assignment.rta"
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in [
            ("assignment.toml", MANIFEST),
            ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
            ("starter/main.rs", SOURCE),
        ]:
            info = tarfile.TarInfo(name)
            info.size = len(contents)
            info.mode = 0o600
            archive.addfile(info, io.BytesIO(contents))
    data = buffer.getvalue()
    while data.endswith(bytes(512)):
        data = data[:-512]
    package.write_bytes(data + bytes(1024))
    return package


def rendered_screen(data):
    screen = [[" "] * 80 for _ in range(24)]
    row = column = 0
    for part in re.split(
        r"(\x1b\[[0-?]*[ -/]*[@-~])", data.decode("utf-8", "replace")
    ):
        if part.startswith("\x1b["):
            if part[2:3] in "?><":
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


def journal_events(workspace):
    state = workspace / ".rustrace"
    metadata = json.loads((state / "session.json").read_bytes())
    with sqlite3.connect(state / (metadata["session_id"] + ".sqlite")) as connection:
        rows = connection.execute("SELECT payload FROM events ORDER BY sequence").fetchall()
    return [json.loads(raw) for (raw,) in rows]


def fake_ghostty(root, bindings):
    bin_dir = root / "bin"
    bin_dir.mkdir()
    binary = bin_dir / "ghostty"
    binary.write_text(
        "#!/bin/sh\n"
        "[ \"$1\" = +list-keybinds ] || exit 90\n"
        "printf '%s' \"$GHOSTTY_BINDINGS\"\n"
    )
    binary.chmod(0o700)
    return bin_dir


def exercise(
    root,
    enhancement_active,
    bindings=None,
    select_all_state=None,
    terminal_program=None,
):
    package = package_at(root)
    config = root / "config" / "rustrace"
    config.mkdir(parents=True)
    (config / "config.toml").write_text('modifier = "command"\n')
    workspace = root / "assignment.work"

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
    }
    if bindings is not None:
        bin_dir = fake_ghostty(root, bindings)
        environment.update({
            "TERM_PROGRAM": "ghostty",
            "GHOSTTY_BIN_DIR": str(bin_dir),
            "GHOSTTY_BINDINGS": bindings,
            "PATH": str(bin_dir) + os.pathsep + environment.get("PATH", ""),
        })
    elif terminal_program is not None:
        environment["TERM_PROGRAM"] = terminal_program
        environment.pop("GHOSTTY_BIN_DIR", None)
    else:
        environment.pop("TERM_PROGRAM", None)
        environment.pop("GHOSTTY_BIN_DIR", None)

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

    def drain():
        nonlocal answered_probe
        if select.select([master], [], [], 0.025)[0]:
            try:
                transcript.extend(os.read(master, 65536))
            except OSError:
                pass
        if not answered_probe and b"\x1b[?u\x1b[c" in transcript:
            response = b"\x1b[?0u\x1b[?1;0c" if enhancement_active else b"\x1b[?1;0c"
            os.write(master, response)
            answered_probe = True
        assert len(transcript) <= 2 * 1024 * 1024

    def wait_for(predicate, description, seconds=20):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            drain()
            if predicate():
                return
            if process.poll() is not None:
                break
        raise AssertionError(
            f"timed out waiting for {description}: {bytes(transcript[-5000:])!r}"
        )

    def wait_for_exit(description, seconds=20):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            drain()
            returncode = process.poll()
            if returncode is not None:
                return returncode
        raise AssertionError(
            f"timed out waiting for {description}: {bytes(transcript[-5000:])!r}"
        )

    def caret_at(line, column):
        marker = rf"Ln {line}, Col {column}(?!\d)"
        return re.search(marker, rendered_screen(transcript)) is not None

    def send(sequence, line, column):
        marker = f"Ln {line}, Col {column}"
        assert not caret_at(line, column), (
            f"wait target was already visible before {sequence!r}: {marker}"
        )
        os.write(master, sequence)
        wait_for(lambda: caret_at(line, column), marker)

    def keybinds_open():
        return "│keybinds" in rendered_screen(transcript)

    def open_keybinds(hints, description):
        assert not keybinds_open(), "keybinds overlay was open before F1"
        os.write(master, b"\x1bOP")
        wait_for(
            lambda: keybinds_open()
            and all(hint in rendered_screen(transcript) for hint in hints),
            description,
        )

    def close_keybinds():
        assert keybinds_open(), "keybinds overlay was not open before Esc"
        os.write(master, b"\x1b")
        wait_for(
            lambda: not keybinds_open() and caret_at(1, 1),
            "closed keybinds",
        )

    try:
        wait_for(
            lambda: answered_probe
            and b" files" in transcript
            and (workspace / "main.rs").is_file(),
            "workspace editor",
            30,
        )
        os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.
        wait_for(lambda: caret_at(1, 1), "initial caret")

        if select_all_state == "defaults":
            wait_for(lambda: b"\x1b[>5u" in transcript, "flags-5 keyboard push")
            send(b"\x05", 1, 11)      # stock Ghostty Super-Right
            send(b"\x01", 1, 1)       # stock Ghostty Super-Left
        elif select_all_state in ("unbound", "unavailable", "non-ghostty"):
            wait_for(lambda: b"\x1b[>5u" in transcript, "flags-5 keyboard push")
            if select_all_state == "unavailable":
                open_keybinds(
                    (
                        "Home / End",
                        "Control-Home / Control-End",
                        "Control-Left / Control-Right",
                        "Option-Backspace",
                        "Control-A",
                    ),
                    "unavailable-probe fallback hints",
                )
                screen = rendered_screen(transcript)
                for false_hint in ("⌘Left", "⌘Right", "⌘Backspace", "Control-U", "Control-K"):
                    assert false_hint not in screen, screen
                close_keybinds()
            elif select_all_state == "non-ghostty":
                open_keybinds(
                    (
                        "⌘Left",
                        "⌘Right",
                        "⌘Up",
                        "⌘Down",
                        "⌘Backspace",
                        "⌘F",
                        "⌘Z",
                    ),
                    f"{terminal_program} Command hints",
                )
                close_keybinds()
            assert not caret_at(3, 12), "Ctrl-A wait target was already visible"
            os.write(master, b"\x01")  # genuine Ctrl-A after the snippet
            wait_for(
                lambda: caret_at(3, 12),
                "Ctrl-A selected the full buffer",
            )
        elif enhancement_active:
            wait_for(lambda: b"\x1b[>5u" in transcript, "flags-5 keyboard push")
            # Ghostty 1.3.1 defaults inject these five forms for
            # Super-Left/Right/Backspace and Alt-Left/Right.
            send(b"\x05", 1, 11)       # default Super-Right: Ctrl-E
            send(b"\x1bb", 1, 7)      # default Alt-Left: Esc b
            send(b"\x1bf", 1, 11)     # default Alt-Right: Esc f
            send(b"\x01", 1, 1)       # default Super-Left: Ctrl-A
            os.write(master, b"\x0b")  # configured Ctrl-K injection
            wait_for(
                lambda: (workspace / "main.rs").read_bytes()
                == "\r\n  café gamma\r\nthird delta".encode(),
                "Ctrl-K bytes",
            )
            send(b"\x1b[B", 2, 1)
            send(b"\x05", 2, 13)
            os.write(master, b"\x15")  # default Super-Backspace: Ctrl-U
            wait_for(
                lambda: (workspace / "main.rs").read_bytes()
                == b"\r\n\r\nthird delta",
                "Ctrl-U bytes",
            )
            send(b"\x1b[B", 3, 1)
            send(b"\x05", 3, 12)
            send(b"\x1bb", 3, 7)
            send(b"\x1bf", 3, 12)
            # Option-Backspace has no Ghostty default binding. ESC Delete
            # exercises the legacy equivalent of protocol Alt-Backspace.
            os.write(master, b"\x1b\x7f")
            wait_for(
                lambda: (workspace / "main.rs").read_bytes() == TRANSLATED,
                "Alt-Backspace bytes",
            )
            wait_for(lambda: caret_at(3, 7), "final caret")
        else:
            assert b"\x1b[>5u" not in transcript
            os.write(master, b"\x01\x05\x15\x0b\x1bb\x1bf\x1b\x7f")
            time.sleep(0.2)
            drain()
            assert (workspace / "main.rs").read_bytes() == SOURCE

        os.write(master, b"\x11")
        returncode = wait_for_exit("clean exit", 30)
        while select.select([master], [], [], 0.025)[0]:
            length_before = len(transcript)
            drain()
            if len(transcript) == length_before:
                break
        assert returncode == 0, bytes(transcript[-5000:])
        assert b"TERMINAL_RESTORED" in transcript
        assert b"\x1b[?2004l" in transcript and b"\x1b[?1049l" in transcript
        if enhancement_active:
            assert transcript.count(b"\x1b[>5u") == 1
            assert transcript.count(b"\x1b[<1u") == 1
        else:
            assert b"\x1b[>5u" not in transcript
            assert b"\x1b[<1u" not in transcript
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

    events = journal_events(workspace)
    editing_events = [
        event["event"]
        for event in events
        if event["event"]["type"] in ("file_edited", "selection_changed")
    ]
    if select_all_state == "defaults":
        assert (workspace / "main.rs").read_bytes() == SOURCE
        assert [event["type"] for event in editing_events] == [
            "selection_changed",
            "selection_changed",
        ]
        assert [
            event["payload"]["active_byte"] for event in editing_events
        ] == [10, 0]
    elif select_all_state in ("unbound", "unavailable", "non-ghostty"):
        assert (workspace / "main.rs").read_bytes() == SOURCE
        assert [event["type"] for event in editing_events] == ["selection_changed"]
        selection = editing_events[0]["payload"]
        assert (selection["anchor_byte"], selection["active_byte"]) == (0, len(SOURCE))
    elif enhancement_active:
        assert (workspace / "main.rs").read_bytes() == TRANSLATED
        assert [event["type"] for event in editing_events] == [
            "selection_changed",
            "selection_changed",
            "selection_changed",
            "selection_changed",
            "file_edited",
            "selection_changed",
            "selection_changed",
            "file_edited",
            "selection_changed",
            "selection_changed",
            "selection_changed",
            "selection_changed",
            "file_edited",
        ], editing_events
        selections = [
            event["payload"]["active_byte"]
            for event in editing_events
            if event["type"] == "selection_changed"
        ]
        assert selections == [10, 6, 10, 0, 2, 15, 4, 15, 10, 15], selections
        transactions = [
            event["payload"]
            for event in editing_events
            if event["type"] == "file_edited"
        ]
        assert all(transaction["origin"] == "keyboard" for transaction in transactions)
        assert [
            (
                transaction["edits"][0]["start_byte"],
                transaction["edits"][0]["end_byte"],
                transaction["edits"][0]["inserted_text"],
            )
            for transaction in transactions
        ] == [(0, 10, ""), (2, 15, ""), (10, 15, "")]
    else:
        assert (workspace / "main.rs").read_bytes() == SOURCE
        assert [event["type"] for event in editing_events] == ["selection_changed"]
        selection = editing_events[0]["payload"]
        assert (selection["anchor_byte"], selection["active_byte"]) == (0, len(SOURCE))

    inspected = subprocess.run(
        [BINARY, "work", str(package), "--inspect"],
        capture_output=True,
        cwd=root,
        timeout=20,
    )
    assert inspected.returncode == 0, inspected.stdout + inspected.stderr
    expected = TRANSLATED if enhancement_active and select_all_state is None else SOURCE
    assert (
        json.dumps(expected.decode(), ensure_ascii=False).encode() in inspected.stdout
    ), inspected.stdout


with tempfile.TemporaryDirectory(prefix="rustrace-injected-chords-") as root_text:
    root = pathlib.Path(root_text)
    (root / "plain").mkdir()
    (root / "configured").mkdir()
    (root / "defaults").mkdir()
    (root / "unbound").mkdir()
    (root / "unavailable").mkdir()
    for terminal in ("WezTerm", "kitty", "iTerm2"):
        (root / terminal).mkdir()
    exercise(root / "plain", enhancement_active=False)
    exercise(root / "configured", enhancement_active=True, bindings=GHOSTTY_CONFIGURED)
    for terminal in ("WezTerm", "kitty", "iTerm2"):
        exercise(
            root / terminal,
            enhancement_active=True,
            select_all_state="non-ghostty",
            terminal_program=terminal,
        )
    exercise(
        root / "defaults",
        enhancement_active=True,
        bindings=GHOSTTY_DEFAULTS,
        select_all_state="defaults",
    )
    exercise(
        root / "unbound",
        enhancement_active=True,
        bindings=GHOSTTY_UNBOUND,
        select_all_state="unbound",
    )
    exercise(
        root / "unavailable",
        enhancement_active=True,
        bindings=GHOSTTY_UNAVAILABLE,
        select_all_state="unavailable",
    )

print("Terminal-aware injected chords, select all, journal, replay, and cleanup passed")
