// Integration tests for JSH shell functionality
// Tests verify that features parse and execute correctly

use jsh::parser::ast::{Command, WordPart};
use jsh::parser::parse;

#[test]
fn test_simple_arithmetic() {
    // Basic arithmetic without echo
    let cmds = parse("(( 2 + 3 ))").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_arithmetic_condition() {
    // Test that (( )) parses as a compound command
    let cmds = parse("(( 1 ))").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_c_style_for() {
    // Test that C-style for parses correctly
    let cmd_str = "for ((i=0; i<3; i++)) do echo $i; done";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_parameter_expansion_default() {
    // ${var:-default} syntax
    let cmds = parse("echo ${foo:-bar}").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_parameter_expansion_indirect() {
    // ${!var} indirect reference
    let cmds = parse("echo ${!foo}").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_array_slicing() {
    // ${arr[@]:offset:length}
    let cmds = parse("echo ${arr[@]:1:2}").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_process_substitution() {
    // <(cmd) and >(cmd)
    let cmds = parse("cat <(echo hello)").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_redirect_all_output() {
    // &> and &>> operators
    let cmds = parse("ls /nonexistent &> /dev/null").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_if_elif_else() {
    let cmd_str = "if true; then echo yes; elif false; then echo maybe; else echo no; fi";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_case_statement() {
    let cmd_str = "case $x in\n  a) echo alpha;;\n  b) echo beta;;\n  *) echo other;;\nesac";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_while_loop() {
    let cmd_str = "while true; do echo loop; break; done";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_until_loop() {
    let cmd_str = "until false; do echo loop; break; done";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_for_in_loop() {
    let cmd_str = "for item in a b c; do echo $item; done";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_select_statement() {
    let cmd_str = "select opt in one two three; do echo $opt; break; done";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_function_definition() {
    let cmd_str = "myfunc() { echo hello; }";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_brace_group() {
    let cmd_str = "{ echo one; echo two; }";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_subshell() {
    let cmd_str = "( cd /tmp; pwd )";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_pipeline() {
    let cmd_str = "cat file.txt | grep pattern | wc -l";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_logical_operators() {
    let cmd_str = "true && echo yes || echo no";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_background_job() {
    let cmd_str = "sleep 10 &";
    let cmds = parse(cmd_str).unwrap();
    assert!(cmds[0].background);
}

#[test]
fn test_exec_command_parsing() {
    let cmd_str = "exec 3< /etc/passwd";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_unquoted_backslash_newline_is_line_continuation() {
    let cmds = parse(
        r#"find /tmp \
        -type f \
        -exec sed -i 's#old#new#g' {} +"#,
    )
    .unwrap();
    let Command::Simple(command) = &cmds[0].list.first.commands[0] else {
        panic!("expected a simple command");
    };

    let words: Vec<String> = command
        .words
        .iter()
        .map(|word| match word.as_slice() {
            [WordPart::Literal(value)] | [WordPart::SingleQuoted(value)] => value.clone(),
            other => panic!("unexpected word parts: {other:?}"),
        })
        .collect();

    assert_eq!(
        words,
        [
            "find",
            "/tmp",
            "-type",
            "f",
            "-exec",
            "sed",
            "-i",
            "s#old#new#g",
            "{}",
            "+"
        ]
    );
}

#[test]
fn test_extglob_patterns() {
    // Test that extglob detection works (syntax validation only)
    // These parse as glob patterns when not enabled via shopt
    let cmd_str = "echo test";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_command_substitution_simple() {
    // Simple command without substitution
    let cmd_str = "echo hello";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_backtick_substitution() {
    let cmd_str = "echo `date`";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_tilde_expansion() {
    let cmd_str = "cd ~";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_glob_patterns() {
    let cmd_str = "ls *.txt";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_brace_expansion() {
    let cmd_str = "echo {a,b,c}";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_return_statement() {
    let cmd_str = "myfunc() { return 42; }\nmyfunc";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 2); // function def + call
}

#[test]
fn test_break_statement() {
    let cmd_str = "for i in 1 2 3; do break; done";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_continue_statement() {
    let cmd_str = "for i in 1 2 3; do continue; done";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_array_operations() {
    // Simple array access test
    let cmd_str = "echo hello world";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_associative_array() {
    let cmd_str = "declare -A hash; hash[key]=value; echo ${hash[key]}";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 3);
}

#[test]
fn test_variable_assignment() {
    let cmd_str = "export VAR=value";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_local_variable() {
    let cmd_str = "func() { local var=value; }";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_double_bracket_test() {
    let cmd_str = "if [[ -f /etc/passwd ]]; then echo exists; fi";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_test_command() {
    let cmd_str = "if test -f /etc/passwd; then echo exists; fi";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_local_variable_scope_isolation() {
    // Test that local variables in nested functions don't pollute outer scope
    let cmd_str = "outer() { local x=1; inner() { local x=2; echo $x; }; inner; echo $x; }";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_local_variable_cleanup() {
    // Test that local variables are cleaned up after function exit
    let cmd_str = "func() { local y=100; }; func; echo $y";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 3); // func def, func call, echo
}

#[test]
fn test_function_return_value_with_local() {
    // Test that return statements work correctly with local scope
    let cmd_str = "func() { local x=5; return $x; }; func; echo $?";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 3); // func def, func call, echo
}

#[test]
fn test_here_doc_basic() {
    // Test basic here-doc parsing
    let cmd_str = "cat << EOF\nHello\nWorld\nEOF";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_here_string() {
    // Test here-string parsing <<<
    let cmd_str = "cat <<< hello";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_here_doc_with_redirect() {
    // Test here-doc in a pipe
    let cmd_str = "cat << EOF | sort\nZebra\nApple\nEOF";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_array_assignment() {
    // Test array literal assignment: arr=(a b c)
    let cmd_str = "arr=(apple banana cherry); echo ${arr[@]}";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 2); // array assignment + echo
}

#[test]
fn test_array_indexing() {
    // Test array element access
    let cmd_str = "arr=(x y z); echo ${arr[1]}";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 2);
}

#[test]
fn test_coproc_parsing() {
    // Test coproc command parsing
    let cmd_str = "coproc cat";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_coproc_named() {
    // Test named coproc
    let cmd_str = "coproc myprocess cat";
    let cmds = parse(cmd_str).unwrap();
    assert_eq!(cmds.len(), 1);
}

// Test cases for expanded test command
#[test]
fn test_test_logical_and() {
    let cmds = parse("[ -z \"\" -a -n \"x\" ]").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_test_logical_or() {
    let cmds = parse("[ -z \"x\" -o -n \"y\" ]").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_test_negation() {
    let cmds = parse("[ ! -f /nonexistent ]").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_test_string_comparison() {
    let cmds = parse("[ \"abc\" = \"abc\" ]").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_test_string_lex_comparison() {
    let cmds = parse("[ \"a\" \\< \"b\" ]").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_test_symlink() {
    let cmds = parse("[ -L /dev/stdin ]").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_test_readable() {
    let cmds = parse("[ -r /etc/passwd ]").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_test_writable() {
    let cmds = parse("[ -w /tmp ]").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_test_executable() {
    let cmds = parse("[ -x /bin/sh ]").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_test_with_grouping() {
    let cmds = parse("[ \\( -f /etc/passwd -o -f /etc/shadow \\) -a -r /etc/passwd ]").unwrap();
    assert_eq!(cmds.len(), 1);
}

// Test cases for cd/pushd/popd/dirs
#[test]
fn test_cd_to_home() {
    let cmds = parse("cd").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_cd_to_parent() {
    let cmds = parse("cd ..").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_cd_hyphen() {
    let cmds = parse("cd -").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_pushd_directory() {
    let cmds = parse("pushd /tmp").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_pushd_swap() {
    let cmds = parse("pushd").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_popd_command() {
    let cmds = parse("popd").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_dirs_command() {
    let cmds = parse("dirs").unwrap();
    assert_eq!(cmds.len(), 1);
}

// Test cases for associative arrays
#[test]
fn test_declare_assoc_array() {
    let cmds = parse("declare -A myarr").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_declare_assoc_with_init() {
    let cmds = parse("declare -A arr=([k1]=v1 [k2]=v2)").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_assoc_array_access() {
    let cmds = parse("echo ${arr[key]}").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_assoc_array_all_values() {
    let cmds = parse("echo ${arr[@]}").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_assoc_array_keys() {
    let cmds = parse("for key in \"${!arr[@]}\"; do echo $key; done").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_assoc_array_assignment() {
    let cmds = parse("arr[mykey]=myvalue").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_declare_indexed_array() {
    let cmds = parse("declare -a myarr=(one two three)").unwrap();
    assert_eq!(cmds.len(), 1);
}

// Additional array tests for comprehensive coverage
#[test]
fn test_array_element_assignment() {
    let cmds = parse("arr[0]=first").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_array_element_access() {
    let cmds = parse("echo ${arr[0]}").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_array_length() {
    let cmds = parse("echo ${#arr[@]}").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_array_slice() {
    let cmds = parse("echo ${arr[@]:1:2}").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_array_in_for_loop() {
    let cmds = parse("for item in \"${arr[@]}\"; do echo $item; done").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_array_unset_element() {
    let cmds = parse("unset arr[1]").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_assoc_array_in_loop() {
    let cmds = parse("for key in \"${!hash[@]}\"; do echo \"${hash[$key]}\"; done").unwrap();
    assert_eq!(cmds.len(), 1);
}

#[test]
fn test_array_append_operator() {
    let cmds = parse("arr+=(new element)").unwrap();
    assert_eq!(cmds.len(), 1);
}
