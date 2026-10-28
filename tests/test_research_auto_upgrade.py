"""Evidence consistency, batched sources and configurable model requests."""
import copy
import json
import os
import subprocess
import time
import unittest
from unittest.mock import patch

from test_research_auto import AutoFixture, a, r
from test_research_bridge import BridgeFixture


class EvidenceUpgradeTests(BridgeFixture):
    def test_bridge_never_reports_wrong_duplicate_or_boolean_seeds(self):
        self.baseline()
        original = self.store.load()
        for seeds in ([1, 999], [1, 1], [2, 1], [True, 2]):
            state = copy.deepcopy(original)
            for item, seed in zip(state['trials'][0]['results']['screen'], seeds):
                item['seed'] = seed
            self.store.save(state)
            self.assertNotIn('screen', self.bridge.snapshot()['recent_trials'][0]['scores'])

    def test_bridge_checks_guardrails_and_numeric_types_before_feedback(self):
        self.baseline()
        original = self.store.load()
        for metric, value in (('loss', True), ('violations', 1)):
            state = copy.deepcopy(original)
            state['trials'][0]['results']['screen'][0]['metrics'][metric] = value
            self.store.save(state)
            with self.assertRaises(r.ResearchError):
                self.bridge.snapshot()

    def test_best_measurements_survive_the_recent_window(self):
        self.baseline()
        state = self.store.load()
        for index in range(1, 20):
            trial = copy.deepcopy(state['trials'][0])
            trial.update(id=f'e{index:06d}', request_id=f'loss-{index}', status='screened_out',
                         parents=['baseline'], kind='candidate')
            trial['results'].pop('confirm')
            state['trials'].append(trial)
        state['charged_seconds'] = sum(trial['charged_seconds'] for trial in state['trials'])
        self.store.save(state)
        context = self.bridge.snapshot()
        self.assertEqual(len(context['recent_trials']), 16)
        self.assertEqual(context['omitted_trials'], 4)
        self.assertEqual(context['recent_trials'][0]['id'], 'baseline')
        self.assertEqual(context['recent_trials'][-1]['id'], 'e000019')
        self.assertEqual(set(context['recent_trials'][0]['scores']), {'screen', 'confirm'})

    def test_stop_during_checkout_never_launches_evaluation(self):
        self.init()
        atomic_json = r.atomic_json

        def stopped(path, value):
            atomic_json(path, value)
            if path.name == 'request.json':
                self.store.stop()

        with patch.object(r, 'atomic_json', side_effect=stopped), self.assertRaises(r.Stopped):
            r.step(self.store)
        self.assertEqual(self.store.load()['trials'][0]['status'], 'interrupted')
        self.assertFalse(list(self.store.root.glob('trials/*/*/stdout.log')))
        self.assertIsNone(self.store.load()['best_id'])

    def test_stop_during_validation_never_promotes(self):
        self.init()
        validate = r.validate_metrics

        def stopped(*args):
            result = validate(*args)
            self.store.stop()
            return result

        with patch.object(r, 'validate_metrics', side_effect=stopped), self.assertRaises(r.Stopped):
            r.step(self.store)
        self.assertIsNone(self.store.load()['best_id'])

    def test_stop_after_last_receipt_never_promotes(self):
        self.init()
        atomic_json = r.atomic_json

        def stopped(path, value):
            atomic_json(path, value)
            if path.name == 'receipt.json' and value['stage'] == 'confirm' and value['seed'] == 4:
                self.store.stop()

        with patch.object(r, 'atomic_json', side_effect=stopped), self.assertRaises(r.Stopped):
            r.step(self.store)
        self.assertIsNone(self.store.load()['best_id'])
        self.assertEqual(self.store.load()['trials'][0]['status'], 'interrupted')

    def test_corrupt_incumbent_cannot_launch_or_consume_trial_budget(self):
        self.baseline()
        self.store.enqueue(self.proposal(self.candidate()))
        state = self.store.load()
        state['trials'][0]['results']['screen'][0]['seed'] = 999
        self.store.save(state)
        before = self.store.state_path.read_bytes()
        with self.assertRaisesRegex(r.ResearchError, 'incumbent'):
            r.step(self.store)
        self.assertEqual(self.store.state_path.read_bytes(), before)
        self.assertFalse((self.store.root / 'trials/e000001').exists())

    def test_validation_is_part_of_the_timeout(self):
        self.manifest['budget']['timeout_seconds'] = 10
        self.init()
        validate = r.validate_metrics
        monotonic = time.monotonic
        elapsed = [0.0]

        def delayed(*args):
            result = validate(*args)
            elapsed[0] += 11.0
            return result

        with patch.object(r.time, 'monotonic', side_effect=lambda: monotonic() + elapsed[0]), \
             patch.object(r, 'validate_metrics', side_effect=delayed):
            result = r.step(self.store)
        self.assertEqual(result['status'], 'failed', result)
        self.assertIn('validation timed out', result['reason'])
        self.assertIsNone(self.store.load()['best_id'])


class SourceAndModelUpgradeTests(AutoFixture):
    def add_sources(self, values):
        self.manifest['editable_paths'].append('src')
        for path, content in values.items():
            target = self.repo / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(content)
        self.git('add', '.')
        self.git('commit', '-qm', 'source fixtures')

    def test_eight_sources_use_two_git_processes_and_preserve_bytes_and_order(self):
        values = {f'src/file {i}.py': ('# 中文\r\nSCORE = %s\n' % i).encode() for i in range(8)}
        values['src/file 0.py'] = b''
        values['src/file 1.py'] = values['src/file 2.py']
        self.add_sources(values)
        (self.repo / 'src/file 3.py').chmod(0o755)
        self.git('add', '.')
        self.git('commit', '-qm', 'executable source')
        self.config['source_paths'] = list(reversed(values))
        self.ready()
        context = self.bridge.snapshot()
        with patch.object(a.subprocess, 'run', wraps=subprocess.run) as calls:
            files = self.loop.sources(context, self.loop.load()['config'])
        self.assertEqual(len(calls.call_args_list), 2)
        self.assertEqual([item['path'] for item in files], self.config['source_paths'])
        for item in files:
            self.assertEqual(item['content'].encode(), values[item['path']])
            self.assertEqual(item['blob'], self.git('rev-parse', context['best']['commit'] + ':' + item['path']))

    def test_oversize_sources_fail_before_reading_any_blob(self):
        self.add_sources({'src/large.py': b'x' * 65537})
        self.baseline()
        context = self.bridge.snapshot()
        with patch.object(a.subprocess, 'run', wraps=subprocess.run) as calls, self.assertRaises(r.ResearchError):
            self.loop.sources(context, {**self.config, 'source_paths': ['src/large.py']})
        self.assertEqual(len(calls.call_args_list), 1)

    def test_batch_rejects_binary_utf8_errors_links_and_directories(self):
        self.add_sources({'src/binary.py': b'x\0y', 'src/nonutf8.py': b'\xff'})
        (self.repo / 'src/link.py').symlink_to('../candidate.py')
        self.git('add', '.')
        self.git('commit', '-qm', 'link fixture')
        self.baseline()
        context = self.bridge.snapshot()
        for path in ('src/binary.py', 'src/nonutf8.py', 'src/link.py', 'src', 'src/missing.py'):
            with self.subTest(path=path), self.assertRaises((r.ResearchError, ValueError)):
                self.loop.sources(context, {**self.config, 'source_paths': [path]})

    def test_sampling_rejects_unknown_fields_and_invalid_numbers_offline(self):
        self.baseline()
        for sampling in ({'temperature': True}, {'temperature': -0.1}, {'temperature': 2.1},
                         {'top_p': 0}, {'top_p': 1.1}, {'top_k': True}, {'top_k': 0},
                         {'seed': -1}, {'seed': 2**32}, {'seed': 1.0}, {'messages': []}, []):
            with self.subTest(sampling=sampling), self.assertRaises(r.ResearchError):
                self.loop.initialize({**self.config, 'sampling': sampling})
        for field in ('tokens', 'messages', True):
            with self.assertRaises(r.ResearchError):
                self.loop.initialize({**self.config, 'token_limit_field': field})
        self.assertFalse(self.loop.state_path.exists())
        self.assertEqual(self.server.calls, [])

    def test_malformed_batch_output_never_becomes_a_source_snapshot(self):
        self.ready()
        context = self.bridge.snapshot()
        execute = subprocess.run
        for corrupt in (lambda data: data[:-1], lambda data: data + b'extra',
                        lambda data: b'f' + data[1:], lambda data: data[:10]):
            def corrupted(*args, **kwargs):
                result = execute(*args, **kwargs)
                if args[0][-1] == '--batch':
                    result.stdout = corrupt(result.stdout)
                return result
            with patch.object(a.subprocess, 'run', side_effect=corrupted), self.assertRaises(r.ResearchError):
                self.loop.sources(context, self.loop.load()['config'])

    def test_comparison_uses_the_trial_incumbent_and_matching_seed_evidence(self):
        self.ready()
        self.store.enqueue(self.proposal(self.candidate()))
        first = r.step(self.store)
        self.store.enqueue(self.proposal(self.candidate('SCORE = 7\nCONFIRM = 9\n'), 'second'))
        second = r.step(self.store)
        state = self.store.load()
        trials = {item['id']: item for item in state['trials']}
        comparison = self.loop.comparison(second, trials, state['manifest'])
        self.assertEqual(second['status'], 'archived')
        self.assertEqual(comparison['reference']['id'], first['id'])
        self.assertAlmostEqual(comparison['stages']['screen']['mean_improvement'], 1)
        self.assertAlmostEqual(comparison['stages']['confirm']['mean_improvement'], -1)
        self.assertEqual(comparison['stages']['screen']['improved_seeds'], 2)
        self.assertTrue(comparison['stages']['screen']['meets_min_delta'])
        self.assertFalse(comparison['stages']['confirm']['meets_min_delta'])
        second['results']['confirm'][0]['seed'] = 999
        self.assertNotIn('confirm', self.loop.comparison(second, trials, state['manifest'])['stages'])

    def test_maximization_comparison_uses_the_strict_delta_threshold(self):
        self.manifest['objective'].update(direction='max', min_delta=2)
        self.ready()
        self.store.enqueue(self.proposal(self.candidate('SCORE = 12\n')))
        result = r.step(self.store)
        state = self.store.load()
        comparison = self.loop.comparison(result, {item['id']: item for item in state['trials']}, state['manifest'])
        self.assertEqual(result['status'], 'screened_out')
        self.assertAlmostEqual(comparison['stages']['screen']['mean_improvement'], 2)
        self.assertEqual(comparison['stages']['screen']['improved_seeds'], 2)
        self.assertFalse(comparison['stages']['screen']['meets_min_delta'])
        self.assertNotIn('confirm', comparison['stages'])


@unittest.skipUnless(os.environ.get('JSH_RESEARCH_BINARY'), 'requires compiled jsh binary')
class ModelUpgradeNativeTests(AutoFixture):
    def test_legacy_attempt_without_stage_remains_ready_offline(self):
        self.ready()
        state = self.loop.load()
        state['attempts'].append({'id': 'legacy-author-failure', 'status': 'author_failed', 'charged_seconds': 0})
        state['reserved_output_tokens'], state['failure_streak'] = 256, 1
        self.loop.save(state)
        before = self.loop.state_path.read_bytes()
        result = self.native('auto-preflight')
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertTrue(json.loads(result.stdout)['ready'])
        self.assertEqual(self.loop.state_path.read_bytes(), before)
        self.assertEqual(self.server.calls, [])

    def test_strict_endpoint_with_alternate_token_field_and_sampling_omissions(self):
        self.config.update(token_limit_field='max_completion_tokens',
                           sampling={'temperature': None, 'top_p': None, 'top_k': None, 'seed': 42})
        self.server.reject_fields = {'max_tokens', 'top_k', 'temperature', 'top_p'}
        self.ready()
        result = self.native('auto-run', '--execute')
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        body = self.server.calls[0]
        self.assertEqual(body['max_completion_tokens'], 256)
        self.assertEqual(body['seed'], 42)
        self.assertFalse(self.server.reject_fields.intersection(body))
        self.assertEqual(json.loads(result.stdout)['reserved_output_tokens'], 256)
        report = json.loads(self.native('auto-report').stdout)
        self.assertEqual(report['recent_attempts'][0]['comparison']['reference']['id'], 'baseline')

    def test_old_frozen_configuration_keeps_original_request_defaults(self):
        self.ready()
        state = self.loop.load()
        state['config'].pop('sampling')
        state['config'].pop('token_limit_field')
        state['config_digest'] = r.digest(state['config'])
        self.loop.save(state)
        result = self.native('auto-run', '--execute')
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        body = self.server.calls[0]
        self.assertEqual(body['max_tokens'], 256)
        self.assertEqual(body['top_k'], 20)
        self.assertEqual(body['temperature'], 1)
        self.assertEqual(body['top_p'], 0.95)


if __name__ == '__main__':
    unittest.main()
