"""A single monotonic time budget spans the entire author/evaluate attempt."""
from pathlib import Path
import subprocess
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from test_research_auto import AutoFixture, a, r


class AttemptDeadlineTests(AutoFixture):
    def set_budget(self):
        self.config['max_seconds'] = 133
        self.ready()
        self.clock = [1000.0]

    def plan(self, command, input_path, *unused):
        if command == 'author-prepare':
            return {'messages': [], 'response_schema': {}}
        value = r.read_json(input_path)
        context, source = value['context'], value['files'][0]
        return {'schema_version': 1, 'campaign_id': context['campaign_id'],
                'manifest_digest': context['manifest_digest'], 'expected_revision': context['revision'],
                'base_commit': context['best']['commit'], 'hypothesis': 'Reduce fixture loss',
                'edits': [{'path': source['path'], 'before_blob': source['blob'], 'content': 'SCORE = 9\n'}]}

    def test_preparation_exhaustion_never_starts_a_model_and_cannot_reset_on_restart(self):
        self.set_budget()
        sources = self.loop.sources

        def delayed(*args):
            result = sources(*args)
            self.clock[0] += 134
            return result

        with patch.object(a.time, 'monotonic', side_effect=lambda: self.clock[0]), \
             patch.object(self.loop, 'sources', side_effect=delayed), \
             patch.object(self.loop, 'native') as native, patch.object(self.loop, 'model_call') as model:
            result = self.loop.run(limit=5, execute=True)
        self.assertEqual(result['stop_reason'], 'auto_budget_exhausted')
        self.assertEqual(result['charged_seconds'], 134)
        self.assertEqual(result['reserved_output_tokens'], 256)
        self.assertEqual(len(result['attempts']), 1)
        native.assert_not_called()
        model.assert_not_called()
        self.assertIsNone(self.loop._attempt_deadline)
        restarted = a.AutoLoop(r, self.loop.b, self.store.root, self.loop.binary)
        before = self.loop.state_path.read_bytes()
        self.assertEqual(restarted.run(execute=True)['stop_reason'], 'auto_budget_exhausted')
        self.assertEqual(self.loop.state_path.read_bytes(), before)
        self.assertEqual(self.server.calls, [])

    def test_late_model_response_never_commits_or_submits(self):
        self.set_budget()

        def delayed(*args):
            self.clock[0] += 134
            return self.base / 'late-response.json'

        with patch.object(a.time, 'monotonic', side_effect=lambda: self.clock[0]), \
             patch.object(self.loop, 'native', side_effect=self.plan) as native, \
             patch.object(self.loop, 'model_call', side_effect=delayed), \
             patch.object(self.loop, 'commit_plan') as commit, patch.object(self.loop.bridge, 'submit') as submit:
            result = self.loop.run(execute=True)
        self.assertEqual(result['stop_reason'], 'auto_budget_exhausted')
        self.assertEqual(native.call_count, 1)
        commit.assert_not_called()
        submit.assert_not_called()
        self.assertEqual(len(self.store.load()['trials']), 1)
        self.assertIsNone(self.loop._attempt_deadline)

    def test_fast_work_near_deadline_can_still_complete(self):
        self.set_budget()

        def prepare(command, *args):
            if command == 'author-prepare':
                self.clock[0] += 124.5  # 8.5 seconds remain; a full evaluation needs 8.
            return self.plan(command, *args)

        with patch.object(a.time, 'monotonic', side_effect=lambda: self.clock[0]), \
             patch.object(self.loop, 'native', side_effect=prepare), \
             patch.object(self.loop, 'model_call', return_value=self.base / 'response.json'):
            result = self.loop.run(execute=True)
        self.assertEqual(result['attempts'][0]['status'], 'kept', result)
        self.assertEqual(result['charged_seconds'], 124.5)
        self.assertEqual(len(self.store.load()['trials']), 2)
        self.assertIsNone(self.loop._attempt_deadline)

    def test_time_exhausted_after_admission_preserves_queue_without_evaluation(self):
        self.set_budget()
        submit = self.loop.bridge.submit

        def delayed(*args, **kwargs):
            result = submit(*args, **kwargs)
            self.clock[0] += 134
            return result

        with patch.object(a.time, 'monotonic', side_effect=lambda: self.clock[0]), \
             patch.object(self.loop, 'native', side_effect=self.plan), \
             patch.object(self.loop, 'model_call', return_value=self.base / 'response.json'), \
             patch.object(self.loop.bridge, 'submit', side_effect=delayed), \
             patch.object(self.loop, 'evaluate') as evaluate:
            result = self.loop.run(execute=True)
        self.assertEqual(result['stop_reason'], 'auto_budget_exhausted')
        self.assertEqual(self.store.load()['trials'][-1]['status'], 'queued')
        self.assertEqual(result['attempts'][0]['trial_id'], 'e000001')
        evaluate.assert_not_called()

    def test_stop_takes_priority_over_simultaneous_deadline(self):
        self.set_budget()

        def stopped(*args):
            self.clock[0] += 134
            self.store.stop()
            raise subprocess.TimeoutExpired('fixture', 1)

        with patch.object(a.time, 'monotonic', side_effect=lambda: self.clock[0]), \
             patch.object(self.loop, 'sources', side_effect=stopped):
            result = self.loop.run(execute=True)
        self.assertEqual(result['stop_reason'], 'interrupted')
        self.assertEqual(result['attempts'][0]['status'], 'interrupted')
        self.assertIsNone(self.loop._attempt_deadline)

    def test_initial_save_failure_does_not_leave_an_active_deadline(self):
        self.set_budget()
        with patch.object(self.loop, 'save', side_effect=OSError('failed reservation')), self.assertRaises(OSError):
            self.loop.run(execute=True)
        self.assertIsNone(self.loop._attempt_deadline)
        self.assertEqual(self.loop.load()['attempts'], [])

    def test_native_and_http_timeouts_are_capped_to_remaining_time(self):
        self.set_budget()
        self.loop._attempt_deadline = 1000.25
        state = self.loop.load()
        attempt = {'id': 'fixture-model', 'status': 'running', 'stage': 'prepare', 'charged_seconds': 0}
        state['attempts'].append(attempt)
        state['reserved_output_tokens'] = 256
        directory = self.loop.directory / 'fixture'
        directory.mkdir()
        self.loop.binary = '/unused/native'
        process = SimpleNamespace(pid=999999, poll=lambda: 0, returncode=0)
        try:
            with patch.object(a.time, 'monotonic', side_effect=lambda: self.clock[0]), \
                 patch.object(a.subprocess, 'run', return_value=SimpleNamespace(returncode=0, stdout=b'{}')) as native:
                self.loop.native('author-prepare', Path('/unused/input'))
                self.assertEqual(native.call_args.kwargs['timeout'], 0.25)
            with patch.object(a.time, 'monotonic', side_effect=lambda: self.clock[0]), \
                 patch.object(a.subprocess, 'Popen', return_value=process), patch.object(r, 'kill_group'):
                self.loop.model_call(state, attempt, directory, {'messages': [], 'response_schema': {}})
            spec = r.read_json(directory / 'http-request.json')
            self.assertEqual(spec['timeout'], 0.25)
        finally:
            self.loop._attempt_deadline = None

    def test_evaluation_does_not_reserve_or_launch_without_a_full_trial_window(self):
        self.set_budget()
        trial = self.store.enqueue(self.proposal(self.candidate()))
        before = self.store.state_path.read_bytes()
        with patch.object(r.time, 'monotonic', return_value=1000), \
             self.assertRaisesRegex(r.ResearchError, 'deadline.*reservation'):
            r.step(self.store, expected_trial_id=trial['id'], deadline=1001)
        self.assertEqual(self.store.state_path.read_bytes(), before)
        self.assertFalse((self.store.root / 'trials/e000001').exists())


if __name__ == '__main__':
    unittest.main()
