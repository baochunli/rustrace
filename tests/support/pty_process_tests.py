"""A child cannot finish writing its terminal output if its parent stops reading."""
import os
import pty
import select
import subprocess
import sys
import time
import unittest

from pty_process import wait_for_pty_exit


class PtyExitTests(unittest.TestCase):
    def test_full_terminal_output_is_drained_before_exit(self):
        master, slave = pty.openpty()
        child = subprocess.Popen(
            [sys.executable, "-c", "import os; os.write(1,b'x'*65536); os.write(1,b'RESTORED')"],
            stdin=subprocess.DEVNULL, stdout=slave, stderr=slave,
        )
        output = bytearray()

        def pump(seconds):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                if select.select([master], [], [], min(.01, max(0, deadline-time.monotonic())))[0]:
                    output.extend(os.read(master, 65536))

        try:
            wait_for_pty_exit(child, pump, 1)
            pump(.05)
            self.assertEqual(child.returncode, 0)
            self.assertEqual(bytes(output), b'x'*65536+b'RESTORED')
        finally:
            if child.poll() is None:
                child.kill()
            os.close(master)
            os.close(slave)
            child.wait(timeout=1)

    def test_hanging_child_still_obeys_the_original_deadline(self):
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(5)"])
        started = time.monotonic()
        try:
            with self.assertRaises(subprocess.TimeoutExpired):
                wait_for_pty_exit(child, time.sleep, .1)
            self.assertLess(time.monotonic()-started, .3)
        finally:
            child.kill()
            child.wait(timeout=1)


if __name__ == "__main__":
    unittest.main()
