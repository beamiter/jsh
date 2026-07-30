/// Phase 13 — list utilities, histogram + Levenshtein, char/ansi/fill.
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
// 13a — prepend / append / drop / headers
// ---------------------------------------------------------------------------

#[test]
fn prepend_scalar() {
    let (out, _, _) = run("from-json | prepend 0 | to-json", "[1,2,3]");
    assert_eq!(squash(&out), "[0,1,2,3]");
}

#[test]
fn prepend_string() {
    let (out, _, _) = run(r#"from-json | prepend "head" | to-json"#, r#"["a","b"]"#);
    assert_eq!(squash(&out), r#"["head","a","b"]"#);
}

#[test]
fn append_scalar() {
    let (out, _, _) = run("from-json | append 99 | to-json", "[1,2,3]");
    assert_eq!(squash(&out), "[1,2,3,99]");
}

#[test]
fn drop_default_one() {
    let (out, _, _) = run("from-json | drop | to-json", "[1,2,3,4]");
    assert_eq!(squash(&out), "[1,2,3]");
}

#[test]
fn drop_n() {
    let (out, _, _) = run("from-json | drop 3 | to-json", "[1,2,3,4,5]");
    assert_eq!(squash(&out), "[1,2]");
}

#[test]
fn drop_more_than_length() {
    let (out, _, _) = run("from-json | drop 99 | to-json", "[1,2,3]");
    assert_eq!(squash(&out), "[]");
}

#[test]
fn headers_basic() {
    let (out, _, _) = run(
        "from-json | headers | to-json",
        r#"[["a","b"],[1,2],[3,4]]"#,
    );
    let s = squash(&out);
    assert!(s.contains(r#""a":1"#));
    assert!(s.contains(r#""b":2"#));
    assert!(s.contains(r#""a":3"#));
    assert!(s.contains(r#""b":4"#));
}

#[test]
fn headers_short_row_gets_null() {
    let (out, _, _) = run("from-json | headers | to-json", r#"[["a","b","c"],[1,2]]"#);
    assert!(out.contains("\"c\": null"));
}

// ---------------------------------------------------------------------------
// 13b — histogram + str distance
// ---------------------------------------------------------------------------

#[test]
fn histogram_counts_descending() {
    let (out, _, _) = run(
        "from-json | histogram | to-json",
        r#"["x","y","x","x","y","z"]"#,
    );
    // x appears 3, y 2, z 1 — sorted desc.
    let pos_x = out.find("\"value\": \"x\"").unwrap();
    let pos_y = out.find("\"value\": \"y\"").unwrap();
    let pos_z = out.find("\"value\": \"z\"").unwrap();
    assert!(pos_x < pos_y && pos_y < pos_z);
    assert!(out.contains("\"count\": 3"));
    assert!(out.contains("\"count\": 2"));
    assert!(out.contains("\"count\": 1"));
}

#[test]
fn histogram_freq_sums_to_one() {
    let (out, _, _) = run("from-json | histogram | to-json", "[1,1,2]");
    // Two distinct values: 1@2/3 and 2@1/3.
    assert!(out.contains("\"freq\": 0.6666"));
    assert!(out.contains("\"freq\": 0.3333"));
}

#[test]
fn histogram_by_field() {
    let (out, _, _) = run(
        "from-json | histogram color | to-json",
        r#"[{"color":"red"},{"color":"red"},{"color":"blue"}]"#,
    );
    assert!(out.contains("\"value\": \"red\""));
    assert!(out.contains("\"value\": \"blue\""));
}

#[test]
fn str_distance_classic() {
    let (out, _, _) = run("printf 'kitten' | str distance sitting", "");
    assert_eq!(out.trim(), "3");
}

#[test]
fn str_distance_zero_for_equal() {
    let (out, _, _) = run("printf 'abc' | str distance abc", "");
    assert_eq!(out.trim(), "0");
}

#[test]
fn str_distance_empty_other() {
    let (out, _, _) = run("printf 'abc' | str distance ''", "");
    assert_eq!(out.trim(), "3");
}

// ---------------------------------------------------------------------------
// 13c — char / ansi / fill
// ---------------------------------------------------------------------------

#[test]
fn char_newline() {
    let (out, _, _) = run("char newline", "");
    // String value carries "\n"; Display adds another '\n' at end of print.
    assert_eq!(out, "\n\n");
}

#[test]
fn char_tab() {
    let (out, _, _) = run("char tab", "");
    assert_eq!(out, "\t\n");
}

#[test]
fn char_hex_codepoint() {
    let (out, _, _) = run("char 0x41", "");
    assert_eq!(out.trim(), "A");
}

#[test]
fn ansi_red() {
    let (out, _, _) = run("ansi red", "");
    assert_eq!(out, "\x1b[31m\n");
}

#[test]
fn ansi_reset_and_numeric() {
    let (out1, _, _) = run("ansi reset", "");
    assert_eq!(out1, "\x1b[0m\n");
    let (out2, _, _) = run("ansi 91", "");
    assert_eq!(out2, "\x1b[91m\n");
}

#[test]
fn fill_left_align_default() {
    let (out, _, _) = run("printf 'hi' | fill -c '.' -w 5", "");
    assert_eq!(out.trim(), "hi...");
}

#[test]
fn fill_center() {
    let (out, _, _) = run("printf 'hi' | fill -c '.' -w 6 -a center", "");
    assert_eq!(out.trim(), "..hi..");
}

#[test]
fn fill_right() {
    let (out, _, _) = run("printf 'hi' | fill -c '.' -w 5 -a right", "");
    assert_eq!(out.trim(), "...hi");
}

#[test]
fn fill_no_op_when_wider() {
    let (out, _, _) = run("printf 'hello' | fill -c '.' -w 3", "");
    assert_eq!(out.trim(), "hello");
}
