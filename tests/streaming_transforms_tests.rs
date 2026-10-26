use jsh::environment::ShellState;
use jsh::pipeline_data::PipelineData;
use jsh::value::Value;
use jsh::value_builtins::VALUE_BUILTINS;
use serde_json::{json, Value as JsonValue};
use std::process::Command;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

fn apply(name: &str, input: PipelineData, args: &[&str]) -> Result<PipelineData, i32> {
    VALUE_BUILTINS[name](
        input,
        &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
        &mut ShellState::new(false),
    )
}

fn counted(input: JsonValue) -> (PipelineData, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&count);
    let values = input
        .as_array()
        .unwrap()
        .clone()
        .into_iter()
        .map(move |value| {
            counter.fetch_add(1, Ordering::Relaxed);
            Value::from_json(value)
        });
    (PipelineData::Stream(Box::new(values)), count)
}

fn as_json(input: PipelineData) -> Vec<JsonValue> {
    input
        .into_values()
        .unwrap()
        .iter()
        .map(Value::to_json)
        .collect()
}

#[test]
fn mapping_transforms_wait_for_demand_and_preserve_values() {
    let cases = [
        (
            "enumerate",
            vec![],
            json!([7, 8, 9]),
            json!({"index": 0, "item": 7}),
        ),
        ("wrap", vec!["n"], json!([7, 8, 9]), json!({"n": 7})),
        (
            "select",
            vec!["b", "a", "b"],
            json!([{"a": 1, "b": 2, "c": 3}, {"a": 4}]),
            json!({"b": 2, "a": 1}),
        ),
        (
            "reject",
            vec!["b"],
            json!([{"a": 1, "b": 2}, {"a": 4}]),
            json!({"a": 1}),
        ),
        ("default", vec!["42"], json!([null, 1, 2]), json!(42)),
        (
            "default",
            vec!["42", "a"],
            json!([{"a": null}, {"b": 3}]),
            json!({"a": 42}),
        ),
    ];
    for (name, args, values, expected) in cases {
        let (input, count) = counted(values);
        let transformed = apply(name, input, &args).unwrap();
        assert!(transformed.is_stream(), "{name}");
        assert_eq!(count.load(Ordering::Relaxed), 0, "{name}");
        let out = apply("take", transformed, &["1"]).unwrap();
        assert_eq!(as_json(out), vec![expected], "{name}");
        assert_eq!(count.load(Ordering::Relaxed), 1, "{name}");
    }
}

#[test]
fn flatten_pulls_only_required_outer_lists_and_keeps_one_level_semantics() {
    let (input, count) = counted(json!([[], [1, [2, 3]], [4], 5]));
    let flattened = apply("flatten", input, &[]).unwrap();
    assert!(flattened.is_stream());
    assert_eq!(count.load(Ordering::Relaxed), 0);
    let first = apply("take", flattened, &["2"]).unwrap();
    assert_eq!(as_json(first), vec![json!(1), json!([2, 3])]);
    assert_eq!(count.load(Ordering::Relaxed), 2);
}

#[test]
fn compact_filters_incrementally_without_losing_falsy_values() {
    for (args, values, expected, consumed) in [
        (
            vec![],
            json!([null, {"a": null}, 0, false, ""]),
            vec![json!(0), json!(false)],
            4,
        ),
        (
            vec!["a"],
            json!([{}, {"a": null}, {"a": false}, {"a": 0}, {"a": 1}]),
            vec![json!({"a": false}), json!({"a": 0})],
            4,
        ),
    ] {
        let (input, count) = counted(values);
        let compacted = apply("compact", input, &args).unwrap();
        assert!(compacted.is_stream());
        assert_eq!(count.load(Ordering::Relaxed), 0);
        let out = apply("take", compacted, &["2"]).unwrap();
        assert_eq!(as_json(out), expected);
        assert_eq!(count.load(Ordering::Relaxed), consumed);
    }
}

#[test]
fn eager_transforms_keep_values_output_and_report_invalid_byte_input() {
    for (name, args) in [
        ("enumerate", vec![]),
        ("wrap", vec!["n"]),
        ("flatten", vec![]),
        ("compact", vec![]),
        ("select", vec!["n"]),
        ("reject", vec!["n"]),
        ("default", vec!["0"]),
    ] {
        let result = apply(name, PipelineData::Values(vec![Value::Int(1)]), &args).unwrap();
        assert!(matches!(result, PipelineData::Values(_)), "{name}");
        assert!(
            apply(name, PipelineData::Bytes(b"invalid JSON".to_vec()), &args).is_err(),
            "{name}"
        );
    }
}

#[test]
fn row_count_commands_reject_bad_arguments_before_reading_input() {
    for name in ["first", "last", "take", "skip", "drop"] {
        for args in [
            vec!["-1"],
            vec!["abc"],
            vec!["1.5"],
            vec![""],
            vec!["184467440737095516160"],
            vec!["1", "2"],
        ] {
            let (input, count) = counted(json!([1, 2, 3]));
            assert!(apply(name, input, &args).is_err(), "{name} {args:?}");
            assert_eq!(count.load(Ordering::Relaxed), 0, "{name} {args:?}");
        }
    }
}

#[test]
fn row_count_defaults_and_zero_remain_supported() {
    for (name, default, zero) in [
        ("first", json!([1]), json!([])),
        ("last", json!([3]), json!([])),
        ("take", json!([1]), json!([])),
        ("skip", json!([2, 3]), json!([1, 2, 3])),
        ("drop", json!([1, 2]), json!([1, 2, 3])),
    ] {
        for (args, expected) in [(vec![], default), (vec!["0"], zero)] {
            let (input, _) = counted(json!([1, 2, 3]));
            assert_eq!(
                as_json(apply(name, input, &args).unwrap()),
                *expected.as_array().unwrap(),
                "{name}"
            );
        }
    }
}

#[test]
fn command_line_streaming_chain_and_invalid_counts() {
    let out = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["-c", "range 1..1000 | wrap n | default 0 unused | reject unused | select n | compact n | enumerate | flatten | skip 1 | take 2 | to-json"])
        .output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<JsonValue>(&out.stdout).unwrap(),
        json!([
            {"index": 1, "item": {"n": 2}}, {"index": 2, "item": {"n": 3}}
        ])
    );
    for name in ["first", "last", "take", "skip", "drop"] {
        let out = Command::new(env!("CARGO_BIN_EXE_jsh"))
            .args(["-c", &format!("range 1..3 | {name} nope | to-json")])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{name}");
        assert!(out.stdout.is_empty(), "{name}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("count must be a non-negative integer"),
            "{name}"
        );
    }
}

#[test]
fn take_zero_does_not_pull_through_any_lazy_transform() {
    for (name, args) in [
        ("enumerate", vec![]),
        ("wrap", vec!["n"]),
        ("flatten", vec![]),
        ("compact", vec![]),
        ("select", vec!["n"]),
        ("reject", vec!["n"]),
        ("default", vec!["0"]),
        ("skip", vec!["0"]),
    ] {
        let (input, count) = counted(json!([null, {"n": 1}, [2], 3]));
        let transformed = apply(name, input, &args).unwrap();
        let out = apply("take", transformed, &["0"]).unwrap();
        assert!(as_json(out).is_empty(), "{name}");
        assert_eq!(count.load(Ordering::Relaxed), 0, "{name}");
        let empty = apply(name, PipelineData::Empty, &args).unwrap();
        assert!(
            matches!(empty, PipelineData::Values(ref values) if values.is_empty()),
            "{name}"
        );
    }
}

#[test]
fn command_line_take_skip_defaults_and_compact_columns_are_available() {
    for (script, expected) in [
        ("range 1..3 | take | to-json", json!([1])),
        ("range 1..3 | skip | to-json", json!([2, 3])),
        (
            r#"echo '[{}, {"n":null}, {"n":0}, {"n":false}]' | from-json | compact n | to-json"#,
            json!([{"n":0}, {"n":false}]),
        ),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_jsh"))
            .args(["-c", script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<JsonValue>(&out.stdout).unwrap(),
            expected,
            "{script}"
        );
    }
    for command in [
        "enumerate extra",
        "flatten extra",
        "wrap a b",
        "take 1 2",
        "skip 1 2",
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_jsh"))
            .args(["-c", &format!("range 1..3 | {command} | to-json")])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{command}");
        assert!(out.stdout.is_empty(), "{command}");
    }
}

#[test]
fn default_fills_every_selected_column_lazily_and_preserves_existing_values() {
    let (input, count) = counted(json!([
        {"a": null, "b": false, "c": 0},
        {"a": 7},
        {"a": null}
    ]));
    let filled = apply("default", input, &["42", "a", "b", "c", "d", "a"]).unwrap();
    assert!(filled.is_stream());
    assert_eq!(count.load(Ordering::Relaxed), 0);
    let first = apply("take", filled, &["1"]).unwrap();
    assert_eq!(
        as_json(first),
        vec![json!({"a": 42, "b": false, "c": 0, "d": 42})]
    );
    assert_eq!(count.load(Ordering::Relaxed), 1);
    let out = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args([
            "-c",
            r#"echo '[{"a":null,"b":false},{"a":7}]' | from-json | default 42 a b c | to-json"#,
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
        json!([
            {"a":42,"b":false,"c":42}, {"a":7,"b":42,"c":42}
        ])
    );
}
