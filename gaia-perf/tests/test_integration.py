"""Real subprocess / loopback HTTP tests. All workload and event data is SYNTHETIC."""
import copy
import csv
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

BASE = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(BASE))
from common import read_json, write_json


def config_for(root):
    from tests_support import synthetic_config
    return synthetic_config(root)


class ProcessTests(unittest.TestCase):
    def test_cleanup_only_owns_launched_handles(self):
        from adapters import ProcessOwner
        with tempfile.TemporaryDirectory() as tmp:
            bystander = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])
            owner = ProcessOwner()
            try:
                child = owner.start('test', [sys.executable, '-c', 'import time; time.sleep(30)'], Path(tmp)/'child.log', Path(tmp))
                records = owner.cleanup(time.monotonic_ns()+2_000_000_000)
                self.assertIsNotNone(child.poll())
                self.assertIsNone(bystander.poll())
                self.assertIsNotNone(records['test']['returncode'])
            finally:
                bystander.terminate()
                bystander.wait(timeout=5)


class SuiteTests(unittest.TestCase):
    def execute(self, root, config, *args):
        path = Path(root)/'config.json'
        write_json(path, config)
        result = subprocess.run([sys.executable, str(BASE/'runner.py'), '--config', str(path),
                                 '--synthetic', '--variant', 'original', *args],
                                capture_output=True, text=True, timeout=20, encoding='utf-8')
        suites = list((Path(root)/'results').glob('*/suite.json'))
        self.assertEqual(len(suites), 1, result.stdout+result.stderr)
        return result, suites[0].parent, read_json(suites[0])

    def test_slow_patch_does_not_block_sampling_and_success_needs_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = config_for(tmp)
            config['variants']['original']['env']['SYNTHETIC_HTTP_DELAY'] = '0.7'
            result, root, suite = self.execute(tmp, config, '--mode', 'patch')
            self.assertEqual(result.returncode, 0, result.stdout+result.stderr)
            run = root/suite['plan'][0]['run_id']
            manifest, summary = read_json(run/'manifest.json'), read_json(run/'summary.json')
            self.assertTrue(summary['measurement_valid'], summary)
            self.assertFalse(summary['valid_for_comparison'])
            self.assertEqual(summary['patch']['verification'], 'verified')
            self.assertGreater(summary['patch']['http_rtt_ms'], 650)
            with (run/'resources.csv').open(encoding='utf-8') as handle:
                rows = [r for r in csv.DictReader(handle) if r['entity'] == 'runner']
            self.assertGreaterEqual(len(rows), 7)
            self.assertLess(max((int(b['mono_ns'])-int(a['mono_ns']))/1e9 for a,b in zip(rows,rows[1:])), .45)
            self.assertEqual(read_json(run/'synthetic_http_count.json')['put_count'], 1)
            self.assertTrue((run/'events.db').exists())
            self.assertTrue(all(p['returncode'] is not None for p in manifest['processes'].values()))

    def test_http_without_real_markers_remains_unverified(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = config_for(tmp)
            config['variants']['original']['env']['SYNTHETIC_NO_MARKERS'] = '1'
            result, root, suite = self.execute(tmp, config, '--mode', 'patch')
            self.assertNotEqual(result.returncode, 0)
            run = root/suite['plan'][0]['run_id']
            summary = read_json(run/'summary.json')
            self.assertEqual(read_json(run/'manifest.json')['status'], 'unverified')
            self.assertEqual(summary['patch']['verification'], 'unverified')
            self.assertIsNone(summary['patch']['apply_ms'])
            self.assertIsNotNone(summary['patch']['http_rtt_ms'])

    def test_application_stage_failure_is_retained_as_failed_round(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = config_for(tmp)
            config['variants']['original']['env']['SYNTHETIC_APPLY_FAILURE'] = '1'
            result, root, suite = self.execute(tmp, config, '--mode', 'patch')
            self.assertNotEqual(result.returncode, 0)
            run = root/suite['plan'][0]['run_id']
            self.assertEqual(read_json(run/'manifest.json')['status'], 'failed')
            self.assertEqual(read_json(run/'summary.json')['patch']['verification'], 'failed')

    def test_budget_skips_whole_round_and_retains_status(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = config_for(tmp)
            config['experiment']['suite_budget_s'] = 1
            result, root, suite = self.execute(tmp, config, '--mode', 'steady')
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(suite['status'], 'incomplete')
            manifest = read_json(root/suite['plan'][0]['run_id']/'manifest.json')
            self.assertEqual(manifest['status'], 'budget_skipped')
            self.assertFalse(manifest.get('processes'))

    def test_early_workload_exit_preserves_failed_run_and_cleans_gaia(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = config_for(tmp)
            config['workload']['env']['SYNTHETIC_EXIT_MEASURE'] = '1'
            result, root, suite = self.execute(tmp, config, '--mode', 'steady')
            self.assertNotEqual(result.returncode, 0)
            manifest = read_json(root/suite['plan'][0]['run_id']/'manifest.json')
            self.assertEqual(manifest['status'], 'failed')
            self.assertTrue(all(p['returncode'] is not None for p in manifest['processes'].values()))

    def test_gaia_crash_during_stop_export_never_becomes_a_complete_round(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = config_for(tmp)
            config['workload']['env']['SYNTHETIC_GAIA_CRASH_ON_STOP'] = '1'
            result, root, suite = self.execute(tmp, config, '--mode', 'steady')
            self.assertNotEqual(result.returncode, 0)
            run = root/suite['plan'][0]['run_id']
            manifest, summary = read_json(run/'manifest.json'), read_json(run/'summary.json')
            self.assertEqual(manifest['status'], 'failed')
            self.assertFalse(summary['measurement_valid'])
            self.assertIsNotNone(manifest['processes']['gaia']['returncode_before_cleanup'])
            self.assertFalse(manifest['processes']['gaia']['exit_requested_by_runner'])

    def test_configuration_rejects_synthetic_without_explicit_cli_flag(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)/'config.json'
            write_json(path, config_for(tmp))
            result = subprocess.run([sys.executable, str(BASE/'runner.py'), '--config', str(path), '--dry-run'],
                                    capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 2)
            self.assertIn('--synthetic', result.stderr)

    def test_optimized_steady_keeps_monitoring_and_coordination_enabled(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = config_for(tmp)
            result, root, suite = self.execute(tmp, config, '--variant', 'optimized', '--mode', 'steady')
            self.assertEqual(result.returncode, 0, result.stdout+result.stderr)
            run = root/suite['plan'][0]['run_id']
            manifest = read_json(run/'manifest.json')
            self.assertEqual(manifest['case_id'], 'G3')
            self.assertTrue(manifest['optimization_verified'])
            self.assertTrue(manifest['optimization_proof']['coordination_enabled'])
            self.assertFalse((run/'patch_http.json').exists())

    def test_missing_monitor_event_and_bad_window_ack_fail_before_measurement(self):
        for section, flag in [('variants', 'SYNTHETIC_NO_MONITOR'), ('workload', 'SYNTHETIC_BAD_ACK')]:
            with self.subTest(flag=flag), tempfile.TemporaryDirectory() as tmp:
                config = config_for(tmp)
                env = config['variants']['original']['env'] if section == 'variants' else config['workload']['env']
                env[flag] = '1'
                result, root, suite = self.execute(tmp, config, '--mode', 'steady')
                self.assertNotEqual(result.returncode, 0)
                manifest = read_json(root/suite['plan'][0]['run_id']/'manifest.json')
                self.assertEqual(manifest['status'], 'failed')
                self.assertNotIn('window', manifest)

    def test_pending_http_is_preserved_without_waiting_for_long_timeout(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = config_for(tmp)
            config['variants']['original']['env']['SYNTHETIC_HTTP_DELAY'] = '30'
            config['patch']['http_timeout_s'] = 30
            began = time.monotonic()
            result, root, suite = self.execute(tmp, config, '--mode', 'patch')
            self.assertLess(time.monotonic()-began, 8)
            self.assertNotEqual(result.returncode, 0)
            run = root/suite['plan'][0]['run_id']
            self.assertEqual(read_json(run/'summary.json')['patch']['verification'], 'unverified')
            self.assertTrue((run/'patch_http.json').exists())

    def test_interrupt_saves_status_and_reaps_created_processes(self):
        with tempfile.TemporaryDirectory() as tmp:
            config_path = Path(tmp)/'config.json'
            write_json(config_path, config_for(tmp))
            result = subprocess.run([sys.executable, str(BASE/'tests'/'interrupt_probe.py'), str(config_path)],
                                    capture_output=True, text=True, timeout=12, encoding='utf-8')
            self.assertEqual(result.returncode, 130, result.stdout+result.stderr)
            suite_path = next((Path(tmp)/'results').glob('*/suite.json'))
            suite = read_json(suite_path)
            manifest = read_json(suite_path.parent/suite['plan'][0]['run_id']/'manifest.json')
            self.assertEqual(manifest['status'], 'interrupted')
            self.assertTrue(all(p['returncode'] is not None for p in manifest['processes'].values()))


if __name__ == '__main__':
    unittest.main()
