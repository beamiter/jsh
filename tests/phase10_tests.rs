/// Phase 10 — math extensions, regex support, and table utilities.
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
// 10a — math extensions
// ---------------------------------------------------------------------------

#[test]
fn math_abs() {
    let (out, _, _) = run("from-json | math abs | to-json", "[-1, -2, 3, -4]");
    assert_eq!(squash(&out), "[1,2,3,4]");
}

#[test]
fn math_round_floor_ceil() {
    let (out1, _, _) = run("from-json | math round | to-json", "[1.4, 1.6, -1.5]");
    assert_eq!(squash(&out1), "[1,2,-2]");
    let (out2, _, _) = run("from-json | math floor | to-json", "[1.9, 2.1]");
    assert_eq!(squash(&out2), "[1,2]");
    let (out3, _, _) = run("from-json | math ceil | to-json", "[1.1, 2.9]");
    assert_eq!(squash(&out3), "[2,3]");
}

#[test]
fn math_sqrt() {
    let (out, _, _) = run("from-json | math sqrt | to-json", "[16, 25, 100]");
    assert_eq!(squash(&out), "[4,5,10]");
}

#[test]
fn math_median_odd() {
    let (out, _, _) = run("from-json | math median", "[1, 2, 3, 4, 5, 6, 7]");
    assert_eq!(out.trim(), "4");
}

#[test]
fn math_median_even() {
    let (out, _, _) = run("from-json | math median", "[1, 2, 3, 4]");
    assert_eq!(out.trim(), "2.5");
    let (out2, _, _) = run("from-json | math median", "[1, 2, 4, 5]");
    assert_eq!(out2.trim(), "3");
}

#[test]
fn math_product() {
    let (out, _, _) = run("from-json | math product", "[1, 2, 3, 4, 5]");
    assert_eq!(out.trim(), "120");
}

// ---------------------------------------------------------------------------
// 10b — regex
// ---------------------------------------------------------------------------

#[test]
fn str_replace_regex_single() {
    let (out, _, _) = run("printf 'foo123bar456' | str replace -r '\\d+' 'X'", "");
    assert_eq!(out.trim(), "fooXbar456");
}

#[test]
fn str_replace_regex_all() {
    let (out, _, _) = run("printf 'foo123bar456' | str replace -r -a '\\d+' 'X'", "");
    assert_eq!(out.trim(), "fooXbarX");
}

#[test]
fn str_replace_literal_unchanged() {
    // Without -r, literal replacement (all occurrences).
    let (out, _, _) = run("printf 'a.b.c' | str replace '.' '-'", "");
    assert_eq!(out.trim(), "a-b-c");
}

#[test]
fn parse_regex_named_captures() {
    let (out, _, _) = run(
        "printf 'a=1\\nb=22\\n' | parse -r '(?P<key>\\w+)=(?P<val>\\d+)' | to-json",
        "",
    );
    assert!(out.contains("\"key\": \"a\""));
    assert!(out.contains("\"val\": \"22\""));
}

#[test]
fn parse_regex_anonymous_captures() {
    let (out, _, _) = run(
        "printf 'x42\\ny99\\n' | parse -r '(\\w)(\\d+)' | to-json",
        "",
    );
    assert!(out.contains("\"capture1\": \"x\""));
    assert!(out.contains("\"capture2\": \"42\""));
}

// ---------------------------------------------------------------------------
// 10c — default / transpose / uniq -c / shuffle
// ---------------------------------------------------------------------------

#[test]
fn default_replaces_null_scalar() {
    let (out, _, _) = run("from-json | default 99 | to-json", "[null, 1, null, 2]");
    assert_eq!(squash(&out), "[99,1,99,2]");
}

#[test]
fn default_fills_missing_field() {
    let (out, _, _) = run(
        "from-json | default 0 age | to-json",
        r#"[{"name":"a"},{"name":"b","age":30}]"#,
    );
    assert!(out.contains("\"age\": 0"));
    assert!(out.contains("\"age\": 30"));
}

#[test]
fn transpose_basic() {
    let (out, _, _) = run(
        "from-json | transpose | to-json",
        r#"[{"a":1,"b":2},{"a":3,"b":4}]"#,
    );
    // Two columns: a → [1,3], b → [2,4]
    assert!(out.contains("\"column\": \"a\""));
    assert!(out.contains("\"column\": \"b\""));
    assert!(out.contains("\"row0\": 1"));
    assert!(out.contains("\"row1\": 3"));
}

#[test]
fn unique_count() {
    let (out, _, _) = run("from-json | unique -c | to-json", "[1, 1, 2, 3, 3, 3]");
    // Three distinct values, sorted by descending count.
    assert!(out.contains("\"value\": 3"));
    assert!(out.contains("\"count\": 3"));
    assert!(out.contains("\"count\": 2"));
    assert!(out.contains("\"count\": 1"));
    // 3 should come first (count 3).
    let pos_three = out.find("\"value\": 3").unwrap();
    let pos_two = out.find("\"value\": 2").unwrap();
    assert!(pos_three < pos_two);
}

#[test]
fn shuffle_preserves_elements() {
    let (out, _, _) = run("from-json | shuffle | length", "[3, 1, 4, 1, 5]");
    assert_eq!(out.trim(), "5");
}

#[test]
fn shuffle_actually_randomizes() {
    // Run shuffle a few times on a large input; not all outputs should be
    // identical to the input order. (Tiny chance of false negative, but with
    // 100 elements that's effectively zero.)
    let input: String = format!(
        "[{}]",
        (0..100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let (out, _, _) = run("from-json | shuffle | to-json", &input);
    let identity: String = format!(
        "[{}]",
        (0..100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    assert_ne!(squash(&out), squash(&identity));
}
