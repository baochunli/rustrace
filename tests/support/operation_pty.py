"""Review H1: production prompts retain their target across reconciliation."""
import ast, fcntl, io, os, pathlib, pty, select, signal, struct
import subprocess, sys, tarfile, tempfile, termios, time

from pty_screen import rendered_screen

from test_home import isolate
isolate()

binary = sys.argv[1]
source = pathlib.Path(__file__).with_name("work_pty.py").read_text()
manifest = next(ast.literal_eval(n.value) for n in ast.parse(source).body
                if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == "manifest" for t in n.targets))
for operation in ["rename", "rename-controls", "rename-cancel", "delete", "confirm", "cancel"]:
    with tempfile.TemporaryDirectory(prefix="rustrace-operation-pty-") as directory:
        root = pathlib.Path(directory)
        package = root / "assignment.rta"
        with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
            for name, data in [("assignment.toml", manifest), ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'), ("starter/main.rs", b"A"), ("starter/other.rs", b"O")]:
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
        proc = subprocess.Popen([sys.executable, "-c", wrapper, binary, "work", str(package)],
                                stdin=slave, stdout=slave, stderr=slave, preexec_fn=child,
                                env={**os.environ, "TERM": "xterm-256color"}, cwd=root)
        transcript = bytearray()
        def pump(seconds):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                if select.select([master], [], [], .02)[0]:
                    transcript.extend(os.read(master, 65536))
                assert len(transcript) < 2 * 1024 * 1024
        def send(keys):
            os.write(master, keys); pump(.12)
        try:
            end = time.monotonic() + 20
            while " files" not in rendered_screen(transcript, rows=32, columns=120) and time.monotonic() < end: pump(.05)
            assert " files" in rendered_screen(transcript, rows=32, columns=120), transcript
            os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.
            pump(.1)
            # Right-click selects other.rs; Esc closes the menu without activating it.
            send(b"\x1b[<2;4;4M"); send(b"\x1b")
            if operation in ["confirm", "cancel"]:
                # Let one edit autosave to reset the session save clock, then make
                # a fresh dirty edit and request deletion from the file menu.
                send(b"\x1b[<0;4;4Mu")
                saved_deadline = time.monotonic() + 5
                other = root / "assignment.work/other.rs"
                while other.read_bytes() != b"uO" and time.monotonic() < saved_deadline:
                    pump(.05)
                assert other.read_bytes() == b"uO", transcript
                send(b"v")
                send(b"\x1b[<2;4;4M"); send(b"\x1b[<0;5;7M")
            elif operation.startswith("rename"):
                send(b"\x1b[<2;4;4M"); send(b"\x1b[<0;5;6M")
                send(b"\x7f" * len("other.rs")); send(b"renamed.rs")
                if operation == "rename-controls":
                    send(b"x\x7f\x1b[B")  # Backspace and ignored tree navigation.
            if operation in ["confirm", "cancel"]:
                confirmation_deadline = time.monotonic() + 2
                while "Delete other.rs?" not in rendered_screen(transcript, rows=32, columns=120) and time.monotonic() < confirmation_deadline:
                    pump(.05)
                assert "Delete other.rs?" in rendered_screen(transcript, rows=32, columns=120), transcript
            (root / "assignment.work/main.rs").write_bytes(b"C")
            if operation != "rename": pump(2.3)  # Poll while prompt/selection is live.
            if operation.startswith("rename"):
                send(b"\x1b" if operation == "rename-cancel" else b"\r")
            else:
                if operation == "delete":
                    send(b"\x1b[<2;4;4M"); send(b"\x1b[<0;5;7M")
                else:
                    send({"confirm": b"y", "cancel": b"\x1b"}[operation])
            send(b"\x11"); proc.wait(timeout=15); pump(.1)
            assert proc.returncode == 0, transcript
            assert b"TERMINAL_RESTORED" in transcript
            assert b"\x1b[?1049l" in transcript and b"\x1b[?2004l" in transcript
            displayed = rendered_screen(transcript, rows=32, columns=120)
            assert "External changes are not accepted" in displayed, transcript
            files = {p.name: p.read_bytes() for p in (root / "assignment.work").glob("*.rs")}
            expected = {"main.rs": b"A"}
            if operation in ["rename", "rename-controls"]: expected["renamed.rs"] = b"O"
            if operation == "rename-cancel": expected["other.rs"] = b"O"
            if operation == "cancel": expected["other.rs"] = b"uvO"
            assert files == expected, (operation, files, expected, transcript)
            print(operation, "target and terminal cleanup passed")
        finally:
            if proc.poll() is None: os.killpg(proc.pid, signal.SIGKILL); proc.wait()
            os.close(master); os.close(slave)
