"""Trusted runner fixtures only; never execute assignment or development source."""
import json
import os
import pathlib
import signal
import subprocess
import sys
import time

mode = sys.argv[1]
if mode == "bytes":
    assert os.read(0, 1) == b""
    os.write(1, bytes([0xff, 0, 0x1b]) + b"]52;c;fixture\x07\n")
    os.write(2, b"stderr\xff\x00")
    sys.exit(7)
elif mode == "argv":
    assert os.read(0, 1) == b""
    os.write(1, json.dumps(sys.argv[2:]).encode())
elif mode == "stdin":
    value = b""
    while True:
        block = os.read(0, 8192)
        if not block:
            break
        value += block
    os.write(1, b"stdout:" + value)
    os.write(2, b"stderr:done")
elif mode == "huge":
    block = bytes(range(256)) * 32
    while True:
        os.write(1, block)
        os.write(2, block)
elif mode in ("sleep", "grandchild"):
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    if len(sys.argv) > 2:
        pathlib.Path(sys.argv[2]).write_text(str(os.getpid()))
    time.sleep(20)
elif mode in ("descendants", "retain"):
    child = subprocess.Popen([sys.executable, __file__, "grandchild", sys.argv[2]])
    os.write(1, str(child.pid).encode() + b"\n")
    if mode == "descendants":
        time.sleep(20)
elif mode == "escaped-pipe":
    subprocess.Popen(
        [sys.executable, __file__, "grandchild", sys.argv[2]],
        start_new_session=True,
    )
elif mode == "deadline-escaped-pipes":
    pathlib.Path(sys.argv[2]).write_text(str(os.getpid()))
    subprocess.Popen(
        [sys.executable, __file__, "grandchild", sys.argv[3]],
        start_new_session=True,
    )
    time.sleep(20)
else:
    raise AssertionError("unknown trusted fixture mode")
