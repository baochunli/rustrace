from test_home import isolate
isolate()

import argparse
import fcntl
import os
import pty
import re
import select
import struct
import termios
import time


def screen_text(output, width, height):
    rows = [[" "] * width for _ in range(height)]
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
            amount = values[0] if values and values[0] else 1
            if command in ("H", "f"):
                row = max(0, amount - 1)
                column = max(0, (values[1] if len(values) > 1 and values[1] else 1) - 1)
            elif command == "A":
                row = max(0, row - amount)
            elif command == "B":
                row = min(height - 1, row + amount)
            elif command == "C":
                column = min(width - 1, column + amount)
            elif command == "D":
                column = max(0, column - amount)
            elif command == "G":
                column = max(0, amount - 1)
            elif command == "d":
                row = max(0, amount - 1)
            elif command == "J" and values and values[0] in (2, 3):
                rows = [[" "] * width for _ in range(height)]
            elif command == "K":
                rows[row][column:] = [" "] * (width - column)
            index = end + 1
            continue
        if character == "\x1b":
            index += 2
            continue
        if character == "\r":
            column = 0
        elif character == "\n":
            row = min(height - 1, row + 1)
        elif character == "\b":
            column = max(0, column - 1)
        elif character >= " ":
            if row < height and column < width:
                rows[row][column] = character
            column = min(width, column + 1)
        index += 1
    return "\n".join("".join(cells) for cells in rows)


def read_available(fd, captured, timeout=0.05):
    ready, _, _ = select.select([fd], [], [], timeout)
    if ready:
        try:
            captured.extend(os.read(fd, 65536))
        except OSError:
            return False
    return True


def wait_for(fd, captured, width, height, predicate, description, timeout=5):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        screen = screen_text(captured, width, height)
        if predicate(screen):
            return screen
        if not read_available(fd, captured, 0.1):
            break
    raise AssertionError(f"{description}:\n{screen_text(captured, width, height)}")


def selected_event(screen):
    selected = re.search(r"Attempt 1 event (\d+)", screen)
    return int(selected.group(1)) if selected else None


parser = argparse.ArgumentParser()
parser.add_argument("binary")
parser.add_argument("bundle")
parser.add_argument("width", type=int)
parser.add_argument("height", type=int)
parser.add_argument("--marker", default="let ")
args = parser.parse_args()

pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execv(args.binary, [args.binary, "replay", args.bundle])

fcntl.ioctl(
    fd,
    termios.TIOCSWINSZ,
    struct.pack("HHHH", args.height, args.width, 0, 0),
)
captured = bytearray()
try:
    screen = wait_for(
        fd,
        captured,
        args.width,
        args.height,
        lambda current: "PAUSE 1x follow:on" in current
        and "A1 t=" in current
        and selected_event(current) is not None,
        "replay Source status did not render",
        20,
    )

    for _ in range(64):
        if args.marker in screen:
            break
        previous = selected_event(screen)
        os.write(fd, b"\x1b[C")
        screen = wait_for(
            fd,
            captured,
            args.width,
            args.height,
            lambda current: "PAUSE 1x follow:on" in current
            and "A1 t=" in current
            and selected_event(current) is not None
            and selected_event(current) != previous,
            "replay did not advance while locating the recorded caret",
        )
    if args.marker not in screen:
        raise AssertionError(f"recorded Source marker {args.marker!r} was not visible")

    followed_first_row = screen.splitlines()[1][26:]
    os.write(fd, b"\x1b[<64;30;5M")
    manual = wait_for(
        fd,
        captured,
        args.width,
        args.height,
        lambda current: "follow:off" in current
        and current.splitlines()[1][26:] != followed_first_row,
        "Source wheel did not enter manual mode",
    )
    manual_first_row = manual.splitlines()[1][26:]

    previous = selected_event(manual)
    os.write(fd, b"\x1b[C")
    after_event = wait_for(
        fd,
        captured,
        args.width,
        args.height,
        lambda current: "follow:off" in current
        and selected_event(current) is not None
        and selected_event(current) != previous,
        "manual Source state did not survive an event change",
    )
    if after_event.splitlines()[1][26:] != manual_first_row:
        raise AssertionError("manual Source viewport moved on an event change")

    os.write(fd, b"v")
    wait_for(
        fd,
        captured,
        args.width,
        args.height,
        lambda screen: "follow:off" in screen and "It does not prove" in screen,
        "manual Source state did not survive a tab toggle",
    )
    os.write(fd, b"v")
    wait_for(
        fd,
        captured,
        args.width,
        args.height,
        lambda current: "follow:off" in current
        and current.splitlines()[1][26:] == manual_first_row,
        "manual Source viewport did not return after the tab toggle",
    )
    os.write(fd, b" ")
    wait_for(
        fd,
        captured,
        args.width,
        args.height,
        lambda screen: "PLAY 1x follow:off" in screen,
        "manual Source state did not survive Play",
    )
    os.write(fd, b" ")
    wait_for(
        fd,
        captured,
        args.width,
        args.height,
        lambda screen: "PAUSE 1x follow:off" in screen,
        "manual Source state did not survive Pause",
    )

    os.write(fd, b"r")
    restored = wait_for(
        fd,
        captured,
        args.width,
        args.height,
        lambda current: "PAUSE 1x follow:on" in current
        and args.marker in current
        and current.splitlines()[1][26:] != manual_first_row,
        "R did not restore recorded Source following",
    )
    gap_row = args.height - min(max(args.height // 3, 10), 12) - 2
    if "A1 t=" not in restored.splitlines()[gap_row][26:]:
        raise AssertionError("selected timestamp was not rendered in the layout gap")
    if "PAUSE" not in restored.splitlines()[args.height - 1][26:33]:
        raise AssertionError("the replay play pill moved from its existing coordinates")
finally:
    try:
        os.write(fd, b"q")
    except OSError:
        pass
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        read_available(fd, captured)
        done, status = os.waitpid(pid, os.WNOHANG)
        if done:
            if not os.WIFEXITED(status) or os.WEXITSTATUS(status) != 0:
                raise AssertionError(f"replay exited with status {status}")
            break
    else:
        os.kill(pid, 9)
        os.waitpid(pid, 0)
        raise AssertionError("replay did not exit")
