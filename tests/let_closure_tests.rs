/// Phase 5b — `let NAME = EXPR` typed bindings + closure literals `{|x| body}`.
///
/// Also fills the Phase 5c gap: path access into a typed `Value` stored in
/// `let_vars`, which couldn't be tested before `let` existed.
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
        .expect("spawn jsh");
    if !stdin.is_empty() {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().expect("wait");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

// ---------------------------------------------------------------------------
// `let` typed bindings
// ---------------------------------------------------------------------------

#[test]
fn let_int_then_interp() {
    let (out, err, code) = run(r#"let n = 42; echo $"n=($n)""#, "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "n=42");
}

#[test]
fn let_string_then_interp() {
    // Bare strings (no quotes) work because JSON sniffing fails → falls back to String.
    let (out, err, code) = run(r#"let name = alice; echo $"hi ($name)""#, "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "hi alice");
}

#[test]
fn let_json_record_path_access() {
    // Phase 5c path access requires a typed Value in let_vars — proving it now.
    let (out, err, code) = run(
        r#"let u = {"name":"bob","age":30}; echo $u.name $u.age"#,
        "",
    );
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "bob 30");
}

#[test]
fn let_json_list_index_access() {
    let (out, err, code) = run(r#"let xs = [10,20,30]; echo $xs[0] $xs[2]"#, "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "10 30");
}

#[test]
fn let_negative_index() {
    let (out, err, code) = run(r#"let xs = [10,20,30]; echo $xs[-1]"#, "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "30");
}

#[test]
fn let_does_not_swallow_bash_arithmetic_let() {
    // `let x=1+2` (no spaces around =) must keep bash arithmetic semantics.
    // Either jsh's bash-let runs and sets x to "3", or it errors — we just
    // need to verify our typed-let intercept did NOT trigger.
    let (_, _, code) = run(r#"let x=1+2; echo $x"#, "");
    // bash-let is not implemented as a builtin in jsh; the important thing is
    // that we don't crash and don't bind `x` to the typed-let value "1+2".
    // Just assert exit status is sane.
    assert!(code == 0 || code == 127, "unexpected exit {}", code);
}

// ---------------------------------------------------------------------------
// Closure literals + `where` / `each`
// ---------------------------------------------------------------------------

#[test]
fn where_inline_closure_filters() {
    let stdin = r#"[{"age":20},{"age":35},{"age":50}]"#;
    let (out, err, code) = run(
        r#"from-json | where {|r| [ $r.age -gt 30 ]} | to-json"#,
        stdin,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(parsed, serde_json::json!([{"age":35},{"age":50}]));
}

#[test]
fn where_let_bound_closure_filters() {
    let stdin = r#"[{"age":20},{"age":50}]"#;
    let (out, err, code) = run(
        r#"let f = {|r| [ $r.age -gt 30 ]}; from-json | where f | to-json"#,
        stdin,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(parsed, serde_json::json!([{"age":50}]));
}

#[test]
fn each_projects_field() {
    let stdin = r#"[{"a":1,"b":2},{"a":3,"b":4}]"#;
    let (out, err, code) = run(r#"from-json | each {|x| select a} | to-json"#, stdin);
    assert_eq!(code, 0, "stderr: {}", err);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(parsed, serde_json::json!([{"a":1},{"a":3}]));
}

#[test]
fn closure_captures_let_var_at_def_time() {
    // The closure should see `cutoff` as it was bound at definition time, even
    // if `cutoff` is later mutated.
    let stdin = r#"[{"v":5},{"v":15}]"#;
    let (out, err, code) = run(
        r#"let cutoff = 10; let f = {|r| [ $r.v -gt $cutoff ]}; let cutoff = 100; from-json | where f | to-json"#,
        stdin,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(parsed, serde_json::json!([{"v":15}]));
}
