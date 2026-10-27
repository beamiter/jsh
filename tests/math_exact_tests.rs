//! `math product` and `math avg` keep integer precision, matching the exact
//! integer rules `math sum` already follows.
use std::process::{Command, Stdio};

fn run(script: &str) -> (String, String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", script])
        .stdin(Stdio::null())
        .output()
        .expect("spawn jsh");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

fn math(json: &str, op: &str) -> (String, String, i32) {
    run(&format!("echo '{json}' | from-json | math {op}"))
}

#[test]
fn integer_product_is_exact_beyond_two_to_the_fifty_three() {
    // The former f64 fold rounded this to 9007199254740992.
    assert_eq!(
        math("[9007199254740993, 1]", "product").0,
        "9007199254740993\n"
    );
    assert_eq!(math("[3, 4, 5]", "product").0, "60\n");
    assert_eq!(math("[-3, 4]", "product").0, "-12\n");
}

#[test]
fn integer_product_outside_i64_is_an_error_like_sum() {
    let (out, err, code) = math("[3037000500, 3037000500]", "product");
    assert_eq!(out, "");
    assert_eq!(code, 1);
    assert!(err.contains("outside the i64 range"), "stderr: {err}");
}

#[test]
fn a_zero_factor_wins_even_after_an_intermediate_overflow() {
    let huge = "9223372036854775807";
    let json = format!("[{huge}, {huge}, {huge}, 0]");
    assert_eq!(math(&json, "product").0, "0\n");
}

#[test]
fn a_float_factor_switches_to_floating_point() {
    assert_eq!(math("[2, 2.5]", "product").0, "5\n");
    assert_eq!(math("[3, 0.5]", "product").0, "1.5\n");
}

#[test]
fn product_reads_a_field_from_records() {
    let (out, _, code) = run(r#"echo '[{"n":2},{"n":21}]' | from-json | math product n"#);
    assert_eq!((out.as_str(), code), ("42\n", 0));
}

#[test]
fn integer_mean_is_exact_when_whole() {
    // The f64 fold rounded each partial sum; an exact total divides once.
    assert_eq!(
        math("[9007199254740993, 9007199254740995]", "avg").0,
        "9007199254740994\n"
    );
    assert_eq!(math("[2, 4]", "avg").0, "3\n");
    assert_eq!(math("[1, 2]", "avg").0, "1.5\n");
    assert_eq!(math("[1, 2]", "mean").0, "1.5\n");
}

#[test]
fn mean_of_mixed_input_keeps_floating_point() {
    assert_eq!(math("[1, 2.5]", "avg").0, "1.75\n");
}

#[test]
fn streamed_integer_product_does_not_retain_rows() {
    let (out, _, code) = run("range 1..20 | math product");
    assert_eq!((out.as_str(), code), ("2432902008176640000\n", 0));
}
