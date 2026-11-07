import json
import sys
from pathlib import Path


def synthetic_config(root):
    base = Path(__file__).resolve().parents[1]
    config = json.loads((base/'config.json').read_text(encoding='utf-8'))
    peer = str(base/'tests'/'synthetic_peer.py')
    config['synthetic'] = True
    for value in config['variants'].values():
        value.update(binary=sys.executable, build_manifest='synthetic-not-a-build',
                     launch_argv=[sys.executable, peer, 'gaia'], env={})
    config['variants']['optimized']['mechanisms']['activation_description'] = 'SYNTHETIC only: protocol simulation'
    config['workload'].update(binary=sys.executable, target_symbol='synthetic_function',
        launch_argv=[sys.executable, peer, 'workload', '--threads', '{threads}', '--seed', '{seed}'], env={})
    config['patch'].update(library=sys.executable, replacement_symbol='synthetic_replacement', events_adapter='csv-v1')
    config['gaia']['config_template'] = str(base/'templates'/'monitor.toml')
    config['environment_lock'] = 'synthetic-not-an-environment'
    config['output_root'] = str(Path(root)/'results')
    config['experiment'].update(threads=[2], repeats=1, warmup_s=.25, duration_s=1.5,
        patch_at_s=.2, sample_interval_s=.15, startup_budget_s=5, drain_timeout_s=.7,
        cleanup_budget_s=2, suite_budget_s=25, suite_reserve_s=3, measure_lead_s=.12)
    return config
