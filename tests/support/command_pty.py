"""Trusted 80x24 production command routing fixture; preserve first failures."""
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

from test_home import isolate
isolate()

binary = str(pathlib.Path(sys.argv[1]).resolve())
mode = sys.argv[2] if len(sys.argv) > 2 else "source"
retained_root = os.environ.get("RUSTRACE_T87_COMMAND_ROOT")
root = (pathlib.Path(retained_root).resolve() if retained_root
        else pathlib.Path(tempfile.mkdtemp(prefix="rustrace-command-pty-")))
if retained_root:
    root.mkdir()
fixture = pathlib.Path(__file__).with_name("command_rustup.py").read_bytes()
bin_dir = root / "bin"
(bin_dir / "v1").mkdir(parents=True)
for target in [bin_dir / "rustup"] + [bin_dir / "v1" / name for name in
        ["rustc", "cargo", "rustdoc", "rust-analyzer", "cargo-clippy", "cargo-fmt", "rustfmt"]]:
    target.write_bytes(fixture)
    target.chmod(0o755)
manifest = b'''format_version = 1
course_id = "course"
assignment_id = "command-pty"
assignment_version = "v1"
title = "Command PTY"
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
package = root / "assignment.rta"
initial_source = "Bé🦀X".encode() if mode in ("diagnostic", "diagnostic_mouse") else (
    b"fn main(){}\n" if mode == "format_rejected" else b"A"
)
expected_source = b"XA" if mode in ("save_check", "manual_check") else initial_source
initial_lock = b"format_rejected" if mode == "format_rejected" else b"lock fixture"
with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
    for name, content in [("assignment.toml", manifest), ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'), ("starter/main.rs", initial_source), ("starter/Cargo.lock", initial_lock)]:
        info = tarfile.TarInfo(name)
        info.size = len(content)
        info.mode = 0o600
        archive.addfile(info, io.BytesIO(content))
data = package.read_bytes()
while data.endswith(bytes(512)):
    data = data[:-512]
package.write_bytes(data + bytes(1024))
work = root / "assignment.work"
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
before = termios.tcgetattr(slave)

def own_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)

wrapper = "import subprocess,termios,sys; before=termios.tcgetattr(0); r=subprocess.run(sys.argv[1:]); assert termios.tcgetattr(0)==before, 'raw mode leaked'; print('TERMINAL_RESTORED', flush=True); sys.exit(r.returncode)"
proc = subprocess.Popen([sys.executable, "-c", wrapper, binary, "work", str(package)],
    stdin=slave, stdout=slave, stderr=slave, preexec_fn=own_terminal, cwd=root,
    env={**os.environ, "PATH":str(bin_dir) + os.pathsep + os.environ["PATH"], "TERM":"xterm-256color"})
transcript = bytearray()

def rendered_screen(data):
    """Reconstruct Ratatui's cursor-addressed diff output."""
    screen = [[" "] * 80 for _ in range(24)]
    row = column = 0
    for part in re.split(r"(\x1b\[[0-?]*[ -/]*[@-~])", data.decode("utf-8", "replace")):
        if part.startswith("\x1b["):
            code = part[-1]
            if part[2:3] == "?":
                continue
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

def main_surface(data):
    """Join the main pane so exact toast text remains assertable after wrapping."""
    return " ".join(
        line[26:].replace("│", " ").strip()
        for line in rendered_screen(data).splitlines()
    )

def process_identity():
    identity = {"pid":proc.pid,"poll":proc.poll(),"expected_executable":sys.executable}
    identity["ps"] = subprocess.run(["ps", "-p", str(proc.pid), "-o", "pid,ppid,pgid,uid,lstart,comm,args"], capture_output=True, text=True).stdout
    try:
        identity["sid"] = os.getsid(proc.pid)
        identity["pgid"] = os.getpgid(proc.pid)
    except ProcessLookupError:
        identity["exited"] = True
    return identity

def recorded_event_parity():
    metadata = json.loads((work / ".rustrace/session.json").read_bytes())
    connection = sqlite3.connect(work / ".rustrace" / (metadata["session_id"] + ".sqlite"))
    rows = connection.execute("SELECT payload FROM events ORDER BY sequence").fetchall()
    connection.close()
    recorded = [json.loads(raw)["event"] for (raw,) in rows]
    for event in recorded:
        payload = event["payload"]
        if event["type"] == "controlled_command_started":
            payload["argv"][0] = pathlib.Path(payload["argv"][0]).name
            payload["argv"][3] = pathlib.Path(payload["argv"][3]).name
            payload["before"].pop("checkpoint_event_hash")
            for tool in payload["tools"]:
                tool["executable"] = pathlib.Path(tool["executable"]).name
        elif event["type"] == "controlled_command_output":
            output = bytes.fromhex(payload["bytes_hex"])
            output = output.replace(os.fsencode(root), b"<FIXTURE_ROOT>")
            payload["bytes_hex"] = output.hex()
        elif event["type"] == "controlled_command_finished":
            payload.pop("started_millis")
            payload.pop("finished_millis")
            payload["after"].pop("checkpoint_event_hash")
    return json.dumps(recorded, sort_keys=True, separators=(",", ":"))

(root / "process-start.json").write_text(json.dumps(process_identity()))
deadline = time.monotonic() + (90 if mode == "huge" else 25)
started_at = None
quit_sent = False
cancel_sent = False
active_paste_sent_at = None
picker_paste_sent_at = None
picker_closed_at = None
picker_reopened_at = None
picker_final_closed_at = None
format_entered_at = None
finished_at = None
diagnostics_seen_at = None
diagnostic_opened_at = None
diagnostic_keys_sent_at = None
diagnostic_picker_opened_at = None
diagnostic_picker_closed_at = None
success = False
manual_check_sent = False
save_toast_seen = False
try:
    while proc.poll() is None and time.monotonic() < deadline:
        if select.select([master], [], [], .05)[0]:
            transcript.extend(os.read(master, 65536))
        assert len(transcript) < 2 * 1024 * 1024, "terminal output exceeded fixture cap"
        if started_at is None and b" files" in transcript:
            os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.
            (work / "target").mkdir(exist_ok=True)
            runner_mode = "diagnostic" if mode == "diagnostic_mouse" else mode if mode in (
                "source", "diagnostic", "format_rejected", "missing", "read_failed", "huge"
            ) else "short_sleep" if mode == "save_check" else "bytes" if mode == "manual_check" else "sleep"
            runner_exit = 101 if mode in ("diagnostic", "diagnostic_mouse") else 0
            (work / "target/runner-fixture.json").write_text(json.dumps({"mode":runner_mode, "exit":runner_exit}))
            if mode == "picker_paste":
                os.write(master, b"\x1b[18~")  # F7 opens the command picker.
            elif mode == "save_check":
                os.write(master, b"X\x13")  # Edit, then explicit save starts Check.
            elif mode == "manual_check":
                os.write(master, b"X")  # Autosave must remain command-silent.
            elif mode == "format_mouse":
                os.write(master, b"\x1b[18~")  # F7; click Format after the picker renders.
            elif mode in ("format", "format_rejected"):
                os.write(master, b"\x1b[18~" + b"\x1b[B" * 3)  # F7, down to Format.
            else:
                os.write(master, b"\x1b[18~\r")  # F7, default Check, Enter.
            started_at = time.monotonic()
        if started_at is None:
            continue
        if mode == "save_check" and not save_toast_seen:
            screen = rendered_screen(transcript)
            if "File saved" in screen:
                assert screen.count("File saved") == 1, repr(screen)
                assert " complete " in screen, repr(screen)
                save_toast_seen = True
        if mode == "manual_check" and not manual_check_sent:
            if (work / "main.rs").is_file() and (work / "main.rs").read_bytes() == expected_source:
                assert not (work / "target/invocation.json").exists(), \
                    "autosave started a command invocation"
                assert not (work / ".rustrace/command-activity.json").exists(), \
                    "autosave claimed command ownership"
                assert not list((work / ".rustrace").glob("command-*-capture.json")), \
                    "autosave recorded command evidence"
                os.write(master, b"\x1b[18~\r")  # Manual Check after autosave.
                manual_check_sent = True
            else:
                continue
        if mode in ("format", "format_mouse", "format_rejected") and format_entered_at is None:
            if time.monotonic() - started_at > .25:
                screen = rendered_screen(transcript)
                assert " MENU " in screen and "Format" in screen, repr(bytes(transcript[-6000:]))
                if mode == "format_mouse":
                    assert screen.splitlines()[13][3:9] == "Format", screen
                    # Final 80x24 menu Format row: zero-based x=3,y=13,
                    # encoded as SGR 1-based cells.
                    os.write(master, b"\x1b[<0;4;14M")
                else:
                    os.write(master, b"\r")
                format_entered_at = time.monotonic()
            continue
        if mode == "picker_paste":
            elapsed = time.monotonic() - started_at
            if picker_paste_sent_at is None and elapsed > .25:
                screen = rendered_screen(transcript)
                assert " MENU " in screen and "Check" in screen, "F7 command picker did not render"
                os.write(master, b"\x16")  # Ctrl-V must be rejected before picker routing.
                picker_paste_sent_at = time.monotonic()
            elif picker_closed_at is None and picker_paste_sent_at is not None and elapsed > .55:
                assert "Paste blocked:" in rendered_screen(transcript), repr(bytes(transcript[-6000:]))
                os.write(master, b"\x1b")
                picker_closed_at = time.monotonic()
            elif picker_reopened_at is None and picker_closed_at is not None and elapsed > .85:
                assert "command selection closed" in rendered_screen(transcript)
                os.write(master, b"\x1b[18~")
                picker_reopened_at = time.monotonic()
            elif picker_reopened_at is not None and elapsed > 1.15 and picker_final_closed_at is None:
                screen = rendered_screen(transcript)
                assert " MENU " in screen and "Check" in screen, "command picker did not reopen"
                os.write(master, b"\x1b")
                picker_final_closed_at = time.monotonic()
            elif picker_final_closed_at is not None and elapsed > 1.45 and not quit_sent:
                assert "command selection closed" in rendered_screen(transcript)
                os.write(master, b"\x11")
                quit_sent = True
            continue
        marker_path = work / ".rustrace/command-activity.json"
        assert marker_path.exists() or time.monotonic() - started_at < 4, "F7 did not start a controlled command"
        invocation = work / "target/invocation.json"
        if invocation.exists() and mode == "active_paste" and active_paste_sent_at is None:
            os.write(master, b"\x1b[200~ACTIVE_PASTE_MUST_NOT_PERSIST\x1b[201~")
            active_paste_sent_at = time.monotonic()
        if invocation.exists() and mode in ("cancel", "quit", "active_paste") and not cancel_sent and (
                mode != "active_paste" or time.monotonic() - active_paste_sent_at > .2):
            os.write(master, b"\x11" if mode == "quit" else b"\x1b")
            cancel_sent = True
            quit_sent = mode == "quit"
        if marker_path.exists() and not json.loads(marker_path.read_bytes())["active"]:
            if finished_at is None:
                finished_at = time.monotonic()
            if mode in ("diagnostic", "diagnostic_mouse"):
                elapsed = time.monotonic() - finished_at
                screen = rendered_screen(transcript)
                if diagnostics_seen_at is None and elapsed > .25:
                    assert "│output" in screen, repr(screen)
                    assert "diagnostics " not in screen.lower(), repr(screen)
                    assert "Alt-Up/Down navigate" not in screen, repr(screen)
                    assert "valid Unicode byte span" in screen, repr(screen)
                    assert "│Bé🦀" in screen, repr(screen)
                    if mode == "diagnostic_mouse":
                        # First rendered diagnostic row at zero-based x=26,y=18.
                        os.write(master, b"\x1b[<0;27;19M")
                    else:
                        os.write(master, b"\x1b[1;3B")  # Alt-Down selects and navigates.
                    diagnostics_seen_at = time.monotonic()
                elif diagnostics_seen_at is None:
                    continue
                elif diagnostic_opened_at is None and elapsed > .25:
                    assert "Ln 1, Col 5" in screen, repr(screen)
                    assert "opened diagnostic target" not in screen, repr(screen)
                    assert " COMPLETE " not in screen, repr(screen)
                    os.write(master, b"\x1b[15~\x1b[17~\x1b[19~")  # F5, F6, reserved F8.
                    diagnostic_opened_at = time.monotonic()
                elif diagnostic_keys_sent_at is None and elapsed > .55:
                    assert len(list((work / ".rustrace").glob("command-*-capture.json"))) == 1
                    assert (work / "main.rs").read_bytes() == initial_source
                    os.write(master, b"\x1b[18~")  # F7 remains the command picker.
                    diagnostic_keys_sent_at = time.monotonic()
                elif diagnostic_picker_opened_at is None and elapsed > .85:
                    assert " MENU " in screen and "Check" in screen, repr(screen)
                    os.write(master, b"\x1b")
                    diagnostic_picker_opened_at = time.monotonic()
                elif diagnostic_picker_closed_at is None and elapsed > 1.15:
                    assert "command selection closed" in screen, repr(screen)
                    os.write(master, b"\x11")
                    diagnostic_picker_closed_at = time.monotonic()
                    quit_sent = True
                continue
            if not quit_sent and time.monotonic() - finished_at > .3:
                if mode in ("format", "format_mouse"):
                    screen = rendered_screen(transcript)
                    assert "Format: no changes" not in screen, repr(bytes(transcript[-6000:]))
                    assert len(list((work / ".rustrace").glob("command-*-capture.json"))) == 1
                elif mode == "format_rejected":
                    surface = main_surface(transcript)
                    assert "Format: unsafe or invalid result rejected; inspect evidence" in surface, repr(bytes(transcript[-6000:]))
                    assert "Command stopped; inspect evidence or tool installation" not in surface
                elif mode == "save_check":
                    screen = rendered_screen(transcript)
                    assert save_toast_seen, repr(screen)
                    assert "File saved" not in screen, repr(screen)
                    assert "Command: exit 0; capture recorded" not in screen, repr(screen)
                elif mode == "manual_check":
                    screen = rendered_screen(transcript)
                    assert "Command: exit 0; capture recorded" not in screen, repr(bytes(transcript[-6000:]))
                    assert "stdout\\xff" in screen and "stderr\\xfe" in screen, repr(bytes(transcript[-6000:]))
                os.write(master, b"\x11")
                quit_sent = True
    assert proc.poll() is not None, "CLI command fixture hung"
    while select.select([master], [], [], .05)[0]:
        chunk = os.read(master, 65536)
        if not chunk:
            break
        transcript.extend(chunk)
    assert proc.returncode == 0, repr(bytes(transcript[-6000:]))
    assert quit_sent and b"TERMINAL_RESTORED" in transcript
    assert b"\x1b[?1049l" in transcript and b"\x1b[?2004l" in transcript
    assert b"\x1b]52;c;fixture" not in transcript, "raw captured OSC52 reached terminal"
    assert b"stdout\xff" not in transcript, "raw invalid UTF8 reached terminal"
    assert b"ACTIVE_PASTE_MUST_NOT_PERSIST" not in transcript
    assert (work / "main.rs").read_bytes() == expected_source
    assert (work / "Cargo.lock").read_bytes() == initial_lock
    if mode == "picker_paste":
        assert not (work / ".rustrace/command-activity.json").exists()
        assert not (work / "target/invocation.json").exists()
        assert not list((work / ".rustrace").glob("command-*-capture.json"))
        metadata = json.loads((work / ".rustrace/session.json").read_bytes())
        connection = sqlite3.connect(work / ".rustrace" / (metadata["session_id"] + ".sqlite"))
        rows = connection.execute("SELECT payload FROM events ORDER BY sequence").fetchall()
        connection.close()
        rejected = []
        for (raw,) in rows:
            event = json.loads(raw)
            if event["event"]["type"] == "paste_rejected":
                assert len(raw) <= 1024
                rejected.append(event["event"]["payload"])
        assert rejected == [{"reason":"outside_editor", "channel":"internal_shortcut"}]
    else:
        assert not json.loads((work / ".rustrace/command-activity.json").read_bytes())["active"]
        captures = list((work / ".rustrace").glob("command-*-capture.json"))
        assert len(captures) == 1
        capture = json.loads(captures[0].read_bytes())
    if mode == "source":
        assert capture["execution"]["outcome"] == {"kind":"exited", "code":0}
        display = b" ".join(re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b" ", transcript).split())
        assert b" ERROR " in display, repr(bytes(transcript[-6000:]))
        assert b"recovery required" in transcript
        assert b"External changes are not accepted" in transcript
        assert b"including unsaved" in display, repr(bytes(transcript[-6000:]))
    elif mode in ("diagnostic", "diagnostic_mouse"):
        assert capture["execution"]["outcome"] == {"kind":"exited", "code":101}
        diagnostics = capture["diagnostics"]
        assert diagnostics["outcome"] == "compiler_errors"
        assert diagnostics["issues"] == []
        assert len(diagnostics["diagnostics"]) == 5
        assert diagnostics["identity"]["command_id"] == capture["start"]["command_id"]
        assert diagnostics["identity"]["argv"] == capture["start"]["argv"]
        assert diagnostics_seen_at and diagnostic_opened_at and diagnostic_keys_sent_at
        assert diagnostic_picker_opened_at and diagnostic_picker_closed_at
        print("RECORDED_EVENT_PARITY=" + recorded_event_parity())
    elif mode in ("format", "format_mouse", "format_rejected"):
        assert capture["execution"]["outcome"] == {"kind":"exited", "code":0}
        results = list((work / ".rustrace").glob("command-*-format-result.json"))
        assert len(results) == 1
        assert results[0].stat().st_size <= 8192, "format decision receipt exceeded its UI evidence bound"
        result = json.loads(results[0].read_bytes())
        expected_decision = "rejected" if mode == "format_rejected" else "no_change"
        assert result["decision"] == expected_decision, result
        assert result["changed_documents"] == 0
        if mode == "format_rejected":
            assert result["returned_workspace_hash"] is not None
            assert "rejected before publication" in result["detail"]
        assert len(list((work / ".rustrace").glob("command-*-format-before.bin"))) == 1
        assert len(list((work / ".rustrace").glob("command-*-format-returned.bin"))) == 1
        if mode in ("format", "format_mouse"):
            print("RECORDED_EVENT_PARITY=" + recorded_event_parity())
    elif mode in ("save_check", "manual_check"):
        assert capture["execution"]["outcome"] == {"kind":"exited", "code":0}
        assert capture["start"]["action"] == "check"
        if mode == "save_check":
            assert save_toast_seen
        if mode == "manual_check":
            assert manual_check_sent, "manual Check was not dispatched after autosave"
        print("RECORDED_EVENT_PARITY=" + recorded_event_parity())
    elif mode == "huge":
        assert capture["execution"]["outcome"]["reason"] == "output_limit"
        assert capture["execution"]["stdout"]["completeness"] == "truncated"
        assert capture["execution"]["stderr"]["completeness"] == "truncated"
    elif mode == "missing":
        assert capture["execution"]["outcome"] == {"kind":"exited", "code":0}
        assert capture["execution"]["stdout"]["completeness"] == "complete"
        assert capture["execution"]["stderr"]["completeness"] == "complete"
    elif mode == "read_failed":
        assert capture["execution"]["outcome"]["reason"] == "capture_failure"
        assert capture["execution"]["stdout"]["completeness"] == "read_failed"
        assert capture["execution"]["stderr"]["completeness"] == "read_failed"
        escaped_pid = int((work / "target/escaped-pid").read_text())
        escaped_until = time.monotonic() + 5
        while time.monotonic() < escaped_until:
            try:
                os.kill(escaped_pid, 0)
            except ProcessLookupError:
                break
            time.sleep(.05)
        else:
            raise AssertionError("escaped read-failure fixture did not exit")
    elif mode != "picker_paste":
        expected_reason = "quit" if mode == "quit" else "cancelled"
        assert capture["execution"]["outcome"]["reason"] == expected_reason
        if mode == "active_paste":
            for artifact in (work / ".rustrace").iterdir():
                if artifact.is_file() and artifact.stat().st_size <= 34 * 1024 * 1024:
                    assert b"ACTIVE_PASTE_MUST_NOT_PERSIST" not in artifact.read_bytes(), artifact
    success = True
    print("80x24 controlled command", mode, "routing, evidence and terminal cleanup passed")
finally:
    (root / "transcript.bin").write_bytes(transcript)
    try:
        (root / "process-before-cleanup.json").write_text(json.dumps(process_identity()))
        if proc.poll() is None:
            os.write(master, b"\x11")
            cleanup_until = time.monotonic() + 3
            # Keep draining the PTY while waiting for terminal restoration.
            while proc.poll() is None and time.monotonic() < cleanup_until:
                if select.select([master], [], [], .05)[0]:
                    chunk = os.read(master, 65536)
                    transcript.extend(chunk[:max(0, 2 * 1024 * 1024 - len(transcript))])
            if proc.poll() is None:
                print("cleanup incomplete; no process signal attempted:", process_identity(), file=sys.stderr)
        (root / "process-after-cleanup.json").write_text(json.dumps(process_identity()))
    finally:
        (root / "transcript.bin").write_bytes(transcript)
        os.close(master)
        os.close(slave)
    if success:
        if retained_root:
            print("fixture preserved:", root, file=sys.stderr)
        else:
            shutil.rmtree(root)
    else:
        print("FAILED fixture preserved:", root, file=sys.stderr)
