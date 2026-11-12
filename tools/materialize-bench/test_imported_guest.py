import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('imported_guest', Path(__file__).with_name('imported_guest.py'))
guest = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guest)


class ImportedGuestTests(unittest.TestCase):
    def manifest(self):
        return dict(git_tip='a' * 40, files=[dict(path='src/main.rs', mode='100644', sha256=hashlib.sha256(b'test').hexdigest())])

    def test_manifest_refuses_unsafe_paths_and_unsupported_modes(self):
        guest.validate_manifest(self.manifest())
        for name in ('', '.', '..', '../outside', '/outside', '.atomic/atomic.redb', '.git/index', 'src/../outside', 'src//main.rs'):
            manifest = self.manifest()
            manifest['files'][0]['path'] = name
            with self.assertRaises(ValueError, msg=name):
                guest.validate_manifest(manifest)
        for field, value in [('mode', '160000'), ('sha256', 'not-a-hash')]:
            manifest = self.manifest()
            manifest['files'][0][field] = value
            with self.assertRaises(ValueError):
                guest.validate_manifest(manifest)
        manifest = self.manifest()
        manifest['files'].append(copy.deepcopy(manifest['files'][0]))
        with self.assertRaises(ValueError):
            guest.validate_manifest(manifest)

    def test_reset_only_touches_owned_files_and_database(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            with self.assertRaises(FileNotFoundError):
                guest.check_owned_root(root)
            (root / 'owner.json').write_text(json.dumps(dict(marker=guest.MARKER, root=str(root))))
            guest.check_owned_root(root)
            repo = root / 'graph'
            (repo / '.atomic').mkdir(parents=True)
            (repo / 'src').mkdir()
            (repo / '.atomic/atomic.redb').write_bytes(b'changed')
            (root / 'graph-baseline.redb').write_bytes(b'original')
            (root / 'golden').mkdir()
            (root / 'golden/0').write_bytes(b'test')
            unrelated = repo / 'keep-user-file'
            unrelated.write_bytes(b'untouched')
            manifest = self.manifest()
            guest.reset_fixture(root, 'graph', manifest, 'render')
            guest.verify_checkout(repo, manifest)
            guest.reset_fixture(root, 'graph', manifest, 'materialize_missing')
            self.assertFalse((repo / 'src/main.rs').exists())
            self.assertEqual((repo / '.atomic/atomic.redb').read_bytes(), b'original')
            self.assertEqual(unrelated.read_bytes(), b'untouched')

    def test_immutable_change_digest_detects_content_and_name_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            changes = repo / '.atomic/changes'
            changes.mkdir(parents=True)
            first = changes / 'first.change'
            first.write_bytes(b'original authored bytes')
            baseline = guest.immutable_changes_digest(repo)
            first.write_bytes(b'altered authored bytes')
            self.assertNotEqual(guest.immutable_changes_digest(repo), baseline)
            first.write_bytes(b'original authored bytes')
            self.assertEqual(guest.immutable_changes_digest(repo), baseline)
            first.rename(changes / 'renamed.change')
            self.assertNotEqual(guest.immutable_changes_digest(repo), baseline)


if __name__ == '__main__':
    unittest.main()
