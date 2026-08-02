# Changelog

## Unreleased

- A startup file is a sourced script. `return` at the top level of one now
  ends the file instead of printing "can only return from a function or sourced
  script" and reading on, and `${BASH_SOURCE[0]}` names the file rather than
  being empty — an rc that locates itself with
  `$(dirname "${BASH_SOURCE[0]}")` was resolving to whatever directory the
  shell started in. `PS1` is also set for an interactive shell, because
  `[ -z "$PS1" ] && return` is how a distribution rc asks whether anyone is
  listening; unset, it made jsh look non-interactive to the file it was about
  to read. Together these are what every stock `.bashrc` opens with, and
  `jsh-remote.sh --incognito` passes exactly such a file with `--rcfile`, so a
  container session began with an error on every start.
- `jsh-remote.sh` takes its session down with it on the docker transport. ssh
  hangs up the remote pty when it goes; `docker exec` leaves what it started
  running, so a closed tab left a jsh in the container with its sandbox
  unlinked underneath it, and every open and close added another. Teardown now
  signals the recorded pid — but only after proving, through the sandbox name
  the session carries in its environment, that the process is really this
  session and not a reused pid. HUP first, since that is what a terminal going
  away means and jsh answers it by saving its session; KILL only for a shell
  that will not leave.
- A root shell trusts the system helpers it could write. "Can the current user
  replace this binary" is a trust signal only for an unprivileged user; root
  can write every file on the system, so the automatic-helper check answered
  yes for `/usr/bin/git` and `/usr/bin/bash` on every root shell and silently
  dropped Git completion, the Git prompt, desktop notifications and the
  `.bashrc` import. Containers run as root by default, which is where this
  showed: the same image reached through `--docker-user` had all of it. For
  euid 0 the question is now whether some *other* user owns the path, matching
  the rule an explicitly configured `JSH_HELPER_*` already used. Group- and
  world-writable is still refused for everyone, and unprivileged behaviour is
  unchanged.
- `jsh-remote.sh` forwards `TERM` and `COLORTERM` into a container. ssh sends
  them for us; `docker exec` sends nothing and the daemon substitutes its own
  default, so a container session drew in 8 colours while every other tab in
  the same terminal drew in 16 million. Only a bare terminal name is
  forwarded, and `LANG` deliberately is not: naming a locale the image never
  generated is worse than inheriting none.

- Retired `JSH_AGENT_AUTO_APPROVE_READONLY`: command text alone cannot prove
  that aliases, functions, Git helpers, or flag-dependent tools are read-only,
  so every Agent proposal now requires explicit approval.
- The complete all-target benchmark suite runs again: the reduce benchmark now
  passes its initializer through the supported `-i` interface.
- Cleared the strict all-feature Clippy backlog and made stdin line processors
  stop on persistent I/O errors instead of repeatedly discarding them. An
  unterminated quoted associative-array initializer now fails closed instead
  of constructing an invalid slice.
- Hardened history persistence: history and lock files no longer follow
  symlinks or block on special files, only regular files are accepted, and
  their descriptors close across `exec`. Per-record/file/entry limits bound
  startup and compaction memory, lock waits are finite, and a renamed lock
  sidecar cannot split cooperating writers onto different lock inodes.
  Newly created state directories start private and writable shared parent
  namespaces are rejected.
- Applied the same descriptor, ownership, hard-link, size, and bounded-lock
  rules to the structured execution journal; decoding retains a fixed newest
  window and atomic compaction never publishes an oversized journal.
- Session snapshots now use no-follow, nonblocking descriptor I/O, reject
  hard links and identity mismatches, enforce a 4 MiB read/write ceiling, and
  preserve the last good snapshot when a replacement exceeds that ceiling.
  Credential-bearing URLs and private-key material are filtered even when the
  environment-variable name itself does not look secret.
- Agent provider envelopes are capped before JSON decoding, and detached
  descendants, including continuous writers, can no longer keep an approved
  command's capture pipe open indefinitely after the direct jsh child exits.
  Approved commands now execute in a fresh one-shot jsh process initialized
  from a private bounded snapshot that is atomically claimed by exactly one
  child, preserving aliases, functions, variables, and options without running
  Rust code after a fork in the threaded shell. AI request/response channels
  and their history/output payloads now have explicit entry and byte ceilings.
  Model prose, provider errors, and displayed working directories are
  control-escaped and display-bounded before they reach the terminal; approval
  cards also expose invisible/bidirectional Unicode and require the deliberate
  dangerous-command confirmation for it. High-confidence secrets are redacted
  from local AI diagnostics too, and Agent prompts now respond promptly to
  terminal interrupts, including while a provider socket is stalled; only one
  timed-out request worker may remain in flight. HUP/TERM status stays latched
  through orderly cleanup, preventing an early executor check from turning
  termination into exit 0.
- Git dependencies are pinned to immutable revisions so a moving branch cannot
  silently change a locked build.

- `source ~/.nvm/nvm.sh` no longer recurses until the stack overflows, and
  `nvm` runs. Three fixes: `$-` now reports the enabled options instead of
  expanding to the literal text `$-` (nvm's "is errexit set?" guard was always
  true, so every call re-invoked itself); redirections and the assignment
  prefix on a shell-function call are now applied to the function body, so
  `f 2>/dev/null` silences it and `V=x f` scopes V to the call; and
  `command NAME ...` execs the already-expanded argv instead of re-joining it
  with spaces and re-parsing, which used to shred any argument containing
  whitespace or newlines.

- `${...}` now picks its operator from the character right after the parameter
  name instead of searching the whole body for each operator in turn. A `-`,
  `=` or `+` inside a pattern used to hijack the expansion, so `${v##*a-b*}`
  was read as `${v##*a}` defaulting to `b*` — which made
  `/etc/profile.d/xdg_dirs_desktop_session.sh` prepend a directory to
  `XDG_CONFIG_DIRS` that was already there. Added `${var:?message}`,
  `${var?message}` and the `^ ^^ , ,,` case-conversion operators, taught
  `${var/pat/rep}` to expand variables in both halves and to accept an escaped
  `\/`, and extended `${var:offset:length}` to negative lengths.

- `source /opt/ros/humble/setup.bash` (and ament setup scripts generally) now
  works. Four Bash gaps were in the way: double quotes did not nest inside a
  backtick substitution, so `"`dirname "$f"`"` cut the command short;
  `${BASH_SOURCE[0]}` was unset, so a sourced script could not find its own
  directory; `:` was not a builtin; and `IFS` started unset, so the common
  `old=$IFS; ...; IFS=$old` idiom emptied it and killed word splitting for the
  rest of the session.
- Publish prebuilt Linux binaries (glibc and static musl, x86_64 and aarch64)
  from tagged releases, together with checksums and a `manifest.json` served at
  a stable "latest" URL.
- Added `scripts/install-jsh.sh`, the one-command install and update path. It
  verifies checksums, checks that the new binary identifies itself as jsh,
  swaps it in atomically, keeps the previous binary for rollback, and reports
  when `PATH` resolves `jsh` to an unrelated binary instead. `--check --json`
  reports installed and latest versions for tooling, backed by a shared cache
  at `~/.cache/jsh/update-check.json`.

## 0.2.0

- Added a documented CLI contract with help, version, syntax-check, stdin,
  startup-file, and session options.
- Corrected Bash-style `$0`/positional arguments, exit propagation, `shift N`,
  `errexit`, and rightmost-nonzero `pipefail` behavior.
- Unified top-level execution across command strings, scripts, stdin, and the
  interactive editor.
- Rejects unterminated quotes, substitutions, and here-documents consistently,
  and propagates INT/HUP/TERM to foreground jobs with conventional statuses.
- Hardened history and session persistence with private permissions, atomic
  writes, multiline-safe history, legacy migration, and secret filtering.
- Made AI assistance explicitly opt-in and removed environment-value leakage
  from completion descriptions.
- Removed the duplicate binary module graph so the executable uses the library
  implementation directly.
- Added end-user documentation and package metadata.

## 0.1.0

- Initial experimental release with Bash-compatible execution, structured
  pipelines, an interactive editor, completion, workflows, sessions, and
  optional AI integration.
