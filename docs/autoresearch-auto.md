# Model-driven research loop

`jsh research auto-*` connects the shared `jagent::research::author` validator to
an explicitly configured chat-completion HTTP endpoint and the existing frozen
local evaluator. Unlike the earlier proposal bridge, it can request source edits,
create a real detached Git commit, submit the bound candidate, evaluate it and
feed the measured outcome into the next round. jagent remains sans-IO.

This is an opt-in, bounded prototype, not a claim of scientific improvement or
of complete Flint/DLC deployment. Only local evaluation is implemented. The test
server is deterministic; it is not a real language model.

## Setup and commands

Run the normal research `init` and baseline `run --execute` first. The campaign
must have a valid baseline and no queued/running trials before `auto-init`.
Select 1..8 small tracked UTF-8 files that are already allowed by the frozen
manifest. Copy `examples/research/auto.json` and edit it for your model server.

```sh
jsh research --root ~/research/pilot auto-init auto.json
jsh research --root ~/research/pilot auto-run --execute --limit 2
jsh research --root ~/research/pilot auto-status
jsh research --root ~/research/pilot stop
```

Initialization is offline. It freezes the endpoint, model, selected paths and
budgets, validates the sources and creates private loop state. It never resets
an existing loop. `auto-run --execute` explicitly authorizes transmitting those
selected sources and evaluating generated candidate code. `--limit` caps attempts
in this invocation; the persistent configuration caps all invocations together.
The runner is foreground and serial, not a daemon or a scheduled task.

Native helpers are also available without network access:

```sh
jsh research author-prepare author-input.json
jsh research author-decode author-input.json http-response.json
```

The binary embeds the controllers; installation does not require a source checkout.
Python 3.10+, Git and Unix are required. Both AI-enabled and core-only builds use
the same shared jagent validation. Native helpers grant no execution authority.

## Model transport

Default scope is a literal loopback IP such as
`http://127.0.0.1:8080/v1/chat/completions`; the configured model name must match
your running server. A non-loopback endpoint must be HTTPS and requires explicit
`auto-init CONFIG --allow-remote`. This transmits selected source and bounded
research history to that endpoint: review privacy before opting in. Userinfo,
query/fragment credentials, port zero and ambiguous paths are rejected.

The non-streaming request uses messages, `max_tokens`, temperature=1, top_p=0.95
and top_k=20. `response_format` can be `json_schema` (default) or `json_object`.
The latter appends the required schema as data but never relaxes native response
validation. Real server support for these fields must be checked on your chosen
Flint serving build; this implementation does not claim live-model compatibility
or performance measurements.

The key is read from `api_key_env` (default `JSH_RESEARCH_API_KEY`) at runtime and
sent only in the Authorization header. Its value is not written into requests,
state or diagnostic output. The HTTP child receives only its configured key and
a small runtime environment. Ambient HTTP proxies are ignored; redirects and
implicit retries are disabled. Response size is capped at 1 MiB, including any
reasoning metadata; truncated, refused or tool-calling responses fail validation.
A transport failure/timeout stops the invocation, even with `--limit > 1`.
An explicit later invocation may create a new budgeted attempt; it is not a replay.
The remote server may continue generation after the client disconnects.

## Candidate and evaluation boundary

The host reads the selected files at the current best commit, including original
Git blob IDs, and jagent accepts only complete replacements for those files.
No arbitrary commands, fabricated commits, supplied scores, new/deleted files,
symlinks or submodules are accepted. The auto loop keeps total selected source
at most 64 KiB after editing so the next round can read it again. The shared
protocol separately supports up to 128 KiB replacement text.

Candidates are committed in private detached worktrees with hooks disabled.
Original HEAD, working files and branches are not reset or merged. The ordinary
bridge admission rechecks campaign/revision/incumbent binding and protected
paths. The evaluator then checks the expected queue ID under its writer lock,
so another controller cannot redirect this attempt onto unrelated queued work.
Scores come from the existing screen/confirm evaluator, not model claims.

The selected model-key variable is removed from the evaluation environment and
restored in the serial controller afterwards. **This is not credential isolation
or an OS sandbox.** Other ambient variables, files, Git configuration, build tools
and same-user processes remain accessible. Run generated code in a disposable,
nonprivileged VM/container without personal credentials or network access, with
read-only evaluator/data and host-enforced quotas. Worktrees and path checks do
not prevent a hostile same-user program from forging its result or reading files.

## Budgets, evidence and recovery

`max_attempts`, total reserved output tokens, elapsed controller time and a
consecutive-failure limit persist in `autoloop/auto.json`. A worst-case time and
full per-call output-token reservation is charged **before** a model call. Failed
or truncated calls still consume tokens/attempts. Token reservations are a local
cap on requested output, not measured usage, input-token accounting or billing.
On normal completion time is reconciled to elapsed time; crashes retain the full
reservation. Timeouts are cooperative/polled, not OS resource guarantees.

Each attempt retains source/context, model-request body (no key), response, parsed
plan, candidate commit/proposal, outcome stage and evidence directory. These
files can contain proprietary code or model text; keep the directory private.
Full source/worktree evidence is retained, not automatically garbage-collected.
The next request receives bounded research scores and recent outcome categories,
not unlimited logs or a claim of long-term semantic research memory.

After an unclean shutdown, inspect processes and both state files first:

```sh
jsh research --root ~/research/pilot status
jsh research --root ~/research/pilot auto-status
# Recover a crashed evaluator first, when applicable and no process remains alive.
jsh research --root ~/research/pilot recover
jsh research --root ~/research/pilot auto-recover
jsh research --root ~/research/pilot resume
```

Recovery never sends another HTTP request or kills an arbitrary persisted PID.
It can reconcile a completed trial back to the interrupted author attempt, but
unknown attempts retain their reservation and become interrupted. Live recorded
process groups block recovery. A queued admitted candidate is not automatically
executed by recovery; inspect it and explicitly use `run --execute` when appropriate.
There is a small process-spawn/record crash window; inspect the host after SIGKILL.
Successful outcome reconciliation does not incorrectly consume a failure streak.

## Validation and remaining work

The read-only CI uses a local HTTP fixture with the actual Rust/jagent validator,
Git commits/worktrees and frozen evaluator. It exercises successive improvements,
malformed/truncated responses, stale snapshots, budget persistence, interruption,
transport failures, proxy/redirect handling, secret scope and crash recovery.
It uses no model credentials, no real Flint weights and no DLC resources.

Not implemented: DLC submit/poll/cancel/artifact adapters, authenticated remote
callbacks, GPU-hour budgets, parallel research workers, an adversarially isolated
judge, automatic publication/merge, or a scientific significance criterion.
The current loop improves a single objective via repeated frozen evaluations;
a truly untouched final test set and independent review are still required.
