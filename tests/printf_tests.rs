use std::process::{Command, Output};

fn run(shell: &str, command: &str, args: &[&str]) -> Output {
    let mut script = command.to_owned();
    for argument in args {
        script.push_str(" '");
        script.push_str(&argument.replace('\'', "'\\''"));
        script.push('\'');
    }
    Command::new(shell)
        .args(["-c", &script])
        .env("LC_ALL", "C.UTF-8")
        .output()
        .expect("run printf fixture")
}

fn matches_bash(command: &str, args: &[&str]) {
    let expected = run("/bin/bash", command, args);
    let actual = run(env!("CARGO_BIN_EXE_jsh"), command, args);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "{args:?}: {:?}",
        actual.stderr
    );
    assert_eq!(actual.stdout, expected.stdout, "{args:?}");
    assert_eq!(
        actual.stderr.is_empty(),
        expected.stderr.is_empty(),
        "{args:?}"
    );
}

#[test]
fn numeric_width_precision_and_flags_match_bash() {
    matches_bash(
        "printf",
        &[
            "[%+08d][%#x][%#o][%.2f][%10.3e][%.4g]",
            "42",
            "255",
            "9",
            "1.25",
            "1.5",
            "1.23456",
        ],
    );
    matches_bash(
        "printf",
        &[
            "[%08.4d][%-8.3s][% d][%X][%u]",
            "42",
            "abcdef",
            "7",
            "0xff",
            "-1",
        ],
    );
}

#[test]
fn dynamic_fields_and_format_reuse_match_bash() {
    matches_bash(
        "printf",
        &["[%*.*f][%s]\n", "8", "2", "1.25", "ok", "-8", "1", "7"],
    );
    matches_bash("printf", &["[%.*s][%s]", "-1", "hello"]);
    matches_bash("printf", &["plain %% text", "ignored", "arguments"]);
    matches_bash("printf", &["%d:%f:%s"]);
}

#[test]
fn numeric_bases_and_character_constants_match_bash() {
    matches_bash(
        "printf",
        &["%d:%i:%u:%o:%x", "0x10", "010", "-1", "16", "255"],
    );
    matches_bash("printf", &["%d:%d:%f", "'A", "\"Z", "'B"]);
    matches_bash(
        "printf",
        &["%d:%u:%f", " 12", "0xffffffffffffffff", "0x1.8p+2"],
    );
    matches_bash("printf", &["[%*s]", "0x5", "x"]);
    matches_bash("printf", &["%d:%f", "12x", "1.2abc"]);
    matches_bash("printf", &["%d", "18446744073709551615"]);
}

#[test]
fn format_and_argument_escapes_preserve_bytes() {
    matches_bash("printf", &[r"\101|\0101|\x41|\u4e2d|\U0001f642|\xff"]);
    matches_bash("printf", &["%b", r"\1\01\001\0001\x41\u4e2d\xff"]);
    matches_bash("printf", &[r"a\cb"]);
    matches_bash("printf", &["%bTAIL%s", r"a\cb", "unreachable"]);
}

#[test]
fn echo_combined_flags_and_escape_termination_match_bash() {
    matches_bash("echo", &["-nne", r"one\ttwo\cignored", "unreachable"]);
    matches_bash("echo", &["-eE", r"literal\n"]);
    matches_bash("echo", &["-e", r"\0101|\x41|\u4e2d|\xff|\1"]);
    matches_bash("echo", &["-not-a-flag", "text"]);
}

#[test]
fn malformed_conversions_and_resource_extremes_fail_without_panicking() {
    for args in [
        vec!["%"],
        vec!["%n", "0"],
        vec!["%1048577d", "1"],
        vec!["%.999999999999999999999999f", "1"],
        vec!["%*s", "-9223372036854775808", "x"],
    ] {
        let output = run(env!("CARGO_BIN_EXE_jsh"), "printf", &args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: {:?}",
            output.stderr
        );
        assert!(!output.stderr.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    }
}

#[test]
fn invalid_numbers_report_failure_and_missing_numbers_default_to_zero() {
    let output = run(
        env!("CARGO_BIN_EXE_jsh"),
        "printf",
        &["%d|%.1f", "bad", "bad"],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"0|0.0");
    assert!(!output.stderr.is_empty());
    matches_bash("printf", &["%d|%.1f"]);
}

#[test]
fn printf_option_separator_and_missing_format_match_bash() {
    matches_bash("printf", &["--", "-%s", "value"]);
    matches_bash("printf", &[]);
    matches_bash("printf", &["-invalid"]);
}

#[test]
fn character_conversion_preserves_a_complete_unicode_character() {
    let output = run(
        env!("CARGO_BIN_EXE_jsh"),
        "printf",
        &["%c:%c", "中文", "🙂ok"],
    );
    assert!(output.status.success());
    assert_eq!(output.stdout, "中:🙂".as_bytes());
}
