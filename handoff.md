# Engineering handoff

Updated: 2026-08-01

This baseline hardens Agent state and cancellation, AI URL/credential handling,
session persistence, execution I/O, parser/completion limits, terminal text, and
automatic helper resolution. It exact-pins the hardened jagent baseline. The
release installer now fails closed, and AI response headers are bounded.

## Completed since the previous handoff

- `scripts/install-jsh.sh` requires a published, format-checked SHA-256 and
  refuses to install unverified bytes; bounds every download by size and keeps
  redirects on HTTPS; validates the version, target, and base-URL grammars
  before they reach a URL or a path, including the version read from the release
  manifest; lists an archive and refuses links, special files, traversal,
  absolute paths, and extra members before extracting only the expected binary;
  makes the update-check cache, the staged binary, and the rollback restore use
  unpredictable, private, atomically replaced names; and bounds the `--version`
  probe with a deadline plus a file instead of a pipe, so a descendant that
  inherits the probe's stdout cannot hang the installer.
  `scripts/test-install-jsh.sh` covers the absent and malformed checksum, the
  symlink, extra-member and traversal archives, the rejected grammars, and the
  cache's permissions and symlink safety.
- `post_json` enforces explicit response-header count and cumulative-byte limits
  before reading a body. The body cap already existed; headers are parsed and
  retained before it applies.

## Remaining boundaries

### Complete transport cancellation

Cancelling an AI request releases the foreground immediately, but a blocking ureq
worker cannot be interrupted in-place. At most one worker remains until its
transport timeout and a second request is refused during that interval. Move to a
transport with cancellable socket I/O or an owned worker process while preserving
the current single-flight and no-command-after-cancel guarantees.

### Replace snapshot preflight with schema-aware decoding

Session load has a 4 MiB file cap, allocation-free JSON depth/value/string
preflight, and post-decode logical validation. The final construction still uses
ordinary Serde. Replace it with bounded map/sequence visitors that enforce
collection counts, per-field limits, and cumulative text while allocating,
without weakening legacy snapshot migration. jterm1's `src/session.rs` now has a
worked example of this shape for a comparable schema.

### Generalize trusted automatic helpers without reopening PATH execution

Automatic bash, git, and notify-send integrations intentionally use fixed,
permission-checked system paths. Nix and custom system layouts can therefore lose
optional integration features. Support explicitly configured absolute helper paths
only after verifying executability, ownership, and every containing directory; do
not restore mutable PATH lookup for background helpers.

### Add a signed release manifest

The installer's mandatory SHA-256 is same-origin: it proves the bytes match what
the release published, not who published them. A detached signature over the
manifest, verified against a key pinned in the installer, is the missing half.
That needs a release-side signing decision, so it is deliberately not
approximated here.

After changing the canonical installer, synchronize and test every vendored jterm
copy. `jterm_core/scripts/install-jsh.sh` and `jterm4/scripts/install-jsh.sh` are
in sync with this revision.

## Release checks

```text
cargo fmt --all -- --check
cargo test --locked --all-targets --all-features --no-fail-fast
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo doc --locked --all-features --no-deps
./scripts/test-install-jsh.sh
```
