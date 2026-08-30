#!/bin/bash
# Exercises scripts/jsh-remote.sh end to end without a remote machine.
#
#   ./scripts/test-jsh-remote.sh
#
# A stub `docker` on PATH turns `docker exec ... CONTAINER CMD` into a local
# run of CMD with HOME pointed at a throwaway directory. That is enough to make
# the whole deployment real: the probe runs, the artifact is staged through
# install-jsh.sh over file://, the binary is pushed, verified, cached, and
# executed, and the sandbox is torn down — all against directories this script
# owns and deletes.
set -uo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REMOTE="${SCRIPT_DIR}/jsh-remote.sh"
INSTALLER="${SCRIPT_DIR}/install-jsh.sh"
for f in "${REMOTE}" "${INSTALLER}"; do
    [ -f "${f}" ] || {
        echo "missing ${f}" >&2
        exit 1
    }
done

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/test-jsh-remote.XXXXXX")"
trap 'rm -rf "${ROOT}"' EXIT

REL="${ROOT}/release"          # fake release tree served over file://
LOCAL_HOME="${ROOT}/local"     # the machine running jsh-remote.sh
CTR_HOME="${ROOT}/container"   # the "remote" $HOME
STUB="${ROOT}/stub"            # stub docker + stub local jsh
LOG="${ROOT}/session.log"      # what the deployed jsh saw
TARGET="x86_64-unknown-linux-musl"
VERSION="0.3.0"
mkdir -p "${LOCAL_HOME}" "${CTR_HOME}" "${STUB}"

pass=0
fail=0

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
matches() { grep -qE -- "$2" <<< "$1"; }
lacks() { ! grep -qE -- "$2" <<< "$1"; }
indent() { printf '    %s\n' "${1//$'\n'/$'\n'    }"; }

# --- stubs -------------------------------------------------------------------

# `docker exec [-i|-t|-it] [-u USER] [-e K=V]... CONTAINER CMD...` -> run CMD
# here, with the container's own HOME so the remote side really does see a
# different home than the local side.
cat > "${STUB}/docker" <<EOF
#!/bin/sh
[ "\$1" = exec ] || { echo "stub docker: unsupported: \$*" >&2; exit 1; }
shift
passed=""
while [ \$# -gt 0 ]; do
    case "\$1" in
        -i|-t|-it|-ti) shift ;;
        -u) shift 2 ;;
        -e) passed="\${passed} \$2"; shift 2 ;;
        *) break ;;
    esac
done
container="\$1"; shift
[ "\${container}" = "testctr" ] || { echo "No such container: \${container}" >&2; exit 1; }
# env -i, because a container does not inherit the launcher's environment. In
# particular it must not see the stub jsh on the launcher's PATH, or every
# deployment would "discover" a jsh that is already there. TERM is what the
# daemon substitutes when nobody says otherwise, so anything -e carries has to
# come after it to win.
run_it() {
    env -i HOME="${CTR_HOME}" \\
        PATH="\${FAKE_DOCKER_PATH:-/usr/local/bin:/usr/bin:/bin}" \\
        TERM=xterm \${passed} "\$@"
}
# The real \`docker exec\` does not take its process down when the client goes
# away — that is what leaves a shell running in the container after a tab is
# closed. FAKE_DOCKER_DETACH reproduces it: start the session, return without
# it, and give it a moment to record its pid the way a real session has by the
# time anyone tears it down.
if [ -n "\${FAKE_DOCKER_DETACH:-}" ]; then
    run_it "\$@" > /dev/null 2>&1 < /dev/null &
    sleep 1
    exit 0
fi
run_it "\$@"
EOF
chmod +x "${STUB}/docker"

# Stands in for a real jsh: same --version contract, and when executed as the
# session it records the environment the launcher handed it.
make_jsh() {
    local path="$1" version="$2"
    cat > "${path}" <<EOF
#!/bin/sh
case "\$1" in
  --version) echo "jsh ${version}" ;;
  *)
    {
      echo "argv=\$*"
      echo "HOME=\${HOME:-}"
      echo "PWD=\$(pwd)"
      echo "JSH_REAL_HOME=\${JSH_REAL_HOME:-}"
      echo "XDG_STATE_HOME=\${XDG_STATE_HOME:-}"
      echo "TERM=\${TERM:-}"
      echo "COLORTERM=\${COLORTERM:-}"
      echo "SANDBOX=\${JSH_REMOTE_SANDBOX:-}"
      echo "SESSION_PID=\$\$"
    } > "${LOG}"
    # A real session lasts as long as the tab; the stub only needs to outlive
    # the launcher, which is what makes the teardown observable.
    if [ -f "${STUB}/linger" ]; then exec sleep 30; fi
    ;;
esac
EOF
    chmod +x "${path}"
}

# A local jsh on PATH, so the launcher defaults to deploying this same version.
make_jsh "${STUB}/jsh" "${VERSION}"

# --- fake release ------------------------------------------------------------

make_release() {
    local v="$1"
    local stage="${ROOT}/stage/jsh-${v}-${TARGET}"
    rm -rf "${ROOT}/stage"
    mkdir -p "${stage}" "${REL}/download/v${v}" "${REL}/latest/download"
    make_jsh "${stage}/jsh" "${v}"
    tar -C "${ROOT}/stage" -czf "${REL}/download/v${v}/jsh-${v}-${TARGET}.tar.gz" "jsh-${v}-${TARGET}"
    (cd "${REL}/download/v${v}" && sha256sum "jsh-${v}-${TARGET}.tar.gz" > "jsh-${v}-${TARGET}.tar.gz.sha256")
    printf '{"schema":1,"version":"%s"}\n' "${v}" > "${REL}/latest/download/manifest.json"
}
make_release "${VERSION}"

PATH_FOR_RUN="${STUB}:/usr/local/bin:/usr/bin:/bin"

# FAKE_DOCKER_PATH is the PATH the stub gives the "container", so a test can put
# a jsh inside it without also putting one on the launcher's PATH.
FAKE_DOCKER_PATH="/usr/local/bin:/usr/bin:/bin"

run() {
    rm -f "${LOG}"
    env -i HOME="${LOCAL_HOME}" PATH="${PATH_FOR_RUN}" \
        TERM="${FAKE_TERM-xterm-256color}" COLORTERM="${FAKE_COLORTERM-truecolor}" \
        XDG_CACHE_HOME="${LOCAL_HOME}/.cache" \
        JSH_INSTALL_BASE_URL="file://${REL}" \
        FAKE_DOCKER_PATH="${FAKE_DOCKER_PATH}" \
        sh "${REMOTE}" --docker testctr "$@" < /dev/null
}

session_field() { sed -n "s/^$1=//p" "${LOG}" 2> /dev/null | head -1; }
# Only the directories this test can actually cause a sandbox to appear in.
# Scanning /tmp and /var/tmp machine-wide would fail on a leftover sandbox from
# unrelated work, which says nothing about the launcher under test.
ctr_sandboxes() {
    find "${CTR_HOME}" "${CTR_HOME}/.cache" -maxdepth 1 -name 'jsh-remote.*' 2> /dev/null
}

# --- tests -------------------------------------------------------------------

echo "== dry run reports the plan and deploys nothing =="
out="$(run --dry-run 2>&1)"
rc=$?
indent "${out}"
assert "exit 0" [ ${rc} -eq 0 ]
assert "detects the platform" matches "${out}" "Linux/$(uname -m) -> ${TARGET}"
assert "defaults to persist" matches "${out}" 'mode: *persist'
assert "plans a push" matches "${out}" "push jsh ${VERSION}"
assert "names the cache path" matches "${out}" 'cache at .*/\.cache/jsh-remote/bin/[0-9a-f]{16}'
assert "nothing landed in the container home" [ -z "$(ls -A "${CTR_HOME}" 2> /dev/null)" ]

echo "== persist: deploy, run, cache, tear down =="
out="$(run --session tab7 2>&1)"
rc=$?
indent "${out}"
assert "exit 0" [ ${rc} -eq 0 ]
assert "the deployed jsh ran" [ -f "${LOG}" ]
assert "session id forwarded" [ "$(session_field argv)" = "--session tab7" ]
assert "HOME is the real remote home" [ "$(session_field HOME)" = "${CTR_HOME}" ]
assert "started in the remote home" [ "$(session_field PWD)" = "${CTR_HOME}" ]
cached_bin="$(find "${CTR_HOME}/.cache/jsh-remote/bin" -name jsh 2> /dev/null | head -1)"
assert "binary cached under the remote HOME" [ -x "${cached_bin}" ]
assert "cache directory is content addressed" matches "${cached_bin}" '/bin/[0-9a-f]{16}/jsh$'
assert "cache directory is private" [ "$(stat -c %a "$(dirname "${cached_bin}")")" = "700" ]
assert "no incoming file left behind" [ ! -e "${cached_bin}.incoming" ]
assert "sandbox torn down" [ -z "$(ctr_sandboxes)" ]

echo "== a session that outlives its client is taken down with its sandbox =="
# `docker exec` does not kill what it started when the client goes away, so
# without the teardown below a closed tab leaves a shell running in the
# container forever — and the next connection's sweep is right to leave it
# alone, because its pid is genuinely alive.
touch "${STUB}/linger"
out="$(FAKE_DOCKER_DETACH=1 run 2>&1)"
indent "${out}"
session_pid="$(session_field SESSION_PID)"
sandbox_path="$(session_field SANDBOX)"
assert "the detached session started" [ -n "${session_pid}" ]
assert "the session knows its own sandbox" matches "${sandbox_path}" 'jsh-remote\.'
# The teardown gives the shell a second to leave on its own before insisting.
for _ in 1 2 3 4 5 6; do
    kill -0 "${session_pid}" 2> /dev/null || break
    sleep 0.5
done
assert "the orphaned session was signalled" [ ! -e "/proc/${session_pid}" ]
assert "its sandbox is gone too" [ ! -d "${sandbox_path}" ]
rm -f "${STUB}/linger"

echo "== a live session belonging to someone else is never signalled =="
# The pid file is not proof of ownership: pids get reused. Only a process whose
# environment names this exact sandbox may be signalled.
sleep 300 &
bystander=$!
mkdir -p "${CTR_HOME}/.cache/jsh-remote.impostor"
printf '%s\n' "${bystander}" > "${CTR_HOME}/.cache/jsh-remote.impostor/pid"
out="$(run 2>&1)"
indent "${out}"
assert "the bystander survived the sweep" kill -0 "${bystander}"
{ kill -9 "${bystander}"; wait "${bystander}"; } 2> /dev/null
rm -rf "${CTR_HOME}/.cache/jsh-remote.impostor"

echo "== a static local jsh lends itself; releases are not needed at all =="
# The stub local jsh reads as lendable (ldd: not a dynamic executable), so
# pointing the release URL at nothing proves the deployment no longer depends
# on a release existing anywhere.
out="$(env -i HOME="${LOCAL_HOME}" PATH="${PATH_FOR_RUN}" \
    TERM=xterm-256color COLORTERM=truecolor XDG_CACHE_HOME="${LOCAL_HOME}/.cache-empty" \
    JSH_INSTALL_BASE_URL="file://${ROOT}/no-such-releases" \
    FAKE_DOCKER_PATH="${FAKE_DOCKER_PATH}" \
    sh "${REMOTE}" --docker testctr -v 2>&1 < /dev/null)"
rc=$?
indent "$(grep -E 'lending|starting' <<< "${out}")"
assert "exit 0" [ ${rc} -eq 0 ]
assert "announces the loan" matches "${out}" "lending it instead of fetching a release"
assert "the lent jsh ran" [ -f "${LOG}" ]
assert "its version came from its own banner" matches "${out}" "starting jsh ${VERSION}"

echo "== a dynamic local jsh cannot lend itself and uses the release =="
# Presence is probed with ldd, so a stub ldd that reports dynamic linkage is
# how a glibc-installed jsh looks to the launcher.
LDDSTUB="${ROOT}/ldd-dynamic"
mkdir -p "${LDDSTUB}"
printf '#!/bin/sh\necho "\tlinux-vdso.so.1 (0x0000)"\n' > "${LDDSTUB}/ldd"
chmod +x "${LDDSTUB}/ldd"
out="$(env -i HOME="${LOCAL_HOME}" PATH="${LDDSTUB}:${PATH_FOR_RUN}" \
    TERM=xterm-256color COLORTERM=truecolor XDG_CACHE_HOME="${LOCAL_HOME}/.cache" \
    JSH_INSTALL_BASE_URL="file://${REL}" \
    FAKE_DOCKER_PATH="${FAKE_DOCKER_PATH}" \
    sh "${REMOTE}" --docker testctr -v 2>&1 < /dev/null)"
rc=$?
assert "exit 0" [ ${rc} -eq 0 ]
assert "no loan is offered" lacks "${out}" "lending it"
assert "the release artifact was staged instead" matches "${out}" "(staging|reusing staged artifact)"
assert "the release jsh ran" [ -f "${LOG}" ]

echo "== the terminal the tab is drawn in crosses into the container =="
assert "TERM forwarded" [ "$(session_field TERM)" = "xterm-256color" ]
assert "COLORTERM forwarded" [ "$(session_field COLORTERM)" = "truecolor" ]
out="$(FAKE_TERM='x; rm -rf /' FAKE_COLORTERM='' run 2>&1)"
indent "${out}"
assert "a value that is not a terminal name is dropped" \
    [ "$(session_field TERM)" = "xterm" ]
assert "an unset variable is not forwarded as empty" [ -z "$(session_field COLORTERM)" ]

echo "== a different architecture never receives the local binary =="
ARM_BIN="${ROOT}/arm-bin"
mkdir -p "${ARM_BIN}"
cat > "${ARM_BIN}/uname" <<'EOF'
#!/bin/sh
case "$1" in
    -s) echo Linux ;;
    -m) echo aarch64 ;;
    *) exec /usr/bin/uname "$@" ;;
esac
EOF
chmod +x "${ARM_BIN}/uname"
out="$(FAKE_DOCKER_PATH="${ARM_BIN}:/usr/local/bin:/usr/bin:/bin" run -v 2>&1)"
rc=$?
indent "${out}"
assert "exit 0 through the fallback" [ ${rc} -eq 0 ]
assert "reports the ARM destination" matches "${out}" 'arch=aarch64'
assert "falls back when no ARM release exists" matches "${out}" 'falling back to shell integration'
assert "does not lend the local x86 binary" lacks "${out}" 'the local jsh is static; lending it'
assert "does not push the local x86 binary" lacks "${out}" 'pushing .*/stub/jsh'

echo "== persist again: cache hit, no transfer =="
before="$(stat -c %Y "${cached_bin}")"
out="$(run -v 2>&1)"
indent "${out}"
assert "reports a cache hit" matches "${out}" 'cache hit'
assert "did not push" lacks "${out}" 'pushing '
assert "cached binary untouched" [ "$(stat -c %Y "${cached_bin}")" = "${before}" ]
assert "ran again" [ -f "${LOG}" ]

echo "== incognito: sandboxed HOME, nothing kept =="
home_before="$(find "${CTR_HOME}" -maxdepth 1 | sort)"
out="$(run --incognito 2>&1)"
rc=$?
indent "${out}"
assert "exit 0" [ ${rc} -eq 0 ]
assert "HOME points into a sandbox" matches "$(session_field HOME)" 'jsh-remote\.[^/]+/home$'
assert "XDG_STATE_HOME follows HOME" matches "$(session_field XDG_STATE_HOME)" 'jsh-remote\.[^/]+/state$'
assert "real home is still reachable" [ "$(session_field JSH_REAL_HOME)" = "${CTR_HOME}" ]
assert "still starts in the real home" [ "$(session_field PWD)" = "${CTR_HOME}" ]
assert "sandbox removed afterwards" [ -z "$(ctr_sandboxes)" ]
assert "remote HOME unchanged" [ "$(find "${CTR_HOME}" -maxdepth 1 | sort)" = "${home_before}" ]

echo "== incognito pushes a local rc =="
printf 'alias ll="ls -l"\n' > "${LOCAL_HOME}/.jshrc"
out="$(run --incognito 2>&1)"
indent "${out}"
assert "starts from the pushed rc" matches "$(session_field argv)" '^--rcfile .*jsh-remote\.[^/]+/rc$'
rm -f "${LOCAL_HOME}/.jshrc"
out="$(run --incognito 2>&1)"
assert "falls back to the remote bashrc name" lacks "$(session_field argv)" 'rcfile'

printf 'export X=1\n' > "${CTR_HOME}/.bashrc"
out="$(run --incognito 2>&1)"
assert "names the destination bashrc when it exists" \
    [ "$(session_field argv)" = "--rcfile ${CTR_HOME}/.bashrc" ]
assert "--no-rc wins" [ "$(cd "${ROOT}" && run --incognito --no-rc > /dev/null 2>&1; session_field argv)" = "--norc" ]
rm -f "${CTR_HOME}/.bashrc"

echo "== an existing jsh is reused instead of deployed =="
mkdir -p "${CTR_HOME}/bin"
make_jsh "${CTR_HOME}/bin/jsh" "${VERSION}"
FAKE_DOCKER_PATH="${CTR_HOME}/bin:/usr/local/bin:/usr/bin:/bin"
out="$(run -v 2>&1)"
FAKE_DOCKER_PATH="/usr/local/bin:/usr/bin:/bin"
indent "${out}"
assert "reuses the installed binary" matches "${out}" "using the existing ${CTR_HOME}/bin/jsh"
assert "did not push" lacks "${out}" 'pushing '
rm -rf -- "${CTR_HOME:?}/bin"

echo "== a binary that is not jsh is refused =="
bad_rel="${ROOT}/bad"
mkdir -p "${bad_rel}"
printf '#!/bin/sh\necho nope\n' > "${bad_rel}/jsh"
chmod +x "${bad_rel}/jsh"
out="$(run --artifact "${bad_rel}/jsh" 2>&1)"
rc=$?
indent "${out}"
assert "exit nonzero" [ ${rc} -ne 0 ]
assert "explains the refusal" matches "${out}" 'does not identify itself as jsh'
bad_sha12="$(sha256sum "${bad_rel}/jsh" | cut -c1-16)"
assert "the refused binary was not cached" [ ! -e "${CTR_HOME}/.cache/jsh-remote/bin/${bad_sha12}/jsh" ]
assert "no half-transferred file left behind" \
    [ -z "$(find "${CTR_HOME}" -name '*.incoming' 2> /dev/null)" ]
assert "sandbox still torn down" [ -z "$(ctr_sandboxes)" ]

echo "== a mismatched version is refused =="
other="${ROOT}/other"
mkdir -p "${other}"
make_jsh "${other}/jsh" "9.9.9"
out="$(run --version "${VERSION}" --artifact "${other}/jsh" 2>&1)"
rc=$?
indent "${out}"
assert "exit nonzero" [ ${rc} -ne 0 ]
assert "names what it got" matches "${out}" 'reports 9\.9\.9'

echo "== when nothing can be deployed, shell integration is the middle tier =="
# Deployment needs a directory that can execute a file; integration needs one
# that can merely be written to, because `bash --rcfile` reads its argument and
# never runs it. That is a whole extra tier on a noexec destination, and the
# terminal keeps blocks, cwd tracking and exit codes even though jsh is absent.
printf 'echo DESTINATION_BASHRC_RAN\n' > "${CTR_HOME}/.bashrc"
out="$(run --no-deploy -v 2>&1)"
rc=$?
indent "${out}"
assert "exit 0" [ ${rc} -eq 0 ]
assert "announces the tier" matches "${out}" 'falling back to shell integration'
assert "starts the destination's own bash" matches "${out}" 'with shell integration'
assert "the destination's bashrc still runs" matches "${out}" 'DESTINATION_BASHRC_RAN'
assert "no jsh was pushed" lacks "${out}" 'pushing .*/jsh'
assert "sandbox torn down" [ -z "$(ctr_sandboxes)" ]
assert "no rc file survives" [ -z "$(find "${CTR_HOME}" -name 'integration.bash' 2> /dev/null)" ]
assert "the destination's bashrc is untouched" \
    [ "$(cat "${CTR_HOME}/.bashrc")" = "echo DESTINATION_BASHRC_RAN" ]

echo "== the other two fallbacks are still available =="
out="$(run --no-deploy --fallback bash 2>&1)"
indent "${out}"
assert "plain shell on request" matches "${out}" 'falling back to the destination.s own shell'
assert "no integration rc is pushed" lacks "${out}" 'shell integration'
out="$(run --no-deploy --fallback fail 2>&1)"
rc=$?
assert "exit nonzero with --fallback fail" [ ${rc} -ne 0 ]
assert "states the reason" matches "${out}" 'no-deploy'
out="$(run --no-deploy --fallback sideways 2>&1)"
assert "an unknown tier is refused" matches "${out}" 'expected integration, bash, or fail'
rm -f "${CTR_HOME}/.bashrc"

echo "== an abandoned sandbox is swept, a live one is not =="
# Sweeping happens in the directory the launcher actually chose, which in
# persist mode is ~/.cache on the destination.
EXECDIR="${CTR_HOME}/.cache"
sandbox_live="${EXECDIR}/jsh-remote.live00"
sandbox_dead="${EXECDIR}/jsh-remote.dead00"
mkdir -p "${sandbox_live}" "${sandbox_dead}"
echo $$ > "${sandbox_live}/pid"                  # this test process: alive
dead_pid="$(sh -c 'echo $$')"                    # exited before we read it back
echo "${dead_pid}" > "${sandbox_dead}/pid"
touch -d '2 hours ago' "${sandbox_live}" "${sandbox_dead}"
run > /dev/null 2>&1
assert "live sandbox survives" [ -d "${sandbox_live}" ]
assert "abandoned sandbox swept" [ ! -d "${sandbox_dead}" ]
rm -rf "${sandbox_live}"

echo "== a sandbox whose owner is gone is swept without waiting out the age guard =="
# SIGKILL is the only way a sandbox outlives its session, and for --incognito
# that directory is exactly what the mode promised would not survive. A recorded
# pid proves a real session owned it, so once that pid is gone there is nothing
# to wait for.
recent_dead="${EXECDIR}/jsh-remote.recent"
mkdir -p "${recent_dead}"
dead_pid="$(sh -c 'echo $$')"
echo "${dead_pid}" > "${recent_dead}/pid"     # brand new, but its owner is gone
run > /dev/null 2>&1
assert "swept immediately, not in an hour" [ ! -d "${recent_dead}" ]

echo "== a young sandbox is never swept =="
# No pid file: this is the one case the age guard is for — a sandbox created
# moments ago that has not reached the session script yet.
young="${EXECDIR}/jsh-remote.young0"
mkdir -p "${young}"
run > /dev/null 2>&1
assert "young sandbox with no recorded owner survives" [ -d "${young}" ]
rm -rf "${young}"

echo "== the launcher never edits shell startup files =="
printf 'original\n' > "${CTR_HOME}/.bashrc"
printf 'original\n' > "${CTR_HOME}/.profile"
run > /dev/null 2>&1
run --incognito > /dev/null 2>&1
assert ".bashrc untouched" [ "$(cat "${CTR_HOME}/.bashrc")" = "original" ]
assert ".profile untouched" [ "$(cat "${CTR_HOME}/.profile")" = "original" ]

echo "== ssh transport: quoting survives the login shell =="
# The stub reproduces the property that makes ssh quoting hard: ssh has no argv
# to the remote side. It joins the command words with spaces and hands the
# result to a shell, which parses it again. Anything jsh-remote.sh failed to
# quote falls apart exactly here.
SSH_HOME="${ROOT}/ssh home"   # a space, because that is the case that breaks
mkdir -p "${SSH_HOME}"
cat > "${STUB}/ssh" <<EOF
#!/bin/sh
dest=""
while [ \$# -gt 0 ]; do
    case "\$1" in
        -o) shift 2 ;;
        -t|-T|-q) shift ;;
        --) shift; dest="\$1"; shift; break ;;
        -*) shift ;;
        *) dest="\$1"; shift; break ;;
    esac
done
[ "\${dest}" = "testhost" ] || { echo "ssh: Could not resolve hostname \${dest}" >&2; exit 255; }
run() { exec env -i HOME="${SSH_HOME}" PATH=/usr/local/bin:/usr/bin:/bin TERM=xterm-256color "\$@"; }
[ \$# -gt 0 ] || run /bin/sh
cmd="\$1"; shift
for a in "\$@"; do cmd="\${cmd} \${a}"; done
run /bin/sh -c "\${cmd}"
EOF
chmod +x "${STUB}/ssh"

run_ssh() {
    rm -f "${LOG}"
    env -i HOME="${LOCAL_HOME}" PATH="${PATH_FOR_RUN}" TERM=xterm-256color \
        XDG_CACHE_HOME="${LOCAL_HOME}/.cache" XDG_RUNTIME_DIR="${ROOT}/run" \
        JSH_INSTALL_BASE_URL="file://${REL}" \
        sh "${REMOTE}" testhost "$@" < /dev/null
}

out="$(run_ssh --session 'tab 9' -v 2>&1)"
rc=$?
indent "${out}"
assert "exit 0" [ ${rc} -eq 0 ]
assert "the deployed jsh ran" [ -f "${LOG}" ]
assert "a session id containing a space stayed one argument" \
    [ "$(session_field argv)" = "--session tab 9" ]
assert "a home containing a space is usable" [ "$(session_field HOME)" = "${SSH_HOME}" ]
assert "cached under the spaced home" [ -n "$(find "${SSH_HOME}/.cache/jsh-remote" -name jsh 2> /dev/null)" ]
assert "sandbox torn down" [ -z "$(find "${SSH_HOME}" -maxdepth 2 -name 'jsh-remote.*' -type d 2> /dev/null)" ]

out="$(run_ssh --incognito 2>&1)"
indent "${out}"
assert "incognito over ssh sandboxes HOME" matches "$(session_field HOME)" 'jsh-remote\.[^/]+/home$'
assert "incognito leaves no sandbox" \
    [ -z "$(find "${ROOT}" -maxdepth 4 -name 'jsh-remote.*' -type d 2> /dev/null)" ]

out="$(env -i HOME="${LOCAL_HOME}" PATH="${PATH_FOR_RUN}" sh "${REMOTE}" nosuchhost 2>&1)"
rc=$?
assert "an unreachable host is reported" [ ${rc} -ne 0 ]
assert "names ssh" matches "${out}" 'cannot reach nosuchhost over ssh'

echo "== a hostile home directory is refused, not quoted around =="
EVIL_HOME="${ROOT}/ev'il"
mkdir -p "${EVIL_HOME}"
cat > "${STUB}/ssh" <<EOF
#!/bin/sh
while [ \$# -gt 0 ]; do case "\$1" in -o) shift 2 ;; --) shift; shift; break ;; -*) shift ;; *) shift; break ;; esac; done
[ \$# -gt 0 ] || exit 0
cmd="\$1"; shift
for a in "\$@"; do cmd="\${cmd} \${a}"; done
exec env -i HOME="${EVIL_HOME}" PATH=/usr/local/bin:/usr/bin:/bin /bin/sh -c "\${cmd}"
EOF
chmod +x "${STUB}/ssh"
out="$(run_ssh 2>&1)"
rc=$?
indent "${out}"
assert "exit nonzero" [ ${rc} -ne 0 ]
assert "refuses the path outright" matches "${out}" 'unusable (home )?directory reported by testhost'
assert "never ran anything there" [ ! -e "${LOG}" ]
rm -f "${STUB}/ssh"

echo "== unknown destinations and options fail cleanly =="
out="$(run --fallback sideways 2>&1)"
assert "rejects an unknown fallback" matches "${out}" 'unknown --fallback'
out="$(env -i HOME="${LOCAL_HOME}" PATH="${PATH_FOR_RUN}" sh "${REMOTE}" --docker nosuch 2>&1)"
rc=$?
assert "reports an unreachable container" [ ${rc} -ne 0 ]
assert "names the container" matches "${out}" 'cannot exec into container nosuch'

echo
printf 'passed %d, failed %d\n' "${pass}" "${fail}"
[ "${fail}" -eq 0 ]
