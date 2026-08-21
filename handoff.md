# Engineering handoff

Updated: 2026-08-21

This baseline unifies command discovery, separates executable AI suggestions
from read-only explanations, completes workflow parameter filling, and fixes
Agent capture and closure UTF-8 regressions on top of the diagnostics, AI,
persistence, execution I/O, parser/completion, terminal text, helper-resolution,
and installer hardening described below.

## Completed since the previous handoff

- `command_catalog` is the discovery contract above the classic and value
  execution tables. Help, command completion, `compgen`, highlighting, typo
  repair, and builtin classification now consume its sorted, unique view.
  Contract tests prove it is exactly the union of both routing tables, every
  public command has help metadata, aliases resolve canonically, and `ls`/`ps`
  remain value-aware only in pipeline context. `def` and `reverse` now carry
  signatures instead of disappearing from signature-driven surfaces.
- Editor AI requests explicitly name Generate, Fix, or Explain and carry a
  monotonically increasing request ID. Explanations have a separate system
  prompt, response validator, size/line bounds, and read-only render state with
  no route into the command buffer or ghost suggestion. A response changes the
  editor only when both its ID and response type match the active request;
  leaving a prompt or editing during a request invalidates that authority and
  late replies are drained.
- Ctrl-G workflow selection now enters a real parameter session. Defaults and
  suggestions are visible and editable, completion inserts the final command
  for review only, cancellation restores the original editor line, and long
  lists scroll around the selected item. Template rendering is single-pass and
  bounded so substituted values cannot recursively create placeholders or
  expand a valid definition without limit. Only declared parameters are
  substituted; Docker/Go/Helm moustache expressions remain literal.
  `workflow`/`wf` exposes the same registry non-interactively. The public Rust
  rendering/session helpers now return `Result`, an intentional 0.x API change
  that prevents callers from silently accepting invalid or oversized output.
- Agent capture pipes are `O_CLOEXEC`. The one-shot child receives only the
  duplicated stdout/stderr destinations, never the capture reader, so a
  detached continuous writer gets `SIGPIPE` after capture closes instead of
  leaving itself and a waiting jsh orphaned. The regression test records the
  writer PID and proves it exits. Linux/Android use atomic `pipe2`; other Unix
  targets use a checked `fcntl(FD_CLOEXEC)` fallback, and CI now checks the
  macOS source-install path.
- Closure expression strings advance on UTF-8 character boundaries. Unicode
  literals now round-trip through both `each` and `par-each` instead of being
  rebuilt as one Latin-1 character per source byte.
- CI matches the documented release gate more closely: warning-free Clippy is
  enforced for both feature sets, tests use no-fail-fast without accidentally
  running Criterion benchmarks or excluding doctests, and API documentation is
  built in its own job.

- `jsh doctor` inspects runtime/terminal state, the effective startup home and
  startup file, persistence namespaces and existing private files, trusted
  helpers, the configured execution journal, and opt-in AI configuration. It
  never sources a file, starts a helper, contacts a provider, or prints a
  credential. JSON reports have `schema_version: 1` and a `healthy` bit;
  `--strict` turns warnings into status 1 for CI, and `--rcfile` diagnoses the
  exact file an interactive launch would use.
- Startup and persistence diagnostics distinguish `JSH_REAL_HOME` from the
  state-writing `$HOME`, and distinguish `$HOME/.jsh/sessions` from the XDG
  execution journal. Existing persistence entries are checked with
  `symlink_metadata` for file type, owner, hard links, and unsafe write bits
  without following them. Custom/disabled journal configuration is reflected
  in the paths that are checked.
- Long single-value options accept `--command=`, `--rcfile=`, and `--session=`.
  Repeated `--rcfile`/`--session` values are rejected instead of silently
  replacing one another, and immediate help/version output treats a closed
  downstream pipe as a normal consumer exit.
- Directory frecency records interactive navigation only. A script's `cd` no
  longer mutates `~/.jsh_z` or emits a persistence warning from an otherwise
  unrelated non-interactive command.

- The optional Agent integration now exact-pins jagent 0.7 and uses
  `prepare_agent_request`/`AgentRequestSpec` plus
  `accept_agent_response` end to end. The cancellable HTTP child returns the
  intact provider envelope instead of discarding completion/tool metadata;
  the parent decodes it with the same prepared value that built the request.
  `JSH_AGENT_PROTOCOL=text|native-tools` selects the wire encoding (Text is the
  compatible default), while both converge on the same explicit proposal
  review state machine. Local action-size, visual-spoof, and danger-policy
  backports removed by this migration now come from the shared 0.7 core.

- The single outbound AI funnel uses jagent's reported request builder and
  fails closed if jagent would omit anything after jsh has already bounded the
  history. The reported omission count drives a model-facing incomplete-context
  notice. Raw and redacted system text, the optional separator, and that notice
  share jagent's 64 KiB byte limit with checked arithmetic; trusted system
  instructions are never shortened and an omission notice is never dropped to
  make a request fit.

- `SessionSnapshot` no longer derives `Deserialize`. `decode_snapshot` is the
  only wire path into one, and it drives `DeserializeSeed`/`Visitor`
  implementations that charge as they build: every map and sequence stops at the
  first entry past *its own* limit, every string is measured before it is owned,
  and the text budget is shared across fields rather than reset per collection.
  The structural preflight bounded the document; it could not express "8 000
  functions and 8 000 environment variables are the same shape and only one of
  them is allowed", so those per-field rules were previously met only after the
  whole structure existed. `validate_snapshot_logical` is unchanged and is now a
  backstop rather than the only check. Legacy migration is untouched: unknown
  fields are still ignored so a snapshot from a newer build restores what this
  one understands, `environment_context` still defaults, and every other field
  is still required. Repeated fields are now refused rather than resolved to the
  last one — two values for `session_id` in one document is not something this
  build wrote, and picking either is a guess. The limit tests call
  `decode_snapshot` directly, without the audit, so a rejection there is proof
  the check moved into decoding rather than merely still existing.

- `jsh-remote.sh` has three tiers rather than two, and the new middle one is the
  default. Deployment needs a directory that can *execute* a file; integration
  needs one that can merely be *written* to, because `bash --rcfile` reads its
  argument and never runs it — `noexec` refuses `execve` and permits `write(2)`,
  so the middle tier survives precisely the case that strands the first. It
  hands the destination's own bash a throwaway rc that emits the same OSC 133
  A/B/C/D and OSC 7 marks `src/osc.rs` does, so the terminal keeps blocks, cwd
  tracking and exit codes; it buys back none of jsh, because jsh is not there.
  The destination's `~/.bashrc` is sourced first so aliases and prompt survive,
  and the file is deleted with the sandbox. `C` comes from `PS0`, not a `DEBUG`
  trap: `PS0` is printed after a command is read and before it runs, which is
  that point exactly, and it fires once per command line with no guards against
  firing for `PROMPT_COMMAND` itself. Below bash 4.4 there is no `PS0`, so the
  other three marks are installed and blocks do not form; `ash`-only images fall
  through to the plain shell as before.

- `JSH_HELPER_<NAME>` names an explicit absolute path for a helper program, so a
  layout that does not use the fixed candidate list — Nix, a Homebrew-style
  prefix, an immutable root — keeps the `.bashrc` import, the Git prompt and
  desktop notifications instead of silently losing them. It is not a return to
  PATH lookup: PATH is mutable shell state any sourced script can rewrite, while
  this is one path that must still pass `trusted_explicit_executable`. That check
  now resolves symlinks (`/run/current-system/sw/bin/git` and
  `/etc/alternatives` indirection are the normal case) and walks the *whole*
  resolved directory chain rather than the leaf and its immediate parent: a
  binary in a private directory under a world-writable ancestor is not safe,
  because the ancestor can be renamed away and replaced wholesale. A configured
  path that fails yields no helper rather than falling back to the automatic
  candidate — starting a different binary than the one that was named is worse
  than the feature being missing — and says so once per helper per process,
  because resolution happens from a prompt callback and a notification thread.

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
  cache's permissions and symlink safety. It also injects a failed installed-
  binary self-check to verify both atomic rollback and the truthful fallback
  when the rollback rename itself fails.
- `post_json` enforces explicit response-header count and cumulative-byte limits
  before reading a body. The body cap already existed; headers are parsed and
  retained before it applies.

## Remaining boundaries

### Add a signed release manifest

The installer's mandatory SHA-256 is same-origin: it proves the bytes match what
the release published, not who published them. A detached signature over the
manifest, verified against a key pinned in the installer, is the missing half.
That needs a release-side signing decision, so it is deliberately not
approximated here.

After changing either canonical script, synchronize and test every vendored jterm
copy. `jterm_core/scripts/install-jsh.sh` and `forge/scripts/install-jsh.sh`
carry `install-jsh.sh`; `jterm_core/scripts/jsh-remote.sh` carries the launcher.
All three vendor bodies match script revision
`fd605616b56bd73265a3a6141c814938aa2859f9`; each differs from its canonical
copy only by a four-line provenance header.

## Release checks

```text
cargo fmt --all -- --check
cargo test --locked --all-features --no-fail-fast
cargo test --locked --no-default-features --no-fail-fast
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo clippy --locked --all-targets --no-default-features -- -D warnings
cargo doc --locked --all-features --no-deps
shellcheck -s sh scripts/install-jsh.sh scripts/jsh-remote.sh
./scripts/test-install-jsh.sh
./scripts/test-jsh-remote.sh
```
