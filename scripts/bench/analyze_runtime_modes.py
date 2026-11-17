#!/usr/bin/env python3
"""Validate interleaved owner-mode studies and compare matching implementations."""
import argparse
from collections import Counter
import json
from pathlib import Path
import statistics

from compare_runtime_mailbox import compare as compare_runtime
from runtime_mailbox import mode_orders

MODES=('off','monitor','profile')
CASES={'snapshot-one':1,'snapshot-thirty-two':32,'capture-one':1,'capture-thirty-two':32}


def validate(data):
    if data['kind']!='world-mailbox-mode-study' or data['worktree_dirty']:
        raise ValueError('expected a committed, clean world mailbox mode study')
    orders=data['mode_orders'];count=len(orders)
    if count<3 or orders!=mode_orders('all',count,data['order_seed']):
        raise ValueError('mode orders must follow the balanced recorded seed')
    expected={(case,mode,repetition) for case in CASES for mode in MODES for repetition in range(count)}
    keys=Counter((row['case'],row['mode'],row['repetition']) for row in data['rows'])
    if set(keys)!=expected or any(value!=1 for value in keys.values()):
        raise ValueError('study has missing or duplicate case/mode/repetition cells')
    if len({row['iterations'] for row in data['rows']})!=1:
        raise ValueError('iteration counts differ within the study')
    for row in data['rows']:
        if row['clients']!=CASES[row['case']] or row['delivered_requests']!=row['iterations']:
            raise ValueError('requester or delivery workload differs')
        commands=row['iterations']+data['warmup_dispatches']
        if row['completed_commands']<commands:
            raise ValueError('owner work did not complete')
        if row['mode']=='off':
            if row['timed_commands'] or row['retained_samples']:
                raise ValueError('Off retained timings')
        elif row['timed_commands']<commands:
            raise ValueError('enabled capture expired or lost work')
        if row['mode']=='monitor' and row['retained_samples']:
            raise ValueError('Monitor retained detailed samples')
        if row['mode']=='profile' and (row['retained_samples']!=512 or row['overwritten_samples']<commands-512):
            raise ValueError('detailed capture is incomplete or unbounded')
    return data


def view(data,mode):
    validate(data)
    return dict(data,kind='world-mailbox-resource-benchmark',mode_selection=mode,
        rows=[row for row in data['rows'] if row['mode']==mode])


def costs(data):
    validate(data)
    result=[]
    for case in sorted(CASES):
        groups={mode:{row['repetition']:row for row in data['rows'] if row['mode']==mode and row['case']==case} for mode in MODES}
        entry={'case':case,'metrics':{}}
        for field in ['wall_ns','host_cpu_ticks','p95_ns','p99_ns','peak_host_rss_kib','peak_host_pss_kib']:
            values={mode:[row[field] for row in groups[mode].values() if row.get(field) is not None] for mode in MODES}
            metric={'medians':{mode:statistics.median(v) if len(v)>=3 else None for mode,v in values.items()},'paired':{}}
            for mode in MODES[1:]:
                changes=[]
                for repetition,off in groups['off'].items():
                    baseline=off.get(field);other=groups[mode][repetition].get(field)
                    if baseline and other is not None:changes.append((other/baseline-1)*100)
                metric['paired'][mode]={'median_change_percent':statistics.median(changes) if len(changes)>=3 else None,'pairs':len(changes)}
            entry['metrics'][field]=metric
        result.append(entry)
    return {'source':data['source'],'cases':result,
        'limits':'Median paired ratios within each repetition; balanced mode order reduces order bias but is not a confidence interval, isolated CPU attribution or game TPS.'}


def analyze(before,after=None,wall_limit=20,tail_limit=30):
    result={'schema_version':1,'before':costs(before)}
    if after is not None:
        result['after']=costs(after)
        for key in ['mode_orders','order_seed']:
            if before[key]!=after[key]:raise ValueError('study mode order differs')
        result['comparisons']={mode:compare_runtime(view(before,mode),view(after,mode),wall_limit,tail_limit) for mode in MODES}
    return result


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before',type=Path);parser.add_argument('after',type=Path,nargs='?')
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--max-wall-regression',type=float,default=20)
    parser.add_argument('--max-tail-regression',type=float,default=30)
    args=parser.parse_args();assert not args.output.exists(),'Choose a new output path'
    result=analyze(json.loads(args.before.read_text()),json.loads(args.after.read_text()) if args.after else None,args.max_wall_regression,args.max_tail_regression)
    args.output.write_text(json.dumps(result,indent=2)+'\n')
    for mode,comparison in result.get('comparisons',{}).items():
        for case in comparison['cases']:
            print(mode,case['case'],'wall change %=',case['metrics']['wall_ns']['change_percent'])
    for period in ['before','after']:
        if period in result:
            for case in result[period]['cases']:
                print(period,case['case'],'mode wall costs',case['metrics']['wall_ns']['paired'])
    if any(c['regressions'] for c in result.get('comparisons',{}).values()):raise SystemExit('Implementation regression guard failed; inspect the recorded comparisons.')
