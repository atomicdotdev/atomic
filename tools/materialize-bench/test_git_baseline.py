import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('git_baseline', Path(__file__).with_name('git_baseline.py'))
git_baseline = importlib.util.module_from_spec(spec)
spec.loader.exec_module(git_baseline)

GIT = shutil.which('git') is not None


@unittest.skipUnless(GIT, 'git executable required')
class GitBaselineTests(unittest.TestCase):
    def test_generated_content_matches_atomic_fixture_format(self):
        golden = git_baseline.build_golden(3, 8)
        self.assertEqual(
            golden['files/file_0002.txt'],
            ''.join(git_baseline.generated_line(2, line) for line in range(8)).encode())
        self.assertEqual(git_baseline.edited_line(3, 1),
                         'file_0000 line_000003 value_00000002\n')

    def test_fixture_history_and_clean_status(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / 'repo'
            root.mkdir()
            golden = git_baseline.build_fixture(root, 3, 8, 2)
            self.assertEqual(golden['files/file_0000.txt'].count(b'value_00000001'), 1)
            git_baseline.verify(root, golden)

    def test_smoke_run_measures_every_case_and_verifies(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / 'out'
            git_baseline.run(output, files=3, lines=8, edits=2,
                             iterations=2, warmups=1, keep_fixtures=False)
            raw = json.loads((output / 'raw.json').read_text())
            cases = {(s['case'], s['iteration']) for s in raw['samples']}
            self.assertEqual(cases, {(case, i) for case in git_baseline.CASES
                                     for i in range(2)})
            for entry in raw['samples']:
                self.assertGreater(entry['total_ns'], 0)
            run_report = json.loads((output / 'run.json').read_text())
            self.assertEqual(run_report['outcome'], 'measured')
            self.assertEqual(len(run_report['summary_ms']), len(git_baseline.CASES))

    def test_corrupted_fixture_fails_verification(self):
        if not GIT:
            self.skipTest('git executable required')
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / 'repo'
            root.mkdir()
            golden = git_baseline.build_fixture(root, 2, 4, 1)
            (root / 'files/file_0000.txt').write_text('corrupted\n')
            with self.assertRaises(RuntimeError):
                git_baseline.verify(root, golden)


if __name__ == '__main__':
    unittest.main()
