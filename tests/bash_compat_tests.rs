/// Bash compatibility regressions found by running real-world setup scripts
/// (e.g. `source scripts/setup_env.sh`) under jsh:
///   * `[[ ... || ... ]]` was split into two commands at the `||`
///   * `"${arr[@]}"` collapsed into a single space-joined word
///   * `arr=("a b" c)` lost element quoting
///   * `command -v NAME` treated `-v` as the command to run
use std::process::{Command, Stdio};

fn jsh_bin() -> String {
    env!("CARGO_BIN_EXE_jsh").to_string()
}

fn run(script: &str) -> (String, String, i32) {
    let out = Command::new(jsh_bin())
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

// ---------------------------------------------------------------------------
// Regressions from `source /opt/ros/humble/setup.bash` (ament setup scripts):
//   AMENT_CURRENT_PREFIX=$(builtin cd "`dirname "${BASH_SOURCE[0]}"`" && pwd)
// ---------------------------------------------------------------------------

#[test]
fn double_quotes_nest_inside_a_backtick_substitution() {
    // The inner `"` must not close the outer quote: the whole backtick body is
    // one command, so `dirname` gets its argument.
    let (out, err, _) = run(r#"f=/a/b/c.txt; echo "`dirname "$f"`""#);
    assert_eq!(out, "/a/b\n");
    assert!(err.is_empty(), "unexpected stderr: {}", err);
}

#[test]
fn backticks_with_nested_quotes_survive_inside_command_substitution() {
    let (out, err, _) =
        run(r#"f=/etc/hostname; d=$(builtin cd "`dirname "$f"`" && pwd); echo "$d""#);
    assert_eq!(out, "/etc\n");
    assert!(err.is_empty(), "unexpected stderr: {}", err);
}

#[test]
fn backtick_body_may_contain_a_closing_paren_inside_command_substitution() {
    let (out, err, _) = run(r#"echo "$(echo "`echo "a)b"`")""#);
    assert_eq!(out, "a)b\n");
    assert!(err.is_empty(), "unexpected stderr: {}", err);
}

#[test]
fn bash_source_names_the_file_being_sourced() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("setup.bash");
    std::fs::write(&script, "echo \"${BASH_SOURCE[0]}\"\n").unwrap();
    let (out, err, code) = run(&format!("source {}", script.display()));
    assert_eq!(code, 0);
    assert!(err.is_empty(), "unexpected stderr: {}", err);
    assert_eq!(out, format!("{}\n", script.display()));
}

#[test]
fn bash_source_is_restored_after_the_sourced_file_returns() {
    let dir = tempfile::tempdir().unwrap();
    let outer = dir.path().join("outer.bash");
    let inner = dir.path().join("inner.bash");
    std::fs::write(&inner, "echo \"inner=${BASH_SOURCE[0]}\"\n").unwrap();
    std::fs::write(
        &outer,
        format!(
            "source {}\necho \"outer=${{BASH_SOURCE[0]}}\"\n",
            inner.display()
        ),
    )
    .unwrap();
    let (out, _, _) = run(&format!("source {}", outer.display()));
    assert_eq!(
        out,
        format!("inner={}\nouter={}\n", inner.display(), outer.display())
    );
}

#[test]
fn setup_script_locates_its_own_directory() {
    // The exact idiom every ament prefix-level setup.bash opens with.
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("setup.bash");
    std::fs::write(
        &script,
        "PREFIX=$(builtin cd \"`dirname \"${BASH_SOURCE[0]}\"`\" && pwd)\necho \"$PREFIX\"\n",
    )
    .unwrap();
    let (out, err, code) = run(&format!("source {}", script.display()));
    assert_eq!(code, 0);
    assert!(err.is_empty(), "unexpected stderr: {}", err);
    assert_eq!(
        out.trim_end(),
        dir.path().canonicalize().unwrap().display().to_string()
    );
}

#[test]
fn colon_is_a_successful_no_op_builtin() {
    let (out, err, code) = run(": ignored args; echo $?");
    assert_eq!(code, 0);
    assert_eq!(out, "0\n");
    assert!(err.is_empty(), "unexpected stderr: {}", err);
}

#[test]
fn colon_expands_its_arguments_for_default_assignment() {
    // `: ${VAR:=default}` is how setup.sh seeds AMENT_CURRENT_PREFIX.
    let (out, _, _) = run(r#": ${V:=fallback}; echo "$V""#);
    assert_eq!(out, "fallback\n");
}

#[test]
fn ifs_starts_at_the_bash_default() {
    let (out, _, _) = run(r#"x="a b c"; set -- $x; echo $#"#);
    assert_eq!(out, "3\n");
}

#[test]
fn saving_and_restoring_ifs_keeps_word_splitting_alive() {
    // Setup scripts stash IFS, switch to ":" to walk a path list, then restore.
    // With IFS unset at startup the restore emptied it and killed splitting.
    let (out, _, _) = run(r#"old=$IFS; IFS=":"; IFS=$old; x="a b c"; set -- $x; echo $#"#);
    assert_eq!(out, "3\n");
}

// ---------------------------------------------------------------------------
// ${...} operator dispatch. Every operator used to be searched for
// independently across the whole body, so punctuation inside a pattern
// hijacked the expansion: `${v##*a-b*}` was read as `${v##*a}` defaulting to
// `b*`. /etc/profile.d/xdg_dirs_desktop_session.sh trips over exactly this.
// ---------------------------------------------------------------------------

#[test]
fn strip_pattern_may_contain_a_dash() {
    let (out, _, _) = run(r#"v=x-y-z; echo "[${v##*-}][${v#*-}][${v%%-*}][${v%-*}]""#);
    assert_eq!(out, "[z][y-z][x][x-y]\n");
}

#[test]
fn strip_pattern_may_contain_equals_and_plus() {
    let (out, _, _) = run(r#"d=a=b=c; p=x+y; echo "[${d#*=}][${d%%=*}][${p#*+}][${p%%+*}]""#);
    assert_eq!(out, "[b=c][a][y][x]\n");
}

#[test]
fn desktop_session_guard_from_etc_profile_d() {
    // `-n "${V##*$D/xdg-$S*}"` must be false when the entry is already present,
    // otherwise the directory gets prepended to XDG_CONFIG_DIRS a second time.
    let script = r#"
        V=/etc/xdg/xdg-ubuntu:/etc/xdg/xdg-jwm-xcb:/etc/xdg
        D=/etc/xdg
        S=jwm-xcb
        if [ -n "${V##*$D/xdg-$S*}" ]; then echo prepend; else echo keep; fi
    "#;
    let (out, _, _) = run(script);
    assert_eq!(out, "keep\n");
}

#[test]
fn colon_default_operators_still_win_over_a_bare_dash() {
    let (out, _, _) = run(r#"unset u; e=; echo "[${u:-a-b}][${e:-c-d}][${e-x}][${u-y-z}]""#);
    assert_eq!(out, "[a-b][c-d][][y-z]\n");
}

#[test]
fn substring_offset_and_length_still_work() {
    let (out, _, _) = run(r#"s=Hello-World; echo "[${s:2}][${s:2:3}][${s: -5}][${s:0:5}]""#);
    assert_eq!(out, "[llo-World][llo][World][Hello]\n");
}

#[test]
fn question_mark_operator_reports_and_aborts() {
    let (out, err, code) = run(r#"echo before; echo "${u:?is required}"; echo after"#);
    assert_eq!(out, "before\n");
    assert!(err.contains("u: is required"), "got stderr {:?}", err);
    assert_eq!(code, 1);
}

#[test]
fn case_conversion_operators() {
    let (out, _, _) = run(r#"s=hello-World; echo "[${s^^}][${s,,}][${s^}][${s,}]""#);
    assert_eq!(
        out,
        "[HELLO-WORLD][hello-world][Hello-World][hello-World]\n"
    );
}

#[test]
fn replacement_pattern_and_text_are_expanded() {
    let (out, _, _) = run(r#"s=x-y-z; sep=-; rep=+; echo "[${s//$sep/$rep}][${s//${sep}/.}]""#);
    assert_eq!(out, "[x+y+z][x.y.z]\n");
}

#[test]
fn replacement_pattern_may_be_an_escaped_slash() {
    let (out, _, _) = run(r#"p=/a/b/c; echo "[${p//\//_}][${p/\//_}][${p//\/}]""#);
    assert_eq!(out, "[_a_b_c][_a/b/c][abc]\n");
}

#[test]
fn anchored_replacements_are_unaffected() {
    let (out, _, _) = run(r#"s=Hello-World; echo "[${s/#Hello/Bye}][${s/%World/Earth}]""#);
    assert_eq!(out, "[Bye-World][Hello-Earth]\n");
}

// ---------------------------------------------------------------------------
// Regressions from `source ~/.nvm/nvm.sh`, which recursed until the stack
// blew: `$-` expanded to the literal text `$-`, so nvm's "is errexit set?"
// guard was always true and the function re-invoked itself forever.
// ---------------------------------------------------------------------------

#[test]
fn dollar_dash_reports_the_enabled_options() {
    let (out, _, _) = run(r#"echo "[$-]"; set -e; echo "[$-]"; set +e; set -x; echo "[$-]""#);
    assert_eq!(out, "[hB]\n[ehB]\n[hBx]\n");
}

#[test]
fn dollar_dash_drives_the_nvm_option_guard() {
    // `[ "${-#*e}" != "$-" ]` is true only while errexit is on.
    let probe = r#"if [ "${-#*e}" != "$-" ]; then echo on; else echo off; fi"#;
    let (out, _, _) = run(&format!("{}; set -e; {}", probe, probe));
    assert_eq!(out, "off\non\n");
}

#[test]
fn redirections_on_a_function_call_apply_to_its_body() {
    let (out, err, _) = run(r#"f() { echo out; echo err >&2; }; f 2>/dev/null; f >/dev/null"#);
    assert_eq!(out, "out\n");
    assert_eq!(err, "err\n");
}

#[test]
fn function_stderr_redirection_holds_inside_command_substitution() {
    // nvm reads `X="$(some_fn 2>/dev/null)"` and breaks out of a loop when X is
    // empty; the leaked diagnostic kept the loop running forever.
    let (out, err, _) =
        run(r#"f() { echo boom >&2; return 2; }; x="$(f 2>/dev/null)"; echo "[$x]""#);
    assert_eq!(out, "[]\n");
    assert!(err.is_empty(), "unexpected stderr: {}", err);
}

#[test]
fn assignment_prefix_on_a_function_call_is_temporary() {
    let (out, _, _) = run(r#"f() { echo "in=$V"; }; V=outer; V=inner f; echo "after=$V""#);
    assert_eq!(out, "in=inner\nafter=outer\n");
}

#[test]
fn command_preserves_arguments_containing_whitespace() {
    // `command X ...` used to re-join argv with spaces and re-parse it, which
    // split any argument holding a space or a newline into separate words.
    let (out, err, _) = run("command printf '<%s>\\n' 'a b' 'c\nd'");
    assert_eq!(out, "<a b>\n<c\nd>\n");
    assert!(err.is_empty(), "unexpected stderr: {}", err);
}

#[test]
fn command_passes_a_multiline_script_argument_through() {
    // The shape nvm uses: a sed program written across several lines.
    let script = "printf 'a\\n' | command sed -e \"\n    s#a#A#;\n  \"";
    let (out, err, _) = run(script);
    assert_eq!(out, "A\n");
    assert!(err.is_empty(), "unexpected stderr: {}", err);
}

// ---------------------------------------------------------------------------
// Sourcing a stock ~/.bashrc: shopt names, `[[ ]]` operands, extended globs
// ---------------------------------------------------------------------------

#[test]
fn shopt_accepts_bash_option_names_it_does_not_model() {
    // The first line of the stock Debian .bashrc that jsh reached.
    let (out, err, code) = run("shopt -s histappend; shopt histappend");
    assert_eq!(out, format!("{:<15}\ton\n", "histappend"));
    assert!(err.is_empty(), "unexpected stderr: {}", err);
    assert_eq!(code, 0);
}

#[test]
fn shopt_remembers_unmodelled_options_across_set_and_unset() {
    let (out, _, _) = run("shopt -s cmdhist progcomp; shopt -u progcomp; \
         shopt -q cmdhist; echo cmdhist=$?; shopt -q progcomp; echo progcomp=$?");
    assert_eq!(out, "cmdhist=0\nprogcomp=1\n");
}

#[test]
fn shopt_rejects_names_bash_does_not_have() {
    let (_, err, code) = run("shopt -s not_an_option");
    assert!(
        err.contains("not_an_option: invalid shell option name"),
        "unexpected stderr: {}",
        err
    );
    assert_eq!(code, 1);
}

#[test]
fn shopt_clustered_o_and_q_query_a_set_option() {
    // `if ! shopt -oq posix` guards the completion block of every stock
    // .bashrc. Reading `-oq` as two option names made it print two errors.
    let (out, err, _) = run("if ! shopt -oq posix; then echo enabled; fi");
    assert_eq!(out, "enabled\n");
    assert!(err.is_empty(), "unexpected stderr: {}", err);

    let (out, _, code) = run("set -o posix; shopt -oq posix; echo status=$?");
    assert_eq!(out, "status=0\n");
    assert_eq!(code, 0);
}

#[test]
fn shopt_prints_a_reusable_form_with_dash_p() {
    let (out, _, _) = run("shopt -s dotglob; shopt -p dotglob; shopt -po errexit");
    assert_eq!(out, "shopt -s dotglob\nset +o errexit\n");
}

#[test]
fn conditional_keeps_an_empty_expansion_as_an_operand() {
    // `[[ -n ${ZSH_VERSION-} ]]` decides whether nvm's completion script takes
    // its zsh branch. Dropping the empty word left the one-operand test
    // `[[ -n ]]`, which is true, so the zsh branch ran and called `autoload`.
    let (out, err, _) = run(
        "if [[ -n ${ZSH_VERSION-} ]]; then echo zsh; else echo bash; fi; \
         if [[ -z ${ZSH_VERSION-} ]]; then echo empty; fi",
    );
    assert_eq!(out, "bash\nempty\n");
    assert!(err.is_empty(), "unexpected stderr: {}", err);
}

#[test]
fn conditional_operands_are_not_word_split() {
    let (out, _, _) = run(r#"v="a b"; [[ $v == "x" ]] && echo yes || echo no"#);
    assert_eq!(out, "no\n");
}

#[test]
fn conditional_patterns_are_not_pathname_expanded() {
    let (out, _, _) = run("cd \"$(mktemp -d)\" && touch one.rs two.rs && \
         f=zzz.rs && [[ $f == *.rs ]] && echo match || echo no-match");
    assert_eq!(out, "match\n");
}

#[test]
fn extended_globs_parse_wherever_a_word_may_appear() {
    // Every one of these was a parse error, which is what stopped
    // `source /usr/share/bash-completion/bash_completion` dead.
    let script = r#"
shopt -s extglob
u=Linux
[[ $u == @(Linux|GNU/*) ]] && echo cond
case --nodir in --!(no-*)dir*) echo case;; *) echo other;; esac
case -abc in -?(\[)+([a-zA-Z0-9?])) echo short;; *) echo long;; esac
f() { [[ $1 == *@(solaris|aix)* ]] && echo osfunc; }
f solaris2.11
"#;
    let (out, err, code) = run(script);
    assert_eq!(out, "cond\ncase\nshort\nosfunc\n");
    assert!(err.is_empty(), "unexpected stderr: {}", err);
    assert_eq!(code, 0);
}

#[test]
fn extended_globs_expand_pathnames_and_leave_negation_alone() {
    let (out, _, _) = run(
        "cd \"$(mktemp -d)\" && touch keep.rs drop.txt && shopt -s extglob && \
         echo !(*.txt)",
    );
    assert_eq!(out, "keep.rs\n");

    // `!(...)` at the head of a command is still bash's pipeline negation.
    let (out, _, _) = run("!(true); echo status=$?");
    assert_eq!(out, "status=1\n");
}

#[test]
fn parameter_expansion_ends_at_its_own_brace() {
    // `${option2%%[<{().[]*}` — bash-completion's __parse_options — used to
    // swallow the closing brace because the `{` inside the bracket expression
    // was counted as a nested expansion.
    let (out, _, _) = run("x='abc<def'; echo \"${x%%[<{().[]*}\"; y=Q; echo ${z:-${y}}");
    assert_eq!(out, "abc\nQ\n");
}

#[test]
fn complete_registers_every_command_it_names() {
    // bash-completion registers `_longopt` for two dozen commands at a time,
    // and `complete -F _minimal ''` names the empty command word.
    let (out, err, code) = run(
        "_f() { :; }; complete -F _f awk bison cat; complete -F _f ''; \
         complete -D -F _f; complete | wc -l",
    );
    assert_eq!(out, "4\n");
    assert!(err.is_empty(), "unexpected stderr: {}", err);
    assert_eq!(code, 0);
}

// ---------------------------------------------------------------------------
// Startup files are sourced scripts
//
// These need a real terminal because startup files only load for an
// interactive shell, so they drive jsh through `script`'s pty and let the rc
// itself report what it reached. Markers go to a directory named by the
// environment rather than one the rc derives, so a test cannot pass or fail on
// the very self-location it is checking.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn interactive_rc_session(rc_body: &str) -> (String, tempfile::TempDir) {
    use std::io::Write;
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().expect("tempdir");
    let rc = dir.path().join("rc.bash");
    std::fs::write(&rc, rc_body).expect("write rc");
    let command = format!("{} --rcfile {}", jsh_bin(), rc.display());
    let mut child = Command::new("script")
        .args(["-qfec", &command, "/dev/null"])
        .env("JSH_TEST_MARKERS", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn interactive jsh");

    // Long enough for the rc to finish and the editor to take the terminal,
    // which is what makes the keystrokes below land in the line editor rather
    // than in a terminal that is still in canonical mode.
    std::thread::sleep(Duration::from_millis(600));
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"exit\r")
        .expect("ask the shell to leave");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait().expect("wait for jsh") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                break;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let out = child.wait_with_output().expect("collect jsh output");
    (String::from_utf8_lossy(&out.stdout).into_owned(), dir)
}

#[cfg(target_os = "linux")]
#[test]
fn a_distribution_guard_neither_errors_nor_truncates_a_startup_file() {
    // The two lines every stock rc opens with. Under bash neither fires in an
    // interactive shell. jsh printed an error for the first — a startup file
    // was executed as a program, where `return` is illegal — and read PS1 as
    // empty, so the shell looked non-interactive to the file it was reading.
    let (output, dir) = interactive_rc_session(
        "[ -z \"$PS1\" ] && return\n\
         case $- in *i*) ;; *) return;; esac\n\
         : > \"$JSH_TEST_MARKERS/tail-reached\"\n",
    );
    assert!(
        !output.contains("can only return"),
        "startup file reported an illegal return: {output}"
    );
    assert!(
        dir.path().join("tail-reached").exists(),
        "the rc stopped at its own guard: {output}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn return_in_a_startup_file_stops_reading_it() {
    // The other half of the same semantics: `return` is not merely tolerated,
    // it ends the file, so a guard that *does* fire skips the rest.
    let (output, dir) = interactive_rc_session(
        ": > \"$JSH_TEST_MARKERS/head-reached\"\n\
         return\n\
         : > \"$JSH_TEST_MARKERS/tail-reached\"\n",
    );
    assert!(
        dir.path().join("head-reached").exists(),
        "the rc did not run at all: {output}"
    );
    assert!(
        !dir.path().join("tail-reached").exists(),
        "return did not stop the startup file: {output}"
    );
    assert!(
        !output.contains("can only return"),
        "return was still reported as illegal: {output}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_startup_file_can_locate_itself() {
    // `$(dirname "${BASH_SOURCE[0]}")` is how an rc finds what sits next to
    // it. With the array empty it resolved to the working directory the shell
    // was started in, and the rc wrote its files there.
    let (output, dir) =
        interactive_rc_session("printf '%s' \"${BASH_SOURCE[0]}\" > \"$JSH_TEST_MARKERS/self\"\n");
    let recorded = std::fs::read_to_string(dir.path().join("self"))
        .unwrap_or_else(|err| panic!("rc did not record BASH_SOURCE ({err}): {output}"));
    assert_eq!(recorded, dir.path().join("rc.bash").display().to_string());
}
