# jsh research: bounded automated-experiment control plane

`jsh research` adds an opt-in research subsystem without replacing the normal
shell. It is inspired by the *publicly described ideas* of Recursive's Eureka
Machine: propose, implement, evaluate, validate, retain useful branches and build
on evidence. This is an independent implementation, **not** its private system
or a claim to reproduce its research results.

Primary reference:
https://recursive.com/updates/first-steps-toward-automated-ai-research

## Responsibility boundary

```text
jagent + Flint (future integration)
    proposes hypotheses and implements candidate Git commits
              | versioned JSON proposal / context
              v
jsh research
    frozen contract -> bounded queue -> isolated worktrees
    -> screening -> confirmation -> archive / best candidate
              | versioned per-seed execution request + receipt
              v
local executor (implemented) / jagent -> DLC executor (not implemented)
```

jsh owns experiment identity, lineage, contracts, evidence, budgets and promotion.
jagent owns model inference, code authoring, interpretation and future cluster
connectivity. No Flint weights, model server, SSH login, DLC credentials, paid job
submission or jagent API changes are part of this change.

The controller currently executes queued experiments **serially**. It supports
multiple research branches and multi-parent lineage, but not parallel workers,
automatic branch recombination or a model-driven hypothesis scheduler. An empty
queue returns `needs_proposal` rather than pretending to perform autonomous
research without a connected model.

## Runtime and installation

The Rust binary embeds `scripts/jsh_research.py` using `include_str!` and execs
`python3 -I -X utf8 -c ...`. Thus `cargo install`/binary distribution does not need
the source checkout at runtime. Python **3.10+**, Git and Unix are required only
for `jsh research`. There are no pip dependencies or new Rust dependencies, and
the `ai` feature is not required. Normal shell invocations never launch Python.

The subcommand is dispatched by the binary's `main`; use `jsh research --help`
for its command list. It is not an interactive-shell builtin. A script literally
named `research` can still be run as `jsh ./research` or `jsh -- research`.

## Smallest useful workflow

First prepare a Git repository with a committed candidate implementation, a
trusted evaluator, and a frozen dataset manifest. The evaluator must write the
result contract below. Copy `examples/research/manifest.json` and replace its
placeholder paths and commands before running anything. Relative `repo` paths
are relative to the manifest file, not the invocation's working directory.

```sh
# This validates configuration and freezes HEAD to an exact commit. No evaluation.
jsh research --root /absolute/path/research-campaign init manifest.json

# Review the normalized immutable contract, then explicitly authorize execution.
jsh research --root /absolute/path/research-campaign export
jsh research --root /absolute/path/research-campaign run --execute

# jagent/a human creates a candidate COMMIT in a separate development worktree.
# Supply that existing commit, a falsifiable hypothesis and a unique request_id.
jsh research --root /absolute/path/research-campaign enqueue proposal.json
jsh research --root /absolute/path/research-campaign run --execute --limit 5

jsh research --root /absolute/path/research-campaign status
jsh research --root /absolute/path/research-campaign export
```

`--root` is a global option and belongs **before** the subcommand. Commands
produce JSON, except `--help`; errors go to stderr. `run` requires `--execute`.
Exit codes are 0 for a completed invocation, 1 for a failed/invalid trial, 2 for
configuration/usage errors and 130 for interrupted execution. A candidate that
is valid but fails to improve the score is a normal research result, not an
execution error.

`enqueue` never executes commands and never accepts a model-supplied evaluator,
metrics or resource override. It only accepts a hypothesis, an existing commit,
a request ID and optional parent trial IDs. Replaying an identical request ID is
idempotent; reusing it with different content is rejected. Relative refs are
resolved on admission, so use exact commits for retry-safe external integration.

## Frozen contract and evaluation

The manifest in the state snapshot is normalized and hashed. No CLI operation
changes it. A different objective, dataset, evaluation command, seed set or
resource ceiling requires a new campaign and a new baseline.

- `editable_paths` is an allowlist of literal repository-relative path prefixes.
  Every candidate is diffed against the frozen baseline, including deletions.
- `protected_paths` must exist in the baseline and cannot overlap editable
  prefixes. Their Git object identities must match in every candidate. Include
  evaluator code, all its scoring dependencies and versioned data manifests.
- `screen` and `confirm` each have a fixed argv array and distinct seed sets.
  Arguments are passed directly to the executable, not through a shell.
- The objective supports `min` or `max` with a strict `min_delta` threshold.
  Every required guardrail is checked on **every seed**, before aggregation.

Each seed and stage gets a fresh detached Git worktree, not a recycled workspace.
The evaluator runs there with these environment variables:

```text
JSH_RESEARCH_RESULT           absolute destination for the result JSON
JSH_RESEARCH_SEED             integer seed
JSH_RESEARCH_STAGE            screen | confirm
JSH_RESEARCH_TRIAL            stable trial ID
JSH_RESEARCH_MANIFEST_SHA256  digest of the frozen experiment contract
```

It must exit successfully and create a regular result file, at most 1 MiB:

```json
{"valid": true, "metrics": {"loss": 0.125, "constraint_violations": 0}}
```

Unknown result fields, missing metrics, false validity, booleans used as numbers,
NaN/infinity, duplicate JSON keys, oversized files, FIFO/device files and symlinks
are rejected. An evaluation that modifies tracked source/evaluation files or
changes HEAD is rejected. The evaluator must actually consume the seed; the
controller cannot prove that arbitrary code does so.

The baseline must pass both stages. Candidates are compared against the current
best using the identical seed sets and frozen contract. Only a mean improvement
strictly beyond `min_delta` in **both** stages promotes the best pointer.
Screening losers skip confirmation. Confirmation losers are archived. Previous
best candidates and negative results remain available as research stepping
stones; multiple parents record idea lineage, **not** an automatic Git merge.

This is a deterministic selection rule, **not** a statistical-significance test.
Confirmation seeds reduce a particular source of noise but repeated adaptation
can still overfit them. Keep a truly untouched final test set and an independent
review outside the adaptive loop. `dataset_id` is provenance metadata, not an
external-data checksum verifier. Untracked external datasets, Python packages,
GPU drivers, caches and ambient environment are not frozen by this implementation.
For serious comparisons, use a pinned execution image and frozen data mounts.

## Persistence, budgets and recovery

The campaign directory contains an atomic `state.json`, an advisory writer lock,
a stop flag, and a per-trial/per-stage/per-seed evidence tree:

```text
state.json
trials/e000001/screen-11/
    request.json
    receipt.json
    result.json
    stdout.log
    stderr.log
    worktree/
```

Writes use exclusive temporary files, fsync and atomic replacement. A single
writer lock prevents competing controllers from launching the same trial.
`status`, `export` and `stop` remain available while a run holds the writer lock.
Newly created campaign directories are private (0700), and state files are 0600.
Use an already-private parent directory and do not share state/logs casually.

`max_trials` includes the baseline, candidates, failures and explicit retries.
Before executing a trial, the controller reserves the worst-case total timeout
for all its screening and confirmation seeds. It charges measured elapsed time
on normal completion, releasing unused reservation. A controller crash retains
the full reservation, so restarting cannot erase consumed budget. Deadlines are
polled; this is not a hard realtime or OS resource-quota mechanism.

```sh
# May run from another terminal while evaluation is active.
jsh research --root /absolute/path/research-campaign stop

# Allows new work; never silently replays an interrupted experiment.
jsh research --root /absolute/path/research-campaign resume

# After a controller crash, fail closed while any recorded process group lives.
jsh research --root /absolute/path/research-campaign recover

# Explicitly retry in a NEW trial/worktree, retaining old evidence and charges.
jsh research --root /absolute/path/research-campaign retry e000001 --request-id retry-0001
jsh research --root /absolute/path/research-campaign run --execute
```

The executor terminates the process group it created on completion, timeout,
stop, Ctrl-C or SIGTERM. A SIGKILL/power loss can leave children behind. Recovery
checks the recorded process group conservatively and **never kills a persisted
PID**, which could have been reused. Inspect/stop orphaned processes yourself
before recovery. There remains a small crash window between process creation
and recording its group; inspect the host after an unclean shutdown.

Per-stream logs are monitored at 8 MiB and truncated to that retention limit on
cleanup; a burst writer can transiently exceed it. Worktrees and artifacts are
retained for diagnosis and **are not automatically garbage-collected**. Apply
filesystem quotas and clean up Git worktrees deliberately when a campaign ends.

## jagent / Flint / DLC handoff contract

`export` supplies a versioned JSON snapshot with the frozen manifest, its digest,
revision, budget consumption, candidate lineage, scores and a proposal template.
It does not include raw stdout/stderr or dump environment variables. Treat
hypotheses and external observations as untrusted data, never as privileged
instructions to the future agent.

A future jagent loop can consume `export`, ask Flint for a hypothesis, implement
and commit a bounded change in its own worktree, submit `enqueue`, invoke the
explicitly authorized runner, then read the next snapshot. Do not treat a model's
claim of an improved score as an evaluation result.

Per-seed `request.json` and `receipt.json` establish the boundary for a future
executor adapter: protocol version, manifest digest, trial/commit identity,
stage/seed, argv, work directory, timeout, metrics and evidence provenance.
**They are local records, not a working remote job API.** Before enabling DLC,
add authenticated submit/poll/cancel/artifact retrieval, attempt/lease IDs,
idempotent submission, stale callback rejection, image/data fingerprints,
GPU-hour accounting and an independent trusted validation path. The current
CLI deliberately has no remote-result-import or `submit-dlc` command.

## Trust boundary and deliberate exclusions

Git worktrees and protected-path checks prevent accidental evaluator changes;
they are **not an OS sandbox or an adversarially tamper-proof judge**. Evaluators
inherit the host environment and filesystem permissions. Code running as the
same user can access credentials, modify other files or forge its own result.
For untrusted/model-generated code, run the whole campaign in a disposable
container/VM with nonprivileged credentials, read-only evaluator/data mounts,
restricted networking and host-enforced quotas. Do not expose a personal SSH
agent or cluster credentials to candidate code.

There is no automatic Git commit, cherry-pick, merge, reset, branch push, cluster
submission, unattended hypothesis generation or modification of jsh's own
executor/approval rules. Promotion updates only a best-candidate pointer. These
limits keep research selection separate from deployment approval.

## Validation

```sh
python3 -m unittest discover -s tests -p test_research.py -v
cargo fmt --all -- --check
cargo test --locked --no-default-features --test research_cli
cargo test --locked --all-features --test research_cli
```

The offline Python tests create disposable Git repositories and run real child
processes. They cover contracts, selection, guardrails, path protection,
idempotence, lineage, budgets, logs, interruption/recovery and explicit retries.
Rust integration tests exercise the actual embedded binary entry point, including
execution from outside a source checkout. The dedicated GitHub workflow runs
both sets. No GPU, model or DLC credentials are required for these tests.
