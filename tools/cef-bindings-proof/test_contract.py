#!/usr/bin/env python3
"""Pure source-contract tests. These do not execute any compiler, translator or CEF binary."""
import copy
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import freeze
import generate


class ContractTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        directory = self.root / generate.PATCH_ROOT
        directory.mkdir(parents=True)
        for tree in ('cef', 'chromium'):
            for identity in generate.CONTRACT[tree + '_patch_order']:
                (directory / (identity + '-fixture.patch')).write_text('synthetic patch input ' + identity)
        self.symbols = {'cpp': ['CefNativeContextEvaluation'],
                        'capi': ['cef_native_context_evaluation_t'],
                        'rust': ['NativeContextEvaluation']}
        self.value = freeze.capture(self.symbols, self.root)

    def test_reviewed_freeze_round_trip(self):
        self.assertEqual(generate.validate_freeze(json.loads(json.dumps(self.value)), self.root), self.value)

    def test_refuses_moving_stack(self):
        self.value['frozen'] = False
        with self.assertRaisesRegex(ValueError, 'final frozen'):
            generate.validate_freeze(self.value, self.root)

    def test_refuses_reordered_or_missing_patch(self):
        for changed in (list(reversed(self.value['cef_patches'])), self.value['cef_patches'][:-1]):
            value = copy.deepcopy(self.value)
            value['cef_patches'] = changed
            with self.assertRaisesRegex(ValueError, 'patch order'):
                generate.validate_freeze(value, self.root)

    def test_refuses_tampered_patch_after_freeze(self):
        (self.root / self.value['cef_patches'][0]['path']).write_text('changed')
        with self.assertRaisesRegex(ValueError, 'digest mismatch'):
            generate.validate_freeze(self.value, self.root)

    def test_refuses_contract_drift(self):
        self.value['contract_sha256'] = '0' * 64
        with self.assertRaisesRegex(ValueError, 'another generation contract'):
            generate.validate_freeze(self.value, self.root)

    def test_refuses_patch_traversal_and_symlink(self):
        value = copy.deepcopy(self.value)
        value['cef_patches'][0]['path'] = '../outside.patch'
        with self.assertRaisesRegex(ValueError, 'outside'):
            generate.validate_freeze(value, self.root)
        path = self.root / self.value['cef_patches'][0]['path']
        original = self.root / 'original.patch'
        path.rename(original)
        path.symlink_to(original)
        with self.assertRaisesRegex(ValueError, 'digest mismatch'):
            generate.validate_freeze(self.value, self.root)

    def test_refuses_empty_or_duplicate_symbol_evidence(self):
        for names in ([], ['Same', 'Same'], ['not a symbol']):
            self.value['symbols']['rust'] = names
            with self.assertRaises(ValueError):
                generate.validate_freeze(self.value, self.root)

    def test_refuses_duplicate_patch_identity(self):
        (self.root / generate.PATCH_ROOT / '0001-second.patch').write_text('other')
        with self.assertRaisesRegex(ValueError, 'exactly one'):
            freeze.capture(self.symbols, self.root)

    def archive(self, missing=None, extra=None):
        archive = self.root / 'support.tar.bz2'
        with tarfile.open(archive, 'w:bz2') as output:
            for name in generate.CONTRACT['support_headers']:
                if name == missing:
                    continue
                data = ('// fixture ' + name).encode()
                member = tarfile.TarInfo('distribution/include/' + name)
                member.size = len(data)
                output.addfile(member, io.BytesIO(data))
            if extra:
                member = tarfile.TarInfo(extra)
                member.size = 7
                output.addfile(member, io.BytesIO(b'ignored'))
        return archive

    def cef(self):
        cef = self.root / 'cef'
        (cef / 'include').mkdir(parents=True)
        return cef

    def test_imports_only_explicit_non_abi_support_headers(self):
        cef = self.cef()
        archive = self.archive(extra='distribution/include/capi/cef_client_capi.h')
        hashes = generate.extract_support(archive, cef)
        self.assertEqual(set(hashes), set(generate.CONTRACT['support_headers']))
        self.assertFalse((cef / 'include/capi').exists())
        self.assertFalse((cef / 'include/cef_api_versions.h').exists())

    def test_refuses_missing_support_input(self):
        with self.assertRaisesRegex(ValueError, 'missing upstream'):
            generate.extract_support(self.archive(missing='cef_version.h'), self.cef())

    def test_never_overwrites_existing_generated_header(self):
        cef = self.cef()
        (cef / 'include/cef_version.h').write_text('private existing value')
        with self.assertRaisesRegex(ValueError, 'overwrite'):
            generate.extract_support(self.archive(), cef)
        self.assertEqual((cef / 'include/cef_version.h').read_text(), 'private existing value')

    def test_symbol_assertions_do_not_accept_prefix_matches(self):
        path = self.root / 'generated.rs'
        path.write_text('pub struct NativeContextEvaluationOther;')
        with self.assertRaisesRegex(ValueError, 'lacks frozen'):
            generate.require_symbols(['NativeContextEvaluation'], [path], 'Rust')
        path.write_text('pub struct NativeContextEvaluation;')
        generate.require_symbols(['NativeContextEvaluation'], [path], 'Rust')

    def test_download_rejects_wrong_digest_and_size(self):
        pin = {'url': 'https://fixture.invalid/archive', 'size': 5, 'sha256': '0' * 64}
        for size in (5, 4):
            pin['size'] = size
            destination = self.root / ('download-' + str(size))
            with patch('urllib.request.urlopen', return_value=io.BytesIO(b'hello')):
                with self.assertRaisesRegex(ValueError, 'mismatch'):
                    generate.download(pin, destination)


if __name__ == '__main__':
    unittest.main()
