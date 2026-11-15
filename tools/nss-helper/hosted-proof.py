#!/usr/bin/env python3
"""Hosted synthetic NSS helper proof through the real Rust launcher fixture.

The caller supplies a trusted fixture builder for a synthetic external PKCS#11
module. No user profile, real token, hardware PIN, or network is used. The
launcher and its bundle must already be inside --scratch. This script is not a
build/dispatch gate and must only run in the isolated hosted proof environment.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import signal
import struct
import subprocess
import time

TIMEOUT = 60
MAX_FRAME = 32 * 1024 * 1024

def file_digest(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()

def header_digests(directories):
    results = []
    for directory in directories.split(os.pathsep):
        root = Path(directory).resolve(strict=True)
        if not root.is_dir(): raise ValueError('header include root is not a directory')
        digest = hashlib.sha256()
        files = sorted(p for p in root.rglob('*') if p.is_file())
        if not files: raise ValueError('empty header include root')
        for path in files:
            name = path.relative_to(root).as_posix().encode()
            digest.update(len(name).to_bytes(8, 'big'))
            digest.update(name)
            digest.update(bytes.fromhex(file_digest(path)))
        results.append({'sha256': digest.hexdigest(), 'file_count': len(files)})
    return results


def run(args, *, env, data=None):
    result = subprocess.run([str(v) for v in args], input=data, env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if result.returncode:
        raise RuntimeError(f'{args[0]} exited {result.returncode}: {result.stderr.decode("utf-8", errors="replace")}')
    return result.stdout

def read_exact(pipe, count, proc):
    if count > MAX_FRAME: raise RuntimeError('oversized helper frame')
    result = bytearray(); deadline = time.monotonic() + TIMEOUT
    with selectors.DefaultSelector() as sel:
        sel.register(pipe, selectors.EVENT_READ)
        while len(result) < count:
            remain = deadline - time.monotonic()
            if remain <= 0 or not sel.select(remain): raise TimeoutError('helper frame timed out')
            part = os.read(pipe.fileno(), count - len(result))
            if not part: raise RuntimeError(f'truncated helper frame (status {proc.poll()})')
            result.extend(part)
    return bytes(result)

def occupy_descriptors():
    source = os.open('/dev/null', os.O_RDONLY)
    for fd in range(200, 217): os.dup2(source, fd, inheritable=True)
    if source not in range(200, 217): os.close(source)

def invoke(launcher, directory, nickname, password, *, env, approve=True, occupied=False):
    fields = [str(directory).encode(), nickname.encode(), password.encode()]
    wire = b'\x02\x01' + b''.join(struct.pack('>Q', len(x)) for x in fields) + b''.join(fields)
    proc = subprocess.Popen([str(launcher)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL, env=env, start_new_session=True,
        close_fds=not occupied, preexec_fn=occupy_descriptors if occupied else None)
    prompts = 0
    try:
        proc.stdin.write(wire); proc.stdin.flush()
        while True:
            status, n1, n2 = struct.unpack('>BQQ', read_exact(proc.stdout, 17, proc))
            if n1 > MAX_FRAME or n2 > MAX_FRAME: raise RuntimeError('oversized result')
            first = read_exact(proc.stdout, n1, proc); second = read_exact(proc.stdout, n2, proc)
            if status == 5:
                if n2: raise RuntimeError('invalid protected confirmation')
                first.decode('utf-8', errors='strict')
                prompts += 1; proc.stdin.write(bytes([2, 1 if approve else 0])); proc.stdin.flush()
                continue
            if status not in range(5) or (status and (n1 or n2)): raise RuntimeError('invalid terminal response')
            proc.stdin.close(); rc = proc.wait(timeout=TIMEOUT)
            if (status == 0) != (rc == 0): raise RuntimeError('launcher status mismatch')
            return status, first, second, prompts
    finally:
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGKILL); proc.wait()
        proc.stdout.close()

def cancel_at_prompt(launcher, directory, nickname, env):
    fields = [str(directory).encode(), nickname.encode(), b'']
    wire = b'\x02\x01' + b''.join(struct.pack('>Q', len(x)) for x in fields) + b''.join(fields)
    proc = subprocess.Popen([str(launcher)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL, env=env, start_new_session=True)
    try:
        proc.stdin.write(wire); proc.stdin.flush()
        status, length, second = struct.unpack('>BQQ', read_exact(proc.stdout, 17, proc))
        if status != 5 or second != 0: raise AssertionError('expected protected confirmation')
        read_exact(proc.stdout, length, proc)
        children_path = Path(f'/proc/{proc.pid}/task/{proc.pid}/children')
        children = [int(x) for x in children_path.read_text().split()]
        if not children: raise AssertionError('missing owned helper child')
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait(timeout=TIMEOUT)
        deadline = time.monotonic() + TIMEOUT
        while time.monotonic() < deadline:
            live = []
            for pid in children:
                try:
                    state = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[0]
                    if state != 'Z': live.append(pid)
                except FileNotFoundError:
                    pass
            if not live: return
            time.sleep(0.05)
        raise AssertionError('helper child survived process-group cancellation')
    finally:
        if proc.poll() is None: os.killpg(proc.pid, signal.SIGKILL); proc.wait()
        proc.stdout.close(); proc.stdin.close()

def original_nonextractable(pk12util, db, nickname, marker, env):
    # Output and both synthetic passwords use inherited anonymous FDs only.
    output = os.memfd_create('synthetic-pk12')
    export_r, export_w = os.pipe(); pin_r, pin_w = os.pipe()
    try:
        os.write(export_w, b'fixture-export\n'); os.close(export_w); export_w = -1
        os.write(pin_w, b'1234\n'); os.close(pin_w); pin_w = -1
        marker.unlink()
        result = subprocess.run([str(pk12util), '-o', f'/proc/self/fd/{output}',
            '-d', 'sql:' + str(db), '-n', nickname, '-w', f'/proc/self/fd/{export_r}',
            '-k', f'/proc/self/fd/{pin_r}'], env=env, pass_fds=(output, export_r, pin_r),
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=TIMEOUT)
        if result.returncode == 0 or os.fstat(output).st_size != 0:
            raise AssertionError('original unexpectedly exported unextractable key')
        error = result.stderr.decode(errors='replace')
        if not re.search(r'add cert and key failed|unable.to.export.key', error, re.I):
            raise AssertionError('original failed before key export')
        return 1 if re.search(r'password|login|authentication|SEC_ERROR_BAD_PASSWORD|token', error, re.I) else 2
    finally:
        marker.touch(mode=0o600)
        for fd in (output, export_r, export_w, pin_r, pin_w):
            if fd >= 0: os.close(fd)

def expected_cert(certutil, openssl, db, nickname, env):
    pem = run([certutil, '-L', '-d', 'sql:' + str(db), '-n', nickname, '-a'], env=env)
    der = run([openssl, 'x509', '-outform', 'DER'], env=env, data=pem)
    return hashlib.sha256(der).hexdigest()

def assert_identity(openssl, p12, password, expected, env):
    r, w = os.pipe()
    try:
        os.write(w, password + b'\n'); os.close(w); w = -1
        process = subprocess.run([str(openssl), 'pkcs12', '-clcerts', '-nokeys', '-passin', f'fd:{r}'],
            input=p12, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, pass_fds=(r,), check=True)
        der = run([openssl, 'x509', '-outform', 'DER'], env=env, data=process.stdout)
        if hashlib.sha256(der).hexdigest() != expected: raise AssertionError('export selected wrong identity')
    finally:
        os.close(r)
        if w >= 0: os.close(w)

def sql_fixture(certutil, openssl, scratch, label, env):
    db = scratch / label; db.mkdir()
    run([certutil, '-N', '-d', 'sql:' + str(db), '--empty-password'], env=env)
    nickname = 'Synthetic ' + label
    run([certutil, '-S', '-d', 'sql:' + str(db), '-n', nickname, '-s', 'CN=' + label,
         '-x', '-t', 'u,u,u', '-m', '17', '-v', '12', '-k', 'rsa', '-g', '2048',
         '-Z', 'SHA256', '-z', '/dev/urandom'], env=env)
    return db, nickname, expected_cert(certutil, openssl, db, nickname, env)

def main():
    p = argparse.ArgumentParser()
    for flag in ('bundle', 'scratch', 'launcher', 'fixture-setup', 'receipt'):
        p.add_argument('--' + flag, required=True, type=Path)
    p.add_argument('--certutil', type=Path, default=os.environ.get('NSS_CERTUTIL'))
    p.add_argument('--openssl', type=Path, default=shutil.which('openssl'))
    p.add_argument('--tool-lib-dir', type=Path, default=os.environ.get('NSS_TEST_LIBDIR'))
    p.add_argument('--sqlite-lib-dir', type=Path, default=os.environ.get('NSS_SQLITE_LIBDIR'))
    a = p.parse_args()
    if not all((a.certutil, a.openssl, a.tool_lib_dir, a.sqlite_lib_dir)):
        p.error('certutil, openssl, NSS test libdir and SQLite libdir are required')
    scratch = a.scratch.resolve(strict=True); launcher = a.launcher.resolve(strict=True); bundle = a.bundle.resolve(strict=True)
    if scratch not in launcher.parents or scratch not in bundle.parents:
        raise ValueError('launcher and bundle must be in isolated scratch')
    if bundle != launcher.parent.parent / 'lib' / 'SeuOffice' / 'nss':
        raise ValueError('fixture launcher/bundle layout mismatch')
    receipt = a.receipt.resolve()
    if scratch not in receipt.parents or receipt.exists():
        raise ValueError('receipt must be a new file inside isolated scratch')
    env = {'HOME': str(scratch), 'PATH': '/usr/bin:/bin', 'LC_ALL': 'C',
           'LD_LIBRARY_PATH': str(a.tool_lib_dir.resolve(strict=True)) + ':' + str(a.sqlite_lib_dir.resolve(strict=True)),
           'NSS_CERTUTIL': str(a.certutil.resolve(strict=True)),
           'NSS_TEST_LIBDIR': str(a.tool_lib_dir.resolve(strict=True)),
           'NSS_SQLITE_LIBDIR': str(a.sqlite_lib_dir.resolve(strict=True))}
    for name in ('NSS_CC', 'NSS_TEST_INCLUDE_DIRS', 'SOFTHSM_MODULE',
                 'SOFTHSM_MODULE_SHA256', 'SOFTHSM_SOURCE_SHA256',
                 'SOFTHSM_SOURCE_ARCHIVE'):
        if not os.environ.get(name): p.error(name + ' is required')
        env[name] = os.environ[name]
    source_sha = file_digest(env['SOFTHSM_SOURCE_ARCHIVE'])
    if source_sha != '61249473054bcd1811519ef9a989a880a7bdcc36d317c9c25457fc614df475f2' or source_sha != env['SOFTHSM_SOURCE_SHA256']:
        raise ValueError('SoftHSM source archive does not match pinned digest')
    module_sha = file_digest(env['SOFTHSM_MODULE'])
    if module_sha != env['SOFTHSM_MODULE_SHA256']:
        raise ValueError('SoftHSM module digest mismatch')
    compiler = Path(env['NSS_CC']).resolve(strict=True)
    compiler_sha = file_digest(compiler)
    compiler_version = run([compiler, '--version'], env=env).decode('utf-8', errors='replace').splitlines()[0]
    headers = header_digests(env['NSS_TEST_INCLUDE_DIRS'])
    manifest_path = bundle / 'manifest.json'
    manifest = json.loads(manifest_path.read_text())
    db, nick, cert = sql_fixture(a.certutil, a.openssl, scratch, 'sql-a', env)
    status, p12, pw, prompts = invoke(launcher, db, nick, '', env=env)
    if status or not p12 or not pw or prompts: raise AssertionError('SQL export failed')
    assert_identity(a.openssl, p12, pw, cert, env)
    status, p12, pw, _ = invoke(launcher, db, nick, '', env=env, occupied=True)
    if status: raise AssertionError('occupied descriptor collision')
    assert_identity(a.openssl, p12, pw, cert, env)
    db2, nick2, cert2 = sql_fixture(a.certutil, a.openssl, scratch, 'sql-b', env)
    with ThreadPoolExecutor(max_workers=2) as pool:
        futures = [pool.submit(invoke, launcher, d, n, '', env=env) for d, n in ((db, nick), (db2, nick2))]
        results = [f.result() for f in futures]
    for result, expected in zip(results, (cert, cert2)):
        status, p12, pw, _ = result
        if status: raise AssertionError('concurrent launch failed')
        assert_identity(a.openssl, p12, pw, expected, env)
    # The product always selects sql:<dir>; an empty legacy-only directory
    # must not be silently reinterpreted as dbm:<dir>.
    legacy = scratch / 'legacy-only'; legacy.mkdir()
    run([a.certutil, '-N', '-d', 'dbm:' + str(legacy), '--empty-password'], env=env)
    status, _, _, _ = invoke(launcher, legacy, nick, '', env=env)
    if status == 0: raise AssertionError('legacy directory incorrectly exported')
    # Fixture builder creates its own synthetic module and DB in scratch; it
    # must register the real module with NSS and identify its generated cert.
    external_dir = scratch / 'external'; external_dir.mkdir()
    fixture = json.loads(run([a.fixture_setup, '--scratch', external_dir], env=env))
    if set(fixture) != {'dir', 'nickname', 'password', 'nonextractable', 'marker', 'cert_sha256', 'protected'}:
        raise ValueError('invalid external fixture contract')
    ext_db = Path(fixture['dir']).resolve(strict=True)
    if external_dir not in ext_db.parents or fixture['protected'] is not True: raise ValueError('fixture not isolated/protected')
    status, p12, pw, prompts = invoke(launcher, ext_db, fixture['nickname'], fixture['password'], env=env)
    if status or prompts == 0: raise AssertionError('protected external token not exercised')
    assert_identity(a.openssl, p12, pw, fixture['cert_sha256'], env)
    status, _, _, prompts = invoke(launcher, ext_db, fixture['nickname'], fixture['password'], env=env, approve=False)
    if status == 0 or prompts == 0: raise AssertionError('protected-token denial succeeded')
    marker = Path(fixture['marker']).resolve(strict=True)
    if external_dir not in marker.parents: raise ValueError('fixture marker not isolated')
    cancel_at_prompt(launcher, ext_db, fixture['nickname'], env)
    pk12util = (a.certutil.resolve(strict=True).parent / 'pk12util').resolve(strict=True)
    original_status = original_nonextractable(pk12util, ext_db, fixture['nonextractable'], marker, env)
    status, _, _, _ = invoke(launcher, ext_db, fixture['nonextractable'], fixture['password'], env=env)
    if status != original_status: raise AssertionError('nonextractable error differs from original pk12util')
    # Copy the synthetic fixture installation to each isolated negative case;
    # the launcher retains its original embedded manifest trust anchor.
    for mode in ('missing', 'tampered'):
        case = scratch / ('negative-' + mode); case.mkdir()
        shutil.copytree(launcher.parent.parent, case / 'usr', symlinks=True)
        copied_launcher = case / 'usr/bin' / launcher.name
        victim = case / 'usr/lib/SeuOffice/nss/libnssutil3.so'
        if mode == 'missing': victim.unlink()
        else:
            with victim.open('r+b') as f:
                byte = f.read(1); f.seek(0); f.write(bytes([byte[0] ^ 1]))
        result = subprocess.run([str(copied_launcher)], input=b'', stdout=subprocess.PIPE,
                                stderr=subprocess.DEVNULL, env=env, timeout=TIMEOUT)
        if result.returncode != 2 or result.stdout:
            raise AssertionError('resolver did not reject missing/tampered bundle')
    proof = {
        'format': 1,
        'result': 'passed',
        'scope': 'synthetic protected callback and real software cryptography; not physical hardware or PIN-pad attestation',
        'bundle_manifest_sha256': file_digest(manifest_path),
        'nss_source': manifest['source']['nss'],
        'nspr_source': manifest['source']['nspr'],
        'provider': {'name': 'SoftHSM2', 'source_sha256': source_sha,
                     'module_sha256': module_sha, 'license': 'BSD-2-Clause'},
        'compiler': {'sha256': compiler_sha, 'version': compiler_version},
        'include_roots': headers,
        'checks': ['sql-export-identity', 'descriptor-collision', 'concurrent-export',
                   'legacy-only-rejection', 'external-token-export-identity',
                   'protected-approval-denial-and-cancellation',
                   'nonextractable-pk12util-error-parity', 'missing-and-tampered-bundle'],
        'limitations': ['Provider source-to-binary correspondence requires independent build provenance.',
                        'No physical token or PIN-pad was tested.'],
    }
    fd = os.open(receipt, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w') as output:
        json.dump(proof, output, indent=2)
        output.write('\n')
    print('synthetic NSS launcher, descriptor collision, concurrency, SQL/legacy, external token and bundle failures: OK')

if __name__ == '__main__': main()
