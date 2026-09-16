"""Bounded process completion while the fixture owns a PTY output buffer."""
import time


def wait_for_pty_exit(process, pump, timeout):
    deadline = time.monotonic() + timeout
    while process.poll() is None and time.monotonic() < deadline:
        pump(min(.05, max(0, deadline - time.monotonic())))
    process.wait(timeout=max(0, deadline - time.monotonic()))
