#!/usr/bin/env python3
"""Capture explicitly reviewed final patch hashes and ABI symbols; does not generate or dispatch."""
import argparse
import json
from pathlib import Path
from generate import CONTRACT, HERE, PATCH_ROOT, ROOT, digest, validate_freeze, write_json


def capture(symbols, root=ROOT):
    value = {'format': 1, 'frozen': True, 'contract_sha256': digest(HERE / 'contract.json'),
             'symbols': symbols}
    for tree in ('cef', 'chromium'):
        patches = []
        for identity in CONTRACT[tree + '_patch_order']:
            candidates = sorted((root / PATCH_ROOT).glob(identity + '-*.patch'))
            if len(candidates) != 1:
                raise ValueError(f'expected exactly one {identity} patch')
            path = candidates[0]
            patches.append({'id': identity, 'path': str(path.relative_to(root)), 'sha256': digest(path)})
        value[tree + '_patches'] = patches
    return validate_freeze(value, root)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--symbols', type=Path, required=True,
                        help='reviewed JSON object with nonempty cpp/capi/rust symbol arrays covering all final extensions')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--confirm-final-stack', action='store_true', required=True)
    args = parser.parse_args()
    value = capture(json.loads(args.symbols.read_text()))
    if args.output.exists():
        raise ValueError('refusing to replace an existing freeze receipt')
    write_json(args.output, value)
    print(digest(args.output))


if __name__ == '__main__':
    main()
