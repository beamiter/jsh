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
/// (Bash's brace-bodied `for` loop; jsh parses `coproc`, the old probe, itself.)
const FORCE_BASH: &str = "for _ in 1; { :; }\n";

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
    startup_on_a_terminal_with(home, probe, jsh())
}

#[cfg(target_os = "linux")]
fn startup_on_a_terminal_with(home: &Path, probe: &str, mut command: Command) -> String {
    use std::io::Write;

    let mut leader = 0;
    let mut follower = 0;
    let size = nix::libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty writes two fds through the out-params and touches nothing
    // else; the remaining arguments are documented as optional.
    let opened = unsafe {
        nix::libc::openpty(
            &mut leader,
            &mut follower,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: both fds come from the successful openpty above and are owned here.
    let (leader, follower) =
        unsafe { (OwnedFd::from_raw_fd(leader), OwnedFd::from_raw_fd(follower)) };

    let mut child = command
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
    if let Some(marker) = seen
        .windows(PROBE_MARKER.len())
        .rposition(|bytes| bytes == PROBE_MARKER.as_bytes())
    {
        // Command output can arrive before the editor enables raw mode for
        // the next prompt. Typing exit before then can be flushed on entry.
        let mut after_probe = seen.split_off(marker + PROBE_MARKER.len());
        wait_for(&mut after_probe, "❯", 10);
        seen.extend_from_slice(&after_probe);
    }
    write!(writer, "exit\r").expect("write to pty");

    let stop = Instant::now() + Duration::from_secs(10);
    while Instant::now() < stop && !matches!(child.try_wait(), Ok(Some(_))) {
        // Drain the next prompt while waiting for exit; otherwise a repaint
        // can fill the PTY and prevent the shell from reading the exit line.
        if let Ok(n) = reader.read(&mut chunk) {
            seen.extend_from_slice(&chunk[..n]);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    String::from_utf8_lossy(&seen).into_owned()
}

#[cfg(target_os = "linux")]
struct VimShellFixture {
    directory: tempfile::TempDir,
    home: std::path::PathBuf,
    shim: std::path::PathBuf,
    original: std::path::PathBuf,
    frontend: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
impl VimShellFixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let home = directory.path().join("home");
        let shim = directory.path().join("native vim shim");
        let original = directory.path().join("original tools");
        for path in [&home, &shim, &original] {
            fs::create_dir(path).expect("fixture directory");
        }
        let frontend = directory.path().join("terminal frontend");
        for (path, script) in [
            (
                shim.join("vim"),
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$VIM_TEST_ARGS\"\nexit 23\n",
            ),
            (original.join("vim"), "#!/bin/sh\nexit 17\n"),
            (frontend.clone(), "#!/bin/sh\nexit 0\n"),
        ] {
            fs::write(&path, script).expect("write executable fixture");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("chmod fixture");
        }
        Self {
            directory,
            home,
            shim,
            original,
            frontend,
        }
    }

    fn reset_path_rc(&self, extra: &str) {
        // The real failure: startup replaces PATH rather than extending it.
        write_guarded_rc(
            &self.home,
            ".bashrc",
            &format!(
                "export PATH='{}':/usr/bin:/bin\n{extra}",
                self.original.display()
            ),
        );
    }

    fn command(&self) -> Command {
        let mut command = jsh();
        command
            .env("JTERM_VIM9_FRONTEND", &self.frontend)
            .env("JTERM_VIM9_SHIM_DIR", &self.shim)
            .env_remove("JTERM_VIM9_AUTO_GUI")
            .env("VIM_TEST_ARGS", self.directory.path().join("vim-args"))
            .env(
                "PATH",
                format!(
                    "{}:{}:/usr/bin:/bin",
                    self.shim.display(),
                    self.original.display()
                ),
            );
        command
    }

    fn run(&self, probe: &str, command: Command) -> String {
        startup_on_a_terminal_with(&self.home, probe, command)
    }
}

#[cfg(target_os = "linux")]
#[test]
fn terminal_vim_entry_survives_bashrc_replacing_path() {
    let fixture = VimShellFixture::new();
    fixture.reset_path_rc("");
    let seen = fixture.run(
        "vim +42 'first source.txt' second.txt; echo \"PRO\"\"BE=[$?]\"",
        fixture.command(),
    );
    assert!(seen.contains("PROBE=[23]"), "terminal session:\n{seen}");
    assert_eq!(
        fs::read_to_string(fixture.directory.path().join("vim-args")).expect("shim received argv"),
        "+42\nfirst source.txt\nsecond.txt\n",
    );
}

#[cfg(target_os = "linux")]
#[test]
fn terminal_vim_entry_survives_conda_hook_replacing_path() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = VimShellFixture::new();
    let conda = fixture.directory.path().join("conda");
    fs::write(
        &conda,
        format!(
            "#!/bin/sh\nif [ \"$1 $2\" = 'shell.posix hook' ]; then\n\
             printf '%s\\n' \"export PATH='{}':/usr/bin:/bin\"\nfi\n",
            fixture.original.display()
        ),
    )
    .expect("write conda hook");
    fs::set_permissions(&conda, fs::Permissions::from_mode(0o700)).expect("chmod conda");
    fixture.reset_path_rc(&format!("export CONDA_EXE='{}'\n", conda.display()));
    let seen = fixture.run("vim; echo \"PRO\"\"BE=[$?]\"", fixture.command());
    assert!(seen.contains("PROBE=[23]"), "terminal session:\n{seen}");
}

#[cfg(target_os = "linux")]
#[test]
fn terminal_vim_entry_uses_current_frontend_after_session_restore() {
    let first = VimShellFixture::new();
    first.reset_path_rc("");
    let mut command = first.command();
    command.args(["--session", "native-vim-restore"]);
    let seen = first.run("vim; echo \"PRO\"\"BE=[$?]\"", command);
    assert!(seen.contains("PROBE=[23]"), "first terminal:\n{seen}");
    assert!(first
        .home
        .join(".jsh/sessions/native-vim-restore.json")
        .exists());

    let second = VimShellFixture::new();
    fs::write(second.shim.join("vim"), "#!/bin/sh\nexit 24\n").expect("write second shim");
    let mut command = second.command();
    command.args(["--session", "native-vim-restore"]);
    let seen = startup_on_a_terminal_with(
        &first.home,
        "vim; echo \"PRO\"\"BE=[$?][$JTERM_VIM9_FRONTEND][$PATH]\"",
        command,
    );
    let expected_path = format!(
        "{}:{}:/usr/bin:/bin",
        second.shim.display(),
        first.original.display()
    );
    assert!(
        seen.contains(&format!(
            "PROBE=[24][{}][{expected_path}]",
            second.frontend.display()
        )),
        "restored terminal:\n{seen}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn terminal_vim_entry_respects_environment_and_startup_opt_out() {
    let fixture = VimShellFixture::new();
    fixture.reset_path_rc("");
    for disabled in ["0", "false", "off"] {
        let mut command = fixture.command();
        command.env("JTERM_VIM9_AUTO_GUI", disabled);
        let seen = fixture.run("vim; echo \"PRO\"\"BE=[$?]\"", command);
        assert!(seen.contains("PROBE=[17]"), "{disabled}:\n{seen}");
    }
    fixture.reset_path_rc("export JTERM_VIM9_AUTO_GUI=0\n");
    let seen = fixture.run("vim; echo \"PRO\"\"BE=[$?]\"", fixture.command());
    assert!(seen.contains("PROBE=[17]"), "startup opt-out:\n{seen}");
}

#[cfg(target_os = "linux")]
#[test]
fn terminal_vim_entry_preserves_user_aliases_and_functions() {
    let fixture = VimShellFixture::new();
    for (rc, status) in [("alias vim='false'\n", 1), ("vim() { return 32; }\n", 32)] {
        // Use the native startup parser for both aliases and functions.
        fs::write(
            fixture.home.join(".bashrc"),
            format!(
                "export PATH='{}':/usr/bin:/bin\n{rc}",
                fixture.original.display()
            ),
        )
        .expect("write rc");
        let mut command = fixture.command();
        command.args(["--rcfile", fixture.home.join(".bashrc").to_str().unwrap()]);
        let seen = fixture.run("vim; echo \"PRO\"\"BE=[$?]\"", command);
        assert!(seen.contains(&format!("PROBE=[{status}]")), "{rc}:\n{seen}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn terminal_vim_entry_does_not_rewrite_noninteractive_path() {
    let fixture = VimShellFixture::new();
    let output = fixture
        .command()
        .args([
            "-c",
            &format!("PATH='{}':/usr/bin:/bin; vim", fixture.original.display()),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run noninteractive jsh");
    assert_eq!(output.status.code(), Some(17));
}

#[cfg(target_os = "linux")]
#[test]
fn terminal_vim_entry_requires_a_live_terminal_frontend() {
    let fixture = VimShellFixture::new();
    fixture.reset_path_rc("");
    for frontend in [None, Some("/nonexistent/jterm-frontend")] {
        let mut command = fixture.command();
        command.env_remove("JTERM_VIM9_FRONTEND");
        if let Some(frontend) = frontend {
            command.env("JTERM_VIM9_FRONTEND", frontend);
        }
        let seen = fixture.run("vim; echo \"PRO\"\"BE=[$?]\"", command);
        assert!(seen.contains("PROBE=[17]"), "{frontend:?}:\n{seen}");
    }
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
