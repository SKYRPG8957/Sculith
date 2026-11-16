#!/usr/bin/env python3
"""Compare matching repeated IPC runs; reject mismatched workloads/hardware."""
import argparse
import json
from pathlib import Path
import statistics


def compare(before, after, wall_limit=20, tail_limit=30):
    if before['worktree_dirty'] or after['worktree_dirty']:
        raise ValueError('benchmark source must be committed and clean')
    for key in ['schema_version','kind','hardware','rust','profile','warmup_dispatches','fixture_sha256','harness_sha256','driver_sha256','sampler_period_seconds']:
        if before[key] != after[key]:
            raise ValueError('incomparable benchmark field: '+key)
    cases = {row['case'] for row in before['rows']}
    if cases != {row['case'] for row in after['rows']}:
        raise ValueError('case sets differ')
    comparison = []
    failed = []
    for case in sorted(cases):
        groups = [[row for row in result['rows'] if row['case'] == case] for result in [before, after]]
        if len(groups[0]) != len(groups[1]):
            raise ValueError('repetition counts differ')
        if min(map(len, groups)) < 3:
            raise ValueError('at least three repetitions per case required')
        for key in ['iterations','workers','subscribers','mode','delivered_requests']:
            if len({row[key] for group in groups for row in group}) != 1:
                raise ValueError('workload mismatch: '+case+' '+key)
        entry = {'case':case, 'metrics':{}}
        fields = ['wall_ns','p50_ns','p95_ns','p99_ns','host_cpu_ticks','worker_cpu_ticks',
                  'peak_host_rss_kib','peak_host_pss_kib','peak_aggregate_rss_kib','peak_aggregate_pss_kib']
        for field in fields:
            values = [[row[field] for row in group if row.get(field) is not None] for group in groups]
            if min(map(len, values)) < 3:
                entry['metrics'][field] = {'before':None,'after':None,'change_percent':None,'reason':'fewer than three observations'}
                continue
            old, new = map(statistics.median, values)
            change = (new/old-1)*100 if old and new else None
            entry['metrics'][field] = {'before':old,'after':new,'change_percent':change}
            limit = wall_limit if field == 'wall_ns' else tail_limit if field in ('p95_ns','p99_ns') else None
            if limit is not None and change is not None and change > limit + 1e-9:
                failed.append({'case':case,'metric':field,'change_percent':change,'limit':limit})
        comparison.append(entry)
    return {'schema_version':1,'before_source':before['source'],'after_source':after['source'],
            'cases':comparison,'regressions':failed,
            'limits':'Median comparisons of one matching environment; not TPS or a causal CPU/memory allocation profile.'}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before',type=Path);parser.add_argument('after',type=Path)
    parser.add_argument('--max-wall-regression',type=float,default=20)
    parser.add_argument('--max-tail-regression',type=float,default=30)
    parser.add_argument('--output',type=Path)
    args = parser.parse_args()
    try:
        result = compare(json.loads(args.before.read_text()),json.loads(args.after.read_text()),args.max_wall_regression,args.max_tail_regression)
    except ValueError as error:
        raise SystemExit(str(error))
    if args.output:
        assert not args.output.exists(),'Choose a new comparison output'
        args.output.write_text(json.dumps(result,indent=2)+'\n')
    for case in result['cases']:
        print(case['case'], 'wall change %=',case['metrics']['wall_ns']['change_percent'],
              'p95 change %=',case['metrics']['p95_ns']['change_percent'])
    if result['regressions']:
        print('REGRESSIONS',json.dumps(result['regressions']))
        raise SystemExit(1)
