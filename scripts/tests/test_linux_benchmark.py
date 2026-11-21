import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('bench', Path(__file__).parents[1] / 'linux_benchmark.py')
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


class BenchmarkTests(unittest.TestCase):
    def test_stage_durations_are_differences_not_cumulative_times(self):
        parsed = bench.parse_stages('noise\n 10.0 ms  mapped files\n 30.0 ms  links: 12 ids\n 130.0 ms  events parsed (8 folds)\n')
        self.assertEqual([r['elapsed_ms'] for r in parsed], [10, 30, 130])
        self.assertEqual([r['duration_ms'] for r in parsed], [10, 20, 100])

    def test_fallback_timer_reset_is_rejected(self):
        with self.assertRaises(ValueError):
            bench.parse_stages('100.0 ms  events parsed\nfast engine declined; running the exact fallback\n2.0 ms  read files\n')

    def test_medians_do_not_mix_versions_or_use_best_run(self):
        rows = [{'version': label, 'wall_seconds': seconds, 'max_rss_kib': 100}
                for label, times in [('baseline', [10, 100, 12]), ('candidate', [9, 8, 10])]
                for seconds in times]
        result = bench.summarize(rows)
        self.assertEqual(result['baseline']['median_seconds'], 12)
        self.assertEqual(result['candidate']['median_seconds'], 9)
        self.assertEqual(result['speedup'], 12 / 9)

    def test_native_linux_with_sufficient_resources_is_required(self):
        bench.validate_host('Linux', 'x86_64', 8, 32 * 1024**3)
        for os_name, arch, cpus, memory in [
            ('Darwin', 'arm64', 8, 32 * 1024**3),
            ('Linux', 'aarch64', 8, 32 * 1024**3),
            ('Linux', 'x86_64', 4, 32 * 1024**3),
            ('Linux', 'x86_64', 8, 16 * 1024**3),
        ]:
            with self.assertRaises(ValueError):
                bench.validate_host(os_name, arch, cpus, memory)


if __name__ == '__main__':
    unittest.main()
