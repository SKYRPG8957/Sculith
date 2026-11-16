import copy
import unittest
from compare_plugin_dispatch import compare


def fixture():
    result = {'worktree_dirty':False,'source':'fixture'}
    for key in ['schema_version','kind','hardware','rust','profile','warmup_dispatches','fixture_sha256','harness_sha256','driver_sha256','sampler_period_seconds']:
        result[key] = key
    result['rows'] = [dict(case='one',iterations=100,workers=8,subscribers=1,mode='off',delivered_requests=100,
                          wall_ns=value,p50_ns=value,p95_ns=value,p99_ns=value,host_cpu_ticks=2,worker_cpu_ticks=3)
                      for value in [100,100,1000,100,100]]
    return result


class ComparisonTests(unittest.TestCase):
    def test_median_resists_one_outlier_and_exact_limit_is_not_a_regression(self):
        before,after = fixture(),fixture()
        for row in after['rows']:
            for field in ['wall_ns','p50_ns','p95_ns','p99_ns']:
                row[field] *= 1.2
        result = compare(before,after,20,30)
        self.assertEqual(result['regressions'],[])
        self.assertAlmostEqual(result['cases'][0]['metrics']['wall_ns']['change_percent'],20)
        self.assertIsNone(result['cases'][0]['metrics']['peak_host_pss_kib']['change_percent'])

    def test_actual_regression_is_reported(self):
        before,after = fixture(),fixture()
        for row in after['rows']: row['wall_ns'] *= 2
        self.assertEqual(compare(before,after)['regressions'][0]['metric'],'wall_ns')

    def test_incompatible_or_uncommitted_runs_are_rejected(self):
        original=fixture()
        for key in ['hardware','fixture_sha256','harness_sha256','driver_sha256','worktree_dirty']:
            changed=copy.deepcopy(original);changed[key]='different'
            with self.assertRaises(ValueError):compare(original,changed)
        changed=copy.deepcopy(original);changed['rows'][0]['iterations'] += 1
        with self.assertRaises(ValueError):compare(original,changed)
        changed=copy.deepcopy(original);changed['rows']=changed['rows'][:2]
        with self.assertRaises(ValueError):compare(original,changed)


if __name__ == '__main__':unittest.main()
