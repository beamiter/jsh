/// Phase 15 — full signature coverage, streaming PipelineData,
/// typed user functions, did-you-mean error recovery.
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
// 15b — Stream variant + short-circuit
// ---------------------------------------------------------------------------

#[test]
fn range_take_does_not_materialize_full_range() {
    // 10 million ints would take noticeable time/memory if materialized.
    // With Phase 15b, `range` returns a Stream and `take` short-circuits.
    let start = std::time::Instant::now();
    let (out, _, code) = run("range 1..10000000 | take 3 | to-json", "");
    assert_eq!(code, 0);
    let compact: String = out.chars().filter(|c| !c.is_whitespace()).collect();
    assert_eq!(compact, "[1,2,3]");
    // Whole pipeline must comfortably finish under a second — if it
    // were materializing 10M ints that wouldn't happen in CI.
    assert!(
        start.elapsed().as_millis() < 2000,
        "range+take took {:?}, expected sub-second",
        start.elapsed()
    );
}

#[test]
fn from_ndjson_streams_lazy() {
    let stdin = (0..50)
        .map(|i| format!(r#"{{"id":{}}}"#, i))
        .collect::<Vec<_>>()
        .join("\n");
    let (out, _, code) = run("from-ndjson | first 3 | to-json", &stdin);
    assert_eq!(code, 0);
    assert!(out.contains("\"id\": 0"));
    assert!(out.contains("\"id\": 2"));
    assert!(!out.contains("\"id\": 10"));
}

#[test]
fn stream_length_drains_correctly() {
    let (out, _, code) = run("range 1..1000 | length", "");
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "1000");
}

#[test]
fn stream_passes_through_where_filter() {
    let (out, _, code) = run("range 1..10 | where {|x| $x > 7 } | to-json", "");
    assert_eq!(code, 0);
    let compact: String = out.chars().filter(|c| !c.is_whitespace()).collect();
    assert_eq!(compact, "[8,9,10]");
}

// ---------------------------------------------------------------------------
// 15c — typed user functions via `def`
// ---------------------------------------------------------------------------

#[test]
fn def_function_callable_with_typed_args() {
    let (out, _, code) = run("def add a:int b:int {|a,b| $a + $b}; add 3 4", "");
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "7");
}

#[test]
fn def_function_rejects_missing_required_arg() {
    let (_, err, code) = run("def add a:int b:int {|a,b| $a + $b}; add 1", "");
    assert_eq!(code, 2);
    assert!(err.contains("missing required arg"), "stderr was: {}", err);
    assert!(err.contains('b'));
}

#[test]
fn def_function_type_check_rejects_string_for_int() {
    let (_, err, code) = run("def add a:int b:int {|a,b| $a + $b}; add hello 4", "");
    assert_eq!(code, 2);
    assert!(err.contains("expected int"), "stderr was: {}", err);
}

#[test]
fn help_renders_user_signature() {
    let (out, _, code) = run("def greet name:string {|n| echo $n}; help greet", "");
    assert_eq!(code, 0);
    assert!(out.contains("user-defined"), "out was: {}", out);
    assert!(out.contains("name : string"), "out was: {}", out);
}

#[test]
fn def_function_with_optional_param() {
    let (out, _, code) = run("def maybe a:int b?:int {|a, b| $a + 0}; maybe 5", "");
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "5");
}

// ---------------------------------------------------------------------------
// 15d — did-you-mean error recovery
// ---------------------------------------------------------------------------

#[test]
fn typo_for_signed_builtin_suggests_correct_name() {
    let (_, err, code) = run("wher 1", "");
    // Not zero — `wher` doesn't exist.
    assert_ne!(code, 0);
    assert!(err.contains("did you mean 'where'?"), "stderr was: {}", err);
}

#[test]
fn typo_for_user_defined_function_is_suggested() {
    let (_, err, code) = run("def grompf x:int {|x| $x}; gromp 1", "");
    assert_ne!(code, 0);
    assert!(
        err.contains("did you mean 'grompf'?"),
        "stderr was: {}",
        err
    );
}

#[test]
fn missing_record_field_suggests_closest_key() {
    let (_, err, code) = run(
        r#"echo {"name":"alice","age":30} | from-json | get nme"#,
        "",
    );
    assert_ne!(code, 0);
    assert!(err.contains("did you mean 'name'?"), "stderr was: {}", err);
}

#[test]
fn get_with_no_close_match_does_not_suggest() {
    let (_, err, code) = run(r#"echo {"x":1} | from-json | get totally_unrelated"#, "");
    assert_ne!(code, 0);
    assert!(err.contains("get: path"), "stderr was: {}", err);
    assert!(!err.contains("did you mean"), "stderr was: {}", err);
}
