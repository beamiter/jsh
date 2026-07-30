/// Phase 9 — closure if/else, format builtin, new str subs, and `do`.
use std::io::Write;
use std::process::{Command, Stdio};

fn jsh_bin() -> String {
    env!("CARGO_BIN_EXE_jsh").to_string()
}

fn run(script: &str, stdin: &str) -> (String, String, i32) {
    let mut child = Command::new(jsh_bin())
        .arg("-c")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

// ---------------------------------------------------------------------------
// 9a — if/else in closure bodies
// ---------------------------------------------------------------------------

fn squash_json(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

#[test]
fn closure_if_else_numeric() {
    let (out, err, code) = run(
        "range 1..5 | each {|x| if $x > 2 { $x * 10 } else { 0 }} | to-json",
        "",
    );
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(squash_json(&out), "[0,0,30,40,50]");
}

#[test]
fn closure_if_else_with_field() {
    let input = r#"[{"age":20},{"age":40}]"#;
    let (out, err, code) = run(
        "from-json | each {|r| if $r.age >= 30 { \"old\" } else { \"young\" }} | to-json",
        input,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(squash_json(&out), "[\"young\",\"old\"]");
}

#[test]
fn closure_else_if_chain() {
    let (out, err, code) = run(
        "range 0..4 | each {|x| if $x == 0 { \"zero\" } else if $x == 1 { \"one\" } else { \"many\" }} | to-json",
        "",
    );
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(
        squash_json(&out),
        "[\"zero\",\"one\",\"many\",\"many\",\"many\"]"
    );
}

// ---------------------------------------------------------------------------
// 9b — format builtin
// ---------------------------------------------------------------------------

#[test]
fn format_record_fields() {
    let input = r#"[{"name":"alice","age":30},{"name":"bob","age":25}]"#;
    let (out, err, code) = run("from-json | format \"{name} is {age}\"", input);
    assert_eq!(code, 0, "stderr: {}", err);
    let lines: Vec<&str> = out.trim().lines().collect();
    assert_eq!(lines, vec!["alice is 30", "bob is 25"]);
}

#[test]
fn format_scalar_input() {
    let (out, err, code) = run("range 1..3 | format \"n={}\"", "");
    assert_eq!(code, 0, "stderr: {}", err);
    let lines: Vec<&str> = out.trim().lines().collect();
    assert_eq!(lines, vec!["n=1", "n=2", "n=3"]);
}

// ---------------------------------------------------------------------------
// 9b — new str subcommands
// ---------------------------------------------------------------------------

#[test]
fn str_length_trims_newline() {
    let (out, _, code) = run("echo hello | str length", "");
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "5");
}

#[test]
fn str_starts_ends_with() {
    let (out1, _, _) = run("echo hello | str starts-with hel", "");
    assert_eq!(out1.trim(), "true");
    let (out2, _, _) = run("echo hello | str ends-with lo", "");
    assert_eq!(out2.trim(), "true");
    let (out3, _, _) = run("echo hello | str starts-with xyz", "");
    assert_eq!(out3.trim(), "false");
}

#[test]
fn str_index_of() {
    let (out1, _, _) = run("echo hello | str index-of llo", "");
    assert_eq!(out1.trim(), "2");
    let (out2, _, _) = run("echo hello | str index-of zzz", "");
    assert_eq!(out2.trim(), "-1");
}

#[test]
fn str_pad_left_right() {
    let (out1, _, _) = run("printf hi | str pad-left 5 x", "");
    assert_eq!(out1.trim(), "xxxhi");
    let (out2, _, _) = run("printf hi | str pad-right 5 x", "");
    assert_eq!(out2.trim(), "hixxx");
}

#[test]
fn str_reverse() {
    let (out, _, _) = run("echo hello | str reverse", "");
    assert_eq!(out.trim(), "olleh");
}

// ---------------------------------------------------------------------------
// 9c — do builtin
// ---------------------------------------------------------------------------

#[test]
fn do_no_args_no_input() {
    let (out, err, code) = run("do {|| 42 + 8}", "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "50");
}

#[test]
fn do_with_positional_args() {
    let (out, err, code) = run("do {|x| $x * 2} 21", "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "42");
}

#[test]
fn do_with_pipeline_input() {
    let (out, err, code) = run("echo hello | do {|s| $s + \"!\"}", "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "hello!");
}

#[test]
fn do_spreads_list_result() {
    let (out, err, code) = run("do {|| range 1..3} | to-json", "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(squash_json(&out), "[1,2,3]");
}

// ---------------------------------------------------------------------------
// Regression: bash for/do loop still parses despite the `do`-as-command
// peek-ahead exception in is_command_start.
// ---------------------------------------------------------------------------

#[test]
fn bash_for_loop_still_works() {
    let (out, err, code) = run("for i in 1 2 3; do echo $i; done", "");
    assert_eq!(code, 0, "stderr: {}", err);
    let lines: Vec<&str> = out.trim().lines().collect();
    assert_eq!(lines, vec!["1", "2", "3"]);
}
