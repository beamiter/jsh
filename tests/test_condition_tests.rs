use std::process::Command;

fn run_condition(name: &str, args: &[&str]) -> std::process::Output {
    let mut script = name.to_string();
    for arg in args {
        script.push_str(" '");
        script.push_str(&arg.replace('\'', "'\\''"));
        script.push('\'');
    }
    Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["-c", &script])
        .output()
        .expect("run jsh condition")
}

#[test]
fn bracket_requires_one_final_delimiter_and_preserves_literal_brackets() {
    for (name, args, status) in [
        ("test", vec!["]"], 0),
        ("test", vec!["]", "=", "]"], 0),
        ("[", vec![], 2),
        ("[", vec!["value"], 2),
        ("[", vec!["]"], 1),
        ("[", vec!["]", "]"], 0),
        ("[", vec!["]", "=", "]", "]"], 0),
        ("test", vec!["value", "]"], 2),
    ] {
        let output = run_condition(name, &args);
        assert_eq!(output.status.code(), Some(status), "{name} {args:?}");
        if status == 2 {
            assert!(!output.stderr.is_empty(), "missing diagnostic: {args:?}");
        }
    }
}

#[test]
fn operator_shaped_strings_obey_small_argument_rules() {
    for (args, status) in [
        (vec![], 1),
        (vec![""], 1),
        (vec!["!"], 0),
        (vec!["("], 0),
        (vec!["-n"], 0),
        (vec!["!", ""], 0),
        (vec!["!", "!"], 1),
        (vec!["!", "=", "!"], 0),
        (vec!["-n", "=", "-n"], 0),
        (vec!["(", "value", ")"], 0),
        (vec!["(", "", ")"], 1),
        (vec!["!", "(", "value", ")"], 1),
        (vec!["value", "-a", "other"], 0),
        (vec!["", "-o", "other"], 0),
        (vec!["", "-a", "other"], 1),
        (vec!["!", "-o", "x"], 0),
        (vec!["-n", "-a", ""], 1),
        (vec!["x", "-a", "!"], 0),
        (vec!["!", "-a", ""], 1),
        (vec!["!", "-z", ""], 1),
    ] {
        let output = run_condition("test", &args);
        assert_eq!(output.status.code(), Some(status), "test {args:?}");
        assert!(
            output.stderr.is_empty(),
            "test {args:?}: {:?}",
            output.stderr
        );
    }
}

#[test]
fn invalid_operands_are_never_accepted_as_successful_conditions() {
    for args in [
        vec!["x", "=", "x", "extra"],
        vec!["x", "=", "x", "-o"],
        vec!["", "=", "x", "-a"],
        vec!["x", "=", "x", "-o", "bad", "-eq", "1"],
        vec!["", "=", "x", "-a", "bad", "-eq", "1"],
        vec!["(", "x", "=", "x"],
        vec!["!", "!", "x"],
    ] {
        let output = run_condition("test", &args);
        assert_eq!(output.status.code(), Some(2), "test {args:?}");
        assert!(!output.stderr.is_empty(), "missing diagnostic: {args:?}");
    }
}
