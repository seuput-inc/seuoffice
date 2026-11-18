"""Static harness-boundary checks, not native or behavioral proof."""
import importlib.util
from pathlib import Path
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('pdf_proof_launcher', HERE / 'run.py')
launcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)


class ProofBoundary(unittest.TestCase):
    def test_inventory_labels_working_tree_not_product_commit(self):
        inventory = launcher.manifest()
        self.assertEqual(inventory['source_kind'], 'working-tree-wip')
        self.assertIsNone(inventory['product_sha'])
        self.assertTrue(all(len(digest) == 64 for digest in inventory['sha256'].values()))

    def test_actual_codec_and_consumer_sources_are_required(self):
        inventory = launcher.manifest()['sha256']
        for name in (
            'crates/shell/src/host/d2/pdf/crypto/pdf_lib.rs',
            'crates/shell/src/host/d2/pdf/crypto/pdf_lib/assembly.rs',
            'crates/shell/src/host/d2/pdf/crypto/pdf_lib/revision.rs',
            'crates/shell/src/host/d2/pdf/pdf_codec.rs',
            'crates/shell/src/host/d2/pdf/native_engine/signatures/pdf.rs',
            'apps/pdf/src/main/pdf-sign.ts',
            'apps/pdf/src/main/pdf-signatures.ts',
        ):
            self.assertIn(name, inventory)

    def test_driver_exposes_no_private_key_or_digest_signing_operation(self):
        source = (HERE / 'main.rs').read_text()
        self.assertNotIn('"sign_digest"', source)
        self.assertNotIn('"private_key"', source)
        self.assertIn('"inspect" =>', source)
        self.assertIn('"assemble" =>', source)
        self.assertIn('"validate" =>', source)


if __name__ == '__main__':
    unittest.main()
