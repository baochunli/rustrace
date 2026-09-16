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

from test_home import isolate
isolate()


BINARY = sys.argv[1]
MANIFEST_V2 = b'''format_version = 2
course_id = "course"
assignment_id = "package-v2"
assignment_version = "v1"
title = "Package v2"
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
CASES = {
    "alpha.in": b"alpha input\n",
    "alpha.expected": b"alpha output\n",
    "second-case.in": b"second input\n",
    "second-case.expected": b"second output\n",
}


def write_package(path, cases=CASES):
    entries = [("assignment.toml", MANIFEST_V2), ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'), ("starter/main.rs", b"fn main() {}\n")]
    entries.extend((f"test-cases/{name}", contents) for name, contents in cases.items())
    with tarfile.open(path, "w", format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in entries:
            info = tarfile.TarInfo(name)
            info.size = len(contents)
            info.mode = 0o644
            info.uid = info.gid = info.mtime = 0
            archive.addfile(info, io.BytesIO(contents))
    with open(path, "rb") as source:
        data = source.read()
    while data.endswith(bytes(512)):
        data = data[:-512]
    with open(path, "wb") as destination:
        destination.write(data + bytes(1024))


def drive_work(package, extra_args=()):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
    before = termios.tcgetattr(slave)

    def child():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    env = {**os.environ, "TERM": "xterm-256color"}
    process = subprocess.Popen(
        [BINARY, "work", package, *extra_args],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        preexec_fn=child,
        env=env,
        cwd=os.path.dirname(package),
    )
    transcript = bytearray()
    deadline = time.monotonic() + 30
    quit_sent = False
    try:
        while process.poll() is None and time.monotonic() < deadline:
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))
            if not quit_sent and b" files" in transcript:
                os.write(master, b"\x11")
                quit_sent = True
            assert len(transcript) < 2 * 1024 * 1024, repr(bytes(transcript[-2000:]))
        assert process.poll() is not None, "work timed out: " + repr(bytes(transcript[-5000:]))
        while select.select([master], [], [], 0.05)[0]:
            chunk = os.read(master, 65536)
            if not chunk:
                break
            transcript.extend(chunk)
        assert process.returncode == 0, repr(bytes(transcript))
        assert quit_sent, repr(bytes(transcript))
        assert b"\x1b[?1049l" in transcript, "terminal cleanup missing"
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


def assert_cases(directory):
    assert os.path.isdir(directory) and not os.path.islink(directory), directory
    for name, contents in CASES.items():
        path = os.path.join(directory, name)
        assert os.path.isfile(path) and not os.path.islink(path), path
        with open(path, "rb") as case_file:
            assert case_file.read() == contents, path


def assert_fresh_collision_rejected(root, kind):
    os.makedirs(root)
    package = os.path.join(root, "assignment.rta")
    write_package(package)
    sibling = os.path.join(root, "test-cases")
    os.mkdir(sibling)
    managed = os.path.join(sibling, "alpha.in")
    if kind == "different":
        with open(managed, "wb") as case_file:
            case_file.write(b"student bytes")
    elif kind == "symlink":
        os.symlink("missing-target", managed)
    elif kind == "directory":
        os.mkdir(managed)
    else:
        raise AssertionError(kind)
    before = os.lstat(managed)

    result = subprocess.run(
        [BINARY, "work", package], capture_output=True, timeout=20, cwd=root
    )

    assert result.returncode != 0, result.stdout + result.stderr
    assert not os.path.exists(os.path.join(root, "assignment.work")), kind
    after = os.lstat(managed)
    assert (before.st_mode, before.st_ino) == (after.st_mode, after.st_ino), kind
    if kind == "different":
        with open(managed, "rb") as case_file:
            assert case_file.read() == b"student bytes"


with tempfile.TemporaryDirectory(prefix="rustrace-package-v2-") as root:
    os.environ["XDG_CONFIG_HOME"] = os.path.join(root, "xdg")
    package = os.path.join(root, "assignment.rta")
    workspace = os.path.join(root, "assignment.work")
    sibling = os.path.join(root, "test-cases")
    write_package(package)

    drive_work(package)
    assert os.path.isfile(os.path.join(workspace, "main.rs"))
    assert_cases(sibling)
    with open(os.path.join(workspace, ".rustrace", "session.json"), encoding="utf-8") as source:
        session_metadata = json.load(source)
    assert len(session_metadata["test_case_suite_hash"]) == 64, session_metadata

    os.remove(os.path.join(sibling, "alpha.in"))
    with open(os.path.join(sibling, "alpha.actual"), "wb") as actual:
        actual.write(b"student output\n")
    snapshot = sorted(os.listdir(sibling))
    inspect = subprocess.run(
        [BINARY, "work", package, "--inspect"], capture_output=True, timeout=20, cwd=root
    )
    assert inspect.returncode == 0, inspect.stdout + inspect.stderr
    assert sorted(os.listdir(sibling)) == snapshot
    assert not os.path.exists(os.path.join(sibling, "alpha.in"))

    drive_work(package)
    assert_cases(sibling)
    with open(os.path.join(sibling, "alpha.actual"), "rb") as actual:
        assert actual.read() == b"student output\n"

    os.remove(os.path.join(sibling, "second-case.in"))
    changed_package = os.path.join(root, "changed-assignment.rta")
    changed_cases = dict(CASES)
    changed_cases["second-case.expected"] = b"changed output\n"
    write_package(changed_package, changed_cases)
    mismatch = subprocess.run(
        [BINARY, "work", changed_package, "--resume"],
        capture_output=True,
        timeout=20,
        cwd=root,
    )
    assert mismatch.returncode != 0, mismatch.stdout + mismatch.stderr
    assert not os.path.exists(os.path.join(sibling, "second-case.in"))
    drive_work(package)
    assert_cases(sibling)

    sibling_snapshot = {
        name: open(os.path.join(sibling, name), "rb").read()
        for name in sorted(os.listdir(sibling))
    }
    doctor = subprocess.run(
        [BINARY, "doctor", package], capture_output=True, timeout=30, cwd=root
    )
    assert b"OK starter package:" in doctor.stdout, doctor.stdout + doctor.stderr
    assert sibling_snapshot == {
        name: open(os.path.join(sibling, name), "rb").read()
        for name in sorted(os.listdir(sibling))
    }

    for kind in ("different", "symlink", "directory"):
        assert_fresh_collision_rejected(os.path.join(root, f"collision-{kind}"), kind)

    symlink_root = os.path.join(root, "collision-root-link")
    os.makedirs(symlink_root)
    package_with_link = os.path.join(symlink_root, "assignment.rta")
    write_package(package_with_link)
    target = os.path.join(symlink_root, "real-cases")
    os.mkdir(target)
    os.symlink("real-cases", os.path.join(symlink_root, "test-cases"))
    result = subprocess.run(
        [BINARY, "work", package_with_link], capture_output=True, timeout=20, cwd=symlink_root
    )
    assert result.returncode != 0, result.stdout + result.stderr
    assert not os.path.exists(os.path.join(symlink_root, "assignment.work"))
    assert os.path.islink(os.path.join(symlink_root, "test-cases"))

    print("sibling files: " + ", ".join(sorted(os.listdir(sibling))))
    print(
        next(
            line
            for line in doctor.stdout.decode("utf-8", errors="replace").splitlines()
            if "starter package:" in line
        )
    )
    print(
        "package v2 fresh placement, collision preflight, inspect isolation, "
        "resume identity-before-repair, unrelated-file preservation, and doctor isolation passed"
    )
