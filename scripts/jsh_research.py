#!/usr/bin/env python3
"""Bounded, model-independent research controller for jsh (Python 3.10+, Unix).

This is an experiment orchestrator, NOT a security sandbox. Only execute code
and evaluators you trust, or run the entire controller inside a disposable VM.
The installed Rust binary embeds this file; no Python packages are required.
"""
from __future__ import annotations

import argparse
import contextlib
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import time
from typing import Any

VERSION = "jsh.research/v1"
MAX_JSON = 1024 * 1024
MAX_STATE = 32 * MAX_JSON
MAX_LOG = 8 * MAX_JSON
TERMINAL = {"baseline", "kept", "archived", "screened_out", "invalid", "failed", "interrupted"}


class ResearchError(Exception):
    """An actionable, user-facing configuration or execution error."""


class Stopped(ResearchError):
    pass


class InvalidResult(ResearchError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ResearchError(message)


def canonical(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=True, allow_nan=False).encode("utf-8")


def digest(value: Any) -> str:
    return hashlib.sha256(canonical(value)).hexdigest()


def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    obj: dict[str, Any] = {}
    for key, value in pairs:
        require(key not in obj, f"duplicate JSON key: {key}")
        obj[key] = value
    return obj


def invalid_constant(value: str) -> None:
    raise ResearchError(f"non-finite JSON number: {value}")


def read_json(path: Path, limit: int = MAX_JSON) -> Any:
    # No pipes, devices, symlinks, duplicate fields, oversized files or NaNs.
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        require(stat.S_ISREG(os.fstat(stream.fileno()).st_mode), f"not a regular file: {path}")
        data = stream.read(limit + 1)
    require(len(data) <= limit, f"JSON exceeds {limit} bytes: {path}")
    try:
        return json.loads(data, object_pairs_hook=unique_object, parse_constant=invalid_constant)
    except (ValueError, UnicodeError, RecursionError) as error:
        raise ResearchError(f"invalid JSON in {path}: {error}") from error


def atomic_json(path: Path, value: Any) -> None:
    data = canonical(value) + b"\n"
    require(len(data) <= MAX_STATE, "research state exceeds 32 MiB; start a new campaign")
    # The writer holds the campaign lock. O_EXCL prevents following a stale link.
    temp = path.with_name(path.name + f".tmp-{os.getpid()}-{time.time_ns()}")
    try:
        fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temp, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        temp.unlink(missing_ok=True)


def text(value: Any, name: str, maximum: int = 4096) -> str:
    require(isinstance(value, str) and 0 < len(value) <= maximum and "\x00" not in value,
            f"{name} must be a nonempty string of at most {maximum} characters")
    return value


def number(value: Any, name: str, minimum: float | None = None) -> float:
    require(type(value) in (int, float), f"{name} must be numeric, not a boolean")
    try:
        result = float(value)
    except (ValueError, OverflowError) as error:
        raise ResearchError(f"{name} is outside the supported numeric range") from error
    require(math.isfinite(result) and abs(result) <= 1e100, f"{name} must be finite and within +/-1e100")
    require(minimum is None or result >= minimum, f"{name} must be >= {minimum}")
    return result


def fields(obj: Any, required: set[str], optional: set[str], name: str) -> None:
    require(isinstance(obj, dict), f"{name} must be a JSON object")
    require(required <= obj.keys(), f"missing {name} fields: {sorted(required - obj.keys())}")
    require(obj.keys() <= required | optional,
            f"unknown {name} fields: {sorted(obj.keys() - required - optional)}")


def git(repo: Path, *args: str, timeout: float = 30) -> str:
    result = subprocess.run(
        ["git", "-c", "core.hooksPath=/dev/null", "-C", str(repo), *args],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        timeout=timeout, check=False,
    )
    require(result.returncode == 0,
            f"git {args[0]} failed: {result.stderr.decode('utf-8', 'replace')[:2000].strip()}")
    output = result.stdout.decode("utf-8", "strict")
    return output if "-z" in args else output.rstrip("\n")


def commit_id(repo: Path, ref: Any) -> str:
    ref = text(ref, "commit", 256)
    require(not ref.startswith("-"), "commit must not start with '-' ")
    sha = git(repo, "rev-parse", "--verify", "--end-of-options", ref + "^{commit}")
    require(re.fullmatch(r"[0-9a-f]{40,64}", sha) is not None, "unexpected Git commit ID")
    return sha


def path_prefix(value: Any) -> str:
    value = text(value, "repository path", 1024).rstrip("/")
    require(value and not value.startswith("/") and "\\" not in value,
            "repository paths must be relative POSIX paths")
    require(all(part not in ("", ".", "..", ".git") for part in value.split("/")),
            "repository paths must not contain empty, '.', '..' or '.git' components")
    require(not any(ch in value for ch in ("\n", "\r", "\t", ":", "*", "?", "[")),
            "repository paths are literal prefixes, not patterns")
    return value


def within(path: str, prefix: str) -> bool:
    return path == prefix or path.startswith(prefix + "/")


def evaluation(value: Any, name: str) -> dict[str, Any]:
    fields(value, {"argv", "seeds"}, set(), name)
    argv = value["argv"]
    require(isinstance(argv, list) and 0 < len(argv) <= 128, f"{name}.argv must be a nonempty argument array")
    require(all(isinstance(arg, str) and "\x00" not in arg and len(arg) <= 8192 for arg in argv),
            f"invalid {name}.argv")
    require(bool(argv[0]), f"{name}.argv[0] must be an executable")
    seeds = value["seeds"]
    require(isinstance(seeds, list) and 1 <= len(seeds) <= 32, f"{name} needs 1..32 seeds")
    require(all(type(seed) is int and 0 <= seed < 2**32 for seed in seeds), f"invalid {name} seed")
    require(len(set(seeds)) == len(seeds), f"duplicate {name} seeds")
    return {"argv": argv, "seeds": seeds}


def normalize_manifest(raw: Any, relative_to: Path) -> dict[str, Any]:
    fields(raw, {"schema_version", "name", "repo", "baseline", "objective", "screen", "confirm",
                 "protected_paths", "editable_paths", "budget"}, {"guardrails", "dataset_id"}, "manifest")
    require(type(raw["schema_version"]) is int and raw["schema_version"] == 1, "unsupported manifest version")
    name = text(raw["name"], "name", 128)
    repo = Path(text(raw["repo"], "repo")).expanduser()
    if not repo.is_absolute():
        repo = relative_to / repo
    repo = Path(git(repo.resolve(), "rev-parse", "--show-toplevel")).resolve()
    baseline = commit_id(repo, raw["baseline"])
    objective = raw["objective"]
    fields(objective, {"metric", "direction"}, {"min_delta"}, "objective")
    metric = text(objective["metric"], "objective.metric", 128)
    require(objective["direction"] in ("min", "max"), "objective.direction must be min or max")
    delta = number(objective.get("min_delta", 0), "objective.min_delta", 0)
    screen, confirm = evaluation(raw["screen"], "screen"), evaluation(raw["confirm"], "confirm")
    require(not set(screen["seeds"]) & set(confirm["seeds"]), "screen and confirmation seeds must be disjoint")
    prefixes: dict[str, list[str]] = {}
    for key in ("protected_paths", "editable_paths"):
        require(isinstance(raw[key], list) and raw[key], f"{key} must be a nonempty array")
        prefixes[key] = sorted(set(path_prefix(value) for value in raw[key]))
    require(not any(within(a, b) or within(b, a) for a in prefixes["protected_paths"]
                    for b in prefixes["editable_paths"]), "protected and editable prefixes must not overlap")
    for path in prefixes["protected_paths"]:
        git(repo, "rev-parse", "--verify", baseline + ":" + path)
    budget = raw["budget"]
    fields(budget, {"max_trials", "max_seconds", "timeout_seconds"}, set(), "budget")
    require(type(budget["max_trials"]) is int and 1 <= budget["max_trials"] <= 10000,
            "max_trials must be an integer in 1..10000 (including baseline)")
    budget = {"max_trials": budget["max_trials"],
              "max_seconds": number(budget["max_seconds"], "max_seconds", 0.1),
              "timeout_seconds": number(budget["timeout_seconds"], "timeout_seconds", 0.1)}
    reservation = (len(screen["seeds"]) + len(confirm["seeds"])) * budget["timeout_seconds"]
    require(reservation <= budget["max_seconds"], "budget must cover one complete trial's timeout reservation")
    guardrails = raw.get("guardrails", {})
    require(isinstance(guardrails, dict) and len(guardrails) <= 128, "guardrails must be a metric-to-bounds object")
    normalized_guards: dict[str, Any] = {}
    for key, bounds in guardrails.items():
        text(key, "guardrail metric", 128)
        fields(bounds, set(), {"min", "max"}, f"guardrail {key}")
        require(bool(bounds), f"guardrail {key} needs a bound")
        normalized_guards[key] = {side: number(value, f"{key}.{side}") for side, value in bounds.items()}
        require(normalized_guards[key].get("min", -1e100) <= normalized_guards[key].get("max", 1e100),
                f"guardrail {key} has reversed bounds")
    dataset = raw.get("dataset_id", "unspecified")
    text(dataset, "dataset_id", 1024)
    return {"schema_version": 1, "name": name, "repo": str(repo), "baseline": baseline,
            "objective": {"metric": metric, "direction": objective["direction"], "min_delta": delta},
            "screen": screen, "confirm": confirm, "budget": budget, "guardrails": normalized_guards,
            "dataset_id": dataset, **prefixes}


class Store:
    def __init__(self, root: Path):
        # Do not silently follow an existing campaign-directory symlink.
        root = root.expanduser().absolute()
        require(not root.is_symlink(), "campaign directory must not be a symlink")
        self.root = root
        self.state_path = root / "state.json"

    @contextlib.contextmanager
    def lock(self, create: bool = False):
        if create:
            self.root.mkdir(mode=0o700, parents=True, exist_ok=True)
        require(self.root.is_dir() and not self.root.is_symlink(), "campaign not initialized")
        fd = os.open(self.root / ".lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        try:
            require(stat.S_ISREG(os.fstat(fd).st_mode), "invalid campaign lock")
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise ResearchError("campaign writer is busy; status/export/stop remain available") from error
            yield
        finally:
            os.close(fd)

    def load(self) -> dict[str, Any]:
        state = read_json(self.state_path, MAX_STATE)
        fields(state, {"protocol", "revision", "manifest", "manifest_digest", "best_id",
                       "charged_seconds", "trials"}, set(), "state")
        require(state["protocol"] == VERSION, "unsupported campaign state")
        require(type(state["revision"]) is int and state["revision"] >= 1, "invalid state revision")
        number(state["charged_seconds"], "charged_seconds", 0)
        require(isinstance(state["trials"], list) and state["trials"], "missing trial records")
        require(digest(state["manifest"]) == state["manifest_digest"], "frozen manifest digest mismatch")
        return state

    def save(self, state: dict[str, Any]) -> None:
        state["revision"] += 1
        atomic_json(self.state_path, state)

    def initialize(self, raw: Any, relative_to: Path) -> dict[str, Any]:
        manifest = normalize_manifest(raw, relative_to)
        with self.lock(create=True):
            require(not os.path.lexists(self.state_path), "campaign already exists; initialization never overwrites it")
            state = {"protocol": VERSION, "revision": 0, "manifest": manifest,
                     "manifest_digest": digest(manifest), "best_id": None, "charged_seconds": 0.0,
                     "trials": [{"id": "baseline", "request_id": "baseline", "commit": manifest["baseline"],
                                 "hypothesis": "Frozen baseline", "kind": "baseline", "parents": [], "status": "queued",
                                 "results": {}, "charged_seconds": 0.0}]}
            self.save(state)
            return state

    def stopped(self) -> bool:
        return os.path.lexists(self.root / "STOP")

    def stop(self) -> None:
        require(self.state_path.is_file(), "campaign not initialized")
        try:
            fd = os.open(self.root / "STOP", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        except FileExistsError:
            return
        with os.fdopen(fd, "w") as stream:
            stream.write("Stop requested. Use resume to allow subsequent work.\n")

    def enqueue(self, proposal: Any) -> dict[str, Any]:
        fields(proposal, {"schema_version", "request_id", "commit", "hypothesis"}, {"parents"}, "proposal")
        require(type(proposal["schema_version"]) is int and proposal["schema_version"] == 1,
                "unsupported proposal version")
        request_id = text(proposal["request_id"], "request_id", 128)
        require(request_id != "baseline", "baseline is a reserved request ID")
        hypothesis = text(proposal["hypothesis"], "hypothesis", 8192)
        with self.lock():
            state = self.load()
            manifest = state["manifest"]
            sha = commit_id(Path(manifest["repo"]), proposal["commit"])
            # Hash the submitted parent list, not a changing implicit best_id.
            request = {**proposal, "commit": sha}
            request_hash = digest(request)
            for trial in state["trials"]:
                if trial["request_id"] == request_id:
                    require(trial.get("request_digest") == request_hash, "request_id replay has different content")
                    return trial
            require(state["best_id"] is not None, "run a valid baseline before proposing candidates")
            require(len(state["trials"]) < manifest["budget"]["max_trials"], "trial budget exhausted")
            parents = proposal.get("parents", [state["best_id"]])
            require(isinstance(parents, list) and 1 <= len(parents) <= 8 and
                    all(isinstance(parent, str) for parent in parents), "parents must contain 1..8 trial IDs")
            require(len(set(parents)) == len(parents), "duplicate parent IDs")
            known = {trial["id"] for trial in state["trials"] if trial["status"] in TERMINAL}
            require(set(parents) <= known, "parents must reference completed existing trials")
            check_candidate(manifest, sha)
            trial = {"id": f"e{len(state['trials']):06d}", "request_id": request_id,
                     "request_digest": request_hash, "commit": sha, "hypothesis": hypothesis,
                     "parents": parents, "kind": "candidate", "status": "queued", "results": {}, "charged_seconds": 0.0}
            state["trials"].append(trial)
            self.save(state)
            return trial


def retry(store: Store, trial_id: str, request_id: str) -> dict[str, Any]:
    text(request_id, "request_id", 128)
    require(request_id != "baseline", "baseline is a reserved request ID")
    request_hash = digest({"retry_of": trial_id, "request_id": request_id})
    with store.lock():
        state = store.load()
        for item in state["trials"]:
            if item["request_id"] == request_id:
                require(item.get("request_digest") == request_hash, "request_id replay has different content")
                return item
        source = next((item for item in state["trials"] if item["id"] == trial_id), None)
        require(source is not None and source["status"] in ("failed", "invalid", "interrupted"),
                "only failed, invalid or interrupted trials can be retried")
        require(len(state["trials"]) < state["manifest"]["budget"]["max_trials"], "trial budget exhausted")
        require(source["kind"] != "baseline" or state["best_id"] is None, "a valid baseline already exists")
        require(source["kind"] != "baseline" or not any(
            item["kind"] == "baseline" and item["status"] in ("queued", "running")
            for item in state["trials"]), "baseline attempt already pending")
        trial = {key: source[key] for key in ("commit", "hypothesis", "kind")}
        trial.update(id=f"e{len(state['trials']):06d}", request_id=request_id, request_digest=request_hash,
                     parents=[source["id"]], retry_of=source["id"], status="queued", results={}, charged_seconds=0.0)
        state["trials"].append(trial)
        store.save(state)
        return trial


def check_candidate(manifest: dict[str, Any], sha: str) -> None:
    repo, base = Path(manifest["repo"]), manifest["baseline"]
    # No shell, globs, external diff drivers or rename heuristics.
    changes = git(repo, "diff", "--no-ext-diff", "--no-renames", "--name-only", "-z", base, sha, "--")
    for path in changes.split("\x00"):
        if not path:
            continue
        require(any(within(path, allowed) for allowed in manifest["editable_paths"]),
                f"candidate changes a non-editable path: {path}")
    for path in manifest["protected_paths"]:
        original = git(repo, "rev-parse", "--verify", base + ":" + path)
        proposed = git(repo, "rev-parse", "--verify", sha + ":" + path)
        require(original == proposed, f"candidate changes protected evaluator/data: {path}")


def validate_metrics(raw: Any, manifest: dict[str, Any]) -> dict[str, float]:
    fields(raw, {"valid", "metrics"}, set(), "evaluation result")
    require(raw["valid"] is True, "evaluator did not explicitly attest valid=true")
    metrics = raw["metrics"]
    require(isinstance(metrics, dict) and 1 <= len(metrics) <= 128, "metrics must be a nonempty object")
    metrics = {text(key, "metric name", 128): number(value, f"metric {key}") for key, value in metrics.items()}
    require(manifest["objective"]["metric"] in metrics, "objective metric is missing")
    for key, bounds in manifest["guardrails"].items():
        require(key in metrics, f"guardrail metric missing: {key}")
        require(("min" not in bounds or metrics[key] >= bounds["min"]) and
                ("max" not in bounds or metrics[key] <= bounds["max"]), f"guardrail failed: {key}")
    return metrics


def mean_score(results: list[dict[str, Any]], metric: str) -> float:
    require(bool(results), "cannot score an empty evaluation")
    return math.fsum(result["metrics"][metric] / len(results) for result in results)


def improves(candidate: list[dict[str, Any]], incumbent: list[dict[str, Any]], objective: dict[str, Any]) -> bool:
    require([item["seed"] for item in candidate] == [item["seed"] for item in incumbent],
            "cannot compare different seed sets")
    sign = 1 if objective["direction"] == "max" else -1
    metric = objective["metric"]
    return sign * (mean_score(candidate, metric) - mean_score(incumbent, metric)) > objective["min_delta"]


def kill_group(process: subprocess.Popen) -> None:
    # The runner created this session. Never kill arbitrary persisted PIDs.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


def run_seed(store: Store, state: dict[str, Any], trial: dict[str, Any], stage: str, seed: int, deadline: float) -> dict[str, Any]:
    manifest = state["manifest"]
    if store.stopped():
        raise Stopped("stop requested")
    remaining = deadline - time.monotonic()
    require(remaining > 0, "trial deadline reached")
    directory = store.root / "trials" / trial["id"] / f"{stage}-{seed}"
    directory.mkdir(mode=0o700, parents=True, exist_ok=False)
    worktree = directory / "worktree"
    git(Path(manifest["repo"]), "worktree", "add", "--detach", str(worktree), trial["commit"], timeout=min(30, remaining))
    result_path = directory / "result.json"
    env = os.environ.copy()
    env.update({"JSH_RESEARCH_RESULT": str(result_path), "JSH_RESEARCH_SEED": str(seed),
                "JSH_RESEARCH_STAGE": stage, "JSH_RESEARCH_TRIAL": trial["id"],
                "JSH_RESEARCH_MANIFEST_SHA256": state["manifest_digest"]})
    request = {"protocol": VERSION, "trial_id": trial["id"], "commit": trial["commit"],
               "manifest_digest": state["manifest_digest"], "stage": stage, "seed": seed,
               "argv": manifest[stage]["argv"], "cwd": str(worktree), "result_path": str(result_path),
               "timeout_seconds": min(manifest["budget"]["timeout_seconds"], deadline - time.monotonic())}
    require(request["timeout_seconds"] > 0, "trial deadline reached during checkout")
    atomic_json(directory / "request.json", request)
    started = time.monotonic()
    with (directory / "stdout.log").open("xb") as stdout, (directory / "stderr.log").open("xb") as stderr:
        process = subprocess.Popen(request["argv"], cwd=worktree, env=env, stdin=subprocess.DEVNULL,
                                   stdout=stdout, stderr=stderr, start_new_session=True)
        try:
            trial["active_process_group"] = process.pid
            store.save(state)
            while process.poll() is None:
                if store.stopped():
                    raise Stopped("stop requested")
                require(time.monotonic() - started <= request["timeout_seconds"], "evaluation timed out")
                require(os.fstat(stdout.fileno()).st_size <= MAX_LOG and os.fstat(stderr.fileno()).st_size <= MAX_LOG,
                        "evaluation log limit exceeded")
                time.sleep(0.05)
            if store.stopped():
                raise Stopped("stop requested")
            require(time.monotonic() - started <= request["timeout_seconds"], "evaluation timed out")
            require(process.returncode == 0, f"evaluation exited with status {process.returncode}")
            require(os.fstat(stdout.fileno()).st_size <= MAX_LOG and os.fstat(stderr.fileno()).st_size <= MAX_LOG,
                    "evaluation log limit exceeded")
        finally:
            kill_group(process)
            trial.pop("active_process_group", None)
            # Burst writers may overshoot the polling limit; cap retained logs.
            stdout.truncate(min(os.fstat(stdout.fileno()).st_size, MAX_LOG))
            stderr.truncate(min(os.fstat(stderr.fileno()).st_size, MAX_LOG))
    try:
        require(commit_id(worktree, "HEAD") == trial["commit"], "evaluation changed HEAD")
        require(not git(worktree, "diff", "--no-ext-diff", "--no-renames", "--name-only", "-z", trial["commit"], "--"),
                "evaluation modified tracked source or evaluator files")
        metrics = validate_metrics(read_json(result_path), manifest)
    except (ResearchError, OSError) as error:
        raise InvalidResult(str(error)) from error
    result = {"seed": seed, "metrics": metrics, "elapsed_seconds": time.monotonic() - started,
              "evidence": str(directory.relative_to(store.root)),
              "result_sha256": hashlib.sha256(canonical({"valid": True, "metrics": metrics})).hexdigest()}
    atomic_json(directory / "receipt.json", {**request, **result})
    return result


def step(store: Store, expected_trial_id: str | None = None) -> dict[str, Any]:
    with store.lock():
        state = store.load()
        require(not store.stopped(), "campaign is stopped; use resume")
        require(not any(trial["status"] == "running" for trial in state["trials"]),
                "interrupted controller detected; use recover before running again")
        trial = next((trial for trial in state["trials"] if trial["status"] == "queued"), None)
        # Check identity under the same writer lock as budget reservation/launch.
        # A concurrent manual controller must not redirect an auto attempt.
        if expected_trial_id is not None:
            require(trial is not None and trial["id"] == expected_trial_id,
                    "queue head changed; refusing unrelated trial")
        if trial is None:
            return {"protocol": VERSION, "status": "needs_proposal", "best_id": state["best_id"]}
        manifest = state["manifest"]
        require(trial["kind"] == "baseline" or state["best_id"] is not None, "no valid baseline")
        check_candidate(manifest, trial["commit"])
        reservation = sum(len(manifest[stage]["seeds"]) for stage in ("screen", "confirm")) * manifest["budget"]["timeout_seconds"]
        require(state["charged_seconds"] + reservation <= manifest["budget"]["max_seconds"],
                "time budget cannot cover the next trial's timeout reservation")
        trial.update(status="running", charged_seconds=reservation, started_unix=time.time())
        state["charged_seconds"] += reservation
        store.save(state)  # Charge before launching; crashes cannot reset the budget.
        started = time.monotonic()
        interrupted = False
        try:
            incumbent = next((item for item in state["trials"] if item["id"] == state["best_id"]), None)
            trial["compared_to"] = state["best_id"]
            for stage in ("screen", "confirm"):
                trial["results"][stage] = []
                for seed in manifest[stage]["seeds"]:
                    trial["results"][stage].append(run_seed(store, state, trial, stage, seed, started + reservation))
                    store.save(state)
                if incumbent and not improves(trial["results"][stage], incumbent["results"][stage], manifest["objective"]):
                    trial["status"] = "screened_out" if stage == "screen" else "archived"
                    trial["reason"] = f"no {stage} mean improvement beyond min_delta"
                    break
            else:
                trial["status"] = "baseline" if trial["kind"] == "baseline" else "kept"
                state["best_id"] = trial["id"]
        except (KeyboardInterrupt, Stopped):
            trial.update(status="interrupted", reason="execution interrupted; no result promoted")
            interrupted = True
        except InvalidResult as error:
            trial.update(status="invalid", reason=str(error)[:4096])
        except (OSError, subprocess.SubprocessError, ResearchError) as error:
            trial.update(status="failed", reason=str(error)[:4096])
        finally:
            elapsed = time.monotonic() - started
            state["charged_seconds"] += elapsed - reservation
            trial["charged_seconds"] = elapsed
            trial["finished_unix"] = time.time()
            trial.pop("active_process_group", None)
            store.save(state)
        if interrupted:
            raise Stopped("execution interrupted; state persisted")
        return trial


def recover(store: Store) -> dict[str, Any]:
    with store.lock():
        state = store.load()
        for trial in state["trials"]:
            if trial["status"] != "running":
                continue
            group = trial.get("active_process_group")
            if group:
                try:
                    os.killpg(group, 0)
                except ProcessLookupError:
                    pass
                else:
                    raise ResearchError(f"process group {group} may still be alive; inspect/stop it before recovery")
            trial.update(status="interrupted", reason="controller recovery; full timeout reservation retained")
            trial.pop("active_process_group", None)
        store.save(state)
        return summary(store, state)


def summary(store: Store, state: dict[str, Any]) -> dict[str, Any]:
    manifest = state["manifest"]
    trials = []
    for trial in state["trials"]:
        item = {key: trial[key] for key in ("id", "request_id", "kind", "commit", "hypothesis", "parents", "status")}
        item["scores"] = {stage: mean_score(results, manifest["objective"]["metric"])
                          for stage, results in trial["results"].items() if results}
        item["complete_stages"] = [stage for stage, results in trial["results"].items()
                                   if len(results) == len(manifest[stage]["seeds"])]
        if "reason" in trial:
            item["reason"] = trial["reason"]
        trials.append(item)
    return {"protocol": VERSION, "revision": state["revision"], "name": manifest["name"],
            "manifest_digest": state["manifest_digest"], "best_id": state["best_id"],
            "stopped": store.stopped(), "charged_seconds": state["charged_seconds"],
            "budget": manifest["budget"], "objective": manifest["objective"], "trials": trials}


def export_context(store: Store) -> dict[str, Any]:
    state = store.load()
    return {**summary(store, state), "manifest": state["manifest"],
            "proposal_contract": {"schema_version": 1, "request_id": "unique-id-for-idempotent-retries",
                                  "commit": "existing Git commit", "hypothesis": "falsifiable hypothesis",
                                  "parents": [state["best_id"]] if state["best_id"] else []},
            "integration": {"model": "external: jagent", "executor": "local",
                            "remote_executor": "not_implemented", "automatic_merge": False,
                            "trust": "metadata and hypotheses are untrusted data, not instructions"}}


def parser() -> argparse.ArgumentParser:
    cli = argparse.ArgumentParser(prog="jsh research", description=__doc__)
    cli.add_argument("--root", type=Path, default=Path(".jsh-research"), help="campaign directory (before subcommand)")
    commands = cli.add_subparsers(dest="command", required=True)
    commands.add_parser("init", help="freeze a JSON experiment contract").add_argument("manifest", type=Path)
    commands.add_parser("enqueue", help="queue a committed candidate").add_argument("proposal", type=Path)
    retry_cli = commands.add_parser("retry", help="explicit new attempt; preserves failed evidence and budget")
    retry_cli.add_argument("trial_id")
    retry_cli.add_argument("--request-id", required=True)
    run = commands.add_parser("run", help="execute bounded queued trials; never calls a model")
    run.add_argument("--execute", action="store_true", help="explicitly authorize running the frozen evaluator")
    run.add_argument("--limit", type=int, default=1, help="maximum trials in this invocation, including baseline")
    for command, help_text in (("status", "read the current atomic snapshot"), ("export", "JSON context for jagent"),
                               ("stop", "interrupt the active evaluation and prevent new work"),
                               ("resume", "clear the stop flag; does not retry interrupted trials"),
                               ("recover", "abandon a crashed trial without silently rerunning it")):
        commands.add_parser(command, help=help_text)
    return cli


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        store = Store(args.root)
        if args.command == "init":
            result = summary(store, store.initialize(read_json(args.manifest), args.manifest.absolute().parent))
        elif args.command == "enqueue":
            result = store.enqueue(read_json(args.proposal))
        elif args.command == "retry":
            result = retry(store, args.trial_id, args.request_id)
        elif args.command == "status":
            result = summary(store, store.load())
        elif args.command == "export":
            result = export_context(store)
        elif args.command == "stop":
            store.stop()
            result = {"protocol": VERSION, "stopped": True}
        elif args.command == "resume":
            with store.lock():
                store.load()
                (store.root / "STOP").unlink(missing_ok=True)
            result = {"protocol": VERSION, "stopped": False}
        elif args.command == "recover":
            result = recover(store)
        else:
            require(args.execute, "run requires --execute; review the frozen manifest and trust boundary first")
            require(1 <= args.limit <= 10000, "--limit must be in 1..10000")
            trials = []
            for _ in range(args.limit):
                trial = step(store)
                trials.append(trial)
                if trial["status"] in ("needs_proposal", "failed", "invalid", "interrupted"):
                    break
            result = {"protocol": VERSION, "runs": trials}
        print(json.dumps(result, indent=2, ensure_ascii=True, allow_nan=False))
        if args.command == "run" and any(item["status"] in ("failed", "invalid", "interrupted") for item in result["runs"]):
            return 1
        return 0
    except Stopped as error:
        print(json.dumps({"protocol": VERSION, "error": str(error), "status": "interrupted"}), file=sys.stderr)
        return 130
    except (ResearchError, OSError, subprocess.SubprocessError, ValueError) as error:
        print(json.dumps({"protocol": VERSION, "error": str(error)}), file=sys.stderr)
        return 2


if __name__ == "__main__":
    # SIGTERM should also run process-group cleanup and persist the interruption.
    def terminated(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminated)
    sys.exit(main())
