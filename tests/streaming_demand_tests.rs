use indexmap::IndexMap;
use jsh::environment::ShellState;
use jsh::pipeline_data::PipelineData;
use jsh::value::{ClosureData, Value};
use jsh::value_builtins::VALUE_BUILTINS;
use serde_json::{json, Value as JsonValue};
use std::collections::HashMap;
use std::process::Command;
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

fn counted(values: Vec<Value>) -> (PipelineData, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&count);
    let source = values.into_iter().inspect(move |_| {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    (PipelineData::Stream(Box::new(source)), count)
}

// An unbounded source that fails deterministically if a consumer reads ahead.
// This catches accidental eager collection without relying on timing or OOM.
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

fn install_predicate(state: &mut ShellState, body: &str) {
    state.let_vars.insert(
        "predicate".to_owned(),
        Value::Closure(Arc::new(ClosureData {
            params: vec!["it".to_owned()],
            body_src: body.to_owned(),
            captured: HashMap::new(),
        })),
    );
}

#[test]
fn field_where_waits_for_demand_and_pulls_only_until_matches() {
    let mut state = ShellState::new(false);
    let (input, count) = bounded_demand(5);
    let wrapped = apply("wrap", input, &["n"], &mut state).unwrap();
    let filtered = apply("where", wrapped, &["n", "-ge", "3"], &mut state).unwrap();
    assert!(filtered.is_stream());
    assert_eq!(count.load(Ordering::Relaxed), 0);
    let selected = apply("take", filtered, &["2"], &mut state).unwrap();
    assert_eq!(json_values(selected), json!([{"n": 3}, {"n": 4}]));
    assert_eq!(count.load(Ordering::Relaxed), 5);
}

#[test]
fn invalid_field_where_is_rejected_without_reading_source() {
    let mut state = ShellState::new(false);
    for args in [
        vec!["n", "wat", "1"],
        vec!["n", "-gt"],
        vec!["n", "-gt", "1", "extra"],
    ] {
        let (input, count) = bounded_demand(0);
        assert!(apply("where", input, &args, &mut state).is_err());
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn quantifiers_short_circuit_streams_at_the_deciding_row() {
    for (name, body, expected) in [("any", "$it >= 2", true), ("all", "$it < 2", false)] {
        let mut state = ShellState::new(false);
        install_predicate(&mut state, body);
        let (input, count) = bounded_demand(3);
        let result = apply(name, input, &["predicate"], &mut state).unwrap();
        assert_eq!(json_values(result), json!([expected]));
        assert_eq!(count.load(Ordering::Relaxed), 3, "{name}");
    }
}

#[test]
fn quantifiers_keep_empty_semantics_and_stop_on_predicate_error() {
    for (name, expected) in [("any", false), ("all", true)] {
        let mut state = ShellState::new(false);
        install_predicate(&mut state, "$it / 0");
        let (empty, _) = counted(vec![]);
        let result = apply(name, empty, &["predicate"], &mut state).unwrap();
        assert_eq!(json_values(result), json!([expected]));
        let (input, count) = bounded_demand(1);
        assert!(apply(name, input, &["predicate"], &mut state).is_err());
        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert!(state.last_error.is_some());
    }
}

#[test]
fn is_empty_preserves_singleton_semantics_and_bounded_lookahead() {
    let mut state = ShellState::new(false);
    for (values, expected, pulls) in [
        (vec![], true, 0),
        (vec![Value::Null], true, 1),
        (vec![Value::String(String::new())], true, 1),
        (vec![Value::List(vec![])], true, 1),
        (vec![Value::Record(IndexMap::new())], true, 1),
        (vec![Value::Binary(vec![])], true, 1),
        (vec![Value::Bool(false)], false, 1),
        (vec![Value::Int(0)], false, 1),
        (vec![Value::Int(1), Value::Null, Value::Int(2)], false, 1),
        (vec![Value::Null, Value::Null, Value::Int(2)], false, 2),
    ] {
        let (input, count) = counted(values);
        assert_eq!(
            json_values(apply("is-empty", input, &[], &mut state).unwrap()),
            json!([expected])
        );
        assert_eq!(count.load(Ordering::Relaxed), pulls);
    }
    let (input, count) = bounded_demand(1);
    let result = apply("is-empty", input, &[], &mut state).unwrap();
    assert_eq!(json_values(result), json!([false]));
    assert_eq!(count.load(Ordering::Relaxed), 1);
}

#[test]
fn chunks_and_windows_wait_for_demand_without_reading_ahead() {
    let mut state = ShellState::new(false);
    for (name, args, pulls, expected) in [
        ("chunks", vec!["3"], 6, json!([[0, 1, 2], [3, 4, 5]])),
        ("window", vec!["3"], 4, json!([[0, 1, 2], [1, 2, 3]])),
        (
            "window",
            vec!["3", "--stride", "2"],
            5,
            json!([[0, 1, 2], [2, 3, 4]]),
        ),
        (
            "window",
            vec!["2", "--stride", "4"],
            6,
            json!([[0, 1], [4, 5]]),
        ),
    ] {
        let (input, count) = bounded_demand(pulls);
        let transformed = apply(name, input, &args, &mut state).unwrap();
        assert!(transformed.is_stream(), "{name} {args:?}");
        assert_eq!(count.load(Ordering::Relaxed), 0);
        let first_two = apply("take", transformed, &["2"], &mut state).unwrap();
        assert_eq!(json_values(first_two), expected);
        assert_eq!(count.load(Ordering::Relaxed), pulls);
    }
}

#[test]
fn take_zero_does_not_pull_new_lazy_filters_or_windows() {
    let mut state = ShellState::new(false);
    for (name, args) in [
        ("where", vec!["n", "-gt", "2"]),
        ("chunks", vec!["3"]),
        ("window", vec!["3", "-s", "2"]),
    ] {
        let (input, count) = bounded_demand(0);
        let transformed = apply(name, input, &args, &mut state).unwrap();
        let empty = apply("take", transformed, &["0"], &mut state).unwrap();
        assert_eq!(json_values(empty), json!([]));
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn finite_windows_match_slice_semantics_for_every_stride() {
    let mut state = ShellState::new(false);
    for len in 0..9 {
        let original: Vec<Value> = (0..len).map(Value::Int).collect();
        for size in 1..11 {
            for stride in 1..12 {
                let expected: Vec<Value> = original
                    .windows(size)
                    .step_by(stride)
                    .map(|window| Value::List(window.to_vec()))
                    .collect();
                let args = [size.to_string(), "--stride".to_owned(), stride.to_string()];
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                for streaming in [false, true] {
                    let input = if streaming {
                        counted(original.clone()).0
                    } else {
                        PipelineData::Values(original.clone())
                    };
                    let out = apply("window", input, &args, &mut state).unwrap();
                    assert_eq!(out.is_stream(), streaming);
                    assert_eq!(out.into_values().unwrap(), expected);
                }
            }
        }
    }
}

#[test]
fn chunks_keep_partial_tail_and_move_rows_without_cloning_payloads() {
    let mut state = ShellState::new(false);
    let payload = Arc::new(ClosureData {
        params: vec![],
        body_src: String::new(),
        captured: HashMap::new(),
    });
    let values = vec![Value::Closure(Arc::clone(&payload)); 5];
    let (input, count) = counted(values);
    let output = apply("chunks", input, &["3"], &mut state).unwrap();
    let chunks = output.into_values().unwrap();
    assert!(
        matches!(&chunks[..], [Value::List(a), Value::List(b)] if a.len() == 3 && b.len() == 2)
    );
    assert_eq!(count.load(Ordering::Relaxed), 5);
    assert_eq!(Arc::strong_count(&payload), 6);
    drop(chunks);
    assert_eq!(Arc::strong_count(&payload), 1);
}

#[test]
fn grouped_streams_retain_only_the_current_group_and_window() {
    let mut state = ShellState::new(false);
    let width = 7;
    for (name, args) in [
        ("chunks", vec!["7"]),
        ("window", vec!["7"]),
        ("window", vec!["7", "--stride", "3"]),
        ("window", vec!["7", "--stride", "12"]),
    ] {
        let payload = Arc::new(ClosureData {
            params: vec![],
            body_src: String::new(),
            captured: HashMap::new(),
        });
        let pulls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&pulls);
        let source_payload = Arc::clone(&payload);
        let source = (0..5000).map(move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
            assert!(
                Arc::strong_count(&source_payload) <= 2 * width + 3,
                "input rows were retained beyond the current group"
            );
            Value::Closure(Arc::clone(&source_payload))
        });
        let output = apply(
            name,
            PipelineData::Stream(Box::new(source)),
            &args,
            &mut state,
        )
        .unwrap();
        for group in output.into_value_iter().unwrap() {
            assert!(matches!(group, Value::List(ref items) if items.len() <= width));
            assert!(Arc::strong_count(&payload) <= 2 * width + 3);
        }
        assert_eq!(pulls.load(Ordering::Relaxed), 5000);
        assert_eq!(Arc::strong_count(&payload), 1);
    }
}

#[test]
fn grouping_arguments_fail_before_consuming_input() {
    let mut state = ShellState::new(false);
    for (name, cases) in [
        (
            "chunks",
            vec![vec![], vec!["0"], vec!["-1"], vec!["x"], vec!["1", "2"]],
        ),
        (
            "window",
            vec![
                vec![],
                vec!["0"],
                vec!["-1"],
                vec!["x"],
                vec!["1", "2"],
                vec!["1", "--stride"],
                vec!["1", "--stride", "0"],
                vec!["1", "--stride", "-1"],
                vec!["1", "--stride", "x"],
                vec!["1", "--stride", "2", "-s", "3"],
                vec!["1", "--unknown", "2"],
            ],
        ),
    ] {
        for args in cases {
            let (input, count) = bounded_demand(0);
            assert!(
                apply(name, input, &args, &mut state).is_err(),
                "{name} {args:?}"
            );
            assert_eq!(count.load(Ordering::Relaxed), 0);
        }
    }
}

#[test]
fn extreme_sizes_do_not_overflow_or_allocate_a_user_sized_buffer() {
    let mut state = ShellState::new(false);
    let max = usize::MAX.to_string();
    for (name, args, expected) in [
        ("chunks", vec![max.as_str()], json!([[1, 2, 3]])),
        ("window", vec![max.as_str()], json!([])),
        (
            "window",
            vec!["2", "--stride", max.as_str()],
            json!([[1, 2]]),
        ),
    ] {
        for streaming in [false, true] {
            let values = vec![Value::Int(1), Value::Int(2), Value::Int(3)];
            let input = if streaming {
                counted(values).0
            } else {
                PipelineData::Values(values)
            };
            assert_eq!(
                json_values(apply(name, input, &args, &mut state).unwrap()),
                expected,
                "{name} {args:?}"
            );
        }
    }
}

#[test]
fn new_stream_consumers_still_reject_malformed_json_boundaries() {
    let mut state = ShellState::new(false);
    install_predicate(&mut state, "true");
    for (name, args) in [
        ("where", vec!["n", "-gt", "0"]),
        ("any", vec!["predicate"]),
        ("all", vec!["predicate"]),
        ("is-empty", vec![]),
        ("chunks", vec!["2"]),
        ("window", vec!["2"]),
    ] {
        let input = PipelineData::Bytes(b"1\ninvalid\n".to_vec());
        assert!(apply(name, input, &args, &mut state).is_err(), "{name}");
    }
}

#[test]
fn command_line_grouped_stream_pipeline_preserves_values() {
    let out = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args([
            "-c",
            "range 0..1000 | wrap n | where n -ge 3 | window 3 --stride 2 | take 2 | to-json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<JsonValue>(&out.stdout).unwrap(),
        json!([[{"n": 3}, {"n": 4}, {"n": 5}], [{"n": 5}, {"n": 6}, {"n": 7}]])
    );
}
