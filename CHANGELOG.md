# Changelog

## Unreleased

- `${...}` now picks its operator from the character right after the parameter
  name instead of searching the whole body for each operator in turn. A `-`,
  `=` or `+` inside a pattern used to hijack the expansion, so `${v##*a-b*}`
  was read as `${v##*a}` defaulting to `b*` — which made
  `/etc/profile.d/xdg_dirs_desktop_session.sh` prepend a directory to
  `XDG_CONFIG_DIRS` that was already there. Added `${var:?message}`,
  `${var?message}` and the `^ ^^ , ,,` case-conversion operators, taught
  `${var/pat/rep}` to expand variables in both halves and to accept an escaped
  `\/`, and extended `${var:offset:length}` to negative lengths.

- `source /opt/ros/humble/setup.bash` (and ament setup scripts generally) now
  works. Four Bash gaps were in the way: double quotes did not nest inside a
  backtick substitution, so `"`dirname "$f"`"` cut the command short;
  `${BASH_SOURCE[0]}` was unset, so a sourced script could not find its own
  directory; `:` was not a builtin; and `IFS` started unset, so the common
  `old=$IFS; ...; IFS=$old` idiom emptied it and killed word splitting for the
  rest of the session.
- Publish prebuilt Linux binaries (glibc and static musl, x86_64 and aarch64)
  from tagged releases, together with checksums and a `manifest.json` served at
  a stable "latest" URL.
- Added `scripts/install-rsh.sh`, the one-command install and update path. It
  verifies checksums, checks that the new binary identifies itself as rsh,
  swaps it in atomically, keeps the previous binary for rollback, and reports
  when `PATH` resolves `rsh` to the BSD remote shell instead. `--check --json`
  reports installed and latest versions for tooling, backed by a shared cache
  at `~/.cache/rsh/update-check.json`.

## 0.2.0

- Added a documented CLI contract with help, version, syntax-check, stdin,
  startup-file, and session options.
- Corrected Bash-style `$0`/positional arguments, exit propagation, `shift N`,
  `errexit`, and rightmost-nonzero `pipefail` behavior.
- Unified top-level execution across command strings, scripts, stdin, and the
  interactive editor.
- Rejects unterminated quotes, substitutions, and here-documents consistently,
  and propagates INT/HUP/TERM to foreground jobs with conventional statuses.
- Hardened history and session persistence with private permissions, atomic
  writes, multiline-safe history, legacy migration, and secret filtering.
- Made AI assistance explicitly opt-in and removed environment-value leakage
  from completion descriptions.
- Removed the duplicate binary module graph so the executable uses the library
  implementation directly.
- Added end-user documentation and package metadata.

## 0.1.0

- Initial experimental release with Bash-compatible execution, structured
  pipelines, an interactive editor, completion, workflows, sessions, and
  optional AI integration.
