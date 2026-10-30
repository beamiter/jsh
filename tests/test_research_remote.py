"""Real HTTP fault injection against a simulated DLC gateway, no GPUs used."""
import copy
import hashlib
import hmac
import importlib.util
import json
import os
from pathlib import Path
import socket
import sys
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

from test_research import Fixture, r
from test_research_bridge import b

SCRIPT = Path(__file__).resolve().parents[1] / 'scripts' / 'jsh_research_remote.py'
spec = importlib.util.spec_from_file_location('remote', SCRIPT)
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
KEY = 'mock-gateway-secret-key-not-for-production'


class MockDLC(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        s = self.server
        if self.headers.get('Authorization') != 'Bearer mock-token':
            self.send_error(401)
            return
        message = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        req, op = message['request'], message['operation']
        digest = r.digest(req)
        s.calls.append(op)
        if op == 'submit' and digest not in s.jobs:
            s.jobs[digest] = {'job_id': 'dlc-mock-' + str(len(s.jobs)), 'status': 'running'}
        job = s.jobs.get(digest)
        if not job:
            self.send_error(404)
            return
        if op == 'cancel':
            job['status'] = 'cancelled'
        if s.mode == 'drop':
            self.connection.shutdown(socket.SHUT_RDWR)
            self.connection.close()
            return
        result = None
        if job['status'] == 'succeeded':
            result = {'request_digest': digest, 'job_id': job['job_id'], 'finished_at': int(time.time()),
                      'metrics': {'loss': 8, 'violations': 0}, 'metrics_sha256': r.digest({'valid': True, 'metrics': {'loss': 8, 'violations': 0}})}
        body = {'protocol': m.PROTOCOL, 'nonce': message['nonce'], 'request_digest': digest,
                'job_id': job['job_id'], 'status': job['status'], 'result': result}
        if s.mode == 'stale':
            body['nonce'] = 'old-nonce'
        if s.mode == 'foreign':
            body['request_digest'] = 'b' * 64
        if s.mode == 'different_job':
            body['job_id'] = 'dlc-foreign'
        if s.mode == 'bad_metrics':
            body['result']['metrics']['violations'] = 1
            body['result']['metrics_sha256'] = r.digest({'valid': True, 'metrics': body['result']['metrics']})
        if s.mode == 'bad_digest':
            body['result']['metrics_sha256'] = '0' * 64
        if s.mode == 'foreign_result':
            body['result']['request_digest'] = 'b' * 64
        raw = r.canonical(body)
        signature = hmac.new(KEY.encode(), raw, hashlib.sha256).hexdigest()
        if s.mode == 'forged':
            signature = '0' * 64
        self.send_response(200)
        self.send_header('X-Jsh-Signature', signature)
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


class RemoteTests(Fixture):
    def setUp(self):
        super().setUp()
        self.init()
        self.trial = self.store.load()['trials'][0]['id']
        self.server = ThreadingHTTPServer(('127.0.0.1', 0), MockDLC)
        self.server.jobs, self.server.calls, self.server.mode = {}, [], ''
        thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        thread.start()
        def close():
            self.server.shutdown()
            self.server.server_close()
            thread.join()
        self.addCleanup(close)
        env = patch.dict(os.environ, MOCK_TOKEN='mock-token', MOCK_KEY=KEY)
        env.start()
        self.addCleanup(env.stop)
        self.config = {'endpoint': f'http://127.0.0.1:{self.server.server_port}',
                       'workspace': 'ws-mock', 'resource_pool': 'pool-mock',
                       'image_digest': 'mock/image@sha256:' + 'c' * 64, 'gpus': 8,
                       'lease_seconds': 60, 'max_gpu_seconds': 960, 'max_jobs': 2,
                       'token_env': 'MOCK_TOKEN', 'signing_key_env': 'MOCK_KEY'}
        self.remote = m.Remote(r, b, self.store.root)
        self.remote.initialize(self.config, allow_loopback=True)

    def submit(self, task='task-1', seed=1):
        return self.remote.submit(task, self.trial, 'screen', seed)

    def succeed(self):
        for job in self.server.jobs.values():
            job['status'] = 'succeeded'

    def test_submit_status_result_and_local_state_unchanged(self):
        original = self.store.load()
        self.assertEqual(self.submit()['status'], 'running')
        self.assertEqual(self.remote.operate('status', 'task-1')['status'], 'running')
        self.succeed()
        result = self.remote.operate('result', 'task-1')
        self.assertEqual(result['status'], 'succeeded')
        self.assertEqual(result['result']['metrics']['loss'], 8)
        self.assertEqual(self.store.load(), original)
        self.assertNotIn(KEY, self.remote.path.read_text())
        self.assertNotIn('mock-token', self.remote.path.read_text())

    def test_duplicate_submit_no_new_network_or_reservation(self):
        first = self.submit()
        self.assertEqual(first, self.submit())
        self.assertEqual(self.server.calls, ['submit'])
        self.assertEqual(self.remote.load()[0]['charged_gpu_seconds'], 480)
        with self.assertRaises(r.ResearchError):
            self.submit(seed=2)

    def test_response_lost_after_creation_recovers_same_job(self):
        self.server.mode = 'drop'
        result = self.submit()
        self.assertEqual(result['status'], 'submission_unknown')
        self.assertEqual(len(self.server.jobs), 1)
        self.remote = m.Remote(r, b, self.store.root)  # process restart
        self.assertEqual(self.submit(), result)
        self.server.mode = ''
        recovered = self.remote.operate('recover', 'task-1')
        self.assertEqual(recovered['status'], 'running')
        self.assertEqual(self.server.calls, ['submit', 'recover'])
        self.assertEqual(self.remote.load()[0]['charged_gpu_seconds'], 480)

    def test_connection_failure_before_submit_never_blindly_resubmits(self):
        with patch.object(m.Gateway, 'call', side_effect=ConnectionError):
            self.assertEqual(self.submit()['status'], 'submission_unknown')
        result = self.remote.operate('recover', 'task-1')
        self.assertIn('last_error', result)
        self.assertFalse(self.server.jobs)
        self.assertEqual(self.remote.load()[0]['charged_gpu_seconds'], 480)

    def test_failures_retain_full_reservation(self):
        self.submit()
        next(iter(self.server.jobs.values()))['status'] = 'failed'
        self.assertEqual(self.remote.operate('status', 'task-1')['status'], 'failed')
        self.assertEqual(self.remote.load()[0]['charged_gpu_seconds'], 480)
        calls = len(self.server.calls)
        self.remote.operate('recover', 'task-1')
        self.assertEqual(len(self.server.calls), calls)

    def test_budget_exhaustion_does_not_call_gateway(self):
        self.submit()
        self.submit('task-2')
        with self.assertRaises(r.ResearchError):
            self.submit('task-3')
        self.assertEqual(len(self.server.jobs), 2)
        self.assertEqual(self.remote.load()[0]['charged_gpu_seconds'], 960)

    def test_forged_stale_foreign_and_rebound_status_rejected(self):
        self.submit()
        for mode in ('forged', 'stale', 'foreign', 'different_job'):
            with self.subTest(mode=mode):
                self.server.mode = mode
                result = self.remote.operate('status', 'task-1')
                self.assertIn('last_error', result)
                self.assertEqual(result['status'], 'running')

    def test_invalid_metrics_and_foreign_result_rejected(self):
        self.submit()
        self.succeed()
        for mode in ('bad_metrics', 'foreign_result', 'bad_digest'):
            self.server.mode = mode
            result = self.remote.operate('result', 'task-1')
            self.assertIn('last_error', result)
            self.assertIsNone(result['result'])

    def test_expiry_requests_cancellation_and_rejects_late_success(self):
        entry = self.submit()
        self.succeed()
        with patch.object(m.time, 'time', return_value=entry['request']['expires_at'] + 1):
            result = self.remote.operate('result', 'task-1')
        self.assertEqual(result['status'], 'cancelled')
        self.assertTrue(result['cancel_requested'])
        self.assertIsNone(result['result'])
        self.assertEqual(self.server.calls[-1], 'cancel')

    def test_cancel_failure_recovery_keeps_cancel_intent(self):
        self.submit()
        self.server.mode = 'drop'
        result = self.remote.operate('cancel', 'task-1')
        self.assertTrue(result['cancel_requested'])
        self.server.mode = ''
        self.assertEqual(self.remote.operate('recover', 'task-1')['status'], 'cancelled')
        self.assertEqual(self.server.calls[-2:], ['cancel', 'cancel'])

    def test_stop_blocks_new_submit_and_cancels_existing(self):
        self.submit()
        self.store.stop()
        with self.assertRaises(r.ResearchError):
            self.submit('task-2')
        self.assertEqual(self.remote.operate('recover', 'task-1')['status'], 'cancelled')

    def test_config_and_ledger_fail_closed(self):
        with self.assertRaises(r.ResearchError):
            m.validate_config(r, self.config)
        for key, value in [('gpus', True), ('image_digest', 'latest'), ('max_gpu_seconds', 1)]:
            config = dict(self.config, **{key: value})
            with self.assertRaises(r.ResearchError):
                m.validate_config(r, config, True)
        self.submit()
        ledger, _ = self.remote.load()
        ledger['charged_gpu_seconds'] = 0
        r.atomic_json(self.remote.path, ledger)
        with self.assertRaises(r.ResearchError):
            self.remote.load()

    def test_concurrent_writer_cannot_submit(self):
        with self.store.lock():
            with self.assertRaises(r.ResearchError):
                self.submit()
        self.assertFalse(self.server.jobs)

    def test_crash_after_persisted_intent_retains_reservation(self):
        with patch.object(m.Gateway, 'call', side_effect=KeyboardInterrupt):
            with self.assertRaises(KeyboardInterrupt):
                self.submit()
        ledger, _ = self.remote.load()
        self.assertEqual(ledger['charged_gpu_seconds'], 480)
        self.assertEqual(ledger['jobs']['task-1']['status'], 'submission_unknown')
        self.assertFalse(self.server.jobs)

    def test_late_success_is_rejected_without_receipt(self):
        self.submit()
        self.succeed()
        ledger, _ = self.remote.load()
        entry = ledger['jobs']['task-1']
        with patch.object(m.time, 'time', return_value=entry['request']['expires_at'] + 1):
            result = self.remote.exchange(ledger, entry, 'result')
        self.assertIn('last_error', result)
        self.assertIsNone(result['result'])

    def test_embedded_entrypoint_matches_source(self):
        import subprocess
        args = [sys.executable, '-I', '-c', SCRIPT.read_text(), '--embedded',
                SCRIPT.with_name('jsh_research.py').read_text(),
                SCRIPT.with_name('jsh_research_bridge.py').read_text(),
                '--root', str(self.store.root), 'remote-ledger']
        if os.environ.get('JSH_RESEARCH_BINARY'):
            args = [os.environ['JSH_RESEARCH_BINARY'], 'research',
                    '--root=' + str(self.store.root), 'remote-ledger']
        result = subprocess.run(args, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)['protocol'], m.PROTOCOL)


if __name__ == '__main__':
    unittest.main()
