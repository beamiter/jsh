#!/usr/bin/env python3
"""Bounded model-driven research loop. Explicit opt-in, local executor only.

Model output is data validated by jagent, never a shell command. The host writes
only preselected tracked files and creates detached candidate commits. This is
not a sandbox: use a disposable, credential-free environment for generated code.
"""
from __future__ import annotations

import argparse
import ipaddress
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import time
import types
import uuid
from urllib.parse import urlsplit

PROTOCOL = "jsh.research.auto/v1"
MAX_RESPONSE = 1024 * 1024
MAX_INPUT = 512 * 1024
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
        if len(key) > 8192 or '\r' in key or '\n' in key: sys.exit(65)
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


class AutoLoop:
    def __init__(self, engine, bridge_module, root, native_binary):
        self.r, self.b = engine, bridge_module
        self.bridge = bridge_module.Bridge(engine, root)
        self.campaign = self.bridge.store
        self.directory = self.campaign.root / 'autoloop'
        self.lock_store = engine.Store(self.directory)
        self.state_path = self.directory / 'auto.json'
        self.binary = str(Path(native_binary).absolute()) if native_binary else None

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
        context = self.bridge.snapshot()
        r.require(context['campaign_id'] == value['campaign_id'] and context['manifest_digest'] == value['manifest_digest'], 'auto loop belongs to another campaign')
        return value

    def save(self, state):
        self.r.atomic_json(self.state_path, state)

    def initialize(self, raw, allow_remote=False):
        r = self.r
        r.fields(raw, {'schema_version', 'endpoint', 'model', 'source_paths', 'max_attempts', 'max_output_tokens',
                       'max_tokens_per_call', 'request_timeout_seconds', 'max_seconds', 'max_failures'},
                 {'api_key_env', 'response_format'}, 'auto config')
        r.require(type(raw['schema_version']) is int and raw['schema_version'] == 1, 'unsupported auto config version')
        endpoint = r.text(raw['endpoint'], 'endpoint', 2048)
        r.require(not any(ord(ch) <= 32 for ch in endpoint), 'invalid endpoint characters')
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
        # Trigger validation of malformed port syntax before persisting config.
        _ = parsed.port
        model = r.text(raw['model'], 'model', 256)
        r.require(model.strip() and not any(ord(ch) < 32 for ch in model), 'invalid model name')
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
                  'allow_remote': bool(allow_remote), 'api_key_env': key_env, 'response_format': response_format, **limits}
        r.require(self.reservation(config, context) <= config['max_seconds'], 'time budget cannot cover one reserved attempt')
        # Validate selected files without sending them anywhere.
        self.sources(context, config)
        with self.lock_store.lock(create=True):
            r.require(not os.path.lexists(self.state_path), 'auto loop already initialized; never resets budgets')
            state = {'protocol': PROTOCOL, 'loop_id': uuid.uuid4().hex, 'campaign_id': context['campaign_id'],
                     'manifest_digest': context['manifest_digest'], 'config': config, 'config_digest': r.digest(config),
                     'charged_seconds': 0.0, 'reserved_output_tokens': 0, 'failure_streak': 0, 'attempts': []}
            self.save(state)
            return self.summary(state)

    @staticmethod
    def reservation(config, context):
        # Include fixed setup allowance; actual elapsed time is charged normally.
        # This is conservative admission, not a kernel-enforced realtime quota.
        return config['request_timeout_seconds'] + context['budget']['trial_reservation_seconds'] + 120.0

    def summary(self, state=None):
        state = state or self.load()
        return {'protocol': PROTOCOL, 'loop_id': state['loop_id'], 'campaign_id': state['campaign_id'],
                'charged_seconds': state['charged_seconds'], 'reserved_output_tokens': state['reserved_output_tokens'],
                'failure_streak': state['failure_streak'], 'stopped': self.campaign.stopped(),
                'limits': {key: state['config'][key] for key in ('max_attempts', 'max_output_tokens', 'max_seconds', 'max_failures')},
                'attempts': state['attempts']}

    def sources(self, context, config):
        r = self.r
        manifest = self.campaign.load()['manifest']
        repo = Path(manifest['repo'])
        base = context['best']['commit']
        files, total = [], 0
        for path in config['source_paths']:
            r.require(any(r.within(path, prefix) for prefix in manifest['editable_paths']) and
                      not any(r.within(path, prefix) for prefix in manifest['protected_paths']), 'source is not editable')
            entry = r.git(repo, '--literal-pathspecs', 'ls-tree', '-z', base, '--', path)
            records = entry.split('\0')
            r.require(len(records) == 2 and records[-1] == '', 'source must name exactly one tracked file')
            header, actual = records[0].split('\t', 1)
            mode, kind, blob = header.split()
            r.require(actual == path and mode in ('100644', '100755') and kind == 'blob', 'source must be a regular tracked file, not a link/submodule')
            size = int(r.git(repo, 'cat-file', '-s', blob))
            total += size
            r.require(total <= 65536, 'selected source exceeds 64 KiB')
            data = subprocess.run(['git', '-C', str(repo), 'cat-file', 'blob', blob], stdin=subprocess.DEVNULL,
                                  stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=15, check=True).stdout
            r.require(len(data) == size and b'\0' not in data, 'invalid source bytes')
            content = data.decode('utf-8', 'strict')
            files.append({'path': path, 'blob': blob, 'content': content})
        return files

    def native(self, command, *paths):
        self.r.require(self.binary is not None, 'compiled jsh binary required for shared jagent validation')
        result = subprocess.run([self.binary, 'research', command, *map(str, paths)],
                                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                timeout=20, check=False)
        self.r.require(result.returncode == 0 and len(result.stdout) <= 2 * MAX_INPUT, 'jagent authoring validation failed')
        return json.loads(result.stdout, object_pairs_hook=self.r.unique_object, parse_constant=self.r.invalid_constant)

    def check_live(self, context):
        self.r.require(not self.campaign.stopped(), 'campaign stopped')
        live = self.bridge.snapshot()
        self.r.require(live['next_action'] == 'needs_proposal' and all(live[key] == context[key]
                       for key in ('campaign_id', 'manifest_digest', 'revision', 'best')), 'research context changed during authoring')

    def model_call(self, state, attempt, directory, prepared):
        r, config = self.r, state['config']
        body = {'model': config['model'], 'messages': prepared['messages'], 'stream': False, 'n': 1,
                'max_tokens': config['max_tokens_per_call'], 'temperature': 1.0, 'top_p': 0.95, 'top_k': 20}
        if config['response_format'] == 'json_schema':
            body['response_format'] = {'type': 'json_schema', 'json_schema': {
                'name': 'research_edit', 'strict': True, 'schema': prepared['response_schema']}}
        else:
            body['response_format'] = {'type': 'json_object'}
            # Even when server grammar support is disabled, the model sees the
            # schema and the exact same strict jagent validator remains mandatory.
            body['messages'] = body['messages'] + [{'role': 'user', 'content': json.dumps({'required_response_schema': prepared['response_schema']})}]
        spec = {'endpoint': config['endpoint'], 'key_env': config['api_key_env'],
                'timeout': config['request_timeout_seconds'], 'body': body}
        spec_path, response_path = directory / 'http-request.json', directory / 'http-response.json'
        r.atomic_json(spec_path, spec)
        attempt['stage'] = 'model_request'
        self.save(state)  # Attempt/token/time reservations already persisted.
        started = time.monotonic()
        process = subprocess.Popen([sys.executable, '-I', '-X', 'utf8', '-c', HTTP_CLIENT, str(spec_path), str(response_path)],
                                   stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                   start_new_session=True)
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

    def run(self, limit=1, execute=False):
        r = self.r
        r.require(execute, 'auto-run requires --execute; authoring transmits selected source and evaluates generated code')
        r.require(type(limit) is int and 1 <= limit <= 1000, 'limit must be 1..1000')
        with self.lock_store.lock():
            state = self.load()
            r.require(not any(a['status'] == 'running' for a in state['attempts']), 'interrupted auto attempt: inspect and use auto-recover')
            stop_reason = 'invocation_limit'
            for _ in range(limit):
                context = self.bridge.snapshot()
                config = state['config']
                if context['next_action'] != 'needs_proposal':
                    stop_reason = context['next_action']
                    break
                reservation = self.reservation(config, context)
                if (len(state['attempts']) >= config['max_attempts'] or
                    state['reserved_output_tokens'] + config['max_tokens_per_call'] > config['max_output_tokens'] or
                    state['charged_seconds'] + reservation > config['max_seconds']):
                    stop_reason = 'auto_budget_exhausted'
                    break
                if state['failure_streak'] >= config['max_failures']:
                    stop_reason = 'failure_limit'
                    break
                index = len(state['attempts']) + 1
                request_id = 'auto-' + state['loop_id'] + '-' + str(index)
                directory = self.directory / ('attempt-%06d' % index)
                directory.mkdir(mode=0o700, exist_ok=False)
                attempt = {'id': request_id, 'status': 'running', 'stage': 'prepare', 'context_revision': context['revision'],
                           'base_commit': context['best']['commit'], 'reserved_output_tokens': config['max_tokens_per_call'],
                           'charged_seconds': reservation, 'evidence': str(directory.relative_to(self.campaign.root))}
                state['attempts'].append(attempt)
                state['reserved_output_tokens'] += config['max_tokens_per_call']
                state['charged_seconds'] += reservation
                self.save(state)
                started = time.monotonic()
                try:
                    files = self.sources(context, config)
                    feedback = [item['status'] for item in state['attempts'][:-1][-4:]]
                    author_input = {'schema_version': 1, 'context': context, 'files': files, 'feedback': feedback}
                    input_path = directory / 'author-input.json'
                    r.require(len(r.canonical(author_input)) + 1 <= MAX_INPUT, 'author input is too large')
                    r.atomic_json(input_path, author_input)
                    prepared = self.native('author-prepare', input_path)
                    response_path = self.model_call(state, attempt, directory, prepared)
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
                    # Never accidentally evaluate a different queue head after a
                    # competing controller has already processed our candidate.
                    live = self.campaign.load()
                    outcome = next(item for item in live['trials'] if item['id'] == trial['id'])
                    if outcome['status'] == 'queued':
                        outcome = r.step(self.campaign, expected_trial_id=trial['id'])
                    r.require(outcome['status'] in r.TERMINAL, 'trial is owned by another controller; inspect before recovery')
                    attempt['status'] = outcome['status']
                    attempt['stage'] = 'finished'
                    if outcome['status'] in ('kept', 'archived', 'screened_out'):
                        state['failure_streak'] = 0
                    else:
                        state['failure_streak'] += 1
                except (KeyboardInterrupt, r.Stopped):
                    attempt['status'] = 'interrupted'
                    state['failure_streak'] += 1
                    stop_reason = 'interrupted'
                except (r.ResearchError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
                    # Source, model replies and remote error bodies can contain
                    # credentials. Persist a stage/category, not arbitrary text.
                    attempt['status'] = 'author_failed'
                    state['failure_streak'] += 1
                finally:
                    elapsed = time.monotonic() - started
                    state['charged_seconds'] += elapsed - reservation
                    attempt['charged_seconds'] = elapsed
                    attempt.pop('active_process_group', None)
                    self.save(state)
                if stop_reason == 'interrupted':
                    break
            return {**self.summary(state), 'stop_reason': stop_reason}

    def recover(self):
        r = self.r
        with self.lock_store.lock():
            state = self.load()
            campaign = self.campaign.load()
            for attempt in state['attempts']:
                if attempt['status'] != 'running':
                    continue
                group = attempt.get('active_process_group')
                if group:
                    try:
                        os.killpg(group, 0)
                    except ProcessLookupError:
                        pass
                    else:
                        raise r.ResearchError('model process group may still be alive; inspect before recovery')
                trial = next((item for item in campaign['trials'] if item['request_id'] == attempt['id']), None)
                r.require(trial is None or trial['status'] != 'running', 'evaluation still running or interrupted: inspect campaign recovery first')
                if trial is not None and trial['status'] in r.TERMINAL:
                    attempt['status'] = trial['status']
                    attempt['trial_id'] = trial['id']
                else:
                    attempt['status'] = 'interrupted'
                attempt['stage'] = 'recovered_without_replay'
                attempt.pop('active_process_group', None)
                state['failure_streak'] += 1
                # Keep full reservation. A lost response may have consumed all
                # output tokens/time; never resend unknown work automatically.
            self.save(state)
            return self.summary(state)


def main(argv, engine, bridge_module, binary):
    # Recognize only new commands; preserve all existing bridge/engine parsing.
    probe = argparse.ArgumentParser(add_help=False)
    probe.add_argument('--root', type=Path, default=Path('.jsh-research'))
    probe.add_argument('command', nargs='?')
    known, _ = probe.parse_known_args(argv)
    commands = {'auto-init', 'auto-run', 'auto-status', 'auto-recover'}
    if known.command not in commands:
        return bridge_module.main(argv, engine)
    cli = argparse.ArgumentParser(prog='jsh research')
    cli.add_argument('--root', type=Path, default=Path('.jsh-research'))
    subs = cli.add_subparsers(dest='command', required=True)
    init = subs.add_parser('auto-init', help='freeze model/source/budget configuration; no network request')
    init.add_argument('config', type=Path)
    init.add_argument('--allow-remote', action='store_true', help='authorize transmitting selected source to this configured HTTPS endpoint')
    run = subs.add_parser('auto-run', help='bounded model -> edit -> commit -> evaluate loop')
    run.add_argument('--execute', action='store_true')
    run.add_argument('--limit', type=int, default=1)
    subs.add_parser('auto-status')
    subs.add_parser('auto-recover')
    args = cli.parse_args(argv)
    try:
        loop = AutoLoop(engine, bridge_module, args.root, binary)
        if args.command == 'auto-init':
            result = loop.initialize(engine.read_json(args.config), args.allow_remote)
        elif args.command == 'auto-run':
            result = loop.run(args.limit, args.execute)
        elif args.command == 'auto-recover':
            result = loop.recover()
        else:
            result = loop.summary()
        sys.stdout.write(engine.canonical(result).decode() + '\n')
        if result.get('stop_reason') == 'interrupted':
            return 130
        if args.command == 'auto-run' and (result.get('stop_reason') in ('failure_limit', 'needs_recovery', 'needs_baseline_retry') or
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
