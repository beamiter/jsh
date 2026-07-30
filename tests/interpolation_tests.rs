/// Phase 5c — `$"..."` interpolation + `$name.field`/`$name[N]` path access.
///
/// Path-into-typed-Value coverage requires `let` bindings (Phase 5b); here we
/// only verify (a) interpolation rendering and (b) bash backward-compat:
/// `$name.txt` keeps working as "var value + literal `.txt`" when the name
/// has no typed Value in `let_vars`.
use std::process::{Command, Stdio};

fn jsh_bin() -> String {
    env!("CARGO_BIN_EXE_jsh").to_string()
}

fn run(script: &str) -> (String, String, i32) {
    let child = Command::new(jsh_bin())
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jsh");
    let out = child.wait_with_output().expect("wait");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn interp_pure_literal() {
    let (out, err, code) = run(r#"echo $"hello world""#);
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "hello world");
}

#[test]
fn interp_with_env_var() {
    let (out, err, code) = run(r#"name=alice; echo $"hi ($name)!""#);
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "hi alice!");
}

#[test]
fn interp_with_arithmetic_expr() {
    let (out, err, code) = run(r#"echo $"sum: ($((1+2)))""#);
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "sum: 3");
}

#[test]
fn interp_multi_parts() {
    let (out, err, code) = run(r#"a=foo; b=bar; echo $"[($a)|($b)|end]""#);
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "[foo|bar|end]");
}

#[test]
fn interp_escape_paren() {
    // \( should be literal "(" — no expression invoked.
    let (out, err, code) = run(r#"echo $"open \(close)""#);
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "open (close)");
}

#[test]
fn path_fallback_preserves_bash_dot_literal() {
    // No typed value for `name` → `$name.txt` must still produce `hello.txt`.
    let (out, err, code) = run(r#"name=hello; echo $name.txt"#);
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "hello.txt");
}

#[test]
fn path_fallback_with_bracket_index() {
    // `$name[0]` with no typed value → `<val>[0]`. Bash compatible.
    let (out, err, code) = run(r#"name=abc; echo $name[0]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "abc[0]");
}

#[test]
fn path_does_not_swallow_trailing_dot_punct() {
    // `$name.` (dot followed by end/space) must NOT be consumed as path.
    let (out, err, code) = run(r#"name=foo; echo "$name. done""#);
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "foo. done");
}
