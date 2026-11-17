#!/usr/bin/env python3
"""Measure local NetherNet session load with the independent Go wire client.

Reports local connection/chunk/control timings and server process CPU/RSS. These
are not official Bedrock rendering, internet RTT, TPS or production capacity.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import threading
import time


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def proc_sample(pid):
    """Linux-only process measurement; unavailable counters remain absent."""
    try:
        stat = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
        status = Path(f'/proc/{pid}/status').read_text()
        counters = {key: int(value) * 1024 for key, value in
                    re.findall(r'^(VmRSS|VmHWM):\s+(\d+) kB$', status, re.M)}
        counters['cpu_seconds'] = (int(stat[11]) + int(stat[12])) / os.sysconf('SC_CLK_TCK')
        return counters
    except (OSError, ValueError, AttributeError):
        return {}


def run(args):
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    config = root / 'server.toml'
    config.write_text('[server]\nmax_players=64\n'
        '[java]\nsessions=true\nrequire_authentication=false\n'
        '[bedrock]\nsessions=true\nrequire_authentication=false\n'
        'login_timeout_secs=30\nnethernet_identity_path=' + json.dumps(str(root / 'identity.pem')) + '\n'
        '[network.java]\nbind="127.0.0.1:0"\n'
        '[network.bedrock]\nbind="127.0.0.1:0"\n', encoding='utf-8')
    profile = root / 'world-profile.json'
    log = root / 'server.log'
    samples = []
    stopping = threading.Event()
    client_result = None
    process = None
    sampler = None
    started = time.monotonic()
    with log.open('w', encoding='utf-8') as output:
        try:
            process = subprocess.Popen([str(args.server), 'run', '--config', str(config),
                '--profile-world', '300', '--world-profile-output', str(profile)],
                stdin=subprocess.PIPE, stdout=output, stderr=output, text=True,
                env=dict(os.environ, NO_COLOR='1', RUST_LOG='info,sculith::protocol::bedrock=debug'))

            def sample():
                while not stopping.wait(.1):
                    samples.append(proc_sample(process.pid))

            sampler = threading.Thread(target=sample, daemon=True)
            sampler.start()
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                text = log.read_text(encoding='utf-8')
                match = re.search(r'NetherNet listening.*?127\.0\.0\.1:(\d+)', text)
                if match:
                    break
                if process.poll() is not None:
                    raise RuntimeError(text)
                time.sleep(.02)
            else:
                raise TimeoutError(text)
            before = proc_sample(process.pid)
            load_started = time.monotonic()
            client_result = subprocess.run([str(args.client), 'http://127.0.0.1:' + match[1],
                '--stress', str(args.clients), str(args.seconds)], capture_output=True,
                text=True, timeout=args.seconds + 75)
            load_elapsed = time.monotonic() - load_started
            after = proc_sample(process.pid)
            (root / 'client.json').write_text(client_result.stdout, encoding='utf-8')
            (root / 'client.stderr').write_text(client_result.stderr, encoding='utf-8')
            report = {'schema_version': 1, 'scope': 'local_nethernet_sessions',
                'server_sha256': digest(args.server), 'client_sha256': digest(args.client),
                'platform': platform.platform(), 'logical_cpus': os.cpu_count(),
                'clients': args.clients, 'active_seconds': args.seconds,
                'load_wall_seconds': load_elapsed, 'client_exit_code': client_result.returncode,
                'rss_peak_sampled_bytes': max((s['VmRSS'] for s in samples if 'VmRSS' in s), default=None),
                'rss_process_high_water_bytes': after.get('VmHWM'),
                'process_cpu_seconds': (after['cpu_seconds'] - before['cpu_seconds']
                    if 'cpu_seconds' in after and 'cpu_seconds' in before else None)}
            report['process_cpu_percent_one_core'] = (report['process_cpu_seconds'] / load_elapsed * 100
                if report['process_cpu_seconds'] is not None else None)
            try:
                report['load'] = json.loads(client_result.stdout)
            except json.JSONDecodeError:
                report['load'] = None
            process.stdin.write('stop\n')
            process.stdin.flush()
            report['server_exit_code'] = process.wait(timeout=15)
            owner = json.loads(profile.read_text(encoding='utf-8'))
            operations = {entry['operation']: entry['totals']['calls'] for entry in owner['operations']}
            report['owner_movements'] = operations.get('move', 0)
            report['owner_block_breaks'] = operations.get('break', 0)
            report['owner_aborted_commands'] = owner['aborted_commands']
            report['owner_maximum_queue_depth'] = owner['maximum_remaining_queue_depth']
            expected_moves = sum(client['movement_inputs_sent'] for client in report['load']['results']) if report['load'] else None
            report['all_sent_movements_reached_owner'] = report['owner_movements'] == expected_moves
            report['total_wall_seconds'] = time.monotonic() - started
            (root / 'report.json').write_text(json.dumps(report, indent=2) + '\n', encoding='utf-8')
            print(json.dumps({key: value for key, value in report.items() if key != 'load'}, indent=2))
            if client_result.returncode or report['server_exit_code']:
                raise RuntimeError(f'load failed; inspect {root}')
            if not report['all_sent_movements_reached_owner'] or report['owner_aborted_commands']:
                raise RuntimeError(f'owner verification failed; inspect {root}')
        finally:
            stopping.set()
            if sampler:
                sampler.join(timeout=1)
            if process is not None and process.poll() is None:
                process.kill()
                process.wait(timeout=5)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--server', type=lambda p: Path(p).resolve(strict=True), required=True)
    parser.add_argument('--client', type=lambda p: Path(p).resolve(strict=True), required=True)
    parser.add_argument('--clients', type=int, choices=range(1, 65), default=1)
    parser.add_argument('--seconds', type=int, choices=range(1, 301), default=15)
    parser.add_argument('--output', type=Path, required=True)
    run(parser.parse_args())
