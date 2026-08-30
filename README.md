# jsh

`jsh` is an experimental interactive shell that combines familiar Bash syntax
with typed, structured-data pipelines. It is built in Rust and includes a
multiline editor, job control, context-aware completion, session restoration,
local workflows, and optional AI-assisted command generation.

> `jsh` implements a broad and useful subset of Bash, but it is not yet a
> drop-in replacement for every Bash script. Keep `/bin/bash` as the interpreter
> for scripts that require exact Bash behavior.

## Highlights

- Bash-style commands, expansion, functions, arrays, redirections, traps, and
  foreground/background jobs.
- Structured JSON, YAML, TOML, XML, CSV, and NDJSON pipelines.
- Typed values, `let` bindings, closures, typed `def` functions, and reusable
  modules through `use`.
- Lazy streams (`range`, `from-ndjson`, `take`) and ordered parallel mapping
  with `par-each`.
- Interactive Emacs/Vi editing, fuzzy history search, Git-aware prompts,
  completions for common developer tools, bookmarks, directory frecency, and
  parameterized workflows.
- A continuous terminal with semantic command boundaries, allowing compatible
  terminals to present a Commands timeline without turning output into blocks.
- Optional OpenAI, Anthropic, or local Ollama integration. AI is disabled until
  explicitly enabled.

## Install

Install or update the released binary:

```sh
curl -fsSL https://github.com/beamiter/jsh/releases/latest/download/install-jsh.sh | sh
```

The installer downloads the build for the current platform, verifies its
checksum, and replaces the binary with `rename(2)`, so shells that are already
running keep the version they started with. It installs next to an existing
`jsh` when it finds one on `PATH`, and falls back to `~/.local/bin` otherwise.
Re-running it is how you update. Useful options:

```sh
./scripts/install-jsh.sh --check          # compare installed against latest
./scripts/install-jsh.sh --channel source # build from source; run from a checkout
                                          # it builds that tree, uncommitted work included
./scripts/install-jsh.sh --git            # build the published repository instead
./scripts/install-jsh.sh --source-dir DIR # build a checkout somewhere else
./scripts/install-jsh.sh --stage-dir DIR --target TRIPLE   # verify only, install nothing
./scripts/install-jsh.sh --help           # bin directory, pinned version, dry run
```

The previous binary is kept under `~/.local/state/jsh/rollback/` so a bad
release can be undone without a network connection.

> The installer identifies binaries by their `jsh --version` banner, never by
> name alone, and tells you when `PATH` resolves `jsh` to some other binary
> instead of this shell.

Build the current checkout:

```sh
cargo build --release
./target/release/jsh --version
```

Or install it into Cargo's binary directory:

```sh
cargo install --path .
```

The default build includes HTTP and AI-provider support. To build the shell core
without its HTTP client dependency:

```sh
cargo build --release --no-default-features
```

## Five-minute tour

Run an interactive shell:

```sh
jsh
```

Execute a command or a script:

```sh
jsh -c 'printf "hello %s\n" world'
jsh ./script.jsh one two
printf 'echo from-stdin\n' | jsh
```

Use structured data without a chain of text parsers:

```sh
jsh -c 'echo '\''[{"name":"Ada","age":36},{"name":"Lin","age":28}]'\'' \
  | from-json | where age -gt 30 | select name | to-table'
```

Files are decoded from their extension and can be converted on save:

```sh
jsh -c 'open users.json | where {|row| [ $row.active = true ]} | save active.yaml'
```

Typed functions and lazy pipelines extend the shell language:

```sh
jsh -c 'def add a:int b:int {|a,b| $a + $b}; add 3 4'
jsh -c 'range 1..1000000 | take 5 | each {|n| $n * $n} | to-json'
```

Discover the available commands from inside jsh:

```sh
help
help where
help --record where
```

Check whether the surrounding environment can provide startup compatibility,
persistent state, trusted helpers, and optional AI without starting a shell or
making a network request:

```sh
jsh doctor
jsh doctor --json
jsh doctor --strict                 # exit 1 when any warning is present
jsh doctor --rcfile ./team.jshrc    # inspect an explicit startup file
```

## Command line

```text
jsh [OPTIONS] [SCRIPT [ARG ...]]
jsh [OPTIONS] -c COMMAND [NAME [ARG ...]]
jsh context <list|show|last-failed> [OPTIONS]
jsh doctor [--json] [--strict] [--rcfile FILE]
```

Important options:

- `-c, --command COMMAND` executes a command string. As in Bash, the following
  `NAME` becomes `$0`, and later values become `$1`, `$2`, and so on.
- `-s, --stdin` reads a program from standard input.
- `-n, --noexec, --check` parses input without executing it.
- `-i, --interactive` requires an interactive terminal editor and cannot be
  combined with syntax-check mode or an explicit command, script, or stdin.
- `--norc` skips the interactive startup file.
- `--rcfile FILE` selects an explicit interactive startup file.
- `--session ID` restores and persists a named interactive terminal session.
- `--help` and `--version` report the binary's interface and version.
- `doctor [--json] [--strict] [--rcfile FILE]` performs a read-only environment
  check. JSON reports carry `schema_version` and `healthy`; strict mode exits 1
  on warnings. Reports describe credential presence, never values, and report
  malformed or unsupported Agent capability negotiation without echoing the
  supplied capability token.

Long options that take one value also accept `=` syntax, including
`--command=...`, `--rcfile=...`, and `--session=...`. Repeating `--rcfile` or
`--session` is an error rather than silently replacing the earlier value.

Startup and session options are accepted for command-line consistency but take
effect only when jsh starts its interactive editor; they do not alter `-c`,
script, stdin, or syntax-check execution.

CLI errors and syntax errors exit with status `2`. Command-not-found and
missing-script failures use `127`; commands or scripts that cannot be
executed or read use `126`.

An assignment-only command takes the status of its last command substitution,
as in Bash: `out=$(false)` returns `1`. That status also reaches `ERR` traps and
`set -e`, so a failed substitution cannot be mistaken for a successful setup
step. Inside the substitution, `exit` and an explicitly enabled `set -e` stop
the remaining commands before their output or status can replace the failure.
The parent shell's `errexit` and `ERR` trap are reset there by default, matching
Bash; `shopt -s inherit_errexit` and `set -E` opt into inheriting them.

## Startup and persistent state

Interactive shells import `~/.bashrc` by default for compatibility. Use
`--rcfile ~/.jshrc` for a native jsh startup file, or `--norc` for a clean
session. Non-interactive `-c`, script, and stdin execution do not implicitly
load interactive configuration or write interactive history.

History is stored at `~/.jsh_history`; named session snapshots live under
`~/.jsh/sessions`. New files are written with private permissions. History uses
a newline-safe JSONL format while retaining compatibility with the previous
tab-separated format. Session snapshots exclude process-specific variables and
names that look like credentials, tokens, passwords, or secrets.

### Helper programs

A few features start a system program: `bash` for the `~/.bashrc` import and for
`source` of a script jsh's own parser cannot read, `git` for the prompt, and
`notify-send` for background-job notifications. jsh looks for these at fixed
absolute paths and never through `PATH`, which is mutable shell state any
sourced script can rewrite.

On a layout that does not use those paths — Nix, a Homebrew-style prefix, an
immutable root — say where the program is:

```sh
export JSH_HELPER_GIT=/run/current-system/sw/bin/git
export JSH_HELPER_BASH=/run/current-system/sw/bin/bash
export JSH_HELPER_NOTIFY_SEND=/run/current-system/sw/bin/notify-send
```

The variable name is `JSH_HELPER_` plus the program name uppercased, with `-`
becoming `_`. The path must be absolute and, after symlinks are resolved, must
be an executable file that no third party can replace — neither it nor any
directory above it may be group- or world-writable or owned by another user.
A path that fails those checks disables that integration and says so once; it
does not quietly fall back to a different binary than the one you named.

## Containers you just walk into

Typing `docker run -it ubuntu bash` gives you jsh, without anything being
installed in the container and without configuring anything anywhere:

```
$ docker run -it ubuntu bash
jsh: entering the container as jsh (read-only mount, nothing installed)
user@f8cb8dfe26b7 / ❯
```

The shell is a read-only bind mount of a static jsh over a path in the
container's `/dev`. That is a tmpfs the runtime creates, and one of the few
places in a container that both executes files and stays out of the image's
writable layer, so `docker diff` on the container is empty afterwards. A
container that is already running cannot be given a mount, so `docker exec -it
web bash` streams the same binary into that same tmpfs instead — still nothing
in the writable layer, and gone when the container stops.

It works on any image, including ones with no libc and no bash: a static binary
needs nothing from the image. Alpine, distroless, and Ubuntu all behave the
same, and the container starts as fast as it did before.

This only happens for a command that is plainly someone going in to look
around, and every rule fails closed:

| | |
|---|---|
| rewritten | `docker run -it IMAGE bash`, `docker exec -it NAME sh`, `docker run -it IMAGE` when the image's own default command is a shell — and the same for `podman` and `nerdctl` |
| left alone | no `-t`; a real command (`docker run -it img bash -c …`); an image with its own `ENTRYPOINT`; an explicit `--entrypoint`; a `--platform` this machine has no binary for; a remote `DOCKER_HOST`; any flag this shell does not recognise, since one of those could be hiding where the image name is |

`command docker …` runs exactly what you typed, and
`JSH_CONTAINER_SHELL=off` turns the whole thing off. The binary jsh injects is
itself, when jsh is a static build; otherwise the artifact
`scripts/jsh-remote.sh` stages, or whatever `JSH_CONTAINER_BINARY` names. When
none of those is static, nothing is rewritten — a dynamically linked shell
would fail inside the image with a "no such file or directory" naming a file
that is plainly there.

Once inside, jsh behaves like the shell it replaced: it reads the container's
own `~/.bashrc` and keeps its history in the container's home, exactly where
bash keeps `.bash_history`. Only the binary is a guest.

## ssh hosts you just walk into

The same idea works over ssh, where there is no mount to add but there has
always been a push:

```
$ ssh build-box
jsh: bringing jsh to build-box for this session (`command ssh` connects plain)
yj@build-box ~ ❯
```

An interactive `ssh destination` — no remote command, only session flags this
shell recognises (`-p`, `-i`, `-l`, `-o`, `-J`, …) — is routed through
`jsh-remote.sh` with the running binary as a candidate. It is lent directly
only when the destination has the same architecture; a different architecture
uses its matching release when one exists and otherwise falls back to shell
integration (or plain ssh when integration is unavailable), so an unsupported
host never blocks login. The destination keeps its own login shell; jsh's
dot-files and a cached copy of the binary land in your remote `$HOME`, so the
next connection skips the transfer. Anything else — `ssh host ls`, port
forwarding, `-N`, `-W`, a flag this shell does not know — runs exactly as typed,
as does `command ssh …`, and `JSH_SSH_SHELL=off` turns it off. Lending needs the
running jsh to be static, which the released Linux binaries now are.

## Remote hosts and containers

`scripts/jsh-remote.sh` runs jsh on a machine that does not have jsh installed.
It places a static musl build there, executes it, and takes it away again.
When the jsh running here is itself static — which a Linux install now is —
that binary is the artifact: nothing is fetched from anywhere, and the far
side runs exactly the version that sent it. Releases are the fallback for a
dynamically linked jsh or a different architecture:

```sh
./scripts/jsh-remote.sh build-box            # ssh
./scripts/jsh-remote.sh --docker my-service  # a running container
```

Completion, the prompt, history search, workflows, and the OSC 133 marks that
drive a terminal's Commands timeline all behave exactly as they do locally,
because the shell doing the work really is jsh.

The destination keeps its own shell. Nothing edits `.bashrc`, `.profile`,
`.zshrc`, `/etc/profile.d`, or the login shell in `/etc/passwd`; nothing needs
root or a package manager; and nothing is downloaded on the far side — the
release artifact is fetched and verified here by `install-jsh.sh --stage-dir`
and then pushed over the same connection, so an air-gapped host works too.

Two modes, because "install nothing" means two different things:

| | `--persist` (default) | `--incognito` |
|---|---|---|
| remote `$HOME` | keeps `~/.jsh_history`, `~/.jsh/`, and a cached binary under `~/.cache`, all private to your account | never written to |
| history across sessions | kept | discarded |
| repeat connections | skip the transfer | transfer each time |
| use it when | the account is yours | the account is shared |

`--incognito` points `HOME` and the XDG variables at a sandbox that is deleted
when the session ends, and sets `JSH_REAL_HOME` to the account's actual home.

That variable separates two questions that are normally the same one. `~`, `cd`
with no argument, the `~/…` abbreviation in the prompt and in completions, and
the startup file all follow `JSH_REAL_HOME`, because those are paths a person
writes. `$HOME` keeps pointing at the sandbox, because that is where programs
write — so history, session snapshots, bookmarks, and the frecency database
still cannot escape it. Set it yourself and the same split applies; leave it
unset, or set it to something that is not an existing absolute directory, and
nothing changes.

When a destination cannot execute a file at all — everything writable is
mounted `noexec` — there is still a middle tier, and it is the default. `bash
--rcfile` *reads* its argument and never runs it, and `noexec` refuses `execve`
while permitting `write`, so jsh can hand the destination's own bash a
throwaway startup file that emits the same OSC 133 and OSC 7 marks jsh does.
The terminal keeps blocks, cwd tracking and exit codes; jsh's completion and
structured pipelines are not there, because jsh is not there. The destination's
own `~/.bashrc` still runs first, and the file is deleted with the sandbox.
`--fallback bash` asks for the unmodified shell instead, and `--fallback fail`
refuses to connect.

Useful options: `--session ID` to forward a terminal session id, `--rcfile FILE`
to push a startup file, `--artifact FILE` to ship a binary you built yourself,
and `--dry-run` to see the plan. `--help` lists the rest.

Any of the four jterm terminals can use this without changes — run it in a pane,
or configure it as the command for a tab.

## Semantic commands and execution context

jsh keeps the terminal as one continuous scrollback while exposing semantic
command boundaries to terminal emulators. A compatible terminal can build a
chronological Commands timeline, jump to the original prompt, copy a command or
its rendered output, and offer rerun actions without imposing a block-based
layout.

The integration retains the portable OSC 133 lifecycle: `A` begins a prompt,
`B` begins command input, `C` begins output, and `D` finishes the command. jsh
adds percent-encoded, size-bounded metadata to `C` and `D`: an execution ID,
the exact command when it fits the protocol limit, the working directory, exit
status, and duration. Oversized commands are explicitly marked as truncated
rather than being presented as exact. The execution ID correlates terminal
scrollback with jsh's structured context.
Commands containing non-structural controls or visually ambiguous formatting
take the non-exact OSC path and are refused by the journal writer. Newline and
tab remain valid multiline shell structure, but hidden formatting never enters
exact OSC or journal metadata.

Percent-encoded newline and tab bytes remain structural command text, so a
multiline command has the same identity in OSC metadata and the JSONL execution
journal. Repeated semantic metadata aliases are treated as ambiguous by the
shared terminal parser rather than resolved last-wins.

Display-only OSC values such as command titles and job notifications keep
ordinary Unicode readable but render controls, non-ASCII spacing, bidi
controls, zero-width text, and other default-ignorables as explicit
percent-encoded bytes. The redundant iTerm2 `CurrentDir` frame is emitted only
when the exact raw path is bounded and unambiguous; OSC 7 remains the canonical
encoded cwd signal. Cwd identity is never lossy-decoded or truncated: an
unrepresentable, oversized, or visually ambiguous value simply omits the
optional cwd metadata.

Query that context either inside an interactive jsh or from another process:

```sh
context list [-n N] [--session ID] [--json]
context show EXECUTION_ID [--json]
context last-failed [--json]

jsh context list [-n N] [--session ID] [--json]
jsh context show EXECUTION_ID [--json]
jsh context last-failed [--json]
```

`list` defaults to the newest 20 records and accepts a limit from 1 to 2,000.
It reports only output availability, truncation, and byte-count metadata;
`show` and `last-failed` include the captured output itself when available.

Execution context is separate from `~/.jsh_history`. Its append-only JSONL
journal defaults to `$XDG_STATE_HOME/jsh/executions.jsonl`, falling back to
`~/.local/state/jsh/executions.jsonl`. The jsh state directory is mode `0700`;
the journal and its `executions.lock` sidecar are mode `0600` and coordinated
with `flock`. Existing journal or lock files that are group/world-writable are
rejected rather than repaired in place; extra read bits on an owner-only file
are tightened back to `0600`. At 32 MiB the journal is compacted to the newest
records, with a post-compaction limit of 24 MiB and 2,000 executions. Individual
metadata and captured-output records also have hard size limits.
Working directories have no truncation bit in journal v1, so start/finish
events require the exact bounded, unambiguous value instead of recording a
prefix or a lossy UTF-8 replacement as though it were another real directory.
If a damaged or externally produced journal repeats an execution ID, the later
start event replaces the whole earlier lifecycle and takes its new chronological
position; stale status, output, and eviction age cannot attach to the new run.

The journal can contain sensitive commands, paths, and terminal output. Set
`JSH_EXECUTION_JOURNAL=0` to disable disk journaling while retaining OSC
integration for the terminal UI. Set `JSH_EXECUTION_JOURNAL_PATH` to override
the location; an empty value behaves like an unset override. A non-empty value
must be an absolute, terminal-visible file path no longer than 16 KiB whose
parent is owned by the current user and is not group/world-writable. Shared
namespaces such as `/tmp` are not valid overrides because the journal and its
fixed `executions.lock` sidecar share one trust boundary. A non-empty override
remains custom even when it spells the default path; jsh never repairs that
explicit namespace in place. Relative paths and locations containing controls,
terminal-invisible text, or other unsafe bytes are rejected.

## AI, explicitly opt-in

AI integration is opt-in. Select a provider when starting jsh:

```sh
JSH_AI_PROVIDER=ollama jsh
JSH_AI_PROVIDER=openai jsh
JSH_AI_PROVIDER=anthropic jsh
```

For cloud providers, inject `OPENAI_API_KEY` or `ANTHROPIC_API_KEY` beforehand
through your normal secret manager or protected environment configuration; do
not type secrets directly into a recorded command line. `JSH_AI_MODEL` and
`JSH_AI_BASE_URL` override provider defaults. Requests include your prompt,
OS, and current-directory path. Cloud requests do not additionally include
recent history or Git status unless
`JSH_AI_SHARE_CONTEXT=1` is set. Generated commands are suggestions: inspect
them before execution, especially when they contain destructive operations.
Plain HTTP is accepted only for a syntactic loopback origin, for any provider;
remote endpoints require HTTPS. This supports local OpenAI-compatible and
Anthropic proxies as well as Ollama without weakening cloud transport. A
validated loopback HTTP request also bypasses environment proxy settings so
its credentials cannot leave the loopback hop; HTTPS keeps the user's normal
proxy configuration.
Every editor-AI history window uses jagent's reported request builder. If
older turns are omitted, jsh tells the model that its context is incomplete;
that notice and the trusted system instructions share a strict 64 KiB byte
ceiling. jsh fails the request rather than truncating the system prompt or
silently discarding the notice. Agent mode uses the stricter prepared request
path described below.

The interactive editor exposes three review-first AI actions:

- Type `# describe the command you want` and press Enter to request a command
  suggestion. The reply stays as ghost text until you explicitly accept it.
- Press `Ctrl-F` after a failed command to request a corrected suggestion.
- Press `Alt-E` with a command on the line to open a read-only explanation.
  Explanations use a separate model contract and display panel; they can never
  become command text or be submitted with Enter.

Each request carries an editor generation ID. Leaving the prompt with
`Ctrl-C`, submitting, editing while a response is pending, or starting another
request invalidates the old ID, so a late provider response cannot replace
newer input.

### Agent mode

The `agent` builtin runs a review-first agent loop on the shared
[jagent](https://github.com/beamiter/jagent) core (the same state machine used
by the anvil, ember, forge, and frost Shell Agent integrations):

```sh
agent find the largest files under target and free some space
```

The model may only *propose* one command per turn. Every proposal shows a
`[y] run  [e] edit  [i] insert  [n] reject  [q] quit` review prompt; recognized
dangerous commands additionally require typing `RUN`. `[i]` moves the command
into your next editor prompt for manual review without executing it and ends
the session. Approved commands execute through the normal jsh parser with
output teed to the terminal, and a bounded sample plus a real exit code is fed
back as the next model turn's observation. If the private snapshot, pipe, or
child boundary cannot start—or the boundary terminates without a normal
status—jsh records an explicit execution failure instead of fabricating exit
code 1. Parent and child coordinate over a separate one-byte readiness pipe:
the child closes it only after snapshot claim/load, state restore, and cwd
setup succeed, before parsing or executing the user command. stdout/stderr can
never forge that signal. Authentication requires one exact marker followed by
EOF. Control, final-cwd, and one-shot nonce descriptors must be three distinct
FIFO identities, not aliases under different fd numbers. The final cwd is a
nonce-bound length-prefixed frame: its prefix is checked incrementally, reads
have fixed per-pass work budgets, and malformed input closes the parent reader
so a writer cannot turn rejection into a shutdown hang. A signal or
status-observation failure after READY is
reported through jagent 0.7's conservative `Cancelled` compatibility bucket;
it is not claimed to be a normal shell exit. Malformed model replies fail
closed and never become proposals; duplicate JSON object members are rejected
recursively instead of being resolved by decoder order.

Agent requests use jagent 0.7's bound request/response path: the selected
system prompt, provider schema, delivery protocol, secret redaction report,
response decoder, and session ingestion cannot drift apart. The compatible
default is the JSON-in-text protocol over one complete response. A terminal or
other peer can advertise its protocol/delivery support through the canonical,
at-most-256-byte `JSH_AGENT_PEER_CAPABILITIES` token, for example:

```sh
export JSH_AGENT_PEER_CAPABILITIES='jagent-agent/1;protocols=text,native-tools;delivery=complete'
export JSH_AGENT_PROTOCOL=native-tools
```

If the peer variable is absent, jsh assumes only the legacy `text+complete`
path; discovery never silently opts an existing integration into native tools.
An explicit `JSH_AGENT_PROTOCOL` remains authoritative, but jsh rejects it
unless both the selected provider and peer advertise that protocol with
`complete` delivery. The current transport does not negotiate streaming.
Malformed, non-canonical, future-version, whitespace-bearing, duplicate, or
oversized peer tokens are rejected without being printed. All three built-in
providers support native `run`/`say`/`done` calls. Tool calls remain proposals
and pass through exactly the same review prompt—selecting the native wire
format never grants execution permission. Run `jsh doctor` to inspect the
effective negotiation without contacting the provider or exposing credentials.
Capability-token v2, which can express exact protocol/delivery pairs rather
than a Cartesian product, remains an explicit peer-aware opt-in; default
emission is v1 for rolling-upgrade safety. jsh exact-pins the revision that
supports both and replies in the decoded peer's schema version.

Before either the ordinary AI client or the independently invokable hidden
Agent transport child touches DNS or an HTTP socket, it revalidates the decoded
public request's origin, canonical unique headers, JSON-object body, and byte
ceilings, including unique body members at every depth. Malformed child envelopes
and unknown provider names are rejected without echoing caller-controlled
values; duplicate or unknown outer envelope fields are rejected as well.
Successful ordinary editor AI replies remain bounded raw bytes until jagent's
canonical decoder has rejected recursive duplicate members, so an ambiguous
last-wins value can never become a suggestion. Both ordinary and Agent
provider responses are capped again after transparent content decoding
(currently gzip), so compressed input cannot expand beyond the same
encoded-envelope ceiling in memory.

The agent keeps its own working directory: an approved `cd` carries into the
following turns (shown as `cwd → …`), while the interactive shell's cwd is
never touched.

`JSH_AGENT_MAX_TURNS` (default 16) bounds the model-turn budget. The former
`JSH_AGENT_AUTO_APPROVE_READONLY` switch is retired: command text cannot prove
what aliases, functions, Git helpers, or tool flags will actually execute, nor
whether a seemingly harmless read would disclose a secret to the model. If the
old switch is still set, jsh warns and continues to require approval for every
proposal. Git branch/dirty metadata is attached only under the same
`JSH_AI_SHARE_CONTEXT` rules as other cloud context. Agent commands run in a
fresh one-shot jsh child initialized from a private, size-bounded snapshot that
is atomically claimed by only one child process. Aliases, functions, variables,
and shell options are therefore available without letting `export` or other
mutations change the interactive shell's state (`cd` persists only within the
agent session). AI worker queues hold at most one bounded request and response;
a detached descendant inheriting the output pipe cannot hold the Agent turn
open after the direct child exits. `Ctrl-C` also releases the Agent promptly
while a provider connection is stalled: cancellation kills and reaps the
transport process group, so there is no abandoned prior request or
single-flight shutdown gate blocking the next one.

Local context queries never send journal data over the network. Local Ollama
may use the most recent failed execution's captured terminal output for command
repair. Cloud providers receive execution output only when
`JSH_AI_SHARE_CONTEXT=1` explicitly opts in; otherwise AI repair falls back to
the command and exit status. Review journal contents before enabling cloud
context sharing because terminal output can contain source code, paths, tokens,
or other secrets.

## Completion

Tab completes by what the word actually names, not by filename alone.

- **Commands and syntax**: builtins, functions, aliases, `PATH` entries and
  shell keywords; the word after `do`, `then`, `else`, `|`, `&&`, `` ` `` and
  `$(` is a command position; redirection targets are files, and `2>&` takes a
  descriptor.
- **Repository state**: branches, tags, remotes, stashes, recent commits,
  modified files (`git add`), worktrees, and `git config` keys. `git branch -d`
  offers only branches Git would let you delete.
- **The machine's own state**: users and groups, jobs and signals, processes
  for `kill`, systemd units, Docker containers and images, ssh hosts from
  `~/.ssh/config` (following `Include`) and `known_hosts`, kubeconfig contexts
  and namespaces, `nvm`/`pyenv`/`rbenv` versions, and a project's virtual
  environment.
- **Project files**: npm scripts and dependencies, `make` targets, Cargo
  binaries, examples, features and packages, and Compose services.
- **Typed pipelines**: `from-json data.json | where <TAB>` offers the fields
  that file actually has, with the type each holds, across JSON, NDJSON, YAML,
  TOML and CSV. A `def` function's parameters complete by their declared type.
- **Flags and their values**: descriptions for everyday tools, and fixed value
  sets where they exist — `curl -X`, `git log --pretty=`, `find -type`,
  `journalctl -p`, `systemctl --state=`, `kubectl -o`, `test` operators,
  `chmod` modes, `man` sections.

Typing that is close but not exact still finds its target: paths fall back to
case-insensitive matching, and everything else to fuzzy subsequence matching,
only when nothing matches exactly — so precise typing keeps its precise order.
The menu underlines the characters that matched. Accepting a candidate is
remembered per command, so the next list leads with what you chose before.

Everything is local. Completion never runs a command to find out what to
offer, and never makes a network request: Docker, Git and systemd are probed
with fixed, trusted binaries under a timeout and an output cap, once per
command line; everything else is read from files. Kubernetes resource names
and remote `scp` paths are deliberately absent for exactly this reason.

`complete` and `compgen` accept the bash spellings, including `-A <action>`,
which resolves to the sources above, and `-F <function>`, which is called with
`COMP_WORDS`, `COMP_CWORD`, `COMP_LINE` and `$1`/`$2`/`$3` as bash sets them.
Both reach one implementation of each action, so a script and a keystroke
cannot disagree about what a user or a service is.

Specifications ship for `git`, `docker`, `cargo`, `kubectl`, `npm`, `gh`,
`systemctl`, `tmux`, `terraform` and `apt`, and more can be placed in
`~/.jsh/completions/`. A spec's argument may name a `generator` to reach any
of the dynamic sources above, and a spec may `wraps` another command when it
is the same interface under a different name — `podman` ships as nothing but
a link to `docker`.

`debug-completion '<command line>'` shows what completion would answer at the
end of that line, and which source answered: a list says what is on offer but
never why, and why is the first question when an answer looks wrong.

## Workflows

Press `Ctrl-G` to search the local workflow registry. Choosing a workflow walks
through its parameters, showing descriptions, defaults, and suggestions, then
places the rendered command on the line for review; it never executes the
command. `Esc` cancels and restores the line that was present before opening
the workflow picker.

Workflow files live in `~/.jsh/workflows/`. Each `.json` file can contain one
workflow or an array. A minimal definition is:

```json
{
  "name": "serve",
  "description": "Serve a directory over HTTP",
  "command": "python3 -m http.server {{port}} --directory {{path}}",
  "parameters": [
    {"name": "port", "default": "8000", "suggestions": ["8000", "8080"]},
    {"name": "path", "description": "Directory to serve", "default": "."}
  ],
  "tags": ["python", "http"]
}
```

Only names declared in `parameters` are substituted. Other moustache syntax is
left untouched, so commands may safely contain Docker/Go/Helm expressions such
as `{{.Id}}`.

Use `workflow list` (or `wf list`) to inspect names and
`workflow show NAME` to inspect a template without entering the picker.

## Development

The main verification commands are:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked --no-fail-fast
cargo test --no-default-features --locked --no-fail-fast
cargo doc --all-features --locked --no-deps
cargo build --release --all-features --locked
shellcheck -s sh scripts/install-jsh.sh scripts/jsh-remote.sh
./scripts/test-install-jsh.sh
./scripts/test-jsh-remote.sh
```

Benchmarks are available through `cargo bench` and the comparison scripts
`bench.sh` and `bench_nu.sh`.

## Current compatibility boundaries

- Startup-file import can transfer environment variables, aliases, and selected
  shell options, but not every arbitrary Bash function or interactive plugin.
- Some advanced Bash options and edge cases remain incomplete. Prefer an
  explicit Bash shebang for production scripts that depend on exact Bash
  parsing or `set -e` corner cases.
- Structured pipeline commands are jsh extensions and are not portable to Bash.
- HTTP and AI features are available only in builds with the `ai` Cargo feature.

Please include the smallest reproducing command, expected status, actual status,
and platform details when reporting a compatibility issue.
