use indexmap::IndexMap;
use jsh::environment::ShellState;
use jsh::pipeline_data::PipelineData;
use jsh::value::{ClosureData, Value};
use jsh::value_builtins::VALUE_BUILTINS;
use std::collections::HashMap;
use std::sync::Arc;

fn run(
    command: &str,
    values: Vec<Value>,
    params: &[&str],
    body: &str,
    captured: HashMap<String, Value>,
    keep_empty: bool,
    threads: usize,
) -> Result<Vec<Value>, i32> {
    let mut state = ShellState::new(false);
    state.let_vars.insert(
        "f".to_owned(),
        Value::Closure(Arc::new(ClosureData {
            params: params.iter().map(|name| (*name).to_owned()).collect(),
            body_src: body.to_owned(),
            captured,
        })),
    );
    let mut args = vec![];
    if command == "par-each" {
        args.extend(["-t".to_owned(), threads.to_string()]);
    }
    if keep_empty {
        args.push("--keep-empty".to_owned());
    }
    args.push("f".to_owned());
    VALUE_BUILTINS[command](PipelineData::Values(values), &args, &mut state)?.into_values()
}

#[test]
fn par_each_matches_sequential_bindings_for_captures_in_and_missing_parameters() {
    for (params, body, captured) in [
        (
            vec![],
            "$in",
            HashMap::from([("in".to_owned(), Value::Int(7))]),
        ),
        (
            vec!["x"],
            "$in",
            HashMap::from([("in".to_owned(), Value::Int(7))]),
        ),
        (
            vec!["in"],
            "$in",
            HashMap::from([("in".to_owned(), Value::Int(7))]),
        ),
        (
            vec!["x", "y"],
            "$y",
            HashMap::from([("y".to_owned(), Value::Int(99))]),
        ),
        (
            vec!["x", "x"],
            "$x",
            HashMap::from([("x".to_owned(), Value::Int(99))]),
        ),
        (
            vec![],
            "$x + 1",
            HashMap::from([("x".to_owned(), Value::Int(41))]),
        ),
        (vec![], "$in * 2", HashMap::new()),
    ] {
        for keep in [false, true] {
            let input = vec![Value::Int(1), Value::Int(2), Value::Int(3)];
            let expected = run(
                "each",
                input.clone(),
                &params,
                body,
                captured.clone(),
                keep,
                1,
            )
            .unwrap();
            for threads in [1, 2, 20] {
                let output = run(
                    "par-each",
                    input.clone(),
                    &params,
                    body,
                    captured.clone(),
                    keep,
                    threads,
                )
                .unwrap();
                assert_eq!(
                    output, expected,
                    "params={params:?}, body={body}, keep={keep}, threads={threads}"
                );
            }
        }
    }
}

#[test]
fn par_each_matches_sequential_json_literal_decoding() {
    for body in [
        r#""\u4e2d\u6587""#,
        r#"[1,"two",null]"#,
        r#"{"a":1,"b":"\u4e2d"}"#,
        "9223372036854775808",
    ] {
        let input = vec![Value::Int(1), Value::Int(2)];
        let expected = run("each", input.clone(), &["x"], body, HashMap::new(), true, 1).unwrap();
        let actual = run("par-each", input, &["x"], body, HashMap::new(), true, 2).unwrap();
        assert_eq!(actual, expected, "{body}");
    }
}

#[test]
fn par_each_preserves_record_order_and_captured_snapshot() {
    let probe = Arc::new(ClosureData {
        params: vec![],
        body_src: String::new(),
        captured: HashMap::new(),
    });
    let mut config = IndexMap::new();
    config.insert("offset".to_owned(), Value::Int(7));
    config.insert(
        "unused_payload".to_owned(),
        Value::List(vec![Value::Closure(Arc::clone(&probe)); 100]),
    );
    let captured = HashMap::from([("config".to_owned(), Value::Record(config))]);
    let input: Vec<Value> = (0..1000)
        .map(|n| Value::Record(IndexMap::from([("n".to_owned(), Value::Int(n))])))
        .collect();
    let output = run(
        "par-each",
        input,
        &["row"],
        "$row.n + $config.offset",
        captured,
        false,
        4,
    )
    .unwrap();
    assert_eq!(output, (7..1007).map(Value::Int).collect::<Vec<_>>());
    assert_eq!(Arc::strong_count(&probe), 1);
}

#[test]
fn par_each_expression_failures_never_return_partial_or_null_results() {
    for body in [
        "$x / 0",
        "$x + 18446744073709551616",
        "$x + 9223372036854775807",
    ] {
        for command in ["each", "par-each"] {
            let output = run(
                command,
                vec![Value::Int(1); 100],
                &["x"],
                body,
                HashMap::new(),
                true,
                4,
            );
            assert!(output.is_err(), "{command}: {body}");
        }
    }
    let empty = run(
        "par-each",
        vec![],
        &["x"],
        "$x + 18446744073709551616",
        HashMap::new(),
        false,
        4,
    )
    .unwrap();
    assert!(empty.is_empty());
}

#[test]
fn par_each_shell_body_still_uses_sequential_fallback() {
    let input = vec![Value::Int(1), Value::Int(2)];
    let expected = run(
        "each",
        input.clone(),
        &["x"],
        "test $x -gt 0",
        HashMap::new(),
        false,
        1,
    )
    .unwrap();
    let output = run(
        "par-each",
        input,
        &["x"],
        "test $x -gt 0",
        HashMap::new(),
        false,
        4,
    )
    .unwrap();
    assert_eq!(output, expected);
}
