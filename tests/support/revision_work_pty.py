import fcntl
import io
import json
import os
import pty
import select
import signal
import struct
import subprocess
import sys
import tarfile
import tempfile
import termios
import time
import zipfile

from test_home import isolate
isolate()


binary = sys.argv[1]
manifest = b'''format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Revision Work Assignment"
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


def command(*args):
    result = subprocess.run(
        [binary, *args], capture_output=True, timeout=30, env=os.environ
    )
    assert result.returncode == 0, (args, result.returncode, result.stdout, result.stderr)
    return result.stdout


def work(package, workspace, edit=None, resume=True):
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
    args = [
            sys.executable,
            "-c",
            wrapper,
            binary,
            "work",
            package,
            "--workspace",
            workspace,
        ]
    if resume:
        args.append("--resume")
    process = subprocess.Popen(
        args,
        stdin=slave,
        stdout=slave,
        stderr=slave,
        preexec_fn=child,
        env={**os.environ, "TERM": "xterm-256color"},
    )
    transcript = bytearray()
    deadline = time.monotonic() + 40
    typed = False
    saved = edit is None
    quit_sent = False
    typed_at = None
    expected_saved = None
    try:
        while process.poll() is None and time.monotonic() < deadline:
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))
            if not typed and b" files" in transcript:
                os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.
                if edit is not None:
                    with open(os.path.join(workspace, "main.rs"), "rb") as source:
                        expected_saved = edit + source.read()
                    os.write(master, edit)
                typed = True
                typed_at = time.monotonic()
            if typed and not saved and open(os.path.join(workspace, "main.rs"), "rb").read() == expected_saved:
                saved = True
                typed_at = time.monotonic()
            if typed and saved and not quit_sent:
                if edit is None or time.monotonic() - typed_at >= 0.35:
                    os.write(master, b"\x11")
                    quit_sent = True
            assert len(transcript) < 2 * 1024 * 1024
        assert process.poll() is not None, ("work timed out", bytes(transcript[-4000:]))
        while select.select([master], [], [], 0.05)[0]:
            try:
                chunk = os.read(master, 65536)
            except OSError:
                break
            if not chunk:
                break
            transcript.extend(chunk)
        assert process.returncode == 0, (process.returncode, bytes(transcript[-4000:]))
        assert quit_sent
        assert b"TERMINAL_RESTORED" in transcript
        assert b"\x1b[?1049l" in transcript and b"\x1b[?2004l" in transcript
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


def rprov_manifest(bundle):
    with zipfile.ZipFile(bundle) as archive:
        data = archive.read("session.rprov")
    assert data[:8] == b"RUSTPROV"
    offset = 40
    path_bytes = int.from_bytes(data[offset : offset + 2], "little")
    payload_bytes = int.from_bytes(data[offset + 8 : offset + 16], "little")
    path_start = offset + 16
    payload_start = path_start + path_bytes
    assert data[path_start:payload_start] == b"manifest.json"
    return json.loads(data[payload_start : payload_start + payload_bytes])


with tempfile.TemporaryDirectory(prefix="rustrace-revision-work-") as temporary:
    package = os.path.join(temporary, "assignment.rta")
    parent = os.path.join(temporary, "parent")
    child = os.path.join(temporary, "child")
    grandchild = os.path.join(temporary, "grandchild")
    parent_bundle = os.path.join(temporary, "parent.zip")
    child_bundle = os.path.join(temporary, "child.zip")

    with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in [
            ("assignment.toml", manifest),
            ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'),
            ("starter/main.rs", b"A"),
        ]:
            info = tarfile.TarInfo(name)
            info.size = len(contents)
            info.mode = 0o600
            archive.addfile(info, io.BytesIO(contents))
    data = open(package, "rb").read()
    while data.endswith(bytes(512)):
        data = data[:-512]
    with open(package, "wb") as output:
        output.write(data + bytes(1024))

    work(package, parent, b"B", resume=False)
    assert open(os.path.join(parent, "main.rs"), "rb").read() != b"A"
    command("submit", parent, "--student-id", "student-1", "--output", parent_bundle)
    command("revise", parent, child, package)

    work(package, child, b"C")
    assert open(os.path.join(child, "main.rs"), "rb").read().startswith(b"C")
    command("submit", child, "--student-id", "student-1", "--output", child_bundle)
    verified = command("verify", child_bundle)
    assert b"Package structure        OK" in verified, verified
    assert len(rprov_manifest(child_bundle)["segments"]) == 2

    command("revise", child, grandchild, package)
    work(package, grandchild)

print(
    "80x24 revised parent/child work, two-segment submit/verify, and third-attempt open passed"
)
