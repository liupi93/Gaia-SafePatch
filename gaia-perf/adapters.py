"""Task 2/3 boundaries. Foreground processes, atomic status files, one asynchronous PUT."""
import csv
import http.client
import json
import os
import signal
import subprocess
import threading
import time
from pathlib import Path

from common import NS, mono_ns, read_json
from resources import ProcReader


class ProcessOwner:
    def __init__(self):
        self.handles = {}
        self.records = {}
        self.logs = []

    def start(self, name, argv, log_path, cwd, env=None):
        log = Path(log_path).open('wb', buffering=0)
        self.logs.append(log)
        child = subprocess.Popen(argv, cwd=cwd, env={**os.environ, **(env or {})},
                                 stdin=subprocess.PIPE, stdout=log, stderr=subprocess.STDOUT,
                                 text=True, encoding='utf-8', bufsize=1,
                                 start_new_session=os.name == 'posix')
        self.handles[name] = child
        record = {'pid': child.pid, 'argv': argv, 'launched_ns': mono_ns(), 'start_ticks': None}
        try:
            record['start_ticks'] = ProcReader().process(child.pid)['start_ticks']
        except (OSError, ValueError):
            pass
        self.records[name] = record
        return child

    def assert_alive(self):
        for name, process in self.handles.items():
            if process.poll() is not None:
                raise RuntimeError(f'{name} exited early: returncode={process.returncode}')

    def cleanup(self, deadline_ns):
        for name, process in self.handles.items():
            self.records[name].update(returncode_before_cleanup=process.poll(), exit_requested_by_runner=False)
        # Gaia first, while target remains alive, so it can restore its code patches.
        ordered = sorted(self.handles, key=lambda name: name != 'gaia')
        for name in ordered:
            process = self.handles[name]
            if process.poll() is None:
                try:
                    self.records[name].update(exit_requested_by_runner=True, exit_requested_ns=mono_ns())
                    if name == 'gaia' and os.name == 'posix':
                        process.send_signal(signal.SIGINT)
                    else:
                        process.terminate()
                    grace = min(.7, max(0, (deadline_ns-mono_ns())/NS)/max(1, len(ordered)))
                    process.wait(timeout=grace)
                except subprocess.TimeoutExpired:
                    process.kill()
                except (ProcessLookupError, OSError):
                    pass
        # Every kill addresses a still-owned Popen handle; never pkill/killall or scan by name.
        for name, process in self.handles.items():
            if process.poll() is None:
                try:
                    process.kill()
                    process.wait(timeout=max(.05, (deadline_ns-mono_ns())/NS))
                except (OSError, subprocess.TimeoutExpired):
                    pass
            self.records[name].update(returncode=process.poll(), reaped_ns=mono_ns())
            if process.stdin:
                try:
                    process.stdin.close()
                except (OSError, BrokenPipeError):
                    pass
        for log in self.logs:
            log.close()
        return self.records


class WorkloadAdapter:
    def __init__(self, process, status_file, run_id):
        self.process, self.status_file, self.run_id = process, Path(status_file), run_id

    def send(self, command):
        if self.process.poll() is not None:
            raise RuntimeError(f'workload exited before {command.split()[0]}: {self.process.returncode}')
        self.process.stdin.write(command + '\n')
        self.process.stdin.flush()

    def wait_phase(self, phase, deadline_ns, start_ns=None, end_ns=None):
        last = 'no status file'
        while mono_ns() < deadline_ns:
            if self.process.poll() is not None:
                raise RuntimeError(f'workload exited waiting for {phase}: {self.process.returncode}')
            try:
                data = read_json(self.status_file)
                if data.get('run_id') != self.run_id or data.get('pid') != self.process.pid:
                    raise ValueError('status.run_id/pid mismatch: wrappers must exec the foreground target')
                if data.get('clock') != 'CLOCK_MONOTONIC':
                    raise ValueError('status.clock must be CLOCK_MONOTONIC')
                if data.get('process_model') != 'single-process-multithread':
                    raise ValueError('status.process_model must be single-process-multithread')
                if data.get('phase') == phase:
                    if phase == 'measure' and (data.get('start_ns') != start_ns or data.get('end_ns') != end_ns):
                        raise ValueError('MEASURE acknowledgement window mismatch')
                    if not isinstance(data.get('ack_ns'), int) or not 0 < data['ack_ns'] <= mono_ns():
                        raise ValueError('status.ack_ns invalid')
                    if phase == 'measure' and data['ack_ns'] >= start_ns:
                        raise ValueError('MEASURE acknowledged after window start')
                    return data
                last = 'phase=' + str(data.get('phase'))
            except (OSError, ValueError) as exc:
                last = str(exc)
            time.sleep(min(.01, max(0, (deadline_ns-mono_ns())/NS)))
        raise RuntimeError(f'workload status timeout waiting for {phase}: {last}')


def http_json(method, path, payload=None, timeout_s=1):
    """Loopback only, no proxy inheritance. Bounded response; timeout is explicit."""
    conn = http.client.HTTPConnection('127.0.0.1', 17890, timeout=max(.01, timeout_s))
    try:
        body = json.dumps(payload).encode('utf-8') if payload is not None else None
        conn.request(method, path, body=body, headers={'Content-Type': 'application/json', 'Connection': 'close'})
        response = conn.getresponse()
        content = response.read(1024*1024 + 1)
        if len(content) > 1024*1024:
            raise ValueError('HTTP body exceeds 1 MiB limit')
        text = content.decode('utf-8', errors='replace')
        try:
            parsed = json.loads(text)
        except ValueError:
            parsed = None
        return {'status_code': response.status, 'body': text, 'json': parsed}
    finally:
        conn.close()


def monitor_evidence(snapshot, pid, *, optimized_tgid=False):
    for event in snapshot.get('events', []):
        entry = (event.get('action') == 'enter'
                 and event.get('detail') in ('uprobe-entry', 'uprobe-entry:monitored'))
        # main keys the legacy audit filter by TGID, so the entry can be labelled blocked.
        filtered_entry = (optimized_tgid and event.get('action') == 'blocked'
                          and event.get('detail') == 'uprobe-entry:pid-filtered')
        if (event.get('kind') == 'hotpatch' and event.get('tgid', event.get('pid')) == pid
                and (entry or filtered_entry)):
            # Gaia timestamp_ns is legacy display time, NEVER subtract it from monotonic_ns.
            return {'target_pid': pid, 'observed_ns': mono_ns(), 'event': event,
                    'note': 'event timestamp retained as opaque legacy value; not used for durations'}
    return None


def render_gaia_config(template, path, binary, symbol, pid):
    text = Path(template).read_text(encoding='utf-8')
    # JSON string literals are valid TOML basic strings for Linux paths/symbols.
    for key, value in {'binary': json.dumps(binary, ensure_ascii=False),
                       'symbol': json.dumps(symbol, ensure_ascii=False), 'pid': str(pid)}.items():
        text = text.replace('{{' + key + '}}', value)
    if '{{' in text:
        raise ValueError('gaia.config_template contains unresolved placeholders')
    import tomllib
    config = tomllib.loads(text)
    targets = config.get('hotpatch', {}).get('targets', [])
    if (len(targets) != 1 or targets[0].get('pid') != pid or targets[0].get('binary') != binary
            or targets[0].get('symbol') != symbol or targets[0].get('patch_action') != 'monitor'
            or targets[0].get('enabled') is not True):
        raise ValueError('gaia.config_template must contain exactly this enabled target with patch_action="monitor"')
    Path(path).write_text(text, encoding='utf-8')


class AsyncPatch(threading.Thread):
    """One attempt, no retries/reload, and no disk writes after the run has finalized."""
    def __init__(self, run_id, payload, scheduled_ns, timeout_s):
        super().__init__(name='gaia-perf-patch', daemon=True)
        self.run_id, self.payload, self.scheduled_ns, self.timeout_s = run_id, payload, scheduled_ns, timeout_s
        self.cancel_event = threading.Event()
        self.lock = threading.Lock()
        self.events, self.result = [], {'state': 'scheduled'}

    def mark(self, event, stamp, outcome=''):
        self.events.append({'run_id': self.run_id, 'attempt_id': 'patch-1', 'event': event,
                            'mono_ns': stamp, 'outcome': outcome})

    def run(self):
        if self.cancel_event.wait(max(0, (self.scheduled_ns-mono_ns())/NS)):
            return
        with self.lock:
            self.mark('request_sent', mono_ns())
            self.result = {'state': 'pending', 'scheduled_ns': self.scheduled_ns,
                           'request_sent_ns': self.events[0]['mono_ns'], 'payload': self.payload}
        try:
            result = http_json('PUT', '/api/v1/config/hotpatch', self.payload, self.timeout_s)
            outcome = 'ok' if 200 <= result['status_code'] < 300 else 'http_error'
        except Exception as exc:
            result, outcome = {'error': repr(exc)}, 'error'
        stamp = mono_ns()
        with self.lock:
            self.mark('http_done', stamp, outcome)
            self.result.update(result, state='done', outcome=outcome, http_done_ns=stamp,
                               http_rtt_ms=(stamp-self.result['request_sent_ns'])/1_000_000)

    def snapshot(self):
        with self.lock:
            return [dict(e) for e in self.events], dict(self.result)

    def cancel(self):
        self.cancel_event.set()


def import_patch_events(path):
    if not Path(path).is_file():
        return []
    with Path(path).open(encoding='utf-8-sig', newline='') as handle:
        reader = csv.DictReader(handle)
        if not {'run_id', 'attempt_id', 'event', 'mono_ns', 'outcome'} <= set(reader.fieldnames or []):
            raise ValueError('patch.events_adapter: missing csv-v1 columns')
        rows = list(reader)
        if any(r['event'] in ('request_sent', 'http_done') for r in rows):
            raise ValueError('patch adapter must not impersonate runner request_sent/http_done')
        return rows
