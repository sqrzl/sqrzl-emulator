import hashlib
import importlib.util
import json
from pathlib import Path
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('release_package', Path(__file__).with_name('release_package.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class ReleasePackageTests(unittest.TestCase):
    def test_bundle_contains_complete_ui_and_verifiable_payload_hashes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / 'binary'; binary.write_bytes(b'executable')
            ui = root / 'ui'; (ui / 'assets').mkdir(parents=True)
            (ui / 'index.html').write_text('<script src="/assets/app.js"></script>')
            (ui / 'assets/app.js').write_text('application')
            archive = module.package(binary, ui, root / 'output', '0.1.0', 'a' * 40, 'linux-x86_64')
            with tarfile.open(archive) as bundle:
                members = {member.name: bundle.extractfile(member).read() for member in bundle.getmembers() if member.isfile()}
            prefix = 'sqrzl-emulator-0.1.0-linux-x86_64/'
            manifest = json.loads(members[prefix + 'manifest.json'])
            self.assertEqual(manifest['source_sha'], 'a' * 40)
            self.assertIn(prefix + 'static/assets/app.js', members)
            self.assertIn(prefix + 'README.txt', members)
            for name, digest in manifest['files'].items():
                self.assertEqual(hashlib.sha256(members[prefix + name]).hexdigest(), digest)
            self.assertEqual(archive.with_suffix(archive.suffix + '.sha256').read_text().split()[0], hashlib.sha256(archive.read_bytes()).hexdigest())

    def test_missing_ui_and_symlink_escape_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); binary = root / 'binary'; binary.write_bytes(b'executable')
            ui = root / 'ui'; ui.mkdir()
            with self.assertRaises(ValueError): module.package(binary, ui, root / 'out', '0.1.0', 'a' * 40, 'linux-x86_64')
            (ui / 'index.html').write_text('app')
            (ui / 'escape').symlink_to(binary)
            with self.assertRaises(ValueError): module.package(binary, ui, root / 'out', '0.1.0', 'a' * 40, 'linux-x86_64')

if __name__ == '__main__': unittest.main()
