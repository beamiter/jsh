"""Campaign ledger integrity, idempotent recovery and shared score projection."""
import copy
import json
import os
import subprocess
import unittest

from test_research_bridge import BridgeFixture, r


class LedgerTests(BridgeFixture):
    def test_total_time_cannot_disagree_with_individual_trials(self):
        self.baseline()
        original = self.store.load()
        for total in (0, original['charged_seconds'] + 1):
            state = copy.deepcopy(original)
            state['charged_seconds'] = total
            self.store.save(state)
            before = self.store.state_path.read_bytes()
            with self.assertRaisesRegex(r.ResearchError, 'charges.*ledger'):
                self.store.load()
            self.assertEqual(self.store.state_path.read_bytes(), before)

    def test_duplicate_trial_and_request_ids_are_rejected(self):
        self.baseline()
        original = self.store.load()
        for duplicate in ('id', 'request_id'):
            state = copy.deepcopy(original)
            extra = copy.deepcopy(state['trials'][0])
            extra.update(id='e000001', request_id='candidate-1', kind='candidate', status='kept',
                         parents=['baseline'], charged_seconds=0)
            extra[duplicate] = state['trials'][0][duplicate]
            state['trials'].append(extra)
            self.store.save(state)
            with self.subTest(duplicate=duplicate), self.assertRaisesRegex(r.ResearchError, 'duplicate'):
                self.store.load()

    def test_malformed_trial_identity_status_and_charge_fail_closed(self):
        self.baseline()
        original = self.store.load()
        for field, value in (('id', '../outside'), ('status', 'promoted_by_model'), ('kind', 'unknown'),
                             ('commit', 'HEAD'), ('charged_seconds', -1), ('charged_seconds', True),
                             ('results', {'extra': []}), ('parents', ['missing'])):
            state = copy.deepcopy(original)
            state['trials'][0][field] = value
            self.store.save(state)
            with self.subTest(field=field, value=value), self.assertRaises(r.ResearchError):
                self.store.load()

    def test_invalid_best_pointer_and_overfull_budget_are_rejected(self):
        self.baseline()
        original = self.store.load()
        for best in ('missing', True):
            state = copy.deepcopy(original)
            state['best_id'] = best
            self.store.save(state)
            with self.assertRaisesRegex(r.ResearchError, 'best'):
                self.store.load()
        state = copy.deepcopy(original)
        state['trials'] *= state['manifest']['budget']['max_trials'] + 1
        self.store.save(state)
        with self.assertRaisesRegex(r.ResearchError, 'trial accounting'):
            self.store.load()

    def test_rounding_and_legacy_optional_fields_remain_supported(self):
        self.baseline()
        state = self.store.load()
        required = {'id', 'request_id', 'commit', 'hypothesis', 'kind', 'parents', 'status', 'results', 'charged_seconds'}
        state['trials'][0] = {key: value for key, value in state['trials'][0].items() if key in required}
        state['trials'][0]['request_id'] = 'legacy request / accepted by enqueue v1'
        state['trials'][0]['charged_seconds'] = 0.1
        extra = copy.deepcopy(state['trials'][0])
        extra.update(id='e000001', request_id='candidate-1', kind='candidate', status='kept',
                     parents=['baseline'], charged_seconds=0.2)
        state['trials'].append(extra)
        state['charged_seconds'] = 0.3
        self.store.save(state)
        self.assertEqual(self.store.load()['charged_seconds'], 0.3)

    def test_noop_recovery_preserves_revision_and_prepared_proposal(self):
        self.baseline()
        proposal = self.bound(self.candidate())
        before = self.store.state_path.read_bytes()
        r.recover(self.store)
        r.recover(self.store)
        self.assertEqual(self.store.state_path.read_bytes(), before)
        self.assertEqual(self.bridge.submit(proposal)['commit'], proposal['commit'])

    def test_recovery_changes_a_running_trial_only_once(self):
        state = self.init()
        state['trials'][0].update(status='running', charged_seconds=8)
        state['charged_seconds'] = 8
        self.store.save(state)
        self.assertEqual(self.store.load()['trials'][0]['status'], 'running')
        revision = state['revision']
        r.recover(self.store)
        recovered = self.store.load()
        self.assertEqual(recovered['revision'], revision + 1)
        self.assertEqual(recovered['charged_seconds'], 8)
        self.assertEqual(recovered['trials'][0]['status'], 'interrupted')
        before = self.store.state_path.read_bytes()
        r.recover(self.store)
        self.assertEqual(self.store.state_path.read_bytes(), before)

    def test_summary_and_export_do_not_advertise_a_partial_stage(self):
        self.baseline()
        source = "import os\nif os.environ['JSH_RESEARCH_SEED'] == '2':\n    raise RuntimeError('second seed failed')\nSCORE = 8\n"
        trial = self.store.enqueue(self.proposal(self.candidate(source)))
        self.assertEqual(r.step(self.store)['status'], 'failed')
        summary = r.summary(self.store, self.store.load())['trials'][-1]
        exported = r.export_context(self.store)['trials'][-1]
        bridged = self.bridge.snapshot()['recent_trials'][-1]
        self.assertEqual(summary['id'], trial['id'])
        self.assertEqual(summary['scores'], {})
        self.assertEqual(summary['complete_stages'], [])
        self.assertEqual(summary['scores'], exported['scores'])
        self.assertEqual(summary['scores'], bridged['scores'])

    def test_summary_requires_valid_ordered_seed_evidence(self):
        self.baseline()
        original = self.store.load()
        for seeds in ([1], [1, 1], [2, 1], [True, 2]):
            state = copy.deepcopy(original)
            state['trials'][0]['results']['screen'] = state['trials'][0]['results']['screen'][:len(seeds)]
            for result, seed in zip(state['trials'][0]['results']['screen'], seeds):
                result['seed'] = seed
            summary = r.summary(self.store, state)['trials'][0]
            self.assertEqual(summary['scores'], {})
            self.assertEqual(summary['complete_stages'], [])

    def test_summary_rejects_boolean_and_failed_guardrail_metrics(self):
        self.baseline()
        original = self.store.load()
        for metric, value in (('loss', True), ('violations', 1)):
            state = copy.deepcopy(original)
            state['trials'][0]['results']['screen'][0]['metrics'][metric] = value
            with self.assertRaises(r.ResearchError):
                r.summary(self.store, state)


@unittest.skipUnless(os.environ.get('JSH_RESEARCH_BINARY'), 'requires compiled jsh binary')
class LedgerNativeTests(BridgeFixture):
    def native(self, *args):
        return subprocess.run([os.environ['JSH_RESEARCH_BINARY'], 'research', '--root', str(self.store.root), *args],
                              cwd=self.base, capture_output=True, text=True, timeout=30)

    def test_noop_recovery_keeps_a_prepared_proposal_valid(self):
        self.baseline()
        proposal = self.bound(self.candidate())
        before = self.store.state_path.read_bytes()
        for _ in range(2):
            result = self.native('recover')
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.store.state_path.read_bytes(), before)
        path = self.base / 'proposal.json'
        path.write_text(json.dumps(proposal))
        result = self.native('submit', str(path))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_corrupt_time_ledger_blocks_execution_without_state_changes(self):
        self.baseline()
        self.store.enqueue(self.proposal(self.candidate()))
        state = self.store.load()
        state['charged_seconds'] = 0
        self.store.save(state)
        before = self.store.state_path.read_bytes()
        result = self.native('run', '--execute')
        self.assertEqual(result.returncode, 2, result.stderr + result.stdout)
        self.assertEqual(self.store.state_path.read_bytes(), before)
        self.assertFalse((self.store.root / 'trials/e000001').exists())

    def test_status_and_export_omit_mismatched_seed_means(self):
        self.baseline()
        state = self.store.load()
        state['trials'][0]['results']['screen'][0]['seed'] = 999
        self.store.save(state)
        for command in ('status', 'export'):
            result = self.native(command)
            self.assertEqual(result.returncode, 0, result.stderr)
            trial = json.loads(result.stdout)['trials'][0]
            self.assertEqual(trial['scores'], {})
            self.assertEqual(trial['complete_stages'], [])


if __name__ == '__main__':
    unittest.main()
