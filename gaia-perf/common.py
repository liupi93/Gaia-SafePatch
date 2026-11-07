"""Shared on-disk contracts; collection needs Python's standard library only."""
import csv
import hashlib
import json
import os
import time
from pathlib import Path

NS = 1_000_000_000
REQUEST_FIELDS = ['run_id', 'request_id', 'worker_id', 'phase', 'arrival_ns', 'start_ns', 'end_ns', 'status']
EVENT_FIELDS = ['run_id', 'attempt_id', 'event', 'mono_ns', 'outcome']
RESOURCE_FIELDS = ['case_id', 'run_id', 'entity', 'pid', 'mono_ns', 'scheduled_ns',
                   'interval_start_ns', 'interval_s', 'cpu_pct', 'rss_mib', 'steal_pct', 'error']


def mono_ns():
    # On Linux monotonic_ns is CLOCK_MONOTONIC, shared with C clock_gettime.
    return time.monotonic_ns()


def read_json(path):
    return json.loads(Path(path).read_text(encoding='utf-8-sig'))


def write_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_name(path.name + '.tmp')
    temp.write_text(json.dumps(value, ensure_ascii=False, indent=2, allow_nan=False) + '\n', encoding='utf-8')
    os.replace(temp, path)


def write_csv(path, fields, rows):
    with Path(path).open('w', encoding='utf-8', newline='') as handle:
        writer = csv.DictWriter(handle, fieldnames=fields, extrasaction='ignore')
        writer.writeheader()
        writer.writerows(rows)


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def expand_argv(argv, context):
    """Only documented placeholders, no shell, environment or expression expansion."""
    return [arg.format_map(context) for arg in argv]
