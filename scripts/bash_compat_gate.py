#!/usr/bin/env python3
"""Small deterministic Bash 5.2 contract, not a language compatibility score.

Run only trusted repository fixtures. A new session contains ordinary descendants,
not deliberately escaping setsid children. Output caps are sampled disk limits,
not a security sandbox. No stdout pipe reader can outlive a timed-out child.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time


def kill_session(session):
    # jsh gives jobs their own process groups, so killpg(shell.pid) is insufficient.
    # Only trusted fixtures are supported: deliberate setsid escape is out of scope.
    deadline = time.monotonic() + 1
    while True:
        live = False
        for entry in Path('/proc').iterdir():
            if not entry.name.isdigit():
                continue
            try:
                stat = (entry / 'stat').read_text().rsplit(')', 1)[1].split()
                if int(stat[3]) != session or stat[0] == 'Z':
                    continue
                live = True
                identity = stat[19]  # starttime
                current = (entry / 'stat').read_text().rsplit(')', 1)[1].split()
                if current[19] == identity and int(current[3]) == session:
                    os.kill(int(entry.name), signal.SIGKILL)
            except (FileNotFoundError, ProcessLookupError):
                pass
        if not live:
            return
        if time.monotonic() >= deadline:
            raise RuntimeError('session cleanup deadline')
        time.sleep(.005)



def run(program, leading, script, files=(), timeout=5, output_limit=1024 * 1024):
    with tempfile.TemporaryDirectory() as tmp, tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        root = Path(tmp)
        env = {"PATH": "/usr/bin:/bin", "HOME": tmp, "LC_ALL": "C", "TERM": "dumb"}
        child = subprocess.Popen([program, *leading, script], cwd=tmp, env=env,
                                 stdin=subprocess.DEVNULL, stdout=out, stderr=err,
                                 start_new_session=True)
        deadline = time.monotonic() + timeout
        failure = None
        try:
            while child.poll() is None:
                if time.monotonic() >= deadline:
                    failure = "timeout"
                    break
                if os.fstat(out.fileno()).st_size + os.fstat(err.fileno()).st_size > output_limit:
                    failure = "output limit"
                    break
                time.sleep(0.005)
        finally:
            # Jobs may have a different process group but retain this session.
            try:
                kill_session(child.pid)
            finally:
                child.kill() if child.poll() is None else None
                child.wait(timeout=2)
        if os.fstat(out.fileno()).st_size + os.fstat(err.fileno()).st_size > output_limit:
            failure = failure or "output limit"
        if failure:
            raise RuntimeError(failure)
        out.seek(0)
        err.seek(0)
        snapshot = {}
        for name in files:
            path = root / name
            if Path(name).is_absolute() or ".." in Path(name).parts:
                raise ValueError("fixture path must be relative")
            if path.is_symlink():
                snapshot[name] = {"symlink": os.readlink(path)}
            elif path.exists():
                if not path.is_file() or path.stat().st_size > output_limit:
                    raise RuntimeError("invalid or oversized fixture output")
                snapshot[name] = {"bytes": path.read_bytes().hex(), "mode": path.stat().st_mode & 0o777}
            else:
                snapshot[name] = None
        return {"stdout": out.read().hex(), "stderr": err.read().hex(),
                "status": child.returncode, "files": snapshot}


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--jsh", required=True)
    p.add_argument("--bash", default="/bin/bash")
    p.add_argument("--fixtures", default=str(Path(__file__).resolve().parents[1] / "tests/data/bash_gate/cases.json"))
    args = p.parse_args()
    bash = str(Path(args.bash).resolve())
    jsh = str(Path(args.jsh).resolve())
    version = subprocess.check_output([bash, "--noprofile", "--norc", "-c", 'printf "%s" "$BASH_VERSION"'], text=True)
    if not version.startswith("5.2."):
        raise SystemExit(f"Bash 5.2 required, got {version}")
    cases = json.loads(Path(args.fixtures).read_text())
    assert cases and len({c['name'] for c in cases}) == len(cases)
    failures = []
    for case in cases:
        try:
            expected = run(bash, ["--noprofile", "--norc", "-c"], case["script"], case.get("files", []))
            actual = run(jsh, ["--norc", "-c"], case["script"], case.get("files", []))
            if expected != actual:
                failures.append({"case": case["name"], "bash": expected, "jsh": actual})
        except (RuntimeError, subprocess.SubprocessError) as e:
            failures.append({"case": case["name"], "error": str(e)})
    print(json.dumps({"bash_version": version, "total": len(cases), "failures": failures}, indent=2))
    return bool(failures)


if __name__ == "__main__":
    raise SystemExit(main())
