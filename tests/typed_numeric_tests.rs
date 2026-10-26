use jsh::closure_expr::try_eval;
use jsh::value::Value;
use std::collections::HashMap;

fn eval(source: &str, vars: &HashMap<String, Value>) -> Result<Value, String> {
    try_eval(source, vars)?.ok_or_else(|| "not an expression".into())
}

#[test]
fn typed_integer_literals_and_operations_remain_exact_above_float_precision() {
    let vars = HashMap::new();
    for (source, expected) in [
        ("9007199254740993 + 0", 9_007_199_254_740_993),
        ("9007199254740993 + 2", 9_007_199_254_740_995),
        ("9007199254740993 - 2", 9_007_199_254_740_991),
        ("3002399751580331 * 3", 9_007_199_254_740_993),
        ("9007199254740993 % 2", 1),
        ("9007199254740993 / 3", 3_002_399_751_580_331),
        ("-9223372036854775808", i64::MIN),
    ] {
        assert_eq!(
            eval(source, &vars).unwrap(),
            Value::Int(expected),
            "{source}"
        );
    }
    assert_eq!(eval("5 / 2", &vars).unwrap(), Value::Float(2.5));
}

#[test]
fn typed_integer_overflow_is_an_error_not_a_panic_or_rounded_value() {
    let vars = HashMap::from([("min".to_owned(), Value::Int(i64::MIN))]);
    for source in [
        "9223372036854775807 + 1",
        "-9223372036854775808 - 1",
        "9223372036854775807 * 2",
        "-$min",
        "$min / -1",
        "9223372036854775808 + 0",
        "match 0 {18446744073709551616 => 1, _ => 0}",
    ] {
        let result = std::panic::catch_unwind(|| eval(source, &vars));
        assert!(result.is_ok(), "{source} panicked");
        assert!(result.unwrap().is_err(), "{source} silently succeeded");
    }
    assert_eq!(eval("$min % -1", &vars).unwrap(), Value::Int(0));
}

#[test]
fn comparisons_do_not_round_large_integers() {
    let vars = HashMap::from([
        ("a".to_owned(), Value::Int(9_007_199_254_740_993)),
        ("b".to_owned(), Value::Int(9_007_199_254_740_992)),
        ("f".to_owned(), Value::Float(9_007_199_254_740_992.0)),
    ]);
    for (source, expected) in [
        ("$a > $b", true),
        ("$a > $f", true),
        ("$a == $f", false),
        ("$b == $f", true),
        ("$f < $a", true),
    ] {
        assert_eq!(
            eval(source, &vars).unwrap(),
            Value::Bool(expected),
            "{source}"
        );
    }
}

#[test]
fn repeated_compiled_expressions_resolve_current_arguments() {
    for n in 0..100 {
        let vars = HashMap::from([("x".to_owned(), Value::Int(n))]);
        assert_eq!(
            eval("($x + 2) * ($x - 1)", &vars).unwrap(),
            Value::Int((n + 2) * (n - 1))
        );
    }
}

#[test]
fn expression_strings_keep_shell_characters_after_escaped_quotes() {
    assert_eq!(
        eval(r#""a\";|&b" + "!""#, &HashMap::new()).unwrap(),
        Value::String("a\";|&b!".into())
    );
}

#[test]
fn mixed_numeric_comparisons_handle_boundaries_and_unordered_values() {
    for (integer, float, expected) in [
        (
            i64::MAX,
            9_223_372_036_854_775_808.0,
            Some(std::cmp::Ordering::Less),
        ),
        (
            i64::MIN,
            -9_223_372_036_854_775_808.0,
            Some(std::cmp::Ordering::Equal),
        ),
        (-1, -1.5, Some(std::cmp::Ordering::Greater)),
        (0, -0.0, Some(std::cmp::Ordering::Equal)),
        (0, f64::INFINITY, Some(std::cmp::Ordering::Less)),
        (0, f64::NEG_INFINITY, Some(std::cmp::Ordering::Greater)),
        (0, f64::NAN, None),
    ] {
        let vars = HashMap::from([
            ("i".into(), Value::Int(integer)),
            ("f".into(), Value::Float(float)),
        ]);
        for (op, reverse, matches) in [
            ("<", ">", expected.is_some_and(std::cmp::Ordering::is_lt)),
            (">", "<", expected.is_some_and(std::cmp::Ordering::is_gt)),
            ("<=", ">=", expected.is_some_and(std::cmp::Ordering::is_le)),
            (">=", "<=", expected.is_some_and(std::cmp::Ordering::is_ge)),
            ("==", "==", expected == Some(std::cmp::Ordering::Equal)),
        ] {
            assert_eq!(
                eval(&format!("$i {op} $f"), &vars).unwrap(),
                Value::Bool(matches)
            );
            assert_eq!(
                eval(&format!("$f {reverse} $i"), &vars).unwrap(),
                Value::Bool(matches)
            );
        }
    }
    assert_ne!(
        Value::List(vec![Value::Int(9_007_199_254_740_993)]),
        Value::List(vec![Value::Float(9_007_199_254_740_992.0)])
    );
}

#[test]
fn closure_scopes_preserve_parameter_and_capture_precedence() {
    use jsh::environment::ShellState;
    use jsh::value::ClosureData;
    let mut state = ShellState::new(false);
    let mut closure = ClosureData {
        params: vec!["x".into()],
        body_src: "$x".into(),
        captured: HashMap::from([("x".into(), Value::Int(50)), ("in".into(), Value::Int(60))]),
    };
    assert_eq!(
        jsh::executor::apply_closure(&closure, &[Value::Int(7)], &mut state).unwrap(),
        Value::Int(7)
    );
    assert_eq!(
        jsh::executor::apply_closure(&closure, &[], &mut state).unwrap(),
        Value::Null
    );
    closure.body_src = "$in".into();
    assert_eq!(
        jsh::executor::apply_closure(&closure, &[Value::Int(7)], &mut state).unwrap(),
        Value::Int(60)
    );
    closure.params = vec!["in".into()];
    assert_eq!(
        jsh::executor::apply_closure(&closure, &[Value::Int(7)], &mut state).unwrap(),
        Value::Int(7)
    );
    closure.captured.clear();
    closure.params.clear();
    assert_eq!(
        jsh::executor::apply_closure(&closure, &[Value::Int(7)], &mut state).unwrap(),
        Value::Int(7)
    );
    closure.body_src = "true; false".into();
    assert_eq!(
        jsh::executor::apply_closure(&closure, &[], &mut state).unwrap(),
        Value::Bool(false)
    );
}
