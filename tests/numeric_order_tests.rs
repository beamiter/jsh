use indexmap::IndexMap;
use jsh::closure_expr::try_eval;
use jsh::environment::ShellState;
use jsh::pipeline_data::PipelineData;
use jsh::value::Value;
use jsh::value_builtins::VALUE_BUILTINS;
use std::collections::HashMap;
use std::process::Command;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

const LARGE: i64 = 9_007_199_254_740_993;

fn apply(
    name: &str,
    values: Vec<Value>,
    args: &[&str],
    state: &mut ShellState,
) -> Result<Vec<Value>, i32> {
    VALUE_BUILTINS[name](
        PipelineData::Values(values),
        &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
        state,
    )?
    .into_values()
}

fn record(value: Value) -> Value {
    Value::Record(IndexMap::from([("n".to_owned(), value)]))
}

#[test]
fn integer_sum_min_and_max_remain_exact_through_i64_boundaries() {
    let mut state = ShellState::new(false);
    for (op, values, expected) in [
        ("sum", vec![LARGE, 0], LARGE),
        ("sum", vec![i64::MAX, 0], i64::MAX),
        ("sum", vec![i64::MIN, 0], i64::MIN),
        ("sum", vec![i64::MAX, 1, -1], i64::MAX),
        ("sum", vec![i64::MAX, i64::MAX, -i64::MAX], i64::MAX),
        ("min", vec![LARGE, LARGE - 1], LARGE - 1),
        ("max", vec![LARGE - 1, LARGE], LARGE),
        ("min", vec![i64::MIN + 1, i64::MIN + 2], i64::MIN + 1),
        ("max", vec![i64::MAX - 1, i64::MAX], i64::MAX),
    ] {
        let output = apply(
            "math",
            values.into_iter().map(Value::Int).collect(),
            &[op],
            &mut state,
        )
        .unwrap();
        assert!(
            matches!(output.as_slice(), [Value::Int(actual)] if *actual == expected),
            "{op}: {output:?}"
        );
    }
}

#[test]
fn integer_sum_reports_out_of_range_final_totals() {
    let mut state = ShellState::new(false);
    for values in [vec![i64::MAX, 1], vec![i64::MIN, -1]] {
        state.last_error = None;
        assert!(apply(
            "math",
            values.into_iter().map(Value::Int).collect(),
            &["sum"],
            &mut state,
        )
        .is_err());
        assert!(state.last_error.is_some());
    }
}

#[test]
fn integer_numeric_strings_booleans_and_columns_keep_coercion_and_precision() {
    let mut state = ShellState::new(false);
    let values = vec![
        record(Value::String(LARGE.to_string())),
        record(Value::Bool(true)),
        record(Value::Bool(false)),
        record(Value::Null),
        Value::Record(IndexMap::new()),
        record(Value::String("not numeric".to_owned())),
    ];
    let result = apply("math", values, &["sum", "n"], &mut state).unwrap();
    assert_eq!(result, vec![Value::Int(LARGE + 1)]);
}

#[test]
fn mixed_sum_switches_to_float_after_exact_integer_prefix() {
    let mut state = ShellState::new(false);
    for (values, expected) in [
        (
            vec![Value::Int(LARGE), Value::Int(1), Value::Float(0.0)],
            (LARGE + 1) as f64,
        ),
        (
            vec![Value::Int(i64::MAX), Value::Int(1), Value::Float(0.5)],
            9_223_372_036_854_775_808.0,
        ),
        (vec![Value::Int(1), Value::Float(2.5)], 3.5),
        (
            vec![Value::String("1.25".to_owned()), Value::Float(2.5)],
            3.75,
        ),
    ] {
        let result = apply("math", values, &["sum"], &mut state).unwrap();
        assert_eq!(result[0].as_f64(), Some(expected));
    }
}

#[test]
fn mixed_min_max_choose_exact_values_at_float_integer_boundaries() {
    let mut state = ShellState::new(false);
    let pairs = [
        (Value::Int(LARGE), Value::Float((LARGE - 1) as f64)),
        (
            Value::Float(9_223_372_036_854_775_808.0),
            Value::Int(i64::MAX),
        ),
        (Value::Int(-1), Value::Float(-1.5)),
    ];
    for (greater, smaller) in pairs {
        for values in [
            vec![greater.clone(), smaller.clone()],
            vec![smaller.clone(), greater.clone()],
        ] {
            let minimum = apply("math", values.clone(), &["min"], &mut state).unwrap();
            let maximum = apply("math", values, &["max"], &mut state).unwrap();
            assert_eq!(minimum, vec![smaller.clone()]);
            assert_eq!(maximum, vec![greater.clone()]);
            if matches!(greater, Value::Int(_)) {
                assert!(matches!(maximum[0], Value::Int(_)));
            }
            if matches!(smaller, Value::Int(_)) {
                assert!(matches!(minimum[0], Value::Int(_)));
            }
        }
    }
}

#[test]
fn exact_aggregates_consume_streams_without_buffering_records() {
    let mut state = ShellState::new(false);
    for (op, expected) in [("sum", LARGE + 3), ("min", 0), ("max", LARGE)] {
        let pulls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&pulls);
        let source = [LARGE, 0, 1, 2].into_iter().map(move |number| {
            counter.fetch_add(1, Ordering::Relaxed);
            record(Value::Int(number))
        });
        let result = VALUE_BUILTINS["math"](
            PipelineData::Stream(Box::new(source)),
            &[op.to_owned(), "n".to_owned()],
            &mut state,
        )
        .unwrap()
        .into_values()
        .unwrap();
        assert_eq!(result, vec![Value::Int(expected)]);
        assert_eq!(pulls.load(Ordering::Relaxed), 4);
    }
}

#[test]
fn sort_and_sort_by_share_exact_numeric_order_and_keep_types() {
    let mut state = ShellState::new(false);
    let values = vec![
        Value::Int(i64::MAX),
        Value::String(LARGE.to_string()),
        Value::Int(LARGE - 1),
        Value::Float(-1.5),
        Value::Int(i64::MIN + 1),
        Value::Int(i64::MIN),
        Value::Null,
    ];
    let expected = vec![
        Value::Null,
        Value::Int(i64::MIN),
        Value::Int(i64::MIN + 1),
        Value::Float(-1.5),
        Value::Int(LARGE - 1),
        Value::String(LARGE.to_string()),
        Value::Int(i64::MAX),
    ];
    assert_eq!(
        apply("sort", values.clone(), &[], &mut state).unwrap(),
        expected
    );
    let rows = values.into_iter().map(record).collect();
    assert_eq!(
        apply("sort-by", rows, &["n"], &mut state).unwrap(),
        expected.into_iter().map(record).collect::<Vec<_>>()
    );
}

#[test]
fn mixed_numeric_text_order_is_consistent_and_nan_is_last_in_numeric_class() {
    let mut state = ShellState::new(false);
    let values = vec![
        Value::String("1x".to_owned()),
        Value::Int(10),
        Value::Int(2),
        Value::String("!".to_owned()),
        Value::Float(f64::NAN),
        Value::Float(f64::INFINITY),
        Value::Null,
        Value::Bool(false),
    ];
    let result = apply("sort", values, &[], &mut state).unwrap();
    assert_eq!(
        result
            .iter()
            .map(Value::to_display_string)
            .collect::<Vec<_>>(),
        ["", "false", "2", "10", "inf", "NaN", "!", "1x"]
    );
    let equal = vec![
        Value::Int(1),
        Value::Float(1.0),
        Value::String("1".to_owned()),
        Value::Bool(true),
    ];
    assert_eq!(
        apply("sort", equal.clone(), &[], &mut state)
            .unwrap()
            .iter()
            .map(Value::to_json)
            .collect::<Vec<_>>(),
        equal.iter().map(Value::to_json).collect::<Vec<_>>()
    );
}

#[test]
fn reverse_sort_preserves_equal_numeric_values_in_input_order() {
    let mut state = ShellState::new(false);
    let equal = vec![
        Value::Int(1),
        Value::Float(1.0),
        Value::String("1".to_owned()),
        Value::Bool(true),
    ];
    let expected: Vec<_> = equal.iter().map(Value::to_json).collect();
    for flag in ["-r", "--reverse"] {
        let actual = apply("sort", equal.clone(), &[flag], &mut state).unwrap();
        assert_eq!(
            actual.iter().map(Value::to_json).collect::<Vec<_>>(),
            expected
        );
    }
    let rows = equal.into_iter().map(record).collect::<Vec<_>>();
    let actual = apply("sort-by", rows.clone(), &["n", "-r"], &mut state).unwrap();
    assert_eq!(
        actual.iter().map(Value::to_json).collect::<Vec<_>>(),
        rows.iter().map(Value::to_json).collect::<Vec<_>>()
    );
}

#[test]
fn extrema_keep_nan_and_missing_number_behavior() {
    let mut state = ShellState::new(false);
    for op in ["min", "max"] {
        assert!(apply(
            "math",
            vec![Value::Null, Value::String("missing".to_owned())],
            &[op],
            &mut state,
        )
        .is_err());
        let actual = apply(
            "math",
            vec![
                Value::Float(f64::NAN),
                Value::Int(LARGE),
                Value::Float(f64::NAN),
            ],
            &[op],
            &mut state,
        )
        .unwrap();
        assert!(matches!(actual.as_slice(), [Value::Int(number)] if *number == LARGE));
        let only_nan = apply("math", vec![Value::Float(f64::NAN)], &[op], &mut state).unwrap();
        let expected = if op == "min" {
            f64::INFINITY
        } else {
            f64::NEG_INFINITY
        };
        assert_eq!(only_nan[0].as_f64(), Some(expected));
    }
    for op in ["-lt", "-gt", "-le", "-ge"] {
        assert!(apply(
            "where",
            vec![record(Value::Int(1))],
            &["n", op, "NaN"],
            &mut state
        )
        .unwrap()
        .is_empty());
    }
}

#[test]
fn where_numeric_relations_compare_integer_arguments_without_rounding() {
    let mut state = ShellState::new(false);
    let bound = (LARGE - 1).to_string();
    let values = vec![record(Value::Int(LARGE)), record(Value::Int(LARGE - 1))];
    assert_eq!(
        apply("where", values, &["n", "-gt", &bound], &mut state).unwrap(),
        vec![record(Value::Int(LARGE))]
    );
    let values = vec![record(Value::Int(i64::MAX))];
    assert_eq!(
        apply(
            "where",
            values.clone(),
            &["n", "-lt", "9223372036854775808.0"],
            &mut state
        )
        .unwrap(),
        values
    );
    let values = vec![record(Value::String("01".to_owned()))];
    assert!(apply("where", values, &["n", "=", "1"], &mut state)
        .unwrap()
        .is_empty());
}

#[test]
fn closure_numeric_string_relations_share_exact_ordering() {
    let vars = HashMap::from([
        ("i".to_owned(), Value::Int(LARGE)),
        ("s".to_owned(), Value::String((LARGE - 1).to_string())),
    ]);
    assert_eq!(try_eval("$i > $s", &vars).unwrap(), Some(Value::Bool(true)));
    assert_eq!(try_eval("$s < $i", &vars).unwrap(), Some(Value::Bool(true)));
}

#[test]
fn cli_exact_sum_and_order_survive_text_pipeline_boundaries() {
    for (script, expected) in [
        (
            "echo '[9007199254740993,0]' | from-json | math sum",
            serde_json::json!(LARGE),
        ),
        (
            "echo '[9007199254740993,9007199254740992]' | from-json | sort | to-json",
            serde_json::json!([LARGE - 1, LARGE]),
        ),
        (
            "range 9007199254740992..9007199254740993 | math max",
            serde_json::json!(LARGE),
        ),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_jsh"))
            .args(["-c", script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),
            expected,
            "{script}"
        );
    }
}
