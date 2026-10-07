//! Exercise Enter through the real editor and REPL, without starting GUI apps.
#![cfg(target_os = "linux")]

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PROMPT_END: &str = "\x1b]133;B\x07";

struct Terminal {
    child: Child,
    reader: File,
    writer: File,
}

impl Terminal {
    fn start(home: &std::path::Path, terminal: &str) -> Self {
        let mut leader = 0;
        let mut follower = 0;
        // SAFETY: openpty initializes two owned fds; optional arguments are null.
        assert_eq!(
            unsafe {
                nix::libc::openpty(
                    &mut leader,
                    &mut follower,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        // SAFETY: these fds were returned by openpty and are owned here.
        let (leader, follower) =
            unsafe { (OwnedFd::from_raw_fd(leader), OwnedFd::from_raw_fd(follower)) };
        // SAFETY: leader remains live; nonblocking reads enforce the deadline.
        unsafe {
            nix::libc::fcntl(
                leader.as_raw_fd(),
                nix::libc::F_SETFL,
                nix::libc::O_NONBLOCK,
            )
        };
        let child = Command::new(env!("CARGO_BIN_EXE_jsh"))
            .arg("--norc")
            .current_dir(home)
            .env("HOME", home)
            .env_remove("JSH_SESSION_ID")
            .env_remove("JSH_REAL_HOME")
            .env("TERM", "xterm-256color")
            .env("TERM_PROGRAM", terminal)
            .env("NO_COLOR", "1")
            .env("JSH_EXECUTION_JOURNAL", "off")
            // The opener executes a fixture that logs argv. The configured
            // helper itself is still an ordinary trusted system executable.
            .env("JSH_HELPER_XDG_OPEN", "/bin/sh")
            .env("JSH_TEST_OPEN_LOG", home.join("opened"))
            .stdin(Stdio::from(follower.try_clone().unwrap()))
            .stdout(Stdio::from(follower.try_clone().unwrap()))
            .stderr(Stdio::from(follower))
            .spawn()
            .unwrap();
        let mut terminal = Self {
            child,
            writer: File::from(leader.try_clone().unwrap()),
            reader: File::from(leader),
        };
        terminal.read_until(PROMPT_END);
        terminal
    }

    fn read_until(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut output = Vec::new();
        let mut chunk = [0; 8192];
        while Instant::now() < deadline {
            match self.reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => output.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("PTY read failed: {error}"),
            }
            if String::from_utf8_lossy(&output).contains(needle) {
                return String::from_utf8(output).unwrap();
            }
        }
        panic!(
            "PTY did not reach {needle:?}: {}",
            String::from_utf8_lossy(&output)
        );
    }

    fn enter(&mut self, line: &str) -> String {
        write!(self.writer, "{line}\r").unwrap();
        self.read_until(PROMPT_END)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn enter_opens_files_in_all_four_terminal_environments() {
    for name in ["anvil", "forge", "ember", "frost"] {
        let home = tempfile::tempdir().unwrap();
        let document = home.path().join("中文报告  notes.txt");
        fs::write(
            &document,
            "printf '%s\\n' \"$0\" >> \"$JSH_TEST_OPEN_LOG\"\n",
        )
        .unwrap();
        let mut terminal = Terminal::start(home.path(), name);
        let output = terminal.enter("\"中文报告  notes.txt\"");
        assert!(output.contains("\x1b]133;D;0;"), "{name}: {output}");
        let output = terminal.enter("中文报告  notes.txt");
        assert!(output.contains("\x1b]133;D;0;"), "{name}: {output}");
        let log = home.path().join("opened");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let contents = fs::read_to_string(&log).unwrap_or_default();
            if contents.lines().count() == 2 {
                assert_eq!(contents, format!("{0}\n{0}\n", document.display()));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{name}: launcher did not log both paths"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn launching_an_application_keeps_the_prompt_usable() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("slow.txt"), "sleep 2\n").unwrap();
    let mut terminal = Terminal::start(home.path(), "frost");
    let start = Instant::now();
    terminal.enter("slow.txt");
    let output = terminal.enter("printf 'READY=%s\\n' yes");
    assert!(output.contains("READY=yes"), "{output}");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "the opener blocked the prompt"
    );
}

#[test]
fn noninteractive_file_commands_are_not_desktop_opens() {
    let home = tempfile::tempdir().unwrap();
    let document = home.path().join("report.txt");
    fs::write(&document, "echo opened > \"$JSH_TEST_OPEN_LOG\"\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", "./report.txt"])
        .current_dir(home.path())
        .env("JSH_HELPER_XDG_OPEN", "/bin/sh")
        .env("JSH_TEST_OPEN_LOG", home.path().join("opened"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(126));
    assert!(!home.path().join("opened").exists());
}
