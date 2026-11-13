# Bash 5.2 stable contract

This small mandatory Linux CI gate is separate from the historical host-dependent
1114-probe corpus. It does not measure language coverage and has no allowlist.
Bash must identify as 5.2.x or the gate fails (Ubuntu 24.04); its exact patch banner
is printed. Both shells receive the same clean environment and isolated working
directory. Raw stdout/stderr bytes and exit status are compared. Selected fixtures
also compare specified relative files' bytes and mode. This is not a recursive
filesystem, environment mutation, signal, job-control, or five-terminal contract.

Run `python3 scripts/bash_compat_gate.py --jsh target/debug/jsh` after building.
The harness's own failure-path tests run with
`python3 scripts/test_bash_compat_gate.py`.

Each trusted fixture starts a fresh session. Linux /proc session members are killed on timeout
and on normal shell exit, so ordinary background jobs in separate process groups cannot retain output
pipes indefinitely. Output uses temporary files with a sampled 1 MiB limit; this
is not a hard resource sandbox, and deliberately detached descendants are outside
the contract. Timeout/output exhaustion fails even if both shells exhaust alike.
Temporary work directories are deleted after collecting declared file outputs.
