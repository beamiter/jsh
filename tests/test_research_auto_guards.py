"""Regression tests for secret scope, transport retries and budget recovery."""
import json
import os
import unittest
from unittest.mock import patch

from test_research_auto import AutoFixture, r


class AutoGuardTests(AutoFixture):
    def test_port_zero_is_rejected_without_initialization(self):
        self.baseline()
        with self.assertRaises(r.ResearchError):
            self.loop.initialize({**self.config, 'endpoint': 'http://127.0.0.1:0/v1/chat/completions'})
        self.assertFalse(self.loop.state_path.exists())
        self.assertEqual(self.server.calls, [])

    def test_reserved_tokens_cannot_disappear_from_persisted_state(self):
        self.ready()
        state = self.loop.load()
        state['attempts'].append({'id': 'lost', 'status': 'running'})
        self.loop.save(state)
        with self.assertRaisesRegex(r.ResearchError, 'reservations'):
            self.loop.load()
        self.assertEqual(self.server.calls, [])

    def test_expected_empty_queue_does_not_fall_back_to_another_operation(self):
        self.baseline()
        before = self.store.load()
        with self.assertRaisesRegex(r.ResearchError, 'queue'):
            r.step(self.store, expected_trial_id='e000001')
        self.assertEqual(self.store.load(), before)

    def test_evaluator_does_not_inherit_configured_model_key(self):
        self.config['api_key_env'] = 'EXPERIMENT_MODEL_KEY'
        self.ready()
        sha = self.candidate("import os\nassert 'EXPERIMENT_MODEL_KEY' not in os.environ\nSCORE = 8\n")
        trial = self.store.enqueue(self.proposal(sha))
        with patch.dict(os.environ, {'EXPERIMENT_MODEL_KEY': 'unit-test-only-not-a-real-key'}):
            result = self.loop.evaluate(trial['id'], self.loop.load()['config'])
            self.assertEqual(os.environ['EXPERIMENT_MODEL_KEY'], 'unit-test-only-not-a-real-key')
        self.assertEqual(result['status'], 'kept', result)
        self.assertEqual(self.server.calls, [])

    def test_key_is_restored_even_when_queue_guard_rejects(self):
        self.ready()
        with patch.dict(os.environ, {'JSH_RESEARCH_API_KEY': 'unit-test-only'}):
            with self.assertRaises(r.ResearchError):
                self.loop.evaluate('e999999', self.loop.load()['config'])
            self.assertEqual(os.environ['JSH_RESEARCH_API_KEY'], 'unit-test-only')


@unittest.skipUnless(os.environ.get('JSH_RESEARCH_BINARY'), 'requires compiled jsh binary')
class AutoTransportGuardTests(AutoFixture):
    def test_transport_failure_stops_multi_attempt_invocation_after_one_request(self):
        self.ready()
        self.server.mode = 'redirect'
        result = self.native('auto-run', '--execute', '--limit', '5')
        self.assertEqual(result.returncode, 1, result.stderr + result.stdout)
        report = json.loads(result.stdout)
        self.assertEqual(report['stop_reason'], 'transport_error')
        self.assertEqual(len(self.server.calls), 1)
        self.assertEqual(report['reserved_output_tokens'], 256)
        self.assertEqual(len(self.store.load()['trials']), 1)

    def test_successful_recovery_does_not_consume_failure_streak(self):
        self.ready()
        self.assertEqual(self.native('auto-run', '--execute').returncode, 0)
        state = self.loop.load()
        state['attempts'][0]['status'] = 'running'
        state['failure_streak'] = 1
        self.loop.save(state)
        recovered = self.loop.recover()
        self.assertEqual(recovered['attempts'][0]['status'], 'kept')
        self.assertEqual(recovered['failure_streak'], 0)
        self.assertEqual(len(self.server.calls), 1)


if __name__ == '__main__':
    unittest.main()
