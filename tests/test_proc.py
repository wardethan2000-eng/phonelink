import os
import subprocess
import sys
import time
import unittest

from phonelink import proc


class ProcIdentityTests(unittest.TestCase):
    def test_self_start_time_is_readable(self):
        self.assertIsInstance(proc.proc_start_time(os.getpid()), int)

    def test_self_matches_its_own_token(self):
        token = proc.proc_start_time(os.getpid())
        self.assertTrue(proc.process_matches(os.getpid(), token))

    def test_wrong_token_does_not_match(self):
        # Same live PID, wrong start token → treated as a different process.
        self.assertFalse(proc.process_matches(os.getpid(), 999_999_999))

    def test_missing_token_never_matches(self):
        self.assertFalse(proc.process_matches(os.getpid(), 0))
        self.assertFalse(proc.process_matches(os.getpid(), None))

    def test_dead_pid_does_not_match(self):
        # Spawn a trivial child, capture its identity, let it exit, then reap it.
        child = subprocess.Popen([sys.executable, "-c", "pass"])
        token = proc.proc_start_time(child.pid)
        child.wait()
        # After the child is gone, neither its old token nor any token matches.
        for _ in range(50):
            if proc.proc_start_time(child.pid) is None:
                break
            time.sleep(0.02)
        self.assertFalse(proc.process_matches(child.pid, token))


if __name__ == "__main__":
    unittest.main()
