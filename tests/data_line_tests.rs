use std::io::Write;
use std::process::{Command, Stdio};

fn run(script: &str, input: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

// A system `uniq` takes precedence over the line-based builtin (see
// src/shadowing.rs); `builtin uniq` keeps exercising the builtin itself.
#[test]
fn uniq_preserves_empty_runs_at_start_middle_and_end() {
    assert_eq!(run("builtin uniq", "\n\na\na\n\nb\n\n\n"), "\na\n\nb\n\n");
    assert_eq!(
        run("builtin uniq -c", "\n\na\na\n\nb\n\n\n"),
        "2 \n2 a\n1 \n1 b\n2 \n"
    );
}

#[test]
fn uniq_distinguishes_empty_input_from_one_empty_line() {
    assert_eq!(run("builtin uniq", ""), "");
    assert_eq!(run("builtin uniq -c", ""), "");
    assert_eq!(run("builtin uniq", "\n"), "\n");
    assert_eq!(run("builtin uniq -c", "\n"), "1 \n");
}

#[test]
fn map_substitutes_zero_based_fields_into_template() {
    assert_eq!(
        run("map '{1}:{0}:{1}'", "one two\nthree four\n"),
        "two:one:two\nfour:three:four\n"
    );
    assert_eq!(run("map 'fixed output'", "one two\n"), "fixed output\n");
}

#[test]
fn map_does_not_reinterpret_inserted_text_or_missing_fields() {
    assert_eq!(run("map '{0}:{1}:{2}'", "{1} two\n"), "{1}:two:{2}\n");
    assert_eq!(
        run("map '{0}:{999999999999999999999999}'", "one\n"),
        "one:{999999999999999999999999}\n"
    );
}
