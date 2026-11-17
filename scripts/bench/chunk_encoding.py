#!/usr/bin/env python3
"""Measure versioned chunk encoding kernels, not sockets or game simulation."""
import argparse
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import time

from plugin_dispatch import ROOT, MemorySampler, digest, hardware


def executable(artifacts):
    matches = []
    for line in artifacts.read_text().splitlines():
        row = json.loads(line)
        if (row.get('reason') == 'compiler-artifact' and
                row['target']['name'] == 'sculith' and row['profile']['test'] and row.get('executable')):
            assert row['profile']['opt_level'] == '3' and not row['profile']['debug_assertions']
            matches.append(Path(row['executable']).resolve(strict=True))
    assert len(set(matches)) == 1, 'Expected one optimized server test executable'
    return matches[0]


def measure(args):
    assert platform.system() == 'Linux', 'Resource sampling requires Linux /proc'
    assert 3 <= args.repetitions <= 20 and 256 <= args.iterations <= 10000
    assert not args.output.exists(), 'Choose a new result path'
    binary = executable(args.artifacts)
    source = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    assert not subprocess.check_output(['git', 'status', '--porcelain'], cwd=ROOT), 'Commit source before measuring'
    sampler = MemorySampler()
    rows = []
    expected = {f'{case}-{codec}' for case in ('flat', 'empty', 'sparse', 'solid', 'dense')
                for codec in ('java', 'java-light', 'bedrock')}
    try:
        for repetition in range(args.repetitions):
            active = None
            seen = set()
            with tempfile.TemporaryFile(mode='w+') as errors:
                process = subprocess.Popen(
                    [str(binary), '--ignored', '--nocapture', '--exact',
                     'protocol::chunk_bench::benchmark_encoders', '--test-threads=1', '--color', 'never'],
                    stdout=subprocess.PIPE, stderr=errors, text=True, cwd=ROOT,
                    env={**os.environ, 'SCULITH_CHUNK_BENCH_ITERATIONS': str(args.iterations)})
                try:
                    for line in process.stdout:
                        if '{' not in line:
                            continue
                        row = json.loads(line[line.index('{'):])
                        case = row['case']
                        key = f'{repetition}:{case}'
                        assert case in expected
                        if row['kind'] == 'phase_start':
                            assert active is None and case not in seen
                            active = key
                            sampler.begin(key, row['host_pid'], row['worker_pids'])
                        else:
                            assert row['kind'] == 'phase_end' and active == key
                            row.update(sampler.end(key))
                            row['repetition'] = repetition
                            rows.append(row)
                            seen.add(case)
                            active = None
                    code = process.wait(timeout=30)
                    errors.seek(0)
                    assert code == 0, errors.read()
                    assert active is None and seen == expected
                finally:
                    if process.poll() is None:
                        process.kill()
                        process.wait(timeout=10)
    finally:
        sampler.close()
    result = {'schema_version': 1, 'kind': 'chunk-encoding-resource-benchmark', 'source': source,
              'worktree_dirty': bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=ROOT)),
              'hardware': hardware(), 'rust': subprocess.check_output(['rustc', '--version'], text=True).strip(),
              'profile': 'release-test/thin-LTO', 'warmup_dispatches': 64,
              'benchmark_sha256': digest(binary),
              'fixture_sha256': digest(ROOT/'tests/fixtures/chunk-encoding-v777-v2225.json'),
              'harness_sha256': digest(ROOT/'crates/sculith-server/src/protocol/chunk_bench.rs'),
              'driver_sha256': digest(Path(__file__)),
              'resource_helper_sha256': digest(ROOT/'scripts/bench/plugin_dispatch.py'),
              'build_settings': {str(path): digest(ROOT/path) for path in (
                  'Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'crates/sculith-server/Cargo.toml')},
              'sampler_period_seconds': .05, 'rows': rows,
              'measured_at_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
              'limits': ['Prepared immutable snapshots; no compression, encryption, sockets or simulation.',
                         'Quantiles time encoding; wall/CPU also include exact byte checks and buffer drop.',
                         '50 ms memory samples include fixtures and harness; short phases may have no samples.',
                         'Process CPU ticks are coarse; zero ticks do not imply zero CPU cost.',
                         'One environment, fixed case order; this is not a multiplayer TPS measurement.']}
    assert subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip() == source
    with args.output.open('x') as output:
        output.write(json.dumps(result, indent=2)+'\n')
    print(f'Recorded {len(rows)} phases at {args.output}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--artifacts', type=Path, required=True, help='cargo test --no-run --message-format=json output')
    parser.add_argument('--iterations', type=int, default=1000)
    parser.add_argument('--repetitions', type=int, default=5)
    parser.add_argument('--output', type=Path, required=True)
    measure(parser.parse_args())
