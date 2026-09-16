"""Bounded observer for these fixtures' Ratatui cursor-addressed output.

Not a terminal emulator: no wrapping, terminal queries, or input handling.
Return the current alternate screen, or its final cells after terminal cleanup;
normal-buffer startup/wrapper output must not contaminate the UI assertions.
Raw transcript security and terminal-restoration checks belong to the callers.
The production copy path's OSC 52 write is out-of-band, never screen text;
this observer skips that one framing without decoding or acting on its payload.
Unicode handling covers CJK, combining marks and the captured modifier/ZWJ emoji,
not the entire Unicode grapheme-break or terminal-width specifications.
"""
import re
import unicodedata


_CSI = re.compile(r"\x1b\[([0-9;?]*)([A-Za-z])")
_OSC52 = re.compile(r"\x1b\]52;c;[A-Za-z0-9+/=]*\x07")


def rendered_screen(data, rows=36, columns=140):
    if not 1 <= rows <= 80 or not 1 <= columns <= 200:
        raise ValueError("PTY screen dimensions exceeded bound")
    if len(data) > 2 * 1024 * 1024:
        raise ValueError("PTY capture exceeded bound")
    cells = [[" "] * columns for _ in range(rows)]
    row = column = 0
    anchor = None
    exited = False
    text = data.decode("utf-8", "replace")
    offset = 0

    def erase(y, x):
        # A wide glyph owns both cells. Overwriting/erasing its continuation
        # must remove the old glyph, rather than manufacture overlapping text.
        if cells[y][x] == "" and x > 0:
            cells[y][x - 1] = " "
        if x + 1 < columns and cells[y][x + 1] == "":
            cells[y][x + 1] = " "
        cells[y][x] = " "

    while offset < len(text):
        character = text[offset]
        if character == "\x1b":
            clipboard = _OSC52.match(text, offset)
            if clipboard is not None:
                offset = clipboard.end()
                continue
            match = _CSI.match(text, offset)
            if match is None:
                # A read can end mid-CSI. Replaying the next larger capture
                # completes it; never expose that tail as printable UI text.
                tail = text[offset:]
                if (re.fullmatch(r"\x1b(?:\[[0-9;?]*)?", tail)
                        or "\x1b]52;c;".startswith(tail)
                        or re.fullmatch(r"\x1b\]52;c;[A-Za-z0-9+/=]*", tail)):
                    break
                raise ValueError("unsupported PTY escape framing")
            offset = match.end()
            parameters, code = match.groups()
            if parameters == "?1049" and code in ("h", "l"):
                exited = code == "l"
                if not exited:
                    cells = [[" "] * columns for _ in range(rows)]
                    row = column = 0
                anchor = None
                continue
            if exited or parameters.startswith("?") or code in ("m", "h", "l", "c", "u"):
                continue
            # Huge coordinates stay off-screen without unbounded integers.
            values = [int(value or "0") if len(value) <= 6 else 1000000
                      for value in parameters.split(";")]
            amount = values[0] or 1
            anchor = None
            if code in ("H", "f"):
                row = min(rows, amount - 1)
                column = min(columns, (values[1] or 1) - 1 if len(values) > 1 else 0)
            elif code == "G":
                column = min(columns, amount - 1)
            elif code == "A":
                row = max(0, row - amount)
            elif code == "B":
                row = min(rows, row + amount)
            elif code == "C":
                column = min(columns, column + amount)
            elif code == "D":
                column = max(0, column - amount)
            elif code == "J" and values[0] == 2:
                cells = [[" "] * columns for _ in range(rows)]
            elif code == "K" and row < rows:
                start = 0 if values[0] in (1, 2) else column
                end = min(columns, column + 1) if values[0] == 1 else columns
                for x in range(start, end):
                    erase(row, x)
            else:
                raise ValueError("unsupported PTY cursor command")
            continue
        offset += 1
        if exited:
            continue
        if character == "\r":
            column = 0
            anchor = None
        elif character == "\n":
            row = min(rows, row + 1)
            anchor = None
        elif character == "\t":
            column = min(columns, (column // 8 + 1) * 8)
            anchor = None
        elif unicodedata.category(character).startswith("C") and character != "\u200d":
            raise ValueError("unsupported raw PTY control")
        else:
            extends = (unicodedata.category(character).startswith("M")
                       or character == "\u200d"
                       or "\U0001f3fb" <= character <= "\U0001f3ff")
            if anchor is not None:
                y, x = anchor
                if extends or cells[y][x].endswith("\u200d"):
                    if len(cells[y][x]) >= 32:
                        raise ValueError("PTY cell cluster exceeded bound")
                    cells[y][x] += character
                    continue
            if extends:
                continue
            size = 2 if unicodedata.east_asian_width(character) in ("W", "F") else 1
            anchor = None
            if row < rows and column + size <= columns:
                for x in range(column, column + size):
                    erase(row, x)
                cells[row][column] = character
                if size == 2:
                    cells[row][column + 1] = ""
                anchor = (row, column)
            column = min(columns, column + size)
    return "\n".join("".join(line) for line in cells)
