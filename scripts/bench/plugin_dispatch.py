#!/usr/bin/env python3
"""Repeat real Rust IPC dispatch and sample Linux process CPU/RSS/PSS."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import threading
import time

ROOT = Path(__file__).resolve().parents[2]


def proc_memory(pid):
    try:
        data = {}
        for line in Path(f'/proc/{pid}/smaps_rollup').read_text().splitlines():
            key, _, value = line.partition(':')
            if key in ('Rss', 'Pss'):
                data[key] = int(value.split()[0])
        return data if len(data) == 2 else None
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        return None


class MemorySampler:
    def __init__(self):
        self.lock = threading.Lock()
        self.active = None
        self.peaks = {}
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def begin(self, key, host, workers):
        with self.lock:
            self.active = (key, host, workers)
            self.peaks[key] = {'memory_samples': 0}

    def end(self, key):
        with self.lock:
            self.active = None
            return dict(self.peaks[key])

    def run(self):
        while not self.stop.wait(.05):
            with self.lock:
                if not self.active:
                    continue
                key, host_pid, worker_pids = self.active
                host = proc_memory(host_pid)
                workers = [proc_memory(pid) for pid in worker_pids]
                if host is None or any(worker is None for worker in workers):
                    continue
                peak = self.peaks[key]
                peak['memory_samples'] += 1
                for label, values in [('host', [host]), ('workers', workers), ('aggregate', [host, *workers])]:
                    for metric in ('Rss', 'Pss'):
                        field = f'peak_{label}_{metric.lower()}_kib'
                        peak[field] = max(peak.get(field, 0), sum(value[metric] for value in values))

    def close(self):
        self.stop.set()
        self.thread.join(timeout=5)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def hardware():
    info = Path('/proc/cpuinfo').read_text()
    model = next(line.partition(':')[2].strip() for line in info.splitlines() if line.startswith('model name'))
    def read(path):
        try: return Path(path).read_text().strip()
        except FileNotFoundError: return None
    return {'platform': platform.platform(), 'cpu_model': model, 'logical_cpus': os.cpu_count(),
            'affinity': sorted(os.sched_getaffinity(0)), 'cpu_max': read('/sys/fs/cgroup/cpu.max'),
            'memory_max': read('/sys/fs/cgroup/memory.max'), 'clock_ticks_per_second': os.sysconf('SC_CLK_TCK')}


def measure(args):
    if platform.system() != 'Linux':
        raise SystemExit('This resource benchmark requires Linux /proc; it is not a portable server check.')
    assert 1 <= args.repetitions <= 20 and 128 <= args.iterations <= 20000
    assert not args.output.exists(), 'Choose a new result path'
    sampler = MemorySampler()
    rows = []
    try:
        for repetition in range(args.repetitions):
            process = subprocess.Popen([str(args.benchmark.resolve(strict=True)), str(args.fixture.resolve(strict=True)), str(args.iterations), args.mode],
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding='utf-8', cwd=ROOT)
            try:
                for line in process.stdout:
                    row = json.loads(line)
                    key = f'{repetition}:{row["case"]}'
                    if row['kind'] == 'phase_start':
                        sampler.begin(key, row['host_pid'], row['worker_pids'])
                    else:
                        assert row['kind'] == 'phase_end'
                        row.update(sampler.end(key))
                        row['repetition'] = repetition
                        rows.append(row)
                error = process.stderr.read()
                assert process.wait(timeout=30) == 0, error
            finally:
                if process.poll() is None:
                    process.kill(); process.wait(timeout=10)
    finally:
        sampler.close()
    assert len(rows) == args.repetitions*6
    result = {'schema_version': 1, 'kind': 'plugin-dispatch-resource-benchmark', 'hardware': hardware(),
              'source': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
              'worktree_dirty': bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=ROOT)),
              'rust': subprocess.check_output(['rustc', '--version'], text=True).strip(),
              'profile': 'release/thin-LTO', 'warmup_dispatches': 128,
              'benchmark_sha256': digest(args.benchmark), 'fixture_sha256': digest(args.fixture),
              'harness_sha256': digest(ROOT/'crates/sculith-plugin-host/examples/dispatch_bench.rs'),
              'driver_sha256': digest(Path(__file__)),
              'measured_at_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
              'sampler_period_seconds': .05, 'rows': rows,
              'limits': ['IPC-only: no game simulation or Minecraft network load.',
                         'CPU ticks measure process CPU, distinct from callback wall time.',
                         'RSS sums count shared mappings repeatedly; aggregate PSS apportions them.',
                         'Memory samples are interval observations, not exact allocation counts or high-water marks.',
                         'Event request values are prepared before the timed loop; process startup/shutdown is excluded.']}
    args.output.write_text(json.dumps(result, indent=2)+'\n')
    for case in sorted({row['case'] for row in rows}):
        values = [row for row in rows if row['case'] == case]
        print(case, 'median wall ms=', round(statistics.median(row['wall_ns'] for row in values)/1e6, 3),
              'median p95 us=', round(statistics.median(row['p95_ns'] for row in values)/1e3, 3))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--benchmark', type=Path, default=ROOT/'target/release/examples/dispatch_bench')
    parser.add_argument('--fixture', type=Path, default=ROOT/'target/release/sculith-profile-fixture')
    parser.add_argument('--iterations', type=int, default=2000)
    parser.add_argument('--repetitions', type=int, default=5)
    parser.add_argument('--mode', choices=('off', 'monitor', 'profile'), default='off')
    parser.add_argument('--output', type=Path, required=True)
    measure(parser.parse_args())
