#!/bin/sh
# Run jsh on a machine that does not have jsh installed.
#
#   jsh-remote.sh build-box
#   jsh-remote.sh --incognito root@10.0.0.7
#   jsh-remote.sh --docker my-container
#
# A static musl jsh is placed on the destination, executed, and taken away
# again. The destination keeps its own shell: nothing here edits .bashrc,
# .profile, .zshrc, /etc/profile.d, or the login shell in /etc/passwd, and
# nothing needs root or a package manager. Other users of that machine cannot
# tell this ran.
#
# Two modes, because "do not install anything" means two different things:
#
#   --persist (default)  jsh keeps its own dot-files in your remote $HOME
#                        (~/.jsh_history, ~/.jsh/) and caches the binary under
#                        ~/.cache, so history and frecency survive and repeat
#                        connections skip the transfer. Private permissions,
#                        your account only.
#
#   --incognito          HOME points at a sandbox that is deleted when the
#                        session ends. Your real remote $HOME is never written
#                        to, which is what a shared account needs. The price is
#                        a fresh transfer each connection and no history across
#                        sessions.
#
# Design notes that are easy to get wrong and expensive to rediscover:
#   * Nothing is downloaded on the destination. The artifact is fetched and
#     verified *here* by install-jsh.sh --stage-dir, then pushed over the
#     transport, so an air-gapped host works and there is still exactly one
#     copy of the download-and-verify logic.
#   * musl only. A static binary has no glibc floor, so one artifact per
#     architecture covers Alpine, ancient CentOS, and everything between.
#   * Sandboxes are created with mktemp -d, never with a predictable name, so
#     nothing in a world-writable /tmp can be pre-created by another user and
#     written through.
#   * The binary cache lives under $HOME and nowhere else. Only its owner can
#     write there, which is what makes reusing a fixed path safe at all.
#   * Cleanup cannot run on the destination: the session ends in exec(), which
#     replaces the shell that would have carried the trap. The local side tears
#     down when the transport returns, and any sandbox orphaned by a crash is
#     swept by the next connection.

# The large single-quoted strings below are programs for the destination shell.
# Local expansion would cross the trust boundary and run them with the wrong
# environment, so each payload carries a narrow SC2016 annotation where it is
# constructed.

set -eu

CACHE_HOME="${XDG_CACHE_HOME:-${HOME:-}/.cache}"
# Where verified artifacts for other machines accumulate on *this* host.
ARTIFACT_CACHE="${JSH_REMOTE_ARTIFACT_CACHE:-${CACHE_HOME}/jsh/remote}"
# Long enough for a 7 MiB push over a slow link, short enough that a black-hole
# route fails the connection instead of hanging a terminal tab forever.
SSH_CONNECT_TIMEOUT=10
# How old a sandbox with *no recorded owner* must be before it is swept. It only
# has to outlast the gap between mktemp -d and the session writing its pid file,
# which is one round trip; an hour is generous on purpose, and costs nothing
# because a sandbox that did record an owner is collected the moment that owner
# is gone rather than waiting for this.
SWEEP_MIN_AGE_MINUTES=60

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
INSTALLER="${SCRIPT_DIR}/install-jsh.sh"

transport="ssh"
destination=""
mode="persist"
want_version=""
artifact=""
session_id=""
rcfile=""
no_rc=0
fallback="integration"
deploy=1
keep_sandbox=0
docker_user=""
dry_run=0
verbose=0
ssh_extra="${JSH_REMOTE_SSH_ARGS:-}"
local_jsh=""

# Filled in as the connection proceeds; the trap reads them.
sandbox=""
tmp_rc=""
cleanup_done=0

usage() {
    cat <<'USAGE'
Usage: jsh-remote.sh [options] <destination> [-- <extra ssh args>]

Destination:
  <destination>        ssh destination, [user@]host
  --docker CONTAINER   a running container instead of an ssh host
  --docker-user USER   run as USER inside the container

Modes:
  --persist            (default) jsh keeps its dot-files in your remote $HOME
                       and caches the binary there; history survives, repeat
                       connections skip the transfer
  --incognito          sandboxed HOME, wiped when the session ends; your real
                       remote $HOME is never written to

Options:
  --version VERSION    jsh release to deploy (default: the local jsh's version)
  --artifact FILE      push this binary instead of fetching a release
  --session ID         pass --session ID to the remote jsh
  --rcfile FILE        push FILE and start from it (default in --incognito:
                       ~/.jshrc here, else the destination's ~/.bashrc)
  --no-rc              start with --norc
  --fallback MODE      when deployment is impossible:
                         integration (default) shell integration only, with no
                           file executed on the destination; blocks, cwd and
                           exit codes work, jsh's own features do not
                         bash        the destination's own shell, unmodified
                         fail        refuse to connect
  --no-deploy          never push; use a jsh already on the destination or fall
                       back
  --keep-sandbox       leave the sandbox behind (debugging)
  --dry-run            probe and print the plan, deploy nothing
  -v, --verbose        narrate each step
  -h, --help           Show this help

Everything after `--` is passed to ssh. It is split on whitespace, so an
argument containing a space has to go through JSH_REMOTE_SSH_ARGS instead.

Environment:
  JSH_REMOTE_SSH_ARGS         extra ssh arguments
  JSH_REMOTE_ARTIFACT_CACHE   verified artifacts for other machines
  JSH_INSTALL_BASE_URL        release base URL, honoured by install-jsh.sh
USAGE
}

say() { printf '%s\n' "$*"; }
warn() { printf 'jsh-remote: %s\n' "$*" >&2; }
note() {
    if [ "${verbose}" -eq 1 ]; then
        printf 'jsh-remote: %s\n' "$*" >&2 || :
    fi
}
die() {
    printf 'jsh-remote: %s\n' "$*" >&2
    exit 1
}
have() { command -v "$1" > /dev/null 2>&1; }

need_arg() {
    [ "$2" -gt 1 ] || die "$1 requires an argument"
}

# Quote one string so a remote /bin/sh reparses it as exactly one argument.
# ssh has no argv: it joins what it is given and hands the result to the login
# shell, so every value that crosses that boundary has to be quoted here.
sq() {
    printf "'"
    printf '%s' "$1" | sed "s/'/'\\\\''/g"
    printf "'"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --docker)
            need_arg "$1" $#
            transport="docker"
            destination="$2"
            shift
            ;;
        --docker-user)
            need_arg "$1" $#
            docker_user="$2"
            shift
            ;;
        --persist) mode="persist" ;;
        --incognito) mode="incognito" ;;
        --version)
            need_arg "$1" $#
            want_version="${2#v}"
            shift
            ;;
        --artifact)
            need_arg "$1" $#
            artifact="$2"
            shift
            ;;
        --session)
            need_arg "$1" $#
            session_id="$2"
            shift
            ;;
        --rcfile)
            need_arg "$1" $#
            rcfile="$2"
            shift
            ;;
        --no-rc) no_rc=1 ;;
        --fallback)
            need_arg "$1" $#
            fallback="$2"
            shift
            ;;
        --no-deploy) deploy=0 ;;
        --keep-sandbox) keep_sandbox=1 ;;
        --local-jsh)
            need_arg "$1" $#
            local_jsh="$2"
            shift
            ;;
        --dry-run) dry_run=1 ;;
        -v | --verbose) verbose=1 ;;
        -h | --help)
            usage
            exit 0
            ;;
        --)
            shift
            while [ $# -gt 0 ]; do
                ssh_extra="${ssh_extra} $1"
                shift
            done
            break
            ;;
        -*) die "unknown option: $1 (try --help)" ;;
        *)
            [ "${transport}" = "ssh" ] || die "--docker and an ssh destination are mutually exclusive"
            [ -z "${destination}" ] || die "more than one destination: ${destination} and $1"
            destination="$1"
            ;;
    esac
    shift
done

[ -n "${destination}" ] || {
    usage >&2
    exit 1
}
case "${fallback}" in
    integration | bash | fail) ;;
    *) die "unknown --fallback: ${fallback} (expected integration, bash, or fail)" ;;
esac
[ "${no_rc}" -eq 0 ] || [ -z "${rcfile}" ] || die "--no-rc and --rcfile conflict"
if [ -n "${rcfile}" ]; then
    [ -f "${rcfile}" ] || die "no such rc file: ${rcfile}"
fi
if [ -n "${artifact}" ]; then
    [ -f "${artifact}" ] || die "no such artifact: ${artifact}"
    [ "${deploy}" -eq 1 ] || die "--artifact and --no-deploy conflict"
fi

# --- untrusted input grammar -------------------------------------------------
#
# Everything the destination reports comes back as text and is then either
# interpolated into a remote command or used to build a path. Validate the
# shapes once, here, instead of discovering downstream that a hostname
# contained a quote.

valid_hex() {
    case "$1" in
        '' | *[!0-9a-f]*) return 1 ;;
    esac
    [ "${#1}" -le 64 ]
}

valid_version() {
    case "$1" in
        '' | *[!0-9A-Za-z.-]* | *..* | .* | -*) return 1 ;;
        [0-9]*) ;;
        *) return 1 ;;
    esac
    [ "${#1}" -le 64 ]
}

# A path the destination told us about. Every one of these is quoted with sq()
# or handed to docker as argv before it reaches a shell, so a space is merely
# unusual and is allowed. A control character or a quote is not: neither belongs
# in a home directory, and refusing them here means a future call site that
# forgets to quote cannot turn one into a command.
valid_remote_path() {
    case "$1" in
        '' | *[!!-~\ ]* | *\'* | *\"*) return 1 ;;
        /*) ;;
        *) return 1 ;;
    esac
    [ "${#1}" -le 4096 ]
}

# --- transport ---------------------------------------------------------------

ssh_opts=""
if [ "${transport}" = "ssh" ]; then
    have ssh || die "ssh is not installed"
    ssh_opts="-o ConnectTimeout=${SSH_CONNECT_TIMEOUT} -o BatchMode=no"
    # Share one authenticated connection across the probe, the push, the
    # session, and the teardown. Without this, deploying costs four handshakes;
    # with it, three of them are a few milliseconds on an existing socket.
    # anvil builds the same ControlPath, so a tab opened there and a session
    # started here reuse each other's master.
    ctl_base="${XDG_RUNTIME_DIR:-${CACHE_HOME}/anvil}"
    case "${ctl_base}" in
        # The options are assembled into a whitespace-split string, so a control
        # path containing a space would silently become two arguments. Dropping
        # multiplexing is slower; getting it wrong is a confusing failure.
        *[!!-~]*) warn "control socket directory contains whitespace; multiplexing disabled" ;;
        *)
            if mkdir -p "${ctl_base}" 2> /dev/null; then
                ssh_opts="${ssh_opts} -o ControlMaster=auto -o ControlPersist=120"
                ssh_opts="${ssh_opts} -o ControlPath=${ctl_base}/cm-%C"
            fi
            ;;
    esac
else
    have docker || die "docker is not installed"
fi

docker_user_opt=""
[ -z "${docker_user}" ] || docker_user_opt="-u ${docker_user}"

# ssh sends TERM for us; docker exec sends nothing and the daemon substitutes
# its own default, so without this a container session runs at TERM=xterm with
# no COLORTERM and everything in it draws in 8 colours while every other tab in
# the same terminal draws in 16 million. Only a bare terminal name is
# forwarded: these become arguments, and a value carrying a space or a quote is
# not a terminal name. LANG is deliberately not forwarded — naming a locale the
# image never generated is worse than inheriting none.
docker_env_opt=""
forward_to_container() {
    case "$2" in
        '' | *[!A-Za-z0-9._-]*) return 0 ;;
    esac
    docker_env_opt="${docker_env_opt} -e $1=$2"
}
forward_to_container TERM "${TERM-}"
forward_to_container COLORTERM "${COLORTERM-}"

# remote_sh SCRIPT [ARG...]
#
# Runs SCRIPT on the destination with the arguments in $1.., reading the script
# itself from stdin. Only for steps that produce output; the interactive session
# needs stdin for the terminal and goes through remote_exec instead.
remote_sh() {
    rs_script="$1"
    shift
    if [ "${transport}" = "ssh" ]; then
        rs_cmd="/bin/sh -s"
        for rs_arg in "$@"; do
            rs_cmd="${rs_cmd} $(sq "${rs_arg}")"
        done
        # shellcheck disable=SC2086 # ssh_opts/ssh_extra are deliberately split
        printf '%s' "${rs_script}" | ssh ${ssh_opts} ${ssh_extra} -- "${destination}" "${rs_cmd}"
    else
        # shellcheck disable=SC2086 # docker_user_opt is deliberately split
        printf '%s' "${rs_script}" | docker exec -i ${docker_user_opt} "${destination}" /bin/sh -s "$@"
    fi
}

# remote_put LOCAL REMOTE_PATH — stream a local file into a remote path.
remote_put() {
    if [ "${transport}" = "ssh" ]; then
        # shellcheck disable=SC2086 # ssh_opts/ssh_extra are deliberately split
        ssh ${ssh_opts} ${ssh_extra} -- "${destination}" "cat > $(sq "$2")" < "$1"
    else
        # shellcheck disable=SC2086 # docker_user_opt is deliberately split
        docker exec -i ${docker_user_opt} "${destination}" \
            /bin/sh -c 'cat > "$1"' jsh-remote "$2" < "$1"
    fi
}

# remote_exec SCRIPT [ARG...] — the interactive session. stdin stays the
# terminal, so the script travels as an argument rather than on stdin.
remote_exec() {
    re_script="$1"
    shift
    if [ "${transport}" = "ssh" ]; then
        re_cmd="/bin/sh -c $(sq "${re_script}") jsh-remote"
        for re_arg in "$@"; do
            re_cmd="${re_cmd} $(sq "${re_arg}")"
        done
        # shellcheck disable=SC2086 # ssh_opts/ssh_extra are deliberately split
        ssh ${ssh_opts} ${ssh_extra} -t -- "${destination}" "${re_cmd}"
    else
        # shellcheck disable=SC2086 # docker_user_opt/docker_env_opt are deliberately split
        docker exec -it ${docker_user_opt} ${docker_env_opt} "${destination}" \
            /bin/sh -c "${re_script}" jsh-remote "$@"
    fi
}

# --- teardown ----------------------------------------------------------------

# shellcheck disable=SC2016 # Expanded by the destination shell.
CLEANUP_SCRIPT='
set -u
d="$1"
case "$d" in
    */jsh-remote.*) ;;
    *) exit 0 ;;
esac
[ -d "$d" ] || exit 0

# ssh takes the session down with it: closing the connection hangs up the
# remote pty and everything on it. `docker exec` has no equivalent — the
# process it started keeps running after its client is gone — so closing a
# container tab left a jsh running in the container with its sandbox unlinked
# underneath it, and every open and close added another one.
#
# The recorded pid alone is not something to kill on, because pids are reused.
# The environment is the proof: the sandbox name is unique to this session and
# the process carries it. A destination without /proc, or without tr or grep,
# simply skips this and behaves as it did before.
p=""
[ -f "$d/pid" ] && p=$(cat "$d/pid" 2>/dev/null || :)
case "$p" in
    "" | *[!0-9]*) p="" ;;
esac
if [ -n "$p" ] && [ -r "/proc/$p/environ" ] &&
    tr "\0" "\n" < "/proc/$p/environ" 2>/dev/null |
    grep -qxF "JSH_REMOTE_SANDBOX=$d" 2>/dev/null; then
    # HUP is what a terminal going away means, and jsh answers it by saving its
    # session and leaving. KILL is the backstop for a shell that will not, not
    # the opening move.
    kill -HUP "$p" 2>/dev/null || :
    i=0
    while [ "$i" -lt 2 ] && kill -0 "$p" 2>/dev/null; do
        sleep 1
        i=$((i + 1))
    done
    kill -0 "$p" 2>/dev/null && kill -KILL "$p" 2>/dev/null || :
fi
rm -rf "$d" 2>/dev/null || :
'

cleanup() {
    [ "${cleanup_done}" -eq 0 ] || return 0
    cleanup_done=1
    [ -n "${tmp_rc}" ] && rm -f "${tmp_rc}" 2> /dev/null
    [ -n "${sandbox}" ] || return 0
    if [ "${keep_sandbox}" -eq 1 ]; then
        warn "sandbox kept at ${sandbox}"
        return 0
    fi
    # Best effort by design: the session is already over, and a destination that
    # has become unreachable must not keep a terminal tab open. Whatever is left
    # behind is swept by the next connection.
    note "removing ${sandbox}"
    remote_sh "${CLEANUP_SCRIPT}" "${sandbox}" > /dev/null 2>&1 || :
    return 0
}
trap 'cleanup' EXIT
trap 'cleanup; exit 130' HUP INT TERM

# --- step 1: probe -----------------------------------------------------------
#
# One round trip that answers everything the local side needs to decide: which
# artifact, where it may be executed from, and whether a usable jsh is already
# there. Pure POSIX sh with no external tools beyond uname/id, so a busybox-only
# image answers it too.

# shellcheck disable=SC2016 # Expanded by the destination shell.
PROBE_SCRIPT='
set -u
want_mode="$1"
may_create="$2"
transport_kind="${3:-ssh}"

printf "os=%s\n" "$(uname -s 2>/dev/null || echo unknown)"
printf "arch=%s\n" "$(uname -m 2>/dev/null || echo unknown)"
printf "uid=%s\n" "$(id -u 2>/dev/null || echo unknown)"
printf "home=%s\n" "${HOME:-}"

# An existing jsh is worth finding: deploying over one that already matches is
# pure waste. Identified by its banner, never by its name.
p=$(command -v jsh 2>/dev/null || :)
case "$p" in
    /*)
        if [ -x "$p" ]; then
            b=$("$p" --version 2>/dev/null </dev/null | head -1 | cut -c1-64 || :)
            case "$b" in
                "jsh "*)
                    rest=${b#jsh }
                    printf "jsh_path=%s\n" "$p"
                    printf "jsh_version=%s\n" "${rest%% *}"
                    ;;
            esac
        fi
        ;;
esac

# A directory is only useful if a file written there can actually be executed.
# noexec on /tmp is common enough that assuming otherwise strands the whole
# deployment, and only an execve tells the truth: a shebang script fails on a
# noexec mount exactly as an ELF binary does.
exec_ok() {
    d="$1"
    [ -n "$d" ] && [ -d "$d" ] && [ -w "$d" ] || return 1
    t=$(mktemp -d "$d/.jshx.XXXXXX" 2>/dev/null) || return 1
    printf "#!/bin/sh\nexit 7\n" > "$t/p" 2>/dev/null || { rm -rf "$t"; return 1; }
    chmod 700 "$t/p" 2>/dev/null || { rm -rf "$t"; return 1; }
    "$t/p" >/dev/null 2>&1
    rc=$?
    rm -rf "$t" 2>/dev/null || :
    [ "$rc" -eq 7 ]
}

# Order matters. Persist mode prefers $HOME because a cache is only safe to keep
# at a fixed path in a directory nobody else can write. Incognito prefers the
# scratch filesystems precisely because it must not touch $HOME at all, and
# falls back to $HOME only when nothing else can execute.
# Built with set -- rather than a space-separated string: a home directory
# containing a space would otherwise split into two candidates, and the real one
# would never be tested.
if [ "$want_mode" = incognito ]; then
    # In a container /dev is a tmpfs the runtime made, and it is the only
    # writable place there that both executes files and stays out of the
    # image writable layer: a sandbox in it never appears in docker diff and
    # is gone when the container stops, which is what incognito promises.
    # Containers only: on a real machine /dev is devtmpfs, and putting a 5 MiB
    # binary in the RAM-backed /dev of somebody else machine is not a thing to
    # do quietly. /dev/shm stays below for the reason it always failed there:
    # it is mounted noexec.
    #
    # (No apostrophes in this comment on purpose: the whole probe travels as a
    # single-quoted string, and one would end it.)
    if [ "$transport_kind" = docker ]; then
        set -- /dev /var/tmp /tmp /dev/shm "${HOME:-}/.cache" "${HOME:-}"
    else
        set -- /var/tmp /tmp /dev/shm "${HOME:-}/.cache" "${HOME:-}"
    fi
else
    # Persist mode may create the XDG cache directory if it is missing, which
    # keeps sandboxes and the binary cache out of the top of $HOME. Incognito
    # never does: creating a directory is already more than it promises, and
    # neither does a dry run, which must leave the destination exactly as it
    # found it.
    if [ "$may_create" = 1 ] && [ -n "${HOME:-}" ]; then
        mkdir -p "$HOME/.cache" 2>/dev/null || :
    fi
    set -- "${HOME:-}/.cache" "${HOME:-}" /var/tmp /tmp /dev/shm
fi
for d do
    case "$d" in ""|"/.cache") continue ;; esac
    # Writable and executable are different questions, and the difference is a
    # whole tier of degradation. A noexec mount refuses execve; it does not
    # refuse write(2), and `bash --rcfile FILE` only ever *reads* FILE. So a
    # destination where nothing can be executed can still be given shell
    # integration, which is most of what the terminal wants.
    if [ -d "$d" ] && [ -w "$d" ]; then printf "writedir=%s\n" "$d"; fi
    if exec_ok "$d"; then printf "execdir=%s\n" "$d"; fi
done

# Integration needs a bash on the far side; ash has neither PROMPT_COMMAND nor
# a DEBUG trap, so there is nothing to hook there.
for b in "${BASH:-}" /bin/bash /usr/bin/bash /usr/local/bin/bash; do
    [ -n "$b" ] && [ -x "$b" ] || continue
    printf "bash=%s\n" "$b"
    break
done
'

note "probing ${destination}"
may_create=1
[ "${dry_run}" -eq 0 ] || may_create=0
probe_out="$(remote_sh "${PROBE_SCRIPT}" "${mode}" "${may_create}" "${transport}" 2> /dev/null)" || {
    if [ "${transport}" = "ssh" ]; then
        die "cannot reach ${destination} over ssh"
    fi
    die "cannot exec into container ${destination}"
}

probe_field() {
    printf '%s\n' "${probe_out}" | sed -n "s/^$1=//p" | head -1
}

remote_os="$(probe_field os)"
remote_arch="$(probe_field arch)"
remote_home="$(probe_field home)"
remote_jsh="$(probe_field jsh_path)"
remote_jsh_version="$(probe_field jsh_version)"
exec_dirs="$(printf '%s\n' "${probe_out}" | sed -n 's/^execdir=//p')"
exec_dir="$(printf '%s\n' "${exec_dirs}" | head -1)"
write_dir="$(printf '%s\n' "${probe_out}" | sed -n 's/^writedir=//p' | head -1)"
remote_bash="$(probe_field bash)"

note "os=${remote_os} arch=${remote_arch} home=${remote_home}" \
    "execdir=${exec_dir:-none} writedir=${write_dir:-none} bash=${remote_bash:-none}"

# --- step 2: decide what to deploy -------------------------------------------

target=""
if [ "${remote_os}" = "Linux" ]; then
    case "${remote_arch}" in
        # musl unconditionally: static, no glibc floor, one artifact per
        # architecture covers every distribution.
        x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
        aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
    esac
fi

local_version=""
if [ -z "${want_version}" ] && [ -z "${artifact}" ]; then
    # Default to the shell running this script, so both ends of the connection
    # are the same jsh. Falls through to "latest" when there is no local jsh.
    probe_bin="${local_jsh}"
    [ -n "${probe_bin}" ] || probe_bin="$(command -v jsh 2> /dev/null || :)"
    if [ -n "${probe_bin}" ] && [ -x "${probe_bin}" ]; then
        banner="$("${probe_bin}" --version 2> /dev/null < /dev/null | head -1 || :)"
        case "${banner}" in
            "jsh "*)
                rest="${banner#jsh }"
                local_version="${rest%% *}"
                valid_version "${local_version}" || local_version=""
                ;;
        esac
    fi
fi

# Reasons we may end up not deploying at all, in the order they are discovered.
skip_reason=""
if [ "${deploy}" -eq 0 ]; then
    skip_reason="--no-deploy"
elif [ -z "${exec_dir}" ]; then
    skip_reason="no directory on ${destination} allows execution (all candidates are noexec or unwritable)"
elif [ -z "${target}" ] && [ -z "${artifact}" ]; then
    skip_reason="no musl artifact for ${remote_os}/${remote_arch}"
fi

# An already-installed jsh of the version we would have deployed is the best
# outcome available: nothing to transfer, nothing to clean up.
reuse_existing=""
if [ -n "${remote_jsh}" ] && valid_remote_path "${remote_jsh}"; then
    if [ -z "${want_version}" ] || [ "${remote_jsh_version}" = "${want_version}" ]; then
        reuse_existing="${remote_jsh}"
    fi
fi

# The local jsh can be its own artifact. A static binary survives arriving in
# any libc's userland, and the far side then runs exactly the version that
# sent it — no release lookup, no network, and it works before the first
# release exists. Only for this machine's architecture, because the local
# binary is by definition built for it; every other target keeps going
# through published artifacts. ldd is the judge of static: a dynamically
# linked jsh would land, fail to find its interpreter, and report a missing
# file that is plainly there.
artifact_version=""
if [ -z "${artifact}" ] && [ -z "${want_version}" ] && [ -z "${skip_reason}" ] \
    && [ -n "${local_version}" ] && [ -x "${probe_bin:-}" ]; then
    case "$(uname -m)" in
        x86_64 | amd64) local_arch="x86_64" ;;
        aarch64 | arm64) local_arch="aarch64" ;;
        *) local_arch="" ;;
    esac
    if [ -n "${local_arch}" ] && [ "${target}" = "${local_arch}-unknown-linux-musl" ] \
        && have ldd \
        && ldd "${probe_bin}" 2>&1 | grep -qiE "statically linked|not a dynamic"; then
        note "the local jsh is static; lending it instead of fetching a release"
        artifact="${probe_bin}"
        artifact_version="${local_version}"
    fi
fi

# --- step 3: stage the artifact locally --------------------------------------

staged_bin=""
staged_sha=""
staged_version=""

stage_release() {
    # stage_release VERSION_OR_EMPTY -> populates staged_* or returns 1
    sr_version="$1"
    [ -f "${INSTALLER}" ] || {
        warn "missing ${INSTALLER}"
        return 1
    }
    if [ -n "${sr_version}" ]; then
        sr_dir="${ARTIFACT_CACHE}/v${sr_version}/${target}"
    else
        sr_dir="${ARTIFACT_CACHE}/latest/${target}"
    fi
    if [ -f "${sr_dir}/jsh" ] && [ -f "${sr_dir}/jsh.meta" ]; then
        note "reusing staged artifact in ${sr_dir}"
    else
        note "staging ${target} ${sr_version:-latest} via install-jsh.sh"
        if [ -n "${sr_version}" ]; then
            sh "${INSTALLER}" --stage-dir "${sr_dir}" --target "${target}" \
                --version "${sr_version}" > /dev/null 2>&1 || return 1
        else
            sh "${INSTALLER}" --stage-dir "${sr_dir}" --target "${target}" \
                > /dev/null 2>&1 || return 1
        fi
    fi
    [ -f "${sr_dir}/jsh" ] && [ -f "${sr_dir}/jsh.meta" ] || return 1
    staged_bin="${sr_dir}/jsh"
    staged_sha="$(sed -n 's/^sha256=//p' "${sr_dir}/jsh.meta" | head -1)"
    staged_version="$(sed -n 's/^version=//p' "${sr_dir}/jsh.meta" | head -1)"
    valid_hex "${staged_sha}" || return 1
    valid_version "${staged_version}" || return 1
    return 0
}

if [ -z "${skip_reason}" ] && [ -z "${reuse_existing}" ]; then
    if [ -n "${artifact}" ]; then
        staged_bin="${artifact}"
        # An explicit --version alongside --artifact is an assertion about the
        # file, and the post-landing check is where it is enforced. Without this
        # the two options would silently ignore each other. A lent local jsh
        # knows its version from its own banner, which also lets the remote
        # cache recognise it on the next connection.
        staged_version="${want_version:-${artifact_version}}"
        if have sha256sum; then
            staged_sha="$(sha256sum "${artifact}" | cut -d' ' -f1)"
        elif have shasum; then
            staged_sha="$(shasum -a 256 "${artifact}" | cut -d' ' -f1)"
        else
            die "need sha256sum or shasum to identify ${artifact}"
        fi
        valid_hex "${staged_sha}" || die "cannot digest ${artifact}"
    elif [ -n "${want_version}" ]; then
        stage_release "${want_version}" \
            || die "cannot obtain jsh ${want_version} for ${target}"
    elif [ -n "${local_version}" ]; then
        if ! stage_release "${local_version}"; then
            warn "no published ${target} artifact for the local jsh ${local_version}; using the latest release"
            stage_release "" || skip_reason="cannot obtain a jsh release for ${target}"
        fi
    else
        stage_release "" || skip_reason="cannot obtain a jsh release for ${target}"
    fi
fi

# --- step 4: dry run ---------------------------------------------------------

if [ "${dry_run}" -eq 1 ]; then
    say "destination: ${destination} (${transport})"
    say "platform:    ${remote_os}/${remote_arch} -> ${target:-unsupported}"
    say "mode:        ${mode}"
    say "exec dir:    ${exec_dir:-none}"
    if [ -n "${reuse_existing}" ]; then
        say "plan:        use the existing jsh ${remote_jsh_version} at ${reuse_existing}"
    elif [ -n "${skip_reason}" ]; then
        say "plan:        no deployment (${skip_reason}); fallback is ${fallback}"
    else
        say "plan:        push jsh ${staged_version:-$(basename "${staged_bin}")} from ${staged_bin}"
        say "             sha256 ${staged_sha}"
        if [ "${mode}" = "persist" ]; then
            say "             cache at ${remote_home}/.cache/jsh-remote/bin/$(printf '%s' "${staged_sha}" | cut -c1-16)"
        else
            say "             sandbox only, no cache, wiped on exit"
        fi
    fi
    exit 0
fi

# shellcheck disable=SC2016 # Expanded by the destination shell.
PREPARE_SCRIPT='
set -u
execdir="$1"
sha12="$2"
version="$3"
want_cache="$4"
home="$5"

sandbox=$(mktemp -d "$execdir/jsh-remote.XXXXXX") || exit 1
chmod 700 "$sandbox" 2>/dev/null || :
printf "sandbox=%s\n" "$sandbox"

# Sweep first. A session that ends any survivable way removes its own sandbox;
# what lands here is what SIGKILL left behind, and for an incognito session that
# is a directory the mode promised would not outlive it.
#
# The pid file is what makes this precise. A sandbox that recorded a pid was
# owned by a real session, so once that pid is gone the directory is garbage and
# is removed immediately — waiting out a timer would leave an incognito sandbox
# on disk for no reason. Only a sandbox with no usable pid needs the age guard,
# and only because it might be seconds old and still on its way to exec.
for d in "$execdir"/jsh-remote.*; do
    [ -d "$d" ] || continue
    [ "$d" = "$sandbox" ] && continue
    p=""
    [ -f "$d/pid" ] && p=$(cat "$d/pid" 2>/dev/null || :)
    case "$p" in
        ""|*[!0-9]*)
            # No owner ever recorded: fall back to age.
            [ -n "$(find "$d" -maxdepth 0 -mmin +'"${SWEEP_MIN_AGE_MINUTES}"' 2>/dev/null || :)" ] \
                || continue
            ;;
        *)
            kill -0 "$p" 2>/dev/null && continue
            ;;
    esac
    rm -rf "$d" 2>/dev/null || :
done

if [ "$want_cache" = 1 ] && [ -n "$home" ]; then
    # Fixed path, so repeat connections skip the transfer. Safe only because it
    # is under $HOME, where nobody else can pre-create what we are about to
    # write through. The directory is named by the digest, so a different build
    # is a different directory and never overwrites this one.
    cdir="$home/.cache/jsh-remote/bin/$sha12"
    if mkdir -p "$cdir" 2>/dev/null; then
        chmod 700 "$home/.cache/jsh-remote" 2>/dev/null || :
        chmod 700 "$cdir" 2>/dev/null || :
        printf "binpath=%s\n" "$cdir/jsh"
        # Cached only when it is really there and really is this jsh; a
        # truncated push from a dropped connection fails the banner and is
        # replaced rather than executed.
        if [ -x "$cdir/jsh" ]; then
            b=$("$cdir/jsh" --version 2>/dev/null </dev/null | head -1 | cut -c1-64 || :)
            if [ "$b" = "jsh $version" ] || { [ -z "$version" ] && [ -n "${b#jsh }" ] && [ "${b#jsh }" != "$b" ]; }; then
                printf "cached=1\n"
                exit 0
            fi
        fi
        printf "cached=0\n"
        exit 0
    fi
fi
printf "binpath=%s\n" "$sandbox/jsh"
printf "cached=0\n"
'

# --- step 5: fall back when there is nothing to deploy -----------------------
#
# Three tiers, not two. Deployment needs a directory that can *execute* a file;
# integration needs one that can merely be *written* to, because `bash --rcfile`
# reads its argument and never runs it. A noexec mount refuses execve and
# permits write(2), so the middle tier survives exactly the case that strands
# the first one.
#
# What integration buys back is the terminal half: OSC 133 blocks, cwd tracking
# and exit codes, which come from marks a shell emits rather than from jsh
# itself. It buys back none of jsh — no completion, no structured pipelines —
# so it is a fallback and not an alternative.

# Shell integration for a bash that has never heard of jsh. Read once by
# --rcfile and deleted with the sandbox; nothing is installed, and the
# destination's own ~/.bashrc still runs first so aliases and prompt survive.
# shellcheck disable=SC2016 # Expanded by the destination shell.
INTEGRATION_RC='# jsh remote shell integration (temporary; removed on exit)
case $- in *i*) ;; *) return 0 ;; esac
if [ -r "$HOME/.bashrc" ]; then . "$HOME/.bashrc"; fi

# OSC 7 carries a file:// URI, so the path is percent-encoded exactly as
# jsh does it. LC_ALL=C makes ${s:i:1} step over bytes rather than characters,
# which is what percent-encoding is defined in terms of.
__jsh_url_pwd() {
    local LC_ALL=C s=$PWD out= i c
    for ((i = 0; i < ${#s}; i++)); do
        c=${s:i:1}
        case $c in
            [-A-Za-z0-9/._~]) out+=$c ;;
            *) printf -v c "%%%02X" "$(printf "%d" "'"'"'$c")"; out+=$c ;;
        esac
    done
    printf "%s" "$out"
}

__jsh_precmd() {
    local status=$?
    # No D before the first command: the mark closes a block, and at the first
    # prompt there is nothing open to close.
    if [ -n "${__jsh_seen-}" ]; then
        printf "\033]133;D;%s\007" "$status"
    fi
    __jsh_seen=1
    printf "\033]7;file://%s%s\007" "${HOSTNAME-}" "$(__jsh_url_pwd)"
}

PROMPT_COMMAND="__jsh_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
PS1="\[\033]133;A\007\]${PS1-}\[\033]133;B\007\]"

# C marks where output begins, and the block state machine needs it to leave
# AwaitingCommand. PS0 is printed after a command is read and before it runs,
# which is that point exactly — and unlike a DEBUG trap it fires once per
# command line with no guards against firing for PROMPT_COMMAND itself.
if [ "${BASH_VERSINFO[0]-0}" -gt 4 ] ||
    { [ "${BASH_VERSINFO[0]-0}" -eq 4 ] && [ "${BASH_VERSINFO[1]-0}" -ge 4 ]; }; then
    PS0=$'"'"'\033]133;C\007'"'"'
fi
'

start_integration_session() {
    # start_integration_session -> execs, or returns 1 when this tier cannot run
    [ "${remote_bash}" != "" ] || return 1
    [ "${write_dir}" != "" ] || return 1
    valid_remote_path "${remote_bash}" || return 1
    valid_remote_path "${write_dir}" || return 1

    prep_out="$(remote_sh "${PREPARE_SCRIPT}" "${write_dir}" "0" "" "0" "${remote_home}")" \
        || return 1
    sandbox="$(printf '%s\n' "${prep_out}" | sed -n 's/^sandbox=//p' | head -1)"
    valid_remote_path "${sandbox}" || return 1
    case "${sandbox}" in
        */jsh-remote.*) ;;
        *) return 1 ;;
    esac

    rc_path="${sandbox}/integration.bash"
    printf '%s' "${INTEGRATION_RC}" > "${tmp_rc}" || return 1
    remote_put "${tmp_rc}" "${rc_path}" || return 1

    note "starting ${remote_bash} with shell integration on ${destination}"
    set +e
    # shellcheck disable=SC2016 # Expanded by the destination shell.
    remote_exec 'printf "%s\n" "$$" > "$1/pid" 2>/dev/null || :
JSH_REMOTE_SANDBOX="$1"
export JSH_REMOTE_SANDBOX
exec "$2" --rcfile "$3" -i' "${sandbox}" "${remote_bash}" "${rc_path}"
    rc=$?
    set -e
    cleanup
    exit "${rc}"
}

if [ -z "${reuse_existing}" ] && [ -n "${skip_reason}" ]; then
    [ "${fallback}" != "fail" ] || die "${skip_reason}"
    if [ "${fallback}" = "integration" ]; then
        warn "${skip_reason}; falling back to shell integration without deploying"
        tmp_rc="$(mktemp "${TMPDIR:-/tmp}/jsh-remote-rc.XXXXXX")" \
            || die "cannot stage the integration rc"
        start_integration_session \
            || warn "shell integration is unavailable on ${destination} (needs bash and a writable directory)"
    else
        warn "${skip_reason}; falling back to the destination's own shell"
    fi
    if [ "${transport}" = "ssh" ]; then
        # shellcheck disable=SC2086 # ssh_opts/ssh_extra are deliberately split
        exec ssh ${ssh_opts} ${ssh_extra} -t -- "${destination}"
    fi
    # shellcheck disable=SC2086 # docker_user_opt/docker_env_opt are deliberately split
    exec docker exec -it ${docker_user_opt} ${docker_env_opt} "${destination}" \
        /bin/sh -lc 'exec ${SHELL:-/bin/sh}'
fi

# --- step 6: prepare the destination -----------------------------------------
#
# Creates the sandbox, reports whether the cache already holds this exact
# binary, and sweeps sandboxes whose owning process is gone.


want_cache=0
[ "${mode}" = "persist" ] && want_cache=1
# The cache only exists where the fixed path is safe, which is under $HOME. When
# $HOME cannot execute, persist mode transfers like incognito does.
case "${exec_dir}" in
    "${remote_home}" | "${remote_home}/"*) ;;
    *) want_cache=0 ;;
esac
[ -n "${remote_home}" ] || want_cache=0

if [ -n "${reuse_existing}" ]; then
    # Still needs a sandbox: it carries the pid file and any pushed rc.
    want_cache=0
    sha12=""
    staged_version="${remote_jsh_version}"
else
    sha12="$(printf '%s' "${staged_sha}" | cut -c1-16)"
    valid_hex "${sha12}" || die "internal error: bad digest ${staged_sha}"
fi

valid_remote_path "${exec_dir}" || die "unusable directory reported by ${destination}: ${exec_dir}"
if [ -n "${remote_home}" ]; then
    valid_remote_path "${remote_home}" \
        || die "unusable home directory reported by ${destination}: ${remote_home}"
fi

note "preparing sandbox in ${exec_dir}"
prep_out="$(remote_sh "${PREPARE_SCRIPT}" \
    "${exec_dir}" "${sha12:-0}" "${staged_version}" "${want_cache}" "${remote_home}")" \
    || die "cannot prepare a working directory in ${exec_dir} on ${destination}"

prep_field() {
    printf '%s\n' "${prep_out}" | sed -n "s/^$1=//p" | head -1
}

sandbox="$(prep_field sandbox)"
bin_path="$(prep_field binpath)"
cached="$(prep_field cached)"
valid_remote_path "${sandbox}" || die "no sandbox was created on ${destination}"
case "${sandbox}" in
    */jsh-remote.*) ;;
    *) die "unexpected sandbox path from ${destination}: ${sandbox}" ;;
esac

# --- step 7: push ------------------------------------------------------------

# shellcheck disable=SC2016 # Expanded by the destination shell.
VERIFY_SCRIPT='
set -u
incoming="$1"
final="$2"
version="$3"
chmod 700 "$incoming" 2>/dev/null || { rm -f "$incoming"; exit 1; }
b=$("$incoming" --version 2>/dev/null </dev/null | head -1 | cut -c1-64 || :)
case "$b" in
    "jsh "*) ;;
    *) rm -f "$incoming"; printf "error=not-jsh\n"; exit 1 ;;
esac
rest=${b#jsh }
got=${rest%% *}
if [ -n "$version" ] && [ "$got" != "$version" ]; then
    rm -f "$incoming"
    printf "error=version-mismatch:%s\n" "$got"
    exit 1
fi
mv -f "$incoming" "$final" || { rm -f "$incoming"; exit 1; }
printf "version=%s\n" "$got"
'

if [ -z "${reuse_existing}" ] && [ "${cached}" != "1" ]; then
    valid_remote_path "${bin_path}" || die "no destination path for the binary on ${destination}"
    # Land under a name nothing can be running, prove it is jsh, and only then
    # rename it into place: an interrupted transfer never leaves something
    # executable at the path the next connection trusts.
    incoming="${bin_path}.incoming"
    size="$(wc -c < "${staged_bin}" | tr -d ' ')"
    note "pushing ${staged_bin} (${size} bytes) to ${bin_path}"
    remote_put "${staged_bin}" "${incoming}" || die "cannot write ${incoming} on ${destination}"
    verify_out="$(remote_sh "${VERIFY_SCRIPT}" "${incoming}" "${bin_path}" "${staged_version}")" || {
        case "${verify_out}" in
            *not-jsh*) die "the binary that landed on ${destination} does not identify itself as jsh (wrong architecture?)" ;;
            *version-mismatch*) die "the binary that landed on ${destination} reports ${verify_out#*:}" ;;
        esac
        die "the binary that landed on ${destination} failed its self-check"
    }
    landed="$(printf '%s\n' "${verify_out}" | sed -n 's/^version=//p' | head -1)"
    [ -z "${landed}" ] || staged_version="${landed}"
elif [ -n "${reuse_existing}" ]; then
    bin_path="${reuse_existing}"
    note "using the existing ${bin_path} (jsh ${remote_jsh_version})"
else
    note "cache hit: ${bin_path}"
fi

# --- step 8: rc file ---------------------------------------------------------
#
# Persist mode leaves this alone: $HOME is real, and jsh imports the
# destination's own ~/.bashrc exactly as it would locally. Incognito has an
# empty HOME, so without help it would start with no aliases at all — a local rc
# is pushed when there is one, and otherwise the destination's real ~/.bashrc is
# named explicitly. Reading it changes nothing; only writing would.

remote_rc=""
if [ "${no_rc}" -eq 0 ]; then
    if [ -n "${rcfile}" ]; then
        remote_rc="${sandbox}/rc"
        note "pushing ${rcfile}"
        remote_put "${rcfile}" "${remote_rc}" || die "cannot push ${rcfile}"
    elif [ "${mode}" = "incognito" ]; then
        if [ -f "${HOME:-}/.jshrc" ]; then
            remote_rc="${sandbox}/rc"
            note "pushing ${HOME}/.jshrc"
            remote_put "${HOME}/.jshrc" "${remote_rc}" || die "cannot push ${HOME}/.jshrc"
        elif [ -n "${remote_home}" ]; then
            remote_rc="${remote_home}/.bashrc"
        fi
    fi
fi

# --- step 9: the session -----------------------------------------------------

# shellcheck disable=SC2016 # Expanded by the destination shell.
SESSION_SCRIPT='
set -u
sandbox="$1"
bin="$2"
want_mode="$3"
realhome="$4"
rc="$5"
session="$6"
norc="$7"

# Written before exec so the next connection can tell a live sandbox from an
# abandoned one. exec() keeps the pid, so this stays true for the whole session.
printf "%s\n" "$$" > "$sandbox/pid" 2>/dev/null || :
# Carried in the environment so the teardown can prove a still-running process
# is this session before it signals it; pids alone are reused. See
# CLEANUP_SCRIPT.
JSH_REMOTE_SANDBOX="$sandbox"
export JSH_REMOTE_SANDBOX

if [ "$want_mode" = incognito ]; then
    mkdir -p "$sandbox/home" "$sandbox/cache" "$sandbox/state" \
             "$sandbox/data" "$sandbox/config" 2>/dev/null || :
    chmod 700 "$sandbox/home" 2>/dev/null || :
    # Every path jsh derives goes through home_dir(), which reads $HOME first,
    # so redirecting these five variables moves history, sessions, bookmarks,
    # the frecency database and the execution journal into the sandbox.
    JSH_REAL_HOME="$realhome"; export JSH_REAL_HOME
    HOME="$sandbox/home"; export HOME
    XDG_CACHE_HOME="$sandbox/cache"; export XDG_CACHE_HOME
    XDG_STATE_HOME="$sandbox/state"; export XDG_STATE_HOME
    XDG_DATA_HOME="$sandbox/data"; export XDG_DATA_HOME
    XDG_CONFIG_HOME="$sandbox/config"; export XDG_CONFIG_HOME
fi

# Start where the operator expects to be, not in the sandbox.
if [ -n "$realhome" ] && [ -d "$realhome" ]; then
    cd "$realhome" 2>/dev/null || cd / 2>/dev/null || :
fi

set --
if [ "$norc" = 1 ]; then
    set -- --norc
elif [ -n "$rc" ] && [ -f "$rc" ]; then
    set -- --rcfile "$rc"
fi
[ -z "$session" ] || set -- "$@" --session "$session"

exec "$bin" "$@"
'

note "starting jsh ${staged_version:-} on ${destination}"
set +e
remote_exec "${SESSION_SCRIPT}" \
    "${sandbox}" "${bin_path}" "${mode}" "${remote_home}" \
    "${remote_rc}" "${session_id}" "${no_rc}"
rc=$?
set -e

cleanup
exit "${rc}"
