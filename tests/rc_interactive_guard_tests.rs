//! A startup file only tells you what it is configured to do if it believes a
//! person is listening.
//!
//! Every distribution's `~/.bashrc` opens with the same four lines:
//!
//! ```sh
//! case $- in
//!     *i*) ;;
//!       *) return;;
//! esac
//! ```
//!
//! and everything the user configured — PATH entries, aliases, the `conda
//! init` block — lives below them. jsh cannot read most such files itself, so
//! it hands them to a helper bash and imports what comes back. A helper bash
//! that is not interactive returns at line four, and the import is empty: not
//! an error, not a warning, just a shell that has forgotten a conda
//! installation which is plainly there. The symptom is reported from the far
//! end, when `conda activate` says to run `conda init` first.
//!
//! These tests drive the real binary. The pty test is the one that would have
//! caught the second half of the bug: an interactive helper wants the terminal,
//! and asking for one it is not entitled to stops it dead — which only happens
//! when jsh actually has a terminal to want.

use std::fs;
use std::io::Read;
use std::os::unix::io::{FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Rejected by jsh's own parser, so `source` falls through to the bash bridge.
const FORCE_BASH: &str = "coproc guard_probe { true; }\n";

const INTERACTIVE_GUARD: &str = "case $- in\n    *i*) ;;\n      *) return;;\nesac\n";

/// What the shell prints once it has run the probe line — never what the
/// terminal echoes while that line is being typed.
const PROBE_MARKER: &str = "PROBE=[";

fn jsh() -> Command {
    Command::new(env!("CARGO_BIN_EXE_jsh"))
}

fn write_guarded_rc(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("{FORCE_BASH}{INTERACTIVE_GUARD}{body}")).expect("write rc");
    path
}

#[test]
fn sourcing_a_guarded_file_runs_the_whole_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rc = write_guarded_rc(dir.path(), "guarded.sh", "export PAST_GUARD=yes\n");

    let output = jsh()
        .args([
            "--norc",
            "-c",
            &format!("source {}; echo \"PAST_GUARD=$PAST_GUARD\"", rc.display()),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run jsh");

    assert!(
        String::from_utf8_lossy(&output.stdout).contains("PAST_GUARD=yes"),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// The helper is interactive now, and an interactive bash without a terminal
/// says so — twice, on stderr. That is the helper's business, not the user's.
#[test]
fn sourcing_a_guarded_file_reports_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rc = write_guarded_rc(dir.path(), "quiet.sh", "export QUIET_GUARD=yes\n");

    let output = jsh()
        .args(["--norc", "-c", &format!("source {}", rc.display())])
        .stdin(Stdio::null())
        .output()
        .expect("run jsh");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("job control") && !stderr.contains("terminal process group"),
        "the helper's own job-control notices reached the user: {stderr}"
    );
}

/// A real error from the sourced file must still come through — the filter
/// above is for two known lines, not for silence.
#[test]
fn a_failing_guarded_file_still_reports_its_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rc = write_guarded_rc(dir.path(), "broken.sh", "no_such_command_hjkl\n");

    let output = jsh()
        .args(["--norc", "-c", &format!("source {}", rc.display())])
        .stdin(Stdio::null())
        .output()
        .expect("run jsh");

    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no_such_command_hjkl"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Start jsh on a pty with `$HOME` pointing at a fixture, and read until the
/// fixture's marker appears.
///
/// The terminal is the point. Without one, an interactive helper bash finds no
/// `/dev/tty`, shrugs, and runs the file; with one, it tries to take the
/// foreground process group, is not in it, and is stopped by `SIGTTOU` — so the
/// startup import times out and the shell comes up with none of the user's
/// configuration. Only a pty tells those two apart.
#[cfg(target_os = "linux")]
fn startup_on_a_terminal(home: &Path, probe: &str) -> String {
    use std::io::Write;

    let mut leader = 0;
    let mut follower = 0;
    // SAFETY: openpty writes two fds through the out-params and touches nothing
    // else; the remaining arguments are documented as optional.
    let opened = unsafe {
        nix::libc::openpty(
            &mut leader,
            &mut follower,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: both fds come from the successful openpty above and are owned here.
    let (leader, follower) =
        unsafe { (OwnedFd::from_raw_fd(leader), OwnedFd::from_raw_fd(follower)) };

    let mut child = jsh()
        .env("HOME", home)
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env_remove("CONDA_EXE")
        .env_remove("CONDA_PREFIX")
        .stdin(Stdio::from(follower.try_clone().expect("clone follower")))
        .stdout(Stdio::from(follower.try_clone().expect("clone follower")))
        .stderr(Stdio::from(follower))
        .spawn()
        .expect("spawn jsh on a pty");

    let mut writer = std::fs::File::from(leader.try_clone().expect("clone leader"));
    // Non-blocking, so the deadline below is a deadline. A shell wedged on the
    // terminal produces no output at all, and a blocking read would wait for it
    // for as long as the test harness allows.
    let raw = std::os::unix::io::AsRawFd::as_raw_fd(&leader);
    // SAFETY: `raw` is the live leader fd owned by this function.
    unsafe { nix::libc::fcntl(raw, nix::libc::F_SETFL, nix::libc::O_NONBLOCK) };
    let mut reader = std::fs::File::from(leader);

    let mut seen = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut wait_for = |seen: &mut Vec<u8>, needle: &str, secs: u64| {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if String::from_utf8_lossy(seen).contains(needle) {
                return;
            }
            match reader.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => seen.extend_from_slice(&chunk[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                // The follower side is gone: the shell has exited.
                Err(_) => return,
            }
        }
    };

    // Wait for the first prompt rather than typing ahead. A line editor takes
    // a burst of bytes for a paste and keeps the carriage returns as text, and
    // more to the point a shell that has not finished its startup import has
    // not loaded the hook the line under test depends on. If the import wedges
    // on the terminal no prompt ever arrives, which is the failure this exists
    // to catch — so it is bounded, not waited on forever.
    wait_for(&mut seen, "❯", 60);
    write!(writer, "{probe}\r").expect("write to pty");
    wait_for(&mut seen, PROBE_MARKER, 60);
    write!(writer, "exit\r").expect("write to pty");

    let stop = Instant::now() + Duration::from_secs(10);
    while Instant::now() < stop && !matches!(child.try_wait(), Ok(Some(_))) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    String::from_utf8_lossy(&seen).into_owned()
}

/// The whole reported failure, on a terminal: a stock guarded `~/.bashrc`, a
/// conda init block inside it, and `conda activate` typed at the prompt.
#[cfg(target_os = "linux")]
#[test]
fn conda_activate_works_in_a_shell_started_on_a_terminal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("home");

    // Enough of conda to be the thing the shell hook is for: `activate` can
    // only change this shell from inside it, so a `conda` that is not a
    // function refuses, exactly as the real one does.
    let conda = dir.path().join("conda");
    fs::write(
        &conda,
        r#"#!/bin/sh
if [ "$1 $2" = "shell.posix hook" ]; then
    printf 'conda() {\n  if [ "$1" = activate ]; then\n    export CONDA_PREFIX=/envs/"$2"\n  else\n    "%s" "$@"\n  fi\n}\n' "$0"
    exit 0
fi
echo "CondaError: Run 'conda init' before 'conda activate'" >&2
exit 1
"#,
    )
    .expect("write fake conda");
    fs::set_permissions(
        &conda,
        <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .expect("chmod fake conda");

    fs::write(
        home.join(".bashrc"),
        format!(
            "{INTERACTIVE_GUARD}\n\
             # >>> conda initialize >>>\n\
             export CONDA_EXE='{}'\n\
             # <<< conda initialize <<<\n",
            conda.display()
        ),
    )
    .expect("write bashrc");

    // The terminal echoes every keystroke, so the line has to be written in a
    // form that does not itself contain what the answer will look like.
    let seen = startup_on_a_terminal(
        &home,
        "conda activate demo; echo \"PRO\"\"BE=[${CONDA_PREFIX}]\"",
    );

    assert!(
        seen.contains("PROBE=[/envs/demo]"),
        "`conda activate` did not reach this shell's own environment.\n\
         Terminal session:\n{seen}"
    );
    assert!(
        !seen.contains("CondaError"),
        "conda was still the binary rather than the hook's function.\n\
         Terminal session:\n{seen}"
    );
}
