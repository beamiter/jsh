#!/bin/bash
# Exercises scripts/install-rsh.sh against a fake release tree served over
# file://, so the whole install/update path runs without network access.
#
#   ./scripts/test-install-rsh.sh
set -uo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
INSTALLER="${SCRIPT_DIR}/install-rsh.sh"
[ -f "${INSTALLER}" ] || {
    echo "missing ${INSTALLER}" >&2
    exit 1
}

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/test-install-rsh.XXXXXX")"
trap 'rm -rf "${ROOT}"' EXIT

REL="${ROOT}/release"
FAKE_HOME="${ROOT}/home"
BIN="${FAKE_HOME}/.local/bin"
TARGET="x86_64-unknown-linux-gnu"
mkdir -p "${FAKE_HOME}"

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
matches() { grep -qE "$2" <<< "$1"; }

# version_is <binary> <expected --version output>
version_is() { [ "$("$1" --version)" = "$2" ]; }

indent() { printf '    %s\n' "${1//$'\n'/$'\n'    }"; }

# make_release <version> [corrupt-checksum]
make_release() {
    local v="$1" corrupt="${2:-}"
    local stage="${ROOT}/stage/rsh-${v}-${TARGET}"
    rm -rf "${ROOT}/stage"
    mkdir -p "${stage}"
    # Stands in for the real binary: same --version contract, nothing else.
    cat > "${stage}/rsh" <<EOF
#!/bin/sh
case "\$1" in
  --version) echo "rsh ${v} (fake)" ;;
  *) echo "fake rsh ${v}: \$*" ;;
esac
EOF
    chmod +x "${stage}/rsh"
    mkdir -p "${REL}/download/v${v}" "${REL}/latest/download"
    tar -C "${ROOT}/stage" -czf "${REL}/download/v${v}/rsh-${v}-${TARGET}.tar.gz" "rsh-${v}-${TARGET}"
    (cd "${REL}/download/v${v}" && sha256sum "rsh-${v}-${TARGET}.tar.gz" > "rsh-${v}-${TARGET}.tar.gz.sha256")
    if [ -n "${corrupt}" ]; then
        # An all-zero digest never matches, whatever the real one happens to be.
        printf '%064d  %s\n' 0 "rsh-${v}-${TARGET}.tar.gz" \
            > "${REL}/download/v${v}/rsh-${v}-${TARGET}.tar.gz.sha256"
    fi
    cat > "${REL}/latest/download/manifest.json" <<EOF
{
  "schema": 1,
  "name": "rsh",
  "version": "${v}",
  "tag": "v${v}",
  "repository": "beamiter/rsh",
  "artifacts": [
    {
      "target": "${TARGET}",
      "file": "rsh-${v}-${TARGET}.tar.gz",
      "sha256": "$(cut -d' ' -f1 < "${REL}/download/v${v}/rsh-${v}-${TARGET}.tar.gz.sha256")",
      "url": "file://${REL}/download/v${v}/rsh-${v}-${TARGET}.tar.gz"
    }
  ]
}
EOF
}

# A pristine environment every time: no inherited PATH, cache, or state.
run() {
    env -i HOME="${FAKE_HOME}" PATH="${PATH_FOR_RUN}" \
        XDG_CACHE_HOME="${FAKE_HOME}/.cache" XDG_STATE_HOME="${FAKE_HOME}/.local/state" \
        RSH_INSTALL_BASE_URL="file://${REL}" RSH_INSTALL_TARGET="${TARGET}" \
        sh "${INSTALLER}" "$@"
}

PATH_FOR_RUN="/usr/local/bin:/usr/bin:/bin"

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
assert "binary landed in ~/.local/bin" [ -x "${BIN}/rsh" ]
assert "installed binary runs" version_is "${BIN}/rsh" "rsh 0.3.0 (fake)"
assert "reports PATH resolution" matches "${out}" 'not on PATH|not this shell|not the copy just installed'

echo "== rerun is a no-op, --force reinstalls =="
PATH_FOR_RUN="${BIN}:/usr/local/bin:/usr/bin:/bin"
out="$(run 2>&1)"
indent "${out}"
assert "no-op when current" matches "${out}" 'already installed'
out="$(run --force 2>&1)"
assert "--force reinstalls" matches "${out}" 'reinstalled rsh 0\.3\.0'

echo "== update keeps a rollback copy =="
make_release 0.3.1
out="$(run 2>&1)"
indent "${out}"
assert "update message" matches "${out}" 'updated rsh 0\.3\.0 -> 0\.3\.1'
assert "new version is live" version_is "${BIN}/rsh" "rsh 0.3.1 (fake)"
assert "previous binary kept for rollback" [ -f "${FAKE_HOME}/.local/state/rsh/rollback/rsh-0.3.0" ]

echo "== shared update-check cache =="
assert "cache written" [ -f "${FAKE_HOME}/.cache/rsh/update-check.json" ]
mv "${REL}/latest/download/manifest.json" "${REL}/manifest.hidden"
out="$(run --check --json --max-age 3600 2> /dev/null)"
assert "fresh cache answers without network" matches "${out}" '"latest":"0\.3\.1"'
out="$(run --check --json 2> /dev/null)"
rc=$?
indent "${out}"
assert "unreachable manifest exits nonzero" [ ${rc} -ne 0 ]
assert "unreachable manifest reported, not guessed" matches "${out}" '"error":"cannot reach'
mv "${REL}/manifest.hidden" "${REL}/latest/download/manifest.json"

echo "== checksum mismatch aborts =="
make_release 0.3.2 corrupt
out="$(run 2>&1)"
rc=$?
indent "${out}"
assert "nonzero exit" [ ${rc} -ne 0 ]
assert "checksum message" matches "${out}" 'checksum mismatch'
assert "old binary untouched" version_is "${BIN}/rsh" "rsh 0.3.1 (fake)"

echo "== a binary that reports the wrong version is rejected =="
make_release 0.3.3
stage="${ROOT}/stage2/rsh-0.3.3-${TARGET}"
mkdir -p "${stage}"
printf '#!/bin/sh\necho "rsh 9.9.9"\n' > "${stage}/rsh"
chmod +x "${stage}/rsh"
tar -C "${ROOT}/stage2" -czf "${REL}/download/v0.3.3/rsh-0.3.3-${TARGET}.tar.gz" "rsh-0.3.3-${TARGET}"
(cd "${REL}/download/v0.3.3" && sha256sum "rsh-0.3.3-${TARGET}.tar.gz" > "rsh-0.3.3-${TARGET}.tar.gz.sha256")
out="$(run 2>&1)"
rc=$?
indent "${out}"
assert "nonzero exit" [ ${rc} -ne 0 ]
assert "version mismatch rejected" matches "${out}" 'reports 9\.9\.9'

echo "== the BSD rsh on PATH is reported, not adopted =="
BSD="${ROOT}/usr-bin"
mkdir -p "${BSD}"
printf '#!/bin/sh\necho "rsh: unknown option -- version" >&2\nexit 1\n' > "${BSD}/rsh"
chmod +x "${BSD}/rsh"
make_release 0.3.4
PATH_FOR_RUN="${BSD}:${BIN}:/usr/local/bin:/usr/bin:/bin"
out="$(run 2>&1)"
indent "${out}"
assert "foreign rsh reported" matches "${out}" 'which is not this shell'
assert "managed copy still updated" matches "${out}" 'updated rsh 0\.3\.1 -> 0\.3\.4'
out="$(run --check --json 2> /dev/null)"
assert "--check reports shadowed_by" matches "${out}" "\"shadowed_by\":\"${BSD}/rsh\""

echo "== an existing rsh on PATH is updated in place =="
ALT="${ROOT}/cargo-bin"
mkdir -p "${ALT}"
printf '#!/bin/sh\necho "rsh 0.1.0"\n' > "${ALT}/rsh"
chmod +x "${ALT}/rsh"
PATH_FOR_RUN="${ALT}:/usr/local/bin:/usr/bin:/bin"
out="$(run 2>&1)"
indent "${out}"
assert "no second copy is created" matches "${out}" "updated rsh 0\.1\.0 -> 0\.3\.4 at ${ALT}/rsh"

echo "== dry run changes nothing =="
make_release 0.4.0
before="$("${ALT}/rsh" --version)"
out="$(run --dry-run 2>&1)"
indent "${out}"
assert "binary untouched" version_is "${ALT}/rsh" "${before}"

# The installer identifies rsh by its --version banner; make sure a real build
# still matches that contract when one is lying around.
if [ -x "${REPO_ROOT}/target/release/rsh" ]; then
    echo "== the real binary matches the identity contract =="
    banner="$("${REPO_ROOT}/target/release/rsh" --version)"
    assert "target/release/rsh reports '${banner}'" matches "${banner}" '^rsh [0-9]'
fi

echo
printf 'passed %d, failed %d\n' "${pass}" "${fail}"
[ "${fail}" -eq 0 ]
