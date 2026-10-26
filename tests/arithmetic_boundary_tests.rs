//! Shell arithmetic must wrap at integer boundaries, never panic in debug or
//! release builds. Compare complete command results with Bash's word arithmetic.
use std::process::{Command, Stdio};

fn assert_matches_bash(script: &str) {
    let run = |shell: &str| {
        Command::new(shell)
            .args(["-c", script])
            .stdin(Stdio::null())
            .output()
            .expect("run shell")
    };
    let expected = run("/bin/bash");
    assert!(expected.status.success(), "Bash fixture failed: {script}");
    let actual = run(env!("CARGO_BIN_EXE_jsh"));
    assert!(
        actual.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(actual.stderr, expected.stderr, "{script}");
    assert_eq!(actual.stdout, expected.stdout, "{script}");
}

#[test]
fn arithmetic_integer_boundaries_wrap_like_bash() {
    for expression in [
        "9223372036854775807 + 1",
        "-9223372036854775807 - 2",
        "9223372036854775807 * 2",
        "(-9223372036854775807 - 1) / -1",
        "(-9223372036854775807 - 1) % -1",
        "-(-9223372036854775807 - 1)",
        "9223372036854775808",
        "18446744073709551615",
        "0xffffffffffffffff",
        "01777777777777777777777",
    ] {
        assert_matches_bash(&format!("printf '%s\\n' \"$(({expression}))\""));
    }
}

#[test]
fn shifts_mask_the_count_like_bash() {
    for value in ["1", "-1", "-9223372036854775808"] {
        for operator in ["<<", ">>"] {
            for count in [0, 1, 63, 64, 65, -1, -64, -65] {
                assert_matches_bash(&format!(
                    "printf '%s\\n' \"$(({value} {operator} {count}))\""
                ));
            }
        }
    }
}

#[test]
fn arithmetic_assignment_and_increment_boundaries_wrap_like_bash() {
    for (initial, expression) in [
        ("9223372036854775807", "x += 1"),
        ("-9223372036854775808", "x -= 1"),
        ("9223372036854775807", "x *= 2"),
        ("-9223372036854775808", "x /= -1"),
        ("-9223372036854775808", "x %= -1"),
        ("9223372036854775807", "++x"),
        ("-9223372036854775808", "--x"),
        ("9223372036854775807", "x++"),
        ("-9223372036854775808", "x--"),
    ] {
        assert_matches_bash(&format!(
            "x={initial}; printf '%s:%s\\n' \"$(({expression}))\" \"$x\""
        ));
    }
}

#[test]
fn logical_short_circuit_preserves_side_effects_and_consumes_complete_chains() {
    for expression in [
        "1 || x++",
        "0 && x++",
        "1 || ++x",
        "0 && --x",
        "1 || (x = 9)",
        "0 && (x += 9)",
        "1 || (x /= 0)",
        "0 && (x %= 0)",
        "1 || (1 / 0)",
        "0 && (1 / 0)",
        "1 || x++ || y++",
        "0 && x++ && y++",
        "0 && x++ || ++y",
        "1 || x++ && ++y",
        "1 || x++ || y++, z = 9",
        "0 && x++ && y++, z = 9",
        "(1 || x++) + 3",
        "(0 && x++) + 3",
        "0 || ++x || ++y",
        "1 && ++x && ++y",
    ] {
        assert_matches_bash(&format!(
            "x=0; y=0; z=0; printf '%s:%s:%s:%s\\n' \"$(({expression}))\" \"$x\" \"$y\" \"$z\""
        ));
    }
    assert_matches_bash("bad=08; printf '%s:%s' \"$((1 || bad))\" \"$((0 && bad))\"");
}

#[test]
fn ternary_evaluates_only_selected_branches_with_correct_associativity() {
    for expression in [
        "1 ? ++x : ++y",
        "0 ? ++x : ++y",
        "1 ? (x = 5) : (y = 6)",
        "0 ? (x = 5) : (y = 6)",
        "1 ? 7 : 1 / 0",
        "0 ? 1 / 0 : 7",
        "1 ? 7 : (x /= 0)",
        "0 ? (x %= 0) : 7",
        "1 ? 2, 3 : 4",
        "0 ? 2, 3 : 4",
        "0 ? 1 : 1 ? ++x : ++y",
        "1 ? 0 ? ++x : ++y : ++z",
        "0 ? 1 : 0 ? ++x : ++y",
        "1 ? (x = 1, y = 2, x + y) : ++z",
        "1 ? (0 && ++x) : ++y",
        "0 ? ++x : (1 || ++y)",
    ] {
        assert_matches_bash(&format!(
            "x=0; y=0; z=0; printf '%s:%s:%s:%s\\n' \"$(({expression}))\" \"$x\" \"$y\" \"$z\""
        ));
    }
}

#[test]
fn equality_binds_less_tightly_than_relational_operators() {
    for expression in [
        "2 == 1 < 2",
        "2 != 1 < 2",
        "0 == 2 > 1",
        "1 != 2 >= 3",
        "1 < 2 == 2 < 3",
        "1 < 2 != 2 < 3",
        "1 == 2 < 3 == 1",
        "3 > 2 > 0",
        "1 == 2 == 0",
        "8 >> 1 == 1 + 3",
        "1 & 2 == 2",
        "2 == 1 < 2 ? 4 : 5",
    ] {
        assert_matches_bash(&format!("printf '%s\\n' \"$(({expression}))\""));
    }
}

#[test]
fn numeric_variables_use_the_same_radix_and_full_width_as_literals() {
    for value in [
        "18446744073709551615",
        "18446744073709551616",
        "0xffffffffffffffff",
        "010",
        "-0xffffffffffffffff",
        "+077",
        " 42 ",
        "",
    ] {
        assert_matches_bash(&format!(
            "x='{value}'; printf '%s:%s:%s\\n' \"$((x))\" \"$((x + 1))\" \"$((++x))\""
        ));
    }
}

#[test]
fn malformed_arithmetic_is_reported_even_in_skipped_branches() {
    for expression in [
        "1 || @",
        "0 && (2 + )",
        "1 ? 2 : @",
        "1 ? 2",
        "1 ? 2 :",
        "1 +",
        "1 2",
        "(1 + 2",
        "1 || 08",
        "1 ? 2 : 08",
        "1 / 0",
        "1 && (1 / 0)",
        "0 || (1 / 0)",
        "1 ? 1 / 0 : 7",
        "0 ? 7 : 1 / 0",
    ] {
        let script = format!("printf '%s' \"$(({expression}))\"; echo unreachable");
        let run = |shell: &str| {
            Command::new(shell)
                .args(["-c", &script])
                .stdin(Stdio::null())
                .output()
                .unwrap()
        };
        let expected = run("/bin/bash");
        let actual = run(env!("CARGO_BIN_EXE_jsh"));
        assert!(
            !expected.status.success(),
            "invalid Bash fixture: {expression}"
        );
        assert_eq!(
            actual.status.code(),
            expected.status.code(),
            "{expression}: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(actual.stdout, expected.stdout, "{expression}");
        assert!(!actual.stderr.is_empty(), "{expression}");
    }
}

#[test]
fn arithmetic_command_and_for_loop_errors_do_not_leak_into_later_commands() {
    for script in [
        "((1 / 0)); printf 'after:%s' \"$?\"",
        "for ((x=1/0;0;x++)); do :; done; printf 'after:%s' \"$?\"",
        "for ((x=0;1/0;x++)); do :; done; printf 'after:%s' \"$?\"",
        "for ((x=0;x<2;x=1/0)); do echo body; done; printf 'after:%s' \"$?\"",
    ] {
        let run = |shell: &str| {
            Command::new(shell)
                .args(["-c", script])
                .stdin(Stdio::null())
                .output()
                .unwrap()
        };
        let expected = run("/bin/bash");
        let actual = run(env!("CARGO_BIN_EXE_jsh"));
        assert_eq!(actual.status.code(), expected.status.code(), "{script}");
        assert_eq!(actual.stdout, expected.stdout, "{script}");
        assert!(!actual.stderr.is_empty(), "{script}");
    }
    assert_matches_bash(
        "((4294967296)); printf '%s' \"$?\"; for ((x=4294967296;x;x=0)); do echo body; done",
    );
}

#[test]
fn nested_parentheses_survive_quoted_unquoted_and_multiline_extraction() {
    for script in [
        "printf '<%s>' $(((1 + 2) * (3 + 4)))",
        "printf '<%s>' \"$(((1 + 2) * (3 + 4)))\"",
        "printf '<%s>' prefix$(((1 + 2) * (3 + 4)))suffix",
        "printf '<%s>' \"prefix$(((1 + 2) * (3 + 4)))suffix\"",
        "printf '<%s>' \"$(((1 +\n2) * (3 +\n4)))\"",
        "printf '<%s><%s>' \"$(((((7)))))\" \"$((8))\"",
        "printf '<%s>' \"$((1 + \\\n2))\"",
    ] {
        assert_matches_bash(script);
    }
}
