"""G3 M1: actual search/edit/F3 with observed keys and verified internal paste."""
from test_home import isolate
isolate()

import fcntl
import os
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

sys.dont_write_bytecode = True
from clipboard_pty import package_at, rendered_screen

for internal in [False, True]:
    with tempfile.TemporaryDirectory(prefix="rustrace-search-revision-") as root:
        package = package_at(root)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 140, 0, 0))

        def child():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        wrapper = (
            "import subprocess,termios,sys; before=termios.tcgetattr(0); "
            "r=subprocess.run(sys.argv[1:]); assert termios.tcgetattr(0)==before; "
            "print('TERMINAL_RESTORED',flush=True); sys.exit(r.returncode)"
        )
        proc = subprocess.Popen(
            [sys.executable, "-c", wrapper, sys.argv[1], "work", package],
            stdin=slave, stdout=slave, stderr=slave, preexec_fn=child, cwd=root,
            env={**os.environ, "TERM": "xterm-256color"},
        )
        transcript = bytearray()

        def pump(seconds):
            until = time.monotonic() + seconds
            while time.monotonic() < until:
                if select.select([master], [], [], 0.02)[0]:
                    transcript.extend(os.read(master, 65536))
                assert len(transcript) < 2 * 1024 * 1024

        def send(keys):
            os.write(master, keys)
            pump(0.15)

        def wait_for_screen(predicate, label, timeout=10):
            until = time.monotonic() + timeout
            while time.monotonic() < until:
                pump(0.05)
                if predicate(rendered_screen(transcript)):
                    return
            raise AssertionError(f"timed out waiting for {label}")

        try:
            until = time.monotonic() + 15
            while b" files" not in transcript and time.monotonic() < until:
                pump(0.05)
            assert b" files" in transcript
            send(b"\x1b[<0;4;3M\x1b[<0;4;3m")
            send(b"\x01abc")
            if internal:
                send(b"\x1b[17~")  # F6: second source buffer
                send(b"\x01" + "é".encode())
                send(b"\x01\x03")  # select and copy inside this session
                send(b"\x1b[15~")  # F5: first buffer
            send(b"\x06")
            wait_for_screen(
                lambda screen: "find and replace" in screen,
                "find-and-replace panel",
            )
            send(b"a\r")
            send(b"\x1b")
            wait_for_screen(
                lambda screen: "find and replace" not in screen,
                "closed find-and-replace panel",
            )
            send(b"\x16" if internal else "é".encode())
            send(b"\x1b[13~")  # F3: old match end byte 1 is inside é
            assert proc.poll() is None, "production F3 crashed after Unicode edit"
            assert "no match" in rendered_screen(transcript)
            until = time.monotonic() + 10
            source = os.path.join(root, "assignment.work", "a.rs")
            while open(source, "rb").read() != "ébc".encode() and time.monotonic() < until:
                pump(0.05)
            assert open(source, "rb").read() == "ébc".encode(), "autosave did not persist search edit"
            send(b"\x11")
            proc.wait(timeout=15)
            pump(0.1)
            assert proc.returncode == 0
            assert b"TERMINAL_RESTORED" in transcript
            assert b"\x1b[?1049l" in transcript and b"\x1b[?2004l" in transcript
            with open(os.path.join(root, "assignment.work", "a.rs"), "rb") as file:
                assert file.read() == "ébc".encode()
            inspected = subprocess.run(
                [sys.argv[1], "work", package, "--inspect"], capture_output=True, timeout=20,
            )
            assert inspected.returncode == 0
            assert "ébc".encode() in inspected.stdout
            print("internal paste" if internal else "keyboard", "search/F3/save/replay/cleanup passed")
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait()
            os.close(master)
            os.close(slave)
