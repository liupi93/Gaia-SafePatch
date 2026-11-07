"""SYNTHETIC protocol peer, not Gaia or a performance workload. Never use as experimental evidence."""
import argparse
import csv
import json
import os
import signal
import sqlite3
import sys
import threading
import time
import tomllib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from common import NS, EVENT_FIELDS, REQUEST_FIELDS, mono_ns, read_json, write_json, write_csv


def workload(args):
    directory = Path(os.environ['GAIA_PERF_RUN_DIR'])
    run_id = os.environ['GAIA_PERF_RUN_ID']
    state = {'phase': 'ready', 'start_ns': None, 'end_ns': None}
    stop = threading.Event()
    rows = [[] for _ in range(args.threads)]

    def status(phase):
        data = {'run_id': run_id, 'pid': os.getpid(), 'phase': phase, 'clock': 'CLOCK_MONOTONIC',
                'process_model': 'single-process-multithread', 'ack_ns': mono_ns(),
                'start_ns': state['start_ns'], 'end_ns': state['end_ns']}
        if phase == 'stopped':
            data['generated_count'] = sum(1 for buffer in rows for r in buffer if r['phase'] == 'measure')
        if phase == 'measure' and os.getenv('SYNTHETIC_BAD_ACK'):
            data['start_ns'] += 1
        write_json(os.environ['GAIA_PERF_STATUS_FILE'], data)

    def worker(index):
        sequence = 0
        while not stop.is_set():
            stamp = mono_ns()
            if state['phase'] == 'ready' or (state['end_ns'] and stamp >= state['end_ns']):
                stop.wait(.001)
                continue
            phase = 'measure' if state['start_ns'] and stamp >= state['start_ns'] else 'warmup'
            sequence += 1
            # Explicit SYNTHETIC admission delay: validates end-arrival, no real patching.
            time.sleep(.002)
            begin = mono_ns()
            time.sleep(.003)
            end = mono_ns()
            rows[index].append({'run_id': run_id, 'request_id': sequence, 'worker_id': index, 'phase': phase,
                                'arrival_ns': stamp, 'start_ns': begin, 'end_ns': end, 'status': 'ok'})

    workers = [threading.Thread(target=worker, args=(i,), daemon=True) for i in range(args.threads)]
    for thread in workers:
        thread.start()
    status('ready')
    for line in sys.stdin:
        parts = line.split()
        if not parts:
            continue
        if parts[0] == 'WARMUP':
            state['phase'] = 'warmup'
            status('warmup')
        elif parts[0] == 'MEASURE':
            if os.getenv('SYNTHETIC_EXIT_MEASURE'):
                os._exit(19)
            if parts[1] != run_id:
                raise ValueError('run id mismatch')
            state.update(start_ns=int(parts[2]), end_ns=int(parts[3]))
            state['phase'] = 'measure'
            status('measure')
        elif parts[0] == 'STOP':
            stop.set()
            for thread in workers:
                thread.join(timeout=.3)
            write_csv(os.environ['GAIA_PERF_REQUESTS_FILE'], REQUEST_FIELDS, (r for buffer in rows for r in buffer))
            if os.getenv('SYNTHETIC_GAIA_CRASH_ON_STOP'):
                # Fault injection targets ONLY the Gaia PID registered by this synthetic run.
                manifest = read_json(directory/'manifest.json')
                os.kill(manifest['processes']['gaia']['pid'], signal.SIGTERM)
                time.sleep(.08)
            status('stopped')


def gaia(args):
    directory = Path(os.environ['GAIA_PERF_RUN_DIR'])
    run_id = os.environ['GAIA_PERF_RUN_ID']
    config = tomllib.loads(Path(args.config).read_text(encoding='utf-8'))
    target = config['hotpatch']['targets'][0]
    with sqlite3.connect(args.db_path) as conn:
        conn.execute('CREATE TABLE synthetic_only (note TEXT)')
        conn.execute("INSERT INTO synthetic_only VALUES ('SYNTHETIC, not actual Gaia events')")
    write_json(os.environ['GAIA_PERF_OPTIMIZATION_STATUS_FILE'], {'run_id': run_id, 'pid': target['pid'],
        'clock': 'CLOCK_MONOTONIC', 'monitoring_enabled': True, 'coordination_enabled': True,
        'observed_ns': mono_ns(), 'evidence': 'SYNTHETIC mechanism simulation, not optimization proof'})
    counter = {'put_count': 0}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *unused):
            pass

        def reply(self, data):
            content = json.dumps(data).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(content)))
            self.end_headers()
            try:
                self.wfile.write(content)
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                pass

        def do_GET(self):
            if self.path == '/api/v1/config/hotpatch':
                self.reply([target])
            elif self.path == '/api/v1/state':
                events = []
                if not os.getenv('SYNTHETIC_NO_MONITOR'):
                    events.append({'kind': 'hotpatch', 'pid': target['pid'], 'tgid': target['pid'],
                                   'action': 'enter', 'detail': 'uprobe-entry:monitored', 'timestamp_ns': 1})
                self.reply({'events': events})
            else:
                self.send_error(404)

        def do_PUT(self):
            if self.path != '/api/v1/config/hotpatch':
                self.send_error(404)
                return
            payload = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            if not all(k in payload for k in ('index', 'binary', 'symbol', 'pid', 'enabled', 'patch_action', 'replace_lib', 'replace_symbol')):
                self.send_error(400)
                return
            counter['put_count'] += 1
            write_json(directory/'synthetic_http_count.json', counter)
            received = mono_ns()
            time.sleep(float(os.getenv('SYNTHETIC_HTTP_DELAY', '0.01')))
            if not os.getenv('SYNTHETIC_NO_MARKERS'):
                begin = mono_ns()
                time.sleep(.005)
                end = mono_ns()
                write_csv(os.environ['GAIA_PERF_PATCH_EVENTS_FILE'], EVENT_FIELDS,
                    [dict(run_id=run_id, attempt_id='patch-1', event=event, mono_ns=stamp,
                          outcome='error' if os.getenv('SYNTHETIC_APPLY_FAILURE') and event=='apply_begin' else 'ok') for event,stamp in
                     (('request_received', received), ('apply_begin', begin), ('apply_end', end))])
                write_json(os.environ['GAIA_PERF_PATCH_PROOF_FILE'], {'run_id': run_id, 'attempt_id': 'patch-1',
                    'pid': target['pid'], 'observed_ns': mono_ns(), 'new_implementation': True,
                    'evidence': 'SYNTHETIC observation; no real code substitution'})
            self.reply([payload])

    ThreadingHTTPServer(('127.0.0.1', 17890), Handler).serve_forever(poll_interval=.05)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('role', choices=('workload', 'gaia'))
    parser.add_argument('--threads', type=int, default=2)
    parser.add_argument('--seed')
    parser.add_argument('--config')
    parser.add_argument('--web-listen')
    parser.add_argument('--db-path')
    args = parser.parse_args()
    (workload if args.role == 'workload' else gaia)(args)


if __name__ == '__main__':
    main()
