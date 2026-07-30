/// Phase 8a — closure-body expression evaluator.
/// Closures can now do `$a + $b`, `$r.age > 30`, etc. without the body being
/// interpreted as a shell command pipeline.
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
// reduce with arithmetic
// ---------------------------------------------------------------------------

#[test]
fn reduce_sum_range() {
    let (out, err, code) = run("range 1..10 | reduce -i 0 {|acc, it| $acc + $it}", "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "55");
}

#[test]
fn reduce_product_range() {
    let (out, err, code) = run("range 1..5 | reduce -i 1 {|acc, it| $acc * $it}", "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "120");
}

// ---------------------------------------------------------------------------
// each with arithmetic / field access
// ---------------------------------------------------------------------------

#[test]
fn each_multiplies_field() {
    let (out, err, code) = run(
        "from-json | each {|r| $r.a * 10} | to-json",
        r#"[{"a":1},{"a":2},{"a":3}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([10, 20, 30]));
}

#[test]
fn each_combines_two_fields() {
    let (out, err, code) = run(
        "from-json | each {|r| $r.price * $r.qty} | math sum",
        r#"[{"price":10,"qty":3},{"price":20,"qty":2}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "70");
}

// ---------------------------------------------------------------------------
// where with closure predicate
// ---------------------------------------------------------------------------

#[test]
fn where_closure_gt() {
    let (out, err, code) = run(
        "from-json | where {|r| $r.age > 30} | to-json",
        r#"[{"age":20},{"age":35},{"age":50}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"age":35},{"age":50}]));
}

#[test]
fn where_closure_and() {
    let (out, err, code) = run(
        "from-json | where {|r| $r.age > 20 && $r.age < 50} | to-json",
        r#"[{"age":10},{"age":35},{"age":50},{"age":60}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"age":35}]));
}

// ---------------------------------------------------------------------------
// any / all with arithmetic
// ---------------------------------------------------------------------------

#[test]
fn any_with_expression() {
    let (out, err, code) = run(
        r#"from-json | any {|r| $r.score >= 90}"#,
        r#"[{"score":50},{"score":80},{"score":95}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "true");
}

#[test]
fn all_with_expression() {
    let (out, err, code) = run(
        r#"from-json | all {|r| $r.score >= 50}"#,
        r#"[{"score":50},{"score":80},{"score":95}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "true");
    let (out2, _, _) = run(
        r#"from-json | all {|r| $r.score >= 60}"#,
        r#"[{"score":50},{"score":80}]"#,
    );
    assert_eq!(out2.trim(), "false");
}

// ---------------------------------------------------------------------------
// string concat via +
// ---------------------------------------------------------------------------

#[test]
fn each_string_concat() {
    let (out, err, code) = run(
        r#"from-json | each {|r| $r.name + "!"} | to-json"#,
        r#"[{"name":"alice"},{"name":"bob"}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!(["alice!", "bob!"]));
}

// ---------------------------------------------------------------------------
// update with closure expression body
// ---------------------------------------------------------------------------

#[test]
fn update_with_expression() {
    let (out, err, code) = run(
        "from-json | update n {|r| $r.n * 2} | to-json",
        r#"[{"n":1},{"n":2},{"n":3}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"n":2},{"n":4},{"n":6}]));
}

// ---------------------------------------------------------------------------
// negation / parens / precedence
// ---------------------------------------------------------------------------

#[test]
fn precedence_with_parens() {
    let (out, err, code) = run(
        "from-json | each {|r| ($r.a + $r.b) * 2} | to-json",
        r#"[{"a":1,"b":2},{"a":3,"b":4}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([6, 14]));
}

#[test]
fn not_operator() {
    let (out, err, code) = run(
        r#"from-json | where {|r| !$r.done} | to-json"#,
        r#"[{"done":true,"id":1},{"done":false,"id":2},{"done":true,"id":3}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"done":false,"id":2}]));
}
