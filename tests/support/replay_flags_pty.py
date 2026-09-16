from test_home import isolate
isolate()

import fcntl
import os
import pty
import re
import select
import struct
import sys
import termios
import time


binary, bundle = sys.argv[1:]
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execv(binary, [binary, "replay", bundle])

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
transcript = bytearray()


def read_once(timeout=0.05):
    if not select.select([fd], [], [], timeout)[0]:
        return
    try:
        chunk = os.read(fd, 65536)
    except OSError:
        return
    transcript.extend(chunk)
    assert len(transcript) < 2 * 1024 * 1024, "terminal output exceeded fixture cap"


def rendered_screen():
    screen = [[" "] * 80 for _ in range(24)]
    row = column = 0
    text = transcript.decode("utf-8", "replace")
    for part in re.split(r"(\x1b\[[0-?]*[ -/]*[@-~])", text):
        if part.startswith("\x1b["):
            if part[2:3] == "?":
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


def wait_screen(value, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        read_once()
        if value in rendered_screen():
            return
    raise AssertionError(f"timed out waiting for {value!r}\n{rendered_screen()}")


def wait_main_surface(value, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        read_once()
        surface = "".join(line[26:].rstrip() for line in rendered_screen().splitlines())
        if value in surface:
            return
    raise AssertionError(f"timed out waiting for wrapped {value!r}\n{rendered_screen()}")


wait_screen("Flags: 2 (F next, V view)")
wait_screen(" PAUSE ")
wait_screen("space play")
wait_screen("s speed r follow Enter evidence")
os.write(fd, b"s")
wait_screen("4x")
os.write(fd, b"i")
os.write(fd, b"g")
wait_screen("followed review indicator event")
os.write(fd, b"\x1b[D" * 32)
wait_screen("Attempt 1 event 1")
os.write(fd, b"f")
wait_screen("followed UNPROVENANCED_EXTERNAL_CHANGE")
wait_screen("evidence artifact · segments/0001/evidence/")
os.write(fd, b"d")
wait_screen("current -> final")
if "evidence artifact ·" in rendered_screen():
    raise AssertionError("opening the diff pane did not close the artifact preview")
os.write(fd, b"d")
wait_screen("│A")
os.write(fd, b"\x1b[B")
wait_screen("Attempt 1 event 2")
os.write(fd, b"\x1b[A")
wait_screen("Attempt 1 event 1")
os.write(fd, b"d")
wait_screen("current -> final")
os.write(fd, b"v")
wait_screen("This package did not validate.")
if "current -> final" in rendered_screen():
    raise AssertionError("opening the flags view did not close the diff pane")
os.write(fd, b"d")
wait_screen("current -> final")
if "This package did not validate." in rendered_screen():
    raise AssertionError("opening the diff pane did not close the flags view")
os.write(fd, b"v")
wait_screen("This package did not validate.")
wait_main_surface("This package did not validate.")
wait_main_surface("It does not prove that the client was unmodified or that the recorded code")
wait_main_surface("originated from the student.")
wait_screen("lines 1-")
os.write(fd, b"\x1b[5~")
wait_screen("no further content")
os.write(fd, b"\x1b[6~")
wait_main_surface("SOURCE_MISMATCH [segment:1 seq:6]: the submitted source tree does not match")
os.write(fd, b"\x1b[6~" * 10)
wait_main_surface("Internal paste is allowed and not inherently suspicious.")
wait_screen("no further content")
os.write(fd, b"\x1b[5~" * 10)
wait_screen("UNPROVENANCED_EXTERNAL_CHANGE")
wait_screen("lines 1-")
os.write(fd, b"\x1b[6~")
wait_main_surface("SOURCE_MISMATCH [segment:1 seq:6]: the submitted source tree does not match")
os.write(fd, b"f")
wait_screen("followed SOURCE_MISMATCH")
wait_main_surface("SOURCE_MISMATCH [segment:1 seq:6]: the submitted source tree does not match")
os.write(fd, b"d")
wait_screen("current -> final")
os.write(fd, b"f")
wait_screen("followed UNPROVENANCED_EXTERNAL_CHANGE")
wait_screen("evidence artifact · segments/0001/evidence/")
if "current -> final" in rendered_screen():
    raise AssertionError("following an artifact flag did not close the diff pane")
os.write(fd, b"v")
wait_screen("UNPROVENANCED_EXTERNAL_CHANGE")
wait_screen("lines 1-")
os.write(fd, b"v")
wait_screen("│A")
os.write(fd, b"q")

deadline = time.monotonic() + 10
while time.monotonic() < deadline:
    read_once()
    done, status = os.waitpid(pid, os.WNOHANG)
    if done:
        if not os.WIFEXITED(status) or os.WEXITSTATUS(status) != 0:
            raise AssertionError(f"replay exited with status {status}")
        break
else:
    os.kill(pid, 9)
    raise AssertionError("replay did not exit")

captured = bytes(transcript)
if b"\x1b[?1049l" not in captured or b"\x1b[?25h" not in captured:
    raise AssertionError("terminal state was not restored")

print("flagged 80x24 status and flags-view toggle passed")
