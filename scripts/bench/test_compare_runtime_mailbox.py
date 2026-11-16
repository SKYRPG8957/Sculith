import copy
import unittest
from compare_runtime_mailbox import compare
from test_compare_plugin_dispatch import fixture


def runtime_fixture():
    data=fixture();data['kind']='world-mailbox-resource-benchmark';data['resource_helper_sha256']='helper'
    for row in data['rows']:row.update(clients=32,workers=0,subscribers=0)
    return data


class RuntimeComparisonTests(unittest.TestCase):
    def test_rejects_changed_queue_requesters_sampling_and_other_benchmark_kinds(self):
        original=runtime_fixture()
        for key in ['kind','resource_helper_sha256']:
            changed=copy.deepcopy(original);changed[key]='different'
            with self.assertRaises(ValueError):compare(original,changed)
        changed=copy.deepcopy(original);changed['rows'][0]['clients']=1
        with self.assertRaises(ValueError):compare(original,changed)
    def test_preserves_the_common_clean_source_and_regression_guards(self):
        before,after=runtime_fixture(),runtime_fixture()
        self.assertEqual(compare(before,after)['regressions'],[])
        for row in after['rows']:row['wall_ns']*=2
        self.assertTrue(compare(before,after)['regressions'])
        after['worktree_dirty']=True
        with self.assertRaises(ValueError):compare(before,after)
