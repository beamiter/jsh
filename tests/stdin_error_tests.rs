use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run_with_invalid_utf8(command: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["-c", command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jsh");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(b"visible\n\xff\n")
        .expect("write invalid UTF-8 fixture");
    child.wait_with_output().expect("wait for jsh")
}

#[test]
fn data_builtin_reports_stdin_decode_errors() {
    let output = run_with_invalid_utf8("filter visible");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "visible\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("jsh: filter: stdin:"));
}

#[test]
fn stream_builtin_reports_stdin_decode_errors() {
    let output = run_with_invalid_utf8("upper");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "VISIBLE\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("jsh: upper: stdin:"));
}
