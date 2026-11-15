import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest

from fixture_utils import noise_file, run


class FixtureUtilsTest(unittest.TestCase):
    def test_noise_is_private_finite_and_removed(self):
        with tempfile.TemporaryDirectory() as directory:
            with noise_file(directory) as path:
                self.assertTrue(stat.S_ISREG(path.stat().st_mode))
                self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
                with path.open('rb') as source:
                    self.assertEqual(len(source.read()), 4096)
                    self.assertEqual(source.read(), b'')
            self.assertFalse(path.exists())

    def test_stdin_and_passed_descriptor(self):
        read_fd, write_fd = os.pipe()
        try:
            os.write(write_fd, b'pin'); os.close(write_fd); write_fd = -1
            code = 'import os,sys; sys.stdout.buffer.write(os.read(int(sys.argv[1]), 16)+sys.stdin.buffer.read())'
            output = run([sys.executable, '-c', code, str(read_fd)], os.environ,
                         data=b'input', pass_fds=(read_fd,))
            self.assertEqual(output, b'pininput')
        finally:
            os.close(read_fd)
            if write_fd >= 0: os.close(write_fd)
        output = run([sys.executable, '-c', 'import sys; print(len(sys.stdin.buffer.read()))'], os.environ)
        self.assertEqual(output, b'0\n')

    def test_timeout_reaps_child(self):
        with self.assertRaises(TimeoutError):
            run([sys.executable, '-c', 'import time; time.sleep(30)'], os.environ, timeout=0.2)

    def test_outer_timeout_reaps_nested_group(self):
        with tempfile.TemporaryDirectory() as directory:
            pidfile = Path(directory) / 'child.pid'
            child = ('import os,sys,time; '
                     'open(sys.argv[1], "w").write(str(os.getpid())); time.sleep(30)')
            wrapper = (
                'import os,signal,sys; '
                'from fixture_utils import interrupt_on_signal,run; '
                'signal.signal(signal.SIGTERM,interrupt_on_signal); '
                'signal.signal(signal.SIGINT,interrupt_on_signal); '
                'run([sys.executable,"-c",sys.argv[1],sys.argv[2]],os.environ)'
            )
            env = dict(os.environ, PYTHONPATH=str(Path(__file__).resolve().parent))
            with self.assertRaises(TimeoutError):
                run([sys.executable, '-c', wrapper, child, str(pidfile)], env, timeout=2)
            pid = int(pidfile.read_text())
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)


if __name__ == '__main__': unittest.main()
