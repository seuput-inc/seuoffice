#!/usr/bin/env python3
"""Hosted-only pinned Linux NSS helper producer; never run on a user machine.

Inputs are pre-fetched archives and an immutable reviewed build environment.
The output is source-only until a hosted build and descriptor-closure fixture
succeeds. No user profiles, certificate stores, keys, or accounts are used.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile

HERE = Path(__file__).resolve().parent
HELPER = HERE.parent.parent / 'crates/shell/src/host/d2/pdf/crypto/nss_export_helper.c'
CONTRACT = json.loads((HERE / 'contract.json').read_text())
if CONTRACT['format'] != 1:
    raise ValueError('unsupported NSS artifact contract')
PINS = {name: (CONTRACT['sources'][name]['filename'],
               CONTRACT['sources'][name]['sha256'],
               CONTRACT['sources'][name]['url'])
        for name in ('nss', 'sqlite', 'glibc')}
ALL_FD = {item['id']: item['fd'] for item in CONTRACT['artifacts']}
if len(ALL_FD) != len(CONTRACT['artifacts']) or len(set(ALL_FD.values())) != len(ALL_FD):
    raise ValueError('duplicate NSS artifact ID or descriptor')
FD = {item['id']: item['fd'] for item in CONTRACT['artifacts'] if item['kind'] == 'library'}
CHECKS = {item['check_for']: (item['id'], item['fd']) for item in CONTRACT['artifacts']
          if item['kind'] == 'integrity'}

def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for part in iter(lambda: f.read(1024 * 1024), b''): h.update(part)
    return h.hexdigest()

def tree_sha(root):
    h = hashlib.sha256()
    for path in sorted(root.rglob('*')):
        if path.is_symlink(): raise ValueError('kernel header tree must not contain symlinks')
        if path.is_file():
            name = path.relative_to(root).as_posix().encode()
            h.update(len(name).to_bytes(8, 'big')); h.update(name)
            h.update(path.stat().st_size.to_bytes(8, 'big'))
            with path.open('rb') as f:
                for part in iter(lambda: f.read(1024 * 1024), b''): h.update(part)
    return h.hexdigest()

def run(args, cwd=None, env=None, capture=False):
    return subprocess.run([str(a) for a in args], cwd=cwd, env=env, check=True,
                          text=True, stdout=subprocess.PIPE if capture else None).stdout

def unpack(archive, out):
    out.mkdir()
    with tarfile.open(archive, 'r:gz') as tf:
        tf.extractall(out, filter='data')
    dirs = list(out.iterdir())
    if len(dirs) != 1 or not dirs[0].is_dir(): raise RuntimeError('unexpected archive root')
    return dirs[0]

def one(paths, desc):
    matches = sorted(set(p.resolve(strict=True) for p in paths if p.exists()))
    if len(matches) != 1: raise RuntimeError(f'expected one {desc}, got {matches}')
    return matches[0]

def dynamic(path, patchelf):
    text = run([patchelf, '--print-needed', path], capture=True)
    return [line for line in text.splitlines() if line]

def tool_record(cfg):
    # The CI owner pins the OCI image and the tool binaries before invocation;
    # these inputs are recorded into the embedded manifest, not self-authorizing.
    required = ['image_digest', 'cc', 'cxx', 'ld', 'make', 'patchelf', 'readelf', 'kernel_headers']
    if set(cfg) != set(required): raise ValueError('toolchain keys differ from contract')
    if not cfg['image_digest'].startswith('sha256:') or len(cfg['image_digest']) != 71 or any(c not in '0123456789abcdef' for c in cfg['image_digest'][7:]):
        raise ValueError('immutable build image digest required')
    result = {'image_digest': cfg['image_digest']}
    for key in required[1:-1]:
        item = cfg[key]
        if set(item) != {'path', 'sha256'}: raise ValueError('tool binary identity required')
        path = Path(item['path']).resolve(strict=True)
        actual = sha(path)
        if actual != item['sha256']: raise ValueError(f'{key} hash mismatch')
        result[key] = f'{path.name}|sha256:{actual}|{run([path, "--version"], capture=True).splitlines()[0]}'
    item = cfg['kernel_headers']
    if set(item) != {'path', 'sha256'}: raise ValueError('kernel header identity required')
    headers = Path(item['path']).resolve(strict=True)
    if not headers.is_dir() or not (headers / 'linux/version.h').is_file() or not (headers / 'asm/unistd.h').is_file():
        raise ValueError('incomplete kernel UAPI headers')
    digest = tree_sha(headers)
    if digest != item['sha256']: raise ValueError('kernel UAPI header hash mismatch')
    result['kernel_headers'] = 'sha256:' + digest
    return result

def main():
    p = argparse.ArgumentParser()
    for name in PINS: p.add_argument('--' + name, required=True, type=Path)
    p.add_argument('--toolchain', required=True, type=Path)
    p.add_argument('--work', required=True, type=Path)
    p.add_argument('--output', required=True, type=Path)
    p.add_argument('--arch', choices=['x86_64', 'aarch64'], required=True)
    p.add_argument('--kernel-min', required=True)
    a = p.parse_args()
    for name, (filename, expected, _) in PINS.items():
        f = getattr(a, name).resolve(strict=True)
        if f.name != filename or sha(f) != expected: raise ValueError(f'{name} source checksum mismatch')
    cfg = json.loads(a.toolchain.read_text())
    tools = tool_record(cfg)
    cc, cxx, make, patchelf = (cfg[x]['path'] for x in ('cc', 'cxx', 'make', 'patchelf'))
    work = a.work.resolve(); out = a.output.resolve()
    if work.exists() or out.exists() or work == out or work in out.parents or out in work.parents:
        raise ValueError('work and output must be fresh, disjoint paths')
    work.mkdir(parents=True); out.mkdir(parents=True)
    triple = run([cc, '-dumpmachine'], capture=True).strip()
    if not triple.startswith(a.arch + '-') or not a.kernel_min:
        raise ValueError('toolchain target or minimum kernel does not match request')
    selected_ld = run([cc, '-print-prog-name=ld'], capture=True).strip()
    resolved_ld = shutil.which(selected_ld)
    if not resolved_ld or Path(resolved_ld).resolve(strict=True) != Path(cfg['ld']['path']).resolve(strict=True):
        raise ValueError('compiler-selected linker does not match pinned ld')
    env = {'PATH': os.environ['PATH'], 'HOME': str(work), 'LC_ALL': 'C', 'TZ': 'UTC',
           'SOURCE_DATE_EPOCH': '1738627200', 'CC': cc, 'CXX': cxx,
           'BUILD_OPT': '1', 'USE_64': '1', 'NSS_DISABLE_GTESTS': '1',
           'NSS_DISABLE_NSPR_TESTS': '1'}
    glibc = unpack(a.glibc, work / 'glibc-src'); build = work / 'glibc-build'; build.mkdir()
    sysroot = work / 'sysroot'
    include = sysroot / 'usr' / 'include'
    include.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(Path(cfg['kernel_headers']['path']).resolve(strict=True), include)
    # BZ 31928: the driver must see -U; -Wp,-U bypasses Ubuntu GCC's defaults.
    glibc_env = dict(env, CFLAGS='-g -O2 -U_FORTIFY_SOURCE')
    run([glibc / 'configure', '--prefix=/usr', '--disable-werror', '--without-selinux', '--with-headers=' + str(include), '--enable-kernel=' + a.kernel_min], build, glibc_env)
    run([make, '-j2'], build, glibc_env); run([make, 'install', 'DESTDIR=' + str(sysroot)], build, glibc_env)
    crt_dir = one(sysroot.glob('**/crt1.o'), 'built glibc startup directory').parent
    compiler_root = ['--sysroot=' + str(sysroot), '-B' + str(crt_dir) + '/']
    for name in ('crt1.o', 'Scrt1.o', 'crti.o', 'crtn.o', 'crtbegin.o', 'crtend.o'):
        crt = Path(run([cc, *compiler_root, '-print-file-name=' + name], capture=True).strip()).resolve(strict=True)
        if name in ('crt1.o', 'Scrt1.o', 'crti.o', 'crtn.o') and sysroot.resolve() not in crt.parents:
            raise ValueError(f'compiler did not resolve {name} from built sysroot')
        tools[name] = 'sha256:' + sha(crt)
    libgcc = Path(run([cc, *compiler_root, '-print-libgcc-file-name'], capture=True).strip()).resolve(strict=True)
    tools['libgcc'] = libgcc.name + '|sha256:' + sha(libgcc)
    # The compiler executable and the just-built glibc sysroot are inputs to all
    # NSS/SQLite objects. Packaging verifies the resulting ELF closure below.
    syscc = cc + ' ' + ' '.join(compiler_root)
    buildenv = dict(env, CC=syscc, CXX=cxx + ' ' + ' '.join(compiler_root))
    sqlite = unpack(a.sqlite, work / 'sqlite-src'); sqlite_prefix = work / 'sqlite-prefix'
    run([sqlite / 'configure', '--prefix=' + str(sqlite_prefix), '--disable-static', '--enable-shared'], sqlite, buildenv)
    run([make, '-j2'], sqlite, buildenv); run([make, 'install'], sqlite, buildenv)
    # A pristine tooling build supplies shlibsign, independent of the patched
    # runtime loader. Patched runtime is built separately with no autogeneration
    # of .chk until every DT_NEEDED rewrite is complete.
    pristine = unpack(a.nss, work / 'nss-pristine'); modified = unpack(a.nss, work / 'nss-patched')
    nss_a, nss_b = pristine / 'nss', modified / 'nss'
    nenv = dict(buildenv, SQLITE_INCLUDE_DIR=str(sqlite_prefix / 'include'),
                SQLITE_LIB_DIR=str(sqlite_prefix / 'lib'), SQLITE_LIB_NAME='sqlite3',
                LD_LIBRARY_PATH=str(sqlite_prefix / 'lib'))
    run([make, 'nss_build_all', '-j2'], nss_a, nenv)
    sign = one((pristine / 'dist').glob('*/bin/shlibsign'), 'pristine shlibsign')
    checktool = one((pristine / 'dist').glob('*/bin/chktest'), 'pristine chktest')
    run(['python3', HERE / 'patch_nss.py', nss_b], env=env)
    run([make, 'nss_build_all', '-j2', 'SHLIBSIGN_SRCDIR='], nss_b, nenv)
    program = nss_b / 'cmd' / 'seu-nss-export'; program.mkdir()
    shutil.copyfile(HELPER, program / 'nss_export_helper.c')
    (program / 'manifest.mn').write_text('CORE_DEPTH = ../..\nMODULE = nss\nCSRCS = nss_export_helper.c\nREQUIRES = dbm seccmd\nPROGRAM = seu-nss-export\n')
    (program / 'Makefile').write_text('include manifest.mn\ninclude $(CORE_DEPTH)/coreconf/config.mk\ninclude ../platlibs.mk\ninclude $(CORE_DEPTH)/coreconf/rules.mk\ninclude ../platrules.mk\n')
    run([make, '-j2', 'USE_STATIC_LIBS=1'], program, nenv)
    helper = one((modified / 'dist').glob('*/bin/seu-nss-export'), 'helper')
    libdir = one((modified / 'dist').glob('*/lib'), 'NSS libdir')
    glibclib = one(sysroot.glob('**/libc.so.6'), 'libc')
    loadername = 'ld-linux-x86-64.so.2' if a.arch == 'x86_64' else 'ld-linux-aarch64.so.1'
    loader = one(sysroot.glob('**/' + loadername), 'loader')
    # Compile the actual helper implementation into a synthetic protocol test.
    # Pristine NSS keeps this test independent of the inherited-FD launcher.
    testdir = nss_a / 'cmd' / 'seu-nss-export-test'
    testdir.mkdir()
    test_source = HELPER.with_name('nss_export_helper_test.c')
    shutil.copyfile(HELPER, testdir / HELPER.name)
    shutil.copyfile(test_source, testdir / test_source.name)
    wrapper = testdir / 'nss_export_helper_assert_test.c'
    wrapper.write_text('#ifdef NDEBUG\n#undef NDEBUG\n#endif\n#include "nss_export_helper_test.c"\n')
    (testdir / 'manifest.mn').write_text('CORE_DEPTH = ../..\nMODULE = nss\nCSRCS = nss_export_helper_assert_test.c\nREQUIRES = dbm seccmd\nPROGRAM = seu-nss-export-test\n')
    shutil.copyfile(program / 'Makefile', testdir / 'Makefile')
    test_build = run([make, '-j2', 'USE_STATIC_LIBS=1'], testdir, nenv, capture=True)
    (work / 'nss-helper-test-build.log').write_text(test_build)
    test_exe = one((pristine / 'dist').glob('*/bin/seu-nss-export-test'), 'helper test')
    test_libdir = one((pristine / 'dist').glob('*/lib'), 'pristine test libdir')
    test_paths = ':'.join(str(path) for path in (test_libdir, sqlite_prefix / 'lib', glibclib.parent))
    test_env = {'HOME': str(work), 'PATH': env['PATH'], 'LC_ALL': 'C', 'TZ': 'UTC'}
    test_result = subprocess.run(
        [str(loader), '--inhibit-cache', '--library-path', test_paths, str(test_exe)],
        env=test_env, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, timeout=60)
    (work / 'nss-helper-test-receipt.json').write_text(json.dumps({
        'format': 1, 'status': 'passed', 'exit_code': test_result.returncode,
        'scope': 'synthetic protected callback protocol, not external-token or UI proof',
        'helper_sha256': sha(HELPER), 'test_sha256': sha(test_source),
        'wrapper_sha256': sha(wrapper), 'executable_sha256': sha(test_exe),
        'producer_sha256': sha(Path(__file__)),
        'nss_source_sha256': PINS['nss'][1], 'toolchain': tools,
        'make_flags': ['-j2', 'USE_STATIC_LIBS=1'],
        'build_log_sha256': sha(work / 'nss-helper-test-build.log'),
        'assertions': 'NDEBUG undefined before including test and assert.h',
        'assertions_enabled': True,
        'cases': ['approval', 'denial', 'invalid-response', 'eof', 'retry-refusal'],
    }, indent=2) + '\n')
    dep_fd = dict(FD)
    dep_fd[loadername] = ALL_FD['loader']
    candidates = {'libc.so.6': glibclib, 'libm.so.6': one(sysroot.glob('**/libm.so.6'), 'libm'),
                  'libsqlite3.so.0': one((sqlite_prefix / 'lib').glob('libsqlite3.so.0'), 'sqlite')}
    for name in FD:
        if name not in candidates and (libdir / name).exists(): candidates[name] = (libdir / name).resolve(strict=True)
    if not set(FD).issubset(candidates):
        raise RuntimeError('incomplete NSS runtime library set')
    entries = [('loader', ALL_FD['loader'], loader, 'loader'), ('nss-export-helper', ALL_FD['nss-export-helper'], helper, 'helper')]
    entries += [(name, FD[name], path, 'library') for name, path in candidates.items()]
    for name, _, src, _ in entries: shutil.copyfile(src, out / name)
    # All private DT_NEEDED edges become fixed inherited descriptor paths. No
    # unlisted SONAME, slash-containing dependency or ambient fallback allowed.
    records = []
    for name, fd, _, kind in entries:
        target = out / name
        run([patchelf, '--remove-rpath', target])
        hdr = run([cfg['readelf']['path'], '-h', target], capture=True)
        expected_machine = 'Advanced Micro Devices X86-64' if a.arch == 'x86_64' else 'AArch64'
        if expected_machine not in hdr: raise RuntimeError(f'wrong ELF machine: {name}')
        tags = run([cfg['readelf']['path'], '-d', target], capture=True)
        if any(tag in tags for tag in ('(RPATH)', '(RUNPATH)', '(AUDIT)', '(DEPAUDIT)', '(FILTER)', '(AUXILIARY)')):
            raise RuntimeError(f'unsafe ELF dynamic tag: {name}')
        needed = dynamic(target, patchelf)
        for soname in needed:
            if soname not in dep_fd or (soname != loadername and soname not in candidates):
                raise RuntimeError(f'unmapped DT_NEEDED {name}: {soname}')
            run([patchelf, '--replace-needed', soname, f'/proc/self/fd/{dep_fd[soname]}', target])
        after = dynamic(target, patchelf)
        if after != [f'/proc/self/fd/{dep_fd[n]}' for n in needed]: raise RuntimeError('DT_NEEDED rewrite mismatch')
        soname = None
        if kind in ('library', 'loader'): soname = run([patchelf, '--print-soname', target], capture=True).strip() or None
        records.append({'id': name, 'fd': fd, 'size': target.stat().st_size, 'sha256': sha(target),
                        'soname': soname, 'kind': kind, 'needed': [dep_fd[n] for n in needed], 'check_for': None})
    # shlibsign uses no actual user credentials; this signs only final synthetic
    # build outputs. The pristine tool uses its own matched build-time runtime.
    signenv = dict(env, LD_LIBRARY_PATH=str(one((pristine / 'dist').glob('*/lib'), 'pristine libdir')) + ':' + str(sqlite_prefix / 'lib'))
    for lib, (check, fd) in CHECKS.items():
        if lib not in candidates: continue
        run([sign, '-i', out / lib, '-o', out / check, '-V'], env=signenv)
        file = out / check
        if not file.is_file() or file.stat().st_size == 0: raise RuntimeError('missing integrity output')
        # The pristine verifier reads the written .chk and final transformed DSO.
        if run([checktool, out / lib], env=signenv, capture=True).strip() != 'SUCCESS':
            raise RuntimeError('final DSO integrity check failed')
        records.append({'id': check, 'fd': fd, 'size': file.stat().st_size, 'sha256': sha(file),
                        'soname': None, 'kind': 'integrity', 'needed': [], 'check_for': lib})
    source = {name: f'{filename}|{url}|sha256:{digest}' for name, (filename, digest, url) in PINS.items()}
    nspr = CONTRACT['sources']['nspr']
    source['nspr'] = f"{nspr['version']}|included-in-{nspr['contained_in']}|sha256:{nspr['sha256']}"
    source['patch_sha256'] = sha(HERE / 'patch_nss.py')
    source['helper_sha256'] = sha(HELPER)
    source['producer_sha256'] = sha(Path(__file__))
    source['contract_sha256'] = sha(HERE / 'contract.json')
    abi = {'target': a.arch + '-unknown-linux-gnu', 'kernel_min': a.kernel_min,
           'glibc': '2.39-5-ge0910f1d3278f05439fb434ee528fc9be1b6bd5e',
           'flags': 'BUILD_OPT=1 USE_64=1 USE_STATIC_LIBS=1 GLIBC_CFLAGS=-g,-O2,-U_FORTIFY_SOURCE GLIBC_CONFIG=--without-selinux SYSROOT_CRT_PREFIX=1'}
    (out / 'licenses').mkdir()
    notice_specs = [
        ('nss', nss_a / 'COPYING', 'nss.txt', PINS['nss'][2] + '#nss/COPYING'),
        ('nspr', pristine / 'nspr' / 'LICENSE', 'nspr.txt', PINS['nss'][2] + '#nspr/LICENSE'),
        ('glibc-copying', glibc / 'COPYING', 'glibc-copying.txt', PINS['glibc'][2] + '#COPYING'),
        ('glibc-lgpl', glibc / 'COPYING.LIB', 'glibc-lgpl.txt', PINS['glibc'][2] + '#COPYING.LIB'),
        ('glibc-notices', glibc / 'LICENSES', 'glibc-notices.txt', PINS['glibc'][2] + '#LICENSES'),
        ('sqlite', sqlite / 'tea' / 'license.terms', 'sqlite.txt', PINS['sqlite'][2] + '#tea/license.terms'),
    ]
    notices = []
    for ident, original, name, url in notice_specs:
        target = out / 'licenses' / name
        shutil.copyfile(original, target)
        notices.append({'id': ident, 'path': 'licenses/' + name, 'sha256': sha(target), 'url': url})
    (out / 'manifest.json').write_text(json.dumps({'format': 1, 'arch': a.arch, 'source': source,
        'toolchain': tools, 'abi': abi, 'artifacts': sorted(records, key=lambda r: r['fd']),
        'notices': notices}, indent=2) + '\n')

if __name__ == '__main__': main()
