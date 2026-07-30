/// Phase 12 — $in alias + each -k + get -i, match expression, url parse/join.
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

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

// ---------------------------------------------------------------------------
// 12a — $in alias + each --keep-empty + get --ignore-errors
// ---------------------------------------------------------------------------

#[test]
fn each_in_alias_bare_body() {
    // `$in` resolves to the current element even when no params declared.
    let (out, _, _) = run("from-json | each {|| $in * 2} | to-json", "[1, 2, 3]");
    assert_eq!(squash(&out), "[2,4,6]");
}

#[test]
fn each_in_alias_with_explicit_param() {
    // Explicit param still works; $in is the same value.
    let (out, _, _) = run("from-json | each {|x| $in + $x} | to-json", "[1, 2, 3]");
    assert_eq!(squash(&out), "[2,4,6]");
}

#[test]
fn each_drops_null_by_default() {
    let (out, _, _) = run(
        "from-json | each {|x| if $x == 0 { null } else { $x }} | to-json",
        "[0, 1, 0, 2, 3]",
    );
    assert_eq!(squash(&out), "[1,2,3]");
}

#[test]
fn each_keep_empty_preserves_null() {
    let (out, _, _) = run(
        "from-json | each -k {|x| if $x == 0 { null } else { $x }} | to-json",
        "[0, 1, 0, 2, 3]",
    );
    assert_eq!(squash(&out), "[null,1,null,2,3]");
}

#[test]
fn get_ignore_errors_returns_null() {
    // Missing path under -i: returns Null instead of erroring.
    let (out, _, code) = run("from-json | get -i nonexistent | to-json", r#"{"a": 1}"#);
    assert_eq!(code, 0);
    // Single Null wrapped in a list by to-json.
    assert_eq!(squash(&out), "[null]");
}

#[test]
fn get_missing_path_errors_without_flag() {
    let (_, err, code) = run("from-json | get nonexistent", r#"{"a": 1}"#);
    assert_ne!(code, 0);
    assert!(err.contains("not found") || err.contains("nonexistent"));
}

// ---------------------------------------------------------------------------
// 12b — match expression
// ---------------------------------------------------------------------------

#[test]
fn match_int_literals() {
    let (out, _, _) = run(
        r#"from-json | each {|x| match $x { 1 => "one", 2 => "two", _ => "other" }} | to-json"#,
        "[1, 2, 3, 1]",
    );
    assert_eq!(squash(&out), r#"["one","two","other","one"]"#);
}

#[test]
fn match_string_literals() {
    let (out, _, _) = run(
        r#"from-json | each {|x| match $x { "a" => 1, "b" => 2, _ => 0 }} | to-json"#,
        r#"["a", "b", "c"]"#,
    );
    assert_eq!(squash(&out), "[1,2,0]");
}

#[test]
fn match_bool_and_null() {
    let (out, _, _) = run(
        r#"from-json | each {|x| match $x { true => "T", false => "F", null => "N", _ => "?" }} | to-json"#,
        "[true, false, null, 1]",
    );
    assert_eq!(squash(&out), r#"["T","F","N","?"]"#);
}

#[test]
fn match_falls_through_to_null_without_wildcard() {
    let (out, _, _) = run(
        r#"from-json | each {|x| match $x { 1 => "one" }} | to-json"#,
        "[1, 2]",
    );
    // 2 has no arm and no wildcard → Null → dropped by default each.
    assert_eq!(squash(&out), r#"["one"]"#);
}

#[test]
fn match_with_field_scrutinee() {
    let (out, _, _) = run(
        r#"from-json | each {|r| match $r.color { "red" => "R", "blue" => "B", _ => "?" }} | to-json"#,
        r#"[{"color":"red"},{"color":"blue"},{"color":"green"}]"#,
    );
    assert_eq!(squash(&out), r#"["R","B","?"]"#);
}

#[test]
fn match_negative_int_pattern() {
    let (out, _, _) = run(
        r#"from-json | each {|x| match $x { -1 => "neg", 0 => "zero", _ => "pos" }} | to-json"#,
        "[-1, 0, 5]",
    );
    assert_eq!(squash(&out), r#"["neg","zero","pos"]"#);
}

// ---------------------------------------------------------------------------
// 12c — url parse / url join
// ---------------------------------------------------------------------------

#[test]
fn url_parse_full() {
    let (out, _, _) = run(
        "url parse 'https://user:pw@example.com:8080/api/v1?x=1&y=2#frag' | to-json",
        "",
    );
    assert!(out.contains("\"scheme\": \"https\""));
    assert!(out.contains("\"username\": \"user\""));
    assert!(out.contains("\"password\": \"pw\""));
    assert!(out.contains("\"host\": \"example.com\""));
    assert!(out.contains("\"port\": \"8080\""));
    assert!(out.contains("\"path\": \"/api/v1\""));
    assert!(out.contains("\"query\": \"x=1&y=2\""));
    assert!(out.contains("\"fragment\": \"frag\""));
    assert!(out.contains("\"x\": \"1\""));
    assert!(out.contains("\"y\": \"2\""));
}

#[test]
fn url_parse_minimal() {
    let (out, _, _) = run("url parse 'https://example.com' | to-json", "");
    assert!(out.contains("\"scheme\": \"https\""));
    assert!(out.contains("\"host\": \"example.com\""));
    assert!(out.contains("\"port\": \"\""));
    assert!(out.contains("\"path\": \"\""));
}

#[test]
fn url_parse_from_pipeline() {
    let (out, _, _) = run("echo 'https://example.com/foo' | url parse | get host", "");
    assert_eq!(out.trim(), "example.com");
}

#[test]
fn url_join_roundtrip() {
    let (out, _, _) = run("url parse 'https://example.com/foo?a=1&b=2' | url join", "");
    let s = out.trim();
    // params record is rebuilt; key order from IndexMap is insertion order.
    assert_eq!(s, "https://example.com/foo?a=1&b=2");
}

#[test]
fn url_join_minimal() {
    let (out, _, _) = run(
        r#"url join '{"scheme":"http","host":"x.com","path":"/y"}'"#,
        "",
    );
    assert_eq!(out.trim(), "http://x.com/y");
}
