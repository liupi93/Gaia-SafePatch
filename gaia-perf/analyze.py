"""Deterministic offline analysis. Never substitutes HTTP success for patch evidence."""
import argparse
import csv
import math
import statistics
from array import array
from collections import defaultdict
from pathlib import Path

from common import NS, EVENT_FIELDS, REQUEST_FIELDS, read_json, write_json, write_csv, sha256


def percentile(values, p):
    """Nearest-rank quantile: sorted[ceil(p*n)-1]. Empty => missing, never zero."""
    if not values:
        return None
    return sorted(values)[max(0, math.ceil(p * len(values)) - 1)]


def latency_stats(values):
    ordered = sorted(values)
    n = len(ordered)
    return {'latency_n': n, 'latency_mean_ms': statistics.fmean(ordered) if n else None,
            'latency_p95_ms': ordered[math.ceil(.95*n)-1] if n else None,
            'latency_p99_ms': ordered[math.ceil(.99*n)-1] if n else None}


def summarize_requests(rows, start_ns, end_ns, run_id, drain_end_ns=None):
    if end_ns <= start_ns:
        raise ValueError('window.end_ns must be > start_ns')
    bins = [{'second': i, 'start_ns': start_ns+i*NS, 'end_ns': min(end_ns, start_ns+(i+1)*NS),
             'completed_ok': 0, '_latencies': array('d')} for i in range(math.ceil((end_ns-start_ns)/NS))]
    latencies, seen, errors = array('d'), set(), []
    counts = dict(generated=0, completed_ok_in_window=0, completed_in_window=0,
                  warmup_completed_in_window=0, warmup_completed_ok_in_window=0,
                  failed=0, failed_in_window=0, unfinished=0, drain_completed=0, completed_total=0)
    for number, raw in enumerate(rows, 2):
        try:
            if raw.get('run_id') != run_id:
                raise ValueError('run_id mismatch')
            phase = raw.get('phase')
            if phase not in ('warmup', 'measure'):
                raise ValueError('phase must be warmup or measure')
            if raw.get('worker_id') in (None, '') or raw.get('request_id') in (None, ''):
                raise ValueError('worker_id and request_id must be nonempty')
            key = (phase, str(raw.get('worker_id')), str(raw.get('request_id')))
            if key in seen:
                raise ValueError('duplicate worker_id/request_id/phase')
            seen.add(key)
            arrival = int(raw['arrival_ns'])
            begin = int(raw['start_ns']) if raw.get('start_ns') not in (None, '') else None
            end = int(raw['end_ns']) if raw.get('end_ns') not in (None, '') else None
            status = raw['status']
            if status not in ('ok', 'error', 'timeout', 'cancelled'):
                raise ValueError('unknown status')
            if phase == 'measure' and not start_ns <= arrival < end_ns:
                raise ValueError('measure arrival outside [start_ns,end_ns)')
            if phase == 'warmup' and arrival >= start_ns:
                raise ValueError('warmup arrival must precede formal window')
            if begin is not None and (begin < arrival or (end is not None and begin > end)):
                raise ValueError('start_ns is not between arrival_ns and end_ns')
            if end is not None and (end < arrival or (drain_end_ns is not None and end > drain_end_ns)):
                raise ValueError('end_ns before arrival or after drain deadline')
            # Throughput is a completion-window metric, independent of the arrival cohort.
            # A warmup request may carry over across start_ns and complete during measurement.
            if end is not None and start_ns <= end < end_ns:
                counts['completed_in_window'] += 1
                counts['completed_ok_in_window'] += status == 'ok'
                counts['failed_in_window'] += status != 'ok'
                bins[(end-start_ns)//NS]['completed_ok'] += status == 'ok'
                if phase == 'warmup':
                    counts['warmup_completed_in_window'] += 1
                    counts['warmup_completed_ok_in_window'] += status == 'ok'
            if phase == 'warmup':
                continue
            counts['generated'] += 1
            if end is None:
                counts['unfinished'] += 1
                continue
            counts['completed_total'] += 1
            counts['failed'] += status != 'ok'
            latency = (end - arrival) / 1_000_000
            latencies.append(latency)
            if end < end_ns:
                bucket = bins[(end-start_ns)//NS]
                bucket['_latencies'].append(latency)
            else:
                counts['drain_completed'] += 1
        except (ValueError, KeyError, TypeError) as exc:
            errors.append(f'requests.csv row {number}: {exc}')
    for bucket in bins:
        bucket.update(latency_stats(bucket.pop('_latencies')))
        bucket['qps'] = bucket['completed_ok'] / ((bucket['end_ns']-bucket['start_ns'])/NS)
    return {**counts, **latency_stats(latencies), 'errors': errors,
            'duration_s': (end_ns-start_ns)/NS,
            'qps': counts['completed_ok_in_window']/((end_ns-start_ns)/NS) if not errors else None,
            'timeline': bins, 'latency_population': 'terminal requests generated in window, including drain; censored excluded',
            'quantile_method': 'nearest-rank'}


def request_timeline(rows, start_ns, end_ns):
    rows = list(rows)
    return summarize_requests(rows, start_ns, end_ns, rows[0]['run_id'] if rows else '')['timeline']


def summarize_patch(events, run_id, proof, target_pid=None, window=None):
    markers, outcomes, errors = {}, {}, []
    known = {'request_sent', 'http_done', 'request_received', 'apply_begin', 'apply_end',
             'admission_closed', 'admission_resumed', 'write_begin', 'write_end'}
    for row in events:
        try:
            if row.get('run_id') != run_id or row.get('attempt_id') != 'patch-1':
                raise ValueError('run_id/attempt_id mismatch')
            event = row['event']
            if event not in known or event in markers:
                raise ValueError('unknown or duplicate event: ' + event)
            markers[event] = int(row['mono_ns'])
            outcomes[event] = row.get('outcome', '')
        except (ValueError, KeyError, TypeError) as exc:
            errors.append(f'patch_events.csv: {exc}')

    def elapsed(a, b):
        if a not in markers or b not in markers:
            return None
        if markers[b] < markers[a]:
            errors.append(f'patch_events.csv: {b} precedes {a}')
            return None
        return (markers[b]-markers[a])/1_000_000

    result = {'http_rtt_ms': elapsed('request_sent', 'http_done'),
              'prepare_ms': elapsed('request_received', 'apply_begin'),
              'apply_ms': elapsed('apply_begin', 'apply_end'),
              'admission_closed_ms': elapsed('admission_closed', 'admission_resumed'),
              'submit_to_apply_ms': None, 'markers': markers, 'errors': errors,
              'observed_ns': proof.get('observed_ns') if isinstance(proof, dict) else None}
    submit = elapsed('request_sent', 'apply_end')
    # Stage ordering is checked even if one of the duration pairs was absent.
    order = [markers[k] for k in ('request_sent', 'request_received', 'apply_begin', 'apply_end') if k in markers]
    if order != sorted(order):
        errors.append('patch_events.csv: application stage order invalid')
    # Any explicit server-stage failure invalidates the attempt, even when a later
    # record claims success. Empty begin/received outcomes remain permitted.
    failed = any(value not in ('', 'ok', 'success') for event, value in outcomes.items()
                 if event not in ('request_sent', 'http_done'))
    failed = failed or ('apply_end' in markers and outcomes.get('apply_end') not in ('ok', 'success'))
    successful = not failed and outcomes.get('apply_end') in ('ok', 'success') and result['apply_ms'] is not None
    evidence = False
    if isinstance(proof, dict):
        observation = proof.get('observed_ns')
        evidence = (proof.get('run_id') == run_id and proof.get('attempt_id') == 'patch-1'
                    and proof.get('new_implementation') is True and bool(proof.get('evidence'))
                    and (target_pid is None or proof.get('pid') == target_pid)
                    and isinstance(observation, int) and observation >= markers.get('request_sent', 2**63))
        if window and isinstance(observation, int) and observation > window[1]:
            evidence = False
    in_window = (not window or (window[0] <= markers.get('request_sent', -1) < window[1]
                               and window[0] <= markers.get('apply_end', -1) <= window[1]))
    if successful and submit is not None and not errors:
        result['submit_to_apply_ms'] = submit
    result['verification'] = ('failed' if failed else 'verified' if successful and evidence and in_window and not errors else 'unverified')
    result['reason'] = ('explicit failure in application-stage outcomes' if failed else '' if result['verification'] == 'verified' else
                        'requires ordered successful application markers and target new-implementation evidence within measurement window')
    return result


def csv_rows(path, required):
    with Path(path).open(encoding='utf-8-sig', newline='') as handle:
        reader = csv.DictReader(handle)
        missing = set(required) - set(reader.fieldnames or [])
        if missing:
            raise ValueError(f'{Path(path).name}: missing columns {sorted(missing)}')
        yield from reader


def summarize_resources(path, start_ns, end_ns, run_id, case_id):
    grouped, errors = defaultdict(list), []
    if not path.is_file():
        return {}, ['resources.csv missing']
    try:
        for row in csv_rows(path, ('entity', 'run_id', 'case_id', 'mono_ns')):
            if row['run_id'] != run_id or row['case_id'] != case_id:
                raise ValueError('resources.csv run_id/case_id mismatch')
            grouped[row['entity']].append(row)
    except (ValueError, OSError) as exc:
        errors.append(str(exc))
    result = {}
    for entity, rows in grouped.items():
        cpu_sum = coverage = steal_sum = steal_coverage = 0
        rss = []
        entity_errors = []
        for row in rows:
            if row.get('error'):
                entity_errors.append(row['error'])
                continue
            try:
                stamp = int(row['mono_ns'])
                interval_start = int(row.get('interval_start_ns') or stamp)
                overlap = max(0, min(end_ns, stamp)-max(start_ns, interval_start)) / NS
                for field in ('cpu_pct', 'steal_pct', 'rss_mib'):
                    if row.get(field) and (not math.isfinite(float(row[field])) or float(row[field]) < 0):
                        raise ValueError('nonfinite/negative resource counter')
                if row.get('cpu_pct') not in (None, '') and overlap > 0:
                    cpu_sum += float(row['cpu_pct']) * overlap
                    coverage += overlap
                if row.get('steal_pct') not in (None, '') and overlap > 0:
                    steal_sum += float(row['steal_pct']) * overlap
                    steal_coverage += overlap
                if start_ns <= stamp < end_ns and row.get('rss_mib') not in (None, ''):
                    rss.append(float(row['rss_mib']))
            except (ValueError, TypeError) as exc:
                entity_errors.append(str(exc))
        result[entity] = {'cpu_mean_pct': cpu_sum/coverage if coverage else None,
                          'cpu_coverage_s': coverage, 'rss_mean_mib': statistics.fmean(rss) if rss else None,
                          'rss_sample_peak_mib': max(rss) if rss else None, 'rss_samples': len(rss),
                          'steal_mean_pct': steal_sum/steal_coverage if steal_coverage else None,
                          'errors': sorted(set(entity_errors))}
    return result, errors


def summarize_counts(path, run_id, start_ns, end_ns):
    """Optional full-count adapter; cannot recover per-request latency or queueing."""
    data = read_json(path)
    if (data.get('run_id') != run_id or data.get('start_ns') != start_ns or data.get('end_ns') != end_ns):
        raise ValueError('counts.json: run_id/window mismatch')
    fields = ('generated', 'completed_ok_in_window', 'completed_in_window', 'failed', 'unfinished', 'drain_completed', 'completed_total')
    if any(type(data.get(k)) is not int or data[k] < 0 for k in fields):
        raise ValueError('counts.json: nonnegative full integer counts required: ' + ','.join(fields))
    for field in ('warmup_completed_in_window', 'warmup_completed_ok_in_window'):
        data.setdefault(field, 0)
        if type(data[field]) is not int or data[field] < 0:
            raise ValueError('counts.json: invalid ' + field)
    warmup_all, warmup_ok = data['warmup_completed_in_window'], data['warmup_completed_ok_in_window']
    formal_window_all = data['completed_in_window']-warmup_all
    formal_window_ok = data['completed_ok_in_window']-warmup_ok
    if (not 0 <= warmup_ok <= warmup_all <= data['completed_in_window']
            or not 0 <= formal_window_ok <= formal_window_all
            or formal_window_all + data['drain_completed'] != data['completed_total']
            or data['completed_total'] + data['unfinished'] != data['generated']
            or not formal_window_all-formal_window_ok <= data['failed'] <= data['completed_total']-formal_window_ok):
        raise ValueError('counts.json: inconsistent full counts')
    return {**{k: data[k] for k in fields}, 'warmup_completed_in_window': warmup_all,
            'warmup_completed_ok_in_window': warmup_ok,
            'failed_in_window': data['completed_in_window']-data['completed_ok_in_window'],
            **latency_stats([]), 'errors': [],
            'qps': data['completed_ok_in_window']/((end_ns-start_ns)/NS),
            'duration_s': (end_ns-start_ns)/NS, 'timeline': [],
            'latency_population': 'unavailable: counts-only workload'}


def analyze_run(run_dir):
    run_dir = Path(run_dir)
    manifest = read_json(run_dir / 'manifest.json')
    summary = {key: manifest.get(key) for key in ('run_id', 'case_id', 'variant', 'mode', 'threads', 'repeat', 'synthetic', 'status')}
    summary.update(analysis_schema_version=1, analyzer_sha256=sha256(__file__))
    errors = list(manifest.get('errors', []))
    window = manifest.get('window') or {}
    start, end = window.get('start_ns'), window.get('end_ns')
    requests, resources, patch = {}, {}, {}
    timeline = []
    if isinstance(start, int) and isinstance(end, int) and end > start:
        try:
            if manifest.get('metrics_format') == 'counts-v1':
                requests = summarize_counts(run_dir/'counts.json', manifest['run_id'], start, end)
            else:
                def validated_rows():
                    for raw in csv_rows(run_dir/'requests.csv', REQUEST_FIELDS):
                        if raw.get('case_id') not in (None, '', manifest['case_id']):
                            raise ValueError('requests.csv: case_id mismatch')
                        yield raw
                requests = summarize_requests(validated_rows(), start, end,
                                              manifest['run_id'], manifest.get('drain_deadline_ns'))
            errors.extend(requests['errors'])
            expected = manifest.get('workload_final', {}).get('generated_count')
            if expected is not None and expected != requests['generated']:
                errors.append('workload generated_count differs from requests/counts export')
            timeline = requests.pop('timeline')
        except (ValueError, OSError, TypeError) as exc:
            errors.append(str(exc))
        resources, resource_errors = summarize_resources(run_dir/'resources.csv', start, end, manifest['run_id'], manifest['case_id'])
        summary['resource_warnings'] = resource_errors
        if manifest['mode'] == 'patch':
            try:
                proof = read_json(run_dir/'patch_proof.json') if (run_dir/'patch_proof.json').is_file() else None
                patch = summarize_patch(list(csv_rows(run_dir/'patch_events.csv', EVENT_FIELDS)), manifest['run_id'], proof,
                                        manifest.get('target_pid'), (start, end))
                errors.extend(patch['errors'])
            except (OSError, ValueError) as exc:
                errors.append(str(exc))
                patch = {'verification': 'unverified', 'reason': str(exc)}
    else:
        errors.append('measurement window unavailable')
    if not manifest.get('monitor_verified'):
        errors.append('target monitoring not verified')
    if manifest.get('variant') == 'optimized' and not manifest.get('optimization_verified'):
        errors.append('optimization monitoring/coordination enablement not verified')
    if manifest.get('mode') == 'patch' and patch.get('verification') != 'verified':
        errors.append('patch application unverified/failed')
    measurement_valid = manifest.get('status') == 'complete' and not errors and requests.get('qps') is not None
    summary.update(requests=requests, resources=resources, patch=patch, errors=errors,
                   measurement_valid=measurement_valid,
                   valid_for_comparison=measurement_valid and manifest.get('synthetic') is False)
    for bucket in timeline:
        bucket.update(case_id=manifest['case_id'], run_id=manifest['run_id'])
        # Raw resources keep the actual timestamps. Timeline attaches interval-overlap CPU
        # and samples INSIDE this second; no forward filling across missing samples.
        per_second, _ = summarize_resources(run_dir/'resources.csv', bucket['start_ns'], bucket['end_ns'],
                                            manifest['run_id'], manifest['case_id'])
        for entity in ('workload', 'gaia', 'runner', 'system'):
            stats = per_second.get(entity, {})
            bucket[f'{entity}_cpu_pct'] = stats.get('cpu_mean_pct')
            bucket[f'{entity}_rss_mib'] = stats.get('rss_mean_mib')
        bucket['system_steal_pct'] = per_second.get('system', {}).get('steal_mean_pct')
        marks = (patch.get('markers') or {})
        bucket['patch_request_sent_ns'] = marks.get('request_sent') if bucket['start_ns'] <= marks.get('request_sent', -1) < bucket['end_ns'] else None
        bucket['patch_apply_end_ns'] = marks.get('apply_end') if bucket['start_ns'] <= marks.get('apply_end', -1) < bucket['end_ns'] else None
    fields = ['case_id', 'run_id', 'second', 'start_ns', 'end_ns', 'completed_ok', 'qps',
              'latency_n', 'latency_mean_ms', 'latency_p95_ms', 'latency_p99_ms']
    fields += [f'{entity}_{metric}' for entity in ('workload', 'gaia', 'runner', 'system') for metric in ('cpu_pct', 'rss_mib')]
    fields += ['system_steal_pct', 'patch_request_sent_ns', 'patch_apply_end_ns']
    write_csv(run_dir/'timeline.csv', fields, timeline)
    write_json(run_dir/'summary.json', summary)
    return summary


METRICS = {'qps': ('requests', 'qps'), 'p95_ms': ('requests', 'latency_p95_ms'),
           'p99_ms': ('requests', 'latency_p99_ms'), 'latency_n': ('requests', 'latency_n'),
           'http_ms': ('patch', 'http_rtt_ms'), 'apply_ms': ('patch', 'apply_ms'),
           'submit_to_apply_ms': ('patch', 'submit_to_apply_ms')}
for _entity in ('workload', 'gaia', 'runner', 'system'):
    for _metric in ('cpu_mean_pct', 'rss_mean_mib', 'rss_sample_peak_mib', 'steal_mean_pct'):
        METRICS[f'{_entity}_{_metric}'] = ('resources', _entity, _metric)


def metric(run, name):
    value = run
    for key in METRICS[name]:
        value = value.get(key) if isinstance(value, dict) else None
    return value


def paired_rows(summaries):
    indexed = {(r.get('case_id'), r.get('threads'), r.get('repeat')): r for r in summaries}
    output = []
    for base_id, candidate_id in (('G1', 'G3'), ('G2', 'G4'), ('G3', 'G4')):
        for threads, repeat in sorted({(r['threads'], r['repeat']) for r in summaries if r.get('threads') is not None}):
            base, candidate = indexed.get((base_id, threads, repeat)), indexed.get((candidate_id, threads, repeat))
            row = {'comparison': f'{candidate_id}/{base_id}', 'threads': threads, 'repeat': repeat,
                   'base_run_id': base.get('run_id') if base else None,
                   'candidate_run_id': candidate.get('run_id') if candidate else None}
            valid = base and candidate and base.get('valid_for_comparison') and candidate.get('valid_for_comparison')
            row['reason'] = '' if valid else 'missing, failed, unverified or synthetic run'
            for name in METRICS:
                b, c = metric(base or {}, name), metric(candidate or {}, name)
                row[name+'_delta'] = c-b if valid and b is not None and c is not None else None
                row[name+'_increase_pct'] = (c-b)/b*100 if valid and b not in (None, 0) and c is not None else None
                if valid and (b is None or c is None or b == 0):
                    row['reason'] = 'some metrics unavailable or zero denominator; corresponding cells blank'
            value = row.get('qps_increase_pct')
            row['throughput_drop_pct'] = -value if value is not None else None
            output.append(row)
    return output


def fmt(value):
    return '—' if value is None else f'{value:.4g}' if isinstance(value, (int, float)) else str(value)


def analyze_suite(root, plots=False):
    root = Path(root)
    suite = read_json(root/'suite.json')
    summaries = []
    for run in suite.get('plan', []):
        path = root/run['run_id']
        if (path/'manifest.json').is_file():
            try:
                summaries.append(analyze_run(path))
            except (ValueError, OSError, KeyError, TypeError) as exc:
                summaries.append({**run, 'status': 'analysis_failed', 'synthetic': suite.get('synthetic'),
                                  'valid_for_comparison': False, 'errors': [str(exc)]})
        else:
            summaries.append({**run, 'status': 'not_started', 'synthetic': suite.get('synthetic'),
                              'valid_for_comparison': False, 'errors': ['manifest missing / run not started']})
    grouped = defaultdict(list)
    for run in summaries:
        grouped[(run['case_id'], run['variant'], run['mode'], run['threads'], bool(run.get('synthetic')))].append(run)
    comparisons = []
    for (cid, variant, mode, threads, synthetic), runs in sorted(grouped.items()):
        good = [r for r in runs if r.get('valid_for_comparison')]
        row = {'case_id': cid, 'variant': variant, 'mode': mode, 'threads': threads, 'synthetic': synthetic,
               'planned_runs': len(runs), 'valid_runs': len(good),
               'reason': '' if len(good) == len(runs) else 'see per-run failures/unverified/synthetic status'}
        for name in METRICS:
            values = [metric(r, name) for r in good if metric(r, name) is not None]
            row.update({name+'_values': ';'.join(str(v) for v in values), name+'_n': len(values),
                        name+'_mean': statistics.fmean(values) if values else None,
                        name+'_min': min(values) if values else None, name+'_max': max(values) if values else None})
        comparisons.append(row)
    fields = ['case_id', 'variant', 'mode', 'threads', 'synthetic', 'planned_runs', 'valid_runs', 'reason']
    fields += [name+'_'+suffix for name in METRICS for suffix in ('values', 'n', 'mean', 'min', 'max')]
    write_csv(root/'comparison.csv', fields, comparisons)
    pairs = paired_rows(summaries)
    write_csv(root/'paired.csv', ['comparison', 'threads', 'repeat', 'base_run_id', 'candidate_run_id', 'reason', 'throughput_drop_pct']
              + [name+'_'+suffix for name in METRICS for suffix in ('delta', 'increase_pct')], pairs)
    run_fields = ['case_id', 'run_id', 'threads', 'repeat', 'status', 'synthetic', 'valid_for_comparison', 'reason'] + list(METRICS)
    write_csv(root/'runs.csv', run_fields, [{**r, 'reason': '; '.join(r.get('errors', [])), **{m: metric(r, m) for m in METRICS}} for r in summaries])
    report = ['# Gaia 性能报告', '', f"套件状态：{suite.get('status', 'unknown')}；计划 {len(summaries)} 轮；正式有效 {sum(bool(r.get('valid_for_comparison')) for r in summaries)} 轮。", '']
    if suite.get('synthetic'):
        report += ['**synthetic：仅用于测试脚本自身；不是 Gaia 性能数据，不得用于原版/优化版性能结论。CPU/RSS 在非 Linux 上留空。**', '']
    report += ['固定并发封闭式负载，每个工作线程一次一个请求。CPU 为单核 100%，允许多线程进程超过 100%；RSS 为 MiB，最大值是采样峰值。',
               '窗口为 [start_ns,end_ns)，统一 CLOCK_MONOTONIC。成功完成 QPS 计算窗口内全部成功完成数（包括预热请求跨入窗口的完成）；延迟为正式窗口内生成请求的 end−arrival（含等待与收尾）；未完成/删失不填零。分位数为该到达队列已结束请求（包括失败）的 nearest-rank，显示样本数。',
               '时间线按完成秒聚合该秒的完整请求延迟；空秒 QPS=0、延迟为空。资源 CPU 按实际采样间隔与窗口重叠加权，边界采样可能略晚，原始时间戳及覆盖秒数保留。',
               '每轮统计独立计算；comparison.csv 的 P95/P99 均值是每轮分位数均值，未合并全部请求。两次重复仅报告均值/范围/有效轮数，不作高置信度显著性结论。', '',
               '|组|线程|重复|状态|QPS|P95 ms|P99 ms|延迟 n|未完成|正式有效|', '|---|---:|---:|---|---:|---:|---:|---:|---:|---|']
    for run in summaries:
        req = run.get('requests', {})
        report.append('|'+ '|'.join(map(fmt, [run['case_id'], run['threads'], run['repeat'], run['status'], req.get('qps'), req.get('latency_p95_ms'), req.get('latency_p99_ms'), req.get('latency_n'), req.get('unfinished'), run.get('valid_for_comparison')]))+'|')
    report += ['', '|组|线程|有效/计划|每轮 QPS|均值|范围|每轮 P99 ms|P99 均值/范围|', '|---|---:|---:|---|---:|---|---|---|']
    for c in comparisons:
        report.append(f"|{c['case_id']}|{c['threads']}|{c['valid_runs']}/{c['planned_runs']}|{c['qps_values']}|{fmt(c['qps_mean'])}|{fmt(c['qps_min'])}–{fmt(c['qps_max'])}|{c['p99_ms_values']}|{fmt(c['p99_ms_mean'])} / {fmt(c['p99_ms_min'])}–{fmt(c['p99_ms_max'])}|")
    report += ['', '|运行|对象|CPU 平均 %（单核100%）|CPU 覆盖秒|RSS 平均 MiB|RSS 采样峰值 MiB|RSS n|steal %|',
               '|---|---|---:|---:|---:|---:|---:|---:|']
    for run in summaries:
        for entity in ('workload', 'gaia', 'runner', 'system'):
            resource = run.get('resources', {}).get(entity, {})
            report.append('|'+ '|'.join(map(fmt, [run['run_id'], entity, resource.get('cpu_mean_pct'), resource.get('cpu_coverage_s'), resource.get('rss_mean_mib'), resource.get('rss_sample_peak_mib'), resource.get('rss_samples'), resource.get('steal_mean_pct')]))+'|')
    report += ['', '|运行|HTTP ms|准备/协调 ms|应用 ms|提交至应用 ms|暂停准入 ms|验证|', '|---|---:|---:|---:|---:|---:|---|']
    for run in summaries:
        if run['mode'] == 'patch':
            p = run.get('patch', {})
            report.append('|'+ '|'.join(map(fmt, [run['run_id'], p.get('http_rtt_ms'), p.get('prepare_ms'), p.get('apply_ms'), p.get('submit_to_apply_ms'), p.get('admission_closed_ms'), p.get('verification', 'unverified')]))+'|')
    report += ['', '对照配对见 paired.csv：G3/G1、G4/G2、G4/G3，同线程、同重复。吞吐下降率=(基准QPS−候选QPS)/基准QPS×100%；延迟增加率=(候选延迟−基准延迟)/基准延迟×100%。CPU 差值单位是百分点，RSS 差值是 MiB。分母为零、未验证或缺失时留空并记录原因。', '', '## 失败、缺项和告警', '']
    report += ['如果新旧业务实现本身计算量不同，前后性能差异不能全部归因于切换机制。', '']
    for run in summaries:
        warnings = list(run.get('errors', [])) + list(run.get('resource_warnings', []))
        for entity, stats in run.get('resources', {}).items():
            warnings.extend(f'{entity}: {msg}' for msg in stats.get('errors', []))
            if entity == 'system' and (stats.get('steal_mean_pct') or 0) >= 5:
                warnings.append('system steal >= 5%，记录云主机波动，未删除本轮')
        if warnings or not run.get('valid_for_comparison'):
            report.append(f"- {run['run_id']}: " + ('；'.join(warnings) or 'synthetic / not a formal valid run'))
    for warning in suite.get('warnings', []):
        report.append('- ' + str(warning))
    report += ['', '应用成功需要等价阶段的 apply_begin/apply_end 和目标程序新实现证据。HTTP、reload success 或第一次观察时刻均不代替真实应用结束时刻。未完成矩阵不会标为完整；原始文件保留。', '']
    (root/'report.md').write_text('\n'.join(report), encoding='utf-8')
    if plots:
        make_plots(root, summaries)
    return summaries


def make_plots(root, summaries):
    try:
        import matplotlib
        matplotlib.use('Agg')
        import matplotlib.pyplot as plt
    except ImportError as exc:
        raise RuntimeError('optional --plots requires matplotlib; collection and CSV/report analysis need no third-party packages') from exc
    target = root/'figures'
    target.mkdir(exist_ok=True)
    real = [run for run in summaries if run.get('valid_for_comparison')]
    if real:
        fig, axes = plt.subplots(3, 2, figsize=(14, 10))
        labels = [f"{r['case_id']} t{r['threads']} r{r['repeat']}" for r in real]
        for ax, name, label in zip(axes.flat,
            ('qps', 'p99_ms', 'workload_cpu_mean_pct', 'gaia_cpu_mean_pct', 'workload_rss_mean_mib', 'gaia_rss_mean_mib'),
            ('QPS', 'Per-run P99 (ms)', 'Workload CPU % (one core=100)', 'Gaia CPU % (one core=100)', 'Workload RSS (MiB)', 'Gaia RSS (MiB)')):
            ax.bar(labels, [metric(r, name) if metric(r, name) is not None else math.nan for r in real])
            ax.set_ylabel(label)
            ax.tick_params(axis='x', rotation=65, labelsize=7)
        fig.tight_layout()
        fig.savefig(target/'comparison.png', dpi=140)
        plt.close(fig)
    for run in summaries:
        path = root/run['run_id']/'timeline.csv'
        if not path.is_file():
            continue
        rows = list(csv_rows(path, ('second', 'qps')))
        if not rows:
            continue
        fig, axes = plt.subplots(3, 1, figsize=(10, 8), sharex=True)
        for ax, field, label in zip(axes, ('qps', 'latency_p99_ms', 'gaia_cpu_pct'), ('QPS', 'P99 ms', 'Gaia CPU % (one core=100)')):
            ax.plot([float(r['second']) for r in rows], [float(r[field]) if r.get(field) else math.nan for r in rows], marker='.')
            ax.set_ylabel(label)
            for event, color in (('request_sent', 'orange'), ('apply_end', 'green')):
                value = run.get('patch', {}).get('markers', {}).get(event)
                if value is not None:
                    ax.axvline((value-int(rows[0]['start_ns']))/NS, color=color, linestyle='--', label=event)
            ax.grid(alpha=.2)
        axes[-1].set_xlabel('Measurement seconds')
        fig.suptitle(('SYNTHETIC - NOT GAIA DATA | ' if run.get('synthetic') else '') + run['run_id'])
        fig.tight_layout()
        fig.savefig(target/(run['run_id']+'.png'), dpi=140)
        plt.close(fig)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input', required=True, type=Path)
    parser.add_argument('--plots', action='store_true')
    args = parser.parse_args()
    try:
        summaries = analyze_suite(args.input, args.plots)
    except (ValueError, OSError, RuntimeError) as exc:
        parser.exit(2, f'analysis: {exc}\n')
    print(f"Analyzed {len(summaries)} runs -> {args.input.resolve() / 'report.md'}")


if __name__ == '__main__':
    main()
