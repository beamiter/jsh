#!/bin/bash
# Exercises scripts/install-jsh.sh against a fake release tree served over
# file://, so the whole install/update path runs without network access.
#
#   ./scripts/test-install-jsh.sh
set -uo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
INSTALLER="${SCRIPT_DIR}/install-jsh.sh"
[ -f "${INSTALLER}" ] || {
    echo "missing ${INSTALLER}" >&2
    exit 1
}

have() { command -v "$1" > /dev/null 2>&1; }
have minisign || {
    echo "need minisign on PATH to run installer acceptance tests" >&2
    echo "(Debian/Ubuntu: sudo apt install minisign)" >&2
    exit 1
}
have python3 || { echo "need python3 to validate installer JSON" >&2; exit 1; }
MINISIGN_DIR="$(cd -- "$(dirname -- "$(command -v minisign)")" && pwd)"

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/test-install-jsh.XXXXXX")"
trap 'rm -rf "${ROOT}"' EXIT

REL="${ROOT}/release"
FAKE_HOME="${ROOT}/home"
BIN="${FAKE_HOME}/.local/bin"
TARGET="x86_64-unknown-linux-gnu"
mkdir -p "${FAKE_HOME}"

# Ephemeral signing key for fixtures. The installer honours JSH_MINISIGN_PUBKEY
# only because every harness run also sets JSH_INSTALL_BASE_URL.
MINI_DIR="${ROOT}/minisign"
mkdir -p "${MINI_DIR}"
printf '\n\n' | minisign -G -p "${MINI_DIR}/test.pub" -s "${MINI_DIR}/test.secret" > /dev/null 2>&1
TEST_MINISIGN_PUBKEY="$(awk '/^R/{print; exit}' "${MINI_DIR}/test.pub")"
[ -n "${TEST_MINISIGN_PUBKEY}" ] || {
    echo "failed to generate ephemeral minisign pubkey" >&2
    exit 1
}

pass=0
fail=0

# assert <description> <command...>
assert() {
    local desc="$1"
    shift
    if "$@"; then
        pass=$((pass + 1))
        printf '  ok   %s\n' "${desc}"
    else
        fail=$((fail + 1))
        printf '  FAIL %s\n' "${desc}"
    fi
}

# matches <text> <extended regex>
matches() { grep -qE -- "$2" <<< "$1"; }
lacks() { ! grep -qE -- "$2" <<< "$1"; }
has_prefix() { [[ "$1" == "$2"* ]]; }

# Parse the entire stdout: extra diagnostics or a second object must fail.
valid_check_error_json() {
    python3 -c '
import json, sys
result = json.loads(sys.argv[1])
assert isinstance(result, dict)
assert result["latest"] is None
assert result["update_available"] is False
assert isinstance(result["error"], str) and result["error"]
' "$1"
}

# version_is <binary> <expected --version output>
version_is() { [ "$("$1" --version)" = "$2" ]; }
digest_is() { [ -f "$1" ] && [ "$(sha256sum "$1")" = "$2" ]; }

indent() { printf '    %s\n' "${1//$'\n'/$'\n'    }"; }

sign_manifest() {
    # sign_manifest PATH — writes PATH.minisig using the ephemeral secret.
    printf '\n' | minisign -Sm "$1" -s "${MINI_DIR}/test.secret" > /dev/null 2>&1
}

# publish_manifest <version> [sha256-override]
# Writes signed manifests under latest/download and download/v$version.
# Default digest is the current sidecar for TARGET; override replaces it.
publish_manifest() {
    local v="$1" sha="${2:-}"
    local archive="jsh-${v}-${TARGET}.tar.gz"
    local dir="${REL}/download/v${v}"
    if [ -z "${sha}" ]; then
        sha="$(cut -d' ' -f1 < "${dir}/${archive}.sha256" | head -1 | tr -d '\r')"
    fi
    mkdir -p "${REL}/latest/download" "${dir}"
    cat > "${REL}/latest/download/manifest.json" <<EOF
{
  "schema": 1,
  "name": "jsh",
  "version": "${v}",
  "tag": "v${v}",
  "repository": "beamiter/jsh",
  "artifacts": [
    {
      "target": "${TARGET}",
      "file": "${archive}",
      "sha256": "${sha}",
      "url": "file://${dir}/${archive}"
    }
  ]
}
EOF
    cp "${REL}/latest/download/manifest.json" "${dir}/manifest.json"
    sign_manifest "${REL}/latest/download/manifest.json"
    cp "${REL}/latest/download/manifest.json.minisig" "${dir}/manifest.json.minisig"
}

# make_release <version> [corrupt-checksum]
make_release() {
    local v="$1" corrupt="${2:-}"
    local stage="${ROOT}/stage/jsh-${v}-${TARGET}"
    rm -rf "${ROOT}/stage"
    mkdir -p "${stage}"
    # Stands in for the real binary: same --version contract, nothing else.
    cat > "${stage}/jsh" <<EOF
#!/bin/sh
case "\$1" in
  --version) echo "jsh ${v} (fake)" ;;
  *) echo "fake jsh ${v}: \$*" ;;
esac
EOF
    chmod +x "${stage}/jsh"
    mkdir -p "${REL}/download/v${v}" "${REL}/latest/download"
    tar -C "${ROOT}/stage" -czf "${REL}/download/v${v}/jsh-${v}-${TARGET}.tar.gz" "jsh-${v}-${TARGET}"
    (cd "${REL}/download/v${v}" && sha256sum "jsh-${v}-${TARGET}.tar.gz" > "jsh-${v}-${TARGET}.tar.gz.sha256")
    if [ -n "${corrupt}" ]; then
        # An all-zero digest never matches, whatever the real one happens to be.
        printf '%064d  %s\n' 0 "jsh-${v}-${TARGET}.tar.gz" \
            > "${REL}/download/v${v}/jsh-${v}-${TARGET}.tar.gz.sha256"
    fi
    publish_manifest "${v}"
}

# Re-checksum an already-written archive and refresh the signed manifest so
# hostile-member tests still reach the archive checks rather than dying on a
# digest mismatch first.
refresh_release_digests() {
    local v="$1"
    (cd "${REL}/download/v${v}" && sha256sum "jsh-${v}-${TARGET}.tar.gz" > "jsh-${v}-${TARGET}.tar.gz.sha256")
    publish_manifest "${v}"
}

# A pristine environment every time: no inherited PATH, cache, or state.
# --channel release explicitly: these tests exercise the release path, and the
# bare default (source) has tests of its own further down.
run() {
    env -i HOME="${FAKE_HOME}" PATH="${PATH_FOR_RUN}" \
        XDG_CACHE_HOME="${FAKE_HOME}/.cache" XDG_STATE_HOME="${FAKE_HOME}/.local/state" \
        JSH_INSTALL_BASE_URL="file://${REL}" JSH_INSTALL_TARGET="${TARGET}" \
        JSH_MINISIGN_PUBKEY="${TEST_MINISIGN_PUBKEY}" \
        sh "${INSTALLER}" --channel release "$@"
}

PATH_FOR_RUN="${MINISIGN_DIR}:/usr/local/bin:/usr/bin:/bin"

echo "== check with nothing installed =="
make_release 0.3.0
out="$(run --check --json 2> /dev/null)"
rc=$?
indent "${out}"
assert "exit 0" [ ${rc} -eq 0 ]
assert "reports nothing installed" matches "${out}" '"installed":null'
assert "reports the latest version" matches "${out}" '"latest":"0\.3\.0"'
assert "update available" matches "${out}" '"update_available":true'

echo "== fresh install =="
out="$(run 2>&1)"
rc=$?
indent "${out}"
assert "exit 0" [ ${rc} -eq 0 ]
assert "binary landed in ~/.local/bin" [ -x "${BIN}/jsh" ]
assert "installed binary runs" version_is "${BIN}/jsh" "jsh 0.3.0 (fake)"
assert "reports PATH resolution" matches "${out}" 'not on PATH|not this shell|not the copy just installed'

echo "== rerun is a no-op, --force reinstalls =="
PATH_FOR_RUN="${BIN}:${MINISIGN_DIR}:/usr/local/bin:/usr/bin:/bin"
out="$(run 2>&1)"
indent "${out}"
assert "no-op when current" matches "${out}" 'already installed'
out="$(run --force 2>&1)"
assert "--force reinstalls" matches "${out}" 'reinstalled jsh 0\.3\.0'

echo "== update keeps a rollback copy =="
make_release 0.3.1
out="$(run 2>&1)"
indent "${out}"
assert "update message" matches "${out}" 'updated jsh 0\.3\.0 -> 0\.3\.1'
assert "new version is live" version_is "${BIN}/jsh" "jsh 0.3.1 (fake)"
assert "previous binary kept for rollback" [ -f "${FAKE_HOME}/.local/state/jsh/rollback/jsh-0.3.0" ]

echo "== a failed installed-binary self-check rolls back atomically =="
# Fault-inject after the incoming file has been renamed into place. The first
# destination rename is corrupted so the self-check fails; the second is the
# rollback rename. Logging their source names proves both swaps stay inside the
# destination directory rather than copying over the live inode.
make_release 0.3.2
MV_STUB="${ROOT}/mv-stub"
MV_COUNT="${ROOT}/mv-count"
MV_LOG="${ROOT}/mv-log"
MV_FAIL_ROLLBACK="${ROOT}/mv-fail-rollback"
REAL_MV="$(command -v mv)"
mkdir -p "${MV_STUB}"
cat > "${MV_STUB}/mv" <<EOF
#!/bin/sh
previous=""
last=""
for arg in "\$@"; do
    previous="\${last}"
    last="\${arg}"
done
count=0
if [ "\${last}" = "${BIN}/jsh" ]; then
    count="\$(cat "${MV_COUNT}" 2> /dev/null || printf '0')"
    count=\$((count + 1))
    printf '%s\n' "\${count}" > "${MV_COUNT}"
    printf '%s\n' "\${previous}" >> "${MV_LOG}"
    if [ "\${count}" -eq 2 ] && [ -f "${MV_FAIL_ROLLBACK}" ]; then
        exit 1
    fi
fi
"${REAL_MV}" "\$@" || exit \$?
if [ "\${last}" = "${BIN}/jsh" ] && [ "\${count}" -eq 1 ]; then
    printf '%s\n' '#!/bin/sh' 'echo "not jsh"' > "\${last}"
    chmod 0755 "\${last}"
fi
EOF
chmod +x "${MV_STUB}/mv"

rm -f "${MV_COUNT}" "${MV_LOG}" "${MV_FAIL_ROLLBACK}"
PATH_FOR_RUN="${MV_STUB}:${BIN}:${MINISIGN_DIR}:/usr/local/bin:/usr/bin:/bin"
out="$(run 2>&1)"
rc=$?
indent "${out}"
assert "failed self-check exits nonzero" [ ${rc} -ne 0 ]
assert "reports the restored version" matches "${out}" 'failed its self-check; restored 0\.3\.1'
assert "previous version is live again" version_is "${BIN}/jsh" "jsh 0.3.1 (fake)"
assert "install uses a same-directory incoming rename" \
    has_prefix "$(sed -n '1p' "${MV_LOG}")" "${BIN}/.jsh.incoming."
assert "rollback uses a same-directory restoring rename" \
    has_prefix "$(sed -n '2p' "${MV_LOG}")" "${BIN}/.jsh.rollback."
assert "successful rollback leaves no restoring temporary" \
    [ -z "$(find "${BIN}" -maxdepth 1 -name '.jsh.rollback.*' -print -quit)" ]

echo "== a failed rollback keeps a truthful recovery copy =="
rm -f "${MV_COUNT}" "${MV_LOG}"
: > "${MV_FAIL_ROLLBACK}"
out="$(run 2>&1)"
rc=$?
indent "${out}"
rollback="${FAKE_HOME}/.local/state/jsh/rollback/jsh-0.3.1"
assert "failed rollback exits nonzero" [ ${rc} -ne 0 ]
assert "does not claim the rollback succeeded" \
    matches "${out}" "rollback failed; previous binary kept at ${rollback}"
assert "the recovery copy is still the previous version" \
    version_is "${rollback}" "jsh 0.3.1 (fake)"
assert "failed rollback leaves no restoring temporary" \
    [ -z "$(find "${BIN}" -maxdepth 1 -name '.jsh.rollback.*' -print -quit)" ]
# Repair the test fixture from the retained copy; the installer deliberately
# leaves the failed new binary in place when the atomic rollback rename fails.
cp "${rollback}" "${BIN}/jsh"
chmod 0755 "${BIN}/jsh"
rm -f "${MV_FAIL_ROLLBACK}" "${MV_COUNT}" "${MV_LOG}"
PATH_FOR_RUN="${BIN}:${MINISIGN_DIR}:/usr/local/bin:/usr/bin:/bin"
assert "fixture recovery restores the previous binary" \
    version_is "${BIN}/jsh" "jsh 0.3.1 (fake)"

echo "== update_available means newer, not merely different =="
# The published release is now OLDER than what is installed: a yanked tag, or a
# source build that ran ahead of the last tag. Comparing the two strings only
# says they differ, which offered the user a downgrade labelled "update".
make_release 0.2.9
out="$(run --check --json --max-age 0 2> /dev/null)"
indent "${out}"
assert "older published release is not an update" matches "${out}" '"update_available":false'
assert "still reports what is published" matches "${out}" '"latest":"0\.2\.9"'
out="$(run 2>&1)"
indent "${out}"
assert "a bare run refuses to walk backwards" matches "${out}" 'is newer than the published 0\.2\.9'
assert "installed binary untouched" version_is "${BIN}/jsh" "jsh 0.3.1 (fake)"
# Asking for the older build by name is still honoured: that is a real request,
# unlike a bare run silently replacing a working shell.
out="$(run --version 0.2.9 2>&1)"
assert "--version still installs the older build" version_is "${BIN}/jsh" "jsh 0.2.9 (fake)"
out="$(run --version 0.3.1 2>&1)"
assert "and back" version_is "${BIN}/jsh" "jsh 0.3.1 (fake)"
# Numeric ordering, not lexicographic: as text "0.10.0" sorts below "0.3.1".
make_release 0.10.0
out="$(run --check --json --max-age 0 2> /dev/null)"
assert "0.10.0 is newer than 0.3.1" matches "${out}" '"update_available":true'
make_release 0.3.1
out="$(run --check --json --max-age 0 2> /dev/null)"
assert "the same version is not an update" matches "${out}" '"update_available":false'

echo "== shared update-check cache =="
make_release 0.3.1
out="$(run --check --json --max-age 0 2> /dev/null)" # refresh the cache after the probes above
assert "cache written" [ -f "${FAKE_HOME}/.cache/jsh/update-check.json" ]
mv "${REL}/latest/download/manifest.json" "${REL}/manifest.hidden"
mv "${REL}/latest/download/manifest.json.minisig" "${REL}/manifest.minisig.hidden"
out="$(run --check --json --max-age 3600 2> /dev/null)"
assert "fresh cache answers without network" matches "${out}" '"latest":"0\.3\.1"'
out="$(run --check --json 2> /dev/null)"
rc=$?
indent "${out}"
assert "unreachable manifest exits nonzero" [ ${rc} -ne 0 ]
assert "unreachable manifest reported, not guessed" matches "${out}" '"error":"cannot reach'
mv "${REL}/manifest.hidden" "${REL}/latest/download/manifest.json"
mv "${REL}/manifest.minisig.hidden" "${REL}/latest/download/manifest.json.minisig"

echo "== a clamped PATH still finds the user's jsh via JSH_LOOKUP_PATH =="
# A terminal runs --check with PATH clamped to system directories so the tools
# the script executes cannot be hijacked. A jsh installed somewhere else —
# ~/.cargo/bin is the common case, and it is neither on the clamped PATH nor
# the default destination — then reports as "nothing installed", and the
# terminal raises an install banner forever. JSH_LOOKUP_PATH carries the PATH
# the terminal's user would have seen, for lookup only.
CARGO_HOME_DIR="${ROOT}/cargo-home"
CARGO_BIN="${CARGO_HOME_DIR}/.cargo/bin"
mkdir -p "${CARGO_BIN}"
cp "${BIN}/jsh" "${CARGO_BIN}/jsh"
clamped_run() {
    env -i HOME="${CARGO_HOME_DIR}" PATH="/usr/local/bin:/usr/bin:/bin:${MINISIGN_DIR}" \
        XDG_CACHE_HOME="${CARGO_HOME_DIR}/.cache" XDG_STATE_HOME="${CARGO_HOME_DIR}/.local/state" \
        JSH_INSTALL_BASE_URL="file://${REL}" JSH_INSTALL_TARGET="${TARGET}" \
        JSH_MINISIGN_PUBKEY="${TEST_MINISIGN_PUBKEY}" \
        "$@" sh "${INSTALLER}" --check --json
}
out="$(clamped_run 2> /dev/null)"
indent "${out}"
assert "the clamp alone misses the installed jsh" matches "${out}" '"installed":null'
assert "and would offer an install of what is already current" matches "${out}" '"update_available":true'
out="$(clamped_run JSH_LOOKUP_PATH="${CARGO_BIN}:/usr/local/bin:/usr/bin:/bin" 2> /dev/null)"
indent "${out}"
assert "the lookup path finds it" matches "${out}" '"installed":"0\.3\.1"'
assert "in place, where the user's shells resolve it" matches "${out}" "\"installed_path\":\"${CARGO_BIN}/jsh\""
assert "so nothing is offered" matches "${out}" '"update_available":false'

echo "== checksum mismatch aborts =="
make_release 0.3.2 corrupt
out="$(run 2>&1)"
rc=$?
indent "${out}"
assert "nonzero exit" [ ${rc} -ne 0 ]
assert "checksum message" matches "${out}" 'checksum mismatch'
assert "old binary untouched" version_is "${BIN}/jsh" "jsh 0.3.1 (fake)"

echo "== an absent or malformed checksum aborts =="
make_release 0.3.21
rm -f "${REL}/download/v0.3.21/jsh-0.3.21-${TARGET}.tar.gz.sha256"
out="$(run 2>&1)"
rc=$?
indent "${out}"
assert "nonzero exit" [ ${rc} -ne 0 ]
assert "refuses to install unverified bytes" matches "${out}" 'refusing to install unverified bytes'
assert "old binary untouched" version_is "${BIN}/jsh" "jsh 0.3.1 (fake)"

make_release 0.3.22
printf 'not-a-digest  jsh-0.3.22-%s.tar.gz\n' "${TARGET}" \
    > "${REL}/download/v0.3.22/jsh-0.3.22-${TARGET}.tar.gz.sha256"
out="$(run 2>&1)"
rc=$?
indent "${out}"
assert "nonzero exit" [ ${rc} -ne 0 ]
assert "malformed digest rejected" matches "${out}" 'is not a SHA-256 digest'

echo "== missing or bad manifest signature aborts =="
make_release 0.3.27
rm -f "${REL}/latest/download/manifest.json.minisig" \
    "${REL}/download/v0.3.27/manifest.json.minisig"
check_err="${ROOT}/check-signature.err"
installed_digest="$(sha256sum "${BIN}/jsh")"
out="$(run --check --json 2> "${check_err}")"
rc=$?
indent "${out}"
assert "unsigned --check exits nonzero" [ ${rc} -ne 0 ]
assert "unsigned --check emits one JSON object with unknown latest and no update" valid_check_error_json "${out}"
assert "unsigned --check leaves the installed bytes untouched" digest_is "${BIN}/jsh" "${installed_digest}"
assert "unsigned --check reports its authentication error" \
    matches "${out}" '"error":"no signature for the release manifest.*refusing to trust unsigned metadata"'
assert "unsigned --check keeps the human diagnostic on stderr" \
    matches "$(< "${check_err}")" 'refusing to trust unsigned metadata'
out="$(run 2>&1)"
rc=$?
indent "${out}"
assert "missing minisig exits nonzero" [ ${rc} -ne 0 ]
assert "refuses unsigned metadata" matches "${out}" 'refusing to trust unsigned metadata'
assert "old binary untouched" version_is "${BIN}/jsh" "jsh 0.3.1 (fake)"

make_release 0.3.28
# Detached sig for different bytes / wrong key must not verify.
printf '{"tampered":true}\n' > "${ROOT}/tamper-manifest.json"
printf '\n' | minisign -Sm "${ROOT}/tamper-manifest.json" -s "${MINI_DIR}/test.secret" > /dev/null 2>&1
cp "${ROOT}/tamper-manifest.json.minisig" "${REL}/latest/download/manifest.json.minisig"
cp "${ROOT}/tamper-manifest.json.minisig" "${REL}/download/v0.3.28/manifest.json.minisig"
out="$(run --check --json 2> "${check_err}")"
rc=$?
indent "${out}"
assert "bad-signature --check exits nonzero" [ ${rc} -ne 0 ]
assert "bad-signature --check emits one JSON object with unknown latest and no update" valid_check_error_json "${out}"
assert "bad-signature --check leaves the installed bytes untouched" digest_is "${BIN}/jsh" "${installed_digest}"
assert "bad-signature --check reports verification failure" \
    matches "${out}" '"error":"release manifest signature verification failed"'
out="$(run 2>&1)"
rc=$?
indent "${out}"
assert "bad signature exits nonzero" [ ${rc} -ne 0 ]
assert "signature verification failed" matches "${out}" 'signature verification failed'
assert "old binary untouched" version_is "${BIN}/jsh" "jsh 0.3.1 (fake)"

echo "== unavailable minisign is structured in --check --json =="
make_release 0.3.281
# Keep the normal check tools but omit minisign even when the host has it in
# /usr/bin. This case must exercise the missing dependency on every machine.
NO_MINISIGN_BIN="${ROOT}/no-minisign-bin"
mkdir -p "${NO_MINISIGN_BIN}"
for tool in awk basename cat curl cut date dirname grep head mkdir mktemp readlink rm sed sh timeout tr uname wc; do
    if have "${tool}"; then
        ln -s "$(command -v "${tool}")" "${NO_MINISIGN_BIN}/${tool}"
    fi
done
PATH_FOR_RUN="${BIN}:${NO_MINISIGN_BIN}"
out="$(run --check --json 2> "${check_err}")"
rc=$?
indent "${out}"
assert "missing-minisign --check exits nonzero" [ ${rc} -ne 0 ]
assert "missing-minisign --check emits one JSON object with unknown latest and no update" valid_check_error_json "${out}"
assert "missing-minisign --check leaves the installed bytes untouched" digest_is "${BIN}/jsh" "${installed_digest}"
assert "missing-minisign --check reports how to install the missing verifier" \
    matches "${out}" '"error":"need minisign to verify the release manifest.*sudo apt install minisign'
PATH_FOR_RUN="${BIN}:${MINISIGN_DIR}:/usr/local/bin:/usr/bin:/bin"

echo "== sidecar matching archive but not signed manifest aborts =="
make_release 0.3.29
real_sha="$(cut -d' ' -f1 < "${REL}/download/v0.3.29/jsh-0.3.29-${TARGET}.tar.gz.sha256")"
# Signed manifest lies; sidecar still matches the archive.
publish_manifest 0.3.29 "$(printf '%064d' 0)"
# Restore a correct sidecar so archive == sidecar != manifest.
printf '%s  jsh-0.3.29-%s.tar.gz\n' "${real_sha}" "${TARGET}" \
    > "${REL}/download/v0.3.29/jsh-0.3.29-${TARGET}.tar.gz.sha256"
out="$(run 2>&1)"
rc=$?
indent "${out}"
assert "nonzero exit" [ ${rc} -ne 0 ]
assert "sidecar/manifest disagreement refused" \
    matches "${out}" 'does not match the signed manifest digest'
assert "old binary untouched" version_is "${BIN}/jsh" "jsh 0.3.1 (fake)"

echo "== signed manifest digests must contain exactly 64 hex characters =="
for digest_length in 63 65; do
    make_release 0.3.291
    publish_manifest 0.3.291 "$(printf '%0*d' "${digest_length}" 0)"
    out="$(run 2>&1)"
    rc=$?
    indent "${out}"
    assert "${digest_length}-character manifest digest exits nonzero" [ ${rc} -ne 0 ]
    assert "${digest_length}-character manifest digest is refused" \
        matches "${out}" 'signed manifest has no SHA-256 for target'
    assert "${digest_length}-character manifest digest leaves installed bytes untouched" \
        digest_is "${BIN}/jsh" "${installed_digest}"
done

echo "== hostile archive members are rejected before extraction =="
make_release 0.3.23
rm -rf "${ROOT}/hostile"
mkdir -p "${ROOT}/hostile/jsh-0.3.23-${TARGET}"
ln -s /etc/passwd "${ROOT}/hostile/jsh-0.3.23-${TARGET}/jsh"
(cd "${ROOT}/hostile" && tar -czf "${REL}/download/v0.3.23/jsh-0.3.23-${TARGET}.tar.gz" "jsh-0.3.23-${TARGET}")
refresh_release_digests 0.3.23
out="$(run 2>&1)"
rc=$?
indent "${out}"
assert "nonzero exit" [ ${rc} -ne 0 ]
assert "symlink member rejected" matches "${out}" 'contains a link or special file'

make_release 0.3.24
rm -rf "${ROOT}/hostile"
mkdir -p "${ROOT}/hostile/jsh-0.3.24-${TARGET}"
printf '#!/bin/sh\necho "jsh 0.3.24 (fake)"\n' > "${ROOT}/hostile/jsh-0.3.24-${TARGET}/jsh"
chmod +x "${ROOT}/hostile/jsh-0.3.24-${TARGET}/jsh"
printf 'payload\n' > "${ROOT}/hostile/jsh-0.3.24-${TARGET}/extra"
(cd "${ROOT}/hostile" && tar -czf "${REL}/download/v0.3.24/jsh-0.3.24-${TARGET}.tar.gz" "jsh-0.3.24-${TARGET}")
refresh_release_digests 0.3.24
out="$(run 2>&1)"
rc=$?
indent "${out}"
assert "nonzero exit" [ ${rc} -ne 0 ]
assert "extra member rejected" matches "${out}" 'unexpected member'

make_release 0.3.25
rm -rf "${ROOT}/hostile"
mkdir -p "${ROOT}/hostile/jsh-0.3.25-${TARGET}"
printf '#!/bin/sh\necho "jsh 0.3.25 (fake)"\n' > "${ROOT}/hostile/jsh-0.3.25-${TARGET}/jsh"
chmod +x "${ROOT}/hostile/jsh-0.3.25-${TARGET}/jsh"
printf 'owned\n' > "${ROOT}/hostile/escape"
(cd "${ROOT}/hostile/jsh-0.3.25-${TARGET}" \
    && tar -cPzf "${REL}/download/v0.3.25/jsh-0.3.25-${TARGET}.tar.gz" \
        -C "${ROOT}/hostile" "jsh-0.3.25-${TARGET}" "../hostile/escape" 2> /dev/null)
refresh_release_digests 0.3.25
out="$(run 2>&1)"
rc=$?
indent "${out}"
assert "nonzero exit" [ ${rc} -ne 0 ]
assert "traversal member rejected" matches "${out}" 'unexpected member'
assert "nothing escaped the temporary directory" [ ! -f "${ROOT}/hostile/escape.installed" ]
assert "old binary untouched" version_is "${BIN}/jsh" "jsh 0.3.1 (fake)"

echo "== untrusted version, target, and base URL grammars are rejected =="
out="$(run --version '../../etc' 2>&1)"
rc=$?
assert "traversal version rejected" [ ${rc} -ne 0 ]
assert "traversal version message" matches "${out}" 'not a valid version'
out="$(env -i HOME="${FAKE_HOME}" PATH="${PATH_FOR_RUN}" \
    JSH_INSTALL_BASE_URL="http://example.invalid/releases" \
    JSH_INSTALL_TARGET="${TARGET}" \
    JSH_MINISIGN_PUBKEY="${TEST_MINISIGN_PUBKEY}" \
    sh "${INSTALLER}" --check 2>&1)"
rc=$?
assert "plain-HTTP base URL rejected" [ ${rc} -ne 0 ]
assert "plain-HTTP message" matches "${out}" 'JSH_INSTALL_BASE_URL'
out="$(env -i HOME="${FAKE_HOME}" PATH="${PATH_FOR_RUN}" \
    JSH_INSTALL_BASE_URL="file://${REL}" \
    JSH_INSTALL_TARGET="../../etc" \
    JSH_MINISIGN_PUBKEY="${TEST_MINISIGN_PUBKEY}" \
    sh "${INSTALLER}" --check 2>&1)"
rc=$?
assert "traversal target rejected" [ ${rc} -ne 0 ]
assert "traversal target message" matches "${out}" 'not a valid target triple'

echo "== the update-check cache is private and symlink-safe =="
make_release 0.3.26
rm -f "${FAKE_HOME}/.cache/jsh/update-check.json"
run --check --json --max-age 0 > /dev/null 2>&1
assert "cache is 0600" [ "$(stat -c '%a' "${FAKE_HOME}/.cache/jsh/update-check.json")" = "600" ]
assert "cache directory is 0700" [ "$(stat -c '%a' "${FAKE_HOME}/.cache/jsh")" = "700" ]
victim="${ROOT}/cache-victim"
printf 'untouched\n' > "${victim}"
rm -f "${FAKE_HOME}/.cache/jsh/update-check.json"
ln -s "${victim}" "${FAKE_HOME}/.cache/jsh/update-check.json"
run --check --json --max-age 0 > /dev/null 2>&1
assert "symlinked cache is replaced, not followed" [ "$(cat "${victim}")" = "untouched" ]
assert "cache is a regular file again" [ ! -L "${FAKE_HOME}/.cache/jsh/update-check.json" ]

echo "== a binary that reports the wrong version is rejected =="
make_release 0.3.3
stage="${ROOT}/stage2/jsh-0.3.3-${TARGET}"
mkdir -p "${stage}"
printf '#!/bin/sh\necho "jsh 9.9.9"\n' > "${stage}/jsh"
chmod +x "${stage}/jsh"
tar -C "${ROOT}/stage2" -czf "${REL}/download/v0.3.3/jsh-0.3.3-${TARGET}.tar.gz" "jsh-0.3.3-${TARGET}"
refresh_release_digests 0.3.3
out="$(run 2>&1)"
rc=$?
indent "${out}"
assert "nonzero exit" [ ${rc} -ne 0 ]
assert "version mismatch rejected" matches "${out}" 'reports 9\.9\.9'

echo "== the BSD jsh on PATH is reported, not adopted =="
BSD="${ROOT}/usr-bin"
mkdir -p "${BSD}"
printf '#!/bin/sh\necho "jsh: unknown option -- version" >&2\nexit 1\n' > "${BSD}/jsh"
chmod +x "${BSD}/jsh"
make_release 0.3.4
PATH_FOR_RUN="${BSD}:${BIN}:${MINISIGN_DIR}:/usr/local/bin:/usr/bin:/bin"
out="$(run 2>&1)"
indent "${out}"
assert "foreign jsh reported" matches "${out}" 'which is not this shell'
assert "managed copy still updated" matches "${out}" 'updated jsh 0\.3\.1 -> 0\.3\.4'
out="$(run --check --json 2> /dev/null)"
assert "--check reports shadowed_by" matches "${out}" "\"shadowed_by\":\"${BSD}/jsh\""

echo "== an existing jsh on PATH is updated in place =="
ALT="${ROOT}/cargo-bin"
mkdir -p "${ALT}"
printf '#!/bin/sh\necho "jsh 0.1.0"\n' > "${ALT}/jsh"
chmod +x "${ALT}/jsh"
PATH_FOR_RUN="${ALT}:${MINISIGN_DIR}:/usr/local/bin:/usr/bin:/bin"
out="$(run 2>&1)"
indent "${out}"
assert "no second copy is created" matches "${out}" "updated jsh 0\.1\.0 -> 0\.3\.4 at ${ALT}/jsh"

echo "== dry run changes nothing =="
make_release 0.4.0
before="$("${ALT}/jsh" --version)"
out="$(run --dry-run 2>&1)"
indent "${out}"
assert "binary untouched" version_is "${ALT}/jsh" "${before}"

echo "== a source build aims for the static musl target =="
# Stubs stand in for the toolchain: cargo records its arguments and the CC
# environment it was given, rustup records target additions. The point under
# test is what the installer asks for, not what rustc does with it.
STUB="${ROOT}/toolchain-stub"
SRCLOG="${ROOT}/source-build.log"
mkdir -p "${STUB}"
case "$(uname -m)" in
    aarch64 | arm64) SRC_ARCH="aarch64" ;;
    *) SRC_ARCH="x86_64" ;;
esac
MUSL_TRIPLE="${SRC_ARCH}-unknown-linux-musl"

cat > "${STUB}/cargo" <<EOF
#!/bin/sh
root=""
prev=""
for a in "\$@"; do
    [ "\$prev" = "--root" ] && root="\$a"
    prev="\$a"
done
{
    echo "argv=\$*"
    echo "cc_x86=\${CC_x86_64_unknown_linux_musl:-}"
    echo "cc_arm=\${CC_aarch64_unknown_linux_musl:-}"
} > "${SRCLOG}"
mkdir -p "\$root/bin"
printf '#!/bin/sh\necho "jsh 9.9.9 (stub source build)"\n' > "\$root/bin/jsh"
chmod +x "\$root/bin/jsh"
EOF
chmod +x "${STUB}/cargo"

cat > "${STUB}/rustup" <<EOF
#!/bin/sh
case "\$1 \$2" in
    "target list") [ -f "${STUB}/target-added" ] && echo "${MUSL_TRIPLE}" ;;
    "target add") : > "${STUB}/target-added"; echo "rustup-add \$3" >> "${SRCLOG}.rustup" ;;
esac
EOF
chmod +x "${STUB}/rustup"
# Both spellings the installer probes for, so the assertion does not depend
# on whether the machine running this suite has real musl compilers.
printf '#!/bin/sh\nexit 0\n' > "${STUB}/musl-gcc"
printf '#!/bin/sh\nexit 0\n' > "${STUB}/${SRC_ARCH}-linux-musl-gcc"
chmod +x "${STUB}/musl-gcc" "${STUB}/${SRC_ARCH}-linux-musl-gcc"

# A PATH with everything the installer needs except a musl compiler, for the
# fallback case: presence is probed with command -v, so absence can only be
# simulated by a PATH that genuinely lacks them — a machine that has
# musl-tools installed would otherwise satisfy the probe behind the stub's
# back.
THIN="${ROOT}/thinbin"
mkdir -p "${THIN}"
for d in /usr/bin /bin; do
    for f in "$d"/*; do
        b="$(basename "$f")"
        case "$b" in
            musl-gcc | *-musl-gcc) continue ;;
        esac
        [ -e "${THIN}/$b" ] || ln -s "$f" "${THIN}/$b" 2>/dev/null
    done
done

# No JSH_INSTALL_TARGET here: the musl default is exactly what is under test.
run_src() {
    env -i HOME="${FAKE_HOME}" PATH="${STUB}:${PATH_FOR_RUN}" \
        XDG_CACHE_HOME="${FAKE_HOME}/.cache" XDG_STATE_HOME="${FAKE_HOME}/.local/state" \
        JSH_INSTALL_BASE_URL="file://${REL}" \
        JSH_MINISIGN_PUBKEY="${TEST_MINISIGN_PUBKEY}" \
        sh "${INSTALLER}" --channel source --bin-dir "${ROOT}/src-bin"
}

rm -f "${STUB}/target-added" "${SRCLOG}.rustup"
out="$(run_src 2>&1)"
rc=$?
indent "$(grep -E 'adding|building' <<< "${out}")"
assert "exit 0" [ ${rc} -eq 0 ]
assert "the std for the target was added first" \
    matches "$(cat "${SRCLOG}.rustup" 2>/dev/null)" "rustup-add ${MUSL_TRIPLE}"
assert "cargo builds the musl target" matches "$(cat "${SRCLOG}")" -- "--target ${MUSL_TRIPLE}"
assert "the musl C compiler reaches the C dependency" \
    matches "$(cat "${SRCLOG}")" "cc_(x86|arm)=(${SRC_ARCH}-linux-)?musl-gcc"
assert "the stub build was installed" [ -x "${ROOT}/src-bin/jsh" ]

echo "== without a musl compiler the build refuses instead of going dynamic =="
rm -rf "${ROOT}/src-bin"; rm -f "${SRCLOG}"
THINSTUB="${ROOT}/thinstub"
mkdir -p "${THINSTUB}"
ln -sf "${STUB}/cargo" "${THINSTUB}/cargo"
ln -sf "${STUB}/rustup" "${THINSTUB}/rustup"
out="$(env -i HOME="${FAKE_HOME}" PATH="${THINSTUB}:${THIN}" \
    XDG_CACHE_HOME="${FAKE_HOME}/.cache" XDG_STATE_HOME="${FAKE_HOME}/.local/state" \
    JSH_INSTALL_BASE_URL="file://${REL}" \
    JSH_MINISIGN_PUBKEY="${TEST_MINISIGN_PUBKEY}" \
    sh "${INSTALLER}" --channel source --bin-dir "${ROOT}/src-bin" --force 2>&1)"
rc=$?
indent "$(grep -E 'musl-tools|lend itself|glibc' <<< "${out}")"
assert "exit nonzero" [ ${rc} -ne 0 ]
assert "names the package to install" matches "${out}" "musl-tools"
assert "says what a dynamic build cannot do" matches "${out}" "lend itself"
assert "names the glibc escape hatch" matches "${out}" "JSH_INSTALL_TARGET=${SRC_ARCH}-unknown-linux-gnu"
assert "cargo was never consulted" [ ! -f "${SRCLOG}" ]
assert "nothing was installed" [ ! -e "${ROOT}/src-bin/jsh" ]

echo "== an explicit gnu triple is obeyed for source too =="
rm -rf "${ROOT}/src-bin"; rm -f "${SRCLOG}"
out="$(env -i HOME="${FAKE_HOME}" PATH="${STUB}:${PATH_FOR_RUN}" \
    XDG_CACHE_HOME="${FAKE_HOME}/.cache" XDG_STATE_HOME="${FAKE_HOME}/.local/state" \
    JSH_INSTALL_BASE_URL="file://${REL}" JSH_INSTALL_TARGET="${SRC_ARCH}-unknown-linux-gnu" \
    JSH_MINISIGN_PUBKEY="${TEST_MINISIGN_PUBKEY}" \
    sh "${INSTALLER}" --channel source --bin-dir "${ROOT}/src-bin" --force 2>&1)"
assert "exit 0" [ $? -eq 0 ]
assert "cargo builds the gnu target" \
    matches "$(cat "${SRCLOG}")" -- "--target ${SRC_ARCH}-unknown-linux-gnu"
assert "no musl compiler is exported for a gnu build" \
    matches "$(cat "${SRCLOG}")" "cc_x86=\$" 

echo "== a bare install defaults to the source channel =="
# No --channel: the default is a source build even when releases exist, so a
# bare install always lands the static musl binary the release would have
# shipped, and works before the first release exists.
rm -rf "${ROOT}/src-bin"; rm -f "${SRCLOG}"
out="$(env -i HOME="${FAKE_HOME}" PATH="${STUB}:${PATH_FOR_RUN}" \
    XDG_CACHE_HOME="${FAKE_HOME}/.cache" XDG_STATE_HOME="${FAKE_HOME}/.local/state" \
    JSH_INSTALL_BASE_URL="file://${REL}" \
    JSH_MINISIGN_PUBKEY="${TEST_MINISIGN_PUBKEY}" \
    sh "${INSTALLER}" --bin-dir "${ROOT}/src-bin" 2>&1)"
rc=$?
indent "$(grep -E 'building' <<< "${out}")"
assert "exit 0" [ ${rc} -eq 0 ]
assert "cargo was consulted" [ -f "${SRCLOG}" ]
assert "cargo builds the musl target" matches "$(cat "${SRCLOG}")" -- "--target ${MUSL_TRIPLE}"
assert "no release was downloaded" lacks "${out}" "downloading"
assert "the source build was installed" [ -x "${ROOT}/src-bin/jsh" ]

echo "== an explicit release channel with no releases falls back to source =="
# The state every repository is in before its first tag: asking for a release
# that does not exist reroutes to source rather than failing the install.
EMPTY_REL="${ROOT}/no-releases"
mkdir -p "${EMPTY_REL}"
rm -rf "${ROOT}/src-bin"; rm -f "${SRCLOG}"
out="$(env -i HOME="${FAKE_HOME}" PATH="${STUB}:${PATH_FOR_RUN}" \
    XDG_CACHE_HOME="${FAKE_HOME}/.cache" XDG_STATE_HOME="${FAKE_HOME}/.local/state" \
    JSH_INSTALL_BASE_URL="file://${EMPTY_REL}" \
    JSH_MINISIGN_PUBKEY="${TEST_MINISIGN_PUBKEY}" \
    sh "${INSTALLER}" --channel release --bin-dir "${ROOT}/src-bin" 2>&1)"
rc=$?
indent "$(grep -E 'falling back|manifest' <<< "${out}")"
assert "exit 0" [ ${rc} -eq 0 ]
assert "says why it is not installing a release" matches "${out}" "no release published yet"
assert "announces the source fallback" matches "${out}" "falling back to --channel source"
assert "the source build was installed" [ -x "${ROOT}/src-bin/jsh" ]

echo "== failed verification never falls back to source =="
# "Build something else instead" is not an answer to a checksum mismatch; only
# not-found is allowed to reroute.
make_release 0.4.0 corrupt-checksum
rm -f "${SRCLOG}"
out="$(env -i HOME="${FAKE_HOME}" PATH="${STUB}:${PATH_FOR_RUN}" \
    XDG_CACHE_HOME="${FAKE_HOME}/.cache" XDG_STATE_HOME="${FAKE_HOME}/.local/state" \
    JSH_INSTALL_BASE_URL="file://${REL}" JSH_INSTALL_TARGET="${TARGET}" \
    JSH_MINISIGN_PUBKEY="${TEST_MINISIGN_PUBKEY}" \
    sh "${INSTALLER}" --channel release --bin-dir "${ROOT}/src-bin" --force 2>&1)"
rc=$?
assert "exit nonzero" [ ${rc} -ne 0 ]
assert "dies on the checksum" matches "${out}" "checksum mismatch"
assert "cargo was never consulted" [ ! -f "${SRCLOG}" ]
make_release 0.3.0

echo "== staging still refuses to fall back =="
# The staged artifact is for another machine; a build on this one is not it.
rm -f "${SRCLOG}"
out="$(env -i HOME="${FAKE_HOME}" PATH="${STUB}:${PATH_FOR_RUN}" \
    XDG_CACHE_HOME="${FAKE_HOME}/.cache" XDG_STATE_HOME="${FAKE_HOME}/.local/state" \
    JSH_INSTALL_BASE_URL="file://${EMPTY_REL}" JSH_INSTALL_TARGET="${TARGET}" \
    JSH_MINISIGN_PUBKEY="${TEST_MINISIGN_PUBKEY}" \
    sh "${INSTALLER}" --stage-dir "${ROOT}/stage-out" 2>&1)"
rc=$?
assert "exit nonzero" [ ${rc} -ne 0 ]
assert "names the unreadable manifest" matches "${out}" "cannot read the release manifest"
assert "cargo was never consulted" [ ! -f "${SRCLOG}" ]

# The installer identifies jsh by its --version banner; make sure a real build
# still matches that contract when one is lying around.
if [ -x "${REPO_ROOT}/target/release/jsh" ]; then
    echo "== the real binary matches the identity contract =="
    banner="$("${REPO_ROOT}/target/release/jsh" --version)"
    assert "target/release/jsh reports '${banner}'" matches "${banner}" '^jsh [0-9]'
fi

echo
printf 'passed %d, failed %d\n' "${pass}" "${fail}"
[ "${fail}" -eq 0 ]
