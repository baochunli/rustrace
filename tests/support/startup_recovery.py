"""Real CLI recovery after pre-metadata interruption; no SQLite repair is allowed."""
import io, json, os, pathlib, pty, select, signal, struct, subprocess, sys
import tarfile, tempfile, termios, fcntl, time

from test_home import isolate
isolate()

binary = sys.argv[1]
manifest = b'''format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Startup recovery"
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

def package(path, contents=manifest):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, data in [("assignment.toml", contents), ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'), ("starter/main.rs", b"A")]:
            info = tarfile.TarInfo(name); info.size = len(data); info.mode = 0o600
            archive.addfile(info, io.BytesIO(data))
    data = stream.getvalue()
    while data.endswith(bytes(512)): data = data[:-512]
    path.write_bytes(data + bytes(1024))

def snapshot(root):
    return {str(p.relative_to(root)): p.read_bytes() for p in root.rglob("*") if p.is_file() and p.name != "writer.lock"}

def work_pty(archive, *args):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
    def child():
        os.setsid(); fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    wrapper = "import subprocess,termios,sys; b=termios.tcgetattr(0); r=subprocess.run(sys.argv[1:]); assert termios.tcgetattr(0)==b; print('TERMINAL_RESTORED',flush=True); sys.exit(r.returncode)"
    proc = subprocess.Popen([sys.executable, "-c", wrapper, binary, "work", str(archive), *args], stdin=slave, stdout=slave, stderr=slave, preexec_fn=child, env={**os.environ, "TERM": "xterm-256color"})
    output = bytearray(); sent = False; deadline = time.monotonic() + 25
    try:
        while proc.poll() is None and time.monotonic() < deadline:
            if select.select([master], [], [], .05)[0]: output.extend(os.read(master, 65536))
            if b" files" in output and not sent: os.write(master, b"\x11"); sent = True
            assert len(output) < 2 * 1024 * 1024
        assert proc.poll() is not None, repr(output)
        while select.select([master], [], [], .05)[0]:
            chunk = os.read(master, 65536)
            if not chunk: break
            output.extend(chunk)
        assert proc.returncode == 0 and sent, repr(output)
        assert b"TERMINAL_RESTORED" in output and b"\x1b[?1049l" in output and b"\x1b[?2004l" in output
    finally:
        if proc.poll() is None: os.killpg(proc.pid, signal.SIGKILL); proc.wait()
        os.close(master); os.close(slave)

with tempfile.TemporaryDirectory(prefix="rustrace-startup-recovery-") as temporary:
    base = pathlib.Path(temporary)
    if len(sys.argv) > 2 and sys.argv[2] == "damaged-id":
        archive = base / "assignment.rta"; package(archive)
        root = archive.with_suffix(".work"); state = root / ".rustrace"
        work_pty(archive)
        metadata = json.loads((state / "session.json").read_bytes())
        declared = metadata["session_id"]
        declared = declared[:-1] + ("1" if declared[-1] == "0" else "0")
        metadata["session_id"] = declared
        (state / "session.json").write_text(json.dumps(metadata))
        before = snapshot(root)
        inspected = subprocess.run([binary, "work", str(archive), "--inspect"], capture_output=True, timeout=20)
        assert inspected.returncode == 0 and b"start a new workspace" in inspected.stdout and b"journal not validated" in inspected.stdout, inspected.stdout
        for _ in range(2):
            rejected = subprocess.run([binary, "work", str(archive), "--resume"], capture_output=True, timeout=20)
            assert rejected.returncode != 0 and b"start a new workspace" in rejected.stdout and b"preserved" in rejected.stdout, rejected.stdout
        assert snapshot(root) == before
        # The student continues in a new workspace; the original stays preserved.
        fresh = base / "assignment-new.work"
        work_pty(archive, "--workspace", str(fresh))
        assert snapshot(root) == before
        verified = subprocess.run([binary, "work", str(archive), "--workspace", str(fresh), "--inspect"], capture_output=True, timeout=20)
        assert verified.returncode == 0, verified.stdout
        for view in [b"saved", b"logical", b"disk"]:
            assert view + b' main.rs (1 bytes): "A"' in verified.stdout
        print("Damaged declared ID: exact original bytes, repeat rejection, new workspace D=S=L and PTY cleanup passed")
        sys.exit(0)
    for index, metadata in enumerate([None, None, b'{"session_id":', b'corrupt metadata', None]):
        folder = base / str(index); folder.mkdir()
        archive = folder / "assignment.rta"; package(archive)
        root = archive.with_suffix(".work")
        state = root / ".rustrace"
        if index == 4:
            proc = subprocess.Popen([binary, "work", str(archive)], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline and not (state / "reserve.bin").exists():
                    assert proc.poll() is None
                    time.sleep(.0005)
                assert (state / "reserve.bin").exists()
                os.kill(proc.pid, signal.SIGSTOP)
                assert not (state / "session.json").exists(), "missed pre-metadata interruption"
            finally:
                if proc.poll() is None: os.kill(proc.pid, signal.SIGKILL)
                proc.wait()
        elif index == 0:
            root.mkdir(); (root / "main.rs").write_bytes(b"A")
        else:
            state.mkdir(parents=True); (root / "main.rs").write_bytes(b"A")
            (state / "interrupted.sqlite").write_bytes(b"not an initialized SQLite database")
            (state / "reserve.bin").write_bytes(b"partial reserve")
            (state / ".artifact-1-0").write_bytes(b"partial publication")
            if metadata is not None: (state / "session.json").write_bytes(metadata)
        before = snapshot(root)
        for choice in ["--resume"]:
            result = subprocess.run([binary, "work", str(archive), choice], capture_output=True, timeout=20)
            assert result.returncode != 0 and b"start a new workspace" in result.stdout and b"preserved" in result.stdout, result.stdout
            assert snapshot(root) == before
        inspected = subprocess.run([binary, "work", str(archive), "--inspect"], capture_output=True, timeout=20)
        assert inspected.returncode == 0 and b"unknown" in inspected.stdout and b"not validated" in inspected.stdout, inspected.stdout
        assert snapshot(root) == before
        (state / "manifest.toml").write_bytes(manifest)
        before = snapshot(root)
        wrong = folder / "wrong.rta"; package(wrong, manifest.replace(b'"v1"', b'"v2"'))
        rejected = subprocess.run([binary, "work", str(wrong), "--workspace", str(root)], capture_output=True, timeout=20)
        assert rejected.returncode != 0 and b"identity mismatch" in rejected.stdout, rejected.stdout
        assert not list(folder.glob("assignment.work.recovery-*"))
        fresh = folder / "assignment-new.work"
        work_pty(archive, "--workspace", str(fresh))
        assert snapshot(root) == before, "a new workspace modified the original's bytes"
        verified = subprocess.run([binary, "work", str(archive), "--workspace", str(fresh), "--inspect"], capture_output=True, timeout=20)
        assert verified.returncode == 0 and b'logical main.rs (1 bytes): "A"' in verified.stdout, verified.stdout
    print("Missing/corrupt startup metadata, real reserve interruption, exact preservation, package mismatch, new-workspace CLI and terminal cleanup passed")
