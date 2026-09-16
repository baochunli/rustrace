import errno
import fcntl
import os
import pty
import struct
import subprocess
import sys
import termios

from test_home import isolate
isolate()


columns = int(sys.argv[1])
rows = int(sys.argv[2])
test_path = sys.argv[3]
command = sys.argv[4:]
# Cargo callers pass their already-temporary config explicitly. Standalone
# invocations use the driver-owned default created above.
if command[0] == "--fixture-config-home":
    os.environ["XDG_CONFIG_HOME"] = command[1]
    command = command[2:]
environment = os.environ.copy()
environment["PATH"] = test_path
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
process = subprocess.Popen(
    command,
    stdin=slave,
    stdout=slave,
    stderr=slave,
    close_fds=True,
    env=environment,
)
os.close(slave)

captured = bytearray()
while True:
    try:
        chunk = os.read(master, 8192)
    except OSError as error:
        if error.errno == errno.EIO:
            break
        raise
    if not chunk:
        break
    captured.extend(chunk)
os.close(master)
status = process.wait()
sys.stdout.buffer.write(captured)
raise SystemExit(status)
