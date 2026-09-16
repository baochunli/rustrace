import os, sys, tarfile, io, tempfile, subprocess, pty, termios, fcntl, struct, select, time, signal

from pty_screen import rendered_screen

from test_home import isolate
isolate()
binary = sys.argv[1]
external = len(sys.argv) > 2 and sys.argv[2] == "external"
manifest = b'''format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "PTY Recovery Assignment"
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
with tempfile.TemporaryDirectory(prefix="rustrace-work-pty-") as root:
    package = os.path.join(root, "assignment.rta")
    with tarfile.open(package, "w", format=tarfile.USTAR_FORMAT) as archive:
        for name, content in [("assignment.toml", manifest), ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'), ("starter/main.rs", b"A")]:
            info = tarfile.TarInfo(name); info.size = len(content); info.mode = 0o600
            archive.addfile(info, io.BytesIO(content))
    data = open(package, "rb").read()
    while data.endswith(bytes(512)): data = data[:-512]
    with open(package, "wb") as file: file.write(data + bytes(1024))
    def run(resume=False):
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
        before = termios.tcgetattr(slave)
        def child():
            os.setsid(); fcntl.ioctl(0, termios.TIOCSCTTY, 0)
        wrapper = "import subprocess,termios,sys; before=termios.tcgetattr(0); r=subprocess.run(sys.argv[1:]); assert termios.tcgetattr(0)==before, 'raw mode leaked'; print('TERMINAL_RESTORED', flush=True); sys.exit(r.returncode)"
        proc = subprocess.Popen([sys.executable, "-c", wrapper, binary, "work", package], stdin=slave, stdout=slave, stderr=slave, preexec_fn=child, env={**os.environ, "TERM":"xterm-256color"}, cwd=root)
        transcript = bytearray()
        deadline = time.monotonic() + 25
        typed = False; unicode_sent = False; quit_sent = False; typed_at = None; changed = False
        confirmation_seen = external; saved_after_cancel = external; cancelled_at = None
        try:
            while proc.poll() is None and time.monotonic() < deadline:
                if select.select([master], [], [], .05)[0]:
                    transcript.extend(os.read(master, 65536))
                screen = rendered_screen(transcript, rows=32, columns=120)
                assert b"Unfinished session" not in transcript, repr(bytes(transcript))
                assert b"Resume [r]" not in transcript, repr(bytes(transcript))
                assert "Unfinished session" not in screen, repr(bytes(transcript))
                assert "Resume [r]" not in screen, repr(bytes(transcript))
                if not typed and " files" in screen:
                    os.write(master, b"\x1b[<0;4;3M\x1b[<0;4;3m")  # Select source below Cargo.toml.
                    os.write(master, b"B"); typed = True; typed_at = time.monotonic()
                if typed and not unicode_sent and time.monotonic() - typed_at >= .2:
                    os.write(master, b"\xc3\xa9"); unicode_sent = True
                if external and unicode_sent and not changed and time.monotonic() - typed_at >= .5:
                    with open(os.path.join(root, "assignment.work", "main.rs"), "wb") as file: file.write(b"REJECTED C")
                    os.write(master, b"\x13"); changed = True
                if unicode_sent and not quit_sent and time.monotonic() - typed_at >= 1:
                    os.write(master, b"\x11"); quit_sent = True
                if quit_sent and not confirmation_seen and "Discard unsaved buffer changes?" in screen:
                    os.write(master, b"\x1b"); confirmation_seen = True; cancelled_at = time.monotonic()
                if confirmation_seen and not saved_after_cancel and time.monotonic() - cancelled_at >= .2:
                    os.write(master, b"\x13"); saved_after_cancel = True; typed_at = time.monotonic(); quit_sent = False
                assert len(transcript) < 2 * 1024 * 1024
            assert proc.poll() is not None, "CLI timed out: " + repr((typed, quit_sent, bytes(transcript[:5000])))
            while select.select([master], [], [], .05)[0]:
                chunk = os.read(master, 65536)
                if not chunk: break
                transcript.extend(chunk)
            assert proc.returncode == 0, repr(bytes(transcript))
            assert typed and unicode_sent and quit_sent and confirmation_seen and saved_after_cancel
            assert b"\x1b[?1049l" in transcript and b"\x1b[?2004l" in transcript, "terminal cleanup missing"
            assert b"TERMINAL_RESTORED" in transcript, "raw terminal mode leaked"
            if external:
                displayed = rendered_screen(transcript, rows=32, columns=120)
                assert "External changes are not accepted" in displayed, "missing direct P2 warning: " + repr(bytes(transcript))
                assert "including unsaved edits" in displayed
                assert b"accept external" not in transcript.lower()
        finally:
            if proc.poll() is None: os.killpg(proc.pid, signal.SIGKILL); proc.wait()
            try: termios.tcsetattr(slave, termios.TCSANOW, before)
            except termios.error: pass
            os.close(master); os.close(slave)
    run()
    if external:
        with open(os.path.join(root, "assignment.work", "main.rs"), "wb") as file: file.write(b"OFFLINE C")
    run(True)
    inspect = subprocess.run([binary, "work", package, "--inspect"], capture_output=True, timeout=20)
    assert inspect.returncode == 0, inspect.stderr + inspect.stdout
    assert b"logical" in inspect.stdout and "BéBéA".encode() in inspect.stdout, inspect.stdout
    assert open(os.path.join(root, "assignment.work", "main.rs"), "rb").read() == "BéBéA".encode()
    print("PTY start, automatic Resume without prompt input, exact keyboard-channel replay, external reconciliation, Inspect, normal quit, and terminal cleanup passed")
