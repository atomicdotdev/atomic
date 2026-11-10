import importlib.util
import json
from pathlib import Path
import unittest

for name in ('run', 'run_agent_sandbox'):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    globals()[name] = module


def report():
    return {'samples': [dict(case=case, iteration=i, total_ns=(2 if case.startswith('atomic') else 1) * 1_000_000,
                             phases_ns={'connect': 100_000})
                        for case in run_agent_sandbox.CASES for i in range(3)]}


class RunnerTests(unittest.TestCase):
    def test_complete_report_and_summary(self):
        data = report()
        run_agent_sandbox.validate_report(data, 3)
        summary = run_agent_sandbox.summarize(data)
        self.assertEqual(len(summary), len(run_agent_sandbox.CASES))
        self.assertEqual(summary['atomic_rpc_full']['median'], 2)
        self.assertEqual(summary['git_clone_full']['median'], 1)
        self.assertEqual(summary['atomic_rpc_full']['phases_ms']['connect']['median'], .1)

    def test_missing_duplicate_unknown_and_invalid_samples_fail(self):
        import copy
        bad = copy.deepcopy(report())
        bad['samples'].pop()
        with self.assertRaises(ValueError):
            run_agent_sandbox.validate_report(bad, 3)
        for field, value in [('case', 'unknown'), ('iteration', 9), ('total_ns', 0)]:
            bad = copy.deepcopy(report())
            bad['samples'][0][field] = value
            with self.assertRaises(ValueError):
                run_agent_sandbox.validate_report(bad, 3)
        bad = copy.deepcopy(report())
        bad['samples'][-1] = bad['samples'][0]
        with self.assertRaises(ValueError):
            run_agent_sandbox.validate_report(bad, 3)

    def test_profiles_shape_matches_example_flags(self):
        for profile in run_agent_sandbox.PROFILES.values():
            self.assertEqual(set(profile), {'files_east', 'files_west', 'lines', 'edits'})


if __name__ == '__main__':
    unittest.main()
