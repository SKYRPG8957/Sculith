import copy
import unittest

from compare_chunk_encoding import compare
from test_compare_plugin_dispatch import fixture as resource_fixture


def fixture():
    result = resource_fixture()
    result.update(kind='chunk-encoding-resource-benchmark', resource_helper_sha256='helper', build_settings={})
    template = result['rows'][0]
    result['rows'] = [dict(template, case=f'{case}-{codec}', repetition=rep, workers=0, subscribers=0,
                           mode='encoding', encoded_bytes=123, encoded_sha256='fixed')
                      for case in ('flat', 'empty', 'sparse', 'solid', 'dense')
                      for codec in ('java', 'java-light', 'bedrock') for rep in range(3)]
    return result


class ChunkComparisonTests(unittest.TestCase):
    def test_matching_output_is_required_even_when_faster(self):
        before, after = fixture(), fixture()
        for row in after['rows']:
            row['wall_ns'] //= 2
        self.assertEqual(compare(before, after)['regressions'], [])
        for key, value in [('encoded_bytes', 124), ('encoded_sha256', 'changed')]:
            changed = copy.deepcopy(after)
            changed['rows'][0][key] = value
            with self.assertRaisesRegex(ValueError, 'wire output changed'):
                compare(before, changed)

    def test_incomplete_duplicate_or_changed_workloads_are_rejected(self):
        before = fixture()
        for transform in (lambda r: r['rows'].pop(), lambda r: r['rows'].append(r['rows'][0]),
                          lambda r: r['rows'][0].update(delivered_requests=0),
                          lambda r: r.update(worktree_dirty=True),
                          lambda r: r.update(resource_helper_sha256='changed'),
                          lambda r: r.update(build_settings={'changed': 'settings'})):
            after = copy.deepcopy(before)
            transform(after)
            with self.assertRaises(ValueError):
                compare(before, after)
