#!/bin/sh
# Install or update jsh for the current user.
#
#   curl -fsSL https://github.com/beamiter/jsh/releases/latest/download/install-jsh.sh | sh
#
# The script is the single source of truth for "how jsh gets onto a machine":
# jterm1..4 shell out to it instead of each carrying their own installer.
#
# Design notes that are easy to get wrong and expensive to rediscover:
#   * Every binary this script touches is identified by its `--version` banner,
#     never by its name alone: a `jsh` on PATH need not be this shell.
#   * The binary is replaced by rename(2), so shells that are already running
#     keep the inode they started with and are never disturbed.
#   * Nothing here edits shell startup files. If PATH resolves to the wrong jsh
#     we say so and print the fix; we do not silently rewrite the user's config.

set -eu

REPO="beamiter/jsh"
BASE_URL="${JSH_INSTALL_BASE_URL:-https://github.com/${REPO}/releases}"
CACHE_HOME="${XDG_CACHE_HOME:-${HOME}/.cache}"
STATE_HOME="${XDG_STATE_HOME:-${HOME}/.local/state}"
# Shared by all four terminals on purpose: one update check per machine per
# interval instead of one per terminal.
CACHE_FILE="${CACHE_HOME}/jsh/update-check.json"
ROLLBACK_DIR="${STATE_HOME}/jsh/rollback"
# glibc floor of the prebuilt gnu artifacts (built on Ubuntu 22.04).
GNU_GLIBC_MIN="2.35"

channel="release"
bin_dir=""
prefix=""
want_version=""
mode="install"
json=0
force=0
dry_run=0
max_age=0
tmp_dir=""

usage() {
    cat <<'USAGE'
Usage: install-jsh.sh [options]

Options:
  --check              Report installed and latest versions, then exit
  --json               Machine-readable output (human text goes to stderr)
  --max-age SECONDS    With --check, reuse a cached result younger than this
  --version VERSION    Install an exact version instead of the latest
  --tag TAG            Same, spelled as a tag (v0.2.0)
  --channel CHANNEL    release (prebuilt, default) or source (cargo build)
  --prefix PATH        Install root; binary lands in PATH/bin
  --bin-dir PATH       Install directory for the binary (overrides --prefix)
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
        --check) mode="check" ;;
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

case "${channel}" in
    release | source) ;;
    *) die "unknown channel: ${channel} (expected release or source)" ;;
esac
case "${max_age}" in
    '' | *[!0-9]*) die "--max-age expects whole seconds" ;;
esac
[ -n "${HOME:-}" ] || die "HOME is not set"

# --- platform detection ------------------------------------------------------

version_ge() {
    # version_ge A B -> true when A >= B, comparing dot-separated numbers.
    [ "$1" = "$2" ] && return 0
    lo="$(printf '%s\n%s\n' "$1" "$2" | sort -t. -k1,1n -k2,2n -k3,3n | head -1)"
    [ "${lo}" = "$2" ]
}

detect_libc() {
    # musl is the portability fallback: static, no glibc floor, slower malloc.
    if have getconf && getconf GNU_LIBC_VERSION > /dev/null 2>&1; then
        glibc="$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')"
    elif have ldd; then
        first="$(ldd --version 2>&1 | head -1)"
        case "${first}" in
            *musl*)
                printf 'musl\n'
                return 0
                ;;
        esac
        glibc="$(printf '%s\n' "${first}" | awk '{print $NF}')"
    else
        printf 'musl\n'
        return 0
    fi
    case "${glibc}" in
        '' | *[!0-9.]*)
            printf 'musl\n'
            return 0
            ;;
    esac
    if version_ge "${glibc}" "${GNU_GLIBC_MIN}"; then
        printf 'gnu\n'
    else
        printf 'musl\n'
    fi
}

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
        printf '%s-unknown-linux-%s\n' "${arch}" "$(detect_libc)"
    fi
    return 0
}

# --- identifying a jsh binary -----------------------------------------------

# Prints the version of a jsh binary, or nothing when the file is not jsh.
# This is the same identity check jterm3 performs before adopting a shell.
jsh_version_of() {
    [ -n "$1" ] && [ -x "$1" ] || return 0
    banner="$("$1" --version 2>/dev/null | head -1)" || return 0
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

# Absolute path of the jsh that PATH resolves to, whatever it turns out to be.
path_jsh() {
    resolved="$(command -v jsh 2>/dev/null)" || return 0
    case "${resolved}" in
        /*) printf '%s\n' "${resolved}" ;;
        *) ;;
    esac
    return 0
}

# --- download helpers --------------------------------------------------------

fetch() {
    # fetch URL DEST
    #
    # Bounded on purpose: a terminal runs `--check` on a background thread, and
    # an unreachable host must fail rather than hang there forever.
    if have curl; then
        curl -fsSL --retry 3 --retry-delay 1 \
            --connect-timeout 10 --max-time 300 -o "$2" "$1"
    elif have wget; then
        wget -q --tries=3 --timeout=30 -O "$2" "$1"
    else
        die "need curl or wget to download ${1}"
    fi
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
    [ -f "${CACHE_FILE}" ] || return 1
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
    dir="$(dirname "${CACHE_FILE}")"
    mkdir -p "${dir}" 2> /dev/null || return 0
    tmp="${CACHE_FILE}.$$"
    printf '{"schema":1,"checked_at":%s,"latest":"%s","target":"%s"}\n' \
        "$(now_epoch)" "$(json_str "$1")" "$(json_str "$2")" > "${tmp}" 2> /dev/null || return 0
    mv -f "${tmp}" "${CACHE_FILE}" 2> /dev/null || rm -f "${tmp}"
    return 0
}

latest_version() {
    # Reads the manifest published at a stable "latest" URL, so no API token
    # and no rate limit are involved.
    make_tmp
    manifest="${tmp_dir}/manifest.json"
    fetch "${BASE_URL}/latest/download/manifest.json" "${manifest}" 2> /dev/null || return 1
    version="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "${manifest}" | head -1)"
    [ -n "${version}" ] || return 1
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
dest_dir="$(resolve_bin_dir)"
dest="${dest_dir}/jsh"
installed_version="$(jsh_version_of "${dest}")"

on_path="$(path_jsh)"
shadowed_by=""
if [ -n "${on_path}" ] && [ "${on_path}" != "${dest}" ]; then
    shadowed_by="${on_path}"
fi

# --- check mode --------------------------------------------------------------

emit_check_json() {
    # $1 latest (may be empty), $2 error (may be empty)
    printf '{"schema":1,"installed":%s,"installed_path":%s,"latest":%s,"target":%s,"update_available":%s,"shadowed_by":%s,"error":%s}\n' \
        "$([ -n "${installed_version}" ] && printf '"%s"' "$(json_str "${installed_version}")" || printf 'null')" \
        "$([ -n "${installed_version}" ] && printf '"%s"' "$(json_str "${dest}")" || printf 'null')" \
        "$([ -n "$1" ] && printf '"%s"' "$(json_str "$1")" || printf 'null')" \
        "$([ -n "${target}" ] && printf '"%s"' "$(json_str "${target}")" || printf 'null')" \
        "$([ -n "$1" ] && [ "$1" != "${installed_version}" ] && printf 'true' || printf 'false')" \
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

if [ "${channel}" = "release" ] && [ -z "${target}" ]; then
    warn "no prebuilt binaries for $(uname -s)/$(uname -m); falling back to --channel source"
    channel="source"
fi

version="${want_version}"
if [ "${channel}" = "release" ] && [ -z "${version}" ]; then
    version="$(latest_version)" || die "cannot read the release manifest from ${BASE_URL}"
fi

if [ -n "${installed_version}" ] && [ "${installed_version}" = "${version}" ] && [ "${force}" -eq 0 ]; then
    say "jsh ${installed_version} is already installed at ${dest}"
    say "use --force to reinstall"
    exit 0
fi

if [ "${dry_run}" -eq 1 ]; then
    say "would install jsh ${version:-from source} to ${dest} (channel: ${channel}, target: ${target:-n/a})"
    exit 0
fi

mkdir -p "${dest_dir}" || die "cannot create ${dest_dir}"
writable_dir "${dest_dir}" || die "${dest_dir} is not writable"
make_tmp
staged="${tmp_dir}/jsh"

if [ "${channel}" = "release" ]; then
    archive="jsh-${version}-${target}.tar.gz"
    url="${BASE_URL}/download/v${version}/${archive}"
    say "downloading ${archive}"
    fetch "${url}" "${tmp_dir}/${archive}" || die "download failed: ${url}"
    # The checksum sits next to the archive, so it guards against a truncated
    # or corrupted transfer; HTTPS is what makes the source trustworthy.
    if fetch "${url}.sha256" "${tmp_dir}/${archive}.sha256" 2> /dev/null; then
        expected="$(cut -d' ' -f1 < "${tmp_dir}/${archive}.sha256")"
        actual="$(sha256_of "${tmp_dir}/${archive}")"
        [ "${expected}" = "${actual}" ] \
            || die "checksum mismatch for ${archive} (expected ${expected}, got ${actual})"
    else
        warn "no published checksum for ${archive}; skipping verification"
    fi
    tar -C "${tmp_dir}" -xzf "${tmp_dir}/${archive}" || die "cannot unpack ${archive}"
    unpacked="${tmp_dir}/jsh-${version}-${target}/jsh"
    [ -f "${unpacked}" ] || die "${archive} does not contain jsh-${version}-${target}/jsh"
    mv "${unpacked}" "${staged}"
else
    have cargo || die "channel 'source' needs cargo (https://rustup.rs)"
    say "building jsh from source; this takes a few minutes"
    cargo_root="${tmp_dir}/cargo-root"
    if [ -n "${version}" ]; then
        cargo install --git "https://github.com/${REPO}" --tag "v${version}" \
            --locked --root "${cargo_root}" jsh || die "cargo install failed"
    else
        cargo install --git "https://github.com/${REPO}" \
            --locked --root "${cargo_root}" jsh || die "cargo install failed"
    fi
    [ -f "${cargo_root}/bin/jsh" ] || die "cargo did not produce a binary"
    mv "${cargo_root}/bin/jsh" "${staged}"
fi

chmod 0755 "${staged}"
staged_version="$(jsh_version_of "${staged}")"
[ -n "${staged_version}" ] || die "the downloaded binary does not identify itself as jsh"
if [ -n "${version}" ] && [ "${staged_version}" != "${version}" ]; then
    die "expected jsh ${version} but the binary reports ${staged_version}"
fi
version="${staged_version}"

# Keep the outgoing binary so a bad release can be undone without a network.
backup=""
if [ -n "${installed_version}" ]; then
    if mkdir -p "${ROLLBACK_DIR}" 2> /dev/null; then
        backup="${ROLLBACK_DIR}/jsh-${installed_version}"
        rm -f "${ROLLBACK_DIR}"/jsh-* 2> /dev/null || :
        cp "${dest}" "${backup}" 2> /dev/null || backup=""
    fi
fi

# Land the new binary with rename(2) inside the destination directory: the swap
# is atomic and running shells keep the inode they were started from.
incoming="${dest_dir}/.jsh.incoming.$$"
cp "${staged}" "${incoming}" || die "cannot write to ${dest_dir}"
chmod 0755 "${incoming}"
mv -f "${incoming}" "${dest}" || {
    rm -f "${incoming}"
    die "cannot replace ${dest}"
}

if [ "$(jsh_version_of "${dest}")" != "${version}" ]; then
    if [ -n "${backup}" ] && [ -f "${backup}" ]; then
        cp "${backup}" "${incoming}" && mv -f "${incoming}" "${dest}"
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
