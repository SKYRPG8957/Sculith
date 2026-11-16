#!/usr/bin/env python3
"""Repeated real world-owner mailbox benchmark; no simulation scaling claim."""
import argparse
import json
from pathlib import Path
import subprocess
import time

from plugin_dispatch import MemorySampler, hardware, digest, ROOT


def measure(args):
    assert 3 <= args.repetitions <= 20
    assert 1024 <= args.iterations <= 2000000 and args.iterations % 32 == 0
    assert not args.output.exists(), 'Choose a new result path'
    sampler = MemorySampler()
    rows = []
    try:
        for repetition in range(args.repetitions):
            process = subprocess.Popen([str(args.benchmark.resolve(strict=True)), str(args.iterations)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding='utf-8', cwd=ROOT)
            try:
                for line in process.stdout:
                    row = json.loads(line); key = f'{repetition}:{row["case"]}'
                    if row['kind'] == 'phase_start':
                        sampler.begin(key, row['host_pid'], [])
                    else:
                        assert row['kind'] == 'phase_end'
                        row.update(sampler.end(key)); row['repetition'] = repetition; rows.append(row)
                error = process.stderr.read()
                assert process.wait(timeout=30) == 0, error
            finally:
                if process.poll() is None: process.kill(); process.wait(timeout=10)
    finally:
        sampler.close()
    assert len(rows) == args.repetitions*4
    result = {'schema_version':1,'kind':'world-mailbox-resource-benchmark','hardware':hardware(),
        'source':subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),
        'worktree_dirty':bool(subprocess.check_output(['git','status','--porcelain'],cwd=ROOT)),
        'rust':subprocess.check_output(['rustc','--version'],text=True).strip(),
        'profile':'release/thin-LTO','warmup_dispatches':256,'fixture_sha256':None,
        'benchmark_sha256':digest(args.benchmark),
        'harness_sha256':digest(ROOT/'crates/sculith-runtime/examples/mailbox_bench.rs'),
        'driver_sha256':digest(Path(__file__)),
        'resource_helper_sha256':digest(ROOT/'scripts/bench/plugin_dispatch.py'),
        'sampler_period_seconds':.05,'measured_at_utc':time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime()),
        'rows':rows,'limits':['One region, two Tokio runtime workers, 1/32 requesting tasks, no game simulation or network load.',
        'Sixteen resident chunks, revision zero; immutable snapshots/captures only.',
        'workers/subscribers are zero: no child processes or event subscribers.',
        'Includes request/reply scheduling and latency storage; startup/shutdown/input preparation excluded.',
        '50 ms RSS/PSS observations, not allocator counts or exact high-water marks.']}
    args.output.write_text(json.dumps(result,indent=2)+'\n')
    import statistics
    for case in sorted({r['case'] for r in rows}):
        group = [r for r in rows if r['case'] == case]
        print(case,'median wall ms=',round(statistics.median(r['wall_ns'] for r in group)/1e6,3))


if __name__ == '__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--benchmark',type=Path,default=ROOT/'target/release/examples/mailbox_bench')
    parser.add_argument('--iterations',type=int,default=20000)
    parser.add_argument('--repetitions',type=int,default=5)
    parser.add_argument('--output',type=Path,required=True)
    measure(parser.parse_args())
