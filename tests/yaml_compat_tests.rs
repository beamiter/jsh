//! Behavior preserved while replacing the unmaintained YAML parser backend.
use jsh::environment::ShellState;
use jsh::pipeline_data::PipelineData;
use jsh::value::Value;
use jsh::value_builtins::VALUE_BUILTINS;
use serde_json::{json, Value as Json};

fn convert(name: &str, input: PipelineData) -> Result<PipelineData, i32> {
    VALUE_BUILTINS[name](input, &[], &mut ShellState::new(false))
}

fn parse(input: &str) -> Json {
    let values = convert("from-yaml", PipelineData::Bytes(input.as_bytes().to_vec()))
        .expect("valid YAML")
        .into_values()
        .unwrap();
    Json::Array(values.iter().map(Value::to_json).collect())
}

#[test]
fn yaml_keeps_field_order_at_every_depth() {
    let value = parse("z: {last: 3, first: 1}\na: 2\nm: [{right: 2, left: 1}]");
    assert_eq!(
        value[0].as_object().unwrap().keys().collect::<Vec<_>>(),
        ["z", "a", "m"]
    );
    assert_eq!(
        value[0]["z"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["last", "first"]
    );
    assert_eq!(
        value[0]["m"][0]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["right", "left"]
    );
}

#[test]
fn yaml_keeps_legacy_implicit_scalar_types() {
    assert_eq!(
        parse("[true, True, FALSE, yes, NO, on, off, tRuE, null, Null, NULL, nUlL, ~]"),
        json!([
            true, true, false, "yes", "NO", "on", "off", "tRuE", null, null, null, "nUlL", null
        ])
    );
    assert_eq!(
        parse("[01, -012, +01, 1_000, 0xFF, 0o12, 0b101, 1e3, 1e999]"),
        json!(["01", "-012", "+01", "1_000", 255, 10, 5, 1000.0, "1e999"])
    );
}

#[test]
fn yaml_quoted_scalars_and_binary_payloads_remain_strings() {
    assert_eq!(
        parse("['null', 'true', '.nan', '01', !!str 123, !!binary SGVsbG8=, !!binary /w==]"),
        json!(["null", "true", ".nan", "01", "123", "SGVsbG8=", "/w=="])
    );
}

#[test]
fn yaml_nonfinite_values_still_become_null() {
    assert_eq!(
        parse("[.nan, .NaN, .inf, -.Inf, +.INF, !!float .nan, !!float 1]"),
        json!([null, null, null, null, null, null, 1.0])
    );
}

#[test]
fn yaml_integer_boundaries_keep_shell_number_semantics() {
    let values = convert(
        "from-yaml",
        PipelineData::Bytes(
            b"[-9223372036854775808, 9223372036854775807, 18446744073709551615]".to_vec(),
        ),
    )
    .unwrap()
    .into_values()
    .unwrap();
    assert!(matches!(values[0], Value::Int(i64::MIN)));
    assert!(matches!(values[1], Value::Int(i64::MAX)));
    assert!(matches!(values[2], Value::Float(value) if value == u64::MAX as f64));
}

#[test]
fn yaml_aliases_keep_scalar_types_and_merge_keys_are_ordinary_fields() {
    let value = parse(
        "标题: &number 012\ncopy: *number\ntagged: &tagged !kind 01\nagain: *tagged\nbase: &base {b: 2, a: 1}\nrecord:\n  <<: *base\n  own: yes\n",
    );
    assert_eq!(
        value,
        json!([{
            "标题": "012", "copy": "012", "tagged": "01", "again": "01",
            "base": {"b": 2, "a": 1},
            "record": {"<<": {"b": 2, "a": 1}, "own": "yes"}
        }])
    );
}

#[test]
fn yaml_scalar_and_complex_keys_keep_their_text_conversion() {
    let value = parse(
        "1: one\ntrue: boolean\nnull: empty\n1.5: decimal\n.nan: nonfinite\n? [a, b]\n: sequence\n? {b: 2, a: 1}\n: mapping\n? !kind 123\n: tagged\n",
    );
    assert_eq!(
        value,
        json!([{
            "1": "one", "true": "boolean", "null": "empty", "1.5": "decimal",
            ".nan": "nonfinite", "- a\n- b": "sequence", "b: 2\na: 1": "mapping",
            "!kind 123": "tagged"
        }])
    );
}

#[test]
fn yaml_rejects_ambiguous_keys_after_json_name_conversion() {
    // The old adapter silently overwrote distinct YAML keys with the same
    // JSON name. Reject both these collisions and differently spelled keys
    // that normalize to one value but escape the parser's duplicate check.
    for input in [
        "1: before\nother: 3\n'1': after",
        "true: before\nTrue: after",
        "1.5: before\n1.50: after",
    ] {
        assert!(convert("from-yaml", PipelineData::Bytes(input.as_bytes().to_vec())).is_err());
    }
}

#[test]
fn yaml_round_trip_keeps_nested_values_and_field_order() {
    let input = json!([{
        "z": ["yes", "01", "1_000", "tRuE", "1e999", ".nan", "null", "true"],
        "a": {"second": null, "first": true},
        "multiline": "first\nsecond\n",
        "n": 9223372036854775807_i64,
        "float": 1.25
    }, {"last": false, "first": -1}]);
    let values = input
        .as_array()
        .unwrap()
        .iter()
        .cloned()
        .map(Value::from_json)
        .collect();
    let yaml = convert("to-yaml", PipelineData::Values(values)).unwrap();
    let output = convert("from-yaml", yaml).unwrap().into_values().unwrap();
    let output = Json::Array(output.iter().map(Value::to_json).collect());
    // Serialized JSON observes insertion order as well as value equality.
    assert_eq!(
        serde_json::to_string(&output).unwrap(),
        serde_json::to_string(&input).unwrap()
    );
}

#[test]
fn yaml_empty_scalar_and_sequence_shapes_are_unchanged() {
    assert_eq!(parse(""), json!([null]));
    assert_eq!(parse("# comment only\n"), json!([null]));
    assert_eq!(parse("hello"), json!(["hello"]));
    assert_eq!(parse("[]"), json!([]));
    assert_eq!(parse("[1, [2, 3], {a: 4}]"), json!([1, [2, 3], {"a": 4}]));
}

#[test]
fn yaml_invalid_documents_fail_without_producing_partial_values() {
    for input in [
        b"a: [1, 2".as_slice(),
        b"a: 1\na: 2",
        b"---\na\n---\nb",
        b"!!int wrong",
        b"*unknown",
        b"a: \xff",
    ] {
        assert!(
            convert("from-yaml", PipelineData::Bytes(input.to_vec())).is_err(),
            "accepted invalid input: {input:?}"
        );
    }
}

#[test]
fn yaml_limits_reject_recursive_aliases_excessive_aliases_and_depth() {
    // Parsing must reach the depth error before exhausting a normal worker
    // stack, even if the surrounding test runner uses a larger stack.
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let at_limit = format!("{}0{}", "[".repeat(32), "]".repeat(32));
            assert!(convert("from-yaml", PipelineData::Bytes(at_limit.into_bytes())).is_ok());
            let over_limit = format!("{}0{}", "[".repeat(33), "]".repeat(33));
            let too_deep = format!("{}0{}", "[".repeat(70), "]".repeat(70));
            let too_many_aliases =
                format!("item: &item 1\nrefs: [{}]", vec!["*item"; 101].join(","));
            for input in [
                "root: &root [*root]".to_owned(),
                over_limit,
                too_deep,
                too_many_aliases,
            ] {
                assert!(
                    convert("from-yaml", PipelineData::Bytes(input.into_bytes())).is_err(),
                    "parser limits must return an error"
                );
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn yaml_tags_do_not_expand_files_or_environment_variables() {
    assert_eq!(
        parse("[!include /definitely-missing-jsh-yaml-file, '${HOME}', !custom value]"),
        json!(["/definitely-missing-jsh-yaml-file", "${HOME}", "value"])
    );
}

#[test]
fn yaml_timestamp_tag_has_an_explicit_error_boundary() {
    assert_eq!(parse("date: 2026-10-26"), json!([{"date": "2026-10-26"}]));
    assert!(convert(
        "from-yaml",
        PipelineData::Bytes(b"!!timestamp 2026-10-26".to_vec())
    )
    .is_err());
}
