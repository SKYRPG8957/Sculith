#!/usr/bin/env python3
"""Compare matching world-owner benchmarks using the shared resource guard."""
import argparse
import json
from pathlib import Path
from compare_plugin_dispatch import compare as compare_resources


def compare(before, after, wall_limit=20, tail_limit=30):
    if before['kind'] != 'world-mailbox-resource-benchmark' or after['kind'] != before['kind']:
        raise ValueError('expected world mailbox benchmark results')
    if before['resource_helper_sha256'] != after['resource_helper_sha256']:
        raise ValueError('resource sampling helper differs')
    for case in {row['case'] for row in before['rows']}:
        for key in ['clients']:
            values = {row[key] for result in [before, after] for row in result['rows'] if row['case'] == case}
            if len(values) != 1: raise ValueError('runtime workload mismatch: '+case+' '+key)
    return compare_resources(before, after, wall_limit, tail_limit)


if __name__ == '__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before',type=Path);parser.add_argument('after',type=Path)
    parser.add_argument('--max-wall-regression',type=float,default=20)
    parser.add_argument('--max-tail-regression',type=float,default=30)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    assert not args.output.exists(),'Choose a new comparison output'
    result=compare(json.loads(args.before.read_text()),json.loads(args.after.read_text()),args.max_wall_regression,args.max_tail_regression)
    args.output.write_text(json.dumps(result,indent=2)+'\n')
    for case in result['cases']:
        print(case['case'],'wall change %=',case['metrics']['wall_ns']['change_percent'],
            'p95 change %=',case['metrics']['p95_ns']['change_percent'])
    if result['regressions']: raise SystemExit('REGRESSIONS '+json.dumps(result['regressions']))
