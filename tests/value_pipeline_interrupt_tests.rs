//! In-process value pipelines must honor signals and leave stdin alone.
//!
//! A value pipeline runs inside the shell process, where the signal handlers
//! only latch a flag. Before these fixes `range 0..100000000 | count` ignored
//! Ctrl-C and SIGTERM until it finished, and a pipeline starting with `range`
//! read the shell's stdin to EOF first: it hung on an idle open stdin and
//! swallowed the input of an enclosing `while read` loop.
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn spawn(script: &str, stdin: Stdio) -> Child {
    Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", script])
        .stdin(stdin)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jsh")
}

/// Signal the child until it exits (a single early signal could land before
/// the pipeline starts), and return its exit code.
fn signal_until_exit(mut child: Child, signal: nix::sys::signal::Signal) -> Option<i32> {
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    let deadline = Instant::now() + Duration::from_secs(20);
    // Let the pipeline get going so the signal interrupts real work.
    std::thread::sleep(Duration::from_millis(150));
    loop {
        if let Some(status) = child.try_wait().expect("poll jsh") {
            return status.code();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("jsh ignored {signal:?} for 20 seconds");
        }
        let _ = nix::sys::signal::kill(pid, signal);
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn assert_interrupted(script: &str) {
    use nix::sys::signal::Signal;
    for (signal, status) in [(Signal::SIGTERM, 143), (Signal::SIGINT, 130)] {
        let child = spawn(script, Stdio::null());
        assert_eq!(
            signal_until_exit(child, signal),
            Some(status),
            "{script} under {signal:?}"
        );
    }
}

#[test]
fn a_lazy_source_into_an_aggregate_stops_on_a_signal() {
    assert_interrupted("range 0..100000000000 | count");
}

#[test]
fn closure_stages_stop_on_a_signal() {
    assert_interrupted("range 0..100000000000 | each {|n| $n + 1} | count");
    assert_interrupted("range 0..100000000000 | where {|n| $n < 0} | to-json");
    assert_interrupted("range 0..100000000000 | reduce -i 0 {|acc, it| $acc + $it}");
}

#[test]
fn shell_body_closures_stop_on_a_signal() {
    assert_interrupted("range 0..100000000000 | each {|n| echo $n} | count");
}

#[test]
fn a_single_value_command_stops_on_a_signal() {
    assert_interrupted("range 0..100000000000");
}

fn run_with_open_stdin(script: &str) -> (String, i32) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn jsh");
    // Hold stdin open without writing: a pipeline that reads it would hang.
    let stdin = child.stdin.take();
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().expect("poll jsh").is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("{script} waited on an idle stdin");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(stdin);
    let output = child.wait_with_output().expect("collect jsh");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

#[test]
fn sources_do_not_wait_for_stdin() {
    assert_eq!(
        run_with_open_stdin("range 1..5 | math sum"),
        ("15\n".into(), 0)
    );
    let (out, code) = run_with_open_stdin("range 1..2 | to-json");
    assert_eq!(code, 0);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out).unwrap(),
        serde_json::json!([1, 2])
    );
    assert_eq!(run_with_open_stdin("range 1..3 | count; echo done").1, 0);
    assert_eq!(
        run_with_open_stdin("range 1..2 >/dev/null; echo done").0,
        "done\n"
    );
}

#[test]
fn sources_inside_a_read_loop_leave_its_input_alone() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args([
            "--norc",
            "-c",
            r#"while read -r l; do echo "$l $(range 1..3 | math sum)"; range 1..2 >/dev/null; done"#,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn jsh");
    child.stdin.take().unwrap().write_all(b"a\nb\nc\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "a 6\nb 6\nc 6\n");
}

#[test]
fn piped_input_still_reaches_a_consuming_first_stage() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", "from-json | math sum"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn jsh");
    child.stdin.take().unwrap().write_all(b"[1, 2, 3]").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "6\n");
}
