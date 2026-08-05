# Changelog

## Unreleased

- Tab completion understands more of what the argument actually is instead of
  falling back to filenames. The ssh family (`ssh`, `scp`, `sftp`, `rsync`,
  `mosh`, `ssh-copy-id`) completes destinations from `~/.ssh/config` aliases
  and `known_hosts` — a `user@` prefix is preserved, `scp`/`rsync` hosts come
  with the `:` already typed, and hashed `known_hosts` entries are skipped
  because they cannot be spelled back. `export`, `unset`, `readonly`,
  `declare` and `local` complete bare variable names (`unset -f` completes
  functions), `alias`/`unalias` complete alias names showing their expansion,
  `which`/`type`/`whereis`/`man` complete command names, and `fg`/`bg`/
  `wait`/`disown`/`kill` complete job specs labelled with the job's command —
  `kill` also offers PIDs and, after `-`, signal names. `${PREF<TAB>`
  completes braced variables closed (`${PREFIX}`), a `VAR=/pa<TAB>`
  assignment completes its value as a path with the assignment kept on the
  inserted text, and `z` ranks the frecency database's directories above the
  plain subdirectory listing.
- Ghost suggestions know every local branch, not just the current one. The
  once-per-prompt Git probe now also caches local branch names, most
  recently committed first, so `git checkout fea` ghosts `feature-x` while
  checkout and switch still prefer the current branch when both match — and
  `git merge`/`git rebase` suggest other branches only, never the one that
  is checked out. Outside a repository no extra Git process is spawned.
- Ghost suggestions follow the command being typed now, not the whole line:
  after `cargo build && git p` the ghost completes `push`, pipes and
  connectors included, with history, abbreviations, git context and the
  filesystem probe all matching against the active segment (whole-line
  history matches still win when they exist). `z` gets the same resolved-path
  preview `cd` has. And arriving in a directory suggests what is usually run
  there: after `cd`/`z`, an empty prompt ghosts the command most often typed
  in that directory — three occurrences before it counts as a habit,
  navigation commands never suggest more navigation.
- Completion forgives typing that is close but not exact. Path completion
  falls back to case-insensitive prefix matches when nothing matches the
  typed case — `cd doc<TAB>` completes `Documents/`, while an exact-case
  match keeps winning outright. Everything else falls back to fuzzy
  subsequence matches when nothing starts with the typed text — subcommand
  tables (`git chk<TAB>` finds checkout), Git refs, dirty files and remotes,
  ssh hosts, docker containers and images, systemctl units, users and
  groups, variable, alias, and bookmark names, npm scripts and dependencies,
  make targets, cargo arguments, and trap signals — and prefix matches keep
  their curated order, so nothing changes while the typing is exact.
  Completion lists from merged sources (pipe suggestions plus PATH commands,
  refs plus files) no longer repeat an entry; the higher-priority spelling
  wins.
- `systemctl` completes unit names probed from systemd itself, with the same
  bounded trusted-binary probe: `start`/`enable`/`mask` list unit files so
  units systemd has not loaded still complete, the rest list loaded units
  labelled with their active state and description, `--user` scopes the
  probe, and `journalctl -u`/`--unit=` completes the same names. Template
  units (`getty@.service`) are skipped — they cannot be operated on without
  an instance. `chown` completes users (and the group half once the `:` is
  typed), `chgrp` completes groups, `su`/`passwd`/`id`/`groups` complete
  users, and `sudo -u <TAB>`/`-g <TAB>` completes the option's value while
  still inside sudo's own option zone — human accounts sort ahead of system
  ones. `trap 'handler' <TAB>` completes signal and shell-condition names
  (EXIT, ERR, DEBUG, INT, …) with a word on when each fires.
- Docker arguments complete from the local daemon, probed the same bounded
  way Git arguments are (fixed trusted binary path, two-second timeout,
  byte-capped output): `docker exec`/`stop`/`restart` and friends complete
  running container names labelled with image and status, `docker
  start`/`rm`/`logs` include stopped ones, and `docker rmi`/`run`/`tag`
  complete image references. `docker container <sub>` and `docker image
  <sub>` reach the same completions one level deeper, and once `docker exec
  app` has its container the arguments beyond it belong to the command inside
  and complete as paths again. `kill` completes the user's own processes from
  /proc — newest first, labelled with the command name, the shell itself
  excluded — alongside the existing job specs. `npm uninstall`, `pnpm
  remove`, `yarn upgrade` and their spellings complete dependency names from
  the nearest package.json, labelled with the declared version.
- When no path matches at an argument position, Tab falls back to arguments
  the same command has been given before: `git checkout release-2<TAB>`
  recalls the branch spelling from history even though nothing in the working
  tree matches. Entries typed in the current directory come first, newest
  first, with quoted arguments kept exactly as typed; pipelines, `sudo`-style
  wrappers, and one-word aliases in old command lines are all seen through to
  the command they ran. `cd` gets the same treatment from the frecency
  database instead: local directories keep priority, and only when none match
  does `cd proj<TAB>` complete the frecent directory from anywhere.
- Entering a container that runs as a non-root user works again. The typed
  `docker exec -it <name> bash` upgrade streams jsh into the container's
  `/dev` tmpfs, and an image that says `USER ubuntu` had that write refused —
  the rewrite then gave up silently and the person landed in plain bash with
  no completion. The push now retries once as uid 0 when the default user is
  refused (whoever can reach the daemon already has root in the container);
  the session itself still runs as whoever was asked for, and the binary
  still lives only in the tmpfs, gone when the container stops.
- `install-jsh.sh` defaults to `--channel source` and refuses to downgrade a
  static build. A bare install now cargo-builds the static musl binary instead
  of consulting the release tree first, and a missing musl toolchain is an
  error naming the package to install (`musl-tools`) rather than a warning
  followed by a dynamically linked build that cannot lend itself into
  containers or onto ssh hosts. Staging (`--stage-dir`) keeps the release
  channel — a staged artifact is for another machine — and an explicit
  `--channel release` still falls back to source when no release exists.
  `JSH_INSTALL_TARGET=<arch>-unknown-linux-gnu` remains the way to ask for a
  glibc build on purpose.
- A helper under a user-private group is trusted again. Ubuntu, Debian and
  Fedora give each user a group of their own and ship umask 002, so every
  helper a person installs into their home — `~/miniconda3`, `~/.local`,
  `~/.cargo` — is mode 0775 with a group containing only them. Reading that
  bit as "somebody else can write here" refused all of it: on a stock Ubuntu
  the conda shell hook stopped loading and `conda activate` answered "Run
  'conda init' before 'conda activate'" in every new shell, with nothing said
  about why. The group bit is now judged by who is in the group instead — the
  owner's primary group with no other members grants exactly the access the
  owner bit already grants — while world-writable, a shared group, and a group
  the owner merely belongs to stay refused as before.
- `jsh-remote.sh` lends the local jsh instead of fetching a release. When the
  jsh on this machine is static and the destination runs the same
  architecture, it is the artifact: pushed as-is, verified by its banner after
  landing, cached on the far side under its own digest. No release lookup, no
  network, and the far side runs exactly the version that sent it — the same
  self-lending the shell itself does for a typed `ssh` or `docker` command,
  now for every terminal that drives the launcher from a host picker.
  Explicit `--artifact` and `--version` behave as before, a jsh already on
  the destination is still preferred over any transfer, and a dynamically
  linked local jsh still goes through published releases.
- A bare `install-jsh.sh` works before the first release: when the release
  manifest cannot be read it falls back to `--channel source` on its own,
  saying why and what it costs, and switches to verified artifacts the moment
  a release exists. Strictly a not-found fallback — staging still dies (its
  artifact is for another machine), and failed checksum verification still
  dies rather than building something else instead.
- A source build aims for the static musl target too. `--channel source` — and
  the automatic fallback to it when no prebuilt artifact fits — used to hand
  cargo the host's default target, so the one install path left produced the
  one binary that cannot lend itself out. It now targets
  `<arch>-unknown-linux-musl`, adds the std through rustup when it is missing,
  and gives the TLS dependency's C sources the musl compiler by the same
  variable the release workflow uses. When a piece is missing — no musl C
  compiler, no rustup — it builds for the host toolchain instead and says
  exactly what was missed (`sudo apt install musl-tools`) and what the dynamic
  result cannot do, rather than failing an install that would otherwise work.
  `JSH_INSTALL_TARGET` picks any other triple for source builds exactly as it
  does for release downloads.
- The Linux install is the static musl build. The installer used to pick a
  glibc artifact whenever the host's glibc was new enough, which produced a jsh
  that could not lend itself out: container entry bind-mounts the running
  binary and ssh entry pushes it, and both need it to be static. musl runs on
  every distribution and libc, `jsh-remote.sh` has only ever deployed musl,
  and now what you install is the same thing you deploy. The gnu artifacts are
  still published; `JSH_INSTALL_TARGET=<arch>-unknown-linux-gnu` selects one.
- `ssh build-box`, typed, arrives as jsh. An interactive ssh session with no
  remote command is routed through `jsh-remote.sh` — which travels inside the
  binary and is published to the cache on first use — with the running jsh as
  the artifact. The destination keeps its own login shell, and jsh's files
  land exactly where the launcher's persist mode has always put them: dot-files
  in the remote `$HOME`, the binary cached so the next connection skips the
  transfer. Everything else fails closed: a remote command, forwarding flags,
  any flag this shell does not recognise, a value the launcher would re-split,
  or a non-static running binary all leave the command exactly as typed.
  `command ssh …` bypasses it; `JSH_SSH_SHELL=off` ends it.
- Containers you just walk into. `docker run -it ubuntu bash` and `docker exec
  -it web bash` now give you jsh, with nothing installed in the container and
  nothing configured anywhere: for `run` the shell is a read-only bind mount of
  a static jsh over a path in the container's `/dev` — a tmpfs the runtime
  creates, so the image's writable layer never sees it and `docker diff` stays
  empty — and for a container already running, where a mount cannot be added,
  the binary is streamed into that same tmpfs. Any image works, including ones
  with no libc and no bash. Rewriting a typed command fails closed everywhere
  it is not obviously right: no terminal, a real command rather than a bare
  shell, an image with its own entrypoint, a remote `DOCKER_HOST` (whose `-v`
  would name someone else's filesystem), a platform this host has no binary
  for, or any flag that could be hiding where the image name is. `command
  docker …` and `JSH_CONTAINER_SHELL=off` are the ways out.
- An incognito container session now sandboxes itself in `/dev` rather than
  `/var/tmp`: same tmpfs, so nothing it does reaches the image's writable
  layer. Real machines are unaffected — there `/dev` is devtmpfs, and only the
  docker transport is offered it.
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
