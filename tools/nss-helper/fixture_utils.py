"""Bounded synthetic fixture inputs and subprocess execution."""
import contextlib
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile


@contextlib.contextmanager
def noise_file(scratch):
    fd, name = tempfile.mkstemp(prefix='nss-noise-', dir=scratch)
    try:
        with os.fdopen(fd, 'wb') as output:
            output.write(os.urandom(4096))
        yield Path(name)
    finally:
        os.unlink(name)

def _signal_group(proc, sig):
    try:
        os.killpg(proc.pid, sig)
    except ProcessLookupError:
        pass


def interrupt_on_signal(signum, frame):
    raise SystemExit(128 + signum)



def run(args, env, data=None, *, pass_fds=(), timeout=300, mode='command',
        forward_stderr=False, synthetic_diagnostics=False):
    name = Path(args[0]).name
    print(f'fixture: {name} [{mode}]', file=sys.stderr, flush=True)
    proc = subprocess.Popen([str(value) for value in args], env=env,
                            stdin=subprocess.PIPE if data is not None else subprocess.DEVNULL,
                            stdout=subprocess.PIPE,
                            stderr=None if forward_stderr else subprocess.PIPE,
                            pass_fds=pass_fds, start_new_session=True)
    try:
        stdout, stderr = proc.communicate(input=data, timeout=timeout)
    except subprocess.TimeoutExpired:
        try:
            _signal_group(proc, signal.SIGTERM)
            try:
                proc.communicate(timeout=3)
            except subprocess.TimeoutExpired:
                _signal_group(proc, signal.SIGKILL)
                proc.communicate()
        except BaseException:
            _signal_group(proc, signal.SIGKILL)
            proc.communicate()
            raise
        raise TimeoutError(f'{name} [{mode}] timed out') from None
    except BaseException:
        _signal_group(proc, signal.SIGKILL)
        proc.communicate()
        raise
    if synthetic_diagnostics:
        print(f'fixture synthetic {name} [{mode}] returncode={proc.returncode}', file=sys.stderr, flush=True)
        if stderr:
            text = stderr[:16384].decode('utf-8', errors='replace')
            print(f'fixture synthetic {name} stderr:\n{text}', file=sys.stderr, flush=True)
    if proc.returncode:
        raise RuntimeError(f'{name} [{mode}] exited {proc.returncode}')
    return stdout
