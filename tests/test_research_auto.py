"""Real HTTP + actual jagent validator + detached Git commit + frozen evaluation."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

from test_research_bridge import BridgeFixture, b, r

SCRIPT = Path(__file__).resolve().parents[1] / 'scripts' / 'jsh_research_auto.py'
SPEC = importlib.util.spec_from_file_location('jsh_research_auto', SCRIPT)
a = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(a)


class ModelHandler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        raw = self.rfile.read(int(self.headers['Content-Length']))
        body = json.loads(raw)
        self.server.calls.append(body)
        self.server.auth.append(self.headers.get('Authorization'))
        if self.server.callback:
            self.server.callback()
        mode = self.server.mode
        if mode == 'delay':
            time.sleep(2)
        if mode == 'redirect':
            self.send_response(307)
            self.send_header('Location', '/must-not-follow')
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        data = json.loads(body['messages'][1]['content'])['untrusted_authoring_input']
        context, source = data['context'], data['files'][0]
        score = float(re.search(r'SCORE\s*=\s*([0-9.]+)', source['content'])[1])
        content = 'SCORE = %s\n' % (score - 1)
        if mode == 'fail_eval':
            content = "raise RuntimeError('candidate failure')\n"
        plan = {'schema_version': 1, 'campaign_id': context['campaign_id'], 'manifest_digest': context['manifest_digest'],
                'expected_revision': context['revision'], 'base_commit': context['best']['commit'], 'hypothesis': 'Reduce synthetic loss',
                'edits': [{'path': source['path'], 'before_blob': source['blob'], 'content': content}]}
        if mode == 'invalid':
            plan['edits'][0]['path'] = 'evaluate.py'
        if mode == 'noop':
            plan['edits'][0]['content'] = source['content']
        response = {'choices': [{'index': 0, 'finish_reason': 'length' if mode == 'length' else 'stop',
                                 'message': {'role': 'assistant', 'content': json.dumps(plan)}}]}
        encoded = b'x' * (a.MAX_RESPONSE + 1) if mode == 'oversize' else json.dumps(response).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(encoded)))
        self.end_headers()
        try:
            self.wfile.write(encoded)
        except (BrokenPipeError, ConnectionResetError):
            pass


class AutoFixture(BridgeFixture):
    def setUp(self):
        super().setUp()
        self.server = ThreadingHTTPServer(('127.0.0.1', 0), ModelHandler)
        self.server.calls, self.server.auth = [], []
        self.server.mode, self.server.callback = 'valid', None
        thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.config = {'schema_version': 1, 'endpoint': 'http://127.0.0.1:%d/v1/chat/completions' % self.server.server_port,
                       'model': 'test-flint', 'source_paths': ['candidate.py'], 'max_attempts': 5, 'max_output_tokens': 4096,
                       'max_tokens_per_call': 256, 'request_timeout_seconds': 5, 'max_seconds': 1000, 'max_failures': 2}
        self.loop = a.AutoLoop(r, b, self.store.root, os.environ.get('JSH_RESEARCH_BINARY'))

    def ready(self):
        self.baseline()
        self.loop.initialize(self.config)

    def native(self, *args):
        return subprocess.run([os.environ['JSH_RESEARCH_BINARY'], 'research', '--root', str(self.store.root), *args],
                              cwd=self.base, capture_output=True, text=True, timeout=30)


class AutoConfigTests(AutoFixture):
    def test_initialize_is_offline_and_never_resets_budget(self):
        self.ready()
        self.assertEqual(len(self.server.calls), 0)
        self.assertEqual(self.loop.state_path.stat().st_mode & 0o777, 0o600)
        with self.assertRaises(r.ResearchError):
            self.loop.initialize(self.config)

    def test_remote_http_credentials_and_ambiguous_endpoints_are_rejected(self):
        self.baseline()
        for endpoint in ('http://localhost:8080/v1/chat/completions', 'http://192.0.2.1/v1/chat/completions',
                         'https://example.com/v1/chat/completions', 'http://user:pass@127.0.0.1/v1/chat/completions',
                         'http://127.0.0.1/v1/chat/completions?key=secret', 'file:///tmp/model'):
            with self.subTest(endpoint=endpoint), self.assertRaises(r.ResearchError):
                self.loop.initialize({**self.config, 'endpoint': endpoint})
        self.assertEqual(len(self.server.calls), 0)

    def test_remote_https_needs_explicit_permission_but_init_never_calls_it(self):
        self.baseline()
        self.loop.initialize({**self.config, 'endpoint': 'https://example.com/v1/chat/completions'}, allow_remote=True)
        self.assertTrue(self.loop.load()['config']['allow_remote'])
        self.assertEqual(len(self.server.calls), 0)

    def test_sources_must_be_selected_regular_editable_files(self):
        self.baseline()
        for paths in ([], ['evaluate.py'], ['missing.py'], ['../candidate.py'], ['candidate.py', 'candidate.py']):
            with self.subTest(paths=paths), self.assertRaises((r.ResearchError, ValueError)):
                self.loop.initialize({**self.config, 'source_paths': paths})

    def test_explicit_execution_is_required(self):
        self.ready()
        with self.assertRaisesRegex(r.ResearchError, '--execute'):
            self.loop.run()
        self.assertEqual(len(self.server.calls), 0)
        self.assertEqual(len(self.loop.load()['attempts']), 0)

    def test_configuration_digest_detects_tampering(self):
        self.ready()
        state = self.loop.load()
        state['config']['max_attempts'] = 100
        r.atomic_json(self.loop.state_path, state)
        with self.assertRaises(r.ResearchError):
            self.loop.load()

    def test_crash_recovery_keeps_reservation_without_model_replay(self):
        self.ready()
        state = self.loop.load()
        state['attempts'].append({'id': 'interrupted-1', 'status': 'running', 'stage': 'model_request', 'charged_seconds': 133})
        state['charged_seconds'], state['reserved_output_tokens'] = 133, 256
        self.loop.save(state)
        with self.assertRaisesRegex(r.ResearchError, 'auto-recover'):
            self.loop.run(execute=True)
        recovered = self.loop.recover()
        self.assertEqual(recovered['charged_seconds'], 133)
        self.assertEqual(recovered['reserved_output_tokens'], 256)
        self.assertEqual(recovered['attempts'][0]['status'], 'interrupted')
        self.assertEqual(len(self.server.calls), 0)

    def test_recovery_refuses_live_groups(self):
        self.ready()
        state = self.loop.load()
        state['attempts'].append({'id': 'interrupted-1', 'status': 'running', 'active_process_group': os.getpgrp()})
        self.loop.save(state)
        with self.assertRaises(r.ResearchError):
            self.loop.recover()

    def test_expected_queue_identity_prevents_running_unrelated_work(self):
        self.baseline()
        self.store.enqueue(self.proposal(self.candidate()))
        with self.assertRaisesRegex(r.ResearchError, 'queue'):
            r.step(self.store, expected_trial_id='e000099')
        self.assertEqual(self.store.load()['trials'][-1]['status'], 'queued')


@unittest.skipUnless(os.environ.get('JSH_RESEARCH_BINARY'), 'requires compiled jsh binary')
class AutoNativeTests(AutoFixture):
    def test_two_model_rounds_create_real_commits_and_learn_from_new_best(self):
        self.ready()
        result = self.native('auto-run', '--execute', '--limit', '2')
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        report = json.loads(result.stdout)
        self.assertEqual([item['status'] for item in report['attempts']], ['kept', 'kept'])
        self.assertEqual(len(self.server.calls), 2)
        self.assertEqual(self.git('rev-parse', 'HEAD'), self.head)
        self.assertEqual((self.repo / 'candidate.py').read_text(), 'SCORE = 10\n')
        second = json.loads(self.server.calls[1]['messages'][1]['content'])['untrusted_authoring_input']
        self.assertEqual(second['files'][0]['content'], 'SCORE = 9.0\n')
        self.assertEqual(second['feedback'], ['kept'])
        self.assertEqual(report['reserved_output_tokens'], 512)
        self.assertEqual(len(self.store.load()['trials']), 3)
        self.assertNotEqual(report['attempts'][0]['candidate_commit'], report['attempts'][1]['candidate_commit'])

    def test_completion_budget_persists_across_invocations(self):
        self.config['max_output_tokens'] = 256
        self.ready()
        self.assertEqual(self.native('auto-run', '--execute', '--limit', '5').returncode, 0)
        result = self.native('auto-run', '--execute', '--limit', '5')
        self.assertEqual(result.returncode, 0)
        self.assertEqual(json.loads(result.stdout)['stop_reason'], 'auto_budget_exhausted')
        self.assertEqual(len(self.server.calls), 1)

    def test_invalid_output_hits_persistent_failure_ceiling_without_commits(self):
        self.server.mode = 'invalid'
        self.ready()
        result = self.native('auto-run', '--execute', '--limit', '5')
        self.assertEqual(result.returncode, 1, result.stderr + result.stdout)
        self.assertEqual(len(self.server.calls), 2)
        self.assertEqual(len(self.store.load()['trials']), 1)
        self.assertEqual(self.loop.load()['reserved_output_tokens'], 512)
        self.native('auto-run', '--execute', '--limit', '5')
        self.assertEqual(len(self.server.calls), 2)

    def test_redirect_timeout_oversize_noop_and_truncation_fail_closed(self):
        for mode in ('redirect', 'oversize', 'noop', 'length', 'delay'):
            with self.subTest(mode=mode):
                # Separate campaign/auto identity for each subtest.
                self.store = r.Store(self.base / ('campaign-' + mode))
                self.bridge = b.Bridge(r, self.store.root)
                self.loop = a.AutoLoop(r, b, self.store.root, os.environ['JSH_RESEARCH_BINARY'])
                self.config['request_timeout_seconds'] = 0.4 if mode == 'delay' else 5
                self.config['max_failures'] = 1
                self.server.mode = mode
                self.ready()
                result = self.native('auto-run', '--execute')
                self.assertEqual(result.returncode, 1, result.stderr + result.stdout)
                self.assertEqual(len(self.store.load()['trials']), 1)
                self.assertEqual(self.loop.load()['reserved_output_tokens'], 256)
        self.assertEqual(len(self.server.calls), 5)  # Redirects were not followed.

    def test_stale_response_never_rebinds_or_submits(self):
        self.ready()
        self.server.callback = self.bridge.resume
        result = self.native('auto-run', '--execute')
        self.assertEqual(result.returncode, 1)
        self.assertEqual(len(self.store.load()['trials']), 1)
        self.assertEqual(self.loop.load()['attempts'][0]['stage'], 'commit_candidate')

    def test_stop_during_model_request_interrupts_without_evaluation(self):
        self.ready()
        self.server.mode = 'delay'
        self.server.callback = self.store.stop
        result = self.native('auto-run', '--execute')
        self.assertEqual(result.returncode, 130, result.stderr + result.stdout)
        self.assertEqual(self.loop.load()['attempts'][0]['status'], 'interrupted')
        self.assertEqual(len(self.store.load()['trials']), 1)

    def test_generated_code_failure_is_not_mistaken_for_improvement(self):
        self.ready()
        self.server.mode = 'fail_eval'
        result = self.native('auto-run', '--execute')
        self.assertEqual(result.returncode, 1, result.stderr + result.stdout)
        self.assertEqual(self.loop.load()['attempts'][0]['status'], 'failed')
        self.assertEqual(self.store.load()['best_id'], 'baseline')

    def test_key_is_sent_only_in_header_and_never_archived(self):
        self.ready()
        secret = 'test-only-secret-DO-NOT-PERSIST-98371'
        with patch.dict(os.environ, {'JSH_RESEARCH_API_KEY': secret, 'HTTP_PROXY': 'http://192.0.2.1:1'}):
            result = self.native('auto-run', '--execute')
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertEqual(self.server.auth, ['Bearer ' + secret])
        for path in self.loop.directory.rglob('*.json'):
            self.assertNotIn(secret, path.read_text(), str(path))
        self.assertNotIn(secret, result.stdout + result.stderr)

    def test_json_object_fallback_still_uses_strict_native_validation(self):
        self.config['response_format'] = 'json_object'
        self.ready()
        self.assertEqual(self.native('auto-run', '--execute').returncode, 0)
        self.assertEqual(self.server.calls[0]['response_format'], {'type': 'json_object'})
        self.assertIn('required_response_schema', self.server.calls[0]['messages'][-1]['content'])

    def test_resume_after_completed_attempt_does_not_replay_http(self):
        self.ready()
        self.assertEqual(self.native('auto-run', '--execute').returncode, 0)
        state = self.loop.load()
        state['attempts'][0]['status'] = 'running'
        self.loop.save(state)
        recovered = self.loop.recover()
        self.assertEqual(recovered['attempts'][0]['status'], 'kept')
        self.assertEqual(len(self.server.calls), 1)


if __name__ == '__main__':
    unittest.main()
