#!/usr/bin/env python3
"""Versioned jagent handoff for the research controller; no model or cluster IO.

The legacy engine remains the sole evaluator. This adapter adds bounded context
and atomic context-bound admission without monkey-patching its execution logic.
"""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import types
import uuid

CONTEXT_PROTOCOL = "jsh.research.bridge/v1"
IDENTITY_PROTOCOL = "jsh.research.identity/v1"
MAX_CONTEXT = 256 * 1024
MAX_PROPOSAL = 32 * 1024
RECENT_TRIALS = 16
PROPOSAL_FIELDS = {"schema_version", "campaign_id", "manifest_digest", "expected_revision",
                   "expected_best_commit", "request_id", "commit", "hypothesis", "parents"}


def load_engine(source: str | None = None):
    if source is not None:
        module = types.ModuleType("jsh_research_embedded")
        sys.modules[module.__name__] = module
        exec(compile(source, "<embedded-jsh-research>", "exec"), module.__dict__)
        return module
    path = Path(__file__).with_name("jsh_research.py")
    spec = importlib.util.spec_from_file_location("jsh_research", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class Bridge:
    def __init__(self, engine, root: Path):
        self.r = engine
        self.store = engine.Store(root)
        self.identity_path = self.store.root / "bridge-identity.json"

    def identity(self, state, create=False):
        r = self.r
        if create and not os.path.lexists(self.identity_path):
            r.atomic_json(self.identity_path, {"protocol": IDENTITY_PROTOCOL,
                "campaign_id": uuid.uuid4().hex, "manifest_digest": state["manifest_digest"]})
        identity = r.read_json(self.identity_path, 4096)
        r.fields(identity, {"protocol", "campaign_id", "manifest_digest"}, set(), "bridge identity")
        r.require(identity["protocol"] == IDENTITY_PROTOCOL, "unsupported bridge identity")
        r.require(isinstance(identity["campaign_id"], str) and
                  re.fullmatch(r"[0-9a-f]{32}", identity["campaign_id"]) is not None,
                  "invalid campaign identity")
        r.require(identity["manifest_digest"] == state["manifest_digest"], "bridge manifest mismatch")
        return identity

    def snapshot(self):
        # Only initial identity creation needs the writer lock. Thereafter an
        # atomic state snapshot is readable even while a long evaluation runs.
        if not os.path.lexists(self.identity_path):
            with self.store.lock():
                state = self.store.load()
                identity = self.identity(state, create=True)
                return self.context(state, identity)
        state = self.store.load()
        return self.context(state, self.identity(state))

    def context(self, state, identity):
        r, store = self.r, self.store
        manifest, trials = state["manifest"], state["trials"]
        budget = manifest["budget"]
        r.require(1 <= len(trials) <= budget["max_trials"] <= 10000, "invalid trial accounting")
        r.require(type(state["revision"]) is int and 1 <= state["revision"] < 2**64,
                  "invalid state revision")
        best = next((trial for trial in trials if trial["id"] == state["best_id"]), None)
        r.require(state["best_id"] is None or
                  (best is not None and best["status"] in ("baseline", "kept")),
                  "invalid best-candidate pointer")
        reservation = sum(len(manifest[stage]["seeds"]) for stage in ("screen", "confirm")) * budget["timeout_seconds"]
        enough_time = state["charged_seconds"] + reservation <= budget["max_seconds"]
        if store.stopped():
            action = "stopped"
        elif any(trial["status"] == "running" for trial in trials):
            action = "needs_recovery"
            # Running is intentionally not permission to launch another worker.
            # A host should inspect status; recover itself fails while locked.
        elif not enough_time:
            action = "budget_exhausted"
        elif any(trial["status"] == "queued" for trial in trials):
            action = "evaluate_queued"
        elif len(trials) >= budget["max_trials"]:
            action = "budget_exhausted"
        elif best is None:
            action = "needs_baseline_retry"
        else:
            action = "needs_proposal"
        recent = []
        ids = set()
        for trial in trials[-RECENT_TRIALS:]:
            trial_id = trial["id"]
            r.require(isinstance(trial_id, str) and
                      re.fullmatch(r"baseline|e[0-9]{6,15}", trial_id) is not None and trial_id not in ids,
                      "invalid or duplicate trial ID")
            ids.add(trial_id)
            r.require(trial["status"] in r.TERMINAL | {"queued", "running"}, "invalid trial status")
            hypothesis = r.text(trial["hypothesis"], "hypothesis", 8192)
            hypothesis = hypothesis.encode("utf-8")[:2048].decode("utf-8", "ignore")
            if not hypothesis.strip():
                hypothesis = "(empty hypothesis in legacy record)"
            scores = {stage: r.mean_score(results, manifest["objective"]["metric"])
                      for stage, results in trial["results"].items()
                      if stage in ("screen", "confirm") and len(results) == len(manifest[stage]["seeds"])}
            recent.append({"id": trial_id, "commit": trial["commit"], "hypothesis": hypothesis,
                           "status": trial["status"], "parents": trial["parents"], "scores": scores})
        for key in ("editable_paths", "protected_paths"):
            r.require(len(manifest[key]) <= 128, "bridge supports at most 128 path prefixes per list")
        context = {"protocol": CONTEXT_PROTOCOL, "campaign_id": identity["campaign_id"],
                   "manifest_digest": state["manifest_digest"], "revision": state["revision"],
                   "next_action": action, "baseline_commit": manifest["baseline"],
                   "best": {"id": best["id"], "commit": best["commit"]} if best else None,
                   "objective": manifest["objective"], "editable_paths": manifest["editable_paths"],
                   "protected_paths": manifest["protected_paths"],
                   "budget": {**{key: budget[key] for key in ("max_trials", "max_seconds")},
                              "trials_used": len(trials), "charged_seconds": state["charged_seconds"],
                              "trial_reservation_seconds": reservation},
                   "recent_trials": recent, "omitted_trials": len(trials) - len(recent)}
        r.require(len(r.canonical(context)) + 1 <= MAX_CONTEXT, "bridge context exceeds 256 KiB")
        return context

    def validate_shape(self, proposal):
        r = self.r
        r.fields(proposal, PROPOSAL_FIELDS, set(), "bound proposal")
        r.require(len(r.canonical(proposal)) <= MAX_PROPOSAL, "proposal exceeds 32 KiB")
        r.require(type(proposal["schema_version"]) is int and proposal["schema_version"] == 2,
                  "submit requires proposal schema_version=2; legacy enqueue is separate")
        for key, pattern in (("campaign_id", r"[0-9a-f]{32}"), ("manifest_digest", r"[0-9a-f]{64}"),
                             ("commit", r"(?:[0-9a-f]{40}|[0-9a-f]{64})"),
                             ("expected_best_commit", r"(?:[0-9a-f]{40}|[0-9a-f]{64})")):
            r.require(isinstance(proposal[key], str) and re.fullmatch(pattern, proposal[key]) is not None,
                      f"invalid {key}")
        request_id = proposal["request_id"]
        r.require(isinstance(request_id, str) and request_id != "baseline" and
                  re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}", request_id) is not None,
                  "invalid request ID")
        revision = proposal["expected_revision"]
        r.require(type(revision) is int and 1 <= revision < 2**64, "invalid expected revision")
        hypothesis = r.text(proposal["hypothesis"], "hypothesis", 8192)
        r.require(hypothesis.strip() and len(hypothesis.encode("utf-8")) <= 8192, "invalid hypothesis byte length")
        parents = proposal["parents"]
        r.require(isinstance(parents, list) and 1 <= len(parents) <= 8 and
                  all(isinstance(parent, str) and re.fullmatch(r"baseline|e[0-9]{6,15}", parent)
                      for parent in parents), "invalid parent IDs")
        r.require(len(set(parents)) == len(parents), "duplicate parent IDs")

    def submit(self, proposal):
        r, store = self.r, self.store
        self.validate_shape(proposal)
        request_digest = r.digest(proposal)
        with store.lock():
            state = store.load()
            identity = self.identity(state)
            r.require(proposal["campaign_id"] == identity["campaign_id"] and
                      proposal["manifest_digest"] == state["manifest_digest"], "foreign campaign or manifest")
            # A replay is a read of the previous admission, even after state
            # advances or stops. Never rebind, rerun or consume another slot.
            for trial in state["trials"]:
                if trial["request_id"] == proposal["request_id"]:
                    r.require(trial.get("proposal_schema") == 2 and
                              trial.get("request_digest") == request_digest,
                              "request_id replay has different content or schema")
                    return trial
            context = self.context(state, identity)
            r.require(context["next_action"] == "needs_proposal", "controller is not requesting a proposal")
            r.require(proposal["expected_revision"] == state["revision"] and
                      proposal["expected_best_commit"] == context["best"]["commit"],
                      "stale proposal; obtain a fresh snapshot and reassess, do not silently rebind")
            known = {trial["id"] for trial in context["recent_trials"] if trial["status"] in r.TERMINAL}
            known.add(context["best"]["id"])
            r.require(set(proposal["parents"]) <= known, "parents are not visible completed research trials")
            sha = r.commit_id(Path(state["manifest"]["repo"]), proposal["commit"])
            r.require(sha == proposal["commit"], "commit must be an exact full object ID")
            r.check_candidate(state["manifest"], sha)
            trial = {"id": f"e{len(state['trials']):06d}", "request_id": proposal["request_id"],
                     "request_digest": request_digest, "proposal_schema": 2,
                     "bridge_binding": {key: proposal[key] for key in
                         ("campaign_id", "manifest_digest", "expected_revision", "expected_best_commit")},
                     "commit": sha, "hypothesis": proposal["hypothesis"], "parents": proposal["parents"],
                     "kind": "candidate", "status": "queued", "results": {}, "charged_seconds": 0.0}
            state["trials"].append(trial)
            store.save(state)
            return trial

    def resume(self):
        # Persist the new epoch BEFORE clearing STOP. A failed write/crash must
        # not reopen the campaign with its old, still-valid proposal bindings.
        with self.store.lock():
            state = self.store.load()
            self.store.save(state)
            (self.store.root / "STOP").unlink(missing_ok=True)
        return {"protocol": self.r.VERSION, "stopped": False, "revision": state["revision"]}


def main(argv=None, engine=None):
    r = engine or load_engine()
    cli = r.parser()
    # Extend the same parser, but delegate all evaluation to the original engine.
    subparsers = next(action for action in cli._actions if hasattr(action, "add_parser"))
    subparsers.add_parser("bridge", help="bounded campaign context for jagent; no raw logs")
    subparsers.add_parser("submit", help="atomically admit a version-2 context-bound proposal").add_argument("proposal", type=Path)
    cli.epilog = "Native jagent helpers: jsh research prepare-request CONTEXT; jsh research validate-proposal CONTEXT PROPOSAL."
    args = cli.parse_args(argv)
    if args.command not in ("bridge", "submit", "resume"):
        return r.main(argv)
    try:
        bridge = Bridge(r, args.root)
        if args.command == "bridge":
            result = bridge.snapshot()
        elif args.command == "submit":
            result = bridge.submit(r.read_json(args.proposal, MAX_PROPOSAL))
        else:
            result = bridge.resume()
        # Compact encoding makes the emitted byte bound match the parser bound.
        sys.stdout.write(r.canonical(result).decode("utf-8") + "\n")
        return 0
    except (r.ResearchError, OSError, subprocess.SubprocessError, ValueError, KeyError, TypeError) as error:
        print(json.dumps({"protocol": CONTEXT_PROTOCOL, "error": str(error)[:4096]}), file=sys.stderr)
        return 2


if __name__ == "__main__":
    arguments = sys.argv[1:]
    embedded = None
    if arguments[:1] == ["--embedded-engine"]:
        embedded = load_engine(arguments[1])
        arguments = arguments[2:]

    def terminated(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminated)
    try:
        sys.exit(main(arguments, embedded))
    except KeyboardInterrupt:
        sys.exit(130)
