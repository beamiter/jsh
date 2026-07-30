use jsh::environment::ShellState;
use jsh::expand::{expand_word_to_string, expand_words};
use jsh::parser::parse_word_parts;

fn make_state() -> ShellState {
    let mut state = ShellState::new(false);
    state.set_var("FOO", "hello");
    state.set_var("BAR", "world");
    state.set_var("NUM", "42");
    state.set_var("EMPTY", "");
    state
}

#[test]
fn test_simple_variable() {
    let mut state = make_state();
    let word = parse_word_parts("$FOO");
    assert_eq!(expand_word_to_string(&word, &mut state), "hello");
}

#[test]
fn test_braced_variable() {
    let mut state = make_state();
    let word = parse_word_parts("${FOO}");
    assert_eq!(expand_word_to_string(&word, &mut state), "hello");
}

#[test]
fn test_special_var_question() {
    let mut state = make_state();
    state.last_exit_code = 5;
    let word = parse_word_parts("$?");
    assert_eq!(expand_word_to_string(&word, &mut state), "5");
}

#[test]
fn test_special_var_hash() {
    let mut state = make_state();
    state.positional_params = vec!["a".into(), "b".into(), "c".into()];
    let word = parse_word_parts("$#");
    assert_eq!(expand_word_to_string(&word, &mut state), "3");
}

#[test]
fn test_special_var_at() {
    let mut state = make_state();
    state.positional_params = vec!["x".into(), "y".into()];
    let word = parse_word_parts("$@");
    assert_eq!(expand_word_to_string(&word, &mut state), "x y");
}

#[test]
fn test_positional_param() {
    let mut state = make_state();
    state.positional_params = vec!["first".into(), "second".into()];
    let word = parse_word_parts("$1");
    assert_eq!(expand_word_to_string(&word, &mut state), "first");
    let word2 = parse_word_parts("$2");
    assert_eq!(expand_word_to_string(&word2, &mut state), "second");
}

#[test]
fn test_default_value() {
    let mut state = make_state();
    let word = parse_word_parts("${UNSET:-default}");
    assert_eq!(expand_word_to_string(&word, &mut state), "default");
    let word2 = parse_word_parts("${FOO:-default}");
    assert_eq!(expand_word_to_string(&word2, &mut state), "hello");
}

#[test]
fn test_assign_default() {
    let mut state = make_state();
    let word = parse_word_parts("${NEWVAR:=assigned}");
    assert_eq!(expand_word_to_string(&word, &mut state), "assigned");
    assert_eq!(state.get_var("NEWVAR"), Some("assigned"));
}

#[test]
fn test_alternate_value() {
    let mut state = make_state();
    let word = parse_word_parts("${FOO:+alt}");
    assert_eq!(expand_word_to_string(&word, &mut state), "alt");
    let word2 = parse_word_parts("${UNSET:+alt}");
    assert_eq!(expand_word_to_string(&word2, &mut state), "");
}

#[test]
fn test_string_length() {
    let mut state = make_state();
    let word = parse_word_parts("${#FOO}");
    assert_eq!(expand_word_to_string(&word, &mut state), "5"); // "hello".len()
}

#[test]
fn test_substring() {
    let mut state = make_state();
    let word = parse_word_parts("${FOO:1:3}");
    assert_eq!(expand_word_to_string(&word, &mut state), "ell");
}

#[test]
fn test_prefix_strip() {
    let mut state = make_state();
    state.set_var("PATH_VAR", "/usr/local/bin");
    let word = parse_word_parts("${PATH_VAR#*/}");
    assert_eq!(expand_word_to_string(&word, &mut state), "usr/local/bin");
}

#[test]
fn test_replace() {
    let mut state = make_state();
    let word = parse_word_parts("${FOO/l/L}");
    assert_eq!(expand_word_to_string(&word, &mut state), "heLlo");
}

#[test]
fn test_replace_all() {
    let mut state = make_state();
    let word = parse_word_parts("${FOO//l/L}");
    assert_eq!(expand_word_to_string(&word, &mut state), "heLLo");
}

#[test]
fn test_arithmetic() {
    let mut state = make_state();
    let word = parse_word_parts("$((1 + 2))");
    assert_eq!(expand_word_to_string(&word, &mut state), "3");
}

#[test]
fn test_arithmetic_multiply() {
    let mut state = make_state();
    let word = parse_word_parts("$((3 * 4))");
    assert_eq!(expand_word_to_string(&word, &mut state), "12");
}

#[test]
fn test_arithmetic_comparison() {
    let mut state = make_state();
    let word = parse_word_parts("$((5 > 3))");
    assert_eq!(expand_word_to_string(&word, &mut state), "1");
    let word2 = parse_word_parts("$((2 > 3))");
    assert_eq!(expand_word_to_string(&word2, &mut state), "0");
}

#[test]
fn test_single_quoted_no_expand() {
    let mut state = make_state();
    let word = parse_word_parts("'$FOO'");
    assert_eq!(expand_word_to_string(&word, &mut state), "$FOO");
}

#[test]
fn test_double_quoted_expand() {
    let mut state = make_state();
    let word = parse_word_parts("\"$FOO\"");
    assert_eq!(expand_word_to_string(&word, &mut state), "hello");
}

#[test]
fn test_tilde_expansion() {
    let mut state = make_state();
    let word = parse_word_parts("~");
    let result = expand_word_to_string(&word, &mut state);
    assert!(!result.is_empty());
    assert!(!result.contains('~'));
}

#[test]
fn test_expand_words_multiple() {
    let mut state = make_state();
    let words = vec![parse_word_parts("$FOO"), parse_word_parts("$BAR")];
    let result = expand_words(&words, &mut state);
    assert_eq!(result, vec!["hello", "world"]);
}

#[test]
fn test_array_expansion() {
    let mut state = make_state();
    state.arrays.insert(
        "arr".to_string(),
        vec!["a".to_string(), "b".to_string(), "c".to_string()],
    );

    let word = parse_word_parts("${arr[0]}");
    assert_eq!(expand_word_to_string(&word, &mut state), "a");

    let word = parse_word_parts("${arr[2]}");
    assert_eq!(expand_word_to_string(&word, &mut state), "c");

    let word = parse_word_parts("${#arr[@]}");
    assert_eq!(expand_word_to_string(&word, &mut state), "3");
}

#[test]
fn test_assoc_array_expansion() {
    let mut state = make_state();
    let mut map = std::collections::HashMap::new();
    map.insert("key".to_string(), "value".to_string());
    state.assoc_arrays.insert("mymap".to_string(), map);

    let word = parse_word_parts("${mymap[key]}");
    assert_eq!(expand_word_to_string(&word, &mut state), "value");
}

#[test]
fn test_unset_variable() {
    let mut state = make_state();
    let word = parse_word_parts("$NONEXISTENT");
    assert_eq!(expand_word_to_string(&word, &mut state), "");
}
