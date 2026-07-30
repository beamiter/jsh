/// Phase 6d — type conversion (`into`) + math aggregations + `range`.
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
// range
// ---------------------------------------------------------------------------

#[test]
fn range_inclusive() {
    let (out, err, code) = run("range 1..5 | to-json", "");
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([1, 2, 3, 4, 5]));
}

#[test]
fn range_exclusive_quoted() {
    // `<` is a shell redirect operator; the user must quote the range literal.
    let (out, err, code) = run(r#"range "0..<3" | to-json"#, "");
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([0, 1, 2]));
}

#[test]
fn range_single_value() {
    let (out, err, code) = run("range 5..5 | to-json", "");
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([5]));
}

// ---------------------------------------------------------------------------
// into int / float / string / bool (whole-value form)
// ---------------------------------------------------------------------------

#[test]
fn into_int_parses_strings() {
    let (out, err, code) = run("from-json | into int | to-json", r#"["1","2","3"]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([1, 2, 3]));
}

#[test]
fn into_float_parses_strings() {
    let (out, err, code) = run("from-json | into float | to-json", r#"["1.5","2.0"]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([1.5, 2.0]));
}

#[test]
fn into_string_renders_numbers() {
    let (out, err, code) = run("from-json | into string | to-json", r#"[1,2,3]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!(["1", "2", "3"]));
}

#[test]
fn into_bool_parses_truthy() {
    let (out, err, code) = run(
        "from-json | into bool | to-json",
        r#"["true","false","true"]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([true, false, true]));
}

// ---------------------------------------------------------------------------
// into with column argument (record form)
// ---------------------------------------------------------------------------

#[test]
fn into_int_on_column() {
    let (out, err, code) = run(
        "from-json | into int a | to-json",
        r#"[{"a":"5","b":"x"},{"a":"7","b":"y"}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"a":5,"b":"x"},{"a":7,"b":"y"}]));
}

#[test]
fn into_float_on_column() {
    let (out, err, code) = run(
        "from-json | into float price | to-json",
        r#"[{"price":"1.5"},{"price":"2.25"}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"price":1.5},{"price":2.25}]));
}

// ---------------------------------------------------------------------------
// math aggregations (number-list mode)
// ---------------------------------------------------------------------------

#[test]
fn math_sum_on_range() {
    let (out, err, code) = run("range 1..10 | math sum", "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "55");
}

#[test]
fn math_avg_on_range() {
    let (out, err, code) = run("range 1..5 | math avg", "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "3");
}

#[test]
fn math_min_max_on_range() {
    let (out, _, _) = run("range 3..7 | math min", "");
    assert_eq!(out.trim(), "3");
    let (out, _, _) = run("range 3..7 | math max", "");
    assert_eq!(out.trim(), "7");
}

#[test]
fn math_stddev_on_range() {
    // stddev(1..5) = sqrt(2) ≈ 1.4142135623730951
    let (out, err, code) = run("range 1..5 | math stddev", "");
    assert_eq!(code, 0, "stderr: {}", err);
    let v: f64 = out.trim().parse().unwrap();
    assert!((v - std::f64::consts::SQRT_2).abs() < 1e-10, "got {}", v);
}

#[test]
fn math_mean_alias() {
    let (out, err, code) = run("range 2..4 | math mean", "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "3");
}

// ---------------------------------------------------------------------------
// math aggregations on a column (record form)
// ---------------------------------------------------------------------------

#[test]
fn math_sum_on_field() {
    let (out, err, code) = run("from-json | math sum a", r#"[{"a":10},{"a":20},{"a":30}]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "60");
}

#[test]
fn math_avg_on_field() {
    let (out, err, code) = run(
        "from-json | math avg score",
        r#"[{"score":80},{"score":90},{"score":100}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "90");
}
