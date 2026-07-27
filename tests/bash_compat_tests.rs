/// Bash compatibility regressions found by running real-world setup scripts
/// (e.g. `source scripts/setup_env.sh`) under rsh:
///   * `[[ ... || ... ]]` was split into two commands at the `||`
///   * `"${arr[@]}"` collapsed into a single space-joined word
///   * `arr=("a b" c)` lost element quoting
///   * `command -v NAME` treated `-v` as the command to run
use std::process::{Command, Stdio};

fn rsh_bin() -> String {
    env!("CARGO_BIN_EXE_rsh").to_string()
}

fn run(script: &str) -> (String, String, i32) {
    let out = Command::new(rsh_bin())
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .output()
        .expect("spawn");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

// ---------------------------------------------------------------------------
// [[ ... ]] conditional expressions
// ---------------------------------------------------------------------------

#[test]
fn conditional_or_with_unary_operators() {
    // The `||` must stay inside the conditional: splitting it into two commands
    // made the shell try to run `-d` as a command.
    let (out, err, code) = run(r#"[[ -f /nonexistent-xyz || -d /etc ]] && echo yes"#);
    assert_eq!(out, "yes\n");
    assert!(err.is_empty(), "unexpected stderr: {}", err);
    assert_eq!(code, 0);
}

#[test]
fn conditional_and_with_unary_operators() {
    let (out, err, _) = run(r#"[[ -d /etc && -d /nonexistent-xyz ]] || echo no"#);
    assert_eq!(out, "no\n");
    assert!(err.is_empty(), "unexpected stderr: {}", err);
}

#[test]
fn conditional_or_of_string_comparisons() {
    let (out, _, _) = run(r#"a=aarch64; [[ "$a" == x86_64 || "$a" == aarch64 ]] && echo match"#);
    assert_eq!(out, "match\n");
}

#[test]
fn conditional_grouping_with_parentheses() {
    let (out, err, _) = run(r#"[[ ( -f /nonexistent-xyz || -d /etc ) && -e /etc ]] && echo ok"#);
    assert_eq!(out, "ok\n");
    assert!(err.is_empty(), "unexpected stderr: {}", err);
}

#[test]
fn conditional_string_ordering_operators() {
    let (out, _, _) = run(r#"[[ abc < abd ]] && [[ abd > abc ]] && echo ordered"#);
    assert_eq!(out, "ordered\n");
}

#[test]
fn conditional_spans_multiple_lines() {
    let (out, _, _) = run("if [[ -d /etc &&\n      -d /usr ]]; then echo multiline; fi");
    assert_eq!(out, "multiline\n");
}

#[test]
fn conditional_exit_status_is_preserved() {
    let (out, _, _) = run(r#"[[ -d /etc ]]; echo "t=$?"; [[ -d /nonexistent-xyz ]]; echo "f=$?""#);
    assert_eq!(out, "t=0\nf=1\n");
}

#[test]
fn conditional_negation_still_parses() {
    let (out, _, _) = run(r#"[[ ! -f /nonexistent-xyz ]] && echo negated"#);
    assert_eq!(out, "negated\n");
}

// ---------------------------------------------------------------------------
// Array expansion and array literals
// ---------------------------------------------------------------------------

#[test]
fn quoted_array_expansion_yields_one_field_per_element() {
    let (out, _, _) = run(r#"a=(x y z); for i in "${a[@]}"; do echo "[$i]"; done"#);
    assert_eq!(out, "[x]\n[y]\n[z]\n");
}

#[test]
fn array_literal_preserves_quoted_elements() {
    let (out, _, _) = run(r#"a=("one two" three); echo "${#a[@]}"; echo "[${a[0]}]""#);
    assert_eq!(out, "2\n[one two]\n");
}

#[test]
fn quoted_array_expansion_keeps_elements_with_spaces_intact() {
    let (out, _, _) = run(r#"a=("one two" three); printf '<%s>\n' "${a[@]}""#);
    assert_eq!(out, "<one two>\n<three>\n");
}

#[test]
fn quoted_array_expansion_passes_separate_arguments() {
    let (out, _, _) = run(r#"f() { echo "$#"; }; a=("one two" three); f "${a[@]}""#);
    assert_eq!(out, "2\n");
}

#[test]
fn empty_array_expands_to_no_arguments() {
    let (out, _, _) = run(r#"a=(); f() { echo "$#"; }; f "${a[@]}""#);
    assert_eq!(out, "0\n");
}

#[test]
fn quoted_star_subscript_joins_elements() {
    let (out, _, _) = run(r#"a=("one two" three); echo "[${a[*]}]""#);
    assert_eq!(out, "[one two three]\n");
}

#[test]
fn unquoted_array_expansion_still_word_splits() {
    let (out, _, _) = run(r#"a=("one two" three); printf '<%s>\n' ${a[@]}"#);
    assert_eq!(out, "<one>\n<two>\n<three>\n");
}

#[test]
fn array_expansion_concatenated_with_literals() {
    let (out, _, _) = run(r#"a=(x y); printf '<%s>\n' "pre-${a[@]}-post""#);
    assert_eq!(out, "<pre-x>\n<y-post>\n");
}

#[test]
fn array_literal_splits_unquoted_command_substitution() {
    let (out, _, _) = run(r#"a=($(echo x y) "$(echo p q)"); echo "${#a[@]} [${a[2]}]""#);
    assert_eq!(out, "3 [p q]\n");
}

#[test]
fn array_literal_ignores_comments() {
    let (out, _, _) =
        run("a=(\n  alpha  # a comment\n  \"beta gamma\"\n)\necho \"${#a[@]} [${a[1]}]\"");
    assert_eq!(out, "2 [beta gamma]\n");
}

#[test]
fn array_literal_keeps_parentheses_inside_quotes() {
    let (out, _, _) = run(r#"a=("x(1)y" z); echo "${#a[@]} [${a[0]}]""#);
    assert_eq!(out, "2 [x(1)y]\n");
}

#[test]
fn for_loop_over_quoted_array_finds_each_path() {
    // The shape that broke `setup_env.sh`: probing a list of candidate paths.
    let script = r#"
        paths=("/nonexistent-xyz" "/etc" "/usr")
        for p in "${paths[@]}"; do
            if [ -d "$p" ]; then echo "found $p"; break; fi
        done
    "#;
    let (out, _, _) = run(script);
    assert_eq!(out, "found /etc\n");
}

// ---------------------------------------------------------------------------
// command -v / -V
// ---------------------------------------------------------------------------

#[test]
fn command_v_reports_external_path() {
    let (out, _, code) = run("command -v sh");
    assert_eq!(code, 0);
    assert!(out.trim().ends_with("/sh"), "got {:?}", out);
}

#[test]
fn command_v_is_silent_and_fails_for_unknown_names() {
    let (out, _, code) = run("command -v definitely-not-a-real-command-xyz");
    assert_eq!(code, 1);
    assert!(out.is_empty(), "got {:?}", out);
}

#[test]
fn command_v_reports_builtins_and_functions_by_name() {
    let (out, _, _) = run("f() { :; }; command -v cd; command -v f");
    assert_eq!(out, "cd\nf\n");
}

#[test]
fn command_v_guards_conditional_the_way_scripts_use_it() {
    let (out, _, _) =
        run(r#"if command -v sh > /dev/null 2>&1; then echo present; else echo missing; fi"#);
    assert_eq!(out, "present\n");
}

#[test]
fn command_capital_v_describes_builtin() {
    let (out, _, code) = run("command -V cd");
    assert_eq!(code, 0);
    assert_eq!(out, "cd is a shell builtin\n");
}

#[test]
fn command_without_options_still_runs_the_command() {
    let (out, _, code) = run("command echo ran");
    assert_eq!(code, 0);
    assert_eq!(out, "ran\n");
}
