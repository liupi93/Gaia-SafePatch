import csv
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from common import NS, write_json, write_csv, REQUEST_FIELDS, EVENT_FIELDS


def row(i, arrival, end, status='ok', start=None):
    return dict(run_id='r', request_id=str(i), worker_id='0', phase='measure',
                arrival_ns=arrival, start_ns=start, end_ns=end, status=status)


class RequestTests(unittest.TestCase):
    def test_qps_excludes_drain_and_latency_includes_admission_wait(self):
        from analyze import summarize_requests
        # 10..12s window: 2 ok completions, 1 drain completion, 1 censored.
        rows = [row(1, 10*NS, 10*NS+100_000_000, start=10*NS+90_000_000),
                row(2, 11*NS, 11*NS+200_000_000),
                row(3, 12*NS-100_000_000, 12*NS+100_000_000),
                row(4, 12*NS-50_000_000, None, 'cancelled')]
        result = summarize_requests(rows, 10*NS, 12*NS, 'r')
        self.assertEqual(result['generated'], 4)
        self.assertEqual(result['completed_ok_in_window'], 2)
        self.assertEqual(result['qps'], 1)
        self.assertEqual(result['unfinished'], 1)
        self.assertEqual(result['drain_completed'], 1)
        self.assertEqual(result['latency_n'], 3)
        self.assertEqual(result['latency_p95_ms'], 200)
        self.assertAlmostEqual(result['latency_mean_ms'], 500/3)

    def test_malformed_order_and_outside_arrivals_are_errors_not_negative_latency(self):
        from analyze import summarize_requests
        result = summarize_requests([row(1, 10*NS, 9*NS), row(2, 12*NS, 13*NS)], 10*NS, 12*NS, 'r')
        self.assertEqual(len(result['errors']), 2)
        self.assertIsNone(result['qps'])

    def test_duplicate_request_and_wrong_run_rejected(self):
        from analyze import summarize_requests
        a = row(1, 10*NS, 11*NS)
        result = summarize_requests([a, a, dict(a, run_id='stale')], 10*NS, 12*NS, 'r')
        self.assertEqual(len(result['errors']), 2)

    def test_empty_worker_or_request_identity_is_rejected(self):
        from analyze import summarize_requests
        result = summarize_requests([dict(row(1, 10*NS, 11*NS), worker_id='')], 10*NS, 12*NS, 'r')
        self.assertIsNone(result['qps'])

    def test_timeline_empty_second_is_zero_qps_and_blank_latency(self):
        from analyze import request_timeline
        rows = [row(1, 10*NS, 10*NS+100_000_000)]
        timeline = request_timeline(rows, 10*NS, 12*NS)
        self.assertEqual([r['qps'] for r in timeline], [1, 0])
        self.assertIsNone(timeline[1]['latency_p99_ms'])
        self.assertEqual(timeline[1]['latency_n'], 0)

    def test_warmup_carryover_counts_for_qps_but_not_formal_latency_cohort(self):
        from analyze import summarize_requests
        warmup = dict(row('w', 9*NS, 10*NS+500_000_000), phase='warmup')
        measure = row('m', 10*NS+800_000_000, 10*NS+900_000_000)
        result = summarize_requests([warmup, measure], 10*NS, 12*NS, 'r')
        self.assertEqual(result['qps'], 1)
        self.assertEqual(result['completed_ok_in_window'], 2)
        self.assertEqual(result['warmup_completed_in_window'], 1)
        self.assertEqual(result['generated'], 1)
        self.assertEqual(result['latency_n'], 1)
        self.assertEqual(result['latency_p99_ms'], 100)
        self.assertEqual(result['timeline'][0]['qps'], 2)
        self.assertEqual(result['timeline'][0]['latency_n'], 1)


class PatchTests(unittest.TestCase):
    def test_http_only_never_proves_application_and_times_remain_missing(self):
        from analyze import summarize_patch
        result = summarize_patch([
            dict(run_id='r', attempt_id='patch-1', event='request_sent', mono_ns=10*NS, outcome=''),
            dict(run_id='r', attempt_id='patch-1', event='http_done', mono_ns=11*NS, outcome='ok')], 'r', None)
        self.assertEqual(result['http_rtt_ms'], 1000)
        self.assertIsNone(result['apply_ms'])
        self.assertIsNone(result['submit_to_apply_ms'])
        self.assertEqual(result['verification'], 'unverified')

    def test_verified_application_and_observation_are_distinct(self):
        from analyze import summarize_patch
        events = [dict(run_id='r', attempt_id='patch-1', event=e, mono_ns=n, outcome='ok') for e, n in [
            ('request_sent', 10*NS), ('request_received', 10*NS+10_000_000),
            ('apply_begin', 10*NS+30_000_000), ('apply_end', 10*NS+60_000_000),
            ('http_done', 10*NS+90_000_000)]]
        proof = {'run_id': 'r', 'attempt_id': 'patch-1', 'pid': 42, 'new_implementation': True,
                 'observed_ns': 10*NS+80_000_000, 'evidence': 'workload build-id observation'}
        result = summarize_patch(events, 'r', proof, target_pid=42, window=(9*NS, 12*NS))
        self.assertEqual(result['verification'], 'verified')
        self.assertEqual(result['apply_ms'], 30)
        self.assertEqual(result['prepare_ms'], 20)
        self.assertEqual(result['submit_to_apply_ms'], 60)
        self.assertEqual(result['observed_ns'], 10*NS+80_000_000)
        events[3]['outcome'] = 'error'
        failed = summarize_patch(events, 'r', proof, target_pid=42)
        self.assertEqual(failed['verification'], 'failed')
        self.assertIsNone(failed['submit_to_apply_ms'])

    def test_wrong_target_and_application_after_window_are_not_verified(self):
        from analyze import summarize_patch
        events = [dict(run_id='r', attempt_id='patch-1', event=e, mono_ns=n, outcome='ok') for e,n in
                  [('request_sent', 10*NS), ('apply_begin', 11*NS), ('apply_end', 12*NS)]]
        proof = {'run_id': 'r', 'attempt_id': 'patch-1', 'pid': 7, 'new_implementation': True,
                 'observed_ns': 12*NS, 'evidence': 'fixture'}
        self.assertEqual(summarize_patch(events, 'r', proof, 42)['verification'], 'unverified')
        self.assertEqual(summarize_patch(events, 'r', dict(proof, pid=42), 42, (10*NS, 11*NS))['verification'], 'unverified')

    def test_failed_application_stage_cannot_be_overridden_by_successful_end(self):
        from analyze import summarize_patch
        for stage in ('request_received', 'apply_begin', 'write_begin'):
            with self.subTest(stage=stage):
                events = [dict(run_id='r', attempt_id='patch-1', event=e, mono_ns=n, outcome='ok') for e,n in
                          [('request_sent', 10*NS), ('request_received', 10*NS+1),
                           ('apply_begin', 11*NS), ('write_begin', 11*NS+1), ('apply_end', 12*NS)]]
                next(e for e in events if e['event']==stage)['outcome'] = 'error'
                proof = dict(run_id='r', attempt_id='patch-1', pid=42, new_implementation=True,
                             observed_ns=12*NS, evidence='fixture observation')
                result = summarize_patch(events, 'r', proof, 42, (10*NS, 13*NS))
                self.assertEqual(result['verification'], 'failed')
                self.assertIsNone(result['submit_to_apply_ms'])


class ResourceAndCountsTests(unittest.TestCase):
    def test_counts_only_gives_throughput_and_missing_quantiles(self):
        from analyze import summarize_counts
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)/'counts.json'
            data = dict(run_id='r', start_ns=10*NS, end_ns=12*NS, generated=5, completed_ok_in_window=3,
                        completed_in_window=3, failed=1, unfinished=1, drain_completed=1, completed_total=4)
            write_json(path, data)
            summary = summarize_counts(path, 'r', 10*NS, 12*NS)
            self.assertEqual(summary['qps'], 1.5)
            self.assertIsNone(summary['latency_p99_ms'])
            write_json(path, dict(data, generated=99))
            with self.assertRaises(ValueError):
                summarize_counts(path, 'r', 10*NS, 12*NS)

    def test_counts_support_warmup_carryover_and_reject_impossible_failures(self):
        from analyze import summarize_counts
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)/'counts.json'
            data = dict(run_id='r', start_ns=10*NS, end_ns=12*NS, generated=1,
                        completed_ok_in_window=2, completed_in_window=2, completed_total=1,
                        drain_completed=0, failed=0, unfinished=0,
                        warmup_completed_in_window=1, warmup_completed_ok_in_window=1)
            write_json(path, data)
            result = summarize_counts(path, 'r', 10*NS, 12*NS)
            self.assertEqual(result['qps'], 1)
            for failed in (1, 99):
                write_json(path, dict(data, failed=failed))
                with self.assertRaises(ValueError):
                    summarize_counts(path, 'r', 10*NS, 12*NS)
            # A known in-window error cannot disappear from full-cohort failed count.
            write_json(path, dict(data, completed_ok_in_window=1, warmup_completed_ok_in_window=1))
            with self.assertRaises(ValueError):
                summarize_counts(path, 'r', 10*NS, 12*NS)

    def test_resource_window_weights_intervals_and_excludes_outside_rss(self):
        from analyze import summarize_resources
        from common import RESOURCE_FIELDS
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)/'resources.csv'
            rows = [dict(case_id='G1', run_id='r', entity='gaia', mono_ns=stamp,
                         interval_start_ns=begin, cpu_pct=cpu, rss_mib=rss) for begin,stamp,cpu,rss in
                    [(9*NS,10*NS,999,999), (10*NS,11*NS,100,10), (11*NS,13*NS,200,999)]]
            write_csv(path, RESOURCE_FIELDS, rows)
            resources, errors = summarize_resources(path, 10*NS, 12*NS, 'r', 'G1')
            self.assertEqual(errors, [])
            self.assertEqual(resources['gaia']['cpu_mean_pct'], 150)
            # Timestamp exactly at start is inside the RSS window; the late sample is excluded.
            self.assertEqual(resources['gaia']['rss_sample_peak_mib'], 999)
            self.assertEqual(resources['gaia']['cpu_coverage_s'], 2)


class ReplayTests(unittest.TestCase):
    def test_replay_is_deterministic_and_synthetic_never_enters_real_comparison(self):
        from analyze import analyze_suite
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run = root / 'r'
            run.mkdir()
            write_json(root/'suite.json', {'synthetic': True, 'status': 'complete', 'plan': [
                {'run_id': 'r', 'case_id': 'G1', 'variant': 'original', 'mode': 'steady', 'threads': 4, 'repeat': 1}]})
            write_json(run/'manifest.json', {'run_id': 'r', 'case_id': 'G1', 'variant': 'original', 'mode': 'steady',
                'threads': 4, 'repeat': 1, 'synthetic': True, 'status': 'complete', 'monitor_verified': True,
                'window': {'start_ns': 10*NS, 'end_ns': 12*NS}, 'errors': []})
            write_csv(run/'requests.csv', ['case_id'] + REQUEST_FIELDS, [dict(row(1, 10*NS, 11*NS), case_id='G1')])
            analyze_suite(root)
            first = {name: (root/name).read_bytes() for name in ('comparison.csv', 'paired.csv', 'report.md')}
            analyze_suite(root)
            self.assertEqual(first, {name: (root/name).read_bytes() for name in first})
            with (root/'comparison.csv').open(encoding='utf-8') as handle:
                comparison = list(csv.DictReader(handle))
            self.assertEqual(comparison[0]['valid_runs'], '0')
            self.assertEqual(comparison[0]['qps_mean'], '')
            self.assertIn('synthetic', (root/'report.md').read_text(encoding='utf-8'))


if __name__ == '__main__':
    unittest.main()
