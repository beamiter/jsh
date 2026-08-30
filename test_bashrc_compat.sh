#!/usr/bin/env bash
# Focused regression gate for Bash startup-file compatibility.

set -Eeuo pipefail

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
cd -- "$REPO_ROOT"

# Startup files are intentionally loaded only by interactive jsh sessions.
# The Rust tests exercise the importer directly and through a pseudo-terminal,
# avoiding the old false-green smoke test that invoked `jsh -c` (which skips
# startup files by contract) and merely printed FAIL while returning success.
cargo test --locked --lib config::tests::
cargo test --locked --test bash_compat_tests
cargo test --locked --test rc_interactive_guard_tests
