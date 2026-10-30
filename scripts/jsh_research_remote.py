#!/usr/bin/env python3
"""Durable remote experiment staging. No automatic promotion or DLC SDK calls.

The HTTPS gateway is a trusted, replaceable executor; it must implement the v1
lease/idempotency contract. Candidate containers must never hold its signing key.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import hmac
import http.client
import json
import os
from pathlib import Path
import re
import signal
import ssl
import sys
import time
import types
import urllib.parse
import uuid

PROTOCOL = 'jsh.research.remote/v1'
MAX_RESPONSE = 256 * 1024
TERMINAL = {'succeeded', 'failed', 'cancelled', 'expired'}
STATES = {'pending', 'running'} | TERMINAL


def checked_int(r, value, low, high):
    r.require(type(value) is int and low <= value <= high, 'invalid remote integer')
    return value


def validate_config(r, value, allow_loopback=False):
    r.fields(value, {'endpoint', 'workspace', 'resource_pool', 'image_digest',
                     'gpus', 'lease_seconds', 'max_gpu_seconds', 'max_jobs',
                     'token_env', 'signing_key_env'}, set(), 'remote config')
    value = dict(value)
    for key in ('workspace', 'resource_pool', 'image_digest'):
        r.text(value[key], key, 512)
    r.require(re.fullmatch(r'.+@sha256:[0-9a-f]{64}', value['image_digest']), 'pin container digest')
    url = urllib.parse.urlsplit(value['endpoint'])
    r.require(url.hostname and not url.username and not url.password and not url.query and
              not url.fragment and url.path in ('', '/'), 'invalid gateway origin')
    loopback = url.hostname in ('127.0.0.1', '::1') and url.scheme == 'http'
    r.require(url.scheme == 'https' or (allow_loopback and loopback), 'HTTPS gateway required')
    value['endpoint'] = value['endpoint'].rstrip('/')
    for key in ('token_env', 'signing_key_env'):
        r.require(re.fullmatch(r'[A-Z_][A-Z0-9_]{0,127}', value[key]), 'invalid credential variable')
    checked_int(r, value['gpus'], 1, 1024)
    checked_int(r, value['lease_seconds'], 1, 86400)
    checked_int(r, value['max_gpu_seconds'], 1, 10**12)
    checked_int(r, value['max_jobs'], 1, 10000)
    r.require(value['gpus'] * value['lease_seconds'] <= value['max_gpu_seconds'], 'insufficient budget')
    return value


class Gateway:
    """Bounded signed JSON transport; no redirects, shell, or implicit retries."""
    def __init__(self, r, config):
        self.r, self.config = r, config

    def call(self, operation, request):
        r, c = self.r, self.config
        token = os.environ.get(c['token_env'], '')
        key = os.environ.get(c['signing_key_env'], '')
        r.require(bool(token) and len(key.encode()) >= 32, 'gateway credentials unavailable')
        nonce = uuid.uuid4().hex
        message = {'protocol': PROTOCOL, 'operation': operation, 'nonce': nonce, 'request': request}
        url = urllib.parse.urlsplit(c['endpoint'])
        conn_cls = http.client.HTTPSConnection if url.scheme == 'https' else http.client.HTTPConnection
        kwargs = {'timeout': 10}
        if url.scheme == 'https':
            kwargs['context'] = ssl.create_default_context()
        conn = conn_cls(url.hostname, url.port, **kwargs)
        # A response-wide monotonic bound also stops slow-drip bodies.
        deadline = time.monotonic() + 10
        try:
            conn.request('POST', '/v1/experiments/' + operation, r.canonical(message),
                         {'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
            response = conn.getresponse()
            r.require(response.status == 200, 'gateway request unsuccessful')
            chunks = bytearray()
            while True:
                remaining = deadline - time.monotonic()
                r.require(remaining > 0, 'gateway response timed out')
                if conn.sock:
                    conn.sock.settimeout(remaining)
                chunk = response.read1(min(65536, MAX_RESPONSE + 1 - len(chunks)))
                chunks.extend(chunk)
                r.require(len(chunks) <= MAX_RESPONSE, 'gateway response too large')
                if not chunk:
                    break
            signature = response.getheader('X-Jsh-Signature', '')
            expected = hmac.new(key.encode(), bytes(chunks), hashlib.sha256).hexdigest()
            r.require(hmac.compare_digest(signature, expected), 'invalid gateway signature')
            body = json.loads(chunks, object_pairs_hook=r.unique_object, parse_constant=r.invalid_constant)
            r.fields(body, {'protocol', 'nonce', 'request_digest', 'job_id', 'status', 'result'}, set(), 'remote reply')
            r.require(body['protocol'] == PROTOCOL and body['nonce'] == nonce, 'stale gateway response')
            r.require(body['request_digest'] == r.digest(request), 'foreign gateway response')
            r.require(body['status'] in STATES, 'unknown remote status')
            r.require(isinstance(body['job_id'], str) and re.fullmatch(r'[A-Za-z0-9_-]{1,128}', body['job_id']), 'invalid job ID')
            body['_attestation'] = {'body_base64': base64.b64encode(chunks).decode('ascii'), 'signature': signature}
            return body
        finally:
            conn.close()


class Remote:
    def __init__(self, r, bridge, root, transport=Gateway):
        self.r, self.bridge, self.store = r, bridge.Bridge(r, root), r.Store(root)
        self.path = self.store.root / 'remote-ledger.json'
        self.transport = transport

    def initialize(self, config, allow_loopback=False):
        r = self.r
        config = validate_config(r, config, allow_loopback)
        with self.store.lock():
            state = self.store.load()
            identity = self.bridge.identity(state, create=True)
            r.require(not os.path.lexists(self.path), 'remote config already frozen')
            ledger = {'protocol': PROTOCOL, 'campaign_id': identity['campaign_id'],
                      'manifest_digest': state['manifest_digest'], 'config': config,
                      'config_digest': r.digest(config), 'charged_gpu_seconds': 0, 'jobs': {}}
            r.atomic_json(self.path, ledger)
            return ledger

    def load(self):
        r = self.r
        ledger = r.read_json(self.path, r.MAX_STATE)
        r.fields(ledger, {'protocol', 'campaign_id', 'manifest_digest', 'config', 'config_digest',
                         'charged_gpu_seconds', 'jobs'}, set(), 'remote ledger')
        r.require(ledger['protocol'] == PROTOCOL and r.digest(ledger['config']) == ledger['config_digest'], 'remote ledger corrupted')
        state = self.store.load()
        identity = self.bridge.identity(state)
        r.require(ledger['campaign_id'] == identity['campaign_id'] and ledger['manifest_digest'] == state['manifest_digest'], 'foreign ledger')
        total = 0
        for task, entry in ledger['jobs'].items():
            req = entry['request']
            r.require(task == req['task_id'] and entry['request_digest'] == r.digest(req), 'corrupt task binding')
            r.require(req['campaign_id'] == ledger['campaign_id'] and req['manifest_digest'] == ledger['manifest_digest'] and
                      req['config_digest'] == ledger['config_digest'], 'foreign task')
            total += checked_int(r, entry['reserved_gpu_seconds'], 1, 10**12)
            r.require(entry['reserved_gpu_seconds'] == req['gpus'] * req['lease_seconds'], 'corrupt reservation')
        r.require(type(ledger['charged_gpu_seconds']) is int and total == ledger['charged_gpu_seconds'] <= ledger['config']['max_gpu_seconds'], 'corrupt GPU accounting')
        r.require(len(ledger['jobs']) <= ledger['config']['max_jobs'], 'corrupt job accounting')
        return ledger, state

    def submit(self, task, trial_id, stage, seed):
        r = self.r
        r.require(isinstance(task, str) and re.fullmatch(r'[a-zA-Z0-9_-]{1,64}', task), 'invalid task ID')
        with self.store.lock():
            ledger, state = self.load()
            if task in ledger['jobs']:
                entry = ledger['jobs'][task]
                req = entry['request']
                r.require((req['trial_id'], req['stage'], req['seed']) == (trial_id, stage, seed), 'task ID reused')
                # Exact local replay is read-only, including after a crash/timeout.
                return entry
            r.require(not self.store.stopped(), 'campaign stopped')
            r.require(stage in ('screen', 'confirm') and type(seed) is int and seed in state['manifest'][stage]['seeds'], 'not a frozen evaluation')
            trial = next((t for t in state['trials'] if t['id'] == trial_id), None)
            r.require(trial is not None and trial['status'] == 'queued', 'trial must be queued')
            r.check_candidate(state['manifest'], trial['commit'])
            c = ledger['config']
            reservation = c['gpus'] * c['lease_seconds']
            r.require(len(ledger['jobs']) < c['max_jobs'] and ledger['charged_gpu_seconds'] + reservation <= c['max_gpu_seconds'], 'remote budget exhausted')
            now = int(time.time())
            req = {'protocol': PROTOCOL, 'task_id': task, 'attempt_id': uuid.uuid4().hex,
                   'campaign_id': ledger['campaign_id'], 'manifest_digest': ledger['manifest_digest'],
                   'config_digest': ledger['config_digest'], 'revision': state['revision'],
                   'trial_id': trial_id, 'commit': trial['commit'], 'stage': stage, 'seed': seed,
                   'argv': state['manifest'][stage]['argv'], 'workspace': c['workspace'],
                   'resource_pool': c['resource_pool'], 'image_digest': c['image_digest'],
                   'gpus': c['gpus'], 'lease_seconds': c['lease_seconds'],
                   'created_at': now, 'expires_at': now + c['lease_seconds']}
            entry = {'request': req, 'request_digest': r.digest(req), 'reserved_gpu_seconds': reservation,
                     'status': 'submission_unknown', 'job_id': None, 'result': None, 'cancel_requested': False}
            ledger['charged_gpu_seconds'] += reservation
            ledger['jobs'][task] = entry
            r.atomic_json(self.path, ledger)  # durable intent BEFORE any possible side effect
            return self.exchange(ledger, entry, 'submit')

    def exchange(self, ledger, entry, operation):
        r, req = self.r, entry['request']
        try:
            body = self.transport(r, ledger['config']).call(operation, req)
            r.require(entry['job_id'] in (None, body['job_id']), 'remote job identity changed')
            r.require(entry['status'] not in TERMINAL or body['status'] == entry['status'], 'terminal status changed')
            r.require(entry['status'] != 'running' or body['status'] != 'pending', 'remote state regressed')
            # Cancellation is durable intent; late success must never become evidence.
            if body['status'] == 'succeeded':
                r.require(not entry['cancel_requested'] and int(time.time()) < req['expires_at'], 'expired or cancelled result')
                result = body['result']
                r.fields(result, {'request_digest', 'job_id', 'finished_at', 'metrics', 'metrics_sha256'}, set(), 'remote result')
                r.require(result['request_digest'] == entry['request_digest'] and result['job_id'] == body['job_id'], 'foreign result')
                checked_int(r, result['finished_at'], req['created_at'], min(req['expires_at'], int(time.time())))
                r.require(isinstance(result['metrics_sha256'], str) and re.fullmatch(r'[0-9a-f]{64}', result['metrics_sha256']), 'invalid artifact digest')
                r.require(result['metrics_sha256'] == r.digest({'valid': True, 'metrics': result['metrics']}), 'metrics digest mismatch')
                _, state = self.load()
                r.validate_metrics({'valid': True, 'metrics': result['metrics']}, state['manifest'])
            else:
                r.require(body['result'] is None, 'unexpected result')
            entry.update(status=body['status'], job_id=body['job_id'], result=body['result'],
                         attestation=body['_attestation'])
            entry.pop('last_error', None)
        except (OSError, ValueError, KeyError, TypeError, http.client.HTTPException, r.ResearchError):
            # Neither failed HTTP nor an invalid signature proves that the GPU stopped.
            entry['last_error'] = 'unverified_response; reconcile same identity, reservation retained'
        r.atomic_json(self.path, ledger)
        return entry

    def operate(self, operation, task):
        with self.store.lock():
            ledger, _ = self.load()
            self.r.require(task in ledger['jobs'], 'unknown task')
            entry = ledger['jobs'][task]
            if entry['status'] in TERMINAL:
                return entry
            if operation == 'cancel' or self.store.stopped() or int(time.time()) >= entry['request']['expires_at']:
                entry['cancel_requested'] = True
                self.r.atomic_json(self.path, ledger)
            if entry['cancel_requested']:
                operation = 'cancel'
            # recover is lookup/reconcile only. It NEVER invokes submit again.
            return self.exchange(ledger, entry, operation)


def main(argv, r, bridge):
    parser = argparse.ArgumentParser(prog='jsh research remote')
    parser.add_argument('--root', type=Path, default=Path('.jsh-research'))
    subs = parser.add_subparsers(dest='command', required=True)
    init = subs.add_parser('remote-init')
    init.add_argument('config', type=Path)
    init.add_argument('--allow-loopback-test', action='store_true')
    submit = subs.add_parser('remote-submit')
    submit.add_argument('task_id')
    submit.add_argument('trial_id')
    submit.add_argument('--stage', choices=['screen', 'confirm'], required=True)
    submit.add_argument('--seed', type=int, required=True)
    submit.add_argument('--execute', action='store_true')
    for name in ('status', 'result', 'cancel', 'recover'):
        sub = subs.add_parser('remote-' + name)
        sub.add_argument('task_id')
    subs.add_parser('remote-ledger')
    args = parser.parse_args(argv)
    try:
        remote = Remote(r, bridge, args.root)
        if args.command == 'remote-init':
            result = remote.initialize(r.read_json(args.config), args.allow_loopback_test)
        elif args.command == 'remote-submit':
            r.require(args.execute, 'remote-submit requires --execute')
            result = remote.submit(args.task_id, args.trial_id, args.stage, args.seed)
        elif args.command == 'remote-ledger':
            with remote.store.lock():
                result, _ = remote.load()
        else:
            result = remote.operate(args.command.removeprefix('remote-'), args.task_id)
        print(r.canonical(result).decode())
        return 1 if result.get('last_error') or result.get('status') in {'failed', 'expired', 'cancelled', 'submission_unknown'} else 0
    except (r.ResearchError, OSError, ValueError, KeyError, TypeError):
        print(json.dumps({'protocol': PROTOCOL, 'error': 'remote operation rejected'}), file=sys.stderr)
        return 2


if __name__ == '__main__':
    args = sys.argv[1:]
    if args[:1] == ['--embedded']:
        r, bridge = types.ModuleType('remote_engine'), types.ModuleType('remote_bridge')
        sys.modules[r.__name__], sys.modules[bridge.__name__] = r, bridge
        exec(compile(args[1], '<embedded-engine>', 'exec'), r.__dict__)
        exec(compile(args[2], '<embedded-bridge>', 'exec'), bridge.__dict__)
        args = args[3:]
    else:
        import jsh_research as r
        import jsh_research_bridge as bridge
    def terminated(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminated)
    try:
        sys.exit(main(args, r, bridge))
    except KeyboardInterrupt:
        sys.exit(130)
