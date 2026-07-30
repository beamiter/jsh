use jsh::parser::{is_incomplete, parse};
use std::process::Command;

fn assert_incomplete(source: &str) {
    assert!(
        is_incomplete(source),
        "input was not incomplete: {source:?}"
    );
    let error = parse(source).expect_err("strict parser accepted incomplete input");
    assert_eq!(error.to_string(), "incomplete input", "source: {source:?}");
}

#[test]
fn strict_parser_rejects_unclosed_quotes_and_substitutions() {
    for source in [
        "echo 'unterminated",
        "echo \"unterminated",
        "echo $'unterminated",
        "echo $(printf hi",
        "echo ${value:-fallback",
        "echo <(printf hi",
        "echo >(printf hi",
        "echo `printf hi",
        "echo $(printf 'unterminated",
    ] {
        assert_incomplete(source);
    }
}

#[test]
fn command_and_process_substitutions_respect_quoted_delimiters() {
    for source in [
        r#"echo $(printf '%s' ")")"#,
        r#"echo <(printf '%s' ")")"#,
        r#"echo >(printf '%s' ")")"#,
        r#"echo ${value:-"}"}"#,
        "echo $(printf ok # \" $( ${ <(\n)\n",
    ] {
        assert!(!is_incomplete(source), "input was incomplete: {source:?}");
        parse(source).unwrap_or_else(|error| panic!("{source:?}: {error}"));
    }
}

#[test]
fn comments_and_here_docs_do_not_activate_embedded_syntax() {
    let comment = "echo ok # ' \" $' $( ${ <( >( | &&";
    assert!(!is_incomplete(comment));
    parse(comment).expect("comment punctuation must remain inert");

    let here_doc = "cat <<'EOF'\n' \" $' $( ${ <( >( | && \\\nEOF\necho done\n";
    assert!(!is_incomplete(here_doc));
    parse(here_doc).expect("here-doc body punctuation must remain inert");
}

#[test]
fn missing_here_doc_delimiter_is_incomplete() {
    assert_incomplete("cat <<EOF\nbody\n");
}

#[test]
fn command_mode_returns_syntax_status_two() {
    for source in [
        "echo '",
        "echo \"",
        "echo $'",
        "echo $(echo",
        "echo ${value",
        "echo <(echo",
        "echo >(echo",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
            .args(["-c", source])
            .output()
            .expect("run jsh");
        assert_eq!(output.status.code(), Some(2), "source: {source:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("incomplete input"),
            "source: {source:?}, stderr: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
