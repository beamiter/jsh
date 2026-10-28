# Bounded model-driven research

This phase implements a real local loop: fresh research context + selected source
-> HTTP chat completion -> strict jagent edit validation -> detached Git commit
-> context-bound admission -> frozen screening/confirmation -> next model call.
It does not install a model, start a server, submit DLC jobs or merge/push code.

## Start

Build the updated jsh binary. Prepare a trusted evaluator and a frozen campaign
as described in autoresearch.md, then run its baseline. Use a disposable VM or
container for generated code; worktrees are not a security sandbox.

```sh
jsh research --root ~/research/pilot init manifest.json
jsh research --root ~/research/pilot run --execute
# Edit examples/research/auto.json for your endpoint, model alias and source files.
jsh research --root ~/research/pilot auto-init auto.json
jsh research --root ~/research/pilot auto-run --execute --limit 10
jsh research --root ~/research/pilot auto-status
```

`auto-init` is offline and refuses to overwrite an existing loop or reset its
budget. It requires a valid baseline, no pending work and 1..8 explicitly selected
editable files. Source is read from the current best Git commit, not uncommitted
working-directory changes. Only regular tracked UTF-8 files are supported;
new files, deletion, binary edits, symlinks and submodules are deliberately not
supported in this first authoring backend. Total input source is at most 64 KiB;
replacement contents are at most 128 KiB. Refactor large tasks into focused campaigns.

The model returns a hypothesis and replacements bound to source blobs and the
exact campaign/revision/incumbent. It cannot return commands, scores or a claimed
candidate commit. jagent validates the response; jsh creates the real detached
commit, rechecks protected paths and submits it under the live campaign lock.
The original checkout/branch is unchanged. No commits are automatically pushed
or merged. A stale response is rejected, never silently rebound.

## Model transport

The endpoint is an explicit `/v1/chat/completions` URL. The default accepts only
literal loopback IPs, not DNS names. To transmit selected source to a remote
HTTPS endpoint, explicitly pass `--allow-remote` to `auto-init`. The decision is
frozen with the configuration. This is a deliberate source-disclosure boundary.
HTTP redirects and environment proxies are disabled. There are no automatic HTTP
retries, so a timeout cannot silently duplicate a potentially charged request.

An optional API key is read from the named environment variable at call time and
sent only in the Authorization header. Its value is never written to configuration,
request artifacts or errors. Source/context and raw model responses ARE private
artifacts under the campaign; they may contain source-code secrets. Do not select
credential files. Generated evaluator processes still inherit the host environment:
run the campaign without production credentials or a forwarded SSH agent.

Sampling uses temperature 1.0, top_p 0.95 and top_k 20, matching the Flint model
card. Each round is a fresh non-streaming request. A separate reasoning_content
field is not carried into subsequent turns; experiment evidence is the memory.
Only one normal stop completion with a valid JSON edit object is accepted.
Truncation, refusals, tool calls, duplicate JSON members and oversized responses
fail closed. The chat-completion response limit is 1 MiB.

By default `response_format=json_schema` requests grammar-constrained output.
For servers without this schema subset, select `json_object` BEFORE initialization;
the model still receives the schema and the same strict native validator remains
mandatory. Compatibility with a mock OpenAI-style endpoint is tested. Actual
Flint quality, speed, tokenizer/template settings and server-build compatibility
must be measured on the user's model service; no live Flint benchmark is claimed.

Primary compatibility references:
https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md
https://huggingface.co/RecursiveSI/flint

## Persistent budgets and recovery

Limits cover total author attempts, conservative reserved completion tokens,
model-request timeout, cumulative loop elapsed time and consecutive failures.
The original campaign's trial/time budgets still independently gate evaluations.
`--limit` bounds only this invocation; restarting does not reset any lifetime cap.
Input tokens and monetary/GPU costs are NOT metered by the completion-token cap.
Providers may ignore max_tokens: enforce cost/rate quotas on the model server too.

Each attempt reserves its full max_tokens before any model IO, including failed
attempts. It reserves request timeout + evaluation timeout reservation + a fixed
setup allowance before execution. Normal completion charges measured elapsed
time; a crash keeps the full reservation. This is conservative admission, not a
kernel-enforced realtime deadline. Evaluations keep their existing per-seed timeouts.
Infrastructure/invalid-output failures stop at a persistent failure streak;
valid kept/archived/screened-out results reset that streak.

```sh
# Works during model calls as well as local evaluation.
jsh research --root ~/research/pilot stop
# Inspect the host/processes after a crash; never kill persisted PIDs blindly.
jsh research --root ~/research/pilot auto-status
jsh research --root ~/research/pilot recover       # only if evaluator recovery is needed
jsh research --root ~/research/pilot auto-recover
jsh research --root ~/research/pilot resume
jsh research --root ~/research/pilot auto-run --execute --limit 3
```

Recovery refuses recorded live process groups, reconciles already-completed
trials, and never automatically resends a model request or reruns an unknown
experiment. A submitted but still queued candidate after a crash must be inspected
and evaluated explicitly with `run --execute`; auto-run will not take unrelated
queued work. A small process-start/persist crash window remains; inspect the host
following SIGKILL/power loss. Worktrees/evidence are retained, not garbage-collected.

Two auto loops cannot own the same campaign loop lock. Model IO does not hold the
campaign writer lock, so status/stop remain usable. A competing manual operation
can invalidate the snapshot; the candidate is then archived without admission.
The executor checks an expected trial ID under its lock, so races cannot make the
auto loop execute another controller's queue head accidentally.

Artifacts include author-input.json, http-request.json, http-response.json,
plan.json, proposal.json and the detached candidate worktree. The atomic auto.json
journal records attempt stage, source/candidate commits, trial identity and budgets.
Error summaries contain only stage/category, not raw remote error bodies.

## DLC boundary

The authoring loop already hands a real candidate to the same context-bound local
controller. A future DLC adapter belongs below evaluation, not in model-generated
commands. It still needs authenticated submit/poll/cancel, attempt leases,
idempotency, artifact fingerprints, remote result validation and GPU-hour budgets.
This implementation intentionally cannot submit paid cluster work.
