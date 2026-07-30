/// Phase 14a — structured errors + try/catch.
use std::io::Write;
use std::process::{Command, Stdio};

fn jsh_bin() -> String {
    env!("CARGO_BIN_EXE_jsh").to_string()
}

fn run(script: &str, stdin: &str) -> (String, String, i32) {
    let mut child = Command::new(jsh_bin())
        .arg("-c")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

// ---------------------------------------------------------------------------
// 14a — try / catch / error make
// ---------------------------------------------------------------------------

#[test]
fn try_returns_value_on_success() {
    let (out, _, code) = run(r#"try {|| 42 }"#, "");
    assert_eq!(out.trim(), "42");
    assert_eq!(code, 0);
}

#[test]
fn try_without_catch_yields_empty_on_error() {
    // Division by zero is caught and swallowed; output is empty (Null).
    let (out, _, code) = run(r#"try {|| 1 / 0 }"#, "");
    assert!(out.trim().is_empty());
    assert_eq!(code, 0);
}

#[test]
fn try_catch_receives_error_record() {
    let (out, _, code) = run(r#"try {|| 1 / 0 } catch {|e| $e.msg }"#, "");
    assert!(out.contains("division by zero"));
    assert_eq!(code, 0);
}

#[test]
fn try_catch_can_inspect_code() {
    let (out, _, _) = run(r#"try {|| 1 / 0 } catch {|e| $e.code }"#, "");
    assert_eq!(out.trim(), "1");
}

#[test]
fn error_make_then_catch() {
    let (out, _, code) = run(r#"try {|| error make "boom" } catch {|e| $e.msg }"#, "");
    assert_eq!(out.trim(), "boom");
    assert_eq!(code, 0);
}

#[test]
fn error_make_without_try_leaves_nonzero_exit() {
    let (_out, _err, code) = run(r#"error make "boom""#, "");
    assert_ne!(code, 0);
}

#[test]
fn try_does_not_poison_exit_code() {
    // Even though the inner closure errored, the outer pipeline succeeded.
    let (out, _, code) = run(r#"try {|| error make "x" } catch {|e| "ok" }"#, "");
    assert_eq!(out.trim(), "ok");
    assert_eq!(code, 0);
}

#[test]
fn try_passes_input_to_block() {
    let (out, _, _) = run(r#"from-json | try {|x| $x + 1 }"#, "10");
    assert_eq!(out.trim(), "11");
}

#[test]
fn nested_try_catch() {
    // Outer catch only runs if inner re-raises. Here inner recovers, so we
    // never enter outer catch.
    let (out, _, _) = run(
        r#"try {|| try {|| 1 / 0 } catch {|e| "inner" } } catch {|e| "outer" }"#,
        "",
    );
    assert_eq!(out.trim(), "inner");
}

// ---------------------------------------------------------------------------
// 14b — signatures + help
// ---------------------------------------------------------------------------

#[test]
fn help_renders_signature() {
    let (out, _, code) = run("help where", "");
    assert_eq!(code, 0);
    assert!(out.contains("where"));
    assert!(out.contains("list -> list"));
    assert!(out.contains("Parameters"));
}

#[test]
fn help_record_form_yields_record() {
    let (out, _, _) = run("help -r each | to-json", "");
    // The record should serialize with name=each and params containing closure.
    assert!(out.contains("\"name\": \"each\""));
    assert!(out.contains("\"closure\""));
    assert!(out.contains("\"params\""));
}

#[test]
fn help_unknown_command_errors_with_msg() {
    // try/catch wraps it so we can read the structured error message.
    let (out, _, _) = run(r#"try {|| help bogus-cmd } catch {|e| $e.msg }"#, "");
    assert!(out.contains("no signature for `bogus-cmd`"));
}

#[test]
fn help_no_args_lists_commands() {
    let (out, _, _) = run("help", "");
    // Should include at least these representative names.
    assert!(out.contains("from-json"));
    assert!(out.contains("where"));
    assert!(out.contains("try"));
}

#[test]
fn help_for_try_documents_catch() {
    let (out, _, _) = run("help try", "");
    assert!(out.contains("closure"));
    assert!(out.contains("catch") || out.contains("handler"));
}

// ---------------------------------------------------------------------------
// 14d — completer + highlighter awareness of signed commands
// ---------------------------------------------------------------------------

#[test]
fn completer_includes_signed_commands() {
    let mut state = jsh::environment::ShellState::new(false);
    jsh::completer::clear_cache();
    let buf = "wh";
    let (_, completions) = jsh::completer::complete(buf, buf.len(), &mut state);
    // `where` is a signed value-aware builtin; it should be offered as a
    // command completion even though it's not on PATH.
    assert!(
        completions.iter().any(|c| c.text == "where"),
        "expected `where` in completions, got: {:?}",
        completions.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
}

#[test]
fn completer_help_subcommand_lists_signed() {
    let mut state = jsh::environment::ShellState::new(false);
    jsh::completer::clear_cache();
    let buf = "help wh";
    let (_, completions) = jsh::completer::complete(buf, buf.len(), &mut state);
    assert!(
        completions.iter().any(|c| c.text == "where"),
        "expected `where` in `help <TAB>` completions"
    );
}

#[test]
fn highlighter_marks_signed_command_as_valid() {
    use crossterm::style::Color;
    let mut state = jsh::environment::ShellState::new(false);
    let spans = jsh::highlighter::highlight("try {|| 1 }", &mut state);
    // First span is the `try` token; it must be green+bold (valid command),
    // not red (unknown).
    let first = spans.iter().find(|s| s.text == "try").expect("try span");
    assert_eq!(first.fg, Some(Color::Green));
    assert!(first.bold);
}

#[test]
fn highlighter_marks_unknown_command_as_red() {
    use crossterm::style::Color;
    let mut state = jsh::environment::ShellState::new(false);
    let spans = jsh::highlighter::highlight("definitely-not-a-cmd-xyz arg", &mut state);
    let first = spans
        .iter()
        .find(|s| s.text == "definitely-not-a-cmd-xyz")
        .expect("cmd span");
    assert_eq!(first.fg, Some(Color::Red));
}

// ---------------------------------------------------------------------------
// 15a — signature-driven arity validation
// ---------------------------------------------------------------------------

#[test]
fn arity_missing_required_arg_reports_param_name() {
    // `wrap` requires a `name` positional. Without it the dispatcher
    // should refuse and mention which param is missing.
    let (_out, err, code) = run("from-json | wrap", "[1,2,3]");
    assert_ne!(code, 0);
    assert!(err.contains("wrap"));
    assert!(err.contains("name"));
}

#[test]
fn arity_signature_covers_new_builtins() {
    // 15a registered the rest of the value builtins. Spot-check a few.
    for n in &[
        "unique",
        "count",
        "open",
        "save",
        "encode",
        "decode",
        "ansi",
        "histogram",
    ] {
        assert!(
            jsh::signature::SIGNATURES.contains_key(*n),
            "missing signature: {}",
            n,
        );
    }
}

#[test]
fn arity_flagged_call_is_not_overconstrained() {
    // `window 3 --stride 2` mixes positional + flag. The validator
    // must not misread `2` as a stray positional.
    let (out, _err, code) = run(
        "from-json | window 3 --stride 2 | to-json",
        "[1,2,3,4,5,6,7]",
    );
    assert_eq!(code, 0);
    let compact: String = out.chars().filter(|c| !c.is_whitespace()).collect();
    assert_eq!(compact, "[[1,2,3],[3,4,5],[5,6,7]]");
}
