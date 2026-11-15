#!/usr/bin/env python3
"""Create a hosted-only real SoftHSM2-backed external PKCS#11 fixture.

All databases, generated RSA keys and wrapper library stay in --scratch. Real
SoftHSM2 performs certificate/key/crypto operations; only the post-setup
protected-path signal is synthetic. No real user store or token is consulted.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
from fixture_utils import interrupt_on_signal, noise_file, run

HERE = Path(__file__).resolve().parent


def run_with_pin(args, env):
    r, w = os.pipe()
    try:
        os.write(w, b'1234\n'); os.close(w); w = -1
        return run(args + ['-f', f'/proc/self/fd/{r}'], env,
                   pass_fds=(r,), mode='token-certificate')
    finally:
        os.close(r)
        if w >= 0: os.close(w)

def main():
    signal.signal(signal.SIGTERM, interrupt_on_signal)
    signal.signal(signal.SIGINT, interrupt_on_signal)
    p = argparse.ArgumentParser(); p.add_argument('--scratch', required=True, type=Path); a = p.parse_args()
    root = a.scratch.resolve(strict=True)
    if any(root.iterdir()): raise ValueError('external fixture scratch must be empty')
    certutil = Path(os.environ['NSS_CERTUTIL']).resolve(strict=True)
    libdir = Path(os.environ['NSS_TEST_LIBDIR']).resolve(strict=True)
    sqlite = Path(os.environ['NSS_SQLITE_LIBDIR']).resolve(strict=True)
    compiler = Path(os.environ['NSS_CC']).resolve(strict=True)
    includes = [Path(s).resolve(strict=True) for s in os.environ['NSS_TEST_INCLUDE_DIRS'].split(':') if s]
    if not includes: raise ValueError('pinned NSS/NSPR include directories required')
    modutil = (certutil.parent / 'modutil').resolve(strict=True)
    openssl = shutil.which('openssl')
    if not openssl: raise ValueError('openssl required')
    env = {'HOME': str(root), 'PATH': '/usr/bin:/bin', 'LC_ALL': 'C',
           'LD_LIBRARY_PATH': str(libdir) + ':' + str(sqlite)}
    provider = Path(os.environ['SOFTHSM_MODULE']).resolve(strict=True)
    if hashlib.sha256(provider.read_bytes()).hexdigest() != os.environ['SOFTHSM_MODULE_SHA256']:
        raise ValueError('synthetic provider artifact hash mismatch')
    if os.environ['SOFTHSM_SOURCE_SHA256'] != '61249473054bcd1811519ef9a989a880a7bdcc36d317c9c25457fc614df475f2':
        raise ValueError('synthetic provider source pin mismatch')
    main_db = root / 'main'; tokens = root / 'tokens'; module_dir = root / 'module'
    for path in (main_db, tokens, module_dir): path.mkdir()
    run([certutil, '-N', '-d', 'sql:' + str(main_db), '--empty-password'], env)
    config = root / 'softhsm2.conf'
    config.write_text('directories.tokendir = ' + str(tokens) + '\nobjectstore.backend = file\nlog.level = ERROR\n')
    marker = root / 'protected-marker'; wrapper = module_dir / 'libtest-token.so'
    defines = ['-DTEST_PROVIDER_PATH=' + json.dumps(str(provider)),
               '-DTEST_PROVIDER_CONFIG=' + json.dumps(str(config)),
               '-DTEST_PROTECTED_MARKER=' + json.dumps(str(marker))]
    inc = [part for path in includes for part in ('-I', str(path))]
    init = module_dir / 'init-token'
    run([compiler, '-O2', *inc, HERE / 'test_softhsm_init.c', '-o', init, '-ldl'], env)
    run([compiler, '-shared', '-fPIC', '-O2', *defines, *inc,
         HERE / 'test_token_wrapper.c', '-o', wrapper, '-ldl'], env)
    env['SOFTHSM2_CONF'] = str(config)
    run([init, provider], env)
    run([modutil, '-force', '-dbdir', 'sql:' + str(main_db), '-add', 'SyntheticExternal',
         '-libfile', wrapper], env)
    nickname = 'SyntheticExternalKey'
    locked_nickname = 'SyntheticNonextractable'
    with noise_file(root) as noise:
        for name, flags, serial in ((nickname, 'token,private,sensitive,extractable', '19'),
                                    (locked_nickname, 'token,private,sensitive,unextractable', '20')):
            run_with_pin([certutil, '-S', '-d', 'sql:' + str(main_db), '-h', 'SyntheticExternal',
                 '-n', name, '-s', 'CN=' + name, '-x', '-t', 'u,u,u', '-m', serial, '-v', '12',
                 '-k', 'rsa', '-g', '2048', '-Z', 'SHA256', '-z', noise, '--keyAttrFlags', flags], env)
    listing = run_with_pin([certutil, '-K', '-d', 'sql:' + str(main_db), '-h', 'SyntheticExternal'], env).decode()
    matches = [m.group(2).strip() for line in listing.splitlines()
               if (m := re.match(r'^<\s*\d+>\s+(\S+)\s+[0-9a-fA-F]{8,}\s+(.+?)\s*$', line))
               and m.group(1) == 'rsa']
    selected = [value for value in matches if nickname in value]
    denied = [value for value in matches if locked_nickname in value]
    if len(selected) != 1 or len(denied) != 1: raise RuntimeError('synthetic RSA keys not uniquely listed')
    pem = run([certutil, '-L', '-d', 'sql:' + str(main_db), '-n', selected[0], '-a'], env)
    der = run([openssl, 'x509', '-outform', 'DER'], env, pem)
    marker.touch(mode=0o600)
    print(json.dumps({'dir': str(main_db), 'nickname': selected[0], 'password': '',
                      'nonextractable': denied[0], 'marker': str(marker),
                      'cert_sha256': hashlib.sha256(der).hexdigest(), 'protected': True}))

if __name__ == '__main__': main()
