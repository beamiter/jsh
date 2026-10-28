"""Read-only readiness, typed feedback, plateau policy and recovery reconciliation."""
import copy
import json
import os
import unittest
from unittest.mock import patch

from test_research_auto import AutoFixture, a, r


class ObserveTests(AutoFixture):
    def test_report_is_read_only_and_contains_no_source_endpoint_or_model(self):
        self.ready()
        paths = [self.loop.state_path, self.store.state_path, self.bridge.identity_path]
        before = [path.read_bytes() for path in paths]
        report = self.loop.report()
        self.assertEqual(report['next_action'], 'needs_proposal')
        self.assertTrue(report['can_run'])
        self.assertEqual(report['remaining']['attempts'], 5)
        self.assertEqual(report['counts']['kept'], 0)
        self.assertEqual(report['recent_attempts'], [])
        encoded = json.dumps(report)
        for private in (self.config['endpoint'], self.config['model'], 'SCORE =', 'api_key_env'):
            self.assertNotIn(private, encoded)
        self.assertEqual([path.read_bytes() for path in paths], before)
        self.assertEqual(self.server.calls, [])

    def test_report_does_not_block_on_evaluator_or_loop_writer(self):
        self.ready()
        with self.store.lock(), self.loop.lock_store.lock():
            self.assertEqual(self.loop.report()['next_action'], 'needs_proposal')

    def test_inspection_never_recreates_a_missing_campaign_identity(self):
        self.ready()
        self.bridge.identity_path.unlink()
        with self.assertRaises((r.ResearchError, OSError)):
            self.loop.report()
        self.assertFalse(self.bridge.identity_path.exists())
        self.assertEqual(self.server.calls, [])

    def test_report_recent_bounds_are_enforced(self):
        self.ready()
        for recent in (0, 65, True, 1.0, '1'):
            with self.subTest(recent=recent), self.assertRaises(r.ResearchError):
                self.loop.report(recent)

    def test_stopped_preflight_does_not_read_sources_or_run_native_helpers(self):
        self.ready()
        self.store.stop()
        with patch.object(self.loop, 'sources', side_effect=AssertionError('must not read')), \
             patch.object(self.loop, 'native', side_effect=AssertionError('must not spawn')):
            result = self.loop.preflight()
        self.assertFalse(result['ready'])
        self.assertEqual(result['reason'], 'stopped')
        self.assertEqual(self.server.calls, [])

    def test_preflight_rechecks_context_after_native_validation(self):
        self.ready()
        with patch.object(self.loop, 'native', side_effect=lambda *args: self.bridge.resume()):
            result = self.loop.preflight()
        self.assertFalse(result['ready'])
        self.assertEqual(result['reason'], 'freshness_check_failed')
        self.assertEqual(self.loop.load()['reserved_output_tokens'], 0)
        self.assertEqual(self.server.calls, [])

    def test_time_ledger_and_attempt_identity_corruption_fail_closed(self):
        self.ready()
        state = self.loop.load()
        state['charged_seconds'] = 1
        self.loop.save(state)
        with self.assertRaisesRegex(r.ResearchError, 'time charges'):
            self.loop.load()

    def test_legacy_configuration_does_not_acquire_a_plateau_limit(self):
        self.ready()
        state = self.loop.load()
        del state['config']['max_no_improvement']
        state['config_digest'] = r.digest(state['config'])
        self.loop.save(state)
        report = self.loop.report()
        self.assertEqual(report['max_no_improvement'], 0)
        self.assertEqual(report['next_action'], 'needs_proposal')

    def test_patience_is_a_strict_bounded_integer(self):
        self.baseline()
        for patience in (True, -1, 1001, '2', 1.0):
            with self.subTest(patience=patience), self.assertRaises(r.ResearchError):
                self.loop.initialize({**self.config, 'max_no_improvement': patience})
        self.assertEqual(self.server.calls, [])

    def test_only_complete_matching_seed_sets_become_feedback_scores(self):
        self.ready()
        campaign = self.store.load()
        trial = copy.deepcopy(campaign['trials'][0])
        self.assertIsNotNone(self.loop.scores(trial, campaign['manifest'])['screen'])
        trial['results']['screen'].pop()
        self.assertIsNone(self.loop.scores(trial, campaign['manifest'])['screen'])
        trial = copy.deepcopy(campaign['trials'][0])
        trial['results']['screen'][0]['seed'] = 999
        self.assertIsNone(self.loop.scores(trial, campaign['manifest'])['screen'])
        trial = copy.deepcopy(campaign['trials'][0])
        trial['results']['screen'][0]['metrics']['loss'] = True
        with self.assertRaises(r.ResearchError):
            self.loop.scores(trial, campaign['manifest'])

    def test_recovery_rejects_an_active_campaign_writer_before_mutation(self):
        self.ready()
        before = self.loop.state_path.read_bytes()
        with self.store.lock(), self.assertRaises((r.ResearchError, OSError)):
            self.loop.recover()
        self.assertEqual(self.loop.state_path.read_bytes(), before)

    def test_failure_and_nonimprovement_counters_have_different_meanings(self):
        attempts = [{'status': 'screened_out', 'base_commit': 'a'},
                    {'status': 'failed', 'base_commit': 'a'},
                    {'status': 'archived', 'base_commit': 'a'}]
        self.assertEqual(a.streaks(attempts, 'a'), (0, 2))
        self.assertEqual(a.streaks(attempts, 'b'), (0, 0))
        attempts.append({'status': 'kept', 'base_commit': 'a'})
        self.assertEqual(a.streaks(attempts, 'a'), (0, 0))


@unittest.skipUnless(os.environ.get('JSH_RESEARCH_BINARY'), 'requires compiled jsh binary')
class ObserveNativeTests(AutoFixture):
    def test_preflight_uses_actual_validator_without_network_or_state_writes(self):
        self.ready()
        paths = [self.loop.state_path, self.store.state_path, self.bridge.identity_path]
        before = [path.read_bytes() for path in paths]
        result = self.native('auto-preflight')
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        report = json.loads(result.stdout)
        self.assertTrue(report['ready'])
        self.assertEqual(report['reason'], 'ready_offline')
        self.assertFalse(report['network_checked'])
        self.assertFalse(report['execution_authorized'])
        self.assertFalse(report['sandbox_verified'])
        self.assertEqual(report['source_files'], 1)
        self.assertEqual([path.read_bytes() for path in paths], before)
        self.assertEqual(self.server.calls, [])

    def test_report_is_bounded_and_reports_measured_best_scores(self):
        self.ready()
        run = self.native('auto-run', '--execute', '--limit', '3')
        self.assertEqual(run.returncode, 0, run.stderr + run.stdout)
        result = self.native('auto-report', '--recent', '1')
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report['counts']['kept'], 3)
        self.assertEqual(report['omitted_attempts'], 2)
        self.assertEqual(len(report['recent_attempts']), 1)
        self.assertEqual(report['recent_attempts'][0]['attempt'], 3)
        self.assertAlmostEqual(report['best']['scores']['confirm'], 7.0000035)
        self.assertEqual(len(self.server.calls), 3)

    def test_feedback_distinguishes_plan_rejection_from_evaluation(self):
        self.ready()
        self.server.mode = 'invalid'
        self.assertEqual(self.native('auto-run', '--execute').returncode, 1)
        self.server.mode = 'valid'
        result = self.native('auto-run', '--execute')
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        sent = json.loads(self.server.calls[1]['messages'][1]['content'])['untrusted_authoring_input']
        self.assertEqual(sent['schema_version'], 2)
        record = sent['feedback'][0]
        self.assertEqual(record['failure_code'], 'plan_rejected')
        self.assertEqual(record['stage'], 'validate_plan')
        self.assertIsNone(record['trial_id'])
        self.assertEqual(record['scores'], {'screen': None, 'confirm': None})
        self.assertEqual(self.loop.load()['failure_streak'], 0)

    def test_plateau_stops_across_invocations_without_resetting_budget(self):
        self.manifest['objective']['direction'] = 'max'
        self.config['max_no_improvement'] = 2
        self.ready()
        result = self.native('auto-run', '--execute', '--limit', '5')
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertEqual(json.loads(result.stdout)['stop_reason'], 'no_improvement_limit')
        self.assertEqual(len(self.server.calls), 2)
        before = self.loop.state_path.read_bytes()
        self.assertEqual(self.loop.report()['no_improvement_streak'], 2)
        self.assertEqual(self.loop.report()['failure_streak'], 0)
        self.assertEqual(self.native('resume').returncode, 0)
        again = self.native('auto-run', '--execute', '--limit', '5')
        self.assertEqual(json.loads(again.stdout)['stop_reason'], 'no_improvement_limit')
        self.assertEqual(self.loop.state_path.read_bytes(), before)
        self.assertEqual(len(self.server.calls), 2)
        feedback = json.loads(self.server.calls[1]['messages'][1]['content'])['untrusted_authoring_input']['feedback'][0]
        self.assertEqual(feedback['status'], 'screened_out')
        self.assertIsNone(feedback['scores']['confirm'])

    def test_preflight_report_and_execution_agree_on_token_exhaustion(self):
        self.config['max_output_tokens'] = 256
        self.ready()
        self.assertEqual(self.native('auto-run', '--execute').returncode, 0)
        before = self.loop.state_path.read_bytes()
        self.assertEqual(self.loop.report()['next_action'], 'auto_budget_exhausted')
        result = self.native('auto-preflight')
        self.assertEqual(result.returncode, 1)
        self.assertEqual(json.loads(result.stdout)['reason'], 'auto_budget_exhausted')
        result = self.native('auto-run', '--execute')
        self.assertEqual(json.loads(result.stdout)['stop_reason'], 'auto_budget_exhausted')
        self.assertEqual(len(self.server.calls), 1)
        self.assertEqual(self.loop.state_path.read_bytes(), before)

    def test_interrupt_after_admission_reconciles_after_manual_evaluation(self):
        self.ready()
        with patch.object(self.loop, 'evaluate', side_effect=KeyboardInterrupt):
            interrupted = self.loop.run(execute=True)
        self.assertEqual(interrupted['attempts'][0]['status'], 'interrupted')
        self.assertEqual(self.loop.report()['next_action'], 'evaluate_queued')
        outcome = r.step(self.store)
        self.assertEqual(outcome['status'], 'kept')
        before = self.loop.load()
        self.assertEqual(self.loop.report()['next_action'], 'needs_reconciliation')
        blocked = self.native('auto-run', '--execute')
        self.assertEqual(json.loads(blocked.stdout)['stop_reason'], 'needs_reconciliation')
        recovered = self.loop.recover()
        self.assertEqual(recovered['attempts'][0]['status'], 'kept')
        self.assertEqual(recovered['failure_streak'], 0)
        self.assertEqual(recovered['charged_seconds'], before['charged_seconds'])
        self.assertEqual(recovered['reserved_output_tokens'], before['reserved_output_tokens'])
        snapshot = self.loop.state_path.read_bytes()
        self.loop.recover()
        self.assertEqual(self.loop.state_path.read_bytes(), snapshot)
        self.assertEqual(len(self.server.calls), 1)

    def test_recovery_rejects_changed_trial_binding_atomically(self):
        self.ready()
        self.assertEqual(self.native('auto-run', '--execute').returncode, 0)
        state = self.loop.load()
        state['attempts'][0]['status'] = 'running'
        state['attempts'][0]['candidate_commit'] = 'f' * 40
        self.loop.save(state)
        before = self.loop.state_path.read_bytes()
        with self.assertRaises(r.ResearchError):
            self.loop.recover()
        self.assertEqual(self.loop.state_path.read_bytes(), before)
        self.assertEqual(len(self.server.calls), 1)

    def test_report_projects_fixed_fields_instead_of_persisted_error_text(self):
        self.ready()
        self.server.mode = 'invalid'
        self.assertEqual(self.native('auto-run', '--execute').returncode, 1)
        state = self.loop.load()
        state['attempts'][0]['reason'] = 'private-provider-body-12345'
        self.loop.save(state)
        result = self.native('auto-report')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn('private-provider-body', result.stdout)
        self.assertNotIn(self.config['endpoint'], result.stdout)
        self.assertEqual(json.loads(result.stdout)['recent_attempts'][0]['failure_code'], 'plan_rejected')


if __name__ == '__main__':
    unittest.main()
