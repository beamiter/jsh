"""Offline research-controller regression suite. Run with python3 -m unittest discover -s tests -p test_research.py."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "jsh_research.py"
SPEC = importlib.util.spec_from_file_location("jsh_research", SCRIPT)
r = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(r)

EVALUATOR = '''import json, os
from pathlib import Path
import candidate
seed = int(os.environ["JSH_RESEARCH_SEED"])
assert not Path("seed-cache").exists(), "worktree reused across seeds"
Path("seed-cache").write_text("fresh")
score = candidate.SCORE
if os.environ["JSH_RESEARCH_STAGE"] == "confirm":
    score = getattr(candidate, "CONFIRM", score)
result = {"valid": True, "metrics": {"loss": score + seed / 1000000, "violations": getattr(candidate, "VIOLATIONS", 0)}}
Path(os.environ["JSH_RESEARCH_RESULT"]).write_text(json.dumps(result))
'''


class Fixture(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="jsh-research-test-")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.repo = self.base / "repo"
        self.repo.mkdir()
        self.git("init", "-q")
        self.git("config", "user.email", "research-test@invalid.example")
        self.git("config", "user.name", "Research Test")
        (self.repo / "candidate.py").write_text("SCORE = 10\n")
        (self.repo / "evaluate.py").write_text(EVALUATOR)
        (self.repo / "dataset.json").write_text('{"frozen": true}')
        self.git("add", ".")
        self.git("commit", "-qm", "baseline")
        self.head = self.git("rev-parse", "HEAD")
        self.manifest = {
            "schema_version": 1, "name": "test", "repo": str(self.repo), "baseline": "HEAD",
            "objective": {"metric": "loss", "direction": "min", "min_delta": 0.01},
            "screen": {"argv": [sys.executable, "-B", "evaluate.py"], "seeds": [1, 2]},
            "confirm": {"argv": [sys.executable, "-B", "evaluate.py"], "seeds": [3, 4]},
            "protected_paths": ["evaluate.py", "dataset.json"], "editable_paths": ["candidate.py"],
            "guardrails": {"violations": {"max": 0}},
            "budget": {"max_trials": 20, "max_seconds": 200, "timeout_seconds": 2},
        }
        self.store = r.Store(self.base / "campaign")

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.repo), *args], stderr=subprocess.DEVNULL).decode().strip()

    def init(self):
        return self.store.initialize(self.manifest, self.base)

    def baseline(self):
        self.init()
        trial = r.step(self.store)
        self.assertEqual(trial["status"], "baseline", trial)
        return trial

    def candidate(self, source="SCORE = 8\n", extra=None):
        (self.repo / "candidate.py").write_text(source)
        for path, content in (extra or {}).items():
            (self.repo / path).write_text(content)
        self.git("add", ".")
        self.git("commit", "-qm", "candidate")
        return self.git("rev-parse", "HEAD")

    def proposal(self, sha, request_id="candidate-1", **extra):
        return {"schema_version": 1, "request_id": request_id, "commit": sha,
                "hypothesis": "Reduce the measured loss without increasing violations", **extra}

    def cli(self, *args):
        return subprocess.run([sys.executable, str(SCRIPT), "--root", str(self.store.root), *args],
                              capture_output=True, text=True, timeout=15)


class ManifestTests(Fixture):
    def test_initialization_freezes_head_and_contract(self):
        state = self.init()
        self.assertEqual(state["manifest"]["baseline"], self.head)
        self.assertEqual(state["manifest_digest"], r.digest(state["manifest"]))
        self.assertEqual(self.store.state_path.stat().st_mode & 0o777, 0o600)
        with self.assertRaises(r.ResearchError):
            self.init()

    def test_manifest_rejects_bad_inputs(self):
        cases = [
            ("schema_version", True), ("name", ""), ("screen", {"argv": "sh evaluate", "seeds": [1]}),
            ("confirm", {"argv": [sys.executable], "seeds": [1]}),
            ("protected_paths", ["../evaluate.py"]), ("editable_paths", ["."]),
            ("protected_paths", []), ("editable_paths", ["evaluate.py"]),
            ("objective", {"metric": "loss", "direction": "maximize"}),
            ("guardrails", {"x": {"min": 2, "max": 1}}),
            ("budget", {"max_trials": True, "max_seconds": 20, "timeout_seconds": 1}),
            ("budget", {"max_trials": 2, "max_seconds": 1, "timeout_seconds": 1}),
        ]
        for key, value in cases:
            with self.subTest(key=key, value=value):
                manifest = copy.deepcopy(self.manifest)
                manifest[key] = value
                with self.assertRaises(r.ResearchError):
                    r.normalize_manifest(manifest, self.base)

    def test_seed_validation(self):
        for seeds in ([], [1, 1], [True], [-1], [2**32], "1,2"):
            with self.subTest(seeds=seeds), self.assertRaises(r.ResearchError):
                r.evaluation({"argv": ["python3"], "seeds": seeds}, "test")

    def test_frozen_contract_digest_detects_change(self):
        state = self.init()
        state["manifest"]["objective"]["direction"] = "max"
        r.atomic_json(self.store.state_path, state)
        with self.assertRaisesRegex(r.ResearchError, "digest"):
            self.store.load()

    def test_root_symlink_rejected(self):
        link = self.base / "link"
        link.symlink_to(self.repo)
        with self.assertRaises(r.ResearchError):
            r.Store(link)

    def test_unknown_manifest_fields_fail_closed(self):
        self.manifest["model_can_change_evaluator"] = True
        with self.assertRaises(r.ResearchError):
            self.init()


class JsonTests(unittest.TestCase):
    def test_duplicate_nan_and_nonregular_json_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "data"
            for data in ('{"x": 1, "x": 2}', '{"x": NaN}', '{"x": Infinity}', 'no-json'):
                path.write_text(data)
                with self.subTest(data=data), self.assertRaises(r.ResearchError):
                    r.read_json(path)
            path.write_text('{"x": 1}')
            with self.assertRaises(r.ResearchError):
                r.read_json(path, limit=2)
            link = Path(temp) / "link"
            link.symlink_to(path)
            with self.assertRaises(OSError):
                r.read_json(link)
            fifo = Path(temp) / "fifo"
            os.mkfifo(fifo)
            with self.assertRaises(r.ResearchError):
                r.read_json(fifo)

    def test_numbers_reject_bools_nan_overflow(self):
        for value in (True, False, float("nan"), float("inf"), 10**1000, "2"):
            with self.subTest(value=str(value)[:30]), self.assertRaises(r.ResearchError):
                r.number(value, "test")

    def test_literal_prefixes_do_not_admit_sibling_paths(self):
        self.assertTrue(r.within("src/x.py", "src"))
        self.assertFalse(r.within("src-other/x.py", "src"))
        for path in ("../src", "src//x", "src/../x", ".git/config", "/tmp", "src/*", "src\\x", "src:foo"):
            with self.subTest(path=path), self.assertRaises(r.ResearchError):
                r.path_prefix(path)


class ExecutionTests(Fixture):
    def test_baseline_then_improvement_and_no_automatic_merge(self):
        self.baseline()
        sha = self.candidate()
        queued = self.store.enqueue(self.proposal(sha))
        self.git("checkout", "-q", self.head)
        outcome = r.step(self.store)
        self.assertEqual(outcome["status"], "kept", outcome)
        self.assertEqual(self.store.load()["best_id"], queued["id"])
        self.assertEqual(self.git("rev-parse", "HEAD"), self.head)
        self.assertEqual(len(outcome["results"]["screen"]), 2)
        self.assertEqual(len(outcome["results"]["confirm"]), 2)
        for sample in outcome["results"]["confirm"]:
            directory = self.store.root / sample["evidence"]
            self.assertTrue((directory / "receipt.json").is_file())
            self.assertTrue((directory / "request.json").is_file())
            self.assertTrue((directory / "stdout.log").is_file())

    def test_screen_failure_skips_confirmation(self):
        self.baseline()
        self.store.enqueue(self.proposal(self.candidate("SCORE = 12\n")))
        outcome = r.step(self.store)
        self.assertEqual(outcome["status"], "screened_out")
        self.assertNotIn("confirm", outcome["results"])
        self.assertEqual(self.store.load()["best_id"], "baseline")

    def test_confirmation_failure_archives_instead_of_promoting(self):
        self.baseline()
        self.store.enqueue(self.proposal(self.candidate("SCORE = 8\nCONFIRM = 12\n")))
        outcome = r.step(self.store)
        self.assertEqual(outcome["status"], "archived")
        self.assertEqual(self.store.load()["best_id"], "baseline")

    def test_guardrail_failure_is_invalid(self):
        self.baseline()
        self.store.enqueue(self.proposal(self.candidate("SCORE = 1\nVIOLATIONS = 1\n")))
        outcome = r.step(self.store)
        self.assertEqual(outcome["status"], "invalid", outcome)
        self.assertIn("guardrail", outcome["reason"])
        self.assertEqual(self.store.load()["best_id"], "baseline")

    def test_evaluator_tamper_is_rejected_before_execution(self):
        self.baseline()
        sha = self.candidate(extra={"evaluate.py": "print('forged')"})
        with self.assertRaises(r.ResearchError):
            self.store.enqueue(self.proposal(sha))
        self.assertEqual(len(self.store.load()["trials"]), 1)

    def test_noneditable_and_whitespace_paths_rejected(self):
        self.baseline()
        sha = self.candidate(extra={" candidate.py": "print('not allowed')"})
        with self.assertRaises(r.ResearchError):
            self.store.enqueue(self.proposal(sha))

    def test_runtime_modification_of_tracked_evaluator_is_invalid(self):
        self.baseline()
        source = "from pathlib import Path\nPath('evaluate.py').write_text('tampered')\nSCORE = 1\n"
        self.store.enqueue(self.proposal(self.candidate(source)))
        outcome = r.step(self.store)
        self.assertEqual(outcome["status"], "invalid", outcome)
        self.assertIn("modified tracked", outcome["reason"])

    def test_missing_result_is_invalid(self):
        self.baseline()
        self.store.enqueue(self.proposal(self.candidate("import os\nos._exit(0)\n")))
        outcome = r.step(self.store)
        self.assertEqual(outcome["status"], "invalid", outcome)

    def test_crashed_evaluation_is_failed_and_cli_nonzero(self):
        self.baseline()
        self.store.enqueue(self.proposal(self.candidate("raise RuntimeError('failure')\n")))
        result = self.cli("run", "--execute")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(json.loads(result.stdout)["runs"][0]["status"], "failed")

    def test_timeout_kills_child_and_preserves_budget(self):
        self.baseline()
        self.store.enqueue(self.proposal(self.candidate("import time\ntime.sleep(60)\nSCORE = 1\n")))
        before = self.store.load()["charged_seconds"]
        outcome = r.step(self.store)
        self.assertEqual(outcome["status"], "failed", outcome)
        self.assertIn("timed out", outcome["reason"])
        self.assertGreater(self.store.load()["charged_seconds"], before)
        self.assertNotIn("active_process_group", outcome)

    def test_excessive_output_is_failed_and_retained_log_is_bounded(self):
        self.baseline()
        source = "import sys\nsys.stdout.write('x' * (9 * 1024 * 1024))\nsys.stdout.flush()\nSCORE = 1\n"
        self.store.enqueue(self.proposal(self.candidate(source)))
        outcome = r.step(self.store)
        self.assertEqual(outcome["status"], "failed", outcome)
        self.assertIn("log limit", outcome["reason"])
        logs = list((self.store.root / "trials" / outcome["id"]).glob("*/stdout.log"))
        self.assertEqual(len(logs), 1)
        self.assertLessEqual(logs[0].stat().st_size, r.MAX_LOG)

    def test_explicit_authorization_is_required(self):
        self.init()
        result = self.cli("run")
        self.assertEqual(result.returncode, 2)
        self.assertIn("--execute", result.stderr)
        self.assertEqual(self.store.load()["trials"][0]["status"], "queued")

    def test_run_stops_at_empty_queue(self):
        self.baseline()
        result = self.cli("run", "--execute", "--limit", "2")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["runs"][0]["status"], "needs_proposal")

    def test_maximize_direction(self):
        self.manifest["objective"]["direction"] = "max"
        self.baseline()
        self.store.enqueue(self.proposal(self.candidate("SCORE = 12\n")))
        self.assertEqual(r.step(self.store)["status"], "kept")


class QueueAndRecoveryTests(Fixture):
    def test_request_id_is_idempotent_across_best_change(self):
        self.baseline()
        proposal = self.proposal(self.candidate())
        first = self.store.enqueue(proposal)
        self.assertEqual(self.store.enqueue(proposal)["id"], first["id"])
        r.step(self.store)
        self.assertEqual(self.store.enqueue(proposal)["id"], first["id"])
        self.assertEqual(len(self.store.load()["trials"]), 2)
        with self.assertRaisesRegex(r.ResearchError, "replay"):
            self.store.enqueue({**proposal, "hypothesis": "different"})

    def test_no_candidates_before_a_valid_baseline(self):
        self.init()
        with self.assertRaisesRegex(r.ResearchError, "valid baseline"):
            self.store.enqueue(self.proposal(self.candidate()))

    def test_proposal_cannot_override_commands(self):
        self.baseline()
        with self.assertRaises(r.ResearchError):
            self.store.enqueue(self.proposal(self.candidate(), argv=["sh", "-c", "anything"]))

    def test_lineage_and_multiple_parents(self):
        self.baseline()
        first = self.store.enqueue(self.proposal(self.candidate("SCORE = 12\n")))
        r.step(self.store)
        second = self.store.enqueue(self.proposal(self.candidate("SCORE = 7\n"), request_id="second",
                                                  parents=["baseline", first["id"]]))
        self.assertEqual(second["parents"], ["baseline", first["id"]])
        for parents in (["future-id"], ["baseline", "baseline"], []):
            with self.subTest(parents=parents), self.assertRaises(r.ResearchError):
                self.store.enqueue(self.proposal(second["commit"], request_id="bad-parents", parents=parents))

    def test_trial_budget_includes_baseline(self):
        self.manifest["budget"]["max_trials"] = 1
        self.baseline()
        with self.assertRaisesRegex(r.ResearchError, "budget"):
            self.store.enqueue(self.proposal(self.candidate()))

    def test_time_budget_checked_before_claim(self):
        self.baseline()
        self.store.enqueue(self.proposal(self.candidate()))
        with self.store.lock():
            state = self.store.load()
            state["charged_seconds"] = 199
            self.store.save(state)
        with self.assertRaisesRegex(r.ResearchError, "budget"):
            r.step(self.store)
        self.assertEqual(self.store.load()["trials"][1]["status"], "queued")

    def test_status_and_stop_do_not_block_on_writer_lock(self):
        self.init()
        with self.store.lock():
            self.assertEqual(self.cli("status").returncode, 0)
            self.assertEqual(self.cli("stop").returncode, 0)
            result = self.cli("run", "--execute")
            self.assertEqual(result.returncode, 2)
            self.assertIn("busy", result.stderr)
        with self.assertRaisesRegex(r.ResearchError, "stopped"):
            r.step(self.store)
        self.assertEqual(self.cli("resume").returncode, 0)
        self.assertEqual(r.step(self.store)["status"], "baseline")

    def test_recovery_keeps_reservation_and_retry_is_explicit(self):
        state = self.init()
        state["trials"][0].update(status="running", charged_seconds=8)
        state["charged_seconds"] = 8
        self.store.save(state)
        with self.assertRaisesRegex(r.ResearchError, "recover"):
            r.step(self.store)
        r.recover(self.store)
        self.assertEqual(self.store.load()["charged_seconds"], 8)
        attempt = r.retry(self.store, "baseline", "retry-1")
        self.assertEqual(r.retry(self.store, "baseline", "retry-1")["id"], attempt["id"])
        self.assertEqual(attempt["kind"], "baseline")
        self.assertEqual(attempt["parents"], ["baseline"])
        self.assertEqual(r.step(self.store)["status"], "baseline")
        self.assertEqual(self.store.load()["best_id"], attempt["id"])
        self.assertGreater(self.store.load()["charged_seconds"], 8)

    def test_recovery_refuses_live_process_groups(self):
        state = self.init()
        state["trials"][0].update(status="running", active_process_group=os.getpgrp())
        self.store.save(state)
        with self.assertRaisesRegex(r.ResearchError, "alive"):
            r.recover(self.store)
        self.assertEqual(self.store.load()["trials"][0]["status"], "running")

    def test_completed_trials_cannot_be_retried(self):
        self.baseline()
        with self.assertRaises(r.ResearchError):
            r.retry(self.store, "baseline", "retry-invalid")

    def test_export_boundary_is_honest_and_has_no_raw_logs(self):
        self.baseline()
        context = r.export_context(self.store)
        self.assertEqual(context["integration"]["remote_executor"], "not_implemented")
        self.assertFalse(context["integration"]["automatic_merge"])
        self.assertIn("proposal_contract", context)
        self.assertNotIn("stdout", json.dumps(context))
        self.assertNotIn("environ", json.dumps(context))

    def test_stop_interrupts_active_process_and_resumes_without_replay(self):
        self.baseline()
        self.store.enqueue(self.proposal(self.candidate("import time\ntime.sleep(60)\nSCORE = 1\n")))
        process = subprocess.Popen([sys.executable, str(SCRIPT), "--root", str(self.store.root), "run", "--execute"],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(lambda: process.poll() is None and process.kill())
        deadline = time.monotonic() + 10
        group = None
        while time.monotonic() < deadline:
            group = self.store.load()["trials"][1].get("active_process_group")
            if group:
                break
            time.sleep(0.02)
        self.assertIsNotNone(group)
        self.store.stop()
        stdout, stderr = process.communicate(timeout=5)
        self.assertEqual(process.returncode, 130, (stdout, stderr))
        self.assertEqual(self.store.load()["trials"][1]["status"], "interrupted")
        with self.assertRaises(ProcessLookupError):
            os.killpg(group, 0)
        self.assertEqual(self.cli("resume").returncode, 0)
        self.assertEqual(r.step(self.store)["status"], "needs_proposal")


class MetricTests(unittest.TestCase):
    def setUp(self):
        self.manifest = {"objective": {"metric": "loss"}, "guardrails": {"violations": {"max": 0}}}

    def test_invalid_result_shapes_and_values(self):
        cases = [
            {}, {"valid": 1, "metrics": {"loss": 1, "violations": 0}},
            {"valid": False, "metrics": {"loss": 1, "violations": 0}},
            {"valid": True, "metrics": {"loss": True, "violations": 0}},
            {"valid": True, "metrics": {"loss": float("nan"), "violations": 0}},
            {"valid": True, "metrics": {"loss": 1}},
            {"valid": True, "metrics": {"violations": 0}},
            {"valid": True, "metrics": {"loss": 1, "violations": 2}},
            {"valid": True, "metrics": {"loss": 1, "violations": 0}, "override_budget": 100},
        ]
        for result in cases:
            with self.subTest(result=result), self.assertRaises(r.ResearchError):
                r.validate_metrics(result, self.manifest)

    def test_seed_mismatch_and_min_delta(self):
        objective = {"metric": "loss", "direction": "min", "min_delta": 0.5}
        base = [{"seed": 1, "metrics": {"loss": 10}}]
        self.assertFalse(r.improves([{"seed": 1, "metrics": {"loss": 9.5}}], base, objective))
        self.assertTrue(r.improves([{"seed": 1, "metrics": {"loss": 9}}], base, objective))
        with self.assertRaises(r.ResearchError):
            r.improves([{"seed": 2, "metrics": {"loss": 1}}], base, objective)


if __name__ == "__main__":
    unittest.main()
