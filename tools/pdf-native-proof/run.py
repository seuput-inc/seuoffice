#!/usr/bin/env python3
"""Build actual WIP codec sources and run the pdf-lib differential on hosted machines."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
try:
    import tomllib
except ImportError:
    tomllib = None

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
SOURCES = [
    'crates/shell/src/host/d2/pdf/crypto/pdf_lib.rs',
    'crates/shell/src/host/d2/pdf/crypto/pdf_lib/assembly.rs',
    'crates/shell/src/host/d2/pdf/crypto/pdf_lib/graph.rs',
    'crates/shell/src/host/d2/pdf/crypto/pdf_lib/revision.rs',
    'crates/shell/src/host/d2/pdf/crypto/pdf_lib/stream.rs',
    'crates/shell/src/host/d2/pdf/crypto/pdf_lib/stream/flate.rs',
    'crates/shell/src/host/d2/pdf/crypto/pdf_lib/upng.rs',
    'crates/shell/src/host/d2/pdf/crypto/pdf_lib/upng/inflate.rs',
    'crates/shell/src/host/d2/pdf/pdf_codec.rs',
    'crates/shell/src/host/d2/pdf/pdf_codec/predecrypt.rs',
    'crates/shell/src/host/d2/pdf/pdf_codec/transform.rs',
    'crates/shell/src/host/d2/pdf/native_engine/signatures/pdf.rs',
    'crates/shell-contract/src/pdf_sign_options.rs',
    'apps/pdf/src/main/pdf-sign.ts', 'apps/pdf/src/main/pdf-signatures.ts',
    'apps/pdf/src/main/forge-native.ts', 'package-lock.json', 'crates/Cargo.toml',
    'crates/shell/Cargo.lock', 'crates/shell/Cargo.toml',
    'tools/pdf-native-proof/build.rs', 'tools/pdf-native-proof/main.rs',
    'tools/pdf-native-proof/compare.ts', 'tools/pdf-native-proof/run.py',
    'tools/pdf-native-proof/dependencies.cjs',
    'tools/pdf-native-proof/test_static.py', 'tools/pdf-native-proof/test-static.mjs',
]


def manifest():
    files = {}
    for name in SOURCES:
        path = ROOT / name
        if not path.is_file():
            raise RuntimeError(f'missing proof input: {name}')
        files[name] = hashlib.sha256(path.read_bytes()).hexdigest()
    for directory in ('crates/jscompat', 'crates/vendor/regress'):
        for path in sorted((ROOT / directory).rglob('*')):
            if path.is_file() and path.suffix in ('.rs', '.toml'):
                files[path.relative_to(ROOT).as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
    return {'source_kind': 'working-tree-wip', 'product_sha': None, 'sha256': files}


def lock_packages(path):
    if tomllib is None:
        raise RuntimeError('Python 3.11 or later is required to inspect Cargo.lock')
    return tomllib.loads(path.read_text())['package']


def identity(package):
    return tuple(package.get(key) for key in ('name', 'version', 'source', 'checksum'))


def locked_version(packages, name):
    versions = {package['version'] for package in packages if package['name'] == name}
    if len(versions) != 1:
        raise RuntimeError(f'production lock has ambiguous {name} versions: {sorted(versions)}')
    return versions.pop()


def materialize_modules(directory):
    """Retain exact source bytes while giving Rust its normal mod.rs layout."""
    directory.mkdir()
    pdf = 'crates/shell/src/host/d2/pdf/'
    modules = {}
    for source in SOURCES:
        if source == pdf + 'pdf_codec.rs':
            target = 'pdf_codec/mod.rs'
        elif source.startswith(pdf + 'pdf_codec/'):
            target = source[len(pdf):]
        elif source == pdf + 'crypto/pdf_lib.rs':
            target = 'pdf_lib/mod.rs'
        elif source.startswith(pdf + 'crypto/pdf_lib/'):
            target = source[len(pdf + 'crypto/'):]
        else:
            continue
        destination = directory / target
        destination.parent.mkdir(parents=True, exist_ok=True)
        content = (ROOT / source).read_bytes()
        with destination.open('xb') as output:
            output.write(content)
        modules[target] = {'source': source, 'sha256': hashlib.sha256(content).hexdigest()}
    return modules


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--scratch', required=True, help='dedicated existing directory outside the checkout')
    parser.add_argument('--static', action='store_true', help='only inspect exact source inputs; not native proof')
    args = parser.parse_args()
    scratch = Path(args.scratch).resolve()
    if not scratch.is_dir() or scratch == ROOT or ROOT in scratch.parents:
        parser.error('--scratch must be an existing directory outside the checkout')
    scratch = Path(tempfile.mkdtemp(prefix='pdf-native-proof-', dir=scratch))
    print(f'Proof artifacts: {scratch}', flush=True)
    env = os.environ.copy()
    for key in ('NODE_OPTIONS', 'NODE_EXTRA_CA_CERTS', 'NODE_USE_SYSTEM_CA', 'NODE_PATH',
                'SSL_CERT_FILE', 'SSL_CERT_DIR', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER'):
        env.pop(key, None)
    env['TMPDIR'] = str(scratch)
    env['TMP'] = str(scratch)
    env['TEMP'] = str(scratch)
    source = manifest()
    evidence = {**source, 'proof_status': 'inventory-only' if args.static else 'incomplete'}
    (scratch / 'wip-source-manifest.json').write_text(json.dumps(evidence, indent=2) + '\n')
    subprocess.run([sys.executable, '-B', str(HERE / 'test_static.py')], cwd=ROOT, env=env, check=True)
    subprocess.run(['node', str(HERE / 'test-static.mjs')], cwd=ROOT, env=env, check=True)
    if args.static:
        print('Static input inventory only; no native codec or consumer executed.')
        return
    production_lock = ROOT / 'crates/shell/Cargo.lock'
    production_packages = lock_packages(production_lock)
    flate_version = locked_version(production_packages, 'flate2')
    json_version = locked_version(production_packages, 'serde_json')
    chrono_version = locked_version(production_packages, 'chrono')
    module_root = scratch / 'modules'
    evidence['module_sources'] = materialize_modules(module_root)
    def js_inventory():
        return subprocess.run(['node', str(HERE / 'dependencies.cjs')], cwd=ROOT, env=env,
                              check=True, capture_output=True, timeout=120).stdout
    js_dependencies = js_inventory()
    (scratch / 'js-dependencies.json').write_bytes(js_dependencies)
    evidence['js_dependencies_sha256'] = hashlib.sha256(js_dependencies).hexdigest()
    evidence['toolchains'] = {
        name: subprocess.run(command, cwd=ROOT, env=env, check=True, capture_output=True,
                             text=True, timeout=30).stdout.strip()
        for name, command in (
            ('rustc', ['rustc', '--version', '--verbose']),
            ('cargo', ['cargo', '--version']),
            ('node', ['node', '--version']),
        )
    }
    def toml_path(path):
        return json.dumps(str(path))
    cargo = f'''[package]
name = "pdf-native-proof"
version = "0.0.0"
edition = "2024"
publish = false
build = {toml_path(HERE / 'build.rs')}
[[bin]]
name = "pdf-native-proof"
path = {toml_path(HERE / 'main.rs')}
[dependencies]
jscompat = {{ path = {toml_path(ROOT / 'crates/jscompat')} }}
flate2 = {{ version = "={flate_version}", features = ["zlib-rs"] }}
serde_json = {{ version = "={json_version}", features = ["preserve_order", "float_roundtrip"] }}
chrono = {{ version = "={chrono_version}", features = ["clock"] }}
[patch.crates-io]
regress = {{ path = {toml_path(ROOT / 'crates/vendor/regress')} }}
[workspace]
'''
    (scratch / 'Cargo.toml').write_text(cargo)
    env['PDF_PROOF_ROOT'] = str(ROOT)
    env['PDF_PROOF_MODULE_ROOT'] = str(module_root)
    env['CARGO_TARGET_DIR'] = str(scratch / 'target')
    env['CARGO_HOME'] = str(scratch / 'cargo-home')
    env['PDF_PROOF_SCRATCH'] = str(scratch)
    # Seed with the product lock, then normalize for this deliberately small
    # workspace. Reject any resolved identity absent from the product lock.
    (scratch / 'Cargo.lock').write_bytes(production_lock.read_bytes())
    subprocess.run(['cargo', 'metadata', '--format-version', '1', '--manifest-path', str(scratch / 'Cargo.toml')],
                   cwd=ROOT, env=env, stdout=subprocess.DEVNULL, check=True, timeout=600)
    allowed = {identity(package) for package in production_packages}
    unexpected = [identity(package) for package in lock_packages(scratch / 'Cargo.lock')
                  if package['name'] != 'pdf-native-proof' and identity(package) not in allowed]
    if unexpected:
        raise RuntimeError(f'proof dependencies differ from production Cargo.lock: {unexpected}')
    subprocess.run(['cargo', 'build', '--locked', '--manifest-path', str(scratch / 'Cargo.toml')],
                   cwd=ROOT, env=env, check=True, timeout=1200)
    executable = scratch / 'target' / 'debug' / ('pdf-native-proof.exe' if os.name == 'nt' else 'pdf-native-proof')
    subprocess.run([str(ROOT / 'node_modules/.bin/tsx'), str(HERE / 'compare.ts'), str(executable)],
                   cwd=ROOT, env=env, check=True, timeout=600)
    if manifest() != source or js_inventory() != js_dependencies:
        raise RuntimeError('proof inputs changed during the native differential; evidence was not accepted')
    evidence['proof_status'] = 'passed'
    evidence['cargo_lock_sha256'] = hashlib.sha256((scratch / 'Cargo.lock').read_bytes()).hexdigest()
    evidence['native_runner_sha256'] = hashlib.sha256(executable.read_bytes()).hexdigest()
    evidence['differential_evidence_sha256'] = hashlib.sha256((scratch / 'differential-evidence.json').read_bytes()).hexdigest()
    (scratch / 'wip-source-manifest.json').write_text(json.dumps(evidence, indent=2) + '\n')
    print('Native codec differential completed; evidence identifies WIP sources, not a product commit.')


if __name__ == '__main__':
    try:
        main()
    except (RuntimeError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        print(f'pdf-native-proof: {error}', file=sys.stderr)
        sys.exit(1)
