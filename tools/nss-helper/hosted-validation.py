#!/usr/bin/env python3
"""Run the real pinned producer and standalone production launcher on a hosted Linux runner."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import shlex
import subprocess
import tarfile
import produce

ROOT = Path(__file__).resolve().parents[2]
SHELL = ROOT / 'crates/shell'
HERE = Path(__file__).resolve().parent


def run(args, *, cwd=None, env=None):
    subprocess.run([str(arg) for arg in args], cwd=cwd, env=env, check=True)


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def executable(name):
    found = shutil.which(name)
    if not found:
        raise RuntimeError(f'required tool missing: {name}')
    return Path(found).resolve(strict=True)


def one(paths, label):
    paths = list(paths)
    if len(paths) != 1:
        raise RuntimeError(f'expected exactly one {label}: {paths}')
    return paths[0].resolve(strict=True)


def main():
    p = argparse.ArgumentParser()
    for name in ('inputs', 'patchelf', 'work', 'output', 'results', 'fixture-setup'):
        p.add_argument('--' + name, type=Path, required=True)
    p.add_argument('--image-digest', required=True)
    a = p.parse_args()
    inputs, results = a.inputs.resolve(strict=True), a.results.resolve(strict=True)
    rust_identity = {}
    for name, version_args in (('rustc', ['-vV']), ('cargo', ['-V'])):
        actual = Path(subprocess.check_output(['rustup', 'which', name], text=True).strip()).resolve(strict=True)
        version = subprocess.check_output([str(actual), *version_args], text=True)
        if not version.startswith(name + ' 1.95.0 '):
            raise RuntimeError(f'unexpected {name} version: {version.splitlines()[0]}')
        rust_identity[name] = {'path': str(actual), 'sha256': digest(actual), 'version': version}
    (results / 'rust-toolchain.json').write_text(json.dumps(rust_identity, indent=2) + '\n')
    pins = json.loads((HERE / 'contract.json').read_text())['sources']
    cc = executable('gcc')
    linker = subprocess.check_output([str(cc), '-print-prog-name=ld'], text=True).strip()
    linker = Path(linker).resolve(strict=True) if '/' in linker else executable(linker)
    if linker != executable('ld'):
        raise RuntimeError('CC-selected linker does not match recorded linker')
    tools = {'cc': cc, 'cxx': executable('g++'), 'ld': linker, 'make': executable('make'),
             'patchelf': a.patchelf.resolve(strict=True), 'readelf': executable('readelf')}
    cfg = {'image_digest': a.image_digest}
    cfg.update({key: {'path': str(path), 'sha256': digest(path)} for key, path in tools.items()})
    headers = a.work.resolve().parent / 'nss-kernel-headers'
    if headers.exists():
        raise RuntimeError('kernel header staging path already exists')
    headers.mkdir()
    for source, name in [('/usr/include/linux', 'linux'), ('/usr/include/asm-generic', 'asm-generic'),
                         ('/usr/include/x86_64-linux-gnu/asm', 'asm')]:
        shutil.copytree(source, headers / name)
    cfg['kernel_headers'] = {'path': str(headers), 'sha256': produce.tree_sha(headers)}
    config = results / 'toolchain.json'
    config.write_text(json.dumps(cfg, indent=2) + '\n')
    run(['python3', HERE / 'produce.py', '--nss', inputs / pins['nss']['filename'],
         '--sqlite', inputs / pins['sqlite']['filename'], '--glibc', inputs / pins['glibc']['filename'],
         '--toolchain', config, '--work', a.work, '--output', a.output,
         '--arch', 'x86_64', '--kernel-min', '3.2.0'])
    callback_receipt_path = a.work / 'nss-helper-test-receipt.json'
    callback_receipt = json.loads(callback_receipt_path.read_text())
    if (callback_receipt.get('format') != 1 or callback_receipt.get('status') != 'passed'
            or callback_receipt.get('exit_code') != 0 or callback_receipt.get('assertions_enabled') is not True
            or callback_receipt.get('helper_sha256') != digest(SHELL / 'src/host/d2/pdf/crypto/nss_export_helper.c')
            or callback_receipt.get('test_sha256') != digest(SHELL / 'src/host/d2/pdf/crypto/nss_export_helper_test.c')
            or callback_receipt.get('producer_sha256') != digest(HERE / 'produce.py')
            or callback_receipt.get('build_log_sha256') != digest(a.work / 'nss-helper-test-build.log')
            or callback_receipt.get('assertions') != 'NDEBUG undefined before including test and assert.h'):
        raise RuntimeError('actual helper callback test receipt is invalid')
    shutil.copyfile(callback_receipt_path, results / callback_receipt_path.name)
    shutil.copyfile(a.work / 'nss-helper-test-build.log', results / 'nss-helper-test-build.log')
    pristine_root = one((a.work / 'nss-pristine').iterdir(), 'pristine NSS source root')
    run(['python3', HERE / 'test_patch.py', '--source', pristine_root / 'nss',
         '--scratch', a.work.resolve().parent / 'nss-patch-regression'])

    scratch = a.work.resolve().parent / 'nss-harness'
    if scratch.exists():
        raise RuntimeError('harness output already exists')
    crate = scratch / 'crates/shell'
    (crate / 'src').mkdir(parents=True)
    (scratch / 'tools/nss-helper').mkdir(parents=True)
    for name in ('nss_manifest.rs', 'nss_build.rs'):
        shutil.copyfile(SHELL / name, crate / name)
    worker = SHELL / 'src/host/d2/pdf/crypto/worker'
    for name in ('nss_bundle.rs', 'nss_exchange.rs'):
        shutil.copyfile(worker / name, crate / 'src' / name)
    shutil.copyfile(SHELL / 'src/signer_child_guard.rs', crate / 'src/signer_child_guard.rs')
    helper = crate / 'src/host/d2/pdf/crypto/nss_export_helper.c'
    helper.parent.mkdir(parents=True)
    shutil.copyfile(SHELL / 'src/host/d2/pdf/crypto/nss_export_helper.c', helper)
    shutil.copyfile(HERE / 'launcher.rs', crate / 'src/main.rs')
    for name in ('contract.json', 'patch_nss.py', 'produce.py'):
        shutil.copyfile(HERE / name, scratch / 'tools/nss-helper' / name)
    (crate / 'Cargo.toml').write_text('[package]\nname="nss-launcher-fixture"\nversion="0.1.0"\nedition="2024"\n[workspace]\n[dependencies]\nserde_json="=1.0.154"\nsha2="=0.10.9"\nlibc="=0.2.191"\nzeroize="=1.9.1"\n[build-dependencies]\nserde_json="=1.0.154"\nsha2="=0.10.9"\n')
    (crate / 'build.rs').write_text('mod nss_manifest;\nmod nss_build;\nfn main() { println!("cargo:rerun-if-env-changed=NSS_BUNDLE_DIR"); let dir = std::env::var_os("NSS_BUNDLE_DIR").unwrap(); nss_build::embed_nss(std::path::Path::new(&dir)).unwrap(); }\n')
    env = dict(os.environ, NSS_BUNDLE_DIR=str(a.output.resolve(strict=True)), CARGO_TARGET_DIR=str(scratch / 'target'),
               RUSTC=rust_identity['rustc']['path'])
    cargo = rust_identity['cargo']['path']
    run([cargo, 'test', '--manifest-path', crate / 'Cargo.toml'], env=env)
    rejected = scratch / 'tampered-build-bundle'
    shutil.copytree(a.output, rejected)
    victim = rejected / 'libnssutil3.so'
    with victim.open('r+b') as f:
        first = f.read(1)
        f.seek(0)
        f.write(bytes([first[0] ^ 1]))
    bad_env = dict(env, NSS_BUNDLE_DIR=str(rejected))
    bad = subprocess.run([cargo, 'build', '--manifest-path', str(crate / 'Cargo.toml')],
                         env=bad_env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    (results / 'tampered-build-validation.log').write_text(bad.stdout)
    if bad.returncode == 0 or 'size or digest mismatch' not in bad.stdout:
        raise RuntimeError('product manifest validation did not reject the tampered artifact')
    run([cargo, 'rustc', '--manifest-path', crate / 'Cargo.toml',
         '--bin', 'nss-launcher-fixture', '--', '--cfg', 'test'], env=env)
    (results / 'harness-Cargo.lock').write_bytes((crate / 'Cargo.lock').read_bytes())
    softhsm_source = inputs / 'softhsm-2.6.1.tar.gz'
    softhsm_digest = '61249473054bcd1811519ef9a989a880a7bdcc36d317c9c25457fc614df475f2'
    if digest(softhsm_source) != softhsm_digest:
        raise RuntimeError('SoftHSM fixture source digest mismatch')
    softhsm_tree = scratch / 'softhsm-source'
    softhsm_tree.mkdir()
    with tarfile.open(softhsm_source, 'r:gz') as archive:
        archive.extractall(softhsm_tree, filter='data')
    softhsm_root = one(softhsm_tree.iterdir(), 'SoftHSM source root')
    softhsm_prefix = scratch / 'softhsm'
    crypto = Path(subprocess.check_output(
        [str(cc), '-print-file-name=libcrypto.so.3'], text=True).strip()).resolve(strict=True)
    # The isolated fixture and product loaders inhibit ld.so.cache. Give the
    # synthetic provider a transitive search path for its host OpenSSL runtime.
    softhsm_env = dict(env, LDFLAGS=shlex.join([
        '-Wl,--disable-new-dtags,-rpath,' + str(crypto.parent),
    ]))
    run([softhsm_root / 'configure', '--prefix=' + str(softhsm_prefix),
         '--disable-p11-kit', '--with-crypto-backend=openssl'], cwd=softhsm_root, env=softhsm_env)
    run(['make', '-j2'], cwd=softhsm_root, env=softhsm_env)
    run(['make', 'install'], cwd=softhsm_root, env=softhsm_env)
    module = one(softhsm_prefix.glob('lib/softhsm/libsofthsm2.so'), 'SoftHSM fixture module')
    env.update(SOFTHSM_MODULE=str(module), SOFTHSM_MODULE_SHA256=digest(module),
               SOFTHSM_SOURCE_SHA256=softhsm_digest, SOFTHSM_SOURCE_ARCHIVE=str(softhsm_source))
    install = scratch / 'install/usr'
    (install / 'bin').mkdir(parents=True)
    (install / 'lib/SeuOffice').mkdir(parents=True)
    shutil.copyfile(scratch / 'target/debug/nss-launcher-fixture', install / 'bin/nss-launcher')
    (install / 'bin/nss-launcher').chmod(0o755)
    shutil.copytree(a.output, install / 'lib/SeuOffice/nss')
    for mode in ('--test-sigchld-ignore', '--test-sigchld-no-wait'):
        run([install / 'bin/nss-launcher', mode], env=env)
    pristine = pristine_root / 'dist'
    # Fixture tools were linked against our built sysroot, not the container's
    # libc. Keep that loader/library selection local to these three tools.
    tool_commands = scratch / 'fixture-tools'
    tool_commands.mkdir()
    glibc_lib = one((a.work / 'sysroot').glob('**/libc.so.6'), 'fixture glibc').parent
    fixture_loader = one((a.work / 'sysroot').glob('**/ld-linux-x86-64.so.2'), 'fixture loader')
    libdir = one(pristine.glob('*/lib'), 'pristine NSS library directory')
    nspr_include = one(pristine.glob('*/include'), 'pristine NSPR include directory')
    nss_include = (pristine / 'public/nss').resolve(strict=True)
    env.update(NSS_TEST_LIBDIR=str(libdir),
               NSS_SQLITE_LIBDIR=str((a.work / 'sqlite-prefix/lib').resolve(strict=True)),
               NSS_CC=str(cc), NSS_TEST_INCLUDE_DIRS=f'{nss_include}:{nspr_include}')
    tool_lib = scratch / 'fixture-libs'
    # NSS dist exports point back into the build tree. The relocated fixture
    # directory needs their bytes, not links relative to the old dist path.
    shutil.copytree(libdir, tool_lib, symlinks=False)
    sqlite = (a.work / 'sqlite-prefix/lib/libsqlite3.so.0').resolve(strict=True)
    shutil.copyfile(sqlite, tool_lib / 'libsqlite3.so.0')
    fixture_library_path = os.pathsep.join(str(path) for path in (
        tool_lib, (a.work / 'sqlite-prefix/lib').resolve(strict=True), glibc_lib,
    ))
    fixture_tools = []
    for name in ('certutil', 'modutil', 'pk12util'):
        program = one(pristine.glob('*/bin/' + name), 'pristine ' + name)
        wrapper = tool_commands / name
        wrapper.write_text('#!/bin/sh\nexec ' + shlex.join([
            str(fixture_loader), '--inhibit-cache', '--library-path',
            fixture_library_path, str(program),
        ]) + ' "$@"\n')
        wrapper.chmod(0o700)
        fixture_tools.append({'name': name, 'binary_sha256': digest(program), 'wrapper_sha256': digest(wrapper)})
    certutil = tool_commands / 'certutil'
    env['NSS_CERTUTIL'] = str(certutil)
    (results / 'fixture-tools.json').write_text(json.dumps({
        'scope': 'hosted synthetic fixture setup only',
        'loader_sha256': digest(fixture_loader), 'tools': fixture_tools,
    }, indent=2) + '\n')
    receipt = scratch / 'install/nss-proof-receipt.json'
    run(['python3', HERE / 'hosted-proof.py', '--bundle', install / 'lib/SeuOffice/nss',
         '--scratch', scratch / 'install', '--launcher', install / 'bin/nss-launcher',
         '--certutil', certutil, '--openssl', executable('openssl'),
         '--tool-lib-dir', tool_lib, '--fixture-setup', a.fixture_setup.resolve(strict=True),
         '--receipt', receipt], env=env)
    shutil.copyfile(receipt, results / 'nss-proof-receipt.json')
    shutil.copyfile(a.output / 'manifest.json', results / 'manifest.json')


if __name__ == '__main__':
    main()
