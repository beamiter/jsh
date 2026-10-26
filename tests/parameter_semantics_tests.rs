//! Regression coverage for expansion evaluation order and UTF-8 semantics.
use std::process::{Command, Stdio};

fn run(script: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["-c", script])
        .stdin(Stdio::null())
        .output()
        .expect("run jsh");
    assert!(
        output.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{script}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn unused_parameter_operands_do_not_run_commands() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("executed");
    for expression in [
        "${v:-WORD}",
        "${v-WORD}",
        "${v:=WORD}",
        "${missing:+WORD}",
        "${missing+WORD}",
        "${empty:+WORD}",
    ] {
        let expression = expression.replace("WORD", &format!("$(touch '{}')", marker.display()));
        run(&format!(
            "v=present; empty=''; unset missing; printf '%s' \"{expression}\""
        ));
        assert!(
            !marker.exists(),
            "unused operand was evaluated: {expression}"
        );
    }
}

#[test]
fn selected_parameter_operands_are_evaluated() {
    assert_eq!(
        run(
            r#"unset absent; empty=''; v=present; printf '%s|%s|%s|%s|%s' "${absent:-$(printf default)}" "${empty:=$(printf assigned)}" "$empty" "${v:+$(printf alternate)}" "${empty+$(printf set)}""#
        ),
        "default|assigned|assigned|alternate|set"
    );
}

#[test]
fn parameter_lengths_and_substrings_count_unicode_characters() {
    assert_eq!(
        run(
            r#"v='甲é🙂z'; a=('甲é🙂z'); printf '%s|%s|%s|%s|%s|%s|%s' "${#v}" "${#a[0]}" "${v:1:2}" "${v: -2}" "${v:0:-1}" "${v:9}" "${v:1:9223372036854775807}""#
        ),
        "4|4|é🙂|🙂z|甲é🙂||é🙂z"
    );
}

#[test]
fn extended_globs_work_in_unicode_directories() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("中文");
    std::fs::create_dir(&directory).unwrap();
    for name in ["one.txt", "two.txt", "other.log"] {
        std::fs::write(directory.join(name), "").unwrap();
    }
    assert_eq!(
        run(&format!(
            "shopt -s extglob; printf '%s\\n' {}/@(one|two).txt",
            directory.display()
        )),
        format!(
            "{}/one.txt\n{}/two.txt\n",
            directory.display(),
            directory.display()
        )
    );
}

#[test]
fn command_substitution_strips_newlines_but_preserves_carriage_returns() {
    assert_eq!(
        run(r#"x=$(printf 'first\r\nsecond\r\n\n'); printf '%s' "$x""#),
        "first\r\nsecond\r"
    );
    assert_eq!(run(r#"x=$(printf '\r'); printf '<%s>' "$x""#), "<\r>");
    assert_eq!(run(r#"x=$(printf '\n\n'); printf '<%s>' "$x""#), "<>");
}
