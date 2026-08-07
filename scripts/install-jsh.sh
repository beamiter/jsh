#!/bin/sh
# Install or update jsh for the current user.
#
#   curl -fsSL https://github.com/beamiter/jsh/releases/latest/download/install-jsh.sh | sh
#
# The script is the single source of truth for "how jsh gets onto a machine":
# All four terminals shell out to it instead of each carrying their own installer.
#
# Design notes that are easy to get wrong and expensive to rediscover:
#   * Every binary this script touches is identified by its `--version` banner,
#     never by its name alone: a `jsh` on PATH need not be this shell.
#   * The binary is replaced by rename(2), so shells that are already running
#     keep the inode they started with and are never disturbed.
#   * Nothing here edits shell startup files. If PATH resolves to the wrong jsh
#     we say so and print the fix; we do not silently rewrite the user's config.
#   * `--stage-dir` stops after "downloaded and verified", so jsh-remote.sh can
#     ship a binary to a machine that has no network without growing a second
#     copy of the download-and-verify logic.
#   * A source build prefers the checkout this script is run from, uncommitted
#     work included. Building the last pushed commit instead would quietly
#     install something other than the tree the user is standing in. Piped from
#     curl there is no checkout to find, and the repository build takes over.

set -eu

REPO="beamiter/jsh"
BASE_URL="${JSH_INSTALL_BASE_URL:-https://github.com/${REPO}/releases}"
CACHE_HOME="${XDG_CACHE_HOME:-${HOME}/.cache}"
STATE_HOME="${XDG_STATE_HOME:-${HOME}/.local/state}"
# Shared by all four terminals on purpose: one update check per machine per
# interval instead of one per terminal.
CACHE_FILE="${CACHE_HOME}/jsh/update-check.json"
ROLLBACK_DIR="${STATE_HOME}/jsh/rollback"
# Ceilings for anything downloaded. A release archive is a few tens of MiB; a
# manifest or checksum is a few hundred bytes. Without a ceiling a hostile or
# broken mirror can fill the user's disk while the script waits patiently.
MAX_ARCHIVE_BYTES=104857600
MAX_METADATA_BYTES=65536
# Seconds allowed for one `--version` probe of an untrusted binary.
PROBE_TIMEOUT=5

# Defaulted after the arguments are parsed: source for an install, release for
# --stage-dir. Empty means "nothing explicit was asked for".
channel=""
bin_dir=""
prefix=""
want_version=""
mode="install"
json=0
force=0
dry_run=0
max_age=0
tmp_dir=""
stage_dir=""
check_requested=0
# Empty means "not resolved yet"; a source build fills it in with the checkout
# to build, and leaves it empty when the repository is the right source.
source_dir=""
from_git=0

usage() {
    cat <<'USAGE'
Usage: install-jsh.sh [options]

Options:
  --check              Report installed and latest versions, then exit
  --json               Machine-readable output (human text goes to stderr)
  --max-age SECONDS    With --check, reuse a cached result younger than this
  --version VERSION    Install an exact version instead of the latest
  --tag TAG            Same, spelled as a tag (v0.2.0)
  --channel CHANNEL    source (cargo build, default) or release (prebuilt).
                       Staging always uses release. An explicit release that
                       finds no published release falls back to source and
                       says so
  --source-dir PATH    Build this checkout, uncommitted work included. Defaults
                       to the checkout this script lives in, when there is one
  --git                Build the published repository even when the script is
                       run from a checkout
  --prefix PATH        Install root; binary lands in PATH/bin
  --bin-dir PATH       Install directory for the binary (overrides --prefix)
  --target TRIPLE      Fetch for this target instead of the detected one
  --stage-dir PATH     Download and verify only; leave the binary in PATH and
                       exit without installing anything
  --force              Reinstall even when the wanted version is present
  --dry-run            Print what would happen, change nothing
  -h, --help           Show this help

Default install directory: the directory of the jsh already on PATH when it is
writable, otherwise ~/.local/bin.

Environment:
  JSH_INSTALL_BASE_URL  Release base URL (mirrors, local testing)
  JSH_INSTALL_TARGET    Force a target triple instead of detecting one
  XDG_CACHE_HOME        Update-check cache base (default ~/.cache)
  XDG_STATE_HOME        Rollback copy base (default ~/.local/state)
USAGE
}

# In --json mode stdout carries exactly one JSON object, so every human-facing
# line goes to stderr instead.
say() {
    if [ "${json}" -eq 1 ]; then
        printf '%s\n' "$*" >&2
    else
        printf '%s\n' "$*"
    fi
}
warn() { printf 'install-jsh: %s\n' "$*" >&2; }
die() {
    printf 'install-jsh: %s\n' "$*" >&2
    exit 1
}
have() { command -v "$1" > /dev/null 2>&1; }

# A directory is a jsh checkout when cargo would build *this* package from it.
# The package name is the test that matters: a directory can carry a Cargo.toml
# and still be some other crate that happens to sit next to a scripts/ dir.
is_jsh_checkout() {
    [ -n "${1:-}" ] || return 1
    [ -f "$1/Cargo.toml" ] || return 1
    [ -f "$1/src/main.rs" ] || return 1
    grep -q '^[[:space:]]*name[[:space:]]*=[[:space:]]*"jsh"[[:space:]]*$' "$1/Cargo.toml" 2> /dev/null
}

abs_dir() {
    (CDPATH='' cd -- "$1" 2> /dev/null && pwd) || return 1
}

# The checkout this script was started from, when it was started from one.
# `curl … | sh` leaves $0 as "sh" with no path to resolve, so this prints
# nothing and the caller builds the repository instead.
script_checkout() {
    _dir=""
    case "${0}" in
        */*) _dir="$(abs_dir "$(dirname -- "${0}")/..")" || return 0 ;;
        *) return 0 ;;
    esac
    is_jsh_checkout "${_dir}" || return 0
    printf '%s\n' "${_dir}"
}

# The version in a checkout's Cargo.toml: the first `version = "…"` after the
# [package] header, so a dependency's pin cannot be read as the package's own.
checkout_version() {
    awk '
        /^[[:space:]]*\[/ { in_pkg = ($0 ~ /^[[:space:]]*\[package\]/) }
        in_pkg && /^[[:space:]]*version[[:space:]]*=/ {
            if (match($0, /"[^"]*"/)) {
                print substr($0, RSTART + 1, RLENGTH - 2)
                exit
            }
        }
    ' "$1/Cargo.toml" 2> /dev/null
}

cleanup() {
    [ -n "${tmp_dir}" ] && [ -d "${tmp_dir}" ] && rm -rf "${tmp_dir}"
    return 0
}
trap cleanup EXIT HUP INT TERM

need_arg() {
    [ "$2" -gt 1 ] || die "$1 requires an argument"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --check)
            mode="check"
            check_requested=1
            ;;
        --json) json=1 ;;
        --max-age)
            need_arg "$1" $#
            max_age="$2"
            shift
            ;;
        --version | --tag)
            need_arg "$1" $#
            want_version="${2#v}"
            shift
            ;;
        --channel)
            need_arg "$1" $#
            channel="$2"
            shift
            ;;
        --prefix)
            need_arg "$1" $#
            prefix="$2"
            shift
            ;;
        --bin-dir)
            need_arg "$1" $#
            bin_dir="$2"
            shift
            ;;
        --target)
            need_arg "$1" $#
            JSH_INSTALL_TARGET="$2"
            export JSH_INSTALL_TARGET
            shift
            ;;
        --source-dir)
            need_arg "$1" $#
            source_dir="$2"
            shift
            ;;
        --git) from_git=1 ;;
        --stage-dir)
            need_arg "$1" $#
            stage_dir="$2"
            mode="stage"
            shift
            ;;
        --force) force=1 ;;
        --dry-run) dry_run=1 ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
    shift
done

# Source is the default channel: it produces the same static musl binary a
# release would ship and works before the first release exists. Staging keeps
# the release channel — the staged artifact is for another machine, and a
# build here would be for this host.
if [ -z "${channel}" ]; then
    if [ "${mode}" = "stage" ]; then
        channel="release"
    else
        channel="source"
    fi
fi
case "${channel}" in
    release | source) ;;
    *) die "unknown channel: ${channel} (expected release or source)" ;;
esac
if [ -n "${source_dir}" ]; then
    # Order-independent, like every other refusal here: the two flags name
    # opposite sources, and resolving them by write order would install
    # whichever one the user did not mean.
    [ "${from_git}" -eq 0 ] || die "--source-dir and --git are mutually exclusive"
    [ "${channel}" = "source" ] || die "--source-dir needs --channel source"
    resolved_source="$(abs_dir "${source_dir}")" || die "no such directory: ${source_dir}"
    is_jsh_checkout "${resolved_source}" \
        || die "${resolved_source} is not a jsh checkout (no Cargo.toml for package jsh)"
    source_dir="${resolved_source}"
    if [ -n "${want_version}" ]; then
        # The tree is the thing being installed, so it has to actually be the
        # version that was asked for. Building it anyway would install
        # something whose banner contradicts the request.
        tree_version="$(checkout_version "${source_dir}")"
        [ "${tree_version}" = "${want_version}" ] \
            || die "--version ${want_version} but ${source_dir} is jsh ${tree_version:-unknown}"
    fi
fi
if [ "${from_git}" -eq 1 ] && [ "${channel}" = "release" ]; then
    die "--git needs --channel source"
fi
case "${max_age}" in
    '' | *[!0-9]*) die "--max-age expects whole seconds" ;;
esac
if [ -n "${stage_dir}" ]; then
    # Staging exists to hand a *verified release artifact* to another machine.
    # Building from source would produce a binary for this host instead, which
    # is exactly the wrong answer, so the combination is refused rather than
    # silently reinterpreted.
    [ "${channel}" = "release" ] || die "--stage-dir needs --channel release"
    # Order-independent: `--check --stage-dir` and `--stage-dir --check` must
    # both be refused, not resolved by whichever flag was written last.
    [ "${check_requested}" -eq 0 ] || die "--stage-dir and --check are mutually exclusive"
    mode="stage"
    case "${stage_dir}" in
        /*) ;;
        *) die "--stage-dir must be an absolute path: ${stage_dir}" ;;
    esac
fi
[ -n "${HOME:-}" ] || die "HOME is not set"

# --- untrusted input grammar -------------------------------------------------
#
# A version, a target triple, and a base URL each arrive from a command line, an
# environment variable, or a downloaded manifest, and each is then interpolated
# into both a URL and a filesystem path. Validate the grammar once, up front,
# instead of hoping that `../`, a newline, or a shell metacharacter does
# something harmless further down.

valid_version() {
    case "$1" in
        '' | *[!0-9A-Za-z.-]* | *..* | .* | -*) return 1 ;;
        [0-9]*) ;;
        *) return 1 ;;
    esac
    [ "${#1}" -le 64 ]
}

valid_target() {
    case "$1" in
        '' | *[!0-9A-Za-z._-]* | *..* | .* | -*) return 1 ;;
    esac
    [ "${#1}" -le 64 ]
}

# HTTPS is the only remote scheme. `file://` and loopback HTTP stay available
# because the acceptance tests and local mirrors need them, and neither can be
# reached by an attacker who does not already control the environment.
valid_base_url() {
    case "$1" in
        *[!!-~]* | *..* | *\\* | *\?* | *\#* | *@*) return 1 ;;
    esac
    case "$1" in
        https://?*) ;;
        file:///*) ;;
        http://localhost | http://localhost[:/]*) ;;
        http://127.0.0.1 | http://127.0.0.1[:/]*) ;;
        *) return 1 ;;
    esac
    [ "${#1}" -le 512 ]
}

valid_sha256() {
    [ "${#1}" -eq 64 ] || return 1
    case "$1" in
        *[!0-9a-fA-F]*) return 1 ;;
    esac
    return 0
}

valid_base_url "${BASE_URL}" \
    || die "JSH_INSTALL_BASE_URL must be a plain HTTPS (or file/loopback) URL: ${BASE_URL}"
if [ -n "${want_version}" ]; then
    valid_version "${want_version}" || die "not a valid version: ${want_version}"
fi
if [ -n "${JSH_INSTALL_TARGET:-}" ]; then
    valid_target "${JSH_INSTALL_TARGET}" \
        || die "not a valid target triple: ${JSH_INSTALL_TARGET}"
fi

# --- platform detection ------------------------------------------------------

detect_target() {
    if [ -n "${JSH_INSTALL_TARGET:-}" ]; then
        printf '%s\n' "${JSH_INSTALL_TARGET}"
        return 0
    fi
    os="$(uname -s)"
    machine="$(uname -m)"
    case "${machine}" in
        x86_64 | amd64) arch="x86_64" ;;
        aarch64 | arm64) arch="aarch64" ;;
        *) arch="" ;;
    esac
    if [ "${os}" = "Linux" ] && [ -n "${arch}" ]; then
        # Static musl, always. It runs on every distribution and libc, and a
        # static jsh can lend itself out: entering a container or an ssh host
        # bind-mounts or pushes the very binary that is running, which a
        # dynamically linked one cannot do. The gnu artifacts are still
        # published for anyone who wants glibc's allocator back —
        # JSH_INSTALL_TARGET=<arch>-unknown-linux-gnu selects one explicitly.
        printf '%s-unknown-linux-musl\n' "${arch}"
    fi
    return 0
}

# --- identifying a jsh binary -----------------------------------------------

# Prints the version of a jsh binary, or nothing when the file is not jsh.
# This is the same identity check frost performs before adopting a shell.
#
# The probe writes to a file rather than a pipe, and runs under a deadline: a
# binary we have not identified yet may fork a descendant that inherits the
# probe's stdout, and a command substitution reading a pipe waits for *every*
# holder to close it, not just the child we started.
jsh_version_of() {
    [ -n "$1" ] && [ -f "$1" ] && [ -x "$1" ] || return 0
    make_tmp
    probe="${tmp_dir}/probe.banner"
    : > "${probe}" 2> /dev/null || return 0
    if have timeout; then
        timeout "${PROBE_TIMEOUT}" "$1" --version > "${probe}" 2> /dev/null < /dev/null || :
    else
        "$1" --version > "${probe}" 2> /dev/null < /dev/null || :
    fi
    banner="$(head -1 "${probe}" 2> /dev/null | cut -c1-256)"
    case "${banner}" in
        # Only the first field is the version, so a banner that grows a build
        # suffix later still parses.
        "jsh "*)
            rest="${banner#jsh }"
            printf '%s\n' "${rest%% *}"
            ;;
        *) ;;
    esac
    return 0
}

# One dotted field of a version, as a number. A missing or non-numeric field
# reads as 0, so a version this script cannot parse sorts as older rather than
# tricking the caller into installing it.
version_field() {
    field="$(printf '%s' "$1" | cut -d. -f"$2")"
    field="${field%%[!0-9]*}"
    [ -n "${field}" ] || field=0
    printf '%s' "${field}"
}

# Is $1 a strictly newer release than $2? Compares MAJOR.MINOR.PATCH
# numerically, because comparing the strings only tells you they differ:
# "0.10.0" is not newer than "0.9.0" as text, and a published release that is
# *older* than the installed one (a yanked tag, or a build from source that ran
# ahead of the last tag) would otherwise be offered as an update.
#
# A pre-release suffix is dropped rather than ordered, so 0.3.0-rc1 and 0.3.0
# compare equal and neither is offered over the other. Ordering them properly
# needs the full semver rule, and offering nothing is the safe answer.
version_gt() {
    left="${1#v}"
    right="${2#v}"
    left="${left%%-*}"
    right="${right%%-*}"
    i=1
    while [ "${i}" -le 3 ]; do
        l="$(version_field "${left}" "${i}")"
        r="$(version_field "${right}" "${i}")"
        [ "${l}" -gt "${r}" ] && return 0
        [ "${l}" -lt "${r}" ] && return 1
        i=$((i + 1))
    done
    return 1
}

# Absolute path of the jsh that PATH resolves to, whatever it turns out to be.
#
# A terminal that runs this script clamps PATH to system directories so the
# tools it executes (curl, mktemp, ...) cannot be hijacked, but the jsh its
# user would run still lives on the user's own PATH — usually ~/.cargo/bin or
# ~/.local/bin. JSH_LOOKUP_PATH carries that PATH for *lookup only*: nothing
# resolved through it is executed except jsh itself, which the terminal
# already executes as the session shell.
path_jsh() {
    resolved="$(
        PATH="${JSH_LOOKUP_PATH:-${PATH}}"
        export PATH
        command -v jsh 2>/dev/null
    )" || return 0
    case "${resolved}" in
        /*) printf '%s\n' "${resolved}" ;;
        *) ;;
    esac
    return 0
}

# --- download helpers --------------------------------------------------------

fetch() {
    # fetch URL DEST MAX_BYTES
    #
    # Bounded on purpose: a terminal runs `--check` on a background thread, and
    # an unreachable host must fail rather than hang there forever. The byte
    # ceiling and the HTTPS-only redirect policy bound what a mirror can do
    # with a connection we opened: a release download follows a redirect to a
    # CDN, so redirects cannot simply be disabled, but they must not be allowed
    # to leave HTTPS.
    if have curl; then
        case "$1" in
            https://*)
                curl -fsSL --proto '=https' --proto-redir '=https' \
                    --retry 3 --retry-delay 1 --connect-timeout 10 --max-time 300 \
                    --max-filesize "$3" -o "$2" "$1"
                ;;
            *)
                curl -fsSL --retry 3 --retry-delay 1 \
                    --connect-timeout 10 --max-time 300 --max-filesize "$3" -o "$2" "$1"
                ;;
        esac
    elif have wget; then
        wget -q --tries=3 --timeout=30 -O "$2" "$1" || return 1
        # wget has no size ceiling of its own, so enforce it after the fact
        # rather than leaving this path unbounded.
        if [ "$(file_size "$2")" -gt "$3" ]; then
            rm -f "$2"
            warn "$1 exceeds its ${3}-byte limit"
            return 1
        fi
    else
        die "need curl or wget to download ${1}"
    fi
}

file_size() {
    wc -c < "$1" 2> /dev/null | tr -d ' ' || printf '0'
}

sha256_of() {
    if have sha256sum; then
        sha256sum "$1" | cut -d' ' -f1
    elif have shasum; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        die "need sha256sum or shasum to verify downloads"
    fi
}

make_tmp() {
    [ -n "${tmp_dir}" ] && return 0
    tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/install-jsh.XXXXXX")" || die "cannot create a temporary directory"
    return 0
}

# --- release metadata --------------------------------------------------------

now_epoch() { date +%s; }

json_str() {
    # Minimal JSON string escaping for the values we emit (paths, versions).
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g' | tr -d '\n'
}

cache_get() {
    # cache_get FIELD -> value from the shared update-check cache, if fresh.
    [ "${max_age}" -gt 0 ] || return 1
    # A symlink at the cache name would make this read (and the write below)
    # act on a file the user never agreed to share with the installer.
    [ -f "${CACHE_FILE}" ] && [ ! -L "${CACHE_FILE}" ] || return 1
    [ "$(file_size "${CACHE_FILE}")" -le "${MAX_METADATA_BYTES}" ] || return 1
    checked="$(sed -n 's/.*"checked_at"[[:space:]]*:[[:space:]]*\([0-9]*\).*/\1/p' "${CACHE_FILE}" | head -1)"
    [ -n "${checked}" ] || return 1
    age=$(( $(now_epoch) - checked ))
    [ "${age}" -ge 0 ] && [ "${age}" -lt "${max_age}" ] || return 1
    value="$(sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" "${CACHE_FILE}" | head -1)"
    [ -n "${value}" ] || return 1
    printf '%s\n' "${value}"
}

cache_put() {
    # cache_put LATEST TARGET
    #
    # Best effort: an unwritable cache never fails an install. The temporary
    # name is unpredictable and private, and it is replaced by rename(2), so a
    # concurrent reader sees either the old file or the new one and never a
    # half-written one — and no other user can pre-create the name we write to.
    dir="$(dirname "${CACHE_FILE}")"
    mkdir -p "${dir}" 2> /dev/null || return 0
    chmod 0700 "${dir}" 2> /dev/null || :
    if [ -L "${CACHE_FILE}" ]; then
        rm -f "${CACHE_FILE}" 2> /dev/null || return 0
    fi
    tmp="$(mktemp "${dir}/update-check.XXXXXX" 2> /dev/null)" || return 0
    chmod 0600 "${tmp}" 2> /dev/null || :
    if printf '{"schema":1,"checked_at":%s,"latest":"%s","target":"%s"}\n' \
        "$(now_epoch)" "$(json_str "$1")" "$(json_str "$2")" > "${tmp}" 2> /dev/null; then
        mv -f "${tmp}" "${CACHE_FILE}" 2> /dev/null || rm -f "${tmp}"
    else
        rm -f "${tmp}"
    fi
    return 0
}

latest_version() {
    # Reads the manifest published at a stable "latest" URL, so no API token
    # and no rate limit are involved. The version it names is untrusted input:
    # it becomes part of a URL and a path, so it must satisfy the same grammar
    # as a version typed on the command line.
    make_tmp
    manifest="${tmp_dir}/manifest.json"
    fetch "${BASE_URL}/latest/download/manifest.json" "${manifest}" "${MAX_METADATA_BYTES}" \
        2> /dev/null || return 1
    version="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "${manifest}" | head -1)"
    valid_version "${version}" || return 1
    printf '%s\n' "${version}"
}

# --- install directory -------------------------------------------------------

writable_dir() { [ -d "$1" ] && [ -w "$1" ]; }

resolve_bin_dir() {
    if [ -n "${bin_dir}" ]; then
        printf '%s\n' "${bin_dir}"
        return 0
    fi
    if [ -n "${prefix}" ]; then
        printf '%s/bin\n' "${prefix}"
        return 0
    fi
    # Update in place when a genuine jsh is already on PATH, so we do not end up
    # with a second copy shadowing the first (the ~/.cargo/bin case).
    existing="$(path_jsh)"
    if [ -n "${existing}" ] && [ -n "$(jsh_version_of "${existing}")" ]; then
        existing_dir="$(dirname "${existing}")"
        if writable_dir "${existing_dir}"; then
            printf '%s\n' "${existing_dir}"
            return 0
        fi
    fi
    printf '%s/.local/bin\n' "${HOME}"
}

# --- state gathering ---------------------------------------------------------

# Created up front: helpers that download run inside command substitutions,
# where a temporary directory they created themselves would escape the trap.
make_tmp

target="$(detect_target)"

if [ "${mode}" = "stage" ]; then
    # Staging installs nothing, so there is no destination to resolve and no
    # local binary to identify. Skipping the probe also keeps `--stage-dir` from
    # executing an unrelated `jsh` on PATH just to answer a question it will
    # never use.
    dest_dir=""
    dest=""
    installed_version=""
    on_path=""
    shadowed_by=""
else
    dest_dir="$(resolve_bin_dir)"
    dest="${dest_dir}/jsh"
    installed_version="$(jsh_version_of "${dest}")"

    on_path="$(path_jsh)"
    shadowed_by=""
    if [ -n "${on_path}" ] && [ "${on_path}" != "${dest}" ]; then
        shadowed_by="${on_path}"
    fi
fi

# --- check mode --------------------------------------------------------------

emit_check_json() {
    # $1 latest (may be empty), $2 error (may be empty)
    printf '{"schema":1,"installed":%s,"installed_path":%s,"latest":%s,"target":%s,"update_available":%s,"shadowed_by":%s,"error":%s}\n' \
        "$([ -n "${installed_version}" ] && printf '"%s"' "$(json_str "${installed_version}")" || printf 'null')" \
        "$([ -n "${installed_version}" ] && printf '"%s"' "$(json_str "${dest}")" || printf 'null')" \
        "$([ -n "$1" ] && printf '"%s"' "$(json_str "$1")" || printf 'null')" \
        "$([ -n "${target}" ] && printf '"%s"' "$(json_str "${target}")" || printf 'null')" \
        "$([ -n "$1" ] && version_gt "$1" "${installed_version}" && printf 'true' || printf 'false')" \
        "$([ -n "${shadowed_by}" ] && printf '"%s"' "$(json_str "${shadowed_by}")" || printf 'null')" \
        "$([ -n "$2" ] && printf '"%s"' "$(json_str "$2")" || printf 'null')"
}

if [ "${mode}" = "check" ]; then
    latest=""
    check_error=""
    if [ -n "${want_version}" ]; then
        latest="${want_version}"
    elif latest="$(cache_get latest)"; then
        :
    elif latest="$(latest_version)"; then
        cache_put "${latest}" "${target}"
    else
        latest=""
        check_error="cannot reach ${BASE_URL}"
    fi

    if [ "${json}" -eq 1 ]; then
        emit_check_json "${latest}" "${check_error}"
    else
        if [ -n "${installed_version}" ]; then
            say "installed: ${installed_version} (${dest})"
        else
            say "installed: none in ${dest_dir}"
        fi
        say "latest:    ${latest:-unknown}"
        if [ -n "${shadowed_by}" ]; then
            say "on PATH:   ${shadowed_by}"
        fi
        if [ -n "${check_error}" ]; then
            warn "${check_error}"
        fi
    fi
    [ -z "${check_error}" ] || exit 1
    exit 0
fi

# --- install -----------------------------------------------------------------

if [ "${mode}" = "stage" ] && [ -z "${target}" ]; then
    # There is no source fallback for staging: the artifact is for another
    # machine, and building here would produce one for this host.
    die "no prebuilt target for $(uname -s)/$(uname -m); pass --target explicitly"
fi
if [ "${channel}" = "release" ] && [ -z "${target}" ]; then
    warn "no prebuilt binaries for $(uname -s)/$(uname -m); falling back to --channel source"
    channel="source"
fi

version="${want_version}"
if [ "${channel}" = "release" ] && [ -z "${version}" ]; then
    if ! version="$(latest_version)"; then
        # No manifest means no release to install — the state every repository
        # is in before its first tag. Staging has no source fallback (the
        # artifact is for another machine); an install has one, and taking it
        # automatically is what lets a bare `install-jsh.sh` work against a
        # repository that has never released. This is a not-found fallback
        # only: a manifest that resolves but names artifacts that fail their
        # checksum still dies, because "build something else instead" is not
        # an answer to failed verification.
        [ "${mode}" != "stage" ] || die "cannot read the release manifest from ${BASE_URL}"
        version=""
        warn "cannot read the release manifest from ${BASE_URL} (no release published yet?)"
        warn "falling back to --channel source: cargo builds from the repository, which takes a few minutes"
        channel="source"
    fi
fi

# Which tree a source build reads. Resolved before the dry-run report, so that
# report can name it. `./scripts/install-jsh.sh` is run from a working tree far
# more often than not, and `cargo install --git` would build the last *pushed*
# commit: the local fix being tested — the whole reason for running the script
# from a checkout — would be silently left out. An explicit --version is the
# one case that still means the repository: it names a published build, which a
# working tree is not, whatever its Cargo.toml says.
if [ "${channel}" = "source" ] && [ -z "${source_dir}" ] && [ "${from_git}" -eq 0 ] \
    && [ -z "${want_version}" ]; then
    source_dir="$(script_checkout)"
fi

if [ -n "${installed_version}" ] && [ "${installed_version}" = "${version}" ] && [ "${force}" -eq 0 ]; then
    say "jsh ${installed_version} is already installed at ${dest}"
    say "use --force to reinstall"
    exit 0
fi

# Refuse to walk backwards by accident. Reached when the newest published
# release is older than what is installed — a yanked tag, or a build from
# source that ran ahead of the last tag. An explicit --version or --force is
# still honoured, because asking for an older build by name is a real thing to
# want; what must not happen is a bare `install-jsh.sh` quietly replacing a
# working shell with an older one.
if [ -n "${installed_version}" ] && [ -n "${version}" ] && [ -z "${want_version}" ] \
    && [ "${force}" -eq 0 ] \
    && version_gt "${installed_version}" "${version}"; then
    say "jsh ${installed_version} at ${dest} is newer than the published ${version}"
    say "nothing to do; use --version ${version} to install that build anyway"
    exit 0
fi

if [ "${dry_run}" -eq 1 ]; then
    if [ "${mode}" = "stage" ]; then
        say "would stage jsh ${version} (${target}) in ${stage_dir}"
    else
        say "would install jsh ${version:-from source} to ${dest} (channel: ${channel}, target: ${target:-n/a})"
        [ -z "${source_dir}" ] || say "would build the checkout at ${source_dir}"
    fi
    exit 0
fi

if [ "${mode}" = "stage" ]; then
    mkdir -p "${stage_dir}" || die "cannot create ${stage_dir}"
    writable_dir "${stage_dir}" || die "${stage_dir} is not writable"
    chmod 0700 "${stage_dir}" 2> /dev/null || :
else
    mkdir -p "${dest_dir}" || die "cannot create ${dest_dir}"
    writable_dir "${dest_dir}" || die "${dest_dir} is not writable"
fi
make_tmp
staged="${tmp_dir}/jsh"

if [ "${channel}" = "release" ]; then
    # Both halves of every name below already satisfy the version/target
    # grammar, so the archive name and URL cannot contain a path segment,
    # traversal, or shell metacharacter.
    valid_version "${version}" || die "not a valid version: ${version}"
    valid_target "${target}" || die "not a valid target triple: ${target}"
    archive="jsh-${version}-${target}.tar.gz"
    url="${BASE_URL}/download/v${version}/${archive}"
    say "downloading ${archive}"
    fetch "${url}" "${tmp_dir}/${archive}" "${MAX_ARCHIVE_BYTES}" || die "download failed: ${url}"

    # The published checksum is mandatory. It is same-origin, so it proves only
    # that the bytes are the ones the release published — but without it a
    # mirror can serve anything at all, and "continue without verification" is
    # exactly the path an attacker would arrange to take.
    fetch "${url}.sha256" "${tmp_dir}/${archive}.sha256" "${MAX_METADATA_BYTES}" 2> /dev/null \
        || die "no published checksum at ${url}.sha256; refusing to install unverified bytes"
    expected="$(cut -d' ' -f1 < "${tmp_dir}/${archive}.sha256" | head -1 | tr -d '\r')"
    valid_sha256 "${expected}" || die "published checksum for ${archive} is not a SHA-256 digest"
    actual="$(sha256_of "${tmp_dir}/${archive}")"
    expected="$(printf '%s' "${expected}" | tr 'A-F' 'a-f')"
    actual="$(printf '%s' "${actual}" | tr 'A-F' 'a-f')"
    [ "${expected}" = "${actual}" ] \
        || die "checksum mismatch for ${archive} (expected ${expected}, got ${actual})"

    # Extract exactly one known member, and only after proving the archive
    # contains nothing else. `tar -x` on an unexamined archive will happily
    # follow an absolute path, a `..` traversal, or a symlink member out of the
    # temporary directory.
    member="jsh-${version}-${target}/jsh"
    names="${tmp_dir}/archive.names"
    types="${tmp_dir}/archive.types"
    tar -tzf "${tmp_dir}/${archive}" > "${names}" 2> /dev/null \
        || die "cannot read the contents of ${archive}"
    tar -tzvf "${tmp_dir}/${archive}" > "${types}" 2> /dev/null \
        || die "cannot read the contents of ${archive}"
    # Names: only the expected directory and the expected binary. This rejects
    # an absolute path, a `..` traversal, and any extra payload by construction.
    while IFS= read -r name; do
        [ -n "${name}" ] || continue
        case "${name}" in
            "${member}" | "jsh-${version}-${target}" | "jsh-${version}-${target}/") ;;
            *) die "${archive} contains an unexpected member: ${name}" ;;
        esac
    done < "${names}"
    # Types: the leading character of a verbose listing is the entry type, so a
    # symlink (l), hard link (h), device, or FIFO is refused before extraction.
    while IFS= read -r entry; do
        [ -n "${entry}" ] || continue
        case "${entry}" in
            -* | d*) ;;
            *) die "${archive} contains a link or special file: ${entry}" ;;
        esac
    done < "${types}"
    grep -qx "${member}" "${names}" || die "${archive} does not contain ${member}"
    tar -C "${tmp_dir}" -xzf "${tmp_dir}/${archive}" "${member}" \
        || die "cannot unpack ${member} from ${archive}"
    unpacked="${tmp_dir}/${member}"
    [ -f "${unpacked}" ] && [ ! -L "${unpacked}" ] \
        || die "${archive} does not contain a regular ${member}"
    [ "$(file_size "${unpacked}")" -le "${MAX_ARCHIVE_BYTES}" ] \
        || die "${member} exceeds its ${MAX_ARCHIVE_BYTES}-byte limit"
    mv "${unpacked}" "${staged}"
else
    have cargo || die "channel 'source' needs cargo (https://rustup.rs)"

    # A source build aims for the same thing the release channel ships: the
    # static musl binary. Static is not a packaging nicety here — an installed
    # jsh lends itself out, bind-mounted into containers and pushed onto ssh
    # hosts, and only a static binary survives arriving in another libc's
    # userland. The pieces that takes: the musl std (rustup adds it), and a
    # musl C compiler for the TLS dependency's C sources.
    #
    # A missing piece fails the install rather than degrading it. A
    # host-toolchain build would look installed but be dynamically linked, and
    # a dynamic jsh cannot lend itself into containers or onto ssh hosts — a
    # silent downgrade here only surfaces much later, as a remote tab with no
    # jsh in it. Naming a gnu triple (JSH_INSTALL_TARGET or --target) is how
    # glibc is asked for on purpose.
    source_target=""
    source_cc=""
    source_arch=""
    if [ "$(uname -s)" = "Linux" ]; then
        case "$(uname -m)" in
            x86_64 | amd64) source_arch="x86_64" ;;
            aarch64 | arm64) source_arch="aarch64" ;;
            *) source_arch="" ;;
        esac
        if [ -n "${JSH_INSTALL_TARGET:-}" ]; then
            # The explicit triple wins for source exactly as it does for
            # release: naming a gnu triple is how glibc is asked for.
            source_target="${JSH_INSTALL_TARGET}"
        elif [ -n "${source_arch}" ]; then
            source_target="${source_arch}-unknown-linux-musl"
        fi
    fi
    case "${source_target}" in
        *-musl)
            for candidate in "${source_arch}-linux-musl-gcc" musl-gcc; do
                if have "${candidate}"; then
                    source_cc="${candidate}"
                    break
                fi
            done
            if [ -z "${source_cc}" ]; then
                warn "a dynamically linked jsh cannot lend itself into containers or onto ssh hosts"
                warn "to build for the host glibc on purpose: JSH_INSTALL_TARGET=${source_arch}-unknown-linux-gnu"
                die "static build needs a musl C compiler (Debian/Ubuntu: sudo apt install musl-tools)"
            elif ! have rustup; then
                die "static build needs rustup to add the ${source_target} std (https://rustup.rs)"
            elif ! rustup target list --installed 2>/dev/null | grep -qx "${source_target}"; then
                say "adding the ${source_target} toolchain target"
                rustup target add "${source_target}" \
                    || die "cannot add the ${source_target} std with rustup"
            fi
            ;;
    esac

    # Name the tree, and say out loud when it carries work that is not
    # committed: "installed from source" is ambiguous about which source, and
    # this is the one line that removes the ambiguity.
    source_origin="https://github.com/${REPO}"
    if [ -n "${source_dir}" ]; then
        source_origin="${source_dir}"
        if have git && git -C "${source_dir}" rev-parse --git-dir > /dev/null 2>&1; then
            if [ -n "$(git -C "${source_dir}" status --porcelain 2> /dev/null)" ]; then
                source_origin="${source_dir} (uncommitted changes included)"
            fi
        fi
    fi
    if [ -n "${source_target}" ]; then
        say "building jsh from ${source_origin} for ${source_target}; this takes a few minutes"
    else
        say "building jsh from ${source_origin}; this takes a few minutes"
    fi
    cargo_root="${tmp_dir}/cargo-root"
    set -- --locked --root "${cargo_root}"
    if [ -n "${source_dir}" ]; then
        set -- "$@" --path "${source_dir}"
    else
        [ -z "${version}" ] || set -- "$@" --tag "v${version}"
    fi
    if [ -n "${source_target}" ]; then
        set -- "$@" --target "${source_target}"
        # The variable name cc-rs actually reads for a cross target; the same
        # one the release workflow sets.
        CC_x86_64_unknown_linux_musl="${source_cc}"
        CC_aarch64_unknown_linux_musl="${source_cc}"
        export CC_x86_64_unknown_linux_musl CC_aarch64_unknown_linux_musl
    fi
    if [ -n "${source_dir}" ]; then
        if ! cargo install "$@"; then
            # The failure this wording exists for: a Cargo.toml edited without
            # its lock file, which --locked refuses. Nothing here writes to the
            # user's tree to fix that.
            if [ -f "${source_dir}/Cargo.lock" ]; then
                warn "if --locked rejected the lock file, run 'cargo update --workspace' in ${source_dir} first"
            fi
            die "cargo install failed for ${source_dir}"
        fi
    else
        cargo install --git "https://github.com/${REPO}" "$@" jsh || die "cargo install failed"
    fi
    [ -f "${cargo_root}/bin/jsh" ] || die "cargo did not produce a binary"
    if [ -n "${source_target}" ] && have ldd; then
        # Confirmation, not enforcement: the triple already decides linkage,
        # and ldd's wording for a static PIE varies. Worth one line because
        # static is the entire point of preferring this target.
        if ldd "${cargo_root}/bin/jsh" 2>&1 | grep -qi "statically\|not a dynamic"; then
            say "built ${source_target}: statically linked"
        fi
    fi
    mv "${cargo_root}/bin/jsh" "${staged}"
fi

chmod 0755 "${staged}"

if [ "${mode}" = "stage" ]; then
    # Deliberately no `--version` probe here. A staged artifact is usually for a
    # different architecture and simply cannot run on this host, and running it
    # would prove nothing about the machine it is headed for anyway. The bytes
    # are already pinned by the published SHA-256 and the archive-member check;
    # the identity check happens on the destination, after the binary lands.
    stage_sha="$(sha256_of "${staged}")"
    valid_sha256 "${stage_sha}" || die "cannot digest the staged binary"
    # The binary is renamed into place and its digest is written afterwards, so
    # a reader that trusts the digest file never sees a half-written binary.
    incoming="$(mktemp "${stage_dir}/.jsh.staging.XXXXXX")" || die "cannot write to ${stage_dir}"
    cat < "${staged}" > "${incoming}" || {
        rm -f "${incoming}"
        die "cannot write to ${stage_dir}"
    }
    chmod 0755 "${incoming}"
    mv -f "${incoming}" "${stage_dir}/jsh" || {
        rm -f "${incoming}"
        die "cannot write ${stage_dir}/jsh"
    }
    printf 'version=%s\ntarget=%s\nsha256=%s\n' "${version}" "${target}" "${stage_sha}" \
        > "${stage_dir}/jsh.meta" || die "cannot write ${stage_dir}/jsh.meta"
    if [ "${json}" -eq 1 ]; then
        printf '{"schema":1,"version":"%s","target":"%s","path":"%s","sha256":"%s"}\n' \
            "$(json_str "${version}")" "$(json_str "${target}")" \
            "$(json_str "${stage_dir}/jsh")" "$(json_str "${stage_sha}")"
    else
        say "staged jsh ${version} (${target}) at ${stage_dir}/jsh"
    fi
    exit 0
fi

staged_version="$(jsh_version_of "${staged}")"
[ -n "${staged_version}" ] || die "the downloaded binary does not identify itself as jsh"
if [ -n "${version}" ] && [ "${staged_version}" != "${version}" ]; then
    die "expected jsh ${version} but the binary reports ${staged_version}"
fi
version="${staged_version}"

# Keep the outgoing binary so a bad release can be undone without a network.
backup=""
if [ -n "${installed_version}" ] && valid_version "${installed_version}"; then
    if mkdir -p "${ROLLBACK_DIR}" 2> /dev/null; then
        chmod 0700 "${ROLLBACK_DIR}" 2> /dev/null || :
        backup="${ROLLBACK_DIR}/jsh-${installed_version}"
        rm -f "${ROLLBACK_DIR}"/jsh-* 2> /dev/null || :
        cp "${dest}" "${backup}" 2> /dev/null || backup=""
    fi
fi

# Land the new binary with rename(2) inside the destination directory: the swap
# is atomic and running shells keep the inode they were started from. The
# staging name is unpredictable, so nothing can pre-create it and have the
# install write through a symlink or an existing file it does not own.
incoming="$(mktemp "${dest_dir}/.jsh.incoming.XXXXXX")" || die "cannot write to ${dest_dir}"
cat < "${staged}" > "${incoming}" || {
    rm -f "${incoming}"
    die "cannot write to ${dest_dir}"
}
chmod 0755 "${incoming}"
mv -f "${incoming}" "${dest}" || {
    rm -f "${incoming}"
    die "cannot replace ${dest}"
}

if [ "$(jsh_version_of "${dest}")" != "${version}" ]; then
    if [ -n "${backup}" ] && [ -f "${backup}" ]; then
        if restoring="$(mktemp "${dest_dir}/.jsh.rollback.XXXXXX" 2> /dev/null)"; then
            cat < "${backup}" > "${restoring}" \
                && chmod 0755 "${restoring}" \
                && mv -f "${restoring}" "${dest}" \
                || rm -f "${restoring}"
        fi
        die "the installed binary failed its self-check; restored ${installed_version}"
    fi
    die "the installed binary failed its self-check at ${dest}"
fi

if [ -z "${installed_version}" ]; then
    say "installed jsh ${version} at ${dest}"
elif [ "${installed_version}" = "${version}" ]; then
    say "reinstalled jsh ${version} at ${dest}"
else
    say "updated jsh ${installed_version} -> ${version} at ${dest}"
fi
if [ -n "${backup}" ]; then
    say "previous binary kept at ${backup}"
fi
cache_put "${version}" "${target}"

# --- post-install PATH report ------------------------------------------------

resolved="$(path_jsh)"
if [ -z "${resolved}" ]; then
    say ""
    warn "${dest_dir} is not on PATH; add it, for example:"
    say "    export PATH=\"${dest_dir}:\$PATH\""
elif [ "${resolved}" != "${dest}" ]; then
    other_version="$(jsh_version_of "${resolved}")"
    say ""
    if [ -z "${other_version}" ]; then
        # Some unrelated binary that happens to be named jsh.
        warn "PATH resolves jsh to ${resolved}, which is not this shell"
    else
        warn "PATH resolves jsh to ${resolved} (jsh ${other_version}), not the copy just installed"
    fi
    say "    put ${dest_dir} earlier on PATH, or rerun with --bin-dir $(dirname "${resolved}")"
fi

say ""
say "running shells keep the version they started with; open a new tab to use jsh ${version}"
