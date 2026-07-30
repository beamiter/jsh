/// Phase 5a — typed-value pipeline tests.
///
/// These exercise the in-process value-aware pipeline path: from-json, where,
/// select, sort-by, to-json, to-table, count, math. Tests run an `jsh -c ...`
/// subprocess, capture stdout, and assert against expected JSON / table.
use std::io::Write;
use std::process::{Command, Stdio};

fn jsh_bin() -> String {
    // CARGO_BIN_EXE_<name> is populated by Cargo for integration tests.
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

#[test]
fn from_json_where_select_to_json() {
    let script = r#"echo '[{"name":"a","age":20},{"name":"b","age":40},{"name":"c","age":50}]' | from-json | where age -gt 30 | select name | to-json"#;
    let (out, err, code) = run(script, "");
    assert_eq!(code, 0, "stderr: {}", err);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    let expected: serde_json::Value =
        serde_json::from_str(r#"[{"name":"b"},{"name":"c"}]"#).unwrap();
    assert_eq!(parsed, expected);
}

#[test]
fn from_json_to_table_renders_columns() {
    let script = r#"echo '[{"a":1,"b":"xx"},{"a":22,"b":"y"}]' | from-json | to-table"#;
    let (out, _err, code) = run(script, "");
    assert_eq!(code, 0);
    assert!(out.contains("a "), "missing header `a`: {:?}", out);
    assert!(out.contains("22"), "missing row value: {:?}", out);
    assert!(out.contains("--"), "missing separator: {:?}", out);
}

#[test]
fn record_key_order_preserved() {
    // Insertion order b, a, c must round-trip exactly.
    let script = r#"echo '[{"b":1,"a":2,"c":3}]' | from-json | to-json"#;
    let (out, _err, code) = run(script, "");
    assert_eq!(code, 0);
    let idx_b = out.find("\"b\"").expect("b");
    let idx_a = out.find("\"a\"").expect("a");
    let idx_c = out.find("\"c\"").expect("c");
    assert!(idx_b < idx_a && idx_a < idx_c, "key order lost: {}", out);
}

#[test]
fn sort_by_reverse() {
    let script = r#"echo '[{"n":3},{"n":1},{"n":2}]' | from-json | sort-by n -r | to-json"#;
    let (out, _err, code) = run(script, "");
    assert_eq!(code, 0);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let expected: serde_json::Value = serde_json::from_str(r#"[{"n":3},{"n":2},{"n":1}]"#).unwrap();
    assert_eq!(parsed, expected);
}

#[test]
fn math_avg() {
    let script = r#"echo '[{"x":2},{"x":4},{"x":6}]' | from-json | math avg x"#;
    let (out, _err, code) = run(script, "");
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "4");
}

#[test]
fn count_returns_int() {
    let script = r#"echo '[{},{},{},{}]' | from-json | count"#;
    let (out, _err, code) = run(script, "");
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "4");
}

#[test]
fn external_then_value_aware_uses_fork_boundary() {
    // `echo` is external (well, builtin printed via fork in pipelines), feeds
    // into from-json which is value-aware. The boundary must serialize.
    let script = r#"echo '[{"k":1}]' | from-json | to-json"#;
    let (out, _err, code) = run(script, "");
    assert_eq!(code, 0);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(parsed, serde_json::json!([{"k": 1}]));
}

#[test]
fn first_and_last() {
    let script_first =
        r#"echo '[{"i":1},{"i":2},{"i":3},{"i":4}]' | from-json | first 2 | to-json"#;
    let (out, _, code) = run(script_first, "");
    assert_eq!(code, 0);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(parsed, serde_json::json!([{"i":1},{"i":2}]));

    let script_last = r#"echo '[{"i":1},{"i":2},{"i":3},{"i":4}]' | from-json | last 2 | to-json"#;
    let (out, _, code) = run(script_last, "");
    assert_eq!(code, 0);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(parsed, serde_json::json!([{"i":3},{"i":4}]));
}

#[test]
fn group_by_field() {
    let script = r#"echo '[{"t":"a","v":1},{"t":"b","v":2},{"t":"a","v":3}]' | from-json | group-by t | to-json"#;
    let (out, _, code) = run(script, "");
    assert_eq!(code, 0);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let expected: serde_json::Value = serde_json::json!([{
        "a": [{"t":"a","v":1},{"t":"a","v":3}],
        "b": [{"t":"b","v":2}]
    }]);
    assert_eq!(parsed, expected);
}
