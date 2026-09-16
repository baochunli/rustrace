"""R1/R2 actual work/PTY driver. Retain every fixture, transcript and first failure."""
# RESUMED LIGHT PREP / UNEXECUTED: no fixture has run and no Red is claimed.
import errno
import fcntl
import hashlib
import io
import json
import os
import pathlib
import pty
import select
import signal
import struct
import subprocess
import sys
import tarfile
import termios
import time
import unicodedata

from test_home import isolate
isolate()

binary, mode, evidence_arg = sys.argv[1:]
assert mode in (
    "open", "edit", "reload_gate", "requests", "crash", "mutate", "invalid",
    "policy", "missing", "unsupported", "malformed", "flood", "partial",
    "oversized", "init_blocked", "resolve_blocked", "blocked_stdin", "crash_loop",
    "quit_command", "blocked", "descendant", "mutate_resolution", "mutate_reload",
    "mutate_shutdown", "mutate_exit", "command_barrier", "command_quiescence",
    "command_failed", "command_cancelled", "completion", "completion_mouse",
    "completion_mouse_control", "completion_paste_rejection",
    "completion_navigation", "completion_automatic", "completion_automatic_control",
    "completion_automatic_keyboard_cancel", "completion_automatic_mouse_cancel",
    "completion_automatic_console_cancel",
    "completion_trigger_mouse_cancel", "completion_trigger_keyboard_cancel",
    "completion_console_retirement", "completion_test_cases_retirement",
    "completion_silence_missing", "completion_silence_crash",
    "completion_silence_timeout", "completion_silence_empty",
    "diagnostics", "diagnostics_malformed", "diagnostics_rust_only",
)
evidence = pathlib.Path(evidence_arg).resolve()
tools = evidence / "tools"
tools.mkdir()
(tools / "mode.txt").write_text(mode)
source = pathlib.Path(__file__).with_name("lsp_fake_tool.py")
for name in ("rustup", "rustc", "cargo", "rustdoc", "rust-analyzer"):
    if mode in ("missing", "completion_silence_missing") and name == "rust-analyzer":
        continue
    executable = tools / name
    executable.write_bytes(("#!" + sys.executable + "\n").encode() + source.read_bytes())
    executable.chmod(0o700)

initial = {"main.rs": '// naïve 東京 🦀\r\nfn main() {}\r\n',
           "other.rs": '// deuxième\r\npub fn other() {}\r\n'}
if mode == "blocked_stdin":
    initial["main.rs"] = "//" + "\\" * (1024 * 1024 - 4) + "\r\n"
elif mode == "command_barrier":
    initial["main.rs"] = "//" + "\\" * (200 * 1024 - 4) + "\r\n"
initial["Cargo.toml"] = '[package]\nname = "student"\nversion = "0.1.0"\n[workspace]\n'
manifest = b'''format_version = 1
course_id = "course"
assignment_id = "lsp-lifecycle"
assignment_version = "v1"
title = "LSP lifecycle fixture"
toolchain = "pinned"
edition = "2024"
allowed_paths = ["*.rs", "Cargo.toml"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
'''
if mode == "edit":
    manifest = manifest.replace(
        b'allowed_paths = ["*.rs", "Cargo.toml"]',
        b'allowed_paths = ["*.rs", "Cargo.toml", "src/**/*.rs"]',
    )
case_entries = []
if mode == "completion_test_cases_retirement":
    manifest = manifest.replace(b"format_version = 1", b"format_version = 2")
    case_entries = [
        ("test-cases/modal.in", b"modal input\n"),
        ("test-cases/modal.expected", b"modal output\n"),
    ]
archive = evidence / "assignment.rta"
stream = io.BytesIO()
with tarfile.open(fileobj=stream, mode="w", format=tarfile.USTAR_FORMAT) as package:
    for name, data in [("assignment.toml", manifest)] + [
            ("starter/" + name, text.encode()) for name, text in initial.items()] + case_entries:
        info = tarfile.TarInfo(name)
        info.size, info.mode = len(data), 0o600
        package.addfile(info, io.BytesIO(data))
data = stream.getvalue()
while data.endswith(bytes(512)):
    data = data[:-512]
archive.write_bytes(data + bytes(1024))
(evidence / "initial.json").write_text(json.dumps(initial))
(evidence / "identities.json").write_text(json.dumps({
    "binary": str(pathlib.Path(binary).resolve()), "mode": mode,
    "python": sys.executable,
    "sha256": {str(p): hashlib.sha256(p.read_bytes()).hexdigest()
               for p in [pathlib.Path(binary), pathlib.Path(__file__), source, archive]},
}, indent=2))

workspace = evidence / "assignment.work"
master, slave = pty.openpty()
rows, columns = ((24, 80)
                 if mode in ("open", "completion", "completion_mouse",
                             "completion_mouse_control", "completion_paste_rejection",
                             "completion_navigation", "completion_console_retirement",
                             "completion_test_cases_retirement", "completion_automatic",
                             "completion_automatic_control", "completion_automatic_keyboard_cancel",
                             "completion_automatic_mouse_cancel", "completion_automatic_console_cancel",
                             "completion_trigger_mouse_cancel",
                             "completion_trigger_keyboard_cancel", "completion_silence_missing",
                             "completion_silence_crash", "completion_silence_timeout",
                             "completion_silence_empty", "diagnostics",
                             "diagnostics_malformed", "diagnostics_rust_only")
                 else (32, 140))
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))


def child_setup():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


# Keep an outer process on the PTY to verify normal terminal restoration.
wrapper = (
    "import subprocess,termios,sys; before=termios.tcgetattr(0); "
    "result=subprocess.run(sys.argv[1:]); "
    "assert termios.tcgetattr(0)==before, 'terminal mode leaked'; "
    "print('TERMINAL_RESTORED',flush=True); sys.exit(result.returncode)"
)
env = {**os.environ, "PATH": str(tools), "TERM": "xterm-256color",
       "RUSTUP_AUTO_INSTALL": "0", "RUST_ANALYZER": "/forbidden/analyzer",
       "RUSTFLAGS": "--forbidden", "RUSTDOCFLAGS": "--forbidden",
       "RA_LOG": "forbidden=trace", "RUSTUP_LOG": "forbidden=trace",
       "CARGO_BUILD_RUSTC_WRAPPER": "/forbidden/wrapper",
       "RUSTRACE_SECRET_SENTINEL": "must-not-cross-policy"}
proc = subprocess.Popen([sys.executable, "-c", wrapper, binary, "work", str(archive)],
                        stdin=slave, stdout=slave, stderr=slave, env=env,
                        cwd=evidence, preexec_fn=child_setup)
output = bytearray()
deadline = time.monotonic() + (15 if mode.startswith("diagnostics") else 60)


def drain():
    if select.select([master], [], [], .02)[0]:
        try:
            output.extend(os.read(master, 65536))
        except OSError as error:
            if error.errno != errno.EIO:
                raise
    assert len(output) <= 2 * 1024 * 1024, "PTY output exceeded fixture cap"


def wait_for(predicate, label):
    while not predicate():
        assert proc.poll() is None, "work exited before " + label
        assert time.monotonic() < deadline, "fixture deadline waiting for " + label
        drain()


def screen_text():
    """Replay the narrow CSI subset emitted by ratatui into the fixed PTY."""
    screen = [[" "] * columns for _ in range(rows)]
    row = column = 0
    index = 0
    data = bytes(output)
    while index < len(data):
        byte = data[index]
        if byte == 0x1b and index + 1 < len(data) and data[index + 1] == ord("["):
            end = index + 2
            while end < len(data) and not 0x40 <= data[end] <= 0x7e:
                end += 1
            if end == len(data):
                break
            if chr(data[end]) in ("H", "f"):
                parameters = data[index + 2:end].decode("ascii").split(";")
                row = max(0, (int(parameters[0]) if parameters[0] else 1) - 1)
                column = max(0, (int(parameters[1])
                                 if len(parameters) > 1 and parameters[1] else 1) - 1)
            index = end + 1
            continue
        if byte < 0x20 or byte == 0x7f:
            if byte == ord("\r"):
                column = 0
            elif byte == ord("\n"):
                row = min(rows - 1, row + 1)
            elif byte == ord("\b"):
                column = max(0, column - 1)
            index += 1
            continue
        end = index
        while (end < len(data) and data[end] >= 0x20
               and data[end] not in (0x7f, 0x1b)):
            end += 1
        for character in data[index:end].decode("utf-8", errors="ignore"):
            width = (0 if unicodedata.combining(character) else
                     2 if unicodedata.east_asian_width(character) in "WF" else 1)
            if width and 0 <= row < rows and 0 <= column < columns:
                screen[row][column] = character
                if width == 2 and column + 1 < columns:
                    screen[row][column + 1] = " "
            column += width
        index = end
    return "\n".join("".join(line) for line in screen)


def contents(name, text):
    path = workspace / name
    return path.is_file() and path.read_bytes() == text.encode()


def send(data):
    with (evidence / "input.jsonl").open("a") as log:
        log.write(json.dumps({"bytes_hex": data.hex()}) + "\n")
    os.write(master, data)


def save_expect(name, text):
    wait_for(lambda: contents(name, text), "saved " + name)


def transcript_messages():
    transcript = tools / "frames.jsonl"
    if not transcript.is_file():
        return []
    return [json.loads(line) for line in transcript.read_bytes().splitlines(keepends=True)
            if line.endswith(b"\n")]


def reload_count():
    return sum(message.get("method") == "rust-analyzer/reloadWorkspace"
               for message in transcript_messages())


def server_launch_count():
    launches = tools / "server-launches.jsonl"
    return len(launches.read_text().splitlines()) if launches.is_file() else 0


def assert_no_reload():
    before = reload_count()
    until = time.monotonic() + .25
    while time.monotonic() < until:
        drain()
    assert reload_count() == before, "F8 escaped an inhibited authority state"


def wait_for_command_restart(expected_text):
    marker = workspace / ".rustrace/command-activity.json"
    wait_for(lambda: marker.is_file() and not json.loads(marker.read_text())["active"],
             "durable inactive command marker")

    def latest_generation_ready():
        frames = transcript_messages()
        return (sum(frame.get("method") == "initialize" for frame in frames) >= 2
                and any(
                    frame.get("method") == "textDocument/didOpen"
                    and frame.get("params", {}).get("textDocument", {}).get("text")
                        == expected_text
                    for frame in frames
                ))

    wait_for(latest_generation_ready, "fresh command-boundary document resync")


try:
    wait_for(lambda: b" files" in output, "production editor")
    # Give asynchronous initialization its declared 30-second protocol budget.
    # On the feature-absent base, continue to edit/quit and let the Rust assertion
    # identify behavioral Red; absence of a transcript is not a harness failure.
    short_startup = ("missing", "completion_silence_missing", "unsupported", "malformed", "flood", "partial",
                     "oversized", "init_blocked", "resolve_blocked", "blocked_stdin")
    startup_deadline = time.monotonic() + (30 if mode not in short_startup else 5)
    while time.monotonic() < startup_deadline:
        messages = transcript_messages()
        if mode in short_startup:
            if (mode in ("missing", "completion_silence_missing", "resolve_blocked")
                    or mode == "blocked_stdin" and (tools / "blocked-stdin-ready").is_file()
                    or mode != "blocked_stdin" and messages):
                break
        elif sum(m.get("method") == "textDocument/didOpen" for m in messages) >= 2:
            break
        assert proc.poll() is None, "work exited before initialization observation"
        drain()
    if mode != "diagnostics_rust_only":
        send(b"\x1b[<0;4;3M\x1b[<0;4;3m")
    expected = dict(initial)
    quit_sent = False
    if mode == "diagnostics_rust_only":
        wait_for(lambda: (tools / "non-rust-diagnostic-sent").exists(),
                 "fake Cargo.toml diagnostic")
        send(b"X")
        cargo_text = "X" + initial["Cargo.toml"]
        save_expect("Cargo.toml", cargo_text)
        quiet_until = time.monotonic() + .4
        while time.monotonic() < quiet_until:
            drain()
        cargo_screen = screen_text()
        assert "TOML live problem" not in cargo_screen, \
            "Cargo.toml received a rust-analyzer live hint"
        (tools / "cargo-diagnostic-screen.txt").write_text(cargo_screen)

        send(b"\x1b[17~X")  # F6 selects main.rs, then edits it.
        rust_text = "X" + initial["main.rs"]
        wait_for(lambda: "Rust live problem" in screen_text(),
                 "Rust live diagnostic source rendering")
        rust_screen = screen_text()
        assert "│output · Rust live problem" in rust_screen, rust_screen
        (tools / "rust-diagnostic-screen.txt").write_text(rust_screen)
        save_expect("main.rs", rust_text)
        expected["Cargo.toml"] = cargo_text
        expected["main.rs"] = rust_text
    elif mode == "diagnostics":
        underline_start = len(output)
        send(b"X")
        wait_for(lambda: "live problem" in screen_text(),
                 "live diagnostic source rendering")
        diagnostic_screen = screen_text()
        diagnostic_row = next(line for line in diagnostic_screen.splitlines()
                              if "X// naïve" in line)
        assert "│X// naïve" in diagnostic_row, "live diagnostic shifted the source column"
        assert "live problem" in diagnostic_row, "live inline message was not rendered"
        assert "│output · live problem" in diagnostic_screen, \
            "caret diagnostic was not rendered in the output header"
        assert b"\x1b[4m" in bytes(output[underline_start:]), \
            "diagnostic span was not underlined in the PTY"
        (tools / "diagnostic-screen.txt").write_text(diagnostic_screen)
        send(b"\x7f")
        wait_for(lambda: "live problem" not in screen_text(),
                 "cleared live diagnostic rendering")
        (tools / "diagnostic-cleared-screen.txt").write_text(screen_text())
        save_expect("main.rs", initial["main.rs"])
    elif mode == "diagnostics_malformed":
        send(b"X")
        wait_for(lambda: any(
            message.get("method") == "textDocument/didChange"
            for message in transcript_messages()), "malformed diagnostic trigger")
        quiet_until = time.monotonic() + .4
        while time.monotonic() < quiet_until:
            drain()
        malformed_screen = screen_text()
        assert "malformed live hint" not in malformed_screen
        assert proc.poll() is None, "malformed diagnostic notification crashed work"
        (tools / "malformed-diagnostic-screen.txt").write_text(malformed_screen)
        send(b"\x7f")
        save_expect("main.rs", initial["main.rs"])
    if mode == "open":
        # Keep the 80x24 terminal and exact cursor chrome visible with the new tab.
        send(b"\x1b[<0;26;2M\x1b[<32;18;2M\x1b[<0;18;2m")
        idle_deadline = time.monotonic() + .25
        while time.monotonic() < idle_deadline:
            drain()
        idle_screen = screen_text()
        (tools / "healthy-idle-screen.txt").write_text(idle_screen)
        assert " files" in idle_screen and "Ln 1, Col 1" in idle_screen, \
            "healthy idle screen lacked stable chrome anchors"
        assert not any(counter in idle_screen for counter in (
            "events ", "storage ", "headroom ",
        )), "healthy idle screen exposed persistent journal counters"
        assert not any(state in idle_screen for state in (
            "LSP reso", "LSP init", "LSP read", "LSP retr", "LSP unav",
            "LSP stop", "LSP off",
        )), "healthy idle screen exposed persistent language-service state"
        send(b"\x1b[C")
        wait_for(lambda: "Ln 1, Col 2" in screen_text(), "idle navigation redraw")
        navigated_deadline = time.monotonic() + .25
        while time.monotonic() < navigated_deadline:
            drain()
        navigated_screen = screen_text()
        (tools / "healthy-idle-navigation-screen.txt").write_text(navigated_screen)
        assert " files" in navigated_screen and "Ln 1, Col 2" in navigated_screen, \
            "navigated idle screen lacked stable chrome anchors"
        assert not any(counter in navigated_screen for counter in (
            "events ", "storage ", "headroom ",
        )), "navigation redraw exposed persistent journal counters"
    if mode == "edit":
        # Disk save acknowledgements work on the accepted base without LSP.
        # They avoid an LSP-dependent harness failure masking the intended Red.
        send(b"\x1b[H\x7fX")  # Home/backspace at byte zero is a no-op, then insert.
        save_expect("main.rs", "X" + initial["main.rs"])
        send(b"\x1a")
        save_expect("main.rs", initial["main.rs"])
        send(b"\x19")
        save_expect("main.rs", "X" + initial["main.rs"])
        send(b"\x1b[17~Y")  # F6, next buffer; existing production binding.
        save_expect("other.rs", "Y" + initial["other.rs"])
        send(b"\x1b[15~\x1b[17~")  # F5/F6 focus only.
        send(b"\x1b[<2;4;4M\x1b[<0;5;6M")  # other.rs menu, rename.
        wait_for(lambda: b"rename file" in output, "rename prompt")
        send(b"\x7f" * len("other.rs") + b"renamed.rs\r")
        wait_for(lambda: contents("renamed.rs", "Y" + initial["other.rs"])
                 and not (workspace / "other.rs").exists(), "rename publication")
        send(b"\x17")  # Ctrl-W deletes the saved selected file.
        wait_for(lambda: not (workspace / "renamed.rs").exists(), "delete publication")
        (workspace / "src").mkdir()
        send(b"\x1b[<2;4;1M\x1b[<0;5;5M")  # files header menu, new file.
        wait_for(lambda: b"new file" in output, "create prompt")
        send(b"new.rs\r")
        wait_for(lambda: contents("src/new.rs", ""), "create publication")
        send(b"Z")
        save_expect("src/new.rs", "Z")
        expected = {"main.rs": "X" + initial["main.rs"], "src/new.rs": "Z"}
    elif mode in ("completion_console_retirement", "completion_test_cases_retirement"):
        send(b"\x00")
        wait_for(lambda: (tools / "completion-request-pending").exists(),
                 "held completion request")
        if mode == "completion_console_retirement":
            send(b"\x1b[20~")
            wait_for(lambda: " CONSOLE " in screen_text(), "Console view")
        else:
            send(b"\x1b[14~")
            wait_for(lambda: " TEST CASES " in screen_text(),
                     "Test Cases modal")
        (tools / "completion-transition-screen.txt").write_text(screen_text())
        send(b"\x1b")
        wait_for(lambda: (" CONSOLE " not in screen_text()
                          and " TEST CASES " not in screen_text()),
                 "return to workspace")
        (tools / "release-completion-response").write_text("release")
        wait_for(lambda: (tools / "completion-response-sent").exists(),
                 "delayed completion response")
        response_deadline = time.monotonic() + .6
        while time.monotonic() < response_deadline:
            drain()
        returned_screen = screen_text()
        (tools / "completion-return-screen.txt").write_text(returned_screen)
        (tools / "completion-authority.json").write_text(json.dumps({
            "old_response_retired": "item-0" not in returned_screen
        }))
    elif mode in ("completion_automatic", "completion_automatic_keyboard_cancel",
                  "completion_automatic_mouse_cancel", "completion_automatic_console_cancel"):
        typed = "abcdefghijklmnopqrst"
        send(typed.encode())
        wait_for(lambda: any(
            message.get("method") == "textDocument/didChange"
            and message.get("params", {}).get("contentChanges", [{}])[0].get("text", "")
                .startswith(typed)
            for message in transcript_messages()), "automatic typing synchronization")
        wait_for(lambda: sum(message.get("method") == "textDocument/completion"
                             for message in transcript_messages()) == 1,
                 "one debounced automatic completion request")
        wait_for(lambda: (tools / "automatic-completion-held").is_file(),
                 "held automatic completion response")
        send(b"z")
        wait_for(lambda: any(
            message.get("method") == "textDocument/didChange"
            and message.get("params", {}).get("contentChanges", [{}])[0].get("text", "")
                .startswith(typed + "z")
            for message in transcript_messages()), "deferred typing synchronization")
        held_until = time.monotonic() + .35
        while time.monotonic() < held_until:
            drain()
        assert sum(message.get("method") == "textDocument/completion"
                   for message in transcript_messages()) == 1, \
            "a second automatic request escaped while the first was in flight"
        if mode == "completion_automatic":
            (tools / "release-automatic-completion").write_text("release")
            wait_for(lambda: sum(message.get("method") == "textDocument/completion"
                                 for message in transcript_messages()) == 2,
                     "one deferred automatic completion request")
            wait_for(lambda: "auto-fresh" in screen_text(),
                     "fresh automatic completion popup")
            (tools / "automatic-completion-screen.txt").write_text(screen_text())
            send(b"\t")
            completed = typed + "zAUTO" + initial["main.rs"]
            wait_for(lambda: any(
                message.get("method") == "textDocument/didChange"
                and message.get("params", {}).get("contentChanges", [{}])[0].get("text")
                    == completed
                for message in transcript_messages()), "automatic completion acceptance")
            save_expect("main.rs", completed)
            expected["main.rs"] = completed
        else:
            if mode == "completion_automatic_keyboard_cancel":
                send(b"\x1b[D\x1b[C")
            elif mode == "completion_automatic_mouse_cancel":
                send(b"\x1b[<0;27;2M\x1b[<0;27;2m"
                     b"\x1b[<0;48;2M\x1b[<0;48;2m")
            else:
                send(b"\x1b[20~")
                wait_for(lambda: " CONSOLE " in screen_text(), "Console focus entry")
            navigation_until = time.monotonic() + .15
            while time.monotonic() < navigation_until:
                drain()
            (tools / "release-automatic-completion").write_text("release")
            response_until = time.monotonic() + .65
            while time.monotonic() < response_until:
                drain()
            requests = sum(message.get("method") == "textDocument/completion"
                           for message in transcript_messages())
            assert requests == 1, \
                f"cancelled deferred intent issued a second completion request: {requests}"
            if mode == "completion_automatic_console_cancel":
                send(b"\x1b")
                wait_for(lambda: " CONSOLE " not in screen_text(), "return from Console")
                returned_until = time.monotonic() + .65
                while time.monotonic() < returned_until:
                    drain()
            cancelled_screen = screen_text()
            (tools / "automatic-completion-cancelled-screen.txt").write_text(cancelled_screen)
            assert "auto-fresh" not in cancelled_screen
            assert "auto-stale" not in cancelled_screen
            assert " COMPLETE " not in cancelled_screen
            completed = typed + "z" + initial["main.rs"]
            save_expect("main.rs", completed)
            expected["main.rs"] = completed
    elif mode == "completion_automatic_control":
        typed = "abcdefghijklmnopqrst"
        send(typed.encode() + b"\x00")
        wait_for(lambda: sum(message.get("method") == "textDocument/completion"
                             for message in transcript_messages()) == 1,
                 "first Ctrl-Space completion request")
        wait_for(lambda: (tools / "automatic-completion-held").is_file(),
                 "held Ctrl-Space completion response")
        send(b"z\x00")
        wait_for(lambda: sum(message.get("method") == "textDocument/completion"
                             for message in transcript_messages()) == 2,
                 "second Ctrl-Space completion request")
        (tools / "release-automatic-completion").write_text("release")
        wait_for(lambda: "auto-fresh" in screen_text(),
                 "fresh Ctrl-Space completion popup")
        send(b"\t")
        completed = typed + "zAUTO" + initial["main.rs"]
        wait_for(lambda: any(
            message.get("method") == "textDocument/didChange"
            and message.get("params", {}).get("contentChanges", [{}])[0].get("text")
                == completed
            for message in transcript_messages()), "Ctrl-Space completion acceptance")
        save_expect("main.rs", completed)
        expected["main.rs"] = completed
    elif mode in ("completion_trigger_mouse_cancel",
                  "completion_trigger_keyboard_cancel"):
        navigation = (b"\x1b[<0;27;2M\x1b[<0;27;2m"
                      if mode == "completion_trigger_mouse_cancel"
                      else b"\x1b[D")
        send(b"x" + navigation)
        changed = "x" + initial["main.rs"]
        wait_for(lambda: any(
            message.get("method") == "textDocument/didChange"
            and message.get("params", {}).get("contentChanges", [{}])[0].get("text")
                == changed
            for message in transcript_messages()), "typing trigger synchronization")
        after_delay = time.monotonic() + .65
        while time.monotonic() < after_delay:
            drain()
        save_expect("main.rs", changed)
        expected["main.rs"] = changed
    elif mode.startswith("completion_silence_"):
        send(b"x")
        changed = "x" + initial["main.rs"]
        if mode != "completion_silence_missing":
            wait_for(lambda: any(
                message.get("method") == "textDocument/didChange"
                and message.get("params", {}).get("contentChanges", [{}])[0].get("text")
                    == changed
                for message in transcript_messages()), "degradation trigger synchronization")
            wait_for(lambda: sum(
                message.get("method") == "textDocument/completion"
                for message in transcript_messages()) == 1,
                "degraded automatic completion request")
        if mode == "completion_silence_crash":
            wait_for(lambda: server_launch_count() >= 2,
                     "automatic completion crash restart")
            wait_for(lambda: sum(
                message.get("method") == "textDocument/didOpen"
                for message in transcript_messages()) >= 4,
                "automatic completion crash document resync")
        elif mode == "completion_silence_timeout":
            timeout_deadline = time.monotonic() + 3.25
            while time.monotonic() < timeout_deadline:
                drain()
        else:
            quiet_deadline = time.monotonic() + .35
            while time.monotonic() < quiet_deadline:
                drain()
        automatic_screen = screen_text()
        (tools / "automatic-silence-screen.txt").write_text(automatic_screen)
        assert not any(text in automatic_screen for text in (
            "completion requested", "action failed", " COMPLETE ",
        )), "automatic degradation exposed completion UI"

        send(b"\x00")
        control_message = ("completion unavailable"
                           if mode == "completion_silence_missing"
                           else "completion requested")
        control_title = ("action failed"
                         if mode == "completion_silence_missing" else "notice")
        wait_for(lambda: (control_message in screen_text()
                          and control_title in screen_text()),
                 "explicit Ctrl-Space degradation message")
        if mode != "completion_silence_missing":
            wait_for(lambda: sum(
                message.get("method") == "textDocument/completion"
                for message in transcript_messages()) == 2,
                "explicit Ctrl-Space degraded request")
        manual_screen = screen_text()
        (tools / "manual-completion-screen.txt").write_text(manual_screen)
        save_expect("main.rs", changed)
        expected["main.rs"] = changed
    elif mode in ("completion", "completion_mouse", "completion_mouse_control",
                  "completion_paste_rejection", "completion_navigation"):
        if mode in ("completion", "completion_paste_rejection"):
            send(b" ")
            wait_for(lambda: any(
                message.get("method") == "textDocument/didChange"
                and message.get("params", {}).get("contentChanges", [{}])[0].get("text", "")
                    .startswith(" ")
                for message in transcript_messages()), "non-trigger edit synchronization")
        before = sum(message.get("method") == "textDocument/completion"
                     for message in transcript_messages())
        time.sleep(.2)
        assert before == 0, "non-trigger input requested completion"
        (tools / "completion-before-trigger.json").write_text(json.dumps({"requests": before}))
        if mode in ("completion", "completion_paste_rejection"):
            send(b"\x1a")
            wait_for(lambda: any(
                message.get("method") == "textDocument/didChange"
                and message.get("params", {}).get("contentChanges", [{}])[0].get("text")
                    == initial["main.rs"]
                for message in transcript_messages()), "pre-completion undo synchronization")
        send(b"\x00")
        wait_for(lambda: sum(message.get("method") == "textDocument/completion"
                             for message in transcript_messages()) == 1,
                 "manual completion request")
        if mode == "completion_navigation":
            wait_for(lambda: (
                " COMPLETE " in screen_text()
                and all("item-" + str(index) in screen_text()
                        for index in range(4))),
                "initial completion popup")
        else:
            wait_for(lambda: "malicious\\u{1b}[2J\\nlabel" in screen_text(),
                     "safe bounded completion list")
        (tools / "completion-screen.txt").write_text(screen_text())
        if mode in ("completion", "completion_mouse", "completion_mouse_control"):
            if mode == "completion_mouse":
                target = "malicious\\u{1b}[2J\\nlabel"
                lines = screen_text().splitlines()
                target_row = next(index for index, line in enumerate(lines)
                                  if target in line)
                target_column = lines[target_row].index(target)
                send(f"\x1b[<0;{target_column + 1};{target_row + 1}M".encode())
            else:
                send(b"\r")
            completed = "\u03bb" + initial["main.rs"]
            wait_for(lambda: any(
                message.get("method") == "textDocument/didChange"
                and message.get("params", {}).get("contentChanges", [{}])[0].get("text")
                    == completed
                for message in transcript_messages()), "accepted completion synchronization")
            if mode == "completion":
                send(b"\x1a")
                wait_for(lambda: sum(
                    message.get("method") == "textDocument/didChange"
                    and message.get("params", {}).get("contentChanges", [{}])[0].get("text")
                        == initial["main.rs"]
                    for message in transcript_messages()) >= 2, "completion undo")
                send(b"\x19")
                wait_for(lambda: sum(
                    message.get("method") == "textDocument/didChange"
                    and message.get("params", {}).get("contentChanges", [{}])[0].get("text")
                        == completed
                    for message in transcript_messages()) >= 2, "completion redo")
            save_expect("main.rs", completed)
            expected["main.rs"] = completed
        elif mode == "completion_paste_rejection":
            send(b"\x1b[200~REJECTED_COMPLETION_PASTE\x1b[201~")
            wait_for(lambda: "Paste blocked:" in screen_text(),
                     "bracketed paste rejection")
            rejected_screen = screen_text()
            assert "malicious\\u{1b}[2J\\nlabel" not in rejected_screen
            (tools / "completion-paste-rejected-screen.txt").write_text(rejected_screen)
            changes_before_enter = sum(
                message.get("method") == "textDocument/didChange"
                for message in transcript_messages())
            send(b"\r")
            wait_for(lambda: sum(
                message.get("method") == "textDocument/didChange"
                for message in transcript_messages()) > changes_before_enter,
                "post-rejection Enter routing")
            changes_before_undo = sum(
                message.get("method") == "textDocument/didChange"
                for message in transcript_messages())
            send(b"\x1a")
            wait_for(lambda: (
                sum(message.get("method") == "textDocument/didChange"
                    for message in transcript_messages()) > changes_before_undo
                and [message for message in transcript_messages()
                     if message.get("method") == "textDocument/didChange"][-1]
                    .get("params", {}).get("contentChanges", [{}])[0].get("text")
                        == initial["main.rs"]),
                "post-rejection Enter undo")
            save_expect("main.rs", initial["main.rs"])
        else:
            send(b"\x1b[B")
            until = time.monotonic() + .15
            while time.monotonic() < until:
                drain()
            send(b"\x1b[B")
            until = time.monotonic() + .15
            while time.monotonic() < until:
                drain()
            third_screen = screen_text()
            assert " COMPLETE " in third_screen
            assert all("item-" + str(index) in third_screen for index in range(4)), \
                "the popup must keep every bounded navigation candidate visible"
            (tools / "completion-third-screen.txt").write_text(third_screen)

            send(b"\x1b[B")
            until = time.monotonic() + .15
            while time.monotonic() < until:
                drain()
            last_screen = screen_text()
            assert " COMPLETE " in last_screen
            assert all("item-" + str(index) in last_screen for index in range(4)), \
                "the popup must keep the accepted last candidate visible"
            (tools / "completion-last-screen.txt").write_text(last_screen)

            send(b"\r")
            completed = "D" + initial["main.rs"]
            wait_for(lambda: any(
                message.get("method") == "textDocument/didChange"
                and message.get("params", {}).get("contentChanges", [{}])[0].get("text")
                    == completed
                for message in transcript_messages()), "visible completion acceptance")
            send(b"\x1a")
            wait_for(lambda: any(
                message.get("method") == "textDocument/didChange"
                and message.get("params", {}).get("contentChanges", [{}])[0].get("text")
                    == initial["main.rs"]
                for message in transcript_messages()), "visible completion undo")
            send(b"\x19")
            wait_for(lambda: sum(
                message.get("method") == "textDocument/didChange"
                and message.get("params", {}).get("contentChanges", [{}])[0].get("text")
                    == completed
                for message in transcript_messages()) >= 2,
                "visible completion redo")
            save_expect("main.rs", completed)
            expected["main.rs"] = completed
    elif mode == "reload_gate":
        send(b"\x06")  # Ctrl-F find panel.
        wait_for(lambda: "find and replace" in screen_text(), "active find panel")
        send(b"\x1b[19~")
        assert_no_reload()
        send(b"\x1b")
        wait_for(lambda: "find and replace" not in screen_text(), "closed find panel")
        send(b"\x1b[<2;4;1M\x1b[<0;5;5M")  # files header menu, new file.
        wait_for(lambda: "new file" in screen_text(), "active create prompt")
        send(b"\x1b[19~")
        assert_no_reload()
        send(b"\x1b")
        wait_for(lambda: "file operation cancelled" in screen_text(), "closed create prompt")
        send(b"\x1b[18~")  # F7 command picker.
        wait_for(lambda: " MENU " in screen_text() and "Check" in screen_text(),
                 "active command picker")
        send(b"\x1b[19~")
        assert_no_reload()
        send(b"\x1b")
        wait_for(lambda: "command selection closed" in screen_text(),
                 "closed command picker")
        send(b"!")
        save_expect("main.rs", "!" + initial["main.rs"])
        send(b"\x1a")
        wait_for(lambda: any(
            message.get("method") == "textDocument/didChange"
            and message.get("params", {}).get("contentChanges", [{}])[0].get("text")
                == initial["main.rs"]
            for message in transcript_messages()), "autosave timer reset undo")
        send(b"X")
        wait_for(lambda: "main.rs*" in screen_text(), "dirty active file")
        send(b"\x17")  # Workspace-owned destructive delete confirmation.
        wait_for(lambda: "dirty file: Y/Enter deletes, N/Esc cancels" in screen_text(),
                 "active destructive confirmation")
        send(b"\x1b[19~")
        assert_no_reload()
        send(b"n")
        wait_for(lambda: "file operation cancelled" in screen_text(),
                 "cancelled destructive confirmation")
        save_expect("main.rs", "X" + initial["main.rs"])
        expected["main.rs"] = "X" + initial["main.rs"]
        send(b"\x1b[19~")
        wait_for(lambda: reload_count() == 1, "permitted reload")
    elif mode == "requests":
        wait_for(lambda: (tools / "requests-complete").is_file(), "safe server requests")
    elif mode == "crash":
        send(b"X")
        save_expect("main.rs", "X" + initial["main.rs"])
        send(b"\x1b[17~Y")
        save_expect("other.rs", "Y" + initial["other.rs"])
        expected = {"main.rs": "X" + initial["main.rs"],
                    "other.rs": "Y" + initial["other.rs"]}
        wait_for(lambda: sum(m.get("method") == "initialize"
                             for m in transcript_messages()) >= 2, "crash restart")
        wait_for(lambda: any(m.get("method") == "textDocument/didOpen"
                             and m["params"]["textDocument"]["text"] == expected["other.rs"]
                             for m in transcript_messages()), "latest resync")
    elif mode == "crash_loop":
        launch_log = tools / "server-launches.jsonl"
        launch_count = lambda: (len(launch_log.read_text().splitlines())
                                if launch_log.is_file() else 0)
        wait_for(lambda: launch_count() == 6, "bounded crash retries")
        until = time.monotonic() + 1
        while time.monotonic() < until:
            drain()
        assert launch_count() == 6, "automatic crash retries exceeded the cap"
    elif mode == "command_quiescence":
        send(b"X")
        save_expect("main.rs", "X" + initial["main.rs"])
        expected["main.rs"] = "X" + initial["main.rs"]
        send(b"\x1b[18~\r")
        wait_for(lambda: (tools / "command-observed.bin").is_file(),
                 "controlled command source observation")
        wait_for_command_restart(expected["main.rs"])
    elif mode in ("command_failed", "command_cancelled"):
        send(b"X")
        save_expect("main.rs", "X" + initial["main.rs"])
        expected["main.rs"] = "X" + initial["main.rs"]
        send(b"\x1b[18~\r")
        wait_for(lambda: (tools / "command-resolution-probe").is_file(),
                 "active command resolver probe")
        if mode == "command_cancelled":
            send(b"\x1b")
        wait_for_command_restart(expected["main.rs"])
    elif mode in ("quit_command", "command_barrier"):
        if mode == "command_barrier":
            wait_for(lambda: (tools / "command-barrier-ready").is_file(),
                     "server-side command barrier stall")
            send(b"X")
            save_expect("main.rs", "X" + initial["main.rs"])
            expected["main.rs"] = "X" + initial["main.rs"]
        send(b"\x1b[18~\r")
        if mode == "command_barrier":
            time.sleep(.25)
            (tools / "release-command-barrier").write_text("resume stdin")
        wait_for(lambda: (tools / "command-running").is_file(),
                 "active controlled command")
        marker = workspace / ".rustrace/command-activity.json"
        wait_for(lambda: marker.is_file() and json.loads(marker.read_text())["active"],
                 "durable active command marker")
        send(b"\x11")
        quit_sent = True
    elif mode in ("mutate", "invalid"):
        wait_for(lambda: (tools / "mutation-complete").is_file(), "server mutation")
        if mode == "mutate":
            wait_for(lambda: contents("main.rs", initial["main.rs"])
                     and list((workspace / ".rustrace").glob("evidence-*.bin")),
                     "P2 restoration")
        else:
            wait_for(lambda: list((workspace / ".rustrace").glob("evidence-*.bin")),
                     "invalid-authority preservation")
    elif mode in ("missing", "unsupported", "malformed", "flood", "partial",
                  "oversized", "init_blocked", "resolve_blocked", "blocked_stdin"):
        if mode != "blocked_stdin":
            send(b"X")
            save_expect("main.rs", "X" + initial["main.rs"])
            expected["main.rs"] = "X" + initial["main.rs"]
    elif mode == "mutate_reload":
        send(b"\x1b[19~")
        wait_for(lambda: (tools / "mutation-complete").is_file(), "reload mutation")
    if not quit_sent:
        send(b"\x11")
    while proc.poll() is None:
        assert time.monotonic() < deadline, "work did not quit"
        drain()
    # Drain the outer wrapper's final restoration marker after process exit.
    while select.select([master], [], [], .05)[0]:
        before = len(output)
        drain()
        if len(output) == before:
            break
    assert proc.returncode == 0, "work or terminal restoration failed"
    assert b"TERMINAL_RESTORED" in output
    assert b"\x1b[?1049l" in output and b"\x1b[?2004l" in output
    expected.setdefault("Cargo.toml", initial["Cargo.toml"])
    (evidence / "expected.json").write_text(json.dumps(expected))
finally:
    (evidence / "pty.bin").write_bytes(output)
    if proc.poll() is None:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()
    os.close(master)
    os.close(slave)
