/// Phase 11 — sort/to-csv/chunks/window/split-by + encode/decode (base64/hex).
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
// 11a — sort / to-csv / chunks / window / split-by
// ---------------------------------------------------------------------------

#[test]
fn sort_ascending_numeric() {
    let (out, _, _) = run("from-json | sort | to-json", "[3, 1, 4, 1, 5, 9, 2, 6]");
    assert_eq!(squash(&out), "[1,1,2,3,4,5,6,9]");
}

#[test]
fn sort_descending_flag() {
    let (out, _, _) = run("from-json | sort -r | to-json", "[3, 1, 4, 1, 5]");
    assert_eq!(squash(&out), "[5,4,3,1,1]");
}

#[test]
fn sort_strings() {
    let (out, _, _) = run(
        "from-json | sort | to-json",
        r#"["banana", "apple", "cherry"]"#,
    );
    assert_eq!(squash(&out), r#"["apple","banana","cherry"]"#);
}

#[test]
fn to_csv_basic() {
    let (out, _, _) = run("from-json | to-csv", r#"[{"a":1,"b":"x"},{"a":2,"b":"y"}]"#);
    let lines: Vec<&str> = out.trim().lines().collect();
    assert_eq!(lines, vec!["a,b", "1,x", "2,y"]);
}

#[test]
fn to_csv_quotes_comma_and_quote() {
    let (out, _, _) = run(
        "from-json | to-csv",
        r#"[{"a":"x,y","b":"he said \"hi\""}]"#,
    );
    assert!(out.contains("\"x,y\""));
    assert!(out.contains("\"he said \"\"hi\"\"\""));
}

#[test]
fn chunks_size_3() {
    let (out, _, _) = run("from-json | chunks 3 | to-json", "[1,2,3,4,5,6,7]");
    assert_eq!(squash(&out), "[[1,2,3],[4,5,6],[7]]");
}

#[test]
fn window_default_stride_1() {
    let (out, _, _) = run("from-json | window 3 | to-json", "[1,2,3,4,5]");
    assert_eq!(squash(&out), "[[1,2,3],[2,3,4],[3,4,5]]");
}

#[test]
fn window_custom_stride() {
    let (out, _, _) = run(
        "from-json | window 3 --stride 2 | to-json",
        "[1,2,3,4,5,6,7]",
    );
    assert_eq!(squash(&out), "[[1,2,3],[3,4,5],[5,6,7]]");
}

#[test]
fn split_by_field() {
    let (out, _, _) = run(
        "from-json | split-by k | to-json",
        r#"[{"k":"a","v":1},{"k":"b","v":2},{"k":"a","v":3}]"#,
    );
    // One Record output with keys "a" and "b".
    assert!(out.contains("\"a\":"));
    assert!(out.contains("\"b\":"));
    assert!(out.contains("\"v\": 3"));
}

// ---------------------------------------------------------------------------
// 11b — encode / decode (base64, hex)
// ---------------------------------------------------------------------------

#[test]
fn encode_base64_basic() {
    let (out, _, _) = run("printf 'hello world' | encode base64", "");
    assert_eq!(out.trim(), "aGVsbG8gd29ybGQ=");
}

#[test]
fn encode_base64_padding_variants() {
    let (out1, _, _) = run("printf 'a' | encode base64", "");
    assert_eq!(out1.trim(), "YQ==");
    let (out2, _, _) = run("printf 'ab' | encode base64", "");
    assert_eq!(out2.trim(), "YWI=");
    let (out3, _, _) = run("printf 'abc' | encode base64", "");
    assert_eq!(out3.trim(), "YWJj");
}

#[test]
fn decode_base64_basic() {
    let (out, _, _) = run("echo 'aGVsbG8gd29ybGQ=' | decode base64", "");
    assert_eq!(out.trim(), "hello world");
}

#[test]
fn base64_roundtrip() {
    let (out, _, _) = run("printf 'Phase 11!' | encode base64 | decode base64", "");
    assert_eq!(out.trim(), "Phase 11!");
}

#[test]
fn encode_hex_basic() {
    let (out, _, _) = run("printf 'hi' | encode hex", "");
    assert_eq!(out.trim(), "6869");
}

#[test]
fn decode_hex_basic() {
    let (out, _, _) = run("echo '6869' | decode hex", "");
    assert_eq!(out.trim(), "hi");
}

#[test]
fn hex_roundtrip() {
    let (out, _, _) = run("printf 'abc' | encode hex | decode hex", "");
    assert_eq!(out.trim(), "abc");
}

#[test]
fn decode_hex_uppercase_text() {
    // ASCII-text payload survives the fork boundary as String (vs. Binary,
    // which would be base64-recoded by the JSON serializer).
    let (out, _, _) = run("echo '68656c6c6f' | decode hex", "");
    assert_eq!(out.trim(), "hello");
}
