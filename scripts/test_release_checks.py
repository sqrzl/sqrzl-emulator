import unittest
from require_release_checks import qualified

class ReleaseChecksTests(unittest.TestCase):
    def test_requires_exact_completed_successful_source(self):
        run = dict(headSha='a' * 40, status='completed', conclusion='success', event='pull_request')
        self.assertTrue(qualified([run], 'a' * 40, 'ci.yml'))
        self.assertFalse(qualified([run], 'b' * 40, 'ci.yml'))
        for key, value in [('status', 'in_progress'), ('conclusion', 'failure'), ('conclusion', 'cancelled')]:
            self.assertFalse(qualified([{**run, key:value}], 'a' * 40, 'ci.yml'))

    def test_narrowable_manual_campaign_does_not_qualify_release(self):
        run = dict(headSha='a' * 40, status='completed', conclusion='success', event='workflow_dispatch')
        self.assertFalse(qualified([run], 'a' * 40, 'large-upload-qualification.yml'))
        self.assertTrue(qualified([{**run, 'event':'push'}], 'a' * 40, 'large-upload-qualification.yml'))

if __name__ == '__main__': unittest.main()
