# Research observation, offline preflight and reconciliation

This builds on [autoresearch-auto.md](autoresearch-auto.md). It adds operational
inspection and bounded host feedback, not a new model, cluster backend or sandbox.

## Inspect before spending a model call

```sh
jsh research --root ~/research/pilot auto-preflight
jsh research --root ~/research/pilot auto-report --recent 8
jsh research --root ~/research/pilot auto-run --execute --limit 2
```

`auto-preflight` reads the existing campaign/loop, checks the shared admission
policy, reads the selected files from Git, invokes the actual jagent author
validator and rechecks the context after validation. A small private temporary
input file is removed afterwards. It does not call the model, execute candidate
code, reserve tokens/trials/time, or write campaign state. Exit 0 means
`ready_offline`; exit 1 means not ready; invalid state/IO errors exit 2.
`network_checked`, `execution_authorized` and `sandbox_verified` are explicitly
false. Offline readiness is not proof that the selected model/server supports the
request or that generated code can safely run. Live admission is always repeated.

`auto-report` returns JSON with a versioned report protocol, revision, next action,
outcome counts, current-best complete-stage scores, remaining local budgets and a
bounded recent-attempt window. `--recent` accepts 1..64, default 16; omissions are
counted explicitly. Reports project fixed fields instead of copying arbitrary
persisted diagnostics. They omit model names, endpoints, source, hypotheses and
logs. They still contain research identities and measured scores: review them
before sharing. They are lock-free, read-only advisory snapshots, not an execution
lease. Inspection does not recreate a deleted campaign identity.

Execution, reports and preflight use one admission policy for stopped campaigns,
pending work/recovery, context readiness, persistent budgets, failure ceilings and
no-improvement limits. A queued candidate still requires explicit execution.

## Typed feedback, with backward-compatible jagent requests

The auto loop now supplies schema-version-2 author inputs. Each of at most four
recent observations contains exactly an attempt index, status, stage, fixed failure
code, trial ID (or null), and screen/confirm scores (or null). jagent still accepts
legacy schema-version-1 inputs with category-only feedback. The edit-plan schema
returned by the model remains version 1; bound candidate submissions remain
version 2. There is no silent downgrade or relaxation of edit validation.

Failure codes distinguish preparation, transport, plan validation, candidate
creation, admission, unresolved evaluation, failed/invalid evaluation and
interruption. They are operational categories, not free-form error messages.
Provider error bodies and arbitrary exception strings never become feedback.
A model envelope containing a provider error is rejected even if it also contains
apparently valid edit choices.

Scores are joined from the campaign by request ID, with stored trial/commit
bindings checked. Only full stage results with the exact ordered seed set become
means. Missing, partial or mismatched stages remain null, not zero. jagent rejects
unbound scores, unknown fields, inconsistent outcome/evidence combinations and
out-of-order attempt indices. These are host-record validation guarantees, not
remote attestation or proof of scientific improvement. Observations remain
untrusted user-role data; the model cannot supply promotion scores.

## Optional no-improvement policy

Add `"max_no_improvement": 3` to a new auto configuration before `auto-init` to
stop after three valid archived/screened-out outcomes against the same incumbent.
The default is 0 (disabled); old frozen configurations without this field retain
that behavior. The option is an integer in 0..1000 and is frozen with the other
configuration. Existing loops are not rewritten or reinitialized on upgrade.

This is separate from `max_failures`: a valid experiment that does not improve is
not an infrastructure failure. Failed/invalid/authoring attempts count toward the
failure ceiling, but not as measured non-improvements. A kept candidate or a new
incumbent starts a new plateau window. Counters are derived from the durable
attempt ledger, so restarting or stop/resume cannot reset them. Reaching the
plateau limit returns `no_improvement_limit`, a normal bounded stop, not a claim
that the research problem is solved. There is no automatic budget/patience override.

## Reconcile interrupted admission without replay

An attempt can be interrupted after its candidate was admitted, then the candidate
can finish through an explicit `run --execute`. Previously only attempts still
marked running were reconciled. Recovery now also checks interrupted and
unresolved-authoring attempts against the original request/trial/commit identity.

```sh
jsh research --root ~/research/pilot status
jsh research --root ~/research/pilot auto-report
# Only after inspecting the host and confirming no evaluation process remains:
jsh research --root ~/research/pilot recover
jsh research --root ~/research/pilot auto-recover
```

A completed but unreconciled candidate is reported as `needs_reconciliation`
when the campaign is otherwise ready. New authoring does not proceed until the
ledger is reconciled. Recovery takes the loop lock and then campaign writer lock,
refuses live process groups or running trials, checks identity bindings before
saving, recomputes failure streaks in attempt order and does not replay HTTP,
rerun experiments, change token reservations or refund unknown work. Repeated
recovery without changes does not rewrite the loop state.

Time-ledger loading additionally checks that the total agrees with the sum of
per-attempt charges. Duplicate attempt IDs, invalid statuses/stages or inconsistent
failure codes fail closed. These checks detect malformed state, not malicious
same-user rewriting of all related records. External isolation, independent final
evaluation, real Flint testing and DLC integration remain required future work.
