/// Phase 6b — text → structured bridges (`lines`, `split row|column`,
/// `parse`, `str <sub>`).
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
// lines
// ---------------------------------------------------------------------------

#[test]
fn lines_splits_stdin_drops_trailing_empty() {
    let (out, err, code) = run("lines | to-json", "a\nb\nc\n");
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!(["a", "b", "c"]));
}

#[test]
fn lines_keeps_internal_empty() {
    let (out, err, code) = run("lines | to-json", "a\n\nb\n");
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!(["a", "", "b"]));
}

// ---------------------------------------------------------------------------
// split row / split column
// ---------------------------------------------------------------------------

#[test]
fn split_row_csv() {
    let (out, err, code) = run(r#"split row "," | to-json"#, "a,b,c");
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!(["a", "b", "c"]));
}

#[test]
fn split_column_per_line() {
    let (out, err, code) = run(
        r#"lines | split column "," name num | to-json"#,
        "a,1\nb,2\n",
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        p,
        serde_json::json!([
            {"name":"a","num":"1"},
            {"name":"b","num":"2"}
        ])
    );
}

// ---------------------------------------------------------------------------
// parse template
// ---------------------------------------------------------------------------

#[test]
fn parse_named_captures() {
    let (out, err, code) = run(
        r#"lines | parse "{name} {age}" | to-json"#,
        "alice 30\nbob 25\n",
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        p,
        serde_json::json!([
            {"name":"alice","age":"30"},
            {"name":"bob","age":"25"}
        ])
    );
}

#[test]
fn parse_skips_non_matching() {
    let (out, err, code) = run(
        r#"lines | parse "{user}:{role}" | to-json"#,
        "alice:admin\nnomatch\nbob:user\n",
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        p,
        serde_json::json!([
            {"user":"alice","role":"admin"},
            {"user":"bob","role":"user"}
        ])
    );
}

// ---------------------------------------------------------------------------
// str subcommands
// ---------------------------------------------------------------------------

#[test]
fn str_trim_per_line() {
    let (out, err, code) = run("lines | str trim | to-json", "  a  \n  b  \n");
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!(["a", "b"]));
}

#[test]
fn str_upcase_and_downcase() {
    let (out, err, code) = run("str upcase", "abc");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "ABC");
    let (out2, _, _) = run("str downcase", "ABC");
    assert_eq!(out2.trim(), "abc");
}

#[test]
fn str_contains_returns_bool() {
    let (out, err, code) = run(r#"str contains "world""#, "hello world");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "true");
    let (out2, _, _) = run(r#"str contains "xyz""#, "hello world");
    assert_eq!(out2.trim(), "false");
}

#[test]
fn str_replace_simple() {
    let (out, err, code) = run(r#"str replace "foo" "bar""#, "foo and foo");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "bar and bar");
}
