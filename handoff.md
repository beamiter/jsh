# Engineering handoff

Updated: 2026-08-01

This baseline hardens Agent state and cancellation, AI URL/credential handling,
session persistence, execution I/O, parser/completion limits, terminal text, and
automatic helper resolution. It exact-pins the hardened jagent baseline. The
release installer now fails closed, and AI response headers are bounded.

## Completed since the previous handoff

- Model requests are cancellable rather than merely abandonable. A blocking ureq
  read cannot be interrupted in place, so running it on a worker thread only ever
  achieved half the job: INT released the foreground, but the request stayed
  connected and billed — and held the single-flight slot — until the provider's
  120-second read timeout, refusing every request in that window with "a previous
  model request is still shutting down". The request now runs in a child process
  (`--jsh-internal-model-request`, dispatched from `internal_child_entrypoint`
  before any startup work), and cancelling it kills the process group. The
  in-flight gate is gone entirely because there is no longer a previous request
  to wait for. The child is this same binary, so TLS verification, the
  zero-redirect policy, the response header caps and the body ceiling are the
  unchanged code in `perform_model_request`; only where it runs moved. The
  envelope travels on stdin — never argv — because it carries the API key, and
  it is jsh's own versioned JSON rather than serde on jagent's types.
- `io_guard::bounded_command_session` generalises `bounded_command_output` with a
  stdin payload, a cancellation predicate polled every ≤100 ms, and an opt-in
  `PR_SET_PDEATHSIG`. The payload is written from a helper thread that is always
  joined: writing inline would deadlock against our own drain loop once it
  exceeds the pipe buffer, and detaching it would outlive the child holding a
  copy of whatever secret it carries. `die_with_parent` closes the one edge a
  process has that a thread did not — a `SIGKILL`ed shell cannot run any of its
  own kill paths, so without it a request could outlive the shell entirely.
  `tests/model_transport_tests.rs` asserts that with a positive control: it
  proves a transport child *is* running against a stalled provider before
  asserting that none survives, and reads `/proc` rather than grepping `ps`,
  whose output contains the harness's own command line.
- `scripts/jsh-remote.sh` runs jsh on a destination that has no jsh: it probes
  the machine, stages a verified static musl artifact locally, pushes it, proves
  its identity after it lands, executes it, and tears the sandbox down.
  `--persist` (default) caches the binary under the destination's `$HOME` and
  lets jsh keep its own dot-files there; `--incognito` redirects `HOME` and the
  XDG variables at a sandbox that is deleted on exit, so a shared account is
  never written to. ssh and `docker exec` are both supported; ssh reuses the
  same `ControlPath` jterm1 builds, so the probe, the push, the session, and the
  teardown share one authenticated connection. A sandbox that recorded an owner
  pid is swept the moment that owner is gone rather than waiting out an age
  guard — SIGKILL is the only way one outlives its session, and for `--incognito`
  that directory is precisely what the mode promised would not survive; the age
  guard now applies only to a sandbox that never recorded an owner, which is the
  one case it was actually for. `scripts/test-jsh-remote.sh`
  covers both transports against stub `ssh`/`docker` binaries — the ssh stub
  reproduces ssh's join-the-words-and-reparse behaviour, which is what makes the
  quoting testable at all.
- `JSH_REAL_HOME` splits "the home a person types" from "the home this shell
  writes to". `ShellState::home_dir` — which already backed `~`, bare `cd`, the
  prompt's `~/…` abbreviation, completion display, and startup-file lookup — now
  resolves through `environment::resolve_home_dir`, while every state path keeps
  calling `dirs::home_dir` and therefore keeps landing in `$HOME`. That is what
  makes `--incognito` usable: the sandbox still captures everything jsh writes,
  but `cd ~` goes where the operator means. An override that is not an existing
  absolute directory is ignored, never fatal, and the variable is excluded from
  session snapshots so one machine's home cannot follow a session to another.
- jterm1 and jterm4 can drive `jsh-remote.sh` from `[[remote_hosts]]` via a
  `deploy` key. The script is vendored into `jterm_core/scripts/` and published
  by `jterm_core::vendored_script`, the same way `install-jsh.sh` already was, so
  changing the canonical copy here means re-vendoring there.
- `install-jsh.sh --stage-dir DIR [--target TRIPLE]` downloads, checksums, and
  unpacks a release artifact without installing it, so the remote launcher does
  not carry a second copy of the download-and-verify logic. Staging deliberately
  skips the `--version` probe: the artifact is usually for another architecture
  and cannot run here, so the identity check moves to the destination.

- `scripts/install-jsh.sh` requires a published, format-checked SHA-256 and
  refuses to install unverified bytes; bounds every download by size and keeps
  redirects on HTTPS; validates the version, target, and base-URL grammars
  before they reach a URL or a path, including the version read from the release
  manifest; lists an archive and refuses links, special files, traversal,
  absolute paths, and extra members before extracting only the expected binary;
  makes the update-check cache, the staged binary, and the rollback restore use
  unpredictable, private, atomically replaced names; and bounds the `--version`
  probe with a deadline plus a file instead of a pipe, so a descendant that
  inherits the probe's stdout cannot hang the installer.
  `scripts/test-install-jsh.sh` covers the absent and malformed checksum, the
  symlink, extra-member and traversal archives, the rejected grammars, and the
  cache's permissions and symlink safety.
- `post_json` enforces explicit response-header count and cumulative-byte limits
  before reading a body. The body cap already existed; headers are parsed and
  retained before it applies.

## Remaining boundaries

### Replace snapshot preflight with schema-aware decoding

Session load has a 4 MiB file cap, allocation-free JSON depth/value/string
preflight, and post-decode logical validation. The final construction still uses
ordinary Serde. Replace it with bounded map/sequence visitors that enforce
collection counts, per-field limits, and cumulative text while allocating,
without weakening legacy snapshot migration. jterm1's `src/session.rs` now has a
worked example of this shape for a comparable schema.

### Generalize trusted automatic helpers without reopening PATH execution

Automatic bash, git, and notify-send integrations intentionally use fixed,
permission-checked system paths. Nix and custom system layouts can therefore lose
optional integration features. Support explicitly configured absolute helper paths
only after verifying executability, ownership, and every containing directory; do
not restore mutable PATH lookup for background helpers.

### Deployment-free shell integration as a third fallback

When no directory on the destination can execute a file, `jsh-remote.sh` falls
back to that machine's own shell and the terminal loses OSC 133 blocks, cwd
tracking, and exit codes along with jsh. A middle option exists: inject a
`PROMPT_COMMAND` that emits the same marks `src/osc.rs` writes into the remote's
existing bash, which needs no file on disk at all. It buys back the terminal-side
features but none of jsh's completion, and it degrades on images whose only
shell is busybox ash, so it is worth building only if unexecutable destinations
turn out to be common in practice.

### Add a signed release manifest

The installer's mandatory SHA-256 is same-origin: it proves the bytes match what
the release published, not who published them. A detached signature over the
manifest, verified against a key pinned in the installer, is the missing half.
That needs a release-side signing decision, so it is deliberately not
approximated here.

After changing either canonical script, synchronize and test every vendored jterm
copy. `jterm_core/scripts/install-jsh.sh` and `jterm4/scripts/install-jsh.sh`
carry `install-jsh.sh`; `jterm_core/scripts/jsh-remote.sh` carries the launcher.
All three are in sync with this revision, and each differs from its canonical
copy only by a three-line vendoring header.

## Release checks

```text
cargo fmt --all -- --check
cargo test --locked --all-targets --all-features --no-fail-fast
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo doc --locked --all-features --no-deps
./scripts/test-install-jsh.sh
./scripts/test-jsh-remote.sh
```
