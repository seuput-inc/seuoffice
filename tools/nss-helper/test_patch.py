#!/usr/bin/env python3
"""Source-only regression against exact unmodified NSS_3_108_RTM files.

Usage: python3 test_patch.py --source <unpacked-nss-dir> --scratch <new-dir>
No compiler, native execution, user files, network or credential stores.
"""
import argparse
import hashlib
from pathlib import Path
import shutil
import subprocess
import sys

HERE = Path(__file__).resolve().parent
# Raw members of contract.json's SHA-256-pinned release archive, not web-reader
# text (which may include URL/Content-Type headers or normalize final newlines).
HASHES = {
    'lib/pk11wrap/pk11load.c': '98b09be1abe2ce2bb45fcacb996d99b99ec9611a9c352eef631d5ac2c4a084c2',
    'lib/freebl/genload.c': '5444e4233fec5daf87931a4d87a8b597ffd27432d3e2c734ef79805e4c7b2b88',
    'lib/freebl/shvfy.c': 'cab3fe57a3fa81ba0354dab809c2b072b138e4df00e2f88bf7fd3dccbd914724',
    'lib/freebl/loader.c': 'f50ee2e194421d6c0b93dedd7be93105745c395f35cfccf9391eb1eeae477769',
    'lib/freebl/lowhash_vector.c': '62c30e09a9032896d2b7deb7018707708b0d347ac1fdb419053529b9f3961487',
    'lib/softoken/lgglue.c': 'b180693e779ebe3539ddd29a5e6ed39392384de8a53fca5942c9d6e11b6bdc78',
    'lib/softoken/fipstokn.c': '9cfe23e265da3131cfdbab5006c95cdae3e0732a68947167ff15799227971901',
}

def need(condition, message):
    if not condition: raise RuntimeError(message)

def main():
    p = argparse.ArgumentParser(); p.add_argument('--source', required=True, type=Path); p.add_argument('--scratch', required=True, type=Path); a = p.parse_args()
    root = a.source.resolve(strict=True); scratch = a.scratch.resolve()
    if scratch.exists() or root in scratch.parents or scratch in root.parents: raise ValueError('scratch must be new and outside source')
    scratch.mkdir(parents=True)
    for relative, expected in HASHES.items():
        src = root / relative; data = src.read_bytes()
        need(hashlib.sha256(data).hexdigest() == expected, f'not pinned upstream source: {relative}')
        target = scratch / relative; target.parent.mkdir(parents=True, exist_ok=True); target.write_bytes(data)
    subprocess.run([sys.executable, str(HERE / 'patch_nss.py'), str(scratch)], check=True)
    def text(relative): return (scratch / relative).read_text()
    pk11 = text('lib/pk11wrap/pk11load.c')
    freebl = text('lib/freebl/genload.c')
    glue = text('lib/softoken/lgglue.c')
    lowhash = text('lib/freebl/lowhash_vector.c')
    loader = text('lib/freebl/loader.c')
    verify = text('lib/freebl/shvfy.c')
    audit = text('lib/softoken/fipstokn.c')
    need('PR_LoadLibrary("/proc/self/fd/208")' in pk11 and 'PR_LoadLibrary(mod->dllName)' in pk11, 'internal/external loader boundary')
    need('my_shlib_name' not in pk11 and 'softoken_shlib_name' not in pk11, 'dead softoken names')
    need('NameOfThisSharedLib' not in loader and 'freebl_lowhash_getLibraryFilePath' not in lowhash, 'dead freebl symbols')
    need('PRLibSpec libSpec;' in freebl and 'PR_LoadLibraryWithFlags(libSpec,' in freebl and '#include <unistd.h>' in freebl, 'lowhash macro/header contract')
    need('loader_LoadLibInReferenceDir' not in freebl and '#if 0' not in freebl, 'freebl old fallback retained')
    need('sftkdb_LoadFromPath' not in glue and '#if 0' not in glue and 'PR_LoadLibrary("/proc/self/fd/214")' in glue, 'DBM old fallback retained')
    for num in (208, 209, 210, 211, 212, 213, 214, 215): need(f'/proc/self/fd/{num}' in verify or f'/proc/self/fd/{num}' in freebl or f'/proc/self/fd/{num}' in glue, f'missing descriptor {num}')
    need('dlopen(LIBAUDIT_NAME' not in audit and '/usr/lib64/libaudit.so.1' in audit, 'audit OS boundary')
    print('NSS_3_108_RTM: seven original hashes and transformed source boundaries OK')

if __name__ == '__main__': main()
