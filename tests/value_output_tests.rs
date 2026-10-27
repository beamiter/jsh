//! How in-process value pipelines write their final result.
use std::process::{Command, Stdio};

fn run(script: &str) -> (String, String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", script])
        .stdin(Stdio::null())
        .output()
        .expect("spawn jsh");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn a_scalar_result_ends_its_line() {
    // This used to print `15next`: the scalar had no line ending of its own.
    assert_eq!(run("range 1..5 | math sum; echo next").0, "15\nnext\n");
    assert_eq!(run("range 1..3 | count; echo next").0, "3\nnext\n");
    assert_eq!(run("range 1..3 | first; echo next").0, "1\nnext\n");
}

#[test]
fn typed_function_results_end_their_line() {
    assert_eq!(
        run("def add a:int b:int {|a,b| $a + $b}; add 3 4; echo next").0,
        "7\nnext\n"
    );
}

#[test]
fn records_and_lists_keep_a_single_line_ending() {
    let (out, _, code) = run(r#"echo '{"a":1}' | from-json | get a; echo next"#);
    assert_eq!((out.as_str(), code), ("1\nnext\n", 0));
    let (out, _, _) = run("range 1..2 | to-json; echo next");
    assert!(out.ends_with("]\nnext\n"), "{out:?}");
    let (out, _, _) = run(r#"range 1..2 | wrap n | first; echo next"#);
    assert!(out.ends_with("n: 1\nnext\n"), "{out:?}");
}

#[test]
fn command_substitution_still_strips_the_line_ending() {
    assert_eq!(
        run(r#"x=$(range 1..5 | math sum); echo "[$x]""#).0,
        "[15]\n"
    );
}
