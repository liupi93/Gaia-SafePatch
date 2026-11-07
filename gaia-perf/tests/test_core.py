"""Meaningful scheduling and Linux-counter regression fixtures (no Gaia data)."""
import copy
import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))


class PlanTests(unittest.TestCase):
    def test_optimized_tgid_filter_event_is_probe_evidence_only_for_target(self):
        from adapters import monitor_evidence
        snapshot = {'events': [{'tgid': 42, 'kind': 'hotpatch', 'action': 'blocked',
                                'detail': 'uprobe-entry:pid-filtered'}]}
        self.assertIsNone(monitor_evidence(snapshot, 42))
        self.assertIsNone(monitor_evidence(snapshot, 43, optimized_tgid=True))
        self.assertEqual(monitor_evidence(snapshot, 42, optimized_tgid=True)['target_pid'], 42)

    def setUp(self):
        self.config = json.loads((Path(__file__).resolve().parents[1] / 'config.json').read_text(encoding='utf-8'))

    def test_return_patch_uses_same_action_and_value_for_both_versions(self):
        from runner import make_patch_payload, make_plan, validate_config
        self.config['patch'].update(action='override_return', override_return_value=99)
        self.config['workload'].update(binary='/fixture/target', target_symbol='target_value')
        plan = make_plan(self.config, modes=['patch'])
        base = make_patch_payload(self.config, 'original', 123)
        main = make_patch_payload(self.config, 'optimized', 456)
        self.assertEqual(base['patch_action'], 'override_return')
        self.assertEqual(base['override_return_value'], 99)
        self.assertEqual(main, dict(base, pid=456, patch_mode='cooperative'))
        self.assertNotIn('replace_lib', base)
        self.assertNotIn('patch.library', '\n'.join(validate_config(self.config, plan)))

    def test_matrix_is_paired_deterministic_and_exactly_16(self):
        from runner import make_plan
        plan = make_plan(self.config)
        self.assertEqual(len(plan), 16)
        self.assertEqual(plan, make_plan(self.config))
        self.assertEqual({r['case_id'] for r in plan}, {'G1', 'G2', 'G3', 'G4'})
        for i in range(0, 16, 2):
            a, b = plan[i:i+2]
            self.assertEqual((a['mode'], a['threads'], a['repeat']), (b['mode'], b['threads'], b['repeat']))
            self.assertEqual({a['variant'], b['variant']}, {'original', 'optimized'})
            self.assertEqual(a['workload_seed'], b['workload_seed'])

    def test_filter_does_not_reintroduce_unselected_cases(self):
        from runner import make_plan
        plan = make_plan(self.config, variants=['original'], modes=['steady'], threads=[1], repeats=1)
        self.assertEqual(len(plan), 1)
        self.assertEqual(plan[0]['case_id'], 'G1')

    def test_missing_fields_point_to_selected_variant_and_workload(self):
        from runner import validate_config, make_plan
        errors = validate_config(self.config, make_plan(self.config, variants=['original'], modes=['steady']))
        combined = '\n'.join(errors)
        self.assertIn('variants.original.binary', combined)
        self.assertIn('workload.launch_argv', combined)
        self.assertNotIn('variants.optimized.binary', combined)
        self.assertNotIn('patch.library', combined)

    def test_duplicate_case_and_invalid_timing_rejected(self):
        from runner import validate_config, make_plan
        self.config['experiment']['cases'].append(copy.deepcopy(self.config['experiment']['cases'][0]))
        self.config['experiment']['patch_at_s'] = 20
        errors = '\n'.join(validate_config(self.config, make_plan(self.config)))
        self.assertIn('experiment.cases', errors)
        self.assertIn('experiment.patch_at_s', errors)

    def test_budget_requires_full_round_and_reserve(self):
        from runner import can_start_round
        self.assertFalse(can_start_round(74.99, self.config['experiment']))
        self.assertTrue(can_start_round(75, self.config['experiment']))

    def test_bad_structures_report_fields_instead_of_crashing(self):
        from runner import validate_config, make_plan
        for field, value in [('repeats', 'two'), ('threads', 4), ('cases', None)]:
            config = copy.deepcopy(self.config)
            config['experiment'][field] = value
            errors = '\n'.join(validate_config(config, make_plan(config)))
            self.assertIn('experiment.'+field, errors)

    def test_invalid_environment_and_unknown_command_placeholder_are_precheck_errors(self):
        from runner import validate_config, make_plan
        self.config['workload']['env'] = {'BAD': 1}
        self.config['workload']['launch_argv'] = ['{not_a_known_key}']
        errors = '\n'.join(validate_config(self.config, make_plan(self.config)))
        self.assertIn('workload.env', errors)
        self.assertIn('workload.launch_argv', errors)


class ProcTests(unittest.TestCase):
    def test_process_stat_comm_parentheses_do_not_shift_fields(self):
        from resources import parse_proc_stat, process_delta
        # Fields 3..24, utime=120 stime=30 starttime=999 rss=2048 pages.
        fields = ['S'] + ['0'] * 21
        fields[11], fields[12], fields[19], fields[21] = '120', '30', '999', '2048'
        snap = parse_proc_stat('42 (name with ) spaces) ' + ' '.join(fields), 4096)
        self.assertEqual(snap['cpu_ticks'], 150)
        self.assertEqual(snap['start_ticks'], 999)
        self.assertEqual(snap['rss_mib'], 8)
        prev = dict(snap, cpu_ticks=0)
        self.assertEqual(process_delta(prev, snap, 0.5, 100)['cpu_pct'], 300)
        self.assertIsNone(process_delta(prev, dict(snap, start_ticks=1000), .5, 100)['cpu_pct'])

    def test_system_guest_is_not_counted_twice_and_steal_separate(self):
        from resources import parse_system_stat, system_delta
        a = parse_system_stat('cpu  100 0 50 200 10 0 0 20 40 0\n')
        b = parse_system_stat('cpu  150 0 70 220 10 0 0 30 60 0\n')
        result = system_delta(a, b)
        self.assertEqual(b['total_ticks'] - a['total_ticks'], 100)
        self.assertEqual(result['cpu_pct'], 70)
        self.assertEqual(result['steal_pct'], 10)


if __name__ == '__main__':
    unittest.main()
