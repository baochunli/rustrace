"""A valid unfinished session resumes automatically without consuming PTY input."""
import fcntl
import os
import pathlib
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time

from test_home import isolate
isolate()


binary = sys.argv[1]
package = pathlib.Path(sys.argv[2])
workspace = pathlib.Path(sys.argv[3])
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
before = termios.tcgetattr(slave)


def child():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


wrapper = (
    "import subprocess,termios,sys; before=termios.tcgetattr(0); "
    "result=subprocess.run(sys.argv[1:]); "
    "assert termios.tcgetattr(0)==before, 'raw mode leaked'; "
    "print('TERMINAL_RESTORED', flush=True); sys.exit(result.returncode)"
)
process = subprocess.Popen(
    [
        sys.executable,
        "-c",
        wrapper,
        binary,
        "work",
        package,
        "--workspace",
        workspace,
    ],
    stdin=slave,
    stdout=slave,
    stderr=slave,
    preexec_fn=child,
    env={**os.environ, "TERM": "xterm-256color"},
)
transcript = bytearray()
quit_sent = False
deadline = time.monotonic() + 25
try:
    while process.poll() is None and time.monotonic() < deadline:
        if select.select([master], [], [], 0.05)[0]:
            transcript.extend(os.read(master, 65536))
        assert b"Unfinished session" not in transcript, bytes(transcript)
        assert b"Resume [r]" not in transcript, bytes(transcript)
        if not quit_sent and b" files" in transcript:
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
    assert quit_sent, bytes(transcript[-4000:])
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

print("valid unfinished session automatically resumed on PTY without prompt input")
