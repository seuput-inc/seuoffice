#!/usr/bin/env python3
"""Hosted-only official CEF translator and matched cef-rs generation; never builds libcef."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import tarfile
import urllib.request

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
CONTRACT = json.loads((HERE / 'contract.json').read_text())
PATCH_ROOT = Path('crates/vendor/tauri-runtime-cef/cef-source')


def digest(path, algorithm='sha256'):
    value = hashlib.new(algorithm)
    with Path(path).open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def validate_freeze(value, root):
    if value.get('format') != 1 or value.get('frozen') is not True:
        raise ValueError('a reviewed final frozen patch stack is required')
    if value.get('contract_sha256') != digest(HERE / 'contract.json'):
        raise ValueError('frozen stack belongs to another generation contract')
    for tree in ('cef', 'chromium'):
        patches = value.get(tree + '_patches', [])
        if [item.get('id') for item in patches] != CONTRACT[tree + '_patch_order']:
            raise ValueError(f'incorrect {tree} patch order')
        for item in patches:
            relative = PurePosixPath(item.get('path', ''))
            if relative.is_absolute() or '..' in relative.parts or relative.parent != PurePosixPath(str(PATCH_ROOT)):
                raise ValueError('patch is outside the assigned source directory')
            if not relative.name.startswith(item['id'] + '-') or not relative.name.endswith('.patch'):
                raise ValueError('patch filename and identity disagree')
            path = root / str(relative)
            if path.is_symlink() or not re.fullmatch(r'[0-9a-f]{64}', item.get('sha256', '')) or digest(path) != item['sha256']:
                raise ValueError(f'frozen patch digest mismatch: {relative}')
    symbols = value.get('symbols', {})
    for stage in ('cpp', 'capi', 'rust'):
        names = symbols.get(stage, [])
        if not isinstance(names, list) or not names or len(names) != len(set(names)):
            raise ValueError(f'explicit unique final {stage} symbols are required')
        if not all(isinstance(name, str) and re.fullmatch(r'[A-Za-z_][A-Za-z_0-9]*', name) for name in names):
            raise ValueError('invalid symbol assertion')
    return value


def download(pin, destination):
    with urllib.request.urlopen(pin['url'], timeout=120) as source, destination.open('xb') as output:
        shutil.copyfileobj(source, output)
    if 'size' in pin and destination.stat().st_size != pin['size']:
        raise ValueError(f'archive size mismatch: {destination.name}')
    for algorithm in ('sha256', 'sha1'):
        if algorithm in pin and digest(destination, algorithm) != pin[algorithm]:
            raise ValueError(f'archive {algorithm} mismatch: {destination.name}')


def extract_support(archive, cef):
    wanted = set(CONTRACT['support_headers'])
    found = {}
    with tarfile.open(archive, 'r:bz2') as source:
        for member in source:
            parts = PurePosixPath(member.name).parts
            if len(parts) != 3 or parts[1] != 'include' or parts[2] not in wanted:
                continue
            name = parts[2]
            if name in found or not member.isfile():
                raise ValueError('duplicate or non-regular support header')
            target = cef / 'include' / name
            if target.exists():
                raise ValueError(f'refusing to overwrite source-generated header: {name}')
            with source.extractfile(member) as data, target.open('xb') as output:
                shutil.copyfileobj(data, output)
            found[name] = digest(target)
    if set(found) != wanted:
        raise ValueError(f'missing upstream support headers: {sorted(wanted - set(found))}')
    return found


def require_symbols(names, paths, stage):
    text = '\n'.join(path.read_text() for path in paths)
    missing = [name for name in names if not re.search(r'\b' + re.escape(name) + r'\b', text)]
    if missing:
        raise ValueError(f'{stage} output lacks frozen symbols: {missing}')


def inventory(root):
    return {str(path.relative_to(root)): digest(path) for path in sorted(root.rglob('*'))
            if path.is_file() and not path.is_symlink() and '.git' not in path.parts}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--freeze', type=Path, required=True)
    parser.add_argument('--freeze-sha256', required=True)
    parser.add_argument('--work', type=Path, required=True)
    parser.add_argument('--results', type=Path, required=True)
    parser.add_argument('--image', required=True)
    args = parser.parse_args()
    if args.image != CONTRACT['image']:
        raise ValueError('hosted OCI image differs from pinned generation image')
    if not re.fullmatch(r'[0-9a-f]{64}', args.freeze_sha256) or digest(args.freeze) != args.freeze_sha256:
        raise ValueError('reviewed frozen-stack receipt mismatch')
    freeze = validate_freeze(json.loads(args.freeze.read_text()), ROOT)
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=False)
    results = args.results.resolve()
    results.mkdir(parents=True, exist_ok=True)
    write_json(results / 'contract.json', CONTRACT)
    shutil.copyfile(args.freeze, results / 'frozen-stack.json')
    write_json(results / 'source-files.json', inventory(HERE))
    env = {'PATH': os.environ['PATH'],
           'RUSTUP_HOME': os.environ.get('RUSTUP_HOME', str(Path.home() / '.rustup'))}
    env.update(SOURCE_DATE_EPOCH='1792108800', TZ='UTC', LC_ALL='C.UTF-8',
               CARGO_TARGET_DIR=str(work / 'target'), CARGO_HOME=str(work / 'cargo-home'))
    home = work / 'home'
    home.mkdir()
    env['HOME'] = str(home)
    commands = []

    def run(command, cwd=None, capture=False):
        command = [str(part) for part in command]
        commands.append({'argv': command, 'cwd': str(cwd or work)})
        write_json(results / 'commands.json', commands)
        if capture:
            return subprocess.check_output(command, cwd=cwd or work, env=env, text=True).strip()
        subprocess.run(command, cwd=cwd or work, env=env, check=True)

    def clone(url, revision, destination):
        destination.mkdir()
        run(['git', 'init', destination])
        run(['git', 'remote', 'add', 'origin', url], destination)
        run(['git', 'fetch', '--depth=1', 'origin', revision], destination)
        run(['git', 'checkout', '--detach', 'FETCH_HEAD'], destination)
        if run(['git', 'rev-parse', 'HEAD'], destination, True) != revision:
            raise ValueError('source checkout revision mismatch')

    tools = {}
    for name in ('rustc', 'cargo'):
        path = Path(run(['rustup', 'which', name], capture=True)).resolve(strict=True)
        version = run([path, '-vV' if name == 'rustc' else '-V'], capture=True)
        if not version.startswith(name + ' ' + CONTRACT['rust'] + ' '):
            raise ValueError(f'unexpected {name} toolchain')
        tools[name] = {'path': str(path), 'sha256': digest(path), 'version': version}
    env['RUSTC'] = tools['rustc']['path']
    write_json(results / 'tools.json', tools)
    # Install only hash-pinned formatter/libclang packages into an isolated prefix.
    prefix = work / 'llvm-support'
    prefix.mkdir()
    for index, package in enumerate(CONTRACT['packages']):
        archive = work / f'package-{index}.deb'
        download(dict(package, url=CONTRACT['ubuntu_snapshot'] + package['path']), archive)
        run(['dpkg-deb', '-x', archive, prefix])
    binaries = work / 'bin'
    binaries.mkdir()
    (binaries / 'clang-format').symlink_to(prefix / 'usr/bin/clang-format-18')
    env['PATH'] = str(binaries) + ':' + env['PATH']
    env['LD_LIBRARY_PATH'] = str(prefix / 'usr/lib/x86_64-linux-gnu')
    env['LIBCLANG_PATH'] = str(prefix / 'usr/lib/x86_64-linux-gnu')
    tools['clang-format'] = {'version': run(['clang-format', '--version'], capture=True),
                             'sha256': digest((binaries / 'clang-format').resolve(strict=True))}
    tools['libclang'] = {'sha256': digest(prefix / 'usr/lib/x86_64-linux-gnu/libclang-18.so.18')}
    src = work / 'src'
    src.mkdir()
    cef = src / 'cef'
    clone('https://github.com/chromiumembedded/cef.git', CONTRACT['cef'], cef)
    for relative in CONTRACT['chromium_inputs']:
        target = src / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        url = f"https://raw.githubusercontent.com/chromium/chromium/{CONTRACT['chromium']}/{relative}"
        with urllib.request.urlopen(url, timeout=120) as source, target.open('xb') as output:
            shutil.copyfileobj(source, output)
    write_json(results / 'chromium-inputs.json', {name: digest(src / name) for name in CONTRACT['chromium_inputs']})
    # Chromium patches are frozen inputs, not applied to these five hash-only prerequisites.
    # The Chromium implementation requires a separate full source/build proof.
    for patch in freeze['cef_patches']:
        path = ROOT / patch['path']
        run(['git', 'apply', '--check', path], cef)
        run(['git', 'apply', path], cef)
    require_symbols(freeze['symbols']['cpp'], list((cef / 'include').rglob('*.h')), 'patched C++')
    clang_archive = work / 'clang.tar.xz'
    download(CONTRACT['clang'], clang_archive)
    clang_root = src / 'third_party/llvm-build/Release+Asserts'
    clang_root.mkdir(parents=True)
    with tarfile.open(clang_archive, 'r:xz') as source:
        source.extractall(clang_root, filter='data')
    tools['clang'] = {'version': run([clang_root / 'bin/clang', '--version'], capture=True),
                      'sha256': digest(clang_root / 'bin/clang')}
    write_json(results / 'tools.json', tools)
    before = json.loads((cef / 'cef_api_versions.json').read_text())
    if before['last'] != '15400':
        raise ValueError('upstream tracked API has changed')
    # This is the single official translator/version allocation for the entire private stack.
    run(['python3', 'tools/version_manager.py', '-a'], cef)
    versions = json.loads((cef / 'cef_api_versions.json').read_text())
    if versions['last'] != CONTRACT['api']:
        raise ValueError('official translator did not allocate private API15401')
    hashes = versions['hashes'][CONTRACT['api']]
    for platform in ('linux', 'mac', 'windows'):
        if not re.fullmatch(r'[0-9a-f]{40}', hashes.get(platform, '')):
            raise ValueError(f'private {platform} API hash missing')
        if hashes[platform] == before['hashes']['15400'][platform]:
            raise ValueError(f'private {platform} API hash unexpectedly equals stock API')
    run(['python3', 'tools/make_api_versions_header.py', 'include/cef_api_versions.h',
         'libcef_dll/cef_api_versions.inc', 'cef_api_versions.json', 'cef_api_untracked.json'], cef)
    require_symbols(freeze['symbols']['capi'], list((cef / 'include/capi').rglob('*.h')), 'generated CAPI')
    support = work / 'support.tar.bz2'
    download(CONTRACT['support_archive'], support)
    support_hashes = extract_support(support, cef)
    write_json(results / 'support-headers.json', support_hashes)
    generator = work / 'cef-rs'
    clone('https://github.com/tauri-apps/cef-rs.git', CONTRACT['cef_rs'], generator)
    lock = generator / 'Cargo.lock'
    if lock.exists():
        raise ValueError('generator unexpectedly gained an upstream Cargo.lock; review pin contract')
    cargo = tools['cargo']['path']
    # Upstream has no tracked lock. Preserve the first actual resolution; every subsequent
    # command uses --locked. The receipt does not claim an independently pre-reviewed lock.
    run([cargo, 'generate-lockfile'], generator)
    shutil.copyfile(lock, results / 'generator-Cargo.lock')
    env['CEF_PATH_linux_x86_64'] = str(cef)
    env['BINDGEN_EXTRA_CLANG_ARGS'] = '-DCEF_API_VERSION=' + CONTRACT['api']
    run([cargo, 'run', '--locked', '-p', 'update-bindings', '--', '--bindgen', '--target', CONTRACT['target']], generator)
    bindings = [generator / directory / 'src/bindings/x86_64_unknown_linux_gnu.rs' for directory in ('sys', 'cef')]
    require_symbols(freeze['symbols']['capi'], [bindings[0]], 'generated Rust sys')
    require_symbols(freeze['symbols']['rust'], [bindings[1]], 'generated Rust safe')
    if not re.search(r'CEF_API_VERSION\s*:\s*[^=]+?=\s*15401\s*;', bindings[0].read_text()):
        raise ValueError('generated sys bindings did not select API15401')
    generated = results / 'generated'
    generated.mkdir()
    # Preserve complete official output, including customized C++ wrappers, not just symbol counts.
    for relative in ('include', 'libcef_dll', 'cef_api_versions.json', 'cef_api_untracked.json',
                     'cef_paths.gypi', 'VERSION.stamp', '.gitignore'):
        source = cef / relative
        target = generated / 'cef' / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        if source.is_dir():
            shutil.copytree(source, target)
        else:
            shutil.copyfile(source, target)
    for relative in ('sys/src/bindings/x86_64_unknown_linux_gnu.rs',
                     'cef/src/bindings/x86_64_unknown_linux_gnu.rs',
                     'cef/src/resources/x86_64_unknown_linux_gnu.rs'):
        target = generated / 'cef-rs' / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(generator / relative, target)
    write_json(results / 'generated-files.json', inventory(generated))
    receipt = {'format': 1, 'status': 'generated', 'api': CONTRACT['api'], 'platform_hashes': hashes,
               'target': CONTRACT['target'], 'contract_sha256': digest(HERE / 'contract.json'),
               'freeze_sha256': args.freeze_sha256, 'generator_lock_sha256': digest(lock),
               'generated_inventory_sha256': digest(results / 'generated-files.json'),
               'scope': 'official translator, CAPI/wrappers, Linux Rust sys/safe/resources generation only',
               'not_verified': ['CEF C++ compilation', 'private libcef binary', 'runtime ABI execution',
                                'macOS/Windows Rust bindings', 'full Chromium patch composition',
                                'gclient_hook/GN/GRIT build'],
               'version_metadata': 'exact upstream archive support header; shallow source VERSION.stamp is not private binary provenance'}
    write_json(results / 'receipt.json', receipt)


if __name__ == '__main__':
    main()
