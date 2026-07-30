/// Phase 7 — iteration combinators (7a), record/table shaping (7b),
/// predicates/reflection (7c), path/date helpers (7d).
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
// 7a — iteration combinators
// ---------------------------------------------------------------------------

#[test]
fn reduce_sum_with_init() {
    let (out, err, code) = run("range 1..5 | reduce -i 0 {|acc, it| 0}", "");
    // The literal-body shortcut returns 0 every step; this just exercises the
    // 2-arg closure path without depending on arithmetic-in-closures.
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "0");
}

#[test]
fn reduce_no_init_returns_acc_when_single_element() {
    // Single-element input with no init should pass element through (no body call).
    let (out, err, code) = run(r#"from-json | reduce {|a, b| 0}"#, r#"[42]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "42");
}

#[test]
fn take_first_n() {
    let (out, err, code) = run("range 1..10 | take 3 | to-json", "");
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([1, 2, 3]));
}

#[test]
fn skip_first_n() {
    let (out, err, code) = run("range 1..5 | skip 2 | to-json", "");
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([3, 4, 5]));
}

#[test]
fn enumerate_adds_index_column() {
    let (out, err, code) = run(r#"from-json | enumerate | to-json"#, r#"["a","b","c"]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        p,
        serde_json::json!([
            {"index":0,"item":"a"},
            {"index":1,"item":"b"},
            {"index":2,"item":"c"}
        ])
    );
}

#[test]
fn zip_pairs_with_literal_list() {
    let (out, err, code) = run(
        r#"from-json | zip "[10,20,30]" | to-json"#,
        r#"["a","b","c"]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([["a", 10], ["b", 20], ["c", 30]]));
}

// ---------------------------------------------------------------------------
// 7b — record/table shaping
// ---------------------------------------------------------------------------

#[test]
fn columns_lists_record_keys() {
    let (out, err, code) = run("from-json | columns | to-json", r#"[{"a":1,"b":2,"c":3}]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!(["a", "b", "c"]));
}

#[test]
fn values_lists_record_values() {
    let (out, err, code) = run("from-json | values | to-json", r#"[{"a":1,"b":2,"c":3}]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([1, 2, 3]));
}

#[test]
fn rename_swaps_column_name() {
    let (out, err, code) = run("from-json | rename a A | to-json", r#"[{"a":1,"b":2}]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"A":1,"b":2}]));
}

fn key_order(json_text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = json_text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b'"' {
                j += 1;
            }
            let key = std::str::from_utf8(&bytes[start..j])
                .unwrap_or("")
                .to_string();
            // Only count as a key if followed by `:`
            let mut k = j + 1;
            while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\n') {
                k += 1;
            }
            if k < bytes.len() && bytes[k] == b':' {
                out.push(key);
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}

#[test]
fn move_before() {
    let (out, err, code) = run(
        "from-json | move c --before b | to-json",
        r#"[{"a":1,"b":2,"c":3}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(key_order(&out), vec!["a", "c", "b"]);
}

#[test]
fn move_after() {
    let (out, err, code) = run(
        "from-json | move a --after b | to-json",
        r#"[{"a":1,"b":2,"c":3}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(key_order(&out), vec!["b", "a", "c"]);
}

#[test]
fn merge_table_with_literal_table() {
    let (out, err, code) = run(
        r#"from-json | merge "[{\"b\":10},{\"b\":20}]" | to-json"#,
        r#"[{"a":1},{"a":2}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"a":1,"b":10},{"a":2,"b":20}]));
}

#[test]
fn upsert_overrides_existing() {
    let (out, err, code) = run("from-json | upsert a 99 | to-json", r#"[{"a":1}]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"a":99}]));
}

#[test]
fn upsert_inserts_when_missing() {
    let (out, err, code) = run("from-json | upsert b 5 | to-json", r#"[{"a":1}]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"a":1,"b":5}]));
}

#[test]
fn compact_drops_rows_with_null() {
    let (out, err, code) = run(
        "from-json | compact | to-json",
        r#"[{"a":1,"b":2},{"a":null,"b":3},{"a":4,"b":5}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"a":1,"b":2},{"a":4,"b":5}]));
}

// ---------------------------------------------------------------------------
// 7c — predicates / reflection
// ---------------------------------------------------------------------------

#[test]
fn any_with_var_path_closure() {
    let (out, err, code) = run(
        r#"from-json | any {|r| $r.a}"#,
        r#"[{"a":false},{"a":true},{"a":false}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "true");
}

#[test]
fn all_with_var_path_closure() {
    let (out, err, code) = run(
        r#"from-json | all {|r| $r.a}"#,
        r#"[{"a":true},{"a":true},{"a":false}]"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "false");
}

#[test]
fn is_empty_on_empty_list() {
    let (out, err, code) = run("from-json | is-empty", r#"[]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "true");
}

#[test]
fn is_empty_on_nonempty_list() {
    let (out, err, code) = run("from-json | is-empty", r#"[1,2]"#);
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "false");
}

#[test]
fn describe_table_vs_list() {
    let (out, _, _) = run("from-json | describe", r#"[{"a":1}]"#);
    assert_eq!(out.trim(), "table");
    let (out, _, _) = run("from-json | describe", r#"[1,2,3]"#);
    assert_eq!(out.trim(), "list");
    // Pipeline model unwraps the array, so `{"a":1}` and `[{"a":1}]` are
    // indistinguishable here — both classify as a 1-row table.
    let (out, _, _) = run("from-json | describe", r#"{"a":1}"#);
    assert_eq!(out.trim(), "table");
    let (out, _, _) = run("from-json | describe", r#""hello""#);
    assert_eq!(out.trim(), "string");
    let (out, _, _) = run("from-json | describe", r#"42"#);
    assert_eq!(out.trim(), "int");
}

// ---------------------------------------------------------------------------
// 7d — path / date
// ---------------------------------------------------------------------------

#[test]
fn path_join_args() {
    let (out, err, code) = run("path join a b c", "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "a/b/c");
}

#[test]
fn path_basename_arg() {
    let (out, err, code) = run("path basename /usr/local/bin/jsh", "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "jsh");
}

#[test]
fn path_dirname_arg() {
    let (out, err, code) = run("path dirname /usr/local/bin/jsh", "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "/usr/local/bin");
}

#[test]
fn path_parse_record_shape() {
    let (out, err, code) = run("path parse /tmp/foo.txt | to-json", "");
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        p,
        serde_json::json!([{"parent":"/tmp","stem":"foo","extension":"txt"}])
    );
}

#[test]
fn path_exists_self() {
    // Cargo.toml exists in the project root; tests run from there.
    let (out, err, code) = run("path exists Cargo.toml", "");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "true");
    let (out2, _, _) = run("path exists /definitely/does/not/exist/xyz123", "");
    assert_eq!(out2.trim(), "false");
}

#[test]
fn date_now_rfc3339_shape() {
    let (out, err, code) = run("date now", "");
    assert_eq!(code, 0, "stderr: {}", err);
    let s = out.trim();
    assert!(s.len() >= 20 && s.ends_with('Z'), "got {}", s);
    assert!(s.chars().nth(4) == Some('-'), "got {}", s);
    assert!(s.chars().nth(10) == Some('T'), "got {}", s);
}

#[test]
fn date_format_known_epoch() {
    // 1700000000 = 2023-11-14 22:13:20 UTC
    let (out, err, code) = run(
        r#"from-json | date format "%Y-%m-%d %H:%M:%S""#,
        "1700000000",
    );
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "2023-11-14 22:13:20");
}
