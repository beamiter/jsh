//! Closure-form `where`/`each` stay lazy on streams when the body is a pure
//! expression, and failures inside a lazy stage are reported rather than
//! passing a truncated stream off as complete.
use jsh::environment::ShellState;
use jsh::pipeline_data::PipelineData;
use jsh::value::{ClosureData, Value};
use jsh::value_builtins::VALUE_BUILTINS;
use serde_json::{json, Value as JsonValue};
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

fn apply(
    name: &str,
    input: PipelineData,
    args: &[&str],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    VALUE_BUILTINS[name](
        input,
        &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
        state,
    )
}

fn json_values(input: PipelineData) -> JsonValue {
    JsonValue::Array(
        input
            .into_values()
            .unwrap()
            .iter()
            .map(Value::to_json)
            .collect(),
    )
}

/// An unbounded source that panics if a consumer reads past `limit` rows, so
/// eager collection is caught deterministically rather than by timing.
fn bounded_demand(limit: usize) -> (PipelineData, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&count);
    let source = (0_i64..).map(move |value| {
        let previous = counter.fetch_add(1, Ordering::Relaxed);
        assert!(previous < limit, "read beyond downstream demand: {limit}");
        Value::Int(value)
    });
    (PipelineData::Stream(Box::new(source)), count)
}

fn install_closure(state: &mut ShellState, body: &str) {
    state.let_vars.insert(
        "f".to_owned(),
        Value::Closure(Arc::new(ClosureData {
            params: vec!["it".to_owned()],
            body_src: body.to_owned(),
            captured: HashMap::new(),
        })),
    );
}

fn run(script: &str, stdin: &str) -> (String, String, i32) {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jsh");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait jsh");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn closure_where_pulls_only_until_downstream_demand_is_met() {
    let mut state = ShellState::new(false);
    install_closure(&mut state, "$it % 7 == 0");
    let (input, count) = bounded_demand(15);
    let filtered = apply("where", input, &["f"], &mut state).unwrap();
    assert!(filtered.is_stream());
    assert_eq!(count.load(Ordering::Relaxed), 0, "built without reading");
    let taken = apply("take", filtered, &["3"], &mut state).unwrap();
    assert_eq!(json_values(taken), json!([0, 7, 14]));
    assert_eq!(count.load(Ordering::Relaxed), 15);
}

#[test]
fn closure_each_maps_on_demand_and_drops_null_unless_kept() {
    let mut state = ShellState::new(false);
    install_closure(&mut state, "if $it % 2 == 0 { $it * 10 } else { null }");
    let (input, count) = bounded_demand(5);
    let mapped = apply("each", input, &["f"], &mut state).unwrap();
    assert!(mapped.is_stream());
    let taken = apply("take", mapped, &["3"], &mut state).unwrap();
    assert_eq!(json_values(taken), json!([0, 20, 40]));
    assert_eq!(count.load(Ordering::Relaxed), 5);

    let (input, _) = bounded_demand(3);
    let kept = apply("each", input, &["-k", "f"], &mut state).unwrap();
    let taken = apply("take", kept, &["3"], &mut state).unwrap();
    assert_eq!(json_values(taken), json!([0, null, 20]));
}

#[test]
fn materialized_input_and_shell_bodies_keep_the_eager_path() {
    let mut state = ShellState::new(false);
    install_closure(&mut state, "$it + 1");
    let values = PipelineData::Values(vec![Value::Int(1), Value::Int(2)]);
    let mapped = apply("each", values, &["f"], &mut state).unwrap();
    assert!(!mapped.is_stream());
    assert_eq!(json_values(mapped), json!([2, 3]));
}

#[test]
fn a_failing_lazy_stage_fails_whatever_drains_it() {
    let mut state = ShellState::new(false);
    install_closure(&mut state, "10 / ($it - 3)");
    let (input, count) = bounded_demand(4);
    let mapped = apply("each", input, &["f"], &mut state).unwrap();
    // The fourth row fails: collecting must not present three rows as all.
    assert_eq!(mapped.into_values(), Err(1));
    assert_eq!(
        count.load(Ordering::Relaxed),
        4,
        "nothing read past the error"
    );
}

#[test]
fn cli_lazy_where_over_a_huge_range_returns_promptly() {
    let (out, err, code) = run(
        "range 0..100000000 | where {|n| $n % 7 == 0} | take 3 | to-json",
        "",
    );
    assert_eq!(code, 0, "{err}");
    let parsed: JsonValue = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed, json!([0, 7, 14]));
}

#[test]
fn cli_stream_error_prints_rows_before_it_then_fails_with_a_message() {
    let (out, err, code) = run("range 0..5 | each {|x| 10 / (3 - $x)}", "");
    assert_eq!(out, "3.3333333333333335\n5\n10\n");
    assert!(err.contains("division by zero"), "{err}");
    assert_eq!(code, 1);
}

#[test]
fn cli_aggregates_over_a_failed_stream_are_not_reported_as_results() {
    let (out, err, code) = run("range 0..5 | each {|x| 10 / (3 - $x)} | count", "");
    assert_eq!(out, "");
    assert!(err.contains("division by zero"), "{err}");
    assert_eq!(code, 1);
}

#[test]
fn cli_eager_closure_errors_are_no_longer_silent() {
    let (out, err, code) = run(r#"echo "[1,0]" | from-json | each {|x| 10 / $x}"#, "");
    assert_eq!(out, "");
    assert!(err.contains("division by zero"), "{err}");
    assert_eq!(code, 1);
}

#[test]
fn cli_try_catches_a_lazy_stage_failure_without_printing_it() {
    let (out, err, code) = run(
        "try {|| range 0..5 | each {|x| 10 / (3 - $x)} } catch {|e| $e.msg}",
        "",
    );
    assert_eq!(out, "division by zero\n");
    assert_eq!(err, "");
    assert_eq!(code, 0);
}

#[test]
fn cli_a_failure_does_not_leak_into_the_next_command() {
    let (out, _, code) = run(
        "range 0..5 | each {|x| 1 / ($x - 1)} | take 1; range 1..3 | count",
        "",
    );
    assert_eq!(out, "-1\n3\n");
    assert_eq!(code, 0);
}

#[test]
fn cli_from_ndjson_skips_malformed_lines_by_default() {
    let (out, _, code) = run(
        "from-ndjson | length",
        "{\"a\":1}\nnot json\n\n{\"a\":3}\r\n",
    );
    assert_eq!((out.as_str(), code), ("2\n", 0));
}

#[test]
fn cli_from_ndjson_strict_names_the_bad_line_and_fails() {
    let (out, err, code) = run(
        "from-ndjson --strict | to-json",
        "{\"a\":1}\n\n{\"a\":2}\nnot json\n{\"a\":3}\n",
    );
    assert_eq!(out, "");
    assert!(
        err.contains("from-ndjson: line 4, column 2: invalid JSON"),
        "{err}"
    );
    assert!(!err.contains("at line 1"), "{err}");
    assert_eq!(code, 1);
}

#[test]
fn cli_from_ndjson_strict_stays_lazy_before_the_bad_line() {
    let (out, err, code) = run(
        "from-ndjson --strict | take 1 | to-json",
        "{\"a\":1}\nnot json\n",
    );
    assert_eq!(code, 0, "{err}");
    let parsed: JsonValue = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed, json!([{"a": 1}]));
}

#[test]
fn cli_from_ndjson_rejects_unknown_options() {
    let (_, err, code) = run("from-ndjson --bogus", "{}\n");
    assert!(err.contains("unknown option"), "{err}");
    assert_eq!(code, 2);
}
