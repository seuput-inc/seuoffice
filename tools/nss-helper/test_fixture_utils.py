import contextlib
import io
import importlib.util
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest

from fixture_utils import noise_file, run

proof_spec = importlib.util.spec_from_file_location('nss_hosted_proof', Path(__file__).with_name('hosted-proof.py'))
proof = importlib.util.module_from_spec(proof_spec)
proof_spec.loader.exec_module(proof)


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

    def test_synthetic_diagnostics_are_opt_in_and_bounded(self):
        code = 'import sys; print("synthetic-out"); sys.stderr.write("synthetic-error"); sys.exit(22)'
        for enabled in (False, True):
            output = io.StringIO()
            with contextlib.redirect_stderr(output), self.assertRaises(RuntimeError):
                run([sys.executable, '-c', code], os.environ, mode='fixture-phase', synthetic_diagnostics=enabled)
            self.assertEqual('synthetic-error' in output.getvalue(), enabled)
            self.assertEqual('[fixture-phase] returncode=22' in output.getvalue(), enabled)
            self.assertNotIn('synthetic-out', output.getvalue())

        output = io.StringIO()
        code = 'import sys; sys.stderr.write("x" * 20000); sys.exit(1)'
        with contextlib.redirect_stderr(output), self.assertRaises(RuntimeError):
            run([sys.executable, '-c', code], os.environ, synthetic_diagnostics=True)
        self.assertLess(len(output.getvalue()), 17000)

    def test_synthetic_success_reports_phase_and_stderr_without_exposing_stdout(self):
        output = io.StringIO()
        code = 'import sys; print("synthetic-payload"); sys.stderr.write("synthetic-warning")'
        with contextlib.redirect_stderr(output):
            result = run([sys.executable, '-c', code], os.environ,
                         mode='fixture-phase', synthetic_diagnostics=True)
        self.assertEqual(result, b'synthetic-payload\n')
        self.assertIn('[fixture-phase] returncode=0', output.getvalue())
        self.assertIn('synthetic-warning', output.getvalue())
        self.assertNotIn('synthetic-payload', output.getvalue())

    def test_helper_stderr_drains_past_the_storage_limit(self):
        code = 'import sys; sys.stderr.buffer.write(b"x" * 200000); sys.stderr.flush(); print("drained")'
        proc = subprocess.Popen([sys.executable, '-c', code], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        pool = ThreadPoolExecutor(max_workers=1)
        try:
            captured = pool.submit(proof.drain_helper_stderr, proc.stderr)
            self.assertEqual(proc.wait(timeout=5), 0)
            self.assertEqual(proc.stdout.read(), b'drained\n')
            self.assertEqual(captured.result(timeout=5), b'x' * 16384)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
            proc.stdout.close()
            proc.stderr.close()
            pool.shutdown()

    def test_helper_diagnostics_remove_credentials_before_output(self):
        output = io.StringIO()
        raw = b'1234 store-secret export-secret ' + b'x' * 20000
        with contextlib.redirect_stderr(output):
            proof.report_helper_diagnostics('synthetic-phase', 1, 2, 0, raw,
                                            (b'store-secret', b'export-secret'))
        text = output.getvalue()
        for secret in ('1234', 'store-secret', 'export-secret'):
            self.assertNotIn(secret, text)
        self.assertIn('returncode=1 status=2 prompts=0', text)
        self.assertLess(len(text.encode()), 17000)

    def test_helper_diagnostics_remove_credentials_cut_at_the_capture_boundary(self):
        for secret in (b'1234', b'store-secret', b'export-secret'):
            prefix = secret[:len(secret) // 2]
            raw = b'x' * (16384 - len(prefix)) + secret
            captured = proof.drain_helper_stderr(io.BytesIO(raw))
            output = io.StringIO()
            with contextlib.redirect_stderr(output):
                proof.report_helper_diagnostics('boundary-phase', 1, 2, 0, captured, (secret,))
            self.assertNotIn(prefix.decode(), output.getvalue())

    def test_helper_diagnostics_redact_overlapping_credential_matches(self):
        exported = b'ab1c' + b'd' * 60
        captured = proof.drain_helper_stderr(io.BytesIO(b'x' * 16381 + exported))
        output = io.StringIO()
        with contextlib.redirect_stderr(output):
            proof.report_helper_diagnostics('overlap-phase', 1, 2, 0, captured, (exported,))
        self.assertNotIn('ab1', output.getvalue())
        self.assertNotIn('ab', output.getvalue())
        output = io.StringIO()
        with contextlib.redirect_stderr(output):
            proof.report_helper_diagnostics('overlap-phase', 1, 2, 0, b'abcd', (b'abc', b'bcd'))
        self.assertNotIn('abcd', output.getvalue())
        self.assertTrue(output.getvalue().endswith('<synthetic-credential>\n'))

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
