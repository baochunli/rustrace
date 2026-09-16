from test_home import isolate
isolate()

import fcntl
import os
import pty
import select
import struct
import sys
import termios
import time


def screen_text(output):
    rows = [[" "] * 80 for _ in range(24)]
    row = 0
    column = 0
    text = bytes(output).decode("utf-8", errors="replace")
    index = 0
    while index < len(text):
        character = text[index]
        if character == "\x1b" and index + 1 < len(text) and text[index + 1] == "[":
            end = index + 2
            while end < len(text) and not ("@" <= text[end] <= "~"):
                end += 1
            if end == len(text):
                break
            parameters = text[index + 2 : end].lstrip("?")
            values = [int(value) if value else 0 for value in parameters.split(";")]
            command = text[end]
            if command in ("H", "f"):
                row = max(0, (values[0] if values and values[0] else 1) - 1)
                column = max(0, (values[1] if len(values) > 1 and values[1] else 1) - 1)
            elif command == "A":
                row = max(0, row - (values[0] if values and values[0] else 1))
            elif command == "B":
                row = min(23, row + (values[0] if values and values[0] else 1))
            elif command == "C":
                column = min(79, column + (values[0] if values and values[0] else 1))
            elif command == "D":
                column = max(0, column - (values[0] if values and values[0] else 1))
            elif command == "G":
                column = max(0, (values[0] if values and values[0] else 1) - 1)
            elif command == "d":
                row = max(0, (values[0] if values and values[0] else 1) - 1)
            elif command == "J" and values and values[0] in (2, 3):
                rows = [[" "] * 80 for _ in range(24)]
            elif command == "K":
                rows[row][column:] = [" "] * (80 - column)
            index = end + 1
            continue
        if character == "\x1b":
            index += 2
            continue
        if character == "\r":
            column = 0
        elif character == "\n":
            row = min(23, row + 1)
        elif character == "\b":
            column = max(0, column - 1)
        elif character >= " ":
            if row < 24 and column < 80:
                rows[row][column] = character
            column = min(80, column + 1)
        index += 1
    return "\n".join("".join(row) for row in rows)


binary, bundle = sys.argv[1:]
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execv(binary, [binary, "replay", bundle])

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
captured = bytearray()
deadline = time.monotonic() + 20
while time.monotonic() < deadline and not (
    b"source" in captured and b"PAUSE" in captured
):
    ready, _, _ = select.select([fd], [], [], 0.1)
    if ready:
        try:
            captured.extend(os.read(fd, 65536))
        except OSError:
            break

if not (b"source" in captured and b"PAUSE" in captured):
    raise AssertionError(f"replay view did not render at 80x24: {bytes(captured)!r}")

# SGR left-down on the fourth visible event row seeks directly to event 4.
os.write(fd, b"\x1b[<0;2;10M")
selection_deadline = time.monotonic() + 5
navigation = bytearray()
while time.monotonic() < selection_deadline and "Attempt 1 event 4" not in screen_text(captured):
    ready, _, _ = select.select([fd], [], [], 0.1)
    if ready:
        try:
            chunk = os.read(fd, 65536)
            captured.extend(chunk)
            navigation.extend(chunk)
        except OSError:
            break
if "Attempt 1 event 4" not in screen_text(captured):
    raise AssertionError(f"mouse click did not seek to event 4: {bytes(navigation)!r}")
if "> 1:4" not in screen_text(captured):
    raise AssertionError(f"selected event marker was not emitted: {bytes(navigation)!r}")

# Start playback, provide no further input, and require the timer-driven state
# change to become visible. Then pause if needed and restore event 4.
os.write(fd, b" ")
playback_deadline = time.monotonic() + 5
while time.monotonic() < playback_deadline and "Attempt 1 event 4" in screen_text(captured):
    ready, _, _ = select.select([fd], [], [], 0.1)
    if ready:
        try:
            captured.extend(os.read(fd, 65536))
        except OSError:
            break
if "Attempt 1 event 4" in screen_text(captured):
    raise AssertionError("replay playback did not redraw after a timer-only advance")
if "PLAY 1x" in screen_text(captured):
    os.write(fd, b" ")
os.write(fd, b"\x1b[<0;2;10M")
selection_deadline = time.monotonic() + 5
while time.monotonic() < selection_deadline and "Attempt 1 event 4" not in screen_text(captured):
    ready, _, _ = select.select([fd], [], [], 0.1)
    if ready:
        try:
            captured.extend(os.read(fd, 65536))
        except OSError:
            break
if "Attempt 1 event 4" not in screen_text(captured):
    raise AssertionError("replay did not return to event 4 after timer playback")

# Drag and release are inert in replay. Wheel-down advances three events and
# wheel-up seeks back three, exercising the SGR 32, 0-release, 65, and 64 forms.
os.write(fd, b"\x1b[<32;3;10M\x1b[<0;3;10m\x1b[<65;2;10M")
wheel_deadline = time.monotonic() + 5
while time.monotonic() < wheel_deadline and "Attempt 1 event 7" not in screen_text(captured):
    ready, _, _ = select.select([fd], [], [], 0.1)
    if ready:
        try:
            captured.extend(os.read(fd, 65536))
        except OSError:
            break
if "Attempt 1 event 7" not in screen_text(captured):
    raise AssertionError("mouse wheel-down did not advance three replay events")
os.write(fd, b"\x1b[<64;2;10M")
wheel_deadline = time.monotonic() + 5
while time.monotonic() < wheel_deadline and "Attempt 1 event 4" not in screen_text(captured):
    ready, _, _ = select.select([fd], [], [], 0.1)
    if ready:
        try:
            captured.extend(os.read(fd, 65536))
        except OSError:
            break
if "Attempt 1 event 4" not in screen_text(captured):
    raise AssertionError("mouse wheel-up did not seek back three replay events")
os.write(fd, b"d")
diff_deadline = time.monotonic() + 5
diff_view = bytearray()
while time.monotonic() < diff_deadline and b"current -> final" not in diff_view:
    ready, _, _ = select.select([fd], [], [], 0.1)
    if ready:
        try:
            chunk = os.read(fd, 65536)
            captured.extend(chunk)
            diff_view.extend(chunk)
        except OSError:
            break
if b"current -> final" not in diff_view:
    raise AssertionError(f"bounded diff pane did not open: {bytes(diff_view)!r}")
if b"\x1b]52;unsafe" in diff_view:
    raise AssertionError("hostile source escaped through the diff pane")

control_errors = []
os.write(fd, b"\x03c")
control_deadline = time.monotonic() + 0.5
while time.monotonic() < control_deadline:
    ready, _, _ = select.select([fd], [], [], 0.05)
    if ready:
        try:
            chunk = os.read(fd, 65536)
            captured.extend(chunk)
        except OSError:
            break
screen = screen_text(captured)
if "comparison target: preceding checkpoint" not in screen:
    control_errors.append("Ctrl+C changed the comparison target")
else:
    os.write(fd, b"c")

os.write(fd, b"\x04d")
control_deadline = time.monotonic() + 0.5
while time.monotonic() < control_deadline:
    ready, _, _ = select.select([fd], [], [], 0.05)
    if ready:
        try:
            chunk = os.read(fd, 65536)
            captured.extend(chunk)
        except OSError:
            break
screen = screen_text(captured)
if "closed comparison; replay state unchanged" not in screen:
    control_errors.append("Ctrl+D closed the diff pane")
os.write(fd, b"q")

while time.monotonic() < deadline:
    ready, _, _ = select.select([fd], [], [], 0.1)
    if ready:
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            break
        if not chunk:
            break
        captured.extend(chunk)
    done, status = os.waitpid(pid, os.WNOHANG)
    if done:
        if not os.WIFEXITED(status) or os.WEXITSTATUS(status) != 0:
            raise AssertionError(f"replay exited with status {status}")
        break
else:
    os.kill(pid, 9)
    raise AssertionError("replay did not exit")

text = bytes(captured)
for expected in (b"package:OK", b"space play", b"PAUSE 1x"):
    if expected not in text:
        raise AssertionError(f"required 80x24 replay content is not visible: {expected!r}")
if b"\x1b]52;unsafe" in text:
    raise AssertionError("hostile source emitted a live OSC sequence")
if b"\\u{1b}" not in text:
    raise AssertionError("hostile source control was not visibly escaped")
if b"\x1b[?1049h" not in text or b"\x1b[?1049l" not in text:
    raise AssertionError("alternate screen was not restored")
if b"\x1b[?25h" not in text:
    raise AssertionError("cursor was not restored")
if control_errors:
    raise AssertionError("; ".join(control_errors))
