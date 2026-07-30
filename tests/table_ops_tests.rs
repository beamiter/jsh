/// Phase 6c — table/record operators (`get`, `update`, `insert`, `reject`,
/// `wrap`, `flatten`).
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
// get (cell-path navigation)
// ---------------------------------------------------------------------------

#[test]
fn get_nested_via_index_then_fields() {
    let (out, err, code) = run("from-json | get 0.a.b", r#"[{"a":{"b":42}}]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "42");
}

#[test]
fn get_column_per_row() {
    let (out, err, code) = run("from-json | get a | to-json", r#"[{"a":10},{"a":20}]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([10, 20]));
}

#[test]
fn get_bracket_index() {
    let (out, err, code) = run("from-json | get [1]", r#"[10,20,30]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "20");
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

#[test]
fn update_literal_value() {
    let (out, err, code) = run("from-json | update a 99 | to-json", r#"[{"a":1}]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"a":99}]));
}

#[test]
fn update_closure_literal_body() {
    let (out, err, code) = run(
        "from-json | update a {|r| 100} | to-json",
        r#"[{"a":1},{"a":2}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"a":100},{"a":100}]));
}

#[test]
fn update_closure_var_path_body() {
    // `{|r| $r.a}` should read $r.a as a typed value, not exec it as a command.
    let (out, err, code) = run(
        "from-json | update b {|r| $r.a} | to-json",
        r#"[{"a":10},{"a":20}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"a":10,"b":10},{"a":20,"b":20}]));
}

// ---------------------------------------------------------------------------
// insert / reject
// ---------------------------------------------------------------------------

#[test]
fn insert_new_column() {
    let (out, err, code) = run(r#"from-json | insert b "hi" | to-json"#, r#"[{"a":1}]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"a":1,"b":"hi"}]));
}

#[test]
fn insert_existing_column_errors() {
    let (_, _, code) = run("from-json | insert a 9 | to-json", r#"[{"a":1}]"#);
    assert_ne!(code, 0);
}

#[test]
fn reject_drops_named_columns() {
    let (out, err, code) = run("from-json | reject b | to-json", r#"[{"a":1,"b":2,"c":3}]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"a":1,"c":3}]));
}

// ---------------------------------------------------------------------------
// wrap / flatten
// ---------------------------------------------------------------------------

#[test]
fn wrap_assigns_column_to_each_value() {
    let (out, err, code) = run("from-json | wrap val | to-json", r#"[1,2,3]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"val":1},{"val":2},{"val":3}]));
}

#[test]
fn flatten_unnests_lists() {
    let (out, err, code) = run("from-json | flatten | to-json", r#"[[1,2],[3,4]]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([1, 2, 3, 4]));
}
