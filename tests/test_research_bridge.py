"""Research bridge unit tests and real jsh/jagent binary integration tests."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import unittest
from concurrent.futures import ThreadPoolExecutor
from unittest.mock import patch

from test_research import Fixture, r

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "jsh_research_bridge.py"
SPEC = importlib.util.spec_from_file_location("jsh_research_bridge", SCRIPT)
b = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(b)


class BridgeFixture(Fixture):
    def setUp(self):
        super().setUp()
        self.bridge = b.Bridge(r, self.store.root)

    def bound(self, commit, request_id="bound-1", context=None):
        context = context or self.bridge.snapshot()
        return {"schema_version": 2, "campaign_id": context["campaign_id"],
                "manifest_digest": context["manifest_digest"], "expected_revision": context["revision"],
                "expected_best_commit": context["best"]["commit"], "request_id": request_id,
                "commit": commit, "hypothesis": "Reduce loss with unchanged guardrails", "parents": [context["best"]["id"]]}

    def bridge_cli(self, *args):
        return subprocess.run([sys.executable, str(SCRIPT), "--root", str(self.store.root), *args],
                              capture_output=True, text=True, timeout=20)


class BridgeTests(BridgeFixture):
    def test_identity_is_persistent_and_independent_between_identical_campaigns(self):
        self.baseline()
        first = self.bridge.snapshot()
        self.assertEqual(first, self.bridge.snapshot())
        other = b.Bridge(r, self.base / "other")
        other.store.initialize(self.manifest, self.base)
        r.step(other.store)
        second = other.snapshot()
        self.assertEqual(first["manifest_digest"], second["manifest_digest"])
        self.assertNotEqual(first["campaign_id"], second["campaign_id"])
        self.assertEqual(self.bridge.identity_path.stat().st_mode & 0o777, 0o600)

    def test_bridge_remains_readable_while_writer_holds_lock(self):
        self.baseline()
        self.bridge.snapshot()
        with self.store.lock():
            result = self.bridge_cli("bridge")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["next_action"], "needs_proposal")

    def test_valid_submission_is_evaluated_by_the_existing_engine(self):
        self.baseline()
        proposal = self.bound(self.candidate())
        admitted = self.bridge.submit(proposal)
        self.assertEqual(admitted["proposal_schema"], 2)
        self.assertEqual(admitted["bridge_binding"]["expected_revision"], proposal["expected_revision"])
        self.assertEqual(self.bridge.snapshot()["next_action"], "evaluate_queued")
        self.assertEqual(r.step(self.store)["status"], "kept")
        self.assertEqual(self.bridge.snapshot()["best"]["id"], admitted["id"])

    def test_exact_replay_survives_state_advance_and_stop_without_extra_charge(self):
        self.baseline()
        proposal = self.bound(self.candidate())
        first = self.bridge.submit(proposal)
        r.step(self.store)
        self.store.stop()
        before = self.store.load()
        self.assertEqual(self.bridge.submit(proposal)["id"], first["id"])
        self.assertEqual(self.store.load(), before)
        with self.assertRaises(r.ResearchError):
            self.bridge.submit({**proposal, "hypothesis": "different content"})

    def test_stale_foreign_and_wrong_incumbent_fail_without_consuming_budget(self):
        self.baseline()
        proposal = self.bound(self.candidate())
        for key, value in (("campaign_id", "e" * 32), ("manifest_digest", "e" * 64),
                           ("expected_revision", proposal["expected_revision"] + 1),
                           ("expected_best_commit", "e" * 40)):
            before = self.store.load()
            with self.subTest(key=key), self.assertRaises(r.ResearchError):
                self.bridge.submit({**proposal, key: value})
            self.assertEqual(self.store.load(), before)

    def test_resume_invalidates_pre_stop_proposals(self):
        self.baseline()
        proposal = self.bound(self.candidate())
        self.store.stop()
        self.assertEqual(self.bridge.snapshot()["next_action"], "stopped")
        result = self.bridge_cli("resume")
        self.assertEqual(result.returncode, 0, result.stderr)
        with self.assertRaisesRegex(r.ResearchError, "stale"):
            self.bridge.submit(proposal)
        self.assertEqual(len(self.store.load()["trials"]), 1)

    def test_parent_visibility_and_exact_commit_required(self):
        self.baseline()
        proposal = self.bound(self.candidate())
        for key, value in (("parents", []), ("parents", ["baseline", "baseline"]),
                           ("parents", ["e000099"]), ("commit", "HEAD"),
                           ("commit", "a" * 41), ("commit", "a" * 40)):
            with self.subTest(key=key, value=value), self.assertRaises(r.ResearchError):
                self.bridge.submit({**proposal, key: value})

    def test_unknown_fields_and_invalid_types_fail_closed(self):
        self.baseline()
        proposal = self.bound(self.candidate())
        for key, value in (("argv", ["sh"]), ("metrics", {"loss": 0}), ("schema_version", True),
                           ("expected_revision", float(proposal["expected_revision"])),
                           ("request_id", "../escape"), ("request_id", "baseline"),
                           ("hypothesis", "研" * 3000), ("hypothesis", "  ")):
            with self.subTest(key=key), self.assertRaises(r.ResearchError):
                self.bridge.submit({**proposal, key: value})

    def test_protected_evaluator_changes_are_still_rejected(self):
        self.baseline()
        sha = self.candidate(extra={"evaluate.py": "print('forged')"})
        with self.assertRaises(r.ResearchError):
            self.bridge.submit(self.bound(sha))
        self.assertEqual(len(self.store.load()["trials"]), 1)

    def test_no_proposal_when_time_budget_cannot_cover_one_attempt(self):
        self.baseline()
        proposal = self.bound(self.candidate())
        state = self.store.load()
        state["charged_seconds"] = 199
        self.store.save(state)
        self.assertEqual(self.bridge.snapshot()["next_action"], "budget_exhausted")
        with self.assertRaises(r.ResearchError):
            self.bridge.submit(proposal)

    def test_memory_is_bounded_and_omissions_explicit(self):
        self.baseline()
        state = self.store.load()
        for i in range(1, 20):
            trial = copy.deepcopy(state["trials"][0])
            trial.update(id=f"e{i:06d}", request_id=f"legacy-{i}", kind="candidate",
                         status="screened_out", hypothesis="研" * 4000, parents=["baseline"])
            trial["results"].pop("confirm")
            state["trials"].append(trial)
        self.store.save(state)
        context = self.bridge.snapshot()
        self.assertEqual(len(context["recent_trials"]), 16)
        self.assertEqual(context["omitted_trials"], 4)
        self.assertEqual(context["best"]["id"], "baseline")
        self.assertTrue(all(len(t["hypothesis"].encode()) <= 2048 for t in context["recent_trials"]))
        self.assertLess(len(r.canonical(context)), b.MAX_CONTEXT)
        self.assertNotIn("stdout", json.dumps(context))

    def test_identity_corruption_is_not_silently_reset(self):
        self.baseline()
        self.bridge.snapshot()
        identity = r.read_json(self.bridge.identity_path)
        identity["manifest_digest"] = "e" * 64
        r.atomic_json(self.bridge.identity_path, identity)
        with self.assertRaises(r.ResearchError):
            self.bridge.snapshot()

    def test_duplicate_encoded_proposal_members_rejected(self):
        self.baseline()
        proposal = self.bound(self.candidate())
        path = self.base / "duplicate.json"
        encoded = json.dumps(proposal).replace("{", '{"schema_version":2,', 1)
        path.write_text(encoded)
        result = self.bridge_cli("submit", str(path))
        self.assertEqual(result.returncode, 2)
        self.assertEqual(len(self.store.load()["trials"]), 1)

    def test_concurrent_delivery_admits_at_most_once(self):
        self.baseline()
        proposal = self.bound(self.candidate())
        path = self.base / "proposal.json"
        path.write_text(json.dumps(proposal))
        with ThreadPoolExecutor(max_workers=4) as pool:
            results = list(pool.map(lambda _: self.bridge_cli("submit", str(path)), range(8)))
        self.assertTrue(any(result.returncode == 0 for result in results))
        self.assertTrue(all(result.returncode in (0, 2) for result in results))
        self.assertEqual(len(self.store.load()["trials"]), 2)

    def test_baseline_retries_cannot_be_queued_twice(self):
        state = self.init()
        state["trials"][0]["status"] = "interrupted"
        self.store.save(state)
        r.retry(self.store, "baseline", "first-retry")
        with self.assertRaisesRegex(r.ResearchError, "pending"):
            r.retry(self.store, "baseline", "second-retry")
        self.assertEqual(len(self.store.load()["trials"]), 2)

    def test_deadline_is_checked_even_if_child_exits_before_first_poll(self):
        self.manifest["budget"]["timeout_seconds"] = 0.1
        self.init()
        original = subprocess.Popen
        def delayed(*args, **kwargs):
            child = original(*args, **kwargs)
            if args[0][0] == sys.executable:
                child.wait(timeout=5)
                time.sleep(0.15)
            return child
        with patch.object(r.subprocess, "Popen", side_effect=delayed):
            outcome = r.step(self.store)
        self.assertEqual(outcome["status"], "failed", outcome)
        self.assertIn("timed out", outcome["reason"])
        self.assertIsNone(self.store.load()["best_id"])

    def test_stop_is_checked_after_fast_child_exit(self):
        self.init()
        original = subprocess.Popen
        def stopped(*args, **kwargs):
            child = original(*args, **kwargs)
            if args[0][0] == sys.executable:
                child.wait(timeout=5)
                self.store.stop()
            return child
        with patch.object(r.subprocess, "Popen", side_effect=stopped), self.assertRaises(r.Stopped):
            r.step(self.store)
        self.assertEqual(self.store.load()["trials"][0]["status"], "interrupted")


@unittest.skipUnless(os.environ.get("JSH_RESEARCH_BINARY"), "requires compiled jsh binary")
class NativeIntegrationTests(BridgeFixture):
    def native(self, *args):
        return subprocess.run([os.environ["JSH_RESEARCH_BINARY"], "research", *args],
                              cwd=self.base, capture_output=True, text=True, timeout=20)

    def files(self, context, proposal):
        cpath, ppath = self.base / "context.json", self.base / "bound.json"
        cpath.write_text(json.dumps(context))
        ppath.write_text(json.dumps(proposal))
        return str(cpath), str(ppath)

    def test_real_binary_end_to_end_bound_admission_evaluation_and_replay(self):
        self.baseline()
        result = self.native("--root", str(self.store.root), "bridge")
        self.assertEqual(result.returncode, 0, result.stderr)
        context = json.loads(result.stdout)
        proposal = self.bound(self.candidate(), context=context)
        cpath, ppath = self.files(context, proposal)
        request = self.native("prepare-request", cpath)
        self.assertEqual(request.returncode, 0, request.stderr)
        self.assertEqual(json.loads(request.stdout)["messages"][1]["role"], "user")
        validated = self.native("validate-proposal", cpath, ppath)
        self.assertEqual(validated.returncode, 0, validated.stderr)
        self.assertEqual(json.loads(validated.stdout), proposal)
        submitted = self.native("--root", str(self.store.root), "submit", ppath)
        self.assertEqual(submitted.returncode, 0, submitted.stderr)
        executed = self.native("--root", str(self.store.root), "run", "--execute")
        self.assertEqual(executed.returncode, 0, executed.stderr)
        self.assertEqual(json.loads(executed.stdout)["runs"][0]["status"], "kept")
        replay = self.native("--root", str(self.store.root), "submit", ppath)
        self.assertEqual(replay.returncode, 0, replay.stderr)
        self.assertEqual(len(self.store.load()["trials"]), 2)

    def test_rust_and_python_reject_the_same_invalid_proposal_corpus(self):
        self.baseline()
        context = self.bridge.snapshot()
        proposal = self.bound(self.candidate(), context=context)
        mutations = [("schema_version", True), ("expected_revision", float(context["revision"])),
                     ("campaign_id", "e" * 32), ("expected_best_commit", "e" * 40),
                     ("parents", ["e000099"]), ("commit", "HEAD"), ("request_id", "../x"),
                     ("metrics", {"loss": 0}), ("hypothesis", "研" * 3000)]
        for key, value in mutations:
            candidate = {**proposal, key: value}
            cpath, ppath = self.files(context, candidate)
            with self.subTest(key=key):
                self.assertEqual(self.native("validate-proposal", cpath, ppath).returncode, 2)
                self.assertEqual(self.native("--root", str(self.store.root), "submit", ppath).returncode, 2)
        self.assertEqual(len(self.store.load()["trials"]), 1)

    def test_snapshot_validation_does_not_bypass_live_freshness(self):
        self.baseline()
        context = self.bridge.snapshot()
        proposal = self.bound(self.candidate(), context=context)
        cpath, ppath = self.files(context, proposal)
        self.store.stop()
        self.assertEqual(self.native("--root", str(self.store.root), "resume").returncode, 0)
        self.assertEqual(self.native("validate-proposal", cpath, ppath).returncode, 0)
        self.assertEqual(self.native("--root", str(self.store.root), "submit", ppath).returncode, 2)
        self.assertEqual(len(self.store.load()["trials"]), 1)

    def test_native_inputs_reject_symlinks_fifos_and_excess_bytes(self):
        self.baseline()
        context = self.bridge.snapshot()
        cpath, _ = self.files(context, self.bound(self.candidate(), context=context))
        link, fifo, large = self.base / "link", self.base / "fifo", self.base / "large"
        link.symlink_to(cpath)
        os.mkfifo(fifo)
        large.write_bytes(b" " * (b.MAX_CONTEXT + 1))
        for path in (link, fifo, large):
            with self.subTest(path=path):
                self.assertEqual(self.native("prepare-request", str(path)).returncode, 2)


if __name__ == "__main__":
    unittest.main()
