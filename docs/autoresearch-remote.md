# Remote experiment staging / DLC gateway contract v1

Implemented against jsh `3b446125` and jagent `5513d173`. This is an opt-in,
replaceable remote execution **client and protocol**, exercised against a real
HTTP mock gateway. It is not an Alibaba Cloud gateway deployment or a claim of
GPU execution. Local `run` and `auto-run` remain unchanged.

## Scope and trust boundary

`Gateway.call(operation, request)` is the replaceable transport interface:
submit, status, result, cancel, recover. `Remote` owns identity, persistent
reservations, state transitions and verified evidence. jagent's
`research::remote` separately supplies sans-IO response binding validation;
the Python client validates the same wire contract independently and does not
require changing the existing pinned jagent dependency.

Only the human-configured executor may select credentials, workspace, pool or
image. Model proposals still cannot override any of these. An immutable request
binds campaign ID, manifest digest, config digest, state revision, trial/commit,
stage/seed/frozen argv, task ID, random attempt ID, resources and absolute lease.
A trial must be queued and pass the existing protected-source checks on first
submission. Retries retain the original request exactly, even if local campaign
state later advances. Fresh tasks recheck live state and stop/budget gates.

Remote measurements are **staged evidence**, not local trial results. They do
not mark a queued trial complete, change best_id, consume local CPU-time budget,
or feed the local model loop. Remote GPU budget is a separate frozen ledger.
Before automated promotion is added, remote baseline and candidates must share
an attested image, datasets, evaluator and complete frozen ordered seed sets.
Do not run local and remote measurements as if they were interchangeable.

## Usage

```sh
# Initialize the normal campaign first. Review and replace all example values.
jsh research --root CAMPAIGN remote-init examples/research/remote.json
# Credentials are resolved from the named environment variables at request time.
# They are never written into the ledger or request sent to candidate workers.
jsh research --root CAMPAIGN remote-submit baseline-screen-1 TRIAL_ID \
  --stage screen --seed 1 --execute
jsh research --root CAMPAIGN remote-status baseline-screen-1
jsh research --root CAMPAIGN remote-result baseline-screen-1
jsh research --root CAMPAIGN remote-cancel baseline-screen-1
jsh research --root CAMPAIGN remote-recover baseline-screen-1
jsh research --root CAMPAIGN remote-ledger
```

The equivalent source-checkout CLI is `python3 scripts/jsh_research_remote.py`.
The installed binary embeds the controller and its engine/bridge dependencies.
`remote-init` performs no network call. Configuration is immutable after init.
For tests only, `--allow-loopback-test` permits HTTP on numeric loopback addresses;
production origins require HTTPS, certificate verification and no redirects.
A bearer credential and a separate HMAC key of at least 32 UTF-8 bytes are required.
Use strong randomly generated keys provisioned to the trusted host and gateway,
never to an evaluated candidate or model prompt.

## Durable budget and recovery semantics

Before any submission, the campaign writer lock protects an atomic, fsynced
`remote-ledger.json` intent and a reservation of `gpus * lease_seconds` GPU-seconds.
Both job count and total GPU-seconds are capped. Divide GPU-seconds by 3600 for
GPU-hours. This implementation deliberately retains the full charge after
success, failure, cancellation, interruption and uncertain delivery. It never
refunds time based on an unverifiable network error or claimed early finish.
The ledger validates reservation totals when reloaded. Deleting or restoring an
old ledger is outside the recovery contract and can lose budget history.

A repeated task ID with the same trial/stage/seed returns the saved entry without
network access or another reservation. Rebinding that ID is rejected. A timeout,
invalid signature or disconnect retains `submission_unknown` or the last verified
state and sets `last_error`. `remote-recover` queries the same immutable request;
it never calls submit again. A crash before delivery may therefore strand a
reservation. This fail-closed tradeoff avoids duplicate expensive jobs when
provider idempotency cannot be proven. A new task is an explicit new paid attempt.

Cancellation intent is persisted before sending. Recovery repeats cancellation,
not submission. Stop/lease expiry encountered during a remote operation also
requests cancellation. A stop marker alone is not a remote daemon: if the host
is offline, only the gateway's watchdog/provider runtime cap stops the GPU.
Cancel ACK must reflect verified terminal provider state, not merely successful
acceptance of a StopJob HTTP request. Unknown cancellation retains its reservation.
Late success after cancellation or expiry is rejected and requires reconciliation.
Terminal observations are cached; they never authorize job resurrection.

## Wire protocol

POST to `/v1/experiments/{operation}` with JSON:

```json
{"protocol":"jsh.research.remote/v1","operation":"status","nonce":"fresh-random-hex","request":{}}
```

`request` is the exact persisted object (see source and HTTP mock tests). Its
SHA-256 uses Python-compatible canonical JSON: UTF-8, sorted keys, ASCII escaped
strings, no whitespace, no NaN/Infinity. The gateway must key its durable record
by authenticated tenant + campaign + task/attempt identity, reject different
content for an existing identity, and check workspace/pool allowlists before IO.
Do not rely on an untrusted DisplayName to identify a job.

Response status must be 200, with `X-Jsh-Signature` equal to the lowercase
HMAC-SHA256 hex digest of the **exact raw response bytes**. The signed JSON has
exactly protocol, nonce, request_digest, job_id, status, result. Status is pending,
running, succeeded, failed, cancelled or expired. Result is null except for success.
All status responses are authenticated, not just metrics. A fresh exchange nonce
prevents replay; a bound digest and stable job ID prevent cross-job substitution.
Unknown fields, duplicate JSON members, nonfinite values and oversized responses
are rejected. Requests have a 10-second socket timeout and response-read deadline;
DNS/platform resolver delays are still subject to operating-system behavior.

Success result contains exactly request_digest, job_id, finished_at (Unix integer),
metrics, metrics_sha256. The latter is SHA-256 of canonical
`{"valid":true,"metrics":METRICS}`. The client verifies the hash, lease, identity
and frozen metric/guardrail rules. Verified raw response bytes (base64), signature,
request and metrics are archived in the ledger. HMAC attests the configured
trusted gateway, not independent scientific correctness. The digest covers the
metrics payload; large artifact download/verification is not implemented.

## Actual Alibaba Cloud DLC integration requirements

Official API references checked on 2026-10-30:

- [CreateJob](https://www.alibabacloud.com/help/en/pai/developer-reference/api-pai-dlc-2020-12-03-createjob):
  POST `/api/v1/jobs`; bind WorkspaceId, ResourceId, CodeSource.Commit,
  digest-pinned JobSpecs image, exact replica/GPU count and approved UserCommand.
  The documented fields include JobMaxRunningTimeMinutes. This running-time cap
  does not replace a gateway absolute lease that also expires queued jobs.
- [GetJob](https://www.alibabacloud.com/help/en/pai/developer-reference/api-pai-dlc-2020-12-03-getjob):
  GET `/api/v1/jobs/{JobId}` for configuration/state verification.
- [StopJob](https://www.alibabacloud.com/help/en/pai/developer-reference/api-pai-dlc-2020-12-03-stopjob):
  POST `/api/v1/jobs/{JobId}/stop`; subsequently poll until terminal.
- [API overview](https://www.alibabacloud.com/help/en/pai/developer-reference/api-pai-dlc-2020-12-03-overview):
  ListJobs for reconciliation. GetJobMetrics reports operational monitoring;
  it must not be confused with signed scientific evaluation results.

The inspected CreateJob schema does not list ClientToken. A production gateway
must durably record a create intent and bind/reconcile the resulting JobId. If
creation outcome is unknown, do not automatically issue a second CreateJob.
Neither a local file lock nor a display-name lookup establishes provider-wide
exactly-once creation. Multiple ambiguous matches require operator resolution.

Before real integration: provision authorized RAM/STS credentials with required
CreateJob/GetJob/ListJobs/StopJob access; confirm the selected region, workspace,
resource pool, quota and single-node GPU allocation; pin a trusted evaluator
image and code/data sources; provision restricted artifact storage; deploy a
persistent authenticated gateway with lease watchdog and immutable evaluation
receipts outside candidate control. Disable provider retries for ambiguous create
operations. Use read-only checks first, then one explicitly budgeted smoke job.
Credentials/resource access were **not available in this session**: no DLC/pai/
aliyun CLI or relevant credential variables were found, and no authorized gateway,
workspace or resource pool was provided. No real API job or GPU workload was run.

## Validation

`python3 -m unittest discover -s tests -p 'test_research_remote.py' -v` runs a local
HTTP mock with signed responses. It exercises success, duplicate submission,
changed ID payload, lost response after creation, connection failure before
creation, restart, cancellation disconnect, task failure, stale/forged/foreign
responses, metric/hash rejection, expired results, budget exhaustion, writer
contention, stop gates and embedded CLI loading. `tests/research_remote.rs` runs
the suite with the installed binary. These are simulated DLC protocol tests.
