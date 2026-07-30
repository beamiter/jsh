use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

fn jsh(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jsh");
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for jsh")
}

fn run_c(command: &str) -> Output {
    jsh(&["-c", command], "")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[cfg(unix)]
fn wait_promptly(child: &mut Child) -> ExitStatus {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let deadline = Instant::now() + Duration::from_millis(750);
    loop {
        if let Some(status) = child.try_wait().expect("poll jsh") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = kill(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
            let _ = child.wait();
            panic!("jsh did not stop promptly");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn errexit_stops_the_current_top_level_list() {
    let output = run_c("set -e; false; echo unreachable");
    assert_eq!(output.status.code(), Some(1));
    assert!(!stdout(&output).contains("unreachable"));
}

#[test]
fn errexit_honors_and_or_and_negation_exemptions() {
    let output = run_c("set -e; false && echo skipped; ! true; echo reached");
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "reached");

    let output = run_c("set -e; true && false; echo unreachable");
    assert_eq!(output.status.code(), Some(1));
    assert!(!stdout(&output).contains("unreachable"));
}

#[test]
fn err_trap_obeys_the_same_conditional_exemptions() {
    let output = run_c("trap 'echo ERR' ERR; false && echo skipped; echo after");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output).trim(), "after");

    let output = run_c("trap 'echo ERR' ERR; false; echo after");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout(&output).lines().collect::<Vec<_>>(),
        ["ERR", "after"]
    );
}

#[test]
fn pipefail_uses_the_rightmost_nonzero_status() {
    let output = run_c("set -o pipefail; sh -c 'exit 7' | sh -c 'exit 3' | true");
    assert_eq!(output.status.code(), Some(3), "stderr: {}", stderr(&output));
}

#[test]
fn command_arg0_and_positional_parameters_are_distinct() {
    let output = jsh(
        &[
            "-c",
            "printf '%s|%s|%s' \"$0\" \"$1\" \"$2\"",
            "named",
            "a",
            "b",
        ],
        "",
    );
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "named|a|b");
}

#[test]
fn source_arguments_start_at_one_and_preserve_arg0() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("source_args.jsh");
    std::fs::write(&path, "printf '%s|%s|%s' \"$0\" \"$1\" \"$2\"").expect("write source file");
    let command = format!("source {} a b", path.display());
    let output = jsh(&["-c", &command, "outer"], "");
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "outer|a|b");
}

#[test]
fn script_path_is_arg0_and_script_arguments_start_at_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("argv.jsh");
    std::fs::write(&path, "printf '%s|%s' \"$0\" \"$1\"").expect("write script");
    let output = jsh(&[path.to_str().expect("utf8 path"), "value"], "");
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), format!("{}|value", path.display()));
}

#[test]
fn shift_accepts_a_count_and_rejects_out_of_range_without_mutation() {
    let output = run_c("set -- a b c; shift 2; printf '%s:%s' \"$#\" \"$1\"");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), "1:c");

    let output = run_c("set -- a; shift 2; printf '%s:%s' \"$?\" \"$1\"");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), "1:a");
    assert!(stderr(&output).contains("shift count out of range"));
}

#[test]
fn exit_without_argument_uses_last_status_and_stops_execution() {
    let output = run_c("false; exit; echo unreachable");
    assert_eq!(output.status.code(), Some(1));
    assert!(!stdout(&output).contains("unreachable"));
}

#[test]
fn failglob_reports_an_error_and_returns_nonzero() {
    let output = run_c("shopt -s failglob; echo /definitely-no-jsh-match-*; echo SHOULD_NOT_RUN");
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("no match: /definitely-no-jsh-match-*"));
    assert!(!stdout(&output).contains("definitely-no-jsh-match"));
    assert!(!stdout(&output).contains("SHOULD_NOT_RUN"));
}

#[test]
fn status_is_updated_between_and_or_pipelines() {
    let output = run_c("false || echo status:$?");
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "status:1");
}

#[test]
fn conditional_context_suppresses_errexit_through_compounds() {
    let output = run_c(
        "set -e; \
         f() { false; echo function-survived; }; \
         f && echo function-ok; \
         { false; echo brace-survived; } && echo brace-ok; \
         (false; echo subshell-survived) && echo subshell-ok; \
         ! f; echo after-negation",
    );
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output).lines().collect::<Vec<_>>(),
        [
            "function-survived",
            "function-ok",
            "brace-survived",
            "brace-ok",
            "subshell-survived",
            "subshell-ok",
            "function-survived",
            "after-negation",
        ]
    );

    let output = run_c("set -e; f() { false; echo SHOULD_NOT_RUN; }; f; echo ALSO_NOT");
    assert_eq!(output.status.code(), Some(1));
    assert!(!stdout(&output).contains("SHOULD_NOT"));
    assert!(!stdout(&output).contains("ALSO_NOT"));
}

#[test]
fn invalid_top_level_control_flow_diagnoses_and_continues() {
    let output = run_c(
        "break; printf 'break:%s\\n' \"$?\"; \
         continue; printf 'continue:%s\\n' \"$?\"; \
         return; printf 'return:%s\\n' \"$?\"; echo after",
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout(&output).lines().collect::<Vec<_>>(),
        ["break:1", "continue:1", "return:2", "after"]
    );
    let errors = stderr(&output);
    assert!(errors.contains("break: only meaningful in a loop"));
    assert!(errors.contains("continue: only meaningful in a loop"));
    assert!(errors.contains("return: can only return"));
}

#[test]
fn return_uses_current_status_and_validates_arguments() {
    let output = run_c("f() { false; return; echo SHOULD_NOT_RUN; }; f");
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(!stdout(&output).contains("SHOULD_NOT_RUN"));

    let output = run_c("f() { return nope; echo SHOULD_NOT_RUN; }; f");
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("numeric argument required"));
    assert!(!stdout(&output).contains("SHOULD_NOT_RUN"));

    let output = run_c("f() { return 7 8; echo SHOULD_NOT_RUN; }; f");
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("too many arguments"));
    assert!(!stdout(&output).contains("SHOULD_NOT_RUN"));
}

#[test]
fn noninteractive_exit_argument_errors_terminate_with_bash_statuses() {
    let output = run_c("exit 7 8; echo SHOULD_NOT_RUN");
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("exit: too many arguments"));
    assert!(!stdout(&output).contains("SHOULD_NOT_RUN"));

    let output = run_c("exit nope extra; echo SHOULD_NOT_RUN");
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("exit: nope: numeric argument required"));
    assert!(!stdout(&output).contains("SHOULD_NOT_RUN"));
}

#[test]
fn interactive_exit_with_too_many_arguments_does_not_request_exit() {
    let mut state = jsh::environment::ShellState::new(true);
    jsh::builtins::reset_exit_request();
    let code = jsh::builtins::run_builtin("exit", &["7".to_string(), "8".to_string()], &mut state);
    assert_eq!(code, 1);
    assert!(!jsh::builtins::EXIT_REQUESTED.load(std::sync::atomic::Ordering::SeqCst));
    jsh::builtins::reset_exit_request();
}

#[cfg(target_os = "linux")]
#[test]
fn ctrl_c_interrupts_noninteractive_command_under_a_pty() {
    let command = format!("{} -c 'sleep 1; echo JSH_DONE'", env!("CARGO_BIN_EXE_jsh"));
    let mut child = Command::new("script")
        .args(["-qfec", &command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn script PTY helper");
    std::thread::sleep(Duration::from_millis(150));
    child
        .stdin
        .as_mut()
        .expect("script stdin")
        .write_all(b"\x03")
        .expect("send Ctrl-C");
    let output = child.wait_with_output().expect("wait for PTY command");
    assert_eq!(
        output.status.code(),
        Some(130),
        "stderr: {}",
        stderr(&output)
    );
    assert!(!stdout(&output).contains("JSH_DONE"));
}

#[cfg(unix)]
#[test]
fn direct_hup_and_term_stop_noninteractive_program_and_child() {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    for (signal, expected) in [(Signal::SIGHUP, 129), (Signal::SIGTERM, 143)] {
        let child = Command::new(env!("CARGO_BIN_EXE_jsh"))
            .args(["-c", "sleep 2; echo JSH_DONE"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn jsh");
        std::thread::sleep(Duration::from_millis(150));
        kill(Pid::from_raw(child.id() as i32), signal).expect("signal jsh");
        let output = child.wait_with_output().expect("wait for signaled jsh");
        assert_eq!(
            output.status.code(),
            Some(expected),
            "signal {signal:?}, stderr: {}",
            stderr(&output)
        );
        assert!(!stdout(&output).contains("JSH_DONE"));
    }
}

#[cfg(unix)]
#[test]
fn hup_and_term_interrupt_blocked_noninteractive_stdin() {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let cases = [
        (vec!["-s"], Signal::SIGHUP, 129),
        (Vec::new(), Signal::SIGTERM, 143),
    ];
    for (args, signal, expected) in cases {
        let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn stdin-reading jsh");
        let mut input = child.stdin.take().expect("piped stdin");
        input
            .write_all(b"echo SHOULD_NOT_RUN\n")
            .expect("write partial program");

        std::thread::sleep(Duration::from_millis(100));
        let pid = Pid::from_raw(child.id() as i32);
        kill(pid, signal).expect("signal blocked jsh");

        let deadline = Instant::now() + Duration::from_millis(750);
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll jsh") {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = kill(pid, Signal::SIGKILL);
                drop(input);
                let _ = child.wait();
                panic!("jsh did not stop promptly for {signal:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        drop(input);

        let mut output = String::new();
        child
            .stdout
            .take()
            .expect("piped stdout")
            .read_to_string(&mut output)
            .expect("read stdout");
        let mut errors = String::new();
        child
            .stderr
            .take()
            .expect("piped stderr")
            .read_to_string(&mut errors)
            .expect("read stderr");

        assert_eq!(
            status.code(),
            Some(expected),
            "signal {signal:?}, stderr: {errors}"
        );
        assert!(!output.contains("SHOULD_NOT_RUN"));
    }
}

#[cfg(unix)]
#[test]
fn term_interrupts_blocking_read_builtin() {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["-c", "read value; echo SHOULD_NOT_RUN"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jsh read builtin");
    let input = child.stdin.take().expect("piped stdin");
    std::thread::sleep(Duration::from_millis(100));
    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).expect("signal blocked read");
    let status = wait_promptly(&mut child);
    drop(input);

    let mut output = String::new();
    child
        .stdout
        .take()
        .expect("piped stdout")
        .read_to_string(&mut output)
        .expect("read stdout");
    assert_eq!(status.code(), Some(143));
    assert!(!output.contains("SHOULD_NOT_RUN"));
}

#[cfg(unix)]
#[test]
fn hup_interrupts_script_blocked_opening_fifo() {
    use nix::sys::signal::{kill, Signal};
    use nix::sys::stat::Mode;
    use nix::unistd::{mkfifo, Pid};

    let dir = tempfile::tempdir().expect("tempdir");
    let fifo = dir.path().join("blocked-script.jsh");
    mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).expect("create fifo");
    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .arg(&fifo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jsh fifo script");
    std::thread::sleep(Duration::from_millis(100));
    kill(Pid::from_raw(child.id() as i32), Signal::SIGHUP).expect("signal fifo reader");
    let status = wait_promptly(&mut child);
    assert_eq!(status.code(), Some(129));
}

#[cfg(unix)]
#[test]
fn signal_during_exit_trap_overrides_final_status() {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["-c", "trap 'sleep 2' EXIT"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jsh EXIT trap");
    std::thread::sleep(Duration::from_millis(200));
    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).expect("signal EXIT trap");
    let status = wait_promptly(&mut child);
    assert_eq!(status.code(), Some(143));
}

#[cfg(unix)]
#[test]
fn signal_stops_remaining_err_trap_commands() {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["-c", "trap 'sleep 2; echo SHOULD_NOT_RUN' ERR; false"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jsh ERR trap");
    std::thread::sleep(Duration::from_millis(200));
    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).expect("signal ERR trap");
    let status = wait_promptly(&mut child);
    let mut output = String::new();
    child
        .stdout
        .take()
        .expect("piped stdout")
        .read_to_string(&mut output)
        .expect("read stdout");
    assert_eq!(status.code(), Some(143));
    assert!(!output.contains("SHOULD_NOT_RUN"));
}

#[cfg(target_os = "linux")]
#[test]
fn idle_interactive_term_returns_signal_status() {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("jsh.pid");
    let rc_file = dir.path().join("idle.jsh");
    std::fs::write(
        &rc_file,
        format!("sh -c 'echo $PPID > {}'\n", pid_file.display()),
    )
    .expect("write rc file");
    let command = format!(
        "{} --rcfile {}",
        env!("CARGO_BIN_EXE_jsh"),
        rc_file.display()
    );
    let mut script = Command::new("script")
        .args(["-qfec", &command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn interactive jsh");
    let input = script.stdin.take().expect("script stdin");
    let deadline = Instant::now() + Duration::from_secs(2);
    let shell_pid = loop {
        if let Ok(pid) = std::fs::read_to_string(&pid_file) {
            break pid.trim().parse::<i32>().expect("numeric jsh pid");
        }
        if Instant::now() >= deadline {
            drop(input);
            let _ = script.kill();
            let _ = script.wait();
            panic!("interactive jsh did not reach its prompt");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    kill(Pid::from_raw(shell_pid), Signal::SIGTERM).expect("signal interactive jsh");
    let status = wait_promptly(&mut script);
    drop(input);
    assert_eq!(status.code(), Some(143));
}
