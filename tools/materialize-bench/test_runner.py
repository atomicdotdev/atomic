import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('runner', Path(__file__).with_name('run.py'))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class RunnerTests(unittest.TestCase):
    def report(self):
        return {'config': {'warmups': 1}, 'samples': [dict(case=case, backend=backend, iteration=i,
                                 total_ns=(2 if backend == 'graph' else 1) * 1_000_000,
                                 phases_ns={'render': 500_000})
                            for case in runner.CASES for backend in ('graph', 'span')
                            for i in range(3)]}

    def test_complete_report_and_summary(self):
        report = self.report()
        runner.validate_report(report, 3)
        summary = runner.summarize(report)
        self.assertEqual(len(summary), 8)
        for row in summary:
            self.assertEqual(row['graph_over_span_median'], 2)
            self.assertEqual(row['graph']['n'], 3)
            self.assertEqual(row['span']['phases_ms']['render']['median'], .5)
            self.assertEqual(row['span']['order_ms']['first']['n'], 2)
            self.assertEqual(row['graph']['order_ms']['first']['n'], 1)

    def test_missing_duplicate_unknown_and_invalid_samples_fail(self):
        report = self.report()
        bad = copy.deepcopy(report)
        bad['samples'].pop()
        with self.assertRaises(ValueError):
            runner.validate_report(bad, 3)
        for field, value in [('case', 'unknown'), ('iteration', 9), ('total_ns', 0)]:
            bad = copy.deepcopy(report)
            bad['samples'][0][field] = value
            with self.assertRaises(ValueError):
                runner.validate_report(bad, 3)
        bad = copy.deepcopy(report)
        bad['samples'][-1] = bad['samples'][0]
        with self.assertRaises(ValueError):
            runner.validate_report(bad, 3)

    def test_nearest_rank_p95(self):
        result = runner.distribution(list(range(1, 21)))
        self.assertEqual(result['median'], 10.5)
        self.assertEqual(result['p95'], 19)


if __name__ == '__main__':
    unittest.main()
