import copy
import unittest
from analyze_runtime_modes import analyze,costs,validate,view,CASES,MODES
from runtime_mailbox import mode_orders
from test_compare_runtime_mailbox import runtime_fixture


def study():
    data=runtime_fixture();data.update(kind='world-mailbox-mode-study',mode_selection='all',order_seed=7,mode_orders=mode_orders('all',5,7),warmup_dispatches=256)
    base=data['rows'][0];data['rows']=[]
    # Large run-to-run variation; paired mode costs remain exactly 10%/20%.
    for repetition,magnitude in enumerate([100,10000,500,2000,1000]):
        for case,clients in CASES.items():
            for index,mode in enumerate(MODES):
                row=dict(base,case=case,clients=clients,mode=mode,repetition=repetition,iterations=1024,delivered_requests=1024,
                    completed_commands=1281,timed_commands=0 if mode=='off' else 1281,retained_samples=512 if mode=='profile' else 0,
                    overwritten_samples=769 if mode=='profile' else 0)
                for field in ['wall_ns','p95_ns','p99_ns']:row[field]=magnitude*(1+index/10)
                data['rows'].append(row)
    return data


class ModeStudyTests(unittest.TestCase):
    def test_balanced_seeded_orders_and_pairing_resist_cohort_magnitude(self):
        orders=mode_orders('all',6,7)
        self.assertEqual(orders,mode_orders('all',6,7))
        for mode in MODES:self.assertEqual([order[0] for order in orders].count(mode),2)
        result=costs(study())
        for case in result['cases']:
            self.assertAlmostEqual(case['metrics']['wall_ns']['paired']['monitor']['median_change_percent'],10)
            self.assertAlmostEqual(case['metrics']['wall_ns']['paired']['profile']['median_change_percent'],20)
    def test_rejects_missing_duplicate_and_incomplete_work_cells(self):
        original=study()
        mutations=[lambda d:d['rows'].pop(),lambda d:d['rows'].append(copy.deepcopy(d['rows'][0])),
            lambda d:d['rows'][0].update(completed_commands=0),lambda d:d['rows'][1].update(timed_commands=0),
            lambda d:d['rows'][2].update(retained_samples=513),lambda d:d.update(worktree_dirty=True),
            lambda d:d['mode_orders'].reverse()]
        for mutate in mutations:
            changed=copy.deepcopy(original);mutate(changed)
            with self.assertRaises(ValueError):validate(changed)
    def test_views_keep_same_mode_and_shared_regression_guards(self):
        original=study();self.assertTrue(all(r['mode']=='off' for r in view(original,'off')['rows']))
        changed=copy.deepcopy(original)
        for row in changed['rows']:
            if row['mode']=='profile':row['wall_ns']*=2
        comparison=analyze(original,changed)['comparisons']
        self.assertEqual(comparison['off']['regressions'],[])
        self.assertTrue(comparison['profile']['regressions'])
        changed['hardware']='different'
        with self.assertRaises(ValueError):analyze(original,changed)
