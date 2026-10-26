use jsh::environment::ShellState;
use jsh::pipeline_data::PipelineData;
use jsh::value::Value;
use jsh::value_builtins::VALUE_BUILTINS;
use serde_json::json;

fn run(name: &str, values: serde_json::Value, args: &[&str]) -> Vec<Value> {
    let values = values
        .as_array()
        .unwrap()
        .iter()
        .cloned()
        .map(Value::from_json)
        .collect();
    VALUE_BUILTINS[name](
        PipelineData::Values(values),
        &args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
        &mut ShellState::new(false),
    )
    .unwrap()
    .into_values()
    .unwrap()
}

#[test]
fn unique_preserves_types_and_nested_boundaries() {
    let input =
        json!([1, "1", true, "true", null, "", [1, 2], "[1, 2]", {"a": 1}, "{a: 1}", 1, "1"]);
    let out = run("unique", input.clone(), &[]);
    assert_eq!(out.len(), 10);
    assert_eq!(
        out.iter().map(Value::to_json).collect::<Vec<_>>(),
        input.as_array().unwrap()[..10]
    );
}

#[test]
fn unique_count_and_histogram_keep_distinct_types_and_stable_ties() {
    for (name, args) in [("unique", vec!["--count"]), ("histogram", vec![])] {
        let out = run(name, json!([1, "1", "", null, 1, "1"]), &args);
        assert_eq!(out.len(), 4, "{name}");
        assert_eq!(
            out.iter()
                .map(|v| v.get("value").unwrap().to_json())
                .collect::<Vec<_>>(),
            vec![json!(1), json!("1"), json!(""), json!(null)]
        );
        assert_eq!(
            out.iter()
                .map(|v| v.get("count").unwrap().to_json())
                .collect::<Vec<_>>(),
            vec![json!(2), json!(2), json!(1), json!(1)]
        );
    }
}

#[test]
fn unique_by_field_distinguishes_missing_null_and_empty_string() {
    let input = json!([{}, {"k": null}, {"k": ""}, {"k": 1}, {"k": "1"}, {}, {"k": null}]);
    let out = run("unique", input, &["k"]);
    assert_eq!(out.len(), 5);
}

#[test]
fn unicode_tables_align_terminal_columns_in_both_renderers() {
    let input = json!([{"姓名": "张三", "value": 1}, {"姓名": "e\u{301}", "value": 2}, {"姓名": "🦀", "value": 3}]);
    let records = input.as_array().unwrap();
    let typed: Vec<_> = records.iter().cloned().map(Value::from_json).collect();
    let expected = "姓名  value\n----  -----\n张三  1    \ne\u{301}     2    \n🦀    3    \n";
    assert_eq!(jsh::value::render_table(&typed), expected);
    assert_eq!(jsh::structured::to_table(records), expected);
}

#[test]
fn inclusive_range_handles_maximum_integer_without_overflow() {
    let out = VALUE_BUILTINS["range"](
        PipelineData::Empty,
        &["9223372036854775806..9223372036854775807".to_owned()],
        &mut ShellState::new(false),
    )
    .unwrap()
    .into_values()
    .unwrap();
    assert_eq!(out, vec![Value::Int(i64::MAX - 1), Value::Int(i64::MAX)]);
}

#[test]
fn skip_preserves_laziness_and_take_only_consumes_requested_values() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let consumed = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&consumed);
    let stream = (0..100).map(move |n| {
        counter.fetch_add(1, Ordering::Relaxed);
        Value::Int(n)
    });
    let mut state = ShellState::new(false);
    let skipped = VALUE_BUILTINS["skip"](
        PipelineData::Stream(Box::new(stream)),
        &["3".to_owned()],
        &mut state,
    )
    .unwrap();
    assert!(skipped.is_stream());
    assert_eq!(consumed.load(Ordering::Relaxed), 0);
    let out = VALUE_BUILTINS["take"](skipped, &["2".to_owned()], &mut state)
        .unwrap()
        .into_values()
        .unwrap();
    assert_eq!(out, vec![Value::Int(3), Value::Int(4)]);
    assert_eq!(consumed.load(Ordering::Relaxed), 5);
}

#[test]
fn unique_keeps_integer_and_float_types_but_normalizes_float_zero_sign() {
    let out = run("unique", json!([1, 1.0, 1.0, -0.0, 0.0]), &[]);
    assert_eq!(out.len(), 3);
    assert!(matches!(out[0], Value::Int(1)));
    assert!(matches!(out[1], Value::Float(n) if n == 1.0));
    assert!(matches!(out[2], Value::Float(n) if n == 0.0));
}
