# Changelog

## Unreleased

- Interactive SSH entry now accepts recognised session options after the
  destination, matching OpenSSH spellings such as `ssh host -p 2222`, so these
  sessions still receive remote jsh completion and suggestions.
- `jsh doctor` now performs a read-only health check across the runtime,
  startup file, persistent-state namespaces, trusted helpers, and opt-in AI
  configuration. `--json` emits a stable support-tool envelope, and the report
  never starts a helper, contacts a provider, or includes credential values.
- Directory frecency now records interactive navigation only. A `cd` inside a
  script no longer mutates `~/.jsh_z`, and a read-only HOME can no longer add an
  unrelated z-jump warning to otherwise successful non-interactive output.
- The next diagnostics/CLI pass adds ten concrete refinements: doctor strict
  status for CI; a versioned JSON schema with an explicit health bit; explicit
  rcfile diagnosis; `JSH_REAL_HOME` awareness; startup readability, size and
  symlink checks; private persistence-file integrity checks; accurate separate
  session and execution-journal namespaces including journal overrides;
  interactive `TERM` validation; `--command=`, `--rcfile=` and `--session=`
  forms; and rejection of duplicate single-value options plus broken-pipe-safe
  help/version output.

- History, z-jump and the execution journal are kept in a directory the user
  owns, whatever its mode bits say. Refusing a group- or world-writable parent
  was a second, weaker statement of something the file's own descriptor already
  proves — `O_NOFOLLOW`, one hard link, owned by this user, forced to 0600, and
  created `O_EXCL` under a name only this process knows. What it did prove was
  that a container image shipping `$HOME` at 0777 gave every shell inside it no
  history at all, and a line saying so at every start. A directory belonging to
  another account is still refused, and so is a data file that another account
  owns.

- The jsh a container is entered with is identified by its bytes, not by its
  version. `docker exec` puts a copy in the container's tmpfs and skipped that
  when one was already there, deciding "already there" by asking it for
  `--version` — which is `CARGO_PKG_VERSION`, and so is equal for every build
  between two releases. A container that stayed up went on running the copy
  from before a rebuild, and a fix built for that container looked as though it
  had not worked. A marker written beside the binary now names the exact bytes
  in the tmpfs; a copy placed by an older jsh has no marker and is replaced
  once.

- `~/.bashrc` is imported as the interactive file it is. Every distribution
  opens it with `case $- in *i*) ;; *) return;; esac`, and the helper bash jsh
  hands it to was not interactive, so the file returned at its fourth line and
  the import came back empty — no PATH entries, no aliases, no `conda init`
  block. Nothing reported it; the first sign was `conda activate` answering
  "run 'conda init' before 'conda activate'" in a shell whose `conda init` had
  been run years ago. The helper is now `bash --norc -i`, which is also what
  the `source` builtin uses when it falls back to bash.
- A helper that is interactive wants a terminal, and jsh has one. Asking for
  the foreground process group from a background one is answered with
  `SIGTTOU`, which stops the helper rather than failing it, so the startup
  import died on its deadline instead. Helpers that source a startup file now
  run in a session of their own, where there is no terminal to ask for. Their
  own "no job control in this shell" notice no longer reaches the user, while
  real errors from the file still do.
- The conda shell hook is loaded from a `$CONDA_EXE` under the user's own
  directories even when the permissions there are loose, which in the
  container images this keeps being reported from means mode 0777. jsh has
  already sourced the equally loose `.bashrc` that named the binary, and bash
  has already run it, so refusing it lost the hook without closing anything.
  A `$CONDA_EXE` owned by a third account is still refused — and said so.
  Every way of failing to load the hook now names the path and the reason,
  where each one used to be a bare `return`.

- A source install builds the checkout it is run from, uncommitted work
  included. `./scripts/install-jsh.sh` used to hand the build to
  `cargo install --git` regardless, so the local fix being tested — the reason
  to run the installer from a working tree at all — was left out of the binary
  that got installed, and the version banner gave no hint of it. Piped from
  curl there is no checkout to find and the repository build is unchanged.
  `--source-dir DIR` names a tree explicitly, `--git` asks for the published
  repository from inside a checkout, and `--version` still means a published
  build: it keeps the repository unless the named tree really is that version.
  The build line now says which tree it read and whether that tree was dirty.

- `compgen` and `complete -A` now reach the same implementation of each
  action the Tab completer uses, so a script and a keystroke cannot disagree
  about what a user, a service or a builtin is. `compgen` previously carried
  its own smaller answers for seven of them.
- Specifications ship for `gh`, `systemctl`, `tmux`, `terraform` and `apt`
  alongside the existing five, and a spec can now say it `wraps` another
  command when it is the same interface under a different name — `podman`
  ships as nothing but a link to `docker`. A test checks every shipped spec
  parses, passes the safety bounds, and describes each subcommand and option
  it offers, so a spec that would be silently dropped at load time fails the
  build instead.
- Git aliases complete as the subcommands they are: `git co<TAB>` offers the
  alias this repository defines, described by what it stands for. `--` ends
  the options, so `rm -- -f<TAB>` means the file named `-f`.
- Typing with the completion menu open narrows it rather than closing it —
  the list opened because the word was ambiguous, and continuing to type is
  the natural way to resolve that. Backspace widens it again, and a character
  no candidate has closes the menu and leaves the word as typed.
- `debug-completion '<command line>'` reports what completion would answer at
  the end of that line and which source answered — spec, probe, history
  fallback, cache, or one of the argument kinds. A completion list shows what
  is on offer but never why, and why is the first question when an answer
  looks wrong.
- The completion engine is now a module tree rather than one file: reading
  the command line, ranking candidates, and the static tables each have their
  own file and their own explanation of what belongs there.

- Bash completion scripts work as written. `complete -A <action>` resolves to
  this shell's own sources — users, groups, hostnames, services, jobs,
  signals, aliases, functions, builtins, keywords, variables, exports, shopt
  and set options — as do the short spellings (`-c`, `-u`, `-v`, …), and
  actions jsh has no notion of yield nothing rather than something wrong. A
  `-F` function now receives the whole command line: `COMP_WORDS`,
  `COMP_CWORD`, `COMP_LINE` and `$1`/`$2`/`$3` as bash sets them. It
  previously saw only the typed prefix, so every function that looked at
  `${COMP_WORDS[1]}` — which is most of bash-completion — concluded it was
  completing a command name.
- A `def` function's parameters complete by their declared type: a `path`
  parameter completes paths, a `bool` offers true and false, a union offers
  each member, and a type that could be anything falls through to the ordinary
  completions rather than inventing a list. Each candidate names the parameter
  it is filling.
- Ghost suggestions can be taken one word at a time with `Ctrl-Right` or
  `Alt-F`, keeping the rest as a ghost — a suggestion is often right at the
  start and wrong at the end. Paths arrive one directory per press. `cd -`
  completes with where it would go back to, and `cd ..` completes upward with
  the directory each level lands in.
- The completion menu underlines the characters the typed text matched, so a
  fuzzy match explains itself: `chk` landing on `checkout` is no longer
  arbitrary.
- Completing a variable no longer rebuilds every name on each keystroke; the
  list is built once per shape of the environment and ranked in place. A
  repeated `$JSH<TAB>` went from 159µs to 8µs.
- `2>&<TAB>` completes file descriptors rather than files, and `!<TAB>`
  completes history expansions — `!!`, `!$`, `!^` and `!prefix` — each showing
  the command it would reach.
- A randomised test drives completion over deliberately awkward command lines
  — unclosed quotes, escapes, substitutions, multi-byte text, redirections —
  at every cursor position, asserting that nothing panics and that the offset
  it returns is always a usable character boundary.

- Completion remembers what was taken. Accepting a candidate records it
  against the command whose argument it was, scored by the same frecency `z`
  uses for directories, and the next list leads with what was chosen before —
  among candidates the typed text matches equally well. It is a ranking hint
  only: nothing is added to a list because of it and nothing is removed, the
  command name itself is never learned (there the typed prefix decides), and
  a habit that stops being one fades. Ghost text reads the same record, so
  `git checkout ma` fills in the branch that gets chosen without waiting for
  Tab. The record lives in `~/.jsh_completions`, private and bounded.
- A multi-word alias completes as the command it stands for. With `alias
  gs='git status'`, `gs --<TAB>` is a `git status` flag position and `dc
  lo<TAB>` reaches `docker compose logs` — the head word is expanded as many
  times as it names an alias, with `alias ls='ls --color=auto'` expanding
  once as the shell itself resolves it, and a cycle terminating.
- Value-aware builtins complete field names from the pipeline's own source:
  `from-json orders.json | where cust<TAB>` offers `customer`, labelled with
  the type it holds, and the same for `select`, `sort-by`, `group-by` and
  their siblings across JSON, NDJSON, YAML, TOML and CSV. The file is parsed,
  never executed, and only when an earlier stage names one — a pipeline fed
  by a command's output would have to run that command to know.
- Options whose value is a fixed set now offer it, in both spellings:
  `curl -X <TAB>`, `git log --pretty=<TAB>`, `find -type <TAB>`, `journalctl
  -p <TAB>`, `systemctl --state=<TAB>`, `kubectl -o <TAB>`, `docker
  --restart <TAB>`, `ps -o <TAB>` and more, each value with a word on what it
  means. `test`, `[` and `[[` complete their operators the same way, `chmod`
  completes both numeric and symbolic modes with what each grants, and `man
  3<TAB>` completes manual sections.
- Completing at command position no longer rebuilds its candidate list on
  every keystroke. The list of every builtin, keyword, alias, function and
  PATH entry is built once and rebuilt only when one of those changes, and
  ranking now scores candidates in place instead of cloning several thousand
  of them per Tab: a repeated Tab at command position went from 671µs to
  4.6µs.

- Path completion understands the prefixes people actually type. `~alice<TAB>`
  completes user names as the home directory they stand for, `~alice/pro<TAB>`
  scans that home, and `$HOME/pro<TAB>` and `${HOME}/pro<TAB>` scan the
  variable's value — while the candidate keeps the spelling that was typed, so
  a `$HOME/` stays `$HOME/` in the command line instead of being expanded or
  escaped into `\$HOME/`. Candidates are now built by appending to the text
  already on the line rather than re-escaping the whole path, which is what
  makes that possible.
- Commands are recognised where shell syntax puts them: after `do`, `then`,
  `else`, `elif`, `if`, `while`, `until`, `!` and `{`, so `while read line; do
  gr<TAB>` completes a command instead of an argument, and inside backtick
  substitution as well as `$(…)`. Shell keywords themselves complete at
  command position, each with a word on what it opens.
- `git` completion reaches further: `git config` offers the keys already set
  here alongside the well-known ones, `git branch -d` offers branches only —
  never a tag, a remote ref, or the branch that is checked out, which Git
  would refuse — `git worktree remove` offers worktree paths labelled with
  what is checked out in each, and `git tag -d` offers tags.
- Flags for everyday tools now complete with an explanation of each: `ps`,
  `df`, `du`, `curl`, `wget`, `sed`, `awk`, `xargs`, `sort`, `uniq`, `head`,
  `tail`, `wc`, `rsync`, `journalctl`, `systemctl`, `ssh`, `scp`, `jq`, `ln`,
  `mv`, `ping` and `diff`. The existing table for `ls`, `grep`, `find` and
  friends was unreachable at the first argument — `ls -<TAB>` never consulted
  it — and now is.
- A completion spec can name a dynamic source. `{"template": {"generator":
  "git_branches"}}` in a spec file reaches the same branch, host, container,
  unit, service, context and project sources the built-in completions use.
  A generator is a fixed name resolved inside the shell, never a command
  line: a spec file is data, and data that could run a program on Tab would
  make every downloaded spec an execution vector. Unknown names yield
  nothing, so a spec written for another shell degrades quietly.
- `source <TAB>` finds a project's virtual environment: the activate script
  leads the listing, three directories deep as it is, with the ordinary
  listing behind it. `nvm`, `pyenv`, `rbenv` and `jenv` complete the versions
  installed here, newest first, read from the directory each keeps them in —
  asking the tool itself would mean sourcing a shell function on Tab.
- The completion menu answers the arrow keys while it is open, instead of
  walking off into history with no way back, and Shift-Tab and Up wrap around
  at the ends. The selected row keeps its description — it is the one whose
  meaning is being asked for — and a scrolled menu says how many matches
  there are and where the selection sits among them.

- Completion probes run once per command line instead of once per keystroke.
  Every external probe and its result — Git refs and status, the Docker
  daemon, systemd, and the decoded history file — is remembered for the
  command being typed and dropped when it runs, so a growing prefix stops
  re-forking Git. Failures are remembered too: a stopped Docker daemon costs
  one timeout while a word is typed rather than one per keystroke. A repeated
  Tab through the history fallback went from 584µs to 0.7µs, and fuzzy
  ranking itself is about 40% cheaper — it no longer lowercases the pattern
  once per candidate, allocates nothing for candidates that are already
  lowercase, and its run bonus is no longer an accidental O(n²) scan that
  compared against the wrong character. `benches/bench_completion.rs` covers
  the paths a keystroke actually takes.
- Redirection targets complete as plain files wherever they appear: `git add
  > n<TAB>` and `cd > n<TAB>` offer the file being written rather than dirty
  files or directories. `>&` is left alone, since its operand is a file
  descriptor. Completing inside an unclosed quote keeps the quoting style
  instead of replacing it with backslashes — a file closes the quote, a
  directory leaves it open because the path continues.
- Commands that open one kind of file offer that kind: `source` and the
  shells offer scripts, `python` offers `.py`, `unzip` offers archives,
  `java` offers jars. Directories always remain, since they are the way to
  reach the file, and a directory holding nothing of that kind keeps its
  whole listing rather than showing an empty menu — the guess is a
  convenience, never a restriction.
- jsh's own builtins complete their arguments: `shopt` and `set -o` name
  their options with what each does and whether it is on by default, `hook
  add` names the hook kinds and then any function, `hook remove` names only
  the hooks actually registered for that kind, and `workflow` names the
  workflows in the registry with their descriptions.
- ssh completion follows `Include`. Aliases now come from the whole config
  chain — `Include config.d/*.conf`, `~` paths, and bare relative paths
  resolved against `~/.ssh` the way ssh resolves them — with a file read at
  most once, so an include cycle terminates, and a depth and file-count
  bound so a keystroke stays cheap. A `Match host …` condition is no longer
  mistaken for a host alias.
- `docker compose` and `docker-compose` complete service names from the
  project's own compose file, labelled with the image or build context, so
  the services about to be started complete before any of them is running.
  `kubectl` completes contexts, clusters, users and namespaces from the
  local kubeconfig — `-n`, `--context`, the inline `--flag=` forms, and
  `kubectl config use-context`, plus `kubectx`/`kubens` — with the current
  context marked as such. `KUBECONFIG` is read from this shell's own
  environment, so an `export` typed at this prompt takes effect at the next
  Tab. Deliberately file-only: a resource name would mean a network round
  trip to a cluster that may be unreachable, and a keystroke must never be
  that.

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
