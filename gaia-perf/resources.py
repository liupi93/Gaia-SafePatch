"""Linux /proc counters. Process CPU: one core = 100%, NOT divided by vCPUs."""
import csv
import os
import threading
from pathlib import Path

from common import NS, RESOURCE_FIELDS, mono_ns


def parse_proc_stat(text, page_size):
    # comm may contain spaces and ')' characters. Fields after the LAST ')' start at 3.
    tail = text[text.rindex(')') + 1:].split()
    return {'cpu_ticks': int(tail[11]) + int(tail[12]), 'start_ticks': int(tail[19]),
            'rss_mib': int(tail[21]) * page_size / (1024 * 1024)}


def process_delta(previous, current, interval_s, clk_tck):
    cpu = None
    if (previous and interval_s > 0 and previous['start_ticks'] == current['start_ticks']
            and current['cpu_ticks'] >= previous['cpu_ticks']):
        cpu = (current['cpu_ticks'] - previous['cpu_ticks']) / clk_tck / interval_s * 100
    return {'cpu_pct': cpu, 'rss_mib': current['rss_mib'], 'steal_pct': None}


def parse_system_stat(text):
    values = [int(x) for x in text.splitlines()[0].split()[1:]]
    values += [0] * max(0, 10 - len(values))
    # guest and guest_nice are already in user/nice. idle and iowait are not busy.
    return {'total_ticks': sum(values[:8]), 'active_ticks': sum(values[i] for i in (0, 1, 2, 5, 6)),
            'steal_ticks': values[7]}


def system_delta(previous, current):
    total = current['total_ticks'] - previous['total_ticks'] if previous else 0
    if total <= 0:
        return {'cpu_pct': None, 'steal_pct': None, 'rss_mib': None}
    return {'cpu_pct': (current['active_ticks'] - previous['active_ticks']) / total * 100,
            'steal_pct': (current['steal_ticks'] - previous['steal_ticks']) / total * 100,
            'rss_mib': None}


class ProcReader:
    def __init__(self, root='/proc'):
        self.root = Path(root)
        self.available = os.name == 'posix' and self.root.joinpath('stat').is_file()
        self.clk_tck = os.sysconf('SC_CLK_TCK') if self.available else None
        self.page_size = os.sysconf('SC_PAGE_SIZE') if self.available else None

    def process(self, pid):
        if not self.available:
            raise OSError('/proc unavailable on this host; no CPU/RSS data invented')
        return parse_proc_stat((self.root / str(pid) / 'stat').read_text(), self.page_size)

    def system(self):
        if not self.available:
            raise OSError('/proc unavailable on this host')
        return parse_system_stat((self.root / 'stat').read_text())


class ResourceSampler(threading.Thread):
    """Dedicated thread, absolute schedule, bounded lifetime; never waits for HTTP."""
    def __init__(self, path, run, pids, start_ns, end_ns, interval_s, reader=None):
        super().__init__(name='gaia-perf-resources', daemon=True)
        self.path, self.run_info, self.pids = Path(path), run, pids
        self.start_ns, self.end_ns = start_ns, end_ns
        self.step_ns = int(interval_s * NS)
        self.reader = reader or ProcReader()
        self.stop_event = threading.Event()
        self.failure = None

    def run(self):
        previous = {}
        identities = {}
        try:
            with self.path.open('w', encoding='utf-8', newline='') as handle:
                writer = csv.DictWriter(handle, fieldnames=RESOURCE_FIELDS)
                writer.writeheader()
                scheduled = self.start_ns
                while not self.stop_event.is_set():
                    if self.stop_event.wait(max(0, (scheduled - mono_ns()) / NS)):
                        break
                    for entity, pid in {**self.pids, 'system': None}.items():
                        row = {'case_id': self.run_info['case_id'], 'run_id': self.run_info['run_id'],
                               'entity': entity, 'pid': pid, 'scheduled_ns': scheduled}
                        try:
                            snap = self.reader.system() if entity == 'system' else self.reader.process(pid)
                            timestamp = mono_ns()
                            old, old_ns = previous.get(entity, (None, timestamp))
                            if entity != 'system':
                                identities.setdefault(entity, snap['start_ticks'])
                                if snap['start_ticks'] != identities[entity]:
                                    raise OSError('PID identity changed; process restarted/reused')
                                delta = process_delta(old, snap, (timestamp - old_ns) / NS, self.reader.clk_tck)
                            else:
                                delta = system_delta(old, snap)
                            row.update(delta, mono_ns=timestamp, interval_start_ns=old_ns,
                                       interval_s=(timestamp - old_ns) / NS)
                            previous[entity] = (snap, timestamp)
                        except (OSError, ValueError, IndexError) as exc:
                            row.update(mono_ns=mono_ns(), error=str(exc))
                        writer.writerow(row)
                    handle.flush()
                    if scheduled >= self.end_ns:
                        break
                    # Skip missed ticks instead of producing a burst of backdated samples.
                    tick = max(1, (mono_ns() - self.start_ns) // self.step_ns + 1)
                    scheduled = min(self.end_ns, self.start_ns + tick * self.step_ns)
        except Exception as exc:
            self.failure = repr(exc)

    def stop(self):
        self.stop_event.set()
