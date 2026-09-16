"""Exercise an ordinary later-file replacement during an unpaused CLI save."""
import fcntl, io, os, pathlib, pty, re, select, signal, struct
import subprocess, sys, tarfile, tempfile, termios, time

from test_home import isolate
isolate()

binary, workspace, manifest, mode = sys.argv[1:]
root = pathlib.Path(workspace)
external = mode == "external"
with tempfile.TemporaryDirectory(prefix="rustrace-live-save-pty-") as directory:
    package = pathlib.Path(directory) / "assignment.rta"
    with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
        for name, data in [("assignment.toml", manifest.encode()), ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n')] + [
            ("starter/" + p.name, b"A") for p in sorted(root.glob("*.rs"))
        ]:
            info = tarfile.TarInfo(name); info.size = len(data); info.mode = 0o600
            archive.addfile(info, io.BytesIO(data))
    data = package.read_bytes()
    while data.endswith(bytes(512)): data = data[:-512]
    package.write_bytes(data + bytes(1024))
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
    def child():
        os.setsid(); fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    wrapper = "import subprocess,termios,sys; before=termios.tcgetattr(0); r=subprocess.run(sys.argv[1:]); assert termios.tcgetattr(0)==before; print('TERMINAL_RESTORED',flush=True); sys.exit(r.returncode)"
    proc = subprocess.Popen([sys.executable, "-c", wrapper, binary, "work", str(package),
                             "--workspace", str(root), "--resume"], stdin=slave,
                            stdout=slave, stderr=slave, preexec_fn=child,
                            env={**os.environ, "TERM": "xterm-256color"})
    transcript = bytearray()
    def pump(seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if select.select([master], [], [], .0001)[0]:
                transcript.extend(os.read(master, 65536))
            assert len(transcript) < 2 * 1024 * 1024
    def wait_for(condition):
        end = time.monotonic() + 30
        while not condition() and proc.poll() is None and time.monotonic() < end: pump(.0001)
        assert condition(), ("timeout", mode, transcript)
    try:
        wait_for(lambda: b" files" in transcript)
        os.write(master, b"\x13")
        wait_for(lambda: (root / "f00.rs").read_bytes() == b"BA")
        if external:
            assert (root / "main.rs").read_bytes() == b"A", "harness missed live publication window"
            replacement = pathlib.Path(directory) / "replacement"
            replacement.write_bytes(b"NEW EXTERNAL C")
            os.replace(replacement, root / "main.rs")
            wait_for(lambda: b"recovery required" in transcript)
            assert (root / "main.rs").read_bytes() == b"NEW EXTERNAL C"
        else:
            wait_for(lambda: all(p.read_bytes() == b"BA" for p in root.glob("*.rs")))
        os.write(master, b"\x11")
        wait_for(lambda: proc.poll() is not None); pump(.1)
        assert b"TERMINAL_RESTORED" in transcript, transcript
        assert b"\x1b[?1049l" in transcript and b"\x1b[?2004l" in transcript
        displayed = b" ".join(re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b" ", transcript).split())
        if external:
            # Atomic replacement may also be caught by the bounded reader's
            # identity check while it is scanning, before byte comparison.
            assert (b"external change during save-all" in displayed
                    or b"save-all recheck failed" in displayed), displayed
            assert (root / "main.rs").read_bytes() == b"NEW EXTERNAL C"
        else:
            assert proc.returncode == 0, transcript
            assert b"external change during save-all" not in displayed
        print(mode, "unpaused save control and terminal cleanup passed; exit", proc.returncode)
    finally:
        if proc.poll() is None: os.killpg(proc.pid, signal.SIGKILL); proc.wait()
        os.close(master); os.close(slave)
