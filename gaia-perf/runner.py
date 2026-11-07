"""Gaia paired benchmark orchestration. No shell command construction or global kill."""
import copy
import math
import random
import argparse
import csv
import datetime
import json
import http.client
import os
import signal
import subprocess
import sys
import time
import uuid
from pathlib import Path

from common import NS, EVENT_FIELDS, RESOURCE_FIELDS, REQUEST_FIELDS, mono_ns, read_json, write_json, write_csv, expand_argv, sha256


def make_plan(config, variants=None, modes=None, threads=None, repeats=None):
    exp = config.get('experiment', {})
    if not isinstance(exp, dict):
        return []
    raw_cases = exp.get('cases', [])
    if not isinstance(raw_cases, list):
        return []
    counts = threads if threads is not None else exp.get('threads', [])
    repetition_count = repeats if repeats is not None else exp.get('repeats', 0)
    if not isinstance(counts, list) or type(repetition_count) is not int or repetition_count < 1:
        return []
    cases = [c for c in raw_cases if isinstance(c, dict)
             and (not variants or c.get('variant') in variants) and (not modes or c.get('mode') in modes)]
    rng = random.Random(exp.get('seed') if type(exp.get('seed')) is int else 20261104)
    plan = []
    for mode in ('steady', 'patch'):
        selected = [c for c in cases if c.get('mode') == mode]
        if not selected:
            continue
        for count in counts:
            if type(count) is not int or count < 1:
                continue
            for repeat in range(1, repetition_count + 1):
                pair = copy.deepcopy(selected)
                rng.shuffle(pair)
                seed = rng.randrange(2**31)
                for case in pair:
                    plan.append({'case_id': case.get('id'), 'variant': case.get('variant'), 'mode': mode,
                                 'threads': count, 'repeat': repeat, 'workload_seed': seed,
                                 'run_id': f"{len(plan)+1:02d}-{case.get('id')}-t{count}-r{repeat}"})
    return plan


def can_start_round(remaining_s, experiment):
    cost = sum(experiment[k] for k in ('startup_budget_s', 'warmup_s', 'duration_s', 'cleanup_budget_s', 'suite_reserve_s'))
    return remaining_s >= cost


def validate_config(config, plan):
    """Return all actionable field errors, including fields for selected cases only."""
    errors = []
    if config.get('schema_version') != 1:
        errors.append('schema_version: must be 1')
    for section in ('variants', 'workload', 'patch', 'experiment', 'gaia'):
        if not isinstance(config.get(section), dict):
            errors.append(f'{section}: must be an object')
    if errors:
        return errors
    exp, workload = config['experiment'], config['workload']
    for field in ('duration_s', 'sample_interval_s', 'startup_budget_s', 'cleanup_budget_s', 'suite_budget_s'):
        value = exp.get(field)
        if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value <= 0:
            errors.append(f'experiment.{field}: must be a finite positive number')
    for field in ('warmup_s', 'drain_timeout_s', 'suite_reserve_s', 'patch_at_s', 'measure_lead_s'):
        value = exp.get(field)
        if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0:
            errors.append(f'experiment.{field}: must be a finite nonnegative number')
    if isinstance(exp.get('duration_s'), (int, float)) and isinstance(exp.get('patch_at_s'), (int, float)):
        if any(r['mode'] == 'patch' for r in plan) and exp['patch_at_s'] >= exp['duration_s']:
            errors.append('experiment.patch_at_s: must be < experiment.duration_s')
    if isinstance(exp.get('drain_timeout_s'), (int, float)) and isinstance(exp.get('cleanup_budget_s'), (int, float)):
        if exp['drain_timeout_s'] >= exp['cleanup_budget_s']:
            errors.append('experiment.drain_timeout_s: must be < experiment.cleanup_budget_s')
    if not isinstance(exp.get('repeats'), int) or isinstance(exp.get('repeats'), bool) or exp['repeats'] < 1:
        errors.append('experiment.repeats: must be a positive integer')
    if not isinstance(exp.get('seed'), int):
        errors.append('experiment.seed: must be an integer')
    if not isinstance(exp.get('threads'), list) or not exp['threads'] or any(type(x) is not int or x < 1 for x in exp['threads']):
        errors.append('experiment.threads: must be positive integers')
    combos, ids = set(), set()
    cases = exp.get('cases')
    if not isinstance(cases, list) or not cases:
        errors.append('experiment.cases: required nonempty array')
        cases = []
    for i, case in enumerate(cases):
        if not isinstance(case, dict):
            errors.append(f'experiment.cases[{i}]: must be an object')
            continue
        combo, cid = (case.get('variant'), case.get('mode')), case.get('id')
        if combo in combos or cid in ids:
            errors.append(f'experiment.cases[{i}]: duplicate id or variant+mode')
        combos.add(combo)
        ids.add(cid)
        if combo[0] not in ('original', 'optimized') or combo[1] not in ('steady', 'patch'):
            errors.append(f'experiment.cases[{i}]: unknown variant/mode')
        if not isinstance(cid, str) or not cid.isalnum():
            errors.append(f'experiment.cases[{i}].id: use alphanumeric id')
    if not plan:
        errors.append('experiment.cases: no selected cases')
    for variant in sorted({r['variant'] for r in plan}):
        v = config['variants'].get(variant) or {}
        if not isinstance(v, dict):
            errors.append(f'variants.{variant}: must be an object')
            continue
        for field in ('binary', 'build_manifest'):
            if not v.get(field):
                errors.append(f'variants.{variant}.{field}: required for selected variant')
        if variant == 'optimized':
            mechanisms = v.get('mechanisms') or {}
            for field in ('monitoring_enabled', 'coordination_enabled'):
                if mechanisms.get(field) is not True:
                    errors.append(f'variants.optimized.mechanisms.{field}: must remain true in G3 and G4')
            if not mechanisms.get('activation_description'):
                errors.append('variants.optimized.mechanisms.activation_description: document actual enablement flags/env/config')
        if not isinstance(v.get('extra_argv', []), list) or not all(isinstance(x, str) for x in v.get('extra_argv', [])):
            errors.append(f'variants.{variant}.extra_argv: must be string array')
    argv = workload.get('launch_argv')
    if not isinstance(argv, list) or not argv or not all(isinstance(x, str) for x in argv):
        errors.append('workload.launch_argv: required nonempty argument array')
    for field in ('binary', 'target_symbol'):
        if not workload.get(field):
            errors.append(f'workload.{field}: required')
    if workload.get('control_protocol') != 'stdin-lines-v1':
        errors.append('workload.control_protocol: only stdin-lines-v1 is implemented')
    if workload.get('process_model') != 'single-process-multithread':
        errors.append('workload.process_model: multiprocess aggregation is unsupported')
    if workload.get('metrics_format') not in ('requests-v1', 'counts-v1'):
        errors.append('workload.metrics_format: use requests-v1 or counts-v1')
    if not isinstance(workload.get('ready_timeout_s'), (int, float)) or workload['ready_timeout_s'] <= 0:
        errors.append('workload.ready_timeout_s: must be positive')
    if any(r['mode'] == 'patch' for r in plan):
        action = config['patch'].get('action', 'replace_function')
        if action == 'replace_function':
            for field in ('library', 'replacement_symbol'):
                if not config['patch'].get(field):
                    errors.append(f'patch.{field}: required for patch mode')
        elif action == 'override_return':
            value = config['patch'].get('override_return_value')
            if type(value) is not int or not -(2**31) <= value < 2**31:
                errors.append('patch.override_return_value: signed 32-bit integer required')
        else:
            errors.append('patch.action: expected replace_function or override_return')
        if config['patch'].get('target_index') != 0:
            errors.append('patch.target_index: must be 0 for the single-target template')
        if config['patch'].get('events_adapter') not in (None, 'csv-v1'):
            errors.append('patch.events_adapter: only null or csv-v1 supported')
        if not isinstance(config['patch'].get('http_timeout_s'), (int, float)) or config['patch']['http_timeout_s'] <= 0:
            errors.append('patch.http_timeout_s: must be positive')
    if config['gaia'].get('listen') != '127.0.0.1:17890':
        errors.append('gaia.listen: must be 127.0.0.1:17890')
    for field in ('config_template', 'log_level'):
        if not config['gaia'].get(field):
            errors.append(f'gaia.{field}: required')
    if not config.get('environment_lock'):
        errors.append('environment_lock: required frozen environment JSON path')
    if type(config.get('synthetic')) is not bool:
        errors.append('synthetic: must be a boolean')
    if not isinstance(config.get('output_root'), str) or not config['output_root']:
        errors.append('output_root: required directory path')
    if isinstance(exp.get('warmup_s'), (int, float)) and isinstance(exp.get('measure_lead_s'), (int, float)):
        if exp['measure_lead_s'] <= 0 or exp['warmup_s'] <= exp['measure_lead_s'] + .02:
            errors.append('experiment.warmup_s / measure_lead_s: warmup must leave positive lead plus acknowledgement time')
    # Expand every command with a representative context BEFORE launching anything.
    context = {key: 'placeholder' for key in ('run_id', 'case_id', 'variant', 'mode', 'output_dir', 'run_dir',
               'config_dir', 'status_file', 'requests_file', 'patch_events_file', 'patch_proof_file',
               'optimization_status_file', 'binary', 'gaia_config', 'db_path')}
    context.update(threads=4, repeat=1, seed=1, pid=1, workload_seed=1)
    commands = [('workload.launch_argv', workload.get('launch_argv'))]
    environments = [('workload.env', workload.get('env', {}))]
    for variant in {r['variant'] for r in plan}:
        v = config['variants'].get(variant)
        if not isinstance(v, dict):
            continue
        commands += [(f'variants.{variant}.extra_argv', v.get('extra_argv', []))]
        if 'launch_argv' in v:
            if not config.get('synthetic'):
                errors.append(f'variants.{variant}.launch_argv: reserved for synthetic peers; formal Gaia launches its verified binary')
            commands.append((f'variants.{variant}.launch_argv', v['launch_argv']))
        environments.append((f'variants.{variant}.env', v.get('env', {})))
    for field, argv in commands:
        try:
            if not isinstance(argv, list) or not all(isinstance(a, str) for a in argv):
                raise ValueError('must be string array')
            expand_argv(argv, context)
        except (ValueError, KeyError, TypeError, AttributeError) as exc:
            errors.append(f'{field}: {exc}')
    for field, env in environments:
        try:
            if not isinstance(env, dict) or any(not isinstance(k, str) or not isinstance(v, str) for k,v in env.items()):
                raise ValueError('must map environment names to string values')
            for value in env.values():
                value.format_map(context)
        except (ValueError, KeyError, TypeError, AttributeError) as exc:
            errors.append(f'{field}: {exc}')
    return errors


class SuiteLock:
    """Kernel lock, released on exit/crash; stale pathname alone never blocks a suite."""
    def __init__(self, path):
        self.path = Path(path)
        self.handle = None

    def __enter__(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.handle = self.path.open('a+b')
        try:
            if os.name == 'posix':
                import fcntl
                fcntl.flock(self.handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            else:
                import msvcrt
                if self.path.stat().st_size == 0:
                    self.handle.write(b'0')
                    self.handle.flush()
                self.handle.seek(0)
                msvcrt.locking(self.handle.fileno(), msvcrt.LK_NBLCK, 1)
        except OSError:
            self.handle.close()
            raise RuntimeError('another gaia-perf suite owns the test lock')
        return self

    def __exit__(self, *unused):
        self.handle.close()


def load_config(path):
    path = Path(path).resolve()
    config = read_json(path)
    if not isinstance(config, dict):
        raise ValueError('config: JSON root must be an object')
    base = path.parent
    def resolve(value):
        return str((base / value).resolve()) if value else value
    for key in ('environment_lock', 'output_root'):
        config[key] = resolve(config.get(key))
    if isinstance(config.get('gaia'), dict):
        config['gaia']['config_template'] = resolve(config['gaia'].get('config_template'))
    if isinstance(config.get('variants'), dict):
        for value in config['variants'].values():
            if isinstance(value, dict):
                for key in ('binary', 'build_manifest'):
                    value[key] = resolve(value.get(key))
    if isinstance(config.get('workload'), dict):
        config['workload']['binary'] = resolve(config['workload'].get('binary'))
    if isinstance(config.get('patch'), dict):
        config['patch']['library'] = resolve(config['patch'].get('library'))
    config['_config_dir'] = str(base)
    return config


def check_workload_children(pid):
    if sys.platform != 'linux':
        return
    for path in Path(f'/proc/{pid}/task').glob('*/children'):
        try:
            if path.read_text().strip():
                raise RuntimeError('workload spawned child processes: single-process aggregation contract violated')
        except FileNotFoundError:
            continue


def check_optimization_proof(path, run_id, pid):
    proof = read_json(path)
    if (proof.get('run_id') != run_id or proof.get('pid') != pid or proof.get('clock') != 'CLOCK_MONOTONIC'
            or proof.get('monitoring_enabled') is not True or proof.get('coordination_enabled') is not True
            or not proof.get('evidence') or not isinstance(proof.get('observed_ns'), int)
            or not 0 < proof['observed_ns'] <= mono_ns()):
        raise ValueError('optimization_status.json: require this run/target, actual monitoring+coordination enabled, and evidence')
    return proof


def normalize_requests(raw, target, case_id):
    from analyze import csv_rows
    def annotated():
        for row in csv_rows(raw, REQUEST_FIELDS):
            if row.get('case_id') not in (None, '', case_id):
                raise ValueError('requests.raw.csv case_id mismatch')
            yield {**row, 'case_id': case_id}
    temporary = target.with_suffix('.csv.tmp')
    write_csv(temporary, ['case_id'] + REQUEST_FIELDS, annotated())
    os.replace(temporary, target)


def make_patch_payload(config, variant, pid):
    patch = config['patch']
    action = patch.get('action', 'replace_function')
    payload = {'index': patch['target_index'], 'binary': config['workload']['binary'],
               'symbol': config['workload']['target_symbol'], 'pid': pid,
               'enabled': True, 'patch_action': action}
    if action == 'override_return':
        payload['override_return_value'] = patch['override_return_value']
        if variant == 'optimized':
            payload['patch_mode'] = 'cooperative'
    else:
        payload.update(replace_lib=patch['library'], replace_symbol=patch['replacement_symbol'])
    return payload


def run_one(config, run, directory, hashes):
    from adapters import (ProcessOwner, WorkloadAdapter, AsyncPatch, http_json, monitor_evidence,
                          render_gaia_config, import_patch_events)
    from environment import check_port
    from resources import ResourceSampler
    directory.mkdir(parents=True, exist_ok=False)
    exp, workload = config['experiment'], config['workload']
    owner = ProcessOwner()
    manifest = {**run, 'synthetic': bool(config.get('synthetic')), 'status': 'starting',
                'started_ns': mono_ns(), 'clock': 'CLOCK_MONOTONIC', 'load_model': 'closed-loop fixed concurrency',
                'metrics_format': workload['metrics_format'], 'hashes': hashes, 'errors': [],
                'variant_config': config['variants'][run['variant']], 'monitor_verified': False,
                'optimization_verified': False, 'cpu_convention': 'one core = 100 percent'}
    save = lambda: write_json(directory/'manifest.json', manifest)
    save()
    # Header-only files are valid artifacts for a failed startup; they imply no measurements.
    write_csv(directory/'patch_events.csv', ['case_id']+EVENT_FIELDS, [])
    write_csv(directory/'resources.csv', RESOURCE_FIELDS, [])
    patch_job = sampler = adapter = None
    cleanup_deadline = None
    interrupted = False
    try:
        check_port()
        startup_deadline = manifest['started_ns'] + int(exp['startup_budget_s']*NS)
        paths = {'status_file': directory/'workload_status.json', 'requests_file': directory/'requests.raw.csv',
                 'patch_events_file': directory/'patch_events.raw.csv', 'patch_proof_file': directory/'patch_proof.json',
                 'optimization_status_file': directory/'optimization_status.json'}
        context = {**run, 'seed': run['workload_seed'], 'output_dir': str(directory), 'run_dir': str(directory),
                   'config_dir': config['_config_dir'], **{k: str(v) for k,v in paths.items()},
                   'binary': workload['binary']}
        environment = {'GAIA_PERF_RUN_DIR': str(directory), 'GAIA_PERF_RUN_ID': run['run_id'],
                       'GAIA_PERF_CASE_ID': run['case_id'], 'GAIA_PERF_THREADS': str(run['threads']),
                       'GAIA_PERF_SEED': str(run['workload_seed']), 'GAIA_PERF_ATTEMPT_ID': 'patch-1',
                       'GAIA_PERF_DRAIN_TIMEOUT_S': str(exp['drain_timeout_s']),
                       'GAIA_PERF_STATUS_FILE': str(paths['status_file']),
                       'GAIA_PERF_REQUESTS_FILE': str(paths['requests_file']),
                       'GAIA_PERF_PATCH_EVENTS_FILE': str(paths['patch_events_file']),
                       'GAIA_PERF_PATCH_PROOF_FILE': str(paths['patch_proof_file']),
                       'GAIA_PERF_OPTIMIZATION_STATUS_FILE': str(paths['optimization_status_file'])}
        workload_env = {key: value.format_map(context) for key, value in workload.get('env', {}).items()}
        if workload.get('safepatch_abi'):
            workload_env['GAIA_WORKLOAD_COOPERATIVE'] = '1' if run['variant'] == 'optimized' else '0'
        child = owner.start('workload', expand_argv(workload['launch_argv'], context), directory/'workload.log', directory,
                            {**workload_env, **environment})
        manifest['target_pid'] = child.pid
        adapter = WorkloadAdapter(child, paths['status_file'], run['run_id'])
        ready = adapter.wait_phase('ready', min(startup_deadline, mono_ns()+int(workload['ready_timeout_s']*NS)))
        manifest['workload_ready'] = ready
        check_workload_children(child.pid)
        render_gaia_config(config['gaia']['config_template'], directory/'gaia.toml', workload['binary'], workload['target_symbol'], child.pid)
        (directory/'gaia.initial.toml').write_bytes((directory/'gaia.toml').read_bytes())
        variant = config['variants'][run['variant']]
        context.update(pid=child.pid, gaia_config=str(directory/'gaia.toml'), db_path=str(directory/'events.db'))
        prefix = variant.get('launch_argv') or [variant['binary']]
        command = expand_argv(prefix, context) + ['--config', str(directory/'gaia.toml'),
                  '--web-listen', config['gaia']['listen'], '--db-path', str(directory/'events.db')]
        command += expand_argv(variant.get('extra_argv', []), context)
        gaia_env = {key: value.format_map(context) for key,value in variant.get('env', {}).items()}
        gaia_env.update(environment, RUST_LOG=config['gaia']['log_level'])
        owner.start('gaia', command, directory/'gaia.log', directory, gaia_env)
        manifest['processes'] = owner.records
        save()
        last_error = 'not ready'
        while mono_ns() < startup_deadline:
            owner.assert_alive()
            try:
                reply = http_json('GET', '/api/v1/config/hotpatch', timeout_s=min(.25, (startup_deadline-mono_ns())/NS))
                targets = reply['json']
                if not isinstance(targets, list) or len(targets) != 1:
                    raise ValueError('expected one configured hotpatch target')
                target = targets[0]
                if any(target.get(k) != v for k,v in {'binary': workload['binary'], 'symbol': workload['target_symbol'],
                       'pid': child.pid, 'enabled': True, 'patch_action': 'monitor'}.items()):
                    raise ValueError('Gaia target does not match this round monitor configuration')
                manifest['gaia_initial_target'] = target
                break
            except (OSError, ValueError, http.client.HTTPException) as exc:
                last_error = str(exc)
                time.sleep(.01)
        else:
            raise RuntimeError('Gaia startup timeout: ' + last_error)
        adapter.send('WARMUP')
        warmup = adapter.wait_phase('warmup', startup_deadline)
        manifest['warmup_start_ns'] = warmup['ack_ns']
        start = warmup['ack_ns'] + int(exp['warmup_s']*NS)
        end = start + int(exp['duration_s']*NS)
        # MEASURE must be acknowledged before start; this lead is INSIDE the warmup.
        prepare_deadline = start - int(exp['measure_lead_s']*NS)
        if prepare_deadline <= mono_ns():
            raise RuntimeError('experiment.warmup_s too short for experiment.measure_lead_s and phase acknowledgement')
        evidence = None
        while mono_ns() < prepare_deadline:
            owner.assert_alive()
            try:
                reply = http_json('GET', '/api/v1/state', timeout_s=min(.15, (prepare_deadline-mono_ns())/NS))
                if isinstance(reply['json'], dict):
                    evidence = monitor_evidence(reply['json'], child.pid,
                        optimized_tgid=run['variant'] == 'optimized' and bool(workload.get('safepatch_abi')))
                if evidence:
                    break
            except (OSError, ValueError, http.client.HTTPException):
                pass
            time.sleep(.005)
        if not evidence:
            raise RuntimeError('no target PID uprobe monitoring event observed during warmup')
        manifest['monitor_verified'] = True
        write_json(directory/'monitor_proof.json', evidence)
        if run['variant'] == 'optimized':
            if workload.get('safepatch_abi'):
                observation = http_json('GET', '/api/v1/hotpatch-observation',
                    timeout_s=min(.25, (prepare_deadline-mono_ns())/NS))['json']
                observed = [row for row in (observation or {}).get('calls', [])
                            if row.get('tgid') == child.pid and row.get('last_entry_ns', 0) > 0]
                if (target.get('patch_mode') != 'cooperative' or not observed
                        or warmup.get('cooperative_abi') is not True):
                    raise RuntimeError('optimized observation or cooperative workload ABI not active')
                write_json(paths['optimization_status_file'],
                    {'run_id': run['run_id'], 'pid': child.pid, 'clock': 'CLOCK_MONOTONIC',
                     'monitoring_enabled': True, 'coordination_enabled': True, 'observed_ns': mono_ns(),
                     'evidence': {'target_patch_mode': target['patch_mode'], 'observed_calls': observed,
                                  'observation_errors': observation.get('errors'), 'workload_ack': warmup}})
            proof = check_optimization_proof(paths['optimization_status_file'], run['run_id'], child.pid)
            manifest.update(optimization_verified=True, optimization_proof=proof)
        # Continue the full configured warmup even when monitoring was proved early.
        time.sleep(max(0, (prepare_deadline-mono_ns())/NS))
        adapter.send(f"MEASURE {run['run_id']} {start} {end}")
        manifest['measurement_ack'] = adapter.wait_phase('measure', start, start, end)
        manifest.update(window={'start_ns': start, 'end_ns': end}, drain_deadline_ns=end+int(exp['drain_timeout_s']*NS), status='measuring')
        save()
        sampler = ResourceSampler(directory/'resources.csv', run,
                                  {'workload': child.pid, 'gaia': owner.handles['gaia'].pid, 'runner': os.getpid()},
                                  start, end, exp['sample_interval_s'])
        sampler.start()
        if run['mode'] == 'patch':
            payload = make_patch_payload(config, run['variant'], child.pid)
            patch_job = AsyncPatch(run['run_id'], payload, start+int(exp['patch_at_s']*NS), config['patch']['http_timeout_s'])
            patch_job.start()
        while mono_ns() < end:
            owner.assert_alive()
            time.sleep(min(.01, max(0, (end-mono_ns())/NS)))
        manifest['window_observed_end_ns'] = mono_ns()
        cleanup_deadline = end + int(exp['cleanup_budget_s']*NS)
        check_workload_children(child.pid)
        adapter.send('STOP')
        manifest['stop_sent_ns'] = mono_ns()
        manifest['workload_final'] = adapter.wait_phase('stopped', manifest['drain_deadline_ns'])
        owner.assert_alive()
        if type(manifest['workload_final'].get('generated_count')) is not int:
            raise RuntimeError('workload_status.generated_count: required complete request count at STOP')
        if workload['metrics_format'] == 'requests-v1':
            normalize_requests(paths['requests_file'], directory/'requests.csv', run['case_id'])
        owner.assert_alive()
        manifest['status'] = 'complete'
    except KeyboardInterrupt:
        interrupted = True
        manifest['status'] = 'interrupted'
        manifest['errors'].append('interrupted by user/signal')
    except Exception as exc:
        manifest['status'] = 'failed'
        manifest['errors'].append(f'{type(exc).__name__}: {exc}')
    finally:
        cleanup_deadline = cleanup_deadline or mono_ns()+int(exp['cleanup_budget_s']*NS)
        manifest['cleanup_start_ns'] = mono_ns()
        if manifest['status'] != 'complete' and adapter is not None and not manifest.get('workload_final'):
            try:
                adapter.send('STOP')
                if mono_ns() < cleanup_deadline:
                    final = adapter.wait_phase('stopped', min(cleanup_deadline, mono_ns()+int(exp['drain_timeout_s']*NS)))
                    manifest['workload_final'] = final
            except (OSError, RuntimeError, KeyboardInterrupt):
                pass
        if sampler is not None:
            sampler.join(timeout=min(.1, max(0, (cleanup_deadline-mono_ns())/NS)))
            sampler.stop()
            sampler.join(timeout=.1)
            if sampler.failure:
                manifest['errors'].append('resource sampler: ' + sampler.failure)
        if patch_job is not None:
            patch_job.cancel()
        manifest['processes'] = owner.cleanup(cleanup_deadline)
        for name, process in manifest['processes'].items():
            if process.get('returncode_before_cleanup') is not None or not process.get('exit_requested_by_runner'):
                manifest['errors'].append(f"{name} exited before runner-requested cleanup: returncode={process.get('returncode')}")
                if manifest['status'] in ('complete', 'unverified'):
                    manifest['status'] = 'failed'
        if any(p.get('returncode') is None for p in manifest['processes'].values()):
            manifest['errors'].append('a created process could not be reaped by cleanup deadline')
        if patch_job is not None:
            patch_job.join(timeout=.05)
            events, http_result = patch_job.snapshot()
            write_json(directory/'patch_http.json', http_result)
        else:
            events = []
        if run['mode'] == 'patch' and config['patch'].get('events_adapter') == 'csv-v1':
            try:
                events += import_patch_events(directory/'patch_events.raw.csv')
            except (OSError, ValueError) as exc:
                manifest['errors'].append(str(exc))
        write_csv(directory/'patch_events.csv', ['case_id']+EVENT_FIELDS, [dict(e, case_id=run['case_id']) for e in events])
        if run['mode'] == 'patch':
            from analyze import summarize_patch
            try:
                proof = read_json(directory/'patch_proof.json') if (directory/'patch_proof.json').is_file() else None
                window = manifest.get('window') or {}
                patch_summary = summarize_patch(events, run['run_id'], proof, manifest.get('target_pid'),
                    (window['start_ns'], window['end_ns']) if window else None)
                manifest['patch_verification'] = patch_summary['verification']
                if manifest['status'] == 'complete' and patch_summary['verification'] != 'verified':
                    manifest['status'] = 'failed' if patch_summary['verification'] == 'failed' else 'unverified'
                    if manifest['status'] == 'failed':
                        manifest['errors'].append('patch application-stage failure')
            except (ValueError, OSError) as exc:
                manifest['errors'].append('patch proof: ' + str(exc))
                if manifest['status'] == 'complete':
                    manifest['status'] = 'unverified'
        if (directory/'requests.raw.csv').is_file() and not (directory/'requests.csv').is_file():
            try:
                normalize_requests(directory/'requests.raw.csv', directory/'requests.csv', run['case_id'])
            except (OSError, ValueError) as exc:
                manifest['errors'].append(str(exc))
        manifest['finished_ns'] = mono_ns()
        manifest['elapsed_s'] = (manifest['finished_ns']-manifest['started_ns'])/NS
        manifest['cleanup_budget_exceeded'] = manifest['finished_ns'] > cleanup_deadline
        save()
    return manifest, interrupted


def collect_suite(config, plan, started_ns):
    from analyze import analyze_suite
    from environment import preflight
    root = Path(config['output_root'])
    suite_id = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')+'-'+uuid.uuid4().hex[:8]
    directory = root/suite_id
    directory.mkdir(parents=True, exist_ok=False)
    suite = {'schema_version': 1, 'suite_id': suite_id, 'synthetic': bool(config.get('synthetic')), 'config': config,
             'plan': plan, 'seed': config['experiment']['seed'], 'started_ns': started_ns,
             'wall_started_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'status': 'preflight', 'warnings': [],
             'python': sys.version, 'clock': time.get_clock_info('monotonic')._asdict() if hasattr(time.get_clock_info('monotonic'), '_asdict') else vars(time.get_clock_info('monotonic'))}
    save = lambda: write_json(directory/'suite.json', suite)
    save()
    interrupted = False
    try:
        with SuiteLock(root/'.gaia-perf-17890.lock'):
            errors, warnings, evidence = preflight(config, plan)
            suite.update(preflight=evidence, warnings=warnings)
            if errors:
                raise RuntimeError('\n'.join(errors))
            hashes = {'workload': sha256(config['workload']['binary'])}
            hashes['tool_scripts'] = {path.name: sha256(path) for path in sorted(Path(__file__).parent.glob('*.py'))}
            for variant in {r['variant'] for r in plan}:
                hashes[variant] = sha256(config['variants'][variant]['binary'])
            if (any(r['mode'] == 'patch' for r in plan)
                    and config['patch'].get('action', 'replace_function') == 'replace_function'):
                hashes['patch_library'] = sha256(config['patch']['library'])
            suite.update(status='running', hashes=hashes)
            save()
            for run in plan:
                remaining = config['experiment']['suite_budget_s']-(mono_ns()-started_ns)/NS
                if interrupted or not can_start_round(remaining, config['experiment']):
                    (directory/run['run_id']).mkdir(exist_ok=True)
                    write_json(directory/run['run_id']/'manifest.json', {**run, 'synthetic': suite['synthetic'],
                        'status': 'not_started' if interrupted else 'budget_skipped',
                        'errors': ['suite interrupted' if interrupted else 'remaining budget cannot fit full round plus reserve']})
                    continue
                print(f"{run['run_id']}: {run['variant']} {run['mode']} threads={run['threads']}", flush=True)
                manifest, interrupted = run_one(config, run, directory/run['run_id'], hashes)
                print(f"  {manifest['status']}: {'; '.join(manifest['errors'])}", flush=True)
            suite['status'] = 'interrupted' if interrupted else 'collected'
    except KeyboardInterrupt:
        interrupted = True
        suite.update(status='interrupted', errors=['interrupted during suite setup/finalization'])
    except Exception as exc:
        suite.update(status='failed', errors=[f'{type(exc).__name__}: {exc}'])
    suite['collection_finished_ns'] = mono_ns()
    save()
    try:
        summaries = analyze_suite(directory)
        complete = (len(summaries) == len(plan) and all(r.get('measurement_valid') for r in summaries))
        if suite['status'] not in ('failed', 'interrupted'):
            suite['status'] = 'complete' if complete else 'incomplete'
        suite['valid_formal_runs'] = sum(bool(r.get('valid_for_comparison')) for r in summaries)
        suite['measurement_valid_runs'] = sum(bool(r.get('measurement_valid')) for r in summaries)
        suite['not_executed_runs'] = sum(r.get('status') in ('not_started', 'budget_skipped') for r in summaries)
    except Exception as exc:
        suite.update(status='analysis_failed', analysis_error=repr(exc))
    suite['finished_ns'] = mono_ns()
    suite['elapsed_s'] = (suite['finished_ns']-started_ns)/NS
    suite['suite_budget_exceeded'] = suite['elapsed_s'] > config['experiment']['suite_budget_s']
    save()
    # Rewrite the small report status after suite finalization, without parsing requests twice.
    report = directory/'report.md'
    if report.is_file():
        text = report.read_text(encoding='utf-8')
        text = text.replace('套件状态：collected；', f"套件状态：{suite['status']}；")
        report.write_text(text, encoding='utf-8')
    print(f"{suite['status']} -> {directory}", flush=True)
    return 130 if interrupted else 0 if suite['status'] == 'complete' else 1


def main():
    started = mono_ns()
    parser = argparse.ArgumentParser(description=__doc__)
    if sys.version_info < (3, 11):
        parser.exit(2, 'environment.python: Python >= 3.11 required (stdlib tomllib); tested locally on 3.12\n')
    parser.add_argument('--config', type=Path, default=Path(__file__).with_name('config.json'))
    parser.add_argument('--variant', choices=('original', 'optimized', 'both'), default='both')
    parser.add_argument('--mode', choices=('steady', 'patch', 'both'), default='both')
    parser.add_argument('--threads', type=int, nargs='+')
    parser.add_argument('--repeats', type=int)
    parser.add_argument('--warmup', type=float)
    parser.add_argument('--duration', type=float)
    parser.add_argument('--seed', type=int)
    parser.add_argument('--dry-run', action='store_true', help='validate config/files and print plan; no processes')
    parser.add_argument('--preflight', action='store_true', help='also validate live Linux environment/port; no processes')
    parser.add_argument('--synthetic', action='store_true', help='explicit opt-in for synthetic:true test configurations ONLY')
    parser.add_argument('--export-environment', type=Path)
    parser.add_argument('--cargo-lock', type=Path, default=Path('/root/Gaia-SafePatch/Cargo.lock'))
    args = parser.parse_args()
    if args.export_environment:
        from environment import observe_environment
        try:
            write_json(args.export_environment, observe_environment(args.cargo_lock))
        except (ValueError, OSError, subprocess.SubprocessError) as exc:
            parser.exit(2, f'environment export: {exc}\n')
        print(args.export_environment.resolve())
        return 0
    try:
        config = load_config(args.config)
        if bool(config.get('synthetic')) != args.synthetic:
            raise ValueError('synthetic configuration requires BOTH synthetic:true AND explicit --synthetic; formal configs cannot use --synthetic')
        exp = config.get('experiment', {})
        for field, value in [('threads', args.threads), ('repeats', args.repeats), ('warmup_s', args.warmup),
                             ('duration_s', args.duration), ('seed', args.seed)]:
            if value is not None:
                exp[field] = value
        # Guard structurally invalid input before expanding ranges or touching the filesystem.
        if not isinstance(exp, dict):
            raise ValueError('experiment: must be an object')
        variants = None if args.variant == 'both' else [args.variant]
        modes = None if args.mode == 'both' else [args.mode]
        plan = make_plan(config, variants, modes)
        errors = validate_config(config, plan)
        if args.dry_run or args.preflight:
            print(json.dumps({'synthetic': config.get('synthetic'), 'rounds': len(plan), 'seed': exp.get('seed'), 'plan': plan}, ensure_ascii=False, indent=2))
        if errors:
            raise ValueError('\n'.join(errors))
        if args.dry_run or args.preflight:
            from environment import preflight
            errors, warnings, evidence = preflight(config, plan, check_environment=args.preflight)
            for warning in warnings:
                print('WARNING: ' + warning)
            if errors:
                raise ValueError('\n'.join(errors))
            print('Preflight OK (no workload/Gaia launched)')
            return 0
    except (ValueError, OSError, TypeError, KeyError) as exc:
        parser.exit(2, str(exc)+'\n')
    # Convert termination to the same data-preserving cleanup path as Ctrl+C.
    def interrupted(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    return collect_suite(config, plan, started)


if __name__ == '__main__':
    raise SystemExit(main())
