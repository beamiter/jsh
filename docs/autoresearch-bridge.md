# jsh / jagent research bridge

This adds a real cross-repository protocol boundary, not a claim that Flint or
DLC is already connected. jsh pins a jagent commit containing the sans-IO
`research` module. jsh invokes that module directly for request preparation and
proposal validation, while its Python controller remains responsible for actual
local experiments. No model weights, endpoints, tokens, SSH or paid jobs are used.

## New commands

```sh
# After init and a valid baseline, export compact context for an external agent.
jsh research --root ~/research/pilot bridge > context.json

# Native Rust/jagent helper: emits messages plus a response schema, not HTTP IO.
jsh research prepare-request context.json > request.json

# An approved external agent implements a candidate in its development worktree
# and creates an actual Git commit. Its bound proposal uses schema_version=2.
jsh research validate-proposal context.json proposal.json > checked.json

# Revalidate live state and admit atomically. Does NOT execute the candidate.
jsh research --root ~/research/pilot submit checked.json
jsh research --root ~/research/pilot run --execute
```

Global `--root` precedes bridge/submit/run. The two native helpers take file
paths, not `--root`; they operate on a saved context. Their input readers reject
oversized files, symlinks and nonregular files, including FIFOs without blocking.
Use the `response_schema` emitted by prepare-request to construct a proposal.
It contains the exact identity/revision/incumbent binding and eligible parents.

## What is bound

A schema-version-2 proposal carries campaign ID, frozen manifest digest,
expected state revision, expected best commit, request ID, complete candidate
commit, hypothesis and parent trial IDs. It cannot carry commands, metrics,
executor overrides, approvals or budget changes. The incumbent binding describes
the comparison context; it does not assert the candidate's Git ancestry.

Each campaign receives a persistent random ID in `bridge-identity.json`, under
the campaign writer lock. Two identical manifests in different campaigns have
different IDs. Existing v1 state is not rewritten or reinitialized; the identity
is created lazily on first `bridge`. Deleting it changes identity and invalidates
old proposals. Corrupt identities fail closed rather than silently resetting.

Rust validation checks a saved snapshot, which can become stale immediately.
`submit` independently repeats all binding and readiness checks under the live
writer lock, verifies the exact commit exists and enforces the original editable
and protected-path rules. Invalid submissions consume neither trial slots nor
evaluation time. Exact request replays return the existing trial even after the
best changes or the campaign stops. Reusing an ID for different content/schema
is rejected. Replay never starts a new evaluation.

`resume` now advances state revision, invalidating proposals prepared before a
stop/resume cycle. Never silently rewrite a stale proposal's binding to make it
pass: retain the commit, obtain fresh context and reassess the candidate.

## Bounded research memory

The context contains at most 16 trials, 2048 UTF-8 bytes per hypothesis,
completed-stage scores only, a separate best-candidate identity and an explicit
omitted-trial count. Usually these are the latest 16 trials. If the incumbent is
older, its record and measured scores occupy one slot alongside the latest 15,
so long runs never lose the target score from model context. Raw logs and ambient
environment are not included. Encoded
context is limited to 256 KiB and proposals to 32 KiB. Full historical evidence
remains in campaign state and per-seed receipts; bounded context is not a
replacement for that evidence.

Scores require the exact ordered, integer seed set and valid numeric/guardrail
metrics. Incomplete, duplicate, reordered or mismatched seeds are not averaged.
Confirmation is only exposed when screening is complete. The updated jagent
validator additionally rejects scores inconsistent with the recorded outcome.

Prepared messages keep a constant system prompt and put all observations in
untrusted user-role data. This preserves the existing jagent trust boundary; it
does not make arbitrary model output safe to execute. The native validator does
not produce an ApprovedCommand or grant execution permission.

`next_action` tells the host whether to request a proposal, evaluate queued work,
handle a stopped/exhausted campaign, inspect a running/interrupted attempt, or
retry a missing baseline. A `needs_recovery` snapshot can also represent an
active writer: inspect status before recovery; the controller's writer lock
prevents recovering or launching a competing trial while it is active. The
bridge remains readable during a run once identity has been initialized.

## Backward compatibility and limits

Legacy `export`/schema-1 `enqueue` remain available for existing human workflows;
they do NOT acquire the new context-binding guarantees. Agent integrations should
use `bridge` + native validation + `submit` exclusively. The bridge adapter is
embedded alongside the existing standard-library engine; installed binaries do
not depend on a source checkout. Normal shell startup is unchanged.

This version remains a serial local executor with explicit run authorization.
Flint transport, code-authoring orchestration, autonomous scheduling, DLC leases,
authenticated remote result ingestion, GPU-hour budgets and an OS sandbox are
not implemented by these commands. Worktrees do not isolate hostile code.

## Regression coverage

The Python suite covers identity isolation, atomic/idempotent admission,
concurrent delivery, stale/foreign responses, stop/resume invalidation, bounded
memory, protected-path checks and budget gates. The binary integration suite
runs the real Rust jagent validator and Python controller together, comparing
invalid-input rejection and executing a full baseline/candidate/promote/replay
cycle without a model or GPU. Core tests also cover late stop/deadline checks
and refusal to queue multiple baseline retries.
