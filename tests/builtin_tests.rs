use jsh::builtins::{is_builtin, run_builtin};
use jsh::environment::ShellState;

fn make_state() -> ShellState {
    ShellState::new(false)
}

#[test]
fn test_interactive_state_has_color_ls_aliases() {
    let state = ShellState::new(true);
    assert_eq!(
        state.aliases.get("ls"),
        Some(&"ls --color=auto".to_string())
    );
    assert_eq!(
        state.aliases.get("ll"),
        Some(&"ls -alF --color=auto".to_string())
    );
    assert_eq!(
        state.aliases.get("la"),
        Some(&"ls -A --color=auto".to_string())
    );
}

#[test]
fn test_noninteractive_state_does_not_seed_ls_aliases() {
    let state = ShellState::new(false);
    assert!(!state.aliases.contains_key("ls"));
    assert!(!state.aliases.contains_key("ll"));
    assert!(!state.aliases.contains_key("la"));
}

#[test]
fn test_is_builtin() {
    assert!(is_builtin("cd"));
    assert!(is_builtin("echo"));
    assert!(is_builtin("export"));
    assert!(is_builtin("declare"));
    assert!(is_builtin("z"));
    assert!(is_builtin("hook"));
    assert!(is_builtin("complete"));
    assert!(is_builtin("disown"));
    assert!(is_builtin("from-json"));
    assert!(is_builtin("where"));
    assert!(is_builtin("sort-by"));
    assert!(is_builtin("select"));
    assert!(is_builtin("to-table"));
    assert!(!is_builtin("nonexistent"));
}

#[test]
fn test_true_false() {
    let mut state = make_state();
    assert_eq!(run_builtin("true", &[], &mut state), 0);
    assert_eq!(run_builtin("false", &[], &mut state), 1);
}

#[test]
fn test_export_and_get() {
    let mut state = make_state();
    run_builtin("export", &["TESTVAR=hello".into()], &mut state);
    assert_eq!(state.env_vars.get("TESTVAR"), Some(&"hello".to_string()));
}

#[test]
fn test_unset() {
    let mut state = make_state();
    state.set_var("MYVAR", "value");
    assert_eq!(state.get_var("MYVAR"), Some("value"));
    run_builtin("unset", &["MYVAR".into()], &mut state);
    assert_eq!(state.get_var("MYVAR"), None);
}

#[test]
fn test_local() {
    let mut state = make_state();
    // local only works within a function scope
    state.push_local_scope();
    run_builtin("local", &["X=hello".into()], &mut state);
    assert_eq!(
        state.local_vars_stack.last().and_then(|s| s.get("X")),
        Some(&"hello".to_string())
    );
    state.pop_local_scope();
}

#[test]
fn test_set_positional() {
    let mut state = make_state();
    run_builtin(
        "set",
        &["--".into(), "a".into(), "b".into(), "c".into()],
        &mut state,
    );
    assert_eq!(state.positional_params, vec!["a", "b", "c"]);
}

#[test]
fn test_shift() {
    let mut state = make_state();
    state.positional_params = vec!["x".into(), "y".into(), "z".into()];
    run_builtin("shift", &[], &mut state);
    assert_eq!(state.positional_params, vec!["y", "z"]);
}

#[test]
fn test_alias() {
    let mut state = make_state();
    run_builtin("alias", &["ll=ls -la".into()], &mut state);
    assert_eq!(state.aliases.get("ll"), Some(&"ls -la".to_string()));
    run_builtin("unalias", &["ll".into()], &mut state);
    assert_eq!(state.aliases.get("ll"), None);
}

#[test]
fn test_trap() {
    let mut state = make_state();
    run_builtin("trap", &["echo bye".into(), "EXIT".into()], &mut state);
    assert_eq!(state.traps.get("EXIT"), Some(&"echo bye".to_string()));
    run_builtin("trap", &["-".into(), "EXIT".into()], &mut state);
    assert_eq!(state.traps.get("EXIT"), None);
}

#[test]
fn test_set_errexit() {
    let mut state = make_state();
    assert!(!state.shell_opts.errexit);
    run_builtin("set", &["-e".into()], &mut state);
    assert!(state.shell_opts.errexit);
    run_builtin("set", &["+e".into()], &mut state);
    assert!(!state.shell_opts.errexit);
}

#[test]
fn test_declare_indexed_array() {
    let mut state = make_state();
    run_builtin("declare", &["-a".into(), "myarr".into()], &mut state);
    assert!(state.arrays.contains_key("myarr"));
}

#[test]
fn test_declare_assoc_array() {
    let mut state = make_state();
    run_builtin("declare", &["-A".into(), "mymap".into()], &mut state);
    assert!(state.assoc_arrays.contains_key("mymap"));
}

#[test]
fn invalid_assoc_initializer_fails_without_partial_mutation() {
    let mut state = make_state();
    state
        .assoc_arrays
        .entry("mymap".into())
        .or_default()
        .insert("existing".into(), "keep".into());

    let status = run_builtin(
        "declare",
        &[
            "-A".into(),
            "mymap=([first]=temporary [broken]=\"unterminated)".into(),
        ],
        &mut state,
    );

    assert_eq!(status, 1);
    assert_eq!(state.assoc_arrays["mymap"].len(), 1);
    assert_eq!(state.assoc_arrays["mymap"]["existing"], "keep");

    let status = run_builtin(
        "declare",
        &["-A".into(), "newmap=([broken]=\"unterminated)".into()],
        &mut state,
    );
    assert_eq!(status, 1);
    assert!(!state.assoc_arrays.contains_key("newmap"));
}

#[test]
fn test_hook_add_remove() {
    let mut state = make_state();
    assert!(state.hooks.precmd.is_empty());
    run_builtin(
        "hook",
        &["add".into(), "precmd".into(), "myfunc".into()],
        &mut state,
    );
    assert_eq!(state.hooks.precmd, vec!["myfunc"]);
    run_builtin(
        "hook",
        &["remove".into(), "precmd".into(), "myfunc".into()],
        &mut state,
    );
    assert!(state.hooks.precmd.is_empty());
}

#[test]
fn test_complete_spec() {
    let mut state = make_state();
    run_builtin(
        "complete",
        &["-W".into(), "start stop restart".into(), "myservice".into()],
        &mut state,
    );
    assert!(state.completion_specs.contains_key("myservice"));
    let spec = &state.completion_specs["myservice"];
    assert_eq!(
        spec.word_list,
        Some(vec!["start".into(), "stop".into(), "restart".into()])
    );

    // Remove
    run_builtin("complete", &["-r".into(), "myservice".into()], &mut state);
    assert!(!state.completion_specs.contains_key("myservice"));
}

#[test]
fn test_double_bracket_string_eq() {
    let mut state = make_state();
    assert_eq!(
        run_builtin(
            "[[",
            &["hello".into(), "==".into(), "hello".into(), "]]".into()],
            &mut state
        ),
        0
    );
    assert_eq!(
        run_builtin(
            "[[",
            &["hello".into(), "==".into(), "world".into(), "]]".into()],
            &mut state
        ),
        1
    );
}

#[test]
fn test_double_bracket_regex() {
    let mut state = make_state();
    let result = run_builtin(
        "[[",
        &[
            "hello123".into(),
            "=~".into(),
            "hello([0-9]+)".into(),
            "]]".into(),
        ],
        &mut state,
    );
    assert_eq!(result, 0);
    // Check BASH_REMATCH
    assert!(state.arrays.contains_key("BASH_REMATCH"));
    let rematch = &state.arrays["BASH_REMATCH"];
    assert_eq!(rematch[0], "hello123");
    assert_eq!(rematch[1], "123");
}

#[test]
fn test_double_bracket_regex_no_match() {
    let mut state = make_state();
    let result = run_builtin(
        "[[",
        &["hello".into(), "=~".into(), "^[0-9]+$".into(), "]]".into()],
        &mut state,
    );
    assert_eq!(result, 1);
}

#[test]
fn test_double_bracket_numeric() {
    let mut state = make_state();
    assert_eq!(
        run_builtin(
            "[[",
            &["5".into(), "-gt".into(), "3".into(), "]]".into()],
            &mut state
        ),
        0
    );
    assert_eq!(
        run_builtin(
            "[[",
            &["2".into(), "-gt".into(), "3".into(), "]]".into()],
            &mut state
        ),
        1
    );
}

#[test]
fn test_double_bracket_file_test() {
    let mut state = make_state();
    assert_eq!(
        run_builtin(
            "[[",
            &["-f".into(), "/etc/hostname".into(), "]]".into()],
            &mut state
        ),
        0
    );
    assert_eq!(
        run_builtin("[[", &["-d".into(), "/tmp".into(), "]]".into()], &mut state),
        0
    );
    assert_eq!(
        run_builtin(
            "[[",
            &["-f".into(), "/nonexistent".into(), "]]".into()],
            &mut state
        ),
        1
    );
}

#[test]
fn test_test_builtin() {
    let mut state = make_state();
    assert_eq!(
        run_builtin("test", &["-n".into(), "hello".into()], &mut state),
        0
    );
    assert_eq!(
        run_builtin("test", &["-z".into(), "".into()], &mut state),
        0
    );
    assert_eq!(
        run_builtin("test", &["-z".into(), "hello".into()], &mut state),
        1
    );
    assert_eq!(
        run_builtin("test", &["3".into(), "-eq".into(), "3".into()], &mut state),
        0
    );
    assert_eq!(
        run_builtin("test", &["3".into(), "-ne".into(), "4".into()], &mut state),
        0
    );
}

/// `compgen` and `complete -A` must resolve an action the same way the Tab
/// completer does — one implementation, so a script and a keystroke cannot
/// disagree about what a user or a builtin is.
#[test]
fn compgen_actions_share_the_completers_sources() {
    let mut state = make_state();
    state
        .aliases
        .insert("jsh_compgen_alias".to_string(), "echo hi".to_string());

    let candidates = |action: &str, prefix: &str, state: &mut ShellState| {
        jsh::completer::action_candidates(action, prefix, state)
            .into_iter()
            .map(|completion| completion.text)
            .collect::<Vec<_>>()
    };

    assert!(
        candidates("alias", "jsh_compgen", &mut state).contains(&"jsh_compgen_alias".to_string())
    );
    assert!(candidates("builtin", "expo", &mut state).contains(&"export".to_string()));
    assert!(candidates("keyword", "wh", &mut state).contains(&"while".to_string()));
    assert!(candidates("user", "roo", &mut state).contains(&"root".to_string()));

    // An action this shell has no notion of yields nothing, rather than
    // something that looks like an answer.
    assert!(candidates("helptopic", "", &mut state).is_empty());
}

#[test]
fn compgen_reports_matches_and_their_absence() {
    let mut state = make_state();
    // A word list filters by the prefix, as bash does.
    assert_eq!(
        run_builtin(
            "compgen",
            &["-W".into(), "alpha beta".into(), "al".into()],
            &mut state
        ),
        0
    );
    // No match at all is a non-zero status, which is what scripts test.
    assert_eq!(
        run_builtin(
            "compgen",
            &["-W".into(), "alpha beta".into(), "zzz".into()],
            &mut state
        ),
        1
    );
    assert!(is_builtin("compgen"));
}
