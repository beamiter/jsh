# Engineering handoff

Updated: 2026-08-01

This baseline hardens Agent state and cancellation, AI URL/credential handling,
session persistence, execution I/O, parser/completion limits, terminal text, and
automatic helper resolution. It exact-pins the hardened jagent baseline.

## Remaining boundaries

### Complete transport cancellation and response-envelope accounting

Cancelling an AI request releases the foreground immediately, but a blocking ureq
worker cannot be interrupted in-place. At most one worker remains until its transport
timeout and a second request is refused during that interval. Move to a transport
with cancellable socket I/O or an owned worker process while preserving the current
single-flight and no-command-after-cancel guarantees.

Response bodies and model text are bounded and redirects are disabled. Add explicit
response-header count and cumulative-byte limits as well.

### Replace snapshot preflight with schema-aware decoding

Session load has a 4 MiB file cap, allocation-free JSON depth/value/string preflight,
and post-decode logical validation. The final construction still uses ordinary
Serde. Replace it with bounded map/sequence visitors that enforce collection counts,
per-field limits, and cumulative text while allocating, without weakening legacy
snapshot migration.

### Generalize trusted automatic helpers without reopening PATH execution

Automatic bash, git, and notify-send integrations intentionally use fixed,
permission-checked system paths. Nix and custom system layouts can therefore lose
optional integration features. Support explicitly configured absolute helper paths
only after verifying executability, ownership, and every containing directory; do
not restore mutable PATH lookup for background helpers.

### Make the release installer supply chain fail closed

- Require a valid published checksum; do not install when it is unavailable.
- Bound archive downloads and strictly validate base URL, version, and target.
- Reject absolute paths, `..`, hard links, and symlinks; extract only the expected
  archive member.
- Make cache temporary files unpredictable, private, symlink-safe, and atomic.
- Bound `jsh --version` probing even when a descendant inherits its output pipe.
- Add a signed release manifest; a same-origin SHA-256 only detects corruption.

After changing the canonical installer, synchronize and test every vendored jterm
copy.

## Release checks

```text
cargo fmt --all -- --check
cargo test --locked --all-targets --all-features --no-fail-fast
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo doc --locked --all-features --no-deps
```
