#!/usr/bin/env python3
"""Reject wire/workload drift before comparing chunk encoder costs."""
import argparse
import json
from pathlib import Path

from compare_plugin_dispatch import compare as compare_resources


def compare(before, after):
    for result in (before, after):
        if result['kind'] != 'chunk-encoding-resource-benchmark':
            raise ValueError('not a chunk encoding benchmark')
        cells = [(row['case'], row['repetition']) for row in result['rows']]
        expected = {f'{case}-{codec}' for case in ('flat', 'empty', 'sparse', 'solid', 'dense')
                    for codec in ('java', 'java-light', 'bedrock')}
        repetitions = {row['repetition'] for row in result['rows']}
        if (len(cells) != len(set(cells)) or
                set(cells) != {(case, rep) for case in expected for rep in repetitions}):
            raise ValueError('missing or duplicate chunk benchmark cell')
        if any(row['iterations'] != row['delivered_requests'] or row['mode'] != 'encoding' or
               row['workers'] != 0 or row['subscribers'] != 0 for row in result['rows']):
            raise ValueError('incomplete or invalid encoding workload')
    for key in ('resource_helper_sha256', 'build_settings'):
        if before[key] != after[key]:
            raise ValueError('incomparable benchmark field: '+key)
    for case in {row['case'] for row in before['rows']}:
        for key in ('encoded_bytes', 'encoded_sha256'):
            if len({row[key] for result in (before, after) for row in result['rows'] if row['case'] == case}) != 1:
                raise ValueError('wire output changed: '+case+' '+key)
    return compare_resources(before, after)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before', type=Path)
    parser.add_argument('after', type=Path)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    result = compare(json.loads(args.before.read_text()), json.loads(args.after.read_text()))
    if args.output:
        with args.output.open('x') as output:
            output.write(json.dumps(result, indent=2)+'\n')
    for case in result['cases']:
        print(case['case'], 'wall %=', case['metrics']['wall_ns']['change_percent'],
              'p95 %=', case['metrics']['p95_ns']['change_percent'])
    if result['regressions']:
        raise SystemExit('REGRESSIONS: '+json.dumps(result['regressions']))
