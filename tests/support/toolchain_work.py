"""Actual work CLI: fake installed tools, immutable startup evidence and PTY UI."""
import fcntl
import io
import json
import os
import pathlib
import pty
import re
import select
import signal
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
assignment_id = "tools"
assignment_version = "v1"
title = "Toolchain assignment"
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


def package(path):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, data in [("assignment.toml", manifest), ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'), ("starter/main.rs", b"A")]:
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = 0o600
            archive.addfile(info, io.BytesIO(data))
    data = stream.getvalue()
    while data.endswith(bytes(512)):
        data = data[:-512]
    path.write_bytes(data + bytes(1024))


def executable(path, body):
    path.write_text("#!" + sys.executable + "\n" + body)
    path.chmod(0o700)


def rendered_screen(data):
    screen = [[" "] * 120 for _ in range(32)]
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
                row = min(31, row + amount)
            elif code == "C":
                column = min(119, column + amount)
            elif code == "D":
                column = max(0, column - amount)
            elif code == "J" and values[0] == 2:
                screen = [[" "] * 120 for _ in range(32)]
            elif code == "K" and 0 <= row < 32:
                screen[row][column:] = [" "] * (120 - column)
            continue
        for character in part:
            if character == "\r":
                column = 0
            elif character == "\n":
                row = min(31, row + 1)
            elif character >= " ":
                if 0 <= row < 32 and 0 <= column < 120:
                    screen[row][column] = character
                column += 1
    return "\n".join("".join(line) for line in screen)


def run_pty(archive, env, resume=False):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))

    def child():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    wrapper = "import subprocess,termios,sys; b=termios.tcgetattr(0); r=subprocess.run(sys.argv[1:]); assert termios.tcgetattr(0)==b, 'terminal mode leaked'; print('TERMINAL_RESTORED',flush=True); sys.exit(r.returncode)"
    args = [sys.executable, "-c", wrapper, binary, "work", str(archive)] + (["--resume"] if resume else [])
    proc = subprocess.Popen(args, stdin=slave, stdout=slave, stderr=slave,
                            preexec_fn=child, env=env, cwd=archive.parent)
    output = bytearray()
    typed_at = None
    quit_sent = False
    deadline = time.monotonic() + 20
    try:
        while proc.poll() is None and time.monotonic() < deadline:
            if select.select([master], [], [], .05)[0]:
                output.extend(os.read(master, 65536))
            if typed_at is None and b" files" in output:
                os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3mB")
                typed_at = time.monotonic()
            if typed_at and not quit_sent and time.monotonic() - typed_at > .25:
                os.write(master, b"\x11")
                quit_sent = True
            assert len(output) < 2 * 1024 * 1024
        assert proc.poll() is not None, repr(output)
        while select.select([master], [], [], .05)[0]:
            chunk = os.read(master, 65536)
            if not chunk:
                break
            output.extend(chunk)
        assert proc.returncode == 0 and quit_sent, repr(output)
        assert b"TERMINAL_RESTORED" in output, "terminal mode leaked"
        assert b"\x1b[?1049l" in output and b"\x1b[?2004l" in output
        # Reconstruct only the alternate-screen lifetime so style escapes do
        # not invent whitespace before the flush-left output label.
        rendered = bytes(output).split(b"\x1b[?1049h", 1)[1].split(b"\x1b[?1049l", 1)[0]
        screen = rendered_screen(rendered)
        assert " files" in screen and "│output" in screen, repr(screen)
        assert "Tools:" not in screen, repr(screen)
        assert b"Toolchain: pinned" in output, repr(output)
        assert b"cargo 1.97.2" in output, repr(output)
        return bytes(output)
    finally:
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
        os.close(master)
        os.close(slave)


with tempfile.TemporaryDirectory(prefix="rustrace-toolchain-work-") as temporary:
    root = pathlib.Path(temporary)
    tools = root / "tools"
    tools.mkdir()
    archive = root / "assignment.rta"
    package(archive)
    env = {**os.environ, "PATH": str(tools), "RUSTUP_TOOLCHAIN": "other",
           "RUSTUP_AUTO_INSTALL": "1", "TERM": "xterm-256color"}
    missing = subprocess.run([binary, "work", str(archive)], env=env,
                             capture_output=True, timeout=15)
    assert missing.returncode != 0 and b"rustup" in missing.stdout, missing.stdout
    assert b"rustup.rs" in missing.stdout, missing.stdout
    assert b"\x1b[?1049h" not in missing.stdout
    state = root / "assignment.work" / ".rustrace"
    original = (state / "session.json").read_bytes()
    recorded = sorted(state.glob("toolchain-*.json"))
    assert len(recorded) == 1, recorded
    first = recorded[0].read_bytes()
    first_report = json.loads(first)["report"]
    assert first_report["selected_toolchain"] is None
    assert first_report["probes"][0]["status"] == "not_found"
    inspected = subprocess.run([binary, "work", str(archive), "--inspect"],
                               env=env, capture_output=True, timeout=15)
    assert inspected.returncode == 0, inspected.stdout
    assert recorded[0].read_bytes() == first

    executable(tools / "rustup", '''import json, os, pathlib, sys
root = pathlib.Path(__file__).parent
args = sys.argv[1:]
with (root / "calls.jsonl").open("a") as log:
    log.write(json.dumps([args, os.environ.get("RUSTUP_TOOLCHAIN"), os.environ.get("RUSTUP_AUTO_INSTALL")]) + "\\n")
assert os.environ["RUSTUP_AUTO_INSTALL"] == "0"
if args == ["--version"]:
    print("rustup 1.28.2 (actual)")
elif args == ["toolchain", "list"]:
    print("other (default)\\npinned")
elif args == ["show", "active-toolchain"]:
    assert os.environ["RUSTUP_TOOLCHAIN"] == "pinned", "assignment pin lost"
    print("pinned (environment override)")
elif args[:2] == ["which", "--toolchain"]:
    assert args[2] == "pinned"
    path = root / args[3]
    if not path.is_file():
        print("component not installed", file=sys.stderr)
        sys.exit(1)
    print(path)
elif args[:2] == ["run", "pinned"]:
    path = pathlib.Path(args[2])
    assert path.is_absolute() and path.parent == root, "unresolved PATH fallback"
    os.environ["RUSTUP_TOOLCHAIN"] = "pinned"
    os.execv(str(path), args[2:])
else:
    raise AssertionError("forbidden command: " + repr(args))
''')
    executable(tools / "rustc", 'print("rustc 1.98.1 (first)\\nrelease: 1.98.1\\nhost: test-host")\n')
    executable(tools / "cargo", 'print("cargo 1.97.2 (actual)")\n')
    # All optional components are absent. Recording/editing must still work.
    first_ui = run_pty(archive, env, resume=True)
    assert b"completion unavailable" in first_ui
    before_resume = {p.name: p.read_bytes() for p in state.glob("toolchain-*.json")}
    executable(tools / "rustc", 'print("rustc 1.98.1 (changed)\\nrelease: 1.98.1\\nhost: test-host")\n')
    run_pty(archive, env, resume=True)
    assert (state / "session.json").read_bytes() == original
    for name, data in before_resume.items():
        assert (state / name).read_bytes() == data
    records = [json.loads(p.read_bytes()) for p in sorted(state.glob("toolchain-*.json"))]
    assert len(records) == 3
    metadata = json.loads(original)
    for record in records:
        assert record["session_id"] == metadata["session_id"]
        assert record["manifest_hash"] == metadata["manifest_hash"]
        assert record["sequence"] >= 1 and record["event_hash"]
    assert records[1]["sequence"] < records[2]["sequence"]
    versions = [[p["stdout"] for p in r["report"]["probes"]
                 if p["component"] == "rustc" and p["purpose"] == "version"] for r in records]
    assert "first" in versions[1][0] and "changed" in versions[2][0], versions
    calls = [json.loads(line) for line in (tools / "calls.jsonl").read_text().splitlines()]
    assert all(auto == "0" for _, _, auto in calls)
    assert all("--install" not in args for args, _, _ in calls)
    assert any(args == ["run", "pinned", str(tools / "cargo"), "-V"] for args, _, _ in calls)
    inspected = subprocess.run([binary, "work", str(archive), "--inspect"],
                               env=env, capture_output=True, timeout=15)
    assert inspected.returncode == 0 and b'"BBA"' in inspected.stdout, inspected.stdout
    print("Toolchain CLI/PTY: blockers, exact pinned launcher/version, optional degradation, immutable resume observations, editing/replay and terminal cleanup passed")
