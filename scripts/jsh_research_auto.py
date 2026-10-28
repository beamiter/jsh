#!/usr/bin/env python3
"""Bounded model-driven research loop; explicit opt-in, local executor only.

Model output is data validated by jagent, never a shell command. Worktrees are
not a sandbox: generated code requires an externally isolated environment.
"""
from __future__ import annotations

import argparse
import ipaddress
import json
import math
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import tempfile
import time
import types
import uuid
from urllib.parse import urlsplit

PROTOCOL = "jsh.research.auto/v1"
MAX_RESPONSE = 1024 * 1024
MAX_INPUT = 512 * 1024
SUCCESS = {'kept', 'archived', 'screened_out'}
STATUSES = SUCCESS | {'running', 'invalid', 'failed', 'interrupted', 'author_failed'}
STAGES = {'prepare', 'model_request', 'validate_plan', 'commit_candidate', 'submit',
          'evaluate', 'finished', 'recovered_without_replay'}
FAILURE_CODES = {'prepare': 'preparation_failed', 'model_request': 'transport_failed',
                 'validate_plan': 'plan_rejected', 'commit_candidate': 'candidate_rejected',
                 'submit': 'admission_rejected', 'evaluate': 'evaluation_unresolved'}
DEFAULT_SAMPLING = {'temperature': 1.0, 'top_p': 0.95, 'top_k': 20}
HTTP_CLIENT = r'''
import json, os, sys, urllib.request
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None
try:
    with open(sys.argv[1], 'rb') as stream:
        spec = json.loads(stream.read(2 * 1024 * 1024 + 1))
    headers = {'Content-Type': 'application/json', 'Accept': 'application/json'}
    key = os.environ.get(spec['key_env'], '')
    if key:
        if len(key) > 8192 or any(ord(ch) < 33 or ord(ch) > 126 for ch in key): sys.exit(65)
        headers['Authorization'] = 'Bearer ' + key
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    request = urllib.request.Request(spec['endpoint'], data=json.dumps(spec['body'], ensure_ascii=False).encode(), headers=headers, method='POST')
    with opener.open(request, timeout=spec['timeout']) as response:
        if response.status != 200: sys.exit(65)
        data = response.read(1024 * 1024 + 1)
    if len(data) > 1024 * 1024: sys.exit(65)
    fd = os.open(sys.argv[2], os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'wb') as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())
except Exception:
    # Never log response bodies, URLs, headers, environment or credentials.
    sys.exit(65)
'''


def load_module(name, source):
    module = types.ModuleType(name)
    sys.modules[name] = module
    exec(compile(source, '<embedded-' + name + '>', 'exec'), module.__dict__)
    return module


def diagnostic_code(attempt):
    """Only fixed categories, never persisted/provider exception strings."""
    status = attempt['status']
    if status in SUCCESS or status == 'running':
        return None
    if status in ('failed', 'invalid'):
        return 'evaluation_' + status
    if status == 'interrupted':
        return 'interrupted'
    return FAILURE_CODES.get(attempt.get('stage'), 'legacy_unknown')


def streaks(attempts, best_commit=None):
    failures, no_improvement = 0, 0
    for attempt in attempts:
        status = attempt['status']
        if status == 'running':
            continue
        failures = 0 if status in SUCCESS else failures + 1
        if status == 'kept':
            no_improvement = 0
        elif status in ('archived', 'screened_out'):
            # A new incumbent installed by a human also starts a new plateau.
            if best_commit is None or attempt.get('base_commit') == best_commit:
                no_improvement += 1
            else:
                no_improvement = 0
    return failures, no_improvement


class AutoLoop:
    def __init__(self, engine, bridge_module, root, native_binary):
        self.r, self.b = engine, bridge_module
        self.bridge = bridge_module.Bridge(engine, root)
        self.campaign = self.bridge.store
        self.directory = self.campaign.root / 'autoloop'
        self.lock_store = engine.Store(self.directory)
        self.state_path = self.directory / 'auto.json'
        self.binary = str(Path(native_binary).absolute()) if native_binary else None

    def view(self):
        # Unlike bridge.snapshot(), inspection must never recreate an identity.
        campaign = self.campaign.load()
        return campaign, self.bridge.context(campaign, self.bridge.identity(campaign))

    def load(self):
        r = self.r
        value = r.read_json(self.state_path, r.MAX_STATE)
        r.fields(value, {'protocol', 'loop_id', 'campaign_id', 'manifest_digest', 'config', 'config_digest',
                         'charged_seconds', 'reserved_output_tokens', 'failure_streak', 'attempts'}, set(), 'auto state')
        r.require(value['protocol'] == PROTOCOL and r.digest(value['config']) == value['config_digest'], 'invalid frozen auto configuration')
        r.require(isinstance(value['attempts'], list) and len(value['attempts']) <= value['config']['max_attempts'], 'invalid attempt accounting')
        r.number(value['charged_seconds'], 'charged_seconds', 0)
        for field in ('reserved_output_tokens', 'failure_streak'):
            r.require(type(value[field]) is int and value[field] >= 0, 'invalid auto budget accounting')
        r.require(value['reserved_output_tokens'] == len(value['attempts']) * value['config']['max_tokens_per_call'],
                  'output-token reservations do not match attempt count')
        r.require(value['reserved_output_tokens'] <= value['config']['max_output_tokens'], 'output-token budget exceeded')
        r.require(value['failure_streak'] <= len(value['attempts']), 'invalid consecutive failure count')
        patience = value['config'].get('max_no_improvement', 0)
        r.require(type(patience) is int and 0 <= patience <= 1000, 'invalid no-improvement limit')
        ids, charges = set(), []
        for attempt in value['attempts']:
            r.require(isinstance(attempt, dict) and attempt.get('status') in STATUSES, 'invalid attempt status')
            identifier = r.text(attempt.get('id'), 'attempt ID', 128)
            r.require(identifier not in ids, 'duplicate attempt ID')
            ids.add(identifier)
            r.require(attempt.get('stage', 'prepare') in STAGES, 'invalid attempt stage')
            charges.append(r.number(attempt.get('charged_seconds', 0), 'attempt charge', 0))
            if 'failure_code' in attempt:
                r.require(attempt['failure_code'] == diagnostic_code(attempt), 'inconsistent failure category')
        r.require(math.isclose(math.fsum(charges), value['charged_seconds'], rel_tol=1e-9, abs_tol=1e-6),
                  'time charges do not match attempt ledger')
        _, context = self.view()
        r.require(context['campaign_id'] == value['campaign_id'] and context['manifest_digest'] == value['manifest_digest'], 'auto loop belongs to another campaign')
        return value

    def save(self, state):
        self.r.atomic_json(self.state_path, state)

    def sampling(self, raw):
        r = self.r
        r.fields(raw, set(), {'temperature', 'top_p', 'top_k', 'seed'}, 'sampling')
        sampling = {**DEFAULT_SAMPLING, **raw}
        for key, maximum in (('temperature', 2.0), ('top_p', 1.0)):
            if sampling[key] is None:
                continue
            sampling[key] = r.number(sampling[key], 'sampling.' + key, 0)
            r.require(sampling[key] <= maximum and (key != 'top_p' or sampling[key] > 0),
                      'invalid sampling.' + key)
        top_k, seed = sampling['top_k'], sampling.get('seed')
        r.require(top_k is None or (type(top_k) is int and 1 <= top_k <= 1000000), 'invalid sampling.top_k')
        r.require(seed is None or (type(seed) is int and 0 <= seed < 2**32), 'invalid sampling.seed')
        return sampling

    def initialize(self, raw, allow_remote=False):
        r = self.r
        r.fields(raw, {'schema_version', 'endpoint', 'model', 'source_paths', 'max_attempts', 'max_output_tokens',
                       'max_tokens_per_call', 'request_timeout_seconds', 'max_seconds', 'max_failures'},
                 {'api_key_env', 'response_format', 'max_no_improvement', 'sampling', 'token_limit_field'}, 'auto config')
        r.require(type(raw['schema_version']) is int and raw['schema_version'] == 1, 'unsupported auto config version')
        endpoint = r.text(raw['endpoint'], 'endpoint', 2048)
        r.require(not any(ord(ch) <= 32 or ord(ch) == 127 for ch in endpoint), 'invalid endpoint characters')
        parsed = urlsplit(endpoint)
        r.require(parsed.scheme in ('http', 'https') and parsed.hostname and not parsed.username and not parsed.password
                  and not parsed.query and not parsed.fragment and parsed.path == '/v1/chat/completions',
                  'endpoint must be an explicit /v1/chat/completions URL without credentials/query/fragment')
        try:
            local = ipaddress.ip_address(parsed.hostname).is_loopback
        except ValueError:
            local = False
        r.require(local or (allow_remote and parsed.scheme == 'https'),
                  'use a literal loopback IP; remote HTTPS requires explicit --allow-remote at initialization')
        r.require(parsed.port is None or 1 <= parsed.port <= 65535, 'invalid endpoint port')
        model = r.text(raw['model'], 'model', 256)
        r.require(model.strip() and not any(ord(ch) < 32 for ch in model), 'invalid model name')
        sampling = self.sampling(raw.get('sampling', {}))
        token_limit_field = raw.get('token_limit_field', 'max_tokens')
        r.require(token_limit_field in ('max_tokens', 'max_completion_tokens'), 'invalid token_limit_field')
        paths = raw['source_paths']
        r.require(isinstance(paths, list) and 1 <= len(paths) <= 8, 'choose 1..8 tracked UTF-8 source files')
        paths = [r.path_prefix(path) for path in paths]
        r.require(len(set(paths)) == len(paths), 'duplicate source paths')
        limits = {}
        for key, minimum, maximum in (('max_attempts', 1, 1000), ('max_tokens_per_call', 1, 32768),
                                      ('max_output_tokens', 1, 100000000), ('max_failures', 1, 20)):
            value = raw[key]
            r.require(type(value) is int and minimum <= value <= maximum, 'invalid ' + key)
            limits[key] = value
        patience = raw.get('max_no_improvement', 0)
        r.require(type(patience) is int and 0 <= patience <= 1000, 'invalid max_no_improvement')
        limits['max_no_improvement'] = patience
        r.require(limits['max_output_tokens'] >= limits['max_tokens_per_call'], 'output-token budget cannot cover one call')
        limits['request_timeout_seconds'] = r.number(raw['request_timeout_seconds'], 'request_timeout_seconds', 0.1)
        r.require(limits['request_timeout_seconds'] <= 3600, 'request timeout exceeds one hour')
        limits['max_seconds'] = r.number(raw['max_seconds'], 'max_seconds', 0.1)
        key_env = raw.get('api_key_env', 'JSH_RESEARCH_API_KEY')
        r.require(isinstance(key_env, str) and re.fullmatch(r'[A-Z][A-Z0-9_]{0,63}', key_env), 'invalid API-key environment variable name')
        response_format = raw.get('response_format', 'json_schema')
        r.require(response_format in ('json_schema', 'json_object'), 'unsupported response format')
        context = self.bridge.snapshot()
        r.require(context['next_action'] == 'needs_proposal', 'run a valid baseline and clear pending work before auto-init')
        config = {'schema_version': 1, 'endpoint': endpoint, 'model': model, 'source_paths': paths,
                  'allow_remote': bool(allow_remote), 'api_key_env': key_env, 'response_format': response_format,
                  'sampling': sampling, 'token_limit_field': token_limit_field, **limits}
        r.require(self.reservation(config, context) <= config['max_seconds'], 'time budget cannot cover one reserved attempt')
        self.sources(context, config)
        with self.lock_store.lock(create=True):
            r.require(not os.path.lexists(self.state_path), 'auto loop already initialized; never resets budgets')
            self.check_live(context)
            state = {'protocol': PROTOCOL, 'loop_id': uuid.uuid4().hex, 'campaign_id': context['campaign_id'],
                     'manifest_digest': context['manifest_digest'], 'config': config, 'config_digest': r.digest(config),
                     'charged_seconds': 0.0, 'reserved_output_tokens': 0, 'failure_streak': 0, 'attempts': []}
            self.save(state)
            return self.summary(state)

    @staticmethod
    def reservation(config, context):
        return config['request_timeout_seconds'] + context['budget']['trial_reservation_seconds'] + 120.0

    def summary(self, state=None):
        state = state or self.load()
        return {'protocol': PROTOCOL, 'loop_id': state['loop_id'], 'campaign_id': state['campaign_id'],
                'charged_seconds': state['charged_seconds'], 'reserved_output_tokens': state['reserved_output_tokens'],
                'failure_streak': state['failure_streak'], 'stopped': self.campaign.stopped(),
                'limits': {key: state['config'][key] for key in ('max_attempts', 'max_output_tokens', 'max_seconds', 'max_failures')},
                'attempts': state['attempts']}

    def decision(self, state, campaign, context):
        """One admission policy shared by execution, report and offline preflight."""
        if context['next_action'] == 'stopped':
            return 'stopped'
        if any(item['status'] == 'running' for item in state['attempts']):
            return 'needs_recovery'
        if context['next_action'] != 'needs_proposal':
            return context['next_action']
        trials = {item['request_id']: item for item in campaign['trials']}
        for item in state['attempts']:
            trial = trials.get(item['id'])
            if item['status'] in ('interrupted', 'author_failed') and trial and trial['status'] in self.r.TERMINAL:
                if item.get('trial_id') != trial['id'] or item['status'] != trial['status']:
                    return 'needs_reconciliation'
        config = state['config']
        if (len(state['attempts']) >= config['max_attempts'] or
            state['reserved_output_tokens'] + config['max_tokens_per_call'] > config['max_output_tokens'] or
            state['charged_seconds'] + self.reservation(config, context) > config['max_seconds']):
            return 'auto_budget_exhausted'
        failures, plateau = streaks(state['attempts'], context['best']['commit'])
        if max(failures, state['failure_streak']) >= config['max_failures']:
            return 'failure_limit'
        if config.get('max_no_improvement', 0) and plateau >= config['max_no_improvement']:
            return 'no_improvement_limit'
        return 'needs_proposal'

    def scores(self, trial, manifest):
        return {'screen': None, 'confirm': None, **self.r.stage_scores(trial, manifest)}

    def comparison(self, trial, trials, manifest):
        """Descriptive paired-seed evidence against the actual trial incumbent."""
        if trial is None or trial['status'] not in self.r.TERMINAL or trial.get('compared_to') is None:
            return None
        reference = trials.get(trial['compared_to'])
        self.r.require(reference is not None and reference['id'] != trial['id'] and
                       reference['status'] in ('baseline', 'kept'), 'invalid comparison incumbent')
        scores, reference_scores = self.scores(trial, manifest), self.scores(reference, manifest)
        metric = manifest['objective']['metric']
        sign = 1 if manifest['objective']['direction'] == 'max' else -1
        stages = {}
        for stage in ('screen', 'confirm'):
            if scores[stage] is None or reference_scores[stage] is None:
                continue
            deltas = [sign * (item['metrics'][metric] - base['metrics'][metric])
                      for item, base in zip(trial['results'][stage], reference['results'][stage])]
            improvement = sign * (scores[stage] - reference_scores[stage])
            stages[stage] = {'samples': len(deltas), 'candidate_mean': scores[stage],
                             'reference_mean': reference_scores[stage], 'mean_improvement': improvement,
                             'min_improvement': min(deltas), 'max_improvement': max(deltas),
                             'improved_seeds': sum(delta > 0 for delta in deltas),
                             'meets_min_delta': improvement > manifest['objective']['min_delta']}
        return {'reference': {'id': reference['id'], 'commit': reference['commit']}, 'stages': stages}

    def observations(self, state, campaign, recent=4, include_running=False):
        trials = {item['request_id']: item for item in campaign['trials']}
        indexed = [(i, item) for i, item in enumerate(state['attempts'], 1)
                   if include_running or item['status'] != 'running']
        observations = []
        for index, item in indexed[-recent:]:
            trial = trials.get(item['id'])
            if trial is not None:
                self.r.require(item.get('candidate_commit', trial['commit']) == trial['commit'] and
                               item.get('trial_id', trial['id']) == trial['id'], 'attempt/trial identity mismatch')
            if item['status'] in SUCCESS | {'failed', 'invalid'}:
                self.r.require(trial is not None and trial['status'] == item['status'], 'outcome requires campaign evidence')
            observations.append({'attempt': index, 'status': item['status'], 'stage': item.get('stage', 'prepare'),
                                 'failure_code': diagnostic_code(item), 'trial_id': trial['id'] if trial else None,
                                 'scores': self.scores(trial, campaign['manifest'])})
        return observations

    def author_input(self, state, context, files, campaign):
        value = {'schema_version': 2, 'context': context, 'files': files,
                 'feedback': self.observations(state, campaign)}
        self.r.require(len(self.r.canonical(value)) + 1 <= MAX_INPUT, 'author input is too large')
        return value

    def report(self, recent=16):
        self.r.require(type(recent) is int and 1 <= recent <= 64, 'recent must be 1..64')
        state = self.load()
        campaign, context = self.view()
        config = state['config']
        counts = {status: 0 for status in sorted(STATUSES)}
        for item in state['attempts']:
            counts[item['status']] += 1
        best = next((item for item in campaign['trials'] if item['id'] == campaign['best_id']), None)
        trials = {item['id']: item for item in campaign['trials']}
        attempts = self.observations(state, campaign, recent, include_running=True)
        by_request = {item['request_id']: item for item in campaign['trials']}
        for item in attempts:
            trial = by_request.get(state['attempts'][item['attempt'] - 1]['id'])
            item['comparison'] = self.comparison(trial, trials, campaign['manifest'])
        failures, plateau = streaks(state['attempts'], best['commit'] if best else None)
        action = self.decision(state, campaign, context)
        return {'protocol': 'jsh.research.auto.report/v1', 'campaign_id': state['campaign_id'],
                'loop_id': state['loop_id'], 'revision': context['revision'], 'snapshot_only': True,
                'next_action': action, 'can_run': action == 'needs_proposal', 'counts': counts,
                'failure_streak': max(failures, state['failure_streak']), 'no_improvement_streak': plateau,
                'max_no_improvement': config.get('max_no_improvement', 0),
                'objective': context['objective'],
                'best': {**context['best'], 'scores': self.scores(best, campaign['manifest'])} if best else None,
                'remaining': {'attempts': config['max_attempts'] - len(state['attempts']),
                              'reserved_output_tokens': config['max_output_tokens'] - state['reserved_output_tokens'],
                              'controller_seconds': max(0.0, config['max_seconds'] - state['charged_seconds']),
                              'campaign_trials': context['budget']['max_trials'] - context['budget']['trials_used']},
                'next_attempt_reservation_seconds': self.reservation(config, context),
                'recent_attempts': attempts,
                'omitted_attempts': max(0, len(state['attempts']) - recent)}

    def preflight(self):
        state = self.load()
        campaign, context = self.view()
        result = {'protocol': 'jsh.research.auto.preflight/v1', 'revision': context['revision'],
                  'ready': False, 'network_checked': False, 'execution_authorized': False,
                  'sandbox_verified': False, 'reason': self.decision(state, campaign, context)}
        if result['reason'] != 'needs_proposal':
            return result
        stage = 'sources'
        try:
            files = self.sources(context, state['config'])
            stage = 'validator'
            # Private temporary scratch only; no campaign writes or reservations.
            with tempfile.TemporaryDirectory(prefix='jsh-research-preflight-') as temporary:
                path = Path(temporary) / 'input.json'
                self.r.atomic_json(path, self.author_input(state, context, files, campaign))
                self.native('author-prepare', path)
            stage = 'freshness'
            self.check_live(context)
            result.update(ready=True, reason='ready_offline', source_files=len(files),
                          source_bytes=sum(len(item['content'].encode('utf-8')) for item in files))
        except (self.r.ResearchError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
            result['reason'] = stage + '_check_failed'
        return result

    def sources(self, context, config):
        r = self.r
        manifest = self.campaign.load()['manifest']
        repo = Path(manifest['repo'])
        base = context['best']['commit']
        files, total = [], 0
        for path in config['source_paths']:
            r.require(any(r.within(path, prefix) for prefix in manifest['editable_paths']) and
                      not any(r.within(path, prefix) for prefix in manifest['protected_paths']), 'source is not editable')
        # ls-tree -l supplies object sizes before any blob is read. Batch both
        # metadata and content instead of launching three Git processes per file.
        listing = r.git(repo, '--literal-pathspecs', 'ls-tree', '-l', '-z', base, '--', *config['source_paths'])
        entries = {}
        for record in listing.split('\0'):
            if not record:
                continue
            header, path = record.split('\t', 1)
            mode, kind, blob, size_text = header.split()
            r.require(path in config['source_paths'] and path not in entries and
                      mode in ('100644', '100755') and kind == 'blob',
                      'source must be a regular tracked file, not a link/submodule')
            size = int(size_text)
            total += size
            r.require(size >= 0 and total <= 65536, 'selected source exceeds 64 KiB')
            entries[path] = (blob, size)
        r.require(set(entries) == set(config['source_paths']), 'source must name exactly one tracked file')
        queries = ''.join(entries[path][0] + '\n' for path in config['source_paths']).encode('ascii')
        content = subprocess.run(['git', '-C', str(repo), 'cat-file', '--batch'], input=queries,
                                 stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=15, check=True).stdout
        offset = 0
        for path in config['source_paths']:
            blob, size = entries[path]
            header = (blob + ' blob ' + str(size) + '\n').encode('ascii')
            r.require(content[offset:offset + len(header)] == header, 'source batch object mismatch')
            offset += len(header)
            data = content[offset:offset + size]
            offset += size
            r.require(len(data) == size and b'\0' not in data and content[offset:offset + 1] == b'\n', 'invalid source bytes')
            offset += 1
            files.append({'path': path, 'blob': blob, 'content': data.decode('utf-8', 'strict')})
        r.require(offset == len(content), 'unexpected source batch content')
        return files

    def native(self, command, *paths):
        self.r.require(self.binary is not None, 'compiled jsh binary required for shared jagent validation')
        result = subprocess.run([self.binary, 'research', command, *map(str, paths)],
                                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                timeout=20, check=False)
        self.r.require(result.returncode == 0 and len(result.stdout) <= 2 * MAX_INPUT, 'jagent authoring validation failed')
        return json.loads(result.stdout, object_pairs_hook=self.r.unique_object, parse_constant=self.r.invalid_constant)

    def check_live(self, context):
        if self.campaign.stopped():
            raise self.r.Stopped('campaign stopped')
        _, live = self.view()
        self.r.require(live['next_action'] == 'needs_proposal' and all(live[key] == context[key]
                       for key in ('campaign_id', 'manifest_digest', 'revision', 'best')), 'research context changed during authoring')

    def model_call(self, state, attempt, directory, prepared):
        r, config = self.r, state['config']
        token_limit_field = config.get('token_limit_field', 'max_tokens')
        r.require(token_limit_field in ('max_tokens', 'max_completion_tokens'), 'invalid token_limit_field')
        body = {'model': config['model'], 'messages': prepared['messages'], 'stream': False, 'n': 1,
                token_limit_field: config['max_tokens_per_call'],
                **{key: value for key, value in self.sampling(config.get('sampling', {})).items() if value is not None}}
        if config['response_format'] == 'json_schema':
            body['response_format'] = {'type': 'json_schema', 'json_schema': {
                'name': 'research_edit', 'strict': True, 'schema': prepared['response_schema']}}
        else:
            body['response_format'] = {'type': 'json_object'}
            body['messages'] = body['messages'] + [{'role': 'user', 'content': json.dumps({'required_response_schema': prepared['response_schema']})}]
        spec = {'endpoint': config['endpoint'], 'key_env': config['api_key_env'],
                'timeout': config['request_timeout_seconds'], 'body': body}
        spec_path, response_path = directory / 'http-request.json', directory / 'http-response.json'
        r.atomic_json(spec_path, spec)
        attempt['stage'] = 'model_request'
        self.save(state)
        if self.campaign.stopped():
            raise r.Stopped('stop requested before model call')
        started = time.monotonic()
        env = {key: os.environ[key] for key in ('PATH', 'LANG', 'LC_ALL', 'SSL_CERT_FILE', 'SSL_CERT_DIR') if key in os.environ}
        if config['api_key_env'] in os.environ:
            env[config['api_key_env']] = os.environ[config['api_key_env']]
        process = subprocess.Popen([sys.executable, '-I', '-X', 'utf8', '-c', HTTP_CLIENT, str(spec_path), str(response_path)],
                                   stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                   start_new_session=True, env=env)
        try:
            attempt['active_process_group'] = process.pid
            self.save(state)
            while process.poll() is None:
                if self.campaign.stopped():
                    raise r.Stopped('stop requested during model call')
                r.require(time.monotonic() - started <= config['request_timeout_seconds'], 'model request timed out')
                time.sleep(0.05)
            if self.campaign.stopped():
                raise r.Stopped('stop requested during model call')
            r.require(time.monotonic() - started <= config['request_timeout_seconds'], 'model request timed out')
            r.require(process.returncode == 0, 'model request failed; no automatic HTTP retry')
        finally:
            r.kill_group(process)
            attempt.pop('active_process_group', None)
        return response_path

    def commit_plan(self, directory, context, files, plan, request_id):
        r = self.r
        self.check_live(context)
        manifest = self.campaign.load()['manifest']
        replacements = {item['path']: item['content'] for item in plan['edits']}
        r.require(sum(len(replacements.get(item['path'], item['content']).encode('utf-8'))
                      for item in files) <= 65536, 'replacement exceeds next-round source budget')
        worktree = directory / 'candidate'
        r.git(Path(manifest['repo']), 'worktree', 'add', '--detach', str(worktree), context['best']['commit'])
        sources = {item['path']: item for item in files}
        changed = []
        for edit in plan['edits']:
            source = sources[edit['path']]
            r.require(edit['before_blob'] == source['blob'], 'source blob mismatch')
            target = worktree / edit['path']
            for parent in target.relative_to(worktree).parents:
                r.require(not (worktree / parent).is_symlink(), 'symlink in source parents')
            fd = os.open(target, os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK)
            with os.fdopen(fd, 'r+b') as stream:
                r.require(stat.S_ISREG(os.fstat(stream.fileno()).st_mode), 'not a regular source file')
                r.require(stream.read(65537) == source['content'].encode(), 'checkout content differs from source snapshot')
                stream.seek(0)
                stream.write(edit['content'].encode('utf-8'))
                stream.truncate()
                stream.flush()
                os.fsync(stream.fileno())
            changed.append(edit['path'])
        r.git(worktree, '--literal-pathspecs', 'add', '--', *changed)
        diff = r.git(worktree, 'diff', '--cached', '--no-ext-diff', '--name-only', '-z', '--')
        paths = {p for p in diff.split('\0') if p}
        r.require(bool(paths) and paths <= set(changed), 'empty change or unexpected staged paths')
        r.git(worktree, '-c', 'user.name=JSH Research', '-c', 'user.email=research@jsh.invalid',
              '-c', 'commit.gpgsign=false', 'commit', '-m', 'research(auto): ' + request_id)
        sha = r.commit_id(worktree, 'HEAD')
        r.check_candidate(manifest, sha)
        return sha

    def evaluate(self, trial_id, config):
        key = os.environ.pop(config['api_key_env'], None)
        try:
            return self.r.step(self.campaign, expected_trial_id=trial_id)
        finally:
            if key is not None:
                os.environ[config['api_key_env']] = key

    def run(self, limit=1, execute=False):
        r = self.r
        r.require(execute, 'auto-run requires --execute; authoring transmits selected source and evaluates generated code')
        r.require(type(limit) is int and 1 <= limit <= 1000, 'limit must be 1..1000')
        with self.lock_store.lock():
            state = self.load()
            r.require(not any(a['status'] == 'running' for a in state['attempts']), 'interrupted auto attempt: inspect and use auto-recover')
            stop_reason = 'invocation_limit'
            for _ in range(limit):
                campaign, context = self.view()
                config = state['config']
                action = self.decision(state, campaign, context)
                if action != 'needs_proposal':
                    stop_reason = action
                    break
                reservation = self.reservation(config, context)
                index = len(state['attempts']) + 1
                request_id = 'auto-' + state['loop_id'] + '-' + str(index)
                directory = self.directory / ('attempt-%06d' % index)
                attempt = {'id': request_id, 'status': 'running', 'stage': 'prepare', 'context_revision': context['revision'],
                           'base_commit': context['best']['commit'], 'reserved_output_tokens': config['max_tokens_per_call'],
                           'charged_seconds': reservation, 'evidence': str(directory.relative_to(self.campaign.root))}
                state['attempts'].append(attempt)
                state['reserved_output_tokens'] += config['max_tokens_per_call']
                state['charged_seconds'] += reservation
                # Persist identity/budget before allocating its evidence path.
                # A failed write must not strand the next attempt's directory.
                self.save(state)
                started = time.monotonic()
                try:
                    directory.mkdir(mode=0o700, exist_ok=False)
                    files = self.sources(context, config)
                    input_path = directory / 'author-input.json'
                    r.atomic_json(input_path, self.author_input(state, context, files, campaign))
                    prepared = self.native('author-prepare', input_path)
                    self.check_live(context)
                    response_path = self.model_call(state, attempt, directory, prepared)
                    attempt['stage'] = 'validate_plan'
                    self.save(state)
                    plan = self.native('author-decode', input_path, response_path)
                    r.atomic_json(directory / 'plan.json', plan)
                    attempt['stage'] = 'commit_candidate'
                    self.save(state)
                    sha = self.commit_plan(directory, context, files, plan, request_id)
                    attempt['candidate_commit'] = sha
                    attempt['stage'] = 'submit'
                    self.save(state)
                    proposal = {'schema_version': 2, 'campaign_id': context['campaign_id'], 'manifest_digest': context['manifest_digest'],
                                'expected_revision': context['revision'], 'expected_best_commit': context['best']['commit'],
                                'request_id': request_id, 'commit': sha, 'hypothesis': plan['hypothesis'], 'parents': [context['best']['id']]}
                    r.atomic_json(directory / 'proposal.json', proposal)
                    trial = self.bridge.submit(proposal)
                    attempt['trial_id'] = trial['id']
                    attempt['stage'] = 'evaluate'
                    self.save(state)
                    live = self.campaign.load()
                    outcome = next(item for item in live['trials'] if item['id'] == trial['id'])
                    if outcome['status'] == 'queued':
                        outcome = self.evaluate(trial['id'], config)
                    r.require(outcome['status'] in r.TERMINAL, 'trial is owned by another controller; inspect before recovery')
                    attempt['status'] = outcome['status']
                    attempt['stage'] = 'finished'
                except (KeyboardInterrupt, r.Stopped):
                    attempt['status'] = 'interrupted'
                    stop_reason = 'interrupted'
                except (r.ResearchError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
                    attempt['status'] = 'author_failed'
                    if attempt['stage'] == 'model_request':
                        stop_reason = 'transport_error'
                finally:
                    elapsed = time.monotonic() - started
                    state['charged_seconds'] += elapsed - reservation
                    attempt['charged_seconds'] = elapsed
                    attempt['failure_code'] = diagnostic_code(attempt)
                    attempt.pop('active_process_group', None)
                    state['failure_streak'] = streaks(state['attempts'])[0]
                    self.save(state)
                if stop_reason in ('interrupted', 'transport_error'):
                    break
            return {**self.summary(state), 'stop_reason': stop_reason}

    def recover(self):
        r = self.r
        # Same lock order as run: loop first, then campaign. Reject an active
        # evaluator before changing any attempt; never replay or kill stored PIDs.
        with self.lock_store.lock(), self.campaign.lock():
            state = self.load()
            campaign = self.campaign.load()
            trials = {item['request_id']: item for item in campaign['trials']}
            before = r.canonical(state)
            for attempt in state['attempts']:
                if attempt['status'] not in ('running', 'interrupted', 'author_failed'):
                    continue
                group = attempt.get('active_process_group')
                if group is not None:
                    r.require(type(group) is int and group > 1, 'invalid recorded process group')
                    try:
                        os.killpg(group, 0)
                    except ProcessLookupError:
                        pass
                    else:
                        raise r.ResearchError('model process group may still be alive; inspect before recovery')
                trial = trials.get(attempt['id'])
                r.require(trial is None or trial['status'] != 'running', 'inspect campaign recovery first')
                if trial is not None:
                    r.require(attempt.get('candidate_commit', trial['commit']) == trial['commit'] and
                              attempt.get('trial_id', trial['id']) == trial['id'], 'attempt/trial identity mismatch')
                    attempt['trial_id'] = trial['id']
                    attempt['status'] = trial['status'] if trial['status'] in r.TERMINAL else 'interrupted'
                elif attempt['status'] != 'running':
                    continue
                else:
                    attempt['status'] = 'interrupted'
                attempt['stage'] = 'recovered_without_replay'
                attempt['failure_code'] = diagnostic_code(attempt)
                attempt.pop('active_process_group', None)
            state['failure_streak'] = streaks(state['attempts'])[0]
            if r.canonical(state) != before:
                self.save(state)
            return self.summary(state)


def main(argv, engine, bridge_module, binary):
    probe = argparse.ArgumentParser(add_help=False)
    probe.add_argument('--root', type=Path, default=Path('.jsh-research'))
    probe.add_argument('command', nargs='?')
    known, _ = probe.parse_known_args(argv)
    commands = {'auto-init', 'auto-run', 'auto-status', 'auto-recover', 'auto-report', 'auto-preflight'}
    if known.command not in commands:
        return bridge_module.main(argv, engine)
    cli = argparse.ArgumentParser(prog='jsh research')
    cli.add_argument('--root', type=Path, default=Path('.jsh-research'))
    subs = cli.add_subparsers(dest='command', required=True)
    init = subs.add_parser('auto-init', help='freeze model/source/budget configuration; no network request')
    init.add_argument('config', type=Path)
    init.add_argument('--allow-remote', action='store_true', help='authorize selected source transmission to configured HTTPS endpoint')
    run = subs.add_parser('auto-run', help='bounded model -> edit -> commit -> evaluate loop')
    run.add_argument('--execute', action='store_true')
    run.add_argument('--limit', type=int, default=1)
    subs.add_parser('auto-status')
    subs.add_parser('auto-recover')
    report = subs.add_parser('auto-report', help='bounded read-only outcomes, budgets and next action; no source/logs')
    report.add_argument('--recent', type=int, default=16)
    subs.add_parser('auto-preflight', help='offline source/validator/readiness checks; no model or evaluation')
    args = cli.parse_args(argv)
    try:
        loop = AutoLoop(engine, bridge_module, args.root, binary)
        if args.command == 'auto-init':
            result = loop.initialize(engine.read_json(args.config), args.allow_remote)
        elif args.command == 'auto-run':
            result = loop.run(args.limit, args.execute)
        elif args.command == 'auto-recover':
            result = loop.recover()
        elif args.command == 'auto-report':
            result = loop.report(args.recent)
        elif args.command == 'auto-preflight':
            result = loop.preflight()
        else:
            result = loop.summary()
        sys.stdout.write(engine.canonical(result).decode() + '\n')
        if args.command == 'auto-preflight':
            return 0 if result['ready'] else 1
        if result.get('stop_reason') == 'interrupted':
            return 130
        if args.command == 'auto-run' and (result.get('stop_reason') in ('failure_limit', 'needs_recovery', 'needs_reconciliation', 'needs_baseline_retry', 'transport_error') or
                                         (result['attempts'] and result['attempts'][-1]['status'] in ('author_failed', 'failed', 'invalid', 'interrupted'))):
            return 1
        return 0
    except (engine.ResearchError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
        print(json.dumps({'protocol': PROTOCOL, 'error': 'auto operation rejected; inspect configuration, campaign state and attempt stage'}), file=sys.stderr)
        return 2


if __name__ == '__main__':
    args = sys.argv[1:]
    if args[:1] == ['--embedded']:
        engine = load_module('jsh_auto_engine', args[1])
        bridge = load_module('jsh_auto_bridge', args[2])
        binary, args = args[3], args[4:]
    else:
        import jsh_research as engine
        import jsh_research_bridge as bridge
        binary = os.environ.get('JSH_RESEARCH_BINARY')

    def terminated(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminated)
    try:
        sys.exit(main(args, engine, bridge, binary))
    except KeyboardInterrupt:
        sys.exit(130)
