use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run_with_input(command: &str, input: &[u8]) -> Output {
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
        .write_all(input)
        .expect("write stdin fixture");
    child.wait_with_output().expect("wait for jsh")
}

#[test]
fn data_builtin_reports_stdin_decode_errors() {
    let output = run_with_input("filter visible", b"visible\n\xff\n");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "visible\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("jsh: filter: stdin:"));
}

#[test]
fn stream_builtin_reports_stdin_decode_errors() {
    let output = run_with_input("upper", b"visible\n\xff\n");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "VISIBLE\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("jsh: upper: stdin:"));
}

#[test]
fn read_leaves_unconsumed_bytes_for_the_next_external_command() {
    let cases: &[(&str, &[u8], &[u8])] = &[
        ("read value", b"first\nsecond\n", b"<first>second\n"),
        ("read -n 2 value", b"abremaining\n", b"<ab>remaining\n"),
        ("read -N 2 value", b"abremaining\n", b"<ab>remaining\n"),
        ("read value", "前\n后\n".as_bytes(), "<前>后\n".as_bytes()),
        (
            "read -d 雪 value",
            "前雪后\n".as_bytes(),
            "<前>后\n".as_bytes(),
        ),
        (
            "read -N 3 value",
            "雪remaining\n".as_bytes(),
            "<雪>remaining\n".as_bytes(),
        ),
        ("read value", b"partial-at-eof", b"<partial-at-eof>"),
    ];
    for (read, input, expected) in cases {
        let command = format!(r#"{read}; printf '<%s>' "$value"; /usr/bin/cat"#);
        let output = run_with_input(&command, input);
        assert_eq!(output.status.code(), Some(0), "{read}: {:?}", output.stderr);
        assert_eq!(output.stdout, *expected, "{read}");
    }
}
