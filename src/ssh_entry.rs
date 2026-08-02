//! Arriving on an ssh host as jsh, from a plain typed `ssh host`.
//!
//! The container path in [`crate::container`] cannot help here — there is no
//! mount to add over ssh — but the push tier has existed all along:
//! `scripts/jsh-remote.sh` probes the destination, pushes a verified static
//! jsh, runs it, and cleans up after itself. What was missing is the way in.
//! The launcher was reachable from a terminal's host picker and from typing
//! its path, never from the `ssh box` a person actually types.
//!
//! An interactive `ssh destination` with no remote command is now routed
//! through that launcher, with this running binary as the artifact. The
//! destination keeps its own login shell — nothing edits `.bashrc` or
//! `/etc/passwd` — and jsh's files land where the launcher's persist mode has
//! always put them: dot-files in your remote `$HOME`, the binary cached under
//! `~/.cache` so the next connection skips the transfer.
//!
//! The same honesty about rewriting applies as for containers, and everything
//! fails closed:
//!
//!   * only an interactive session: any remote command, and any flag that
//!     reshapes the session (`-N`, `-L`, `-W`, `-T`, …) or that this module
//!     does not recognise, leaves the command exactly as typed;
//!   * only when the running jsh is static (or a staged musl artifact
//!     exists) — without a binary to push there is nothing to offer, and the
//!     launcher's own fallback would burn seconds discovering that;
//!   * `command ssh …` bypasses this entirely, and `JSH_SSH_SHELL=off` turns
//!     it off for good.
//!
//! The launcher travels inside this binary (`include_str!`) and is published
//! to the cache on first use, so an installed jsh needs no scripts directory.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::environment::ShellState;

/// Vendored at build time from this repository's own `scripts/`; the published
/// copies are refreshed whenever their bytes disagree with the binary's.
const LAUNCHER: &str = include_str!("../scripts/jsh-remote.sh");
/// The launcher stages release artifacts through its sibling installer, so the
/// two are published together; with an explicit artifact it goes unused, but a
/// future jsh without a static self will want the release path to work.
const INSTALLER: &str = include_str!("../scripts/install-jsh.sh");

/// Short flags that take a value, attached (`-p2222`) or not (`-p 2222`).
/// `-L`/`-R`/`-D`/`-W` are deliberately absent: forwarding means the command
/// is infrastructure, not someone logging in to work.
const VALUE_FLAGS: &[char] = &['p', 'l', 'i', 'o', 'J', 'F', 'B', 'b', 'c', 'e', 'm', 'E'];

/// Short flags that stand alone and do not change the shape of the session.
const BOOL_FLAGS: &[char] = &[
    '4', '6', 'A', 'a', 'C', 'K', 'k', 'X', 'x', 'Y', 'y', 'v', 'q', 't',
];

/// Rewrite an interactive `ssh destination` to deploy and run jsh there.
///
/// Returns `None` — leaving the command exactly as typed — for anything this
/// cannot do confidently.
pub(crate) fn upgrade_entry(argv: &[String], state: &ShellState) -> Option<Vec<String>> {
    if !enabled(state) || !state.interactive {
        return None;
    }
    if Path::new(argv.first()?).file_name()?.to_str()? != "ssh" {
        return None;
    }
    let (destination, flags) = plan(&argv[1..])?;
    let binary = crate::container::static_jsh()?;
    let launcher = published_launcher()?;

    let mut rewritten = vec![
        "/bin/sh".to_string(),
        launcher.display().to_string(),
        "--persist".to_string(),
        "--artifact".to_string(),
        binary.display().to_string(),
        destination.clone(),
    ];
    if !flags.is_empty() {
        rewritten.push("--".to_string());
        rewritten.extend(flags);
    }
    eprintln!("jsh: bringing jsh to {destination} for this session (`command ssh` connects plain)");
    Some(rewritten)
}

fn enabled(state: &ShellState) -> bool {
    let setting = state
        .get_var("JSH_SSH_SHELL")
        .map(str::to_string)
        .or_else(|| std::env::var("JSH_SSH_SHELL").ok());
    !matches!(
        setting.as_deref().map(str::trim),
        Some("off" | "0" | "no" | "false")
    )
}

/// Split `ssh`'s arguments into the destination and the pass-through flags,
/// or nothing when this is not a plain interactive session.
fn plan(args: &[String]) -> Option<(String, Vec<String>)> {
    let mut flags = Vec::new();
    let mut index = 0;
    while let Some(token) = args.get(index) {
        if !token.starts_with('-') {
            break;
        }
        let width = flag_width(token)?;
        if width == 2 {
            let value = args.get(index + 1)?;
            // The launcher rejoins pass-through arguments with spaces before
            // handing them to ssh, so a value containing one would arrive as
            // two. Not worth guessing about.
            if value.chars().any(char::is_whitespace) {
                return None;
            }
        }
        flags.extend(args[index..index + width].iter().cloned());
        index += width;
    }
    let destination = args.get(index)?;
    if args.len() > index + 1 {
        // A remote command: `ssh host ls` runs ls, and must keep doing so.
        return None;
    }
    // `ssh://user@host:port/` URIs and hostnames that look like options are
    // both possible and both rare; the launcher's grammar takes `[user@]host`.
    if destination.starts_with('-')
        || destination.contains("://")
        || destination.chars().any(|c| c.is_whitespace() || c == '\'')
    {
        return None;
    }
    Some((destination.clone(), flags))
}

/// How many argv slots a token occupies, or `None` when it is not a flag this
/// module recognises — which stops the analysis, because an unknown flag may
/// or may not swallow the token after it.
fn flag_width(token: &str) -> Option<usize> {
    let letters: Vec<char> = token.strip_prefix('-')?.chars().collect();
    let (last, leading) = letters.split_last()?;
    // Everything before the last letter must be a standalone boolean
    // (`-4A`, `-vv`); the last decides whether a value follows, and an
    // attached value (`-p2222`, `-oBatchMode=no`) closes the token itself.
    for (position, letter) in leading.iter().enumerate() {
        if BOOL_FLAGS.contains(letter) {
            continue;
        }
        if VALUE_FLAGS.contains(letter) && position == 0 && letters.len() > 1 {
            // The rest of the token is this flag's attached value. Only in
            // first position: `-Cp2222` would be legal ssh, but reading it
            // right is not worth the ambiguity with a mistyped flag.
            return Some(1);
        }
        return None;
    }
    if BOOL_FLAGS.contains(last) {
        Some(1)
    } else if VALUE_FLAGS.contains(last) {
        Some(2)
    } else {
        None
    }
}

/// The launcher and its installer, on disk where `/bin/sh` can read them.
///
/// Published under the cache with private permissions, refreshed whenever the
/// bytes differ from what this binary carries, and written via temp + rename
/// so a concurrent session never reads half a script.
fn published_launcher() -> Option<PathBuf> {
    let dir = dirs::cache_dir()?.join("jsh");
    std::fs::create_dir_all(&dir).ok()?;
    let launcher = publish(&dir, "jsh-remote.sh", LAUNCHER)?;
    publish(&dir, "install-jsh.sh", INSTALLER)?;
    Some(launcher)
}

fn publish(dir: &Path, name: &str, source: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join(name);
    if crate::io_guard::read_regular_file_prefix(&path, source.len() + 1)
        .is_ok_and(|bytes| bytes == source.as_bytes())
    {
        return Some(path);
    }
    let staging = dir.join(format!("{name}.{}", std::process::id()));
    let mut file = std::fs::File::create(&staging).ok()?;
    file.write_all(source.as_bytes()).ok()?;
    file.set_permissions(std::fs::Permissions::from_mode(0o700))
        .ok()?;
    drop(file);
    match std::fs::rename(&staging, &path) {
        Ok(()) => Some(path),
        Err(_) => {
            let _ = std::fs::remove_file(&staging);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    fn planned(line: &str) -> Option<(String, Vec<String>)> {
        plan(&args(line))
    }

    #[test]
    fn a_bare_destination_is_an_interactive_session() {
        let (destination, flags) = planned("build-box").expect("plan");
        assert_eq!(destination, "build-box");
        assert!(flags.is_empty());

        let (destination, _) = planned("yj@10.0.0.7").expect("plan");
        assert_eq!(destination, "yj@10.0.0.7");
    }

    #[test]
    fn session_flags_travel_through_to_ssh() {
        let (destination, flags) =
            planned("-p 2222 -i /k/id -o StrictHostKeyChecking=no dev@host").expect("plan");
        assert_eq!(destination, "dev@host");
        assert_eq!(
            flags,
            [
                "-p",
                "2222",
                "-i",
                "/k/id",
                "-o",
                "StrictHostKeyChecking=no"
            ]
        );

        // Attached values and clustered booleans are still one flag each.
        let (_, flags) = planned("-p2222 -4C host").expect("plan");
        assert_eq!(flags, ["-p2222", "-4C"]);
    }

    #[test]
    fn anything_that_is_not_a_login_is_left_alone() {
        for line in [
            // A remote command must keep running that command.
            "host ls -la",
            "host true",
            // Forwarding and no-session shapes are infrastructure.
            "-L 8080:localhost:80 host",
            "-N host",
            "-T host",
            "-W remote:22 host",
            "-f host",
            // Unknown flags could swallow the destination.
            "--frobnicate host",
            // A value with whitespace would be re-split by the launcher.
            "-o ProxyCommand=ssh jump -W %h:%p host",
            // Not the launcher's destination grammar.
            "ssh://user@host:22",
            "-p",
        ] {
            assert!(
                planned(line).is_none(),
                "should not have rewritten: ssh {line}"
            );
        }
    }

    #[test]
    fn the_published_launcher_matches_the_scripts_in_this_repository() {
        // The embedded copies are the ones a released binary will publish;
        // they must be the files this checkout tests everywhere else.
        assert_eq!(
            LAUNCHER,
            std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/scripts/jsh-remote.sh"
            ))
            .expect("read launcher")
        );
        assert!(INSTALLER.contains("unknown-linux-musl"));
    }
}
