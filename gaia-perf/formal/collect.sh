#!/usr/bin/env bash
set -euo pipefail
export PATH=/root/.cargo/bin:$PATH RUSTUP_AUTO_INSTALL=0
unset RUSTUP_TOOLCHAIN
cd "$GAIA_FORMAL_DIR"
work=$PWD
cd collector
if ! findmnt -rn -t cgroup2 | grep . >/dev/null; then
    mkdir -p /run/gaia-cgroup2
    mount -t cgroup2 none /run/gaia-cgroup2
fi
cc -std=c11 -O2 -g -Wall -Wextra -Werror -fno-pie -no-pie -fcf-protection=none -pthread workload.c -o workload
nm workload | grep -E ' (target_value|gaia_safepatch_state)$'
python3 - "$work" <<'PY'
import json
import subprocess
import sys
from pathlib import Path
from common import sha256
from environment import observe_environment

root = Path(sys.argv[1])
config = json.loads(Path('config.json').read_text())
lock = observe_environment(root/'base/Cargo.lock')
Path('environment.lock.json').write_text(json.dumps(lock, indent=2))
config['environment_lock'] = 'environment.lock.json'
for ref, name in (('base', 'original'), ('main', 'optimized')):
    source = root/ref
    binary = source/'target/release/gaia-xdp'
    environment = observe_environment(source/'Cargo.lock')
    if any(environment[key] != lock[key] for key in ('toolchain', 'cargo_lock_sha256')):
        raise SystemExit('build toolchains or Cargo.lock differ')
    manifest = {
        'profile': 'release',
        'source_commit': subprocess.check_output(['git', '-C', source, 'rev-parse', 'HEAD'], text=True).strip(),
        'binary_sha256': sha256(binary),
        'toolchain': environment['toolchain'],
        'cargo_lock_sha256': environment['cargo_lock_sha256'],
        'instrumentation_sha256': sha256(root/f'{ref}-instrumentation.diff'),
        'instrumentation_diff': str(root/f'{ref}-instrumentation.diff'),
        'build_log': str(root/f'{ref}-release.log'),
    }
    path = Path(f'{ref}-build.json')
    path.write_text(json.dumps(manifest, indent=2))
    config['variants'][name].update(binary=str(binary), build_manifest=str(path),
                                    expected_commit=manifest['source_commit'])
config['variants']['optimized']['mechanisms']['activation_description'] = (
    'main cooperative patch mode and workload opt-in ABI, verified with target observations')
config['workload'].update(binary=str(Path('workload').resolve()), launch_argv=['{binary}'],
                          target_symbol='target_value', safepatch_abi=True)
config['patch'].update(action='override_return', override_return_value=99, events_adapter='csv-v1')
Path('config.formal.json').write_text(json.dumps(config, indent=2))
PY

summarize() {
    local results_root=$1
    local suite
    suite=$(find "$results_root" -maxdepth 1 -mindepth 1 -type d | sort | tail -n 1)
    [ -n "$suite" ] || return 0
    printf 'GAIA_SUITE_DIR=%s\n' "$(realpath "$suite")"
    python3 - "$suite" <<'PY'
import json
import sys
from pathlib import Path

suite = Path(sys.argv[1])
data = json.loads((suite/'suite.json').read_text())
for r in data.get('plan', []):
    run = suite/r['run_id']
    path = run/'summary.json'
    if not path.is_file():
        continue
    item = json.loads(path.read_text())
    if not item.get('valid_for_comparison'):
        print(r['run_id'], item.get('status'), item.get('errors'), flush=True)
        for name in ('gaia.log', 'workload.log'):
            path = run/name
            if path.is_file():
                print(name, '\n'.join(path.read_text(errors='replace').splitlines()[-10:]))
PY
    printf 'GAIA_REPORT_ARCHIVE_BEGIN\n'
    (cd "$suite"; tar -czf - suite.json report.md runs.csv paired.csv comparison.csv) | base64 -w0
    printf '\nGAIA_REPORT_ARCHIVE_END\n'
}

python3 runner.py --config config.formal.json --preflight
python3 - <<'PY'
import json
from pathlib import Path
c = json.loads(Path('config.formal.json').read_text())
c['output_root'] = 'smoke-results'
Path('config.smoke.json').write_text(json.dumps(c))
PY
printf 'GAIA_SMOKE_START\n'
if python3 -u runner.py --config config.smoke.json --threads 4 --repeats 1 --warmup 2 --duration 12; then
    summarize smoke-results
else
    summarize smoke-results
    exit 5
fi
printf 'GAIA_FORMAL_START\n'
if python3 -u runner.py --config config.formal.json; then
    summarize results
else
    summarize results
    exit 6
fi
printf 'GAIA_FORMAL_COMPLETE\n'
